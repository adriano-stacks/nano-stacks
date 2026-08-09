use clarity::types::StacksEpochId;
use clarity::vm::costs::CostErrors;
use clarity::vm::errors::{
    CommonCheckErrorKind, EarlyReturnError, RuntimeCheckErrorKind, RuntimeError, VmExecutionError,
};
use clarity::vm::types::{ResponseData, TypeSignature};
use clarity::vm::{ClarityVersion, SymbolicExpression, Value};
use clarity_types::types::{ASCIIData, CharType};
use clarity_types::{ClarityName, ClarityTypeError};
use std::sync::Mutex;
use walrus::ir::InstrSeqId;
use walrus::InstrSeqBuilder;
use wasmtime::{AsContextMut, Instance, Trap};

use crate::error::WasmError;
use crate::runtime_shape::RuntimeShapeStore;
use crate::wasm_generator::{clar2wasm_ty, GeneratorError, WasmGenerator};
use crate::wasm_utils::{
    get_global, read_bytes_from_wasm, read_from_wasm_indirect, read_identifier_from_wasm,
    signature_from_string,
};

const LOG2_ERROR_MESSAGE: &str = "log2 must be passed a positive integer";
const SQRTI_ERROR_MESSAGE: &str = "sqrti must be passed a positive integer";
const POW_ERROR_MESSAGE: &str = "Power argument to (pow ...) must be a u32 integer";

/// Represents various error conditions that can occur
/// during Clarity contract execution
/// or other Stacks blockchain operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorMap {
    /// Indicates that the error is not related to Clarity contract execution.
    NotClarityError = -1,

    /// Represents an arithmetic overflow error in Clarity contract execution.
    /// This occurs when a calculation exceeds the maximum value representable.
    ArithmeticOverflow = 0,

    /// Represents an arithmetic underflow error in Clarity contract execution.
    /// This occurs when a calculation results in a value below the minimum representable value.
    ArithmeticUnderflow = 1,

    /// Indicates an attempt to divide by zero in a Clarity contract.
    DivisionByZero = 2,

    /// Represents an error in calculating the logarithm base 2 in a Clarity contract.
    /// This could occur for negative inputs.
    ArithmeticLog2Error = 3,

    /// Represents an error in calculating the integer square root in a Clarity contract.
    /// This could occur for negative inputs.
    ArithmeticSqrtiError = 4,

    /// Indicates an error in constructing a type, possibly due to invalid parameters.
    BadTypeConstruction = 5,

    /// Represents a deliberate panic in contract execution,
    /// usually triggered by `(unwrap-panic...)` and `(unwrap-err-panic...)`.
    Panic = 6,

    /// Indicates a failure in an assertion that was expected to cause a short return,
    /// usually triggered by `(asserts!...)`.
    ShortReturnAssertionFailure = 7,

    /// Represents an error in exponentiation operations in a Clarity contract.
    /// This could occur for invalid bases or exponents.
    ArithmeticPowError = 8,

    /// Indicates an attempt to use a name that is already in use, possibly for a variable or function.
    NameAlreadyUsed = 9,

    /// Represents a short-return error for an expected value that wraps a Response type.
    /// Usually triggered by `(try!...)`.
    ShortReturnExpectedValueResponse = 10,

    /// Represents a short-return error for an expected value that wraps an Optional type.
    /// Usually triggered by `(try!...)`.
    ShortReturnExpectedValueOptional = 11,

    /// Represents a short-return error for an expected value.
    /// usually triggered by `(unwrap!...)` and `(unwrap-err!...)`.
    ShortReturnExpectedValue = 12,

    /// Indicates an attempt to use a function with the wrong amount of arguments
    ArgumentCountMismatch = 13,

    /// Indicates an attempt to use a function with too few arguments
    ArgumentCountAtLeast = 14,

    /// Indicates an attempt to use a function with too many arguments
    ArgumentCountAtMost = 15,

    /// Indicates an attempt to use a function with too many arguments
    SequenceElementArityMismatch = 16,

    /// Indicates a runtime cost overrun
    CostOverrunRuntime = 100,

    /// Indicates a read count cost overrun
    CostOverrunReadCount = 101,

    /// Indicates a read length cost overrun
    CostOverrunReadLength = 102,

    /// Indicates a write count cost overrun
    CostOverrunWriteCount = 103,

    /// Indicates a write length cost overrun
    CostOverrunWriteLength = 104,

    ExternError = 105,

    // Indicate that a call to TypeSignature.size() failed
    SignatureTypeSizeCheckError = 106,

    /// A catch-all for errors that are not mapped to specific error codes.
    /// This might be used for unexpected or unclassified errors.
    NotMapped = 99,
}

impl From<i32> for ErrorMap {
    fn from(error_code: i32) -> Self {
        match error_code {
            -1 => ErrorMap::NotClarityError,
            0 => ErrorMap::ArithmeticOverflow,
            1 => ErrorMap::ArithmeticUnderflow,
            2 => ErrorMap::DivisionByZero,
            3 => ErrorMap::ArithmeticLog2Error,
            4 => ErrorMap::ArithmeticSqrtiError,
            // TODO: This error needs to be removed/changed the same way it has been in stacks/core
            5 => ErrorMap::BadTypeConstruction,
            6 => ErrorMap::Panic,
            7 => ErrorMap::ShortReturnAssertionFailure,
            8 => ErrorMap::ArithmeticPowError,
            9 => ErrorMap::NameAlreadyUsed,
            10 => ErrorMap::ShortReturnExpectedValueResponse,
            11 => ErrorMap::ShortReturnExpectedValueOptional,
            12 => ErrorMap::ShortReturnExpectedValue,
            13 => ErrorMap::ArgumentCountMismatch,
            14 => ErrorMap::ArgumentCountAtLeast,
            15 => ErrorMap::ArgumentCountAtMost,
            16 => ErrorMap::SequenceElementArityMismatch,
            100 => ErrorMap::CostOverrunRuntime,
            101 => ErrorMap::CostOverrunReadCount,
            102 => ErrorMap::CostOverrunReadLength,
            103 => ErrorMap::CostOverrunWriteCount,
            104 => ErrorMap::CostOverrunWriteLength,
            105 => ErrorMap::ExternError,
            106 => ErrorMap::SignatureTypeSizeCheckError,
            _ => ErrorMap::NotMapped,
        }
    }
}

pub(crate) fn resolve_error<S>(
    e: wasmtime::Error,
    instance: Instance,
    mut store: S,
    epoch_id: &StacksEpochId,
    clarity_version: &ClarityVersion,
) -> VmExecutionError
where
    S: AsContextMut,
    S::Data: RuntimeShapeStore,
{
    if let Some(vm_error) = e.root_cause().downcast_ref::<VmExecutionError>() {
        if let Some(vm_error) = clone_vm_execution_error(vm_error) {
            return vm_error;
        }
        return crate::error::wasm_error(WasmError::Expect(vm_error.to_string()));
    };

    if let Some(vm_error) = e.root_cause().downcast_ref::<RuntimeCheckErrorKind>() {
        if let Some(vm_error) = clone_runtime_check_error(vm_error) {
            return VmExecutionError::RuntimeCheck(vm_error);
        }
        return crate::error::wasm_error(WasmError::Expect(vm_error.to_string()));
    };

    if let Some(vm_error) = e.root_cause().downcast_ref::<RuntimeError>() {
        return crate::error::wasm_error(WasmError::Expect(vm_error.to_string()));
    };

    // Check if the error is caused by
    // an unreachable Wasm trap.
    //
    // In this case, runtime errors are handled
    // by being mapped to the corresponding ClarityWasm Errors.
    if let Some(Trap::UnreachableCodeReached) = e.root_cause().downcast_ref::<Trap>() {
        return from_runtime_error_code(instance, &mut store, e, epoch_id, clarity_version);
    }

    // All other errors are treated as general runtime errors.
    crate::error::wasm_error(WasmError::Runtime(e))
}

fn clone_vm_execution_error(error: &VmExecutionError) -> Option<VmExecutionError> {
    match error {
        VmExecutionError::RuntimeCheck(error) => {
            clone_runtime_check_error(error).map(VmExecutionError::RuntimeCheck)
        }
        _ => None,
    }
}

fn clone_runtime_check_error(error: &RuntimeCheckErrorKind) -> Option<RuntimeCheckErrorKind> {
    match error {
        RuntimeCheckErrorKind::TypeValueError(ty, value) => Some(
            RuntimeCheckErrorKind::TypeValueError(ty.clone(), value.clone()),
        ),
        RuntimeCheckErrorKind::UnionTypeValueError(types, value) => Some(
            RuntimeCheckErrorKind::UnionTypeValueError(types.clone(), value.clone()),
        ),
        // `at-block` in an epoch that withdrew it, raised by the host's
        // `enter_at_block`. The identity matters twice over: it is the text in
        // the receipt, and it decides whether there *is* a receipt.
        // `RuntimeCheckErrorKind::rejectable()` is false for it, so stacks-core
        // fails the transaction and accepts the block — while the fallback below
        // turns it into `Expect("AtBlockUnavailable")`, an internal error that
        // `is_acceptable_runtime_failure` refuses, which stops the node on a
        // block the network accepted.
        //
        RuntimeCheckErrorKind::AtBlockUnavailable => {
            Some(RuntimeCheckErrorKind::AtBlockUnavailable)
        }
        // The other unit variant a host function raises: a `contract-call?`
        // whose target is neither a contract name nor a dispatchable callable
        // (`check_constant_call_target` and the two allowance readers in
        // `linker.rs`). It is not `rejectable()` either, so stacks-core fails the
        // transaction and accepts the block — where `Expect` below would stop the
        // node on a block the network took.
        RuntimeCheckErrorKind::ContractCallExpectName => {
            Some(RuntimeCheckErrorKind::ContractCallExpectName)
        }
        _ => None,
    }
}

/// Converts a WebAssembly runtime error code into a Clarity `Error`.
///
/// This function interprets an error code from a WebAssembly runtime execution and
/// translates it into an appropriate Clarity error type. It handles various categories
/// of errors including arithmetic errors, short returns, and other runtime issues.
///
/// # Returns
///
/// Returns a Clarity `Error` that corresponds to the runtime error encountered during
/// WebAssembly execution.
///
fn from_runtime_error_code<S>(
    instance: Instance,
    mut store: S,
    e: wasmtime::Error,
    epoch_id: &StacksEpochId,
    clarity_version: &ClarityVersion,
) -> VmExecutionError
where
    S: AsContextMut,
    S::Data: RuntimeShapeStore,
{
    let runtime_error_code = get_global_i32(&instance, &mut store, "runtime-error-code");

    match ErrorMap::from(runtime_error_code) {
        ErrorMap::NotClarityError => crate::error::wasm_error(WasmError::Runtime(e)),
        ErrorMap::ArithmeticOverflow => {
            VmExecutionError::Runtime(RuntimeError::ArithmeticOverflow, Some(Vec::new()))
        }
        ErrorMap::ArithmeticUnderflow => {
            VmExecutionError::Runtime(RuntimeError::ArithmeticUnderflow, Some(Vec::new()))
        }
        ErrorMap::DivisionByZero => {
            VmExecutionError::Runtime(RuntimeError::DivisionByZero, Some(Vec::new()))
        }
        ErrorMap::ArithmeticLog2Error => VmExecutionError::Runtime(
            RuntimeError::Arithmetic(LOG2_ERROR_MESSAGE.into()),
            Some(Vec::new()),
        ),
        ErrorMap::ArithmeticSqrtiError => VmExecutionError::Runtime(
            RuntimeError::Arithmetic(SQRTI_ERROR_MESSAGE.into()),
            Some(Vec::new()),
        ),
        ErrorMap::BadTypeConstruction => VmExecutionError::Runtime(
            RuntimeError::Arithmetic("invalid type construction".into()),
            Some(Vec::new()),
        ),
        ErrorMap::Panic => {
            // TODO: see issue: #531
            // This RuntimeError::UnwrapFailure need to have a proper context.
            VmExecutionError::Runtime(RuntimeError::UnwrapFailure, Some(Vec::new()))
        }
        ErrorMap::ShortReturnAssertionFailure => {
            let clarity_val = short_return_value(&instance, &mut store, epoch_id, clarity_version);
            VmExecutionError::EarlyReturn(EarlyReturnError::AssertionFailed(Box::new(clarity_val)))
        }
        ErrorMap::ArithmeticPowError => VmExecutionError::Runtime(
            RuntimeError::Arithmetic(POW_ERROR_MESSAGE.into()),
            Some(Vec::new()),
        ),
        ErrorMap::NameAlreadyUsed => {
            let runtime_error_arg_offset =
                get_global_i32(&instance, &mut store, "runtime-error-arg-offset");
            let runtime_error_arg_len =
                get_global_i32(&instance, &mut store, "runtime-error-arg-len");

            let memory = instance
                .get_memory(&mut store, "memory")
                .unwrap_or_else(|| panic!("Could not find wasm instance memory"));
            let arg_name = read_identifier_from_wasm(
                memory,
                &mut store,
                runtime_error_arg_offset,
                runtime_error_arg_len,
            )
            .unwrap_or_else(|e| panic!("Could not recover arg_name: {e}"));

            VmExecutionError::RuntimeCheck(RuntimeCheckErrorKind::NameAlreadyUsed(arg_name))
        }
        ErrorMap::ShortReturnExpectedValueResponse => {
            let clarity_val = short_return_value(&instance, &mut store, epoch_id, clarity_version);
            VmExecutionError::EarlyReturn(EarlyReturnError::UnwrapFailed(Box::new(
                Value::Response(ResponseData {
                    committed: false,
                    data: Box::new(clarity_val),
                }),
            )))
        }
        ErrorMap::ShortReturnExpectedValueOptional => {
            VmExecutionError::EarlyReturn(EarlyReturnError::UnwrapFailed(Box::new(
                Value::Optional(clarity::vm::types::OptionalData { data: None }),
            )))
        }
        ErrorMap::ShortReturnExpectedValue => {
            let clarity_val = short_return_value(&instance, &mut store, epoch_id, clarity_version);
            VmExecutionError::EarlyReturn(EarlyReturnError::UnwrapFailed(Box::new(clarity_val)))
        }
        ErrorMap::ArgumentCountMismatch => {
            let (expected, got) = get_runtime_error_arg_lengths(&instance, &mut store);
            VmExecutionError::RuntimeCheck(RuntimeCheckErrorKind::IncorrectArgumentCount(
                expected, got,
            ))
        }
        ErrorMap::ArgumentCountAtLeast => {
            let (expected, got) = get_runtime_error_arg_lengths(&instance, &mut store);
            CommonCheckErrorKind::RequiresAtLeastArguments(expected, got).into()
        }
        ErrorMap::ArgumentCountAtMost => {
            let (expected, got) = get_runtime_error_arg_lengths(&instance, &mut store);
            CommonCheckErrorKind::RequiresAtMostArguments(expected, got).into()
        }
        ErrorMap::SequenceElementArityMismatch => {
            let (expected, found) = get_runtime_error_arg_lengths(&instance, &mut store);
            VmExecutionError::RuntimeCheck(
                ClarityTypeError::SequenceElementArityMismatch { expected, found }.into(),
            )
        }
        ErrorMap::CostOverrunRuntime => VmExecutionError::from(CostErrors::CostOverflow),
        ErrorMap::CostOverrunReadCount => VmExecutionError::from(CostErrors::CostOverflow),
        ErrorMap::CostOverrunReadLength => VmExecutionError::from(CostErrors::CostOverflow),
        ErrorMap::CostOverrunWriteCount => VmExecutionError::from(CostErrors::CostOverflow),
        ErrorMap::CostOverrunWriteLength => VmExecutionError::from(CostErrors::CostOverflow),
        ErrorMap::ExternError => {
            match instance.get_global(store.as_context_mut(), "linked-error") {
                None => crate::error::wasm_error(WasmError::GlobalNotFound(
                    "runtime-error-linked".to_owned(),
                )),
                Some(global) => match global.get(store.as_context_mut()).unwrap_externref() {
                    None => crate::error::wasm_error(WasmError::Expect("".to_owned())),
                    Some(linked_error_extern) => match linked_error_extern
                        .data()
                        .downcast_ref::<Mutex<Option<VmExecutionError>>>()
                    {
                        None => crate::error::wasm_error(WasmError::Expect(
                            "runtime-error-linked should hold an error type".to_owned(),
                        )),
                        Some(error) => match error.lock() {
                            Ok(mut error) => error.take().unwrap_or_else(|| {
                                crate::error::wasm_error(WasmError::Expect(
                                    "runtime-error-linked was already consumed".to_owned(),
                                ))
                            }),
                            Err(_) => crate::error::wasm_error(WasmError::Expect(
                                "runtime-error-linked is poisoned".to_owned(),
                            )),
                        },
                    },
                },
            }
        }
        ErrorMap::SignatureTypeSizeCheckError => crate::error::wasm_error(WasmError::Expect(
            "FAIL: .size() overflowed on too large of a type. construction should have failed!"
                .into(),
        )),
        _ => panic!("Runtime error code {runtime_error_code} not supported"),
    }
}

/// Retrieves the value of a 32-bit integer global variable from a WebAssembly instance.
///
/// This function attempts to fetch a global variable by name from the provided WebAssembly
/// instance and return its value as an `i32`. It's designed to simplify the process of
/// reading global variables in WebAssembly modules.
///
/// # Returns
///
/// Returns the value of the global variable as an `i32`.
///
fn get_global_i32(instance: &Instance, store: &mut impl AsContextMut, name: &str) -> i32 {
    instance
        .get_global(&mut *store, name)
        .and_then(|glob| glob.get(store).i32())
        .unwrap_or_else(|| panic!("Could not find ${name} global with i32 value"))
}

/// Retrieves the expected and actual argument counts from a byte-encoded string.
///
/// This function interprets a string as a sequence of bytes, where the first 4 bytes
/// represent the expected number of arguments, and the bytes at positions 16 to 19
/// represent the actual number of arguments received. It converts these byte sequences
/// into `usize` values and returns them as a tuple.
///
/// # Returns
///
/// A tuple `(expected, got)` where:
/// - `expected` is the number of arguments expected.
/// - `got` is the number of arguments actually received.
fn extract_expected_and_got(bytes: &[u8]) -> (usize, usize) {
    // Assuming the first 4 bytes represent the expected value
    let expected = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;

    // Assuming the next 4 bytes represent the got value
    let got = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;

    (expected, got)
}

/// Retrieves and deserializes a Clarity value from WebAssembly memory in the context of a short return.
///
/// This function is used to extract a Clarity value that has been stored in WebAssembly memory
/// as part of a short return operation. It reads necessary metadata from global variables,
/// deserializes the type information, and then reads and deserializes the actual value.
///
/// # Returns
///
/// Returns a deserialized Clarity `Value` representing the short return value.
///
fn short_return_value<S>(
    instance: &Instance,
    store: &mut S,
    epoch_id: &StacksEpochId,
    clarity_version: &ClarityVersion,
) -> Value
where
    S: AsContextMut,
    S::Data: RuntimeShapeStore,
{
    let val_offset = get_global_i32(instance, store, "runtime-error-value-offset");
    let type_ser_offset = get_global_i32(instance, store, "runtime-error-type-ser-offset");
    let type_ser_len = get_global_i32(instance, store, "runtime-error-type-ser-len");

    let memory = instance
        .get_memory(&mut *store, "memory")
        .unwrap_or_else(|| panic!("Could not find wasm instance memory"));

    let type_ser_str = read_identifier_from_wasm(memory, store, type_ser_offset, type_ser_len)
        .unwrap_or_else(|e| panic!("Could not recover stringified type: {e}"));

    let value_ty = signature_from_string(&type_ser_str, *clarity_version, *epoch_id)
        .unwrap_or_else(|e| panic!("Could not recover thrown value: {e}"));

    read_from_wasm_indirect(memory, store, &value_ty, val_offset, *epoch_id)
        .unwrap_or_else(|e| panic!("Could not read thrown value from memory: {e}"))
}

/// Retrieves the argument lengths from the runtime error global variables.
///
/// This function reads the global variables `runtime-error-arg-offset` and `runtime-error-arg-len`
/// from the WebAssembly instance and constructs a string representing the argument lengths.
///
/// # Returns
///
/// A string representing the argument lengths.
fn get_runtime_error_arg_lengths(
    instance: &Instance,
    store: &mut impl AsContextMut,
) -> (usize, usize) {
    let runtime_error_arg_offset = get_global_i32(instance, store, "runtime-error-arg-offset");
    let runtime_error_arg_len = get_global_i32(instance, store, "runtime-error-arg-len");

    let memory = instance
        .get_memory(&mut *store, "memory")
        .unwrap_or_else(|| panic!("Could not find wasm instance memory"));
    let arg_lengths = read_bytes_from_wasm(
        memory,
        store,
        runtime_error_arg_offset,
        runtime_error_arg_len,
    )
    .unwrap_or_else(|e| panic!("Could not recover arg_lengths: {e}"));

    extract_expected_and_got(&arg_lengths)
}

pub(crate) fn generate_name_already_used_error(
    generator: &mut WasmGenerator,
    builder: &mut InstrSeqBuilder,
    name: &ClarityName,
) -> Result<(), GeneratorError> {
    let (arg_name_offset, arg_name_len) =
        generator.add_clarity_string_literal(&CharType::ASCII(ASCIIData {
            data: name.as_bytes().to_vec(),
        }))?;

    builder
        .i32_const(arg_name_offset as i32)
        .global_set(get_global(&generator.module, "runtime-error-arg-offset")?)
        .i32_const(arg_name_len as i32)
        .global_set(get_global(&generator.module, "runtime-error-arg-len")?)
        .i32_const(ErrorMap::NameAlreadyUsed as i32)
        .call(generator.func_by_name("stdlib.runtime-error"));

    // prevents type errors in the generated binary
    builder.unreachable();

    Ok(())
}

impl WasmGenerator {
    /// A branch's block, or one that raises `NameAlreadyUsed` for the name the
    /// branch binds.
    ///
    /// The interpreter checks a binding's name where it *binds* it, so a `match`
    /// branch that is not taken never rejects its name. Refusing the whole
    /// contract at compile time is not the same judgement: mainnet 8,668,096
    /// called `auto-alex-v3-endpoint-v2-02::rebase`, which binds `err` in an
    /// error branch it does not reach, and the chain answers `(ok u390)` — while
    /// the compiler would not build the contract at all, so every call into it
    /// failed.
    ///
    /// `already_used` is the caller's answer to `binding_name_already_used`,
    /// taken *before* the branch's own binding is in scope.
    pub(crate) fn block_from_bound_expr(
        &mut self,
        builder: &mut InstrSeqBuilder,
        expr: &SymbolicExpression,
        binding: &ClarityName,
        already_used: bool,
    ) -> Result<InstrSeqId, GeneratorError> {
        if !already_used {
            return self.block_from_expr(builder, expr);
        }

        let return_type = clar2wasm_ty(self.get_expr_type(expr).ok_or_else(|| {
            GeneratorError::TypeError("Expression results must be typed".to_owned())
        })?);
        let block_type = self.bounded_control_type(&[], &return_type)?;
        let mut block = builder.dangling_instr_seq(block_type);
        generate_name_already_used_error(self, &mut block, binding)?;

        Ok(block.id())
    }

    /// Memory-backed counterpart to [`Self::block_from_bound_expr`] for a
    /// control value too wide for a Wasm block result.
    pub(crate) fn block_from_bound_expr_into_memory(
        &mut self,
        builder: &mut InstrSeqBuilder,
        expr: &SymbolicExpression,
        binding: &ClarityName,
        already_used: bool,
        result_offset: walrus::LocalId,
        result_type: &TypeSignature,
    ) -> Result<InstrSeqId, GeneratorError> {
        if !already_used {
            return self.block_from_expr_into_memory(builder, expr, result_offset, result_type);
        }

        self.note_control_arity(0, clar2wasm_ty(result_type).len());
        let mut block = builder.dangling_instr_seq(None);
        generate_name_already_used_error(self, &mut block, binding)?;
        Ok(block.id())
    }

    /// Returns `true` if binding `name` here is what the interpreter calls
    /// `NameAlreadyUsed`.
    ///
    /// Mirrors `eval_with_new_binding` (`clarity/src/vm/functions/options.rs`),
    /// which is the only place a name is checked where it binds rather than where
    /// it is defined: a reserved word, one of the contract's own functions, or an
    /// enclosing local. The analyzer's `check_special_match` checks a narrower
    /// set — it misses reserved words other than `block-height`, and it misses
    /// *read-only* functions, which are absent from the map it consults — so a
    /// contract carrying either shape deploys and the disagreement is only
    /// visible when the branch runs.
    pub(crate) fn binding_name_already_used(&self, name: &ClarityName) -> bool {
        let analysis = &self.contract_analysis;
        self.is_reserved_name(name)
            || analysis.private_function_types.contains_key(name)
            || analysis.public_function_types.contains_key(name)
            || analysis.read_only_function_types.contains_key(name)
            || self.bindings.contains(name)
    }

    /// Returns `true` if `name` is already claimed by another contract-level definition.
    ///
    /// Mirrors the interpreter's `ContractContext::is_name_used` so the compiler can emit a
    /// `NameAlreadyUsed` runtime error for collisions the analyzer's `check_name_used` misses.
    pub(crate) fn is_already_used_name(&self, name: &ClarityName) -> bool {
        trait HasClarityName {
            fn has_key(&self, name: &ClarityName) -> bool;
        }

        impl<V> HasClarityName for std::collections::BTreeMap<ClarityName, V> {
            fn has_key(&self, name: &ClarityName) -> bool {
                self.contains_key(name)
            }
        }

        impl HasClarityName for std::collections::BTreeSet<ClarityName> {
            fn has_key(&self, name: &ClarityName) -> bool {
                self.contains(name)
            }
        }

        let ca = &self.contract_analysis;
        let define_maps: [&dyn HasClarityName; _] = [
            &ca.variable_types,
            &ca.persisted_variable_types,
            &ca.map_types,
            &ca.fungible_tokens,
            &ca.non_fungible_tokens,
            &ca.defined_traits,
        ];

        self.is_reserved_name(name)
            || self.defined_functions.contains(name.as_str())
            || define_maps.into_iter().any(|hk| hk.has_key(name))
    }
}
