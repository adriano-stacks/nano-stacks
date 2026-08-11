use clarity::vm::analysis::ContractAnalysis;
use clarity::vm::contexts::GlobalContext;
use clarity::vm::costs::cost_functions::ClarityCostFunction;
use clarity::vm::costs::{runtime_cost, CostTracker};
use clarity::vm::errors::{RuntimeError, VmExecutionError};
use clarity::vm::events::*;
use clarity::vm::types::signatures::CallableSubtype;
use clarity::vm::types::{
    AssetIdentifier, BuffData, CallableData, FunctionType, ListData, ListTypeData, OptionalData,
    PrincipalData, QualifiedContractIdentifier, ResponseData, SequenceData, SequenceSubtype,
    TupleData, TypeSignature,
};
use clarity::vm::{CallStack, ContractContext, Value};
use stacks_common::types::chainstate::StacksBlockId;
use stacks_common::types::StacksEpochId;
use wasmtime::{AsContextMut, Linker, Memory, Store, Val};

use crate::cost::{CostGlobals, CostMeter};
use crate::error::WasmError;
use crate::error_mapping;
use crate::linker::{link_cost_globals, link_host_functions};
use crate::runtime_shape::{RuntimeShapeArena, RuntimeShapeStore};
use crate::wasm_generator::{uses_packed_abi, uses_packed_value};
use crate::wasm_utils::*;
use crate::{CompiledContract, ModuleCache};

// The context used when making calls into the Wasm module.
pub struct ClarityWasmContext<'a, 'b, 'hooks> {
    pub global_context: &'a mut GlobalContext<'b, 'hooks>,
    contract_context: Option<&'a ContractContext>,
    contract_context_mut: Option<&'a mut ContractContext>,
    pub call_stack: &'a mut CallStack,
    pub sender: Option<PrincipalData>,
    pub caller: Option<PrincipalData>,
    pub sponsor: Option<PrincipalData>,
    // Stack of senders, used for `as-contract` expressions.
    sender_stack: Vec<PrincipalData>,
    /// Stack of callers, used for `contract-call?` and `as-contract` expressions.
    caller_stack: Vec<PrincipalData>,
    /// Stack of block hashes, used for `at-block` expressions.
    bhh_stack: Vec<StacksBlockId>,
    /// Contract analysis data, used for typing information, and only available
    /// when initializing a contract. Should always be `Some` when initializing
    /// a contract, and `None` otherwise.
    pub contract_analysis: Option<&'a ContractAnalysis>,
    pub cost_globals: Option<CostGlobals>,
    /// The instance's exported linear memory, cached at instantiation: a host
    /// function resolving it through `get_export("memory")` walked the export
    /// map by name on every one of millions of calls per replay.
    pub memory: Option<Memory>,
    /// Type signatures already parsed from this instance's memory, by the
    /// (offset, length) of their serialized text. The texts are constants the
    /// compiler placed in the data segment, so within one call the same
    /// coordinates always hold the same string — and parsing it fresh was
    /// twelve microseconds on each of millions of size measurements a mainnet
    /// replay makes.
    pub parsed_types: std::collections::HashMap<(i32, i32), TypeSignature>,
    pub module_cache: &'a ModuleCache,
    runtime_shapes: RuntimeShapeArena,
}

impl<'a, 'b, 'hooks> ClarityWasmContext<'a, 'b, 'hooks> {
    #[allow(clippy::too_many_arguments)]
    pub fn new_init(
        global_context: &'a mut GlobalContext<'b, 'hooks>,
        contract_context: &'a mut ContractContext,
        call_stack: &'a mut CallStack,
        sender: Option<PrincipalData>,
        caller: Option<PrincipalData>,
        sponsor: Option<PrincipalData>,
        contract_analysis: Option<&'a ContractAnalysis>,
        cost_globals: Option<CostGlobals>,
        module_cache: &'a ModuleCache,
    ) -> Self {
        ClarityWasmContext {
            global_context,
            contract_context: None,
            contract_context_mut: Some(contract_context),
            call_stack,
            sender,
            caller,
            sponsor,
            sender_stack: vec![],
            caller_stack: vec![],
            bhh_stack: vec![],
            contract_analysis,
            cost_globals,
            memory: None,
            parsed_types: std::collections::HashMap::new(),
            module_cache,
            runtime_shapes: RuntimeShapeArena::default(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_run(
        global_context: &'a mut GlobalContext<'b, 'hooks>,
        contract_context: &'a ContractContext,
        call_stack: &'a mut CallStack,
        sender: Option<PrincipalData>,
        caller: Option<PrincipalData>,
        sponsor: Option<PrincipalData>,
        contract_analysis: Option<&'a ContractAnalysis>,
        module_cache: &'a ModuleCache,
    ) -> Self {
        ClarityWasmContext {
            global_context,
            contract_context: Some(contract_context),
            contract_context_mut: None,
            call_stack,
            sender,
            caller,
            sponsor,
            sender_stack: vec![],
            caller_stack: vec![],
            bhh_stack: vec![],
            contract_analysis,
            cost_globals: None,
            memory: None,
            parsed_types: std::collections::HashMap::new(),
            module_cache,
            runtime_shapes: RuntimeShapeArena::default(),
        }
    }

    pub fn push_sender(&mut self, sender: PrincipalData) {
        if let Some(current) = self.sender.take() {
            self.sender_stack.push(current);
        }
        self.sender = Some(sender);
    }

    pub fn pop_sender(&mut self) -> Result<PrincipalData, VmExecutionError> {
        self.sender
            .take()
            .ok_or(RuntimeError::NoSenderInContext.into())
            .inspect(|_| {
                self.sender = self.sender_stack.pop();
            })
    }

    /// How deep the sender and caller stacks are, so a function can put them
    /// back where it found them.
    ///
    /// `as-contract` pushes on entry and pops on exit, but an early return out
    /// of its body — `asserts!` or `try!` inside it — branches straight past
    /// the pop. Restoring at the function boundary is what makes the leak
    /// impossible rather than merely unlikely: the next call then cannot
    /// inherit a sender the previous one left switched.
    #[must_use]
    pub fn principal_depth(&self) -> (usize, usize) {
        (self.sender_stack.len(), self.caller_stack.len())
    }

    /// Unwind the sender and caller stacks back to a recorded depth.
    pub fn restore_principal_depth(&mut self, (sender, caller): (usize, usize)) {
        while self.sender_stack.len() > sender {
            let _ = self.pop_sender();
        }
        while self.caller_stack.len() > caller {
            let _ = self.pop_caller();
        }
    }

    pub fn push_caller(&mut self, caller: PrincipalData) {
        if let Some(current) = self.caller.take() {
            self.caller_stack.push(current);
        }
        self.caller = Some(caller);
    }

    pub fn pop_caller(&mut self) -> Result<PrincipalData, VmExecutionError> {
        self.caller
            .take()
            .ok_or(RuntimeError::NoCallerInContext.into())
            .inspect(|_| {
                self.caller = self.caller_stack.pop();
            })
    }

    pub fn push_at_block(&mut self, bhh: StacksBlockId) {
        self.bhh_stack.push(bhh);
    }

    pub fn pop_at_block(&mut self) -> Result<StacksBlockId, VmExecutionError> {
        self.bhh_stack
            .pop()
            .ok_or(crate::error::wasm_error(WasmError::WasmGeneratorError(
                "Could not pop at_block".to_string(),
            )))
    }

    /// Return an immutable reference to the contract_context
    pub fn contract_context(&self) -> &ContractContext {
        if let Some(contract_context) = &self.contract_context {
            contract_context
        } else if let Some(contract_context) = &self.contract_context_mut {
            contract_context
        } else {
            unreachable!("contract_context and contract_context_mut are both None")
        }
    }

    /// Return a mutable reference to the contract_context if we are currently
    /// initializing a contract, else, return an error.
    pub fn contract_context_mut(&mut self) -> Result<&mut ContractContext, VmExecutionError> {
        match &mut self.contract_context_mut {
            Some(contract_context) => Ok(contract_context),
            None => Err(crate::error::wasm_error(
                WasmError::DefineFunctionCalledInRunMode,
            )),
        }
    }

    pub fn push_to_event_batch(&mut self, event: StacksTransactionEvent) {
        if let Some(batch) = self.global_context.event_batches.last_mut() {
            batch.0.events.push(event);
        }
    }

    pub fn construct_print_transaction_event(
        contract_id: &QualifiedContractIdentifier,
        value: &Value,
    ) -> StacksTransactionEvent {
        let print_event = SmartContractEventData {
            key: (contract_id.clone(), "print".to_string()),
            value: value.clone(),
        };

        StacksTransactionEvent::SmartContractEvent(print_event)
    }

    pub fn register_print_event(&mut self, value: Value) -> Result<(), VmExecutionError> {
        let event = Self::construct_print_transaction_event(
            &self.contract_context().contract_identifier,
            &value,
        );

        self.push_to_event_batch(event);
        Ok(())
    }

    pub fn register_stx_transfer_event(
        &mut self,
        sender: PrincipalData,
        recipient: PrincipalData,
        amount: u128,
        memo: BuffData,
    ) -> Result<(), VmExecutionError> {
        let event_data = STXTransferEventData {
            sender,
            recipient,
            amount,
            memo,
        };
        let event = StacksTransactionEvent::STXEvent(STXEventType::STXTransferEvent(event_data));

        self.push_to_event_batch(event);
        Ok(())
    }

    pub fn register_stx_burn_event(
        &mut self,
        sender: PrincipalData,
        amount: u128,
    ) -> Result<(), VmExecutionError> {
        let event_data = STXBurnEventData { sender, amount };
        let event = StacksTransactionEvent::STXEvent(STXEventType::STXBurnEvent(event_data));

        self.push_to_event_batch(event);
        Ok(())
    }

    pub fn register_nft_transfer_event(
        &mut self,
        sender: PrincipalData,
        recipient: PrincipalData,
        value: Value,
        asset_identifier: AssetIdentifier,
    ) -> Result<(), VmExecutionError> {
        let event_data = NFTTransferEventData {
            sender,
            recipient,
            asset_identifier,
            value,
        };
        let event = StacksTransactionEvent::NFTEvent(NFTEventType::NFTTransferEvent(event_data));

        self.push_to_event_batch(event);
        Ok(())
    }

    pub fn register_nft_mint_event(
        &mut self,
        recipient: PrincipalData,
        value: Value,
        asset_identifier: AssetIdentifier,
    ) -> Result<(), VmExecutionError> {
        let event_data = NFTMintEventData {
            recipient,
            asset_identifier,
            value,
        };
        let event = StacksTransactionEvent::NFTEvent(NFTEventType::NFTMintEvent(event_data));

        self.push_to_event_batch(event);
        Ok(())
    }

    pub fn register_nft_burn_event(
        &mut self,
        sender: PrincipalData,
        value: Value,
        asset_identifier: AssetIdentifier,
    ) -> Result<(), VmExecutionError> {
        let event_data = NFTBurnEventData {
            sender,
            asset_identifier,
            value,
        };
        let event = StacksTransactionEvent::NFTEvent(NFTEventType::NFTBurnEvent(event_data));

        self.push_to_event_batch(event);
        Ok(())
    }

    pub fn register_ft_transfer_event(
        &mut self,
        sender: PrincipalData,
        recipient: PrincipalData,
        amount: u128,
        asset_identifier: AssetIdentifier,
    ) -> Result<(), VmExecutionError> {
        let event_data = FTTransferEventData {
            sender,
            recipient,
            asset_identifier,
            amount,
        };
        let event = StacksTransactionEvent::FTEvent(FTEventType::FTTransferEvent(event_data));

        self.push_to_event_batch(event);
        Ok(())
    }

    pub fn register_ft_mint_event(
        &mut self,
        recipient: PrincipalData,
        amount: u128,
        asset_identifier: AssetIdentifier,
    ) -> Result<(), VmExecutionError> {
        let event_data = FTMintEventData {
            recipient,
            asset_identifier,
            amount,
        };
        let event = StacksTransactionEvent::FTEvent(FTEventType::FTMintEvent(event_data));

        self.push_to_event_batch(event);
        Ok(())
    }

    pub fn register_ft_burn_event(
        &mut self,
        sender: PrincipalData,
        amount: u128,
        asset_identifier: AssetIdentifier,
    ) -> Result<(), VmExecutionError> {
        let event_data = FTBurnEventData {
            sender,
            asset_identifier,
            amount,
        };
        let event = StacksTransactionEvent::FTEvent(FTEventType::FTBurnEvent(event_data));

        self.push_to_event_batch(event);
        Ok(())
    }
}

impl RuntimeShapeStore for ClarityWasmContext<'_, '_, '_> {
    fn runtime_shapes(&self) -> Option<&RuntimeShapeArena> {
        Some(&self.runtime_shapes)
    }

    fn runtime_shapes_mut(&mut self) -> Option<&mut RuntimeShapeArena> {
        Some(&mut self.runtime_shapes)
    }
}

/// Successful return of a contract initialization
///
/// Contains the result of the execution of the top-level expressions, and the cost of executing
/// them.
#[derive(Debug, PartialEq)]
pub struct ContractInitReturn {
    pub ret: Option<Value>,
    pub cost: CostMeter,
}

/// Initialize a contract, executing all of the top-level expressions and
/// registering all of the definitions in the context. Returns the value
/// returned from the last top-level expression.
pub fn initialize_contract(
    global_context: &mut GlobalContext,
    contract_context: &mut ContractContext,
    sponsor: Option<PrincipalData>,
    contract_analysis: &ContractAnalysis,
    wasm: &[u8],
    module_cache: &ModuleCache,
) -> Result<ContractInitReturn, VmExecutionError> {
    let publisher: PrincipalData = contract_context.contract_identifier.issuer.clone().into();

    let mut call_stack = CallStack::new();
    let epoch = global_context.epoch_id;
    let clarity_version = *contract_context.get_clarity_version();
    // A deploy's top level runs with this raised and every later call to the
    // same contract runs with it lowered, exactly as `Contract::initialize_from_ast`
    // brackets `eval_all` with it. It is what `contract-call?` through a constant
    // consults: the constant's value is not frozen until the deploy that defines
    // it has finished, so the reference refuses to dispatch through one here and
    // dispatches through the same one afterwards.
    contract_context.is_deploying = true;
    // One engine for every module the cache holds: a module can only be
    // instantiated in a store belonging to the engine that built it.
    let engine = module_cache.engine().clone();
    let module = module_cache.native_module(wasm)?;
    let init_context = ClarityWasmContext::new_init(
        global_context,
        contract_context,
        &mut call_stack,
        Some(publisher.clone()),
        Some(publisher),
        sponsor.clone(),
        Some(contract_analysis),
        None,
        module_cache,
    );
    let mut store = Store::new(&engine, init_context);
    let mut linker = Linker::new(&engine);
    // Link in the host interface functions and globals.
    link_host_functions(&mut linker)?;
    store.data_mut().cost_globals = Some(
        link_cost_globals(&mut linker, &mut store.as_context_mut())
            .map_err(|e| crate::error::wasm_error(WasmError::UnableToLoadModule(e.into())))?,
    );

    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|e| crate::error::wasm_error(WasmError::UnableToLoadModule(e)))?;

    // Call the `.top-level` function, which contains all top-level expressions
    // from the contract.
    let top_level = instance
        .get_func(&mut store, ".top-level")
        .ok_or(crate::error::wasm_error(WasmError::DefinesNotFound))?;

    // Get the type of the last top-level expression with a return value.
    let return_type = contract_analysis.expressions.iter().rev().find_map(|expr| {
        contract_analysis
            .type_map
            .as_ref()
            .and_then(|type_map| type_map.get_type_expected(expr))
    });
    let packed_top_level = return_type.is_some_and(uses_packed_value);
    let memory = instance
        .get_memory(&mut store, "memory")
        .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;
    store.data_mut().memory = Some(memory);
    let packed_return_offset =
        if let Some(return_type) = return_type.filter(|_| packed_top_level) {
            let stack_pointer = instance.get_global(&mut store, "stack-pointer").ok_or(
                crate::error::wasm_error(WasmError::GlobalNotFound("stack-pointer".into())),
            )?;
            let offset = stack_pointer
                .get(&mut store)
                .i32()
                .ok_or(crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
            stack_pointer
                .set(&mut store, Val::I32(offset + get_type_size(return_type)))
                .map_err(|error| crate::error::wasm_error(WasmError::Runtime(error)))?;
            Some(offset)
        } else {
            None
        };
    let arguments = packed_return_offset.map_or_else(Vec::new, |offset| vec![Val::I32(offset)]);
    let mut results = if packed_top_level {
        Vec::new()
    } else {
        top_level
            .ty(&mut store)
            .results()
            .map(placeholder_for_type)
            .collect()
    };

    top_level
        .call(&mut store, &arguments, results.as_mut_slice())
        .map_err(|e| {
            error_mapping::resolve_error(e, instance, &mut store, &epoch, &clarity_version)
        })?;

    // Lowered only once the top level has succeeded, as the reference lowers it:
    // a failed deploy publishes no contract at all, so the flag it left behind is
    // unobservable.
    store.data_mut().contract_context_mut()?.is_deploying = false;

    let ret = if let Some(return_type) = return_type {
        if let Some(return_offset) = packed_return_offset {
            Some(read_from_wasm_indirect(
                memory,
                &mut store,
                return_type,
                return_offset,
                epoch,
            )?)
        } else {
            wasm_to_clarity_value(return_type, 0, &results, memory, &mut &mut store, epoch)
                .map(|(val, _offset)| val)?
        }
    } else {
        None
    };

    let remaining = store
        .data()
        .cost_globals
        .ok_or(crate::error::wasm_error(WasmError::GlobalNotFound(
            "cost-*".to_string(),
        )))?
        .remaining_costs(&mut store)
        .map_err(|e| crate::error::wasm_error(WasmError::UnableToLoadModule(e)))?;
    let cost = CostMeter::used_from_remaining(remaining);

    Ok(ContractInitReturn { ret, cost })
}

/// Charge what a refused call was already charged before it was refused.
///
/// `DefinedFunction::execute_apply` charges `UserFunctionApplication` and one
/// `InnerTypeCheckCost` per argument *before* it type-checks any argument, so a
/// call refused for a mistyped one has still paid for all of them. A compiled
/// contract charges the same things in the function's own prelude, which a call
/// refused at this boundary never enters — so the refusal pays them here, and
/// the cost in the receipt is the cost the chain records.
fn charge_refused_application(
    tracker: &mut GlobalContext,
    arguments: &[Value],
    argument_sizes: Option<&[u64]>,
    expected: &[TypeSignature],
    epoch: StacksEpochId,
) -> Result<(), VmExecutionError> {
    runtime_cost(
        ClarityCostFunction::UserFunctionApplication,
        tracker,
        expected.len(),
    )?;
    // From 3.3 an argument is checked at the size of the value passed, not of
    // the type declared (`callables.rs`, `uses_arg_size_for_cost`) — which is
    // also why the two branches count different things when the call was
    // refused for having the wrong number of arguments in the first place.
    if epoch.uses_arg_size_for_cost() {
        if argument_sizes.is_some_and(|sizes| sizes.len() != arguments.len()) {
            return Err(crate::error::wasm_error(WasmError::ValueTypeMismatch));
        }
        for (index, argument) in arguments.iter().enumerate() {
            let size = match argument_sizes {
                Some(sizes) => sizes[index],
                None => u64::from(argument.size()?),
            };
            runtime_cost(ClarityCostFunction::InnerTypeCheckCost, tracker, size)?;
        }
    } else {
        for expected_type in expected {
            runtime_cost(
                ClarityCostFunction::InnerTypeCheckCost,
                tracker,
                expected_type.size()?,
            )?;
        }
    }
    Ok(())
}

/// Apply the same Clarity 2+ function-entry conversion as the interpreter.
///
/// The original value is kept for the refusal because that is what
/// `DefinedFunction::execute_apply` names when an implicit cast or Epoch 4
/// sanitization fails.
pub(crate) fn admit_function_argument(
    expected_type: &TypeSignature,
    argument: &Value,
    epoch: StacksEpochId,
) -> Result<Value, VmExecutionError> {
    let cast_argument = implicit_contract_cast(expected_type, argument)?;
    let admitted = if epoch.sanitize_in_function_invocation() {
        Value::sanitize_value(&epoch, expected_type, cast_argument)
            .ok_or_else(|| {
                clarity::vm::errors::RuntimeCheckErrorKind::TypeValueError(
                    Box::new(expected_type.clone()),
                    argument.to_error_string(),
                )
            })?
            .0
    } else {
        cast_argument
    };

    if !expected_type.admits(&epoch, &admitted)? {
        return Err(clarity::vm::errors::RuntimeCheckErrorKind::TypeValueError(
            Box::new(expected_type.clone()),
            argument.to_error_string(),
        )
        .into());
    }
    Ok(admitted)
}

#[allow(clippy::too_many_arguments)]
pub fn call_function(
    function_name: &str,
    arguments: &[Value],
    module: &CompiledContract,
    global_context: &mut GlobalContext,
    contract_context: &ContractContext,
    call_stack: &mut CallStack,
    sender: Option<PrincipalData>,
    caller: Option<PrincipalData>,
    sponsor: Option<PrincipalData>,
    module_cache: &ModuleCache,
) -> Result<Value, VmExecutionError> {
    call_function_with_argument_sizes(
        function_name,
        arguments,
        None,
        module,
        global_context,
        contract_context,
        call_stack,
        sender,
        caller,
        sponsor,
        module_cache,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn call_function_with_argument_sizes(
    function_name: &str,
    arguments: &[Value],
    argument_sizes: Option<&[u64]>,
    module: &CompiledContract,
    global_context: &mut GlobalContext,
    contract_context: &ContractContext,
    call_stack: &mut CallStack,
    sender: Option<PrincipalData>,
    caller: Option<PrincipalData>,
    sponsor: Option<PrincipalData>,
    module_cache: &ModuleCache,
) -> Result<Value, VmExecutionError> {
    let contract_size = global_context
        .database
        .get_contract_size(&contract_context.contract_identifier)?;
    runtime_cost(
        ClarityCostFunction::LoadContract,
        global_context,
        contract_size,
    )?;
    let function = contract_context
        .lookup_function(function_name)
        .ok_or_else(|| {
            clarity::vm::errors::RuntimeCheckErrorKind::UndefinedFunction(function_name.into())
        })?;
    let function_type = module
        .analysis
        .get_public_function_type(function_name)
        .or_else(|| module.analysis.get_read_only_function_type(function_name))
        .or_else(|| module.analysis.get_private_function(function_name))
        .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(
            function_name.into(),
        )))?;
    let fixed_function = match function_type {
        FunctionType::Fixed(function) => function,
        _ => {
            return Err(crate::error::wasm_error(WasmError::InvalidFunctionKind(
                function_name.into(),
            )));
        }
    };
    let return_type = fixed_function.returns.clone();
    let packed_abi = uses_packed_abi(fixed_function);
    let expected_arguments = function.get_arg_types();
    let read_only = function.is_read_only();
    let epoch = global_context.epoch_id;
    // Entering the function and type-checking its arguments is charged by the
    // function's own prelude, which runs whoever calls it — except when the call
    // is refused here and never enters it.
    if arguments.len() != expected_arguments.len() {
        charge_refused_application(
            global_context,
            arguments,
            argument_sizes,
            expected_arguments,
            epoch,
        )?;
        return Err(
            clarity::vm::errors::RuntimeCheckErrorKind::IncorrectArgumentCount(
                expected_arguments.len(),
                arguments.len(),
            )
            .into(),
        );
    }

    let clarity_version = *contract_context.get_clarity_version();
    let engine = module_cache.engine().clone();
    // Native code for a contract already called in this process comes back
    // without touching Cranelift, which is the whole reason a replay moves.
    let wasm_module = module.native(module_cache)?.clone();
    let context = ClarityWasmContext::new_run(
        global_context,
        contract_context,
        call_stack,
        sender.clone(),
        caller,
        sponsor.clone(),
        Some(&module.analysis),
        module_cache,
    );
    let mut store = Store::new(&engine, context);
    let mut linker = crate::phases::time(crate::phases::Phase::LinkerSetup, || {
        let mut linker = Linker::new(&engine);
        link_host_functions(&mut linker)?;
        Ok::<_, VmExecutionError>(linker)
    })?;
    let cost_globals = link_cost_globals(&mut linker, &mut store)?;
    store.data_mut().cost_globals = Some(cost_globals);
    let instance = crate::phases::time(crate::phases::Phase::Instantiate, || {
        linker
            .instantiate(&mut store, &wasm_module)
            .map_err(|error| crate::error::wasm_error(WasmError::UnableToLoadModule(error)))
    })?;
    let call_setup = crate::phases::start();
    let memory = instance
        .get_memory(&mut store, "memory")
        .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;
    store.data_mut().memory = Some(memory);
    let stack_pointer =
        instance
            .get_global(&mut store, "stack-pointer")
            .ok_or(crate::error::wasm_error(WasmError::GlobalNotFound(
                "stack-pointer".into(),
            )))?;
    let mut offset = stack_pointer
        .get(&mut store)
        .i32()
        .ok_or(crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
    let argument_sizes_global =
        instance
            .get_global(&mut store, "argument-sizes")
            .ok_or(crate::error::wasm_error(WasmError::GlobalNotFound(
                "argument-sizes".into(),
            )))?;
    let argument_sizes_offset = argument_sizes_global
        .get(&mut store)
        .i32()
        .ok_or(crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
    if argument_sizes.is_some_and(|sizes| sizes.len() != arguments.len()) {
        return Err(crate::error::wasm_error(WasmError::ValueTypeMismatch));
    }
    for (index, argument) in arguments.iter().enumerate() {
        let byte_index = i32::try_from(index)
            .ok()
            .and_then(|index| index.checked_mul(4))
            .ok_or(crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
        let size = match argument_sizes {
            Some(sizes) => sizes[index],
            None => u64::from(argument.size()?),
        };
        let size = i32::try_from(size)
            .map_err(|_| crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
        let size_offset = argument_sizes_offset
            .checked_add(byte_index)
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or(crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
        memory
            .write(&mut store, size_offset, &size.to_le_bytes())
            .map_err(|error| {
                crate::error::wasm_error(WasmError::UnableToWriteMemory(error.into()))
            })?;
    }
    let arguments_offset = offset;
    let mut representation_offset = offset;
    let mut in_memory_offset = if packed_abi {
        offset + expected_arguments.iter().map(get_type_size).sum::<i32>()
    } else {
        offset
    };
    let mut wasm_arguments = Vec::new();
    for (argument, expected_type) in arguments.iter().zip(expected_arguments) {
        let argument = match admit_function_argument(expected_type, argument, epoch) {
            Ok(argument) => argument,
            Err(error) => {
                charge_refused_application(
                    store.data_mut().global_context,
                    arguments,
                    argument_sizes,
                    expected_arguments,
                    epoch,
                )?;
                return Err(error);
            }
        };
        if packed_abi {
            let (written, in_memory_written) = write_to_wasm(
                &mut store,
                memory,
                expected_type,
                representation_offset,
                in_memory_offset,
                &argument,
                true,
            )?;
            representation_offset += written;
            in_memory_offset += in_memory_written;
        } else {
            let (values, next_offset) =
                pass_argument_to_wasm(memory, &mut store, expected_type, &argument, offset)?;
            wasm_arguments.extend(values);
            offset = next_offset;
        }
    }
    let packed_return_offset = packed_abi.then_some(in_memory_offset);
    if let Some(return_offset) = packed_return_offset {
        wasm_arguments.extend([Val::I32(arguments_offset), Val::I32(return_offset)]);
        offset = return_offset + get_type_size(&return_type);
    }
    stack_pointer
        .set(&mut store, Val::I32(offset))
        .map_err(|error| crate::error::wasm_error(WasmError::Runtime(error)))?;
    let wasm_function = instance
        .get_func(&mut store, function_name)
        .ok_or_else(|| {
            clarity::vm::errors::RuntimeCheckErrorKind::UndefinedFunction(function_name.into())
        })?;
    let mut results = if packed_abi {
        Vec::new()
    } else {
        wasm_value_types(&return_type)
            .into_iter()
            .map(placeholder_for_type)
            .collect::<Vec<_>>()
    };
    if read_only {
        store.data_mut().global_context.begin_read_only();
    } else {
        store.data_mut().global_context.begin();
    }
    crate::phases::finish(crate::phases::Phase::CallSetup, call_setup);
    let call_result = crate::phases::time(crate::phases::Phase::WasmInvoke, || {
        wasm_function.call(&mut store, &wasm_arguments, &mut results)
    });
    let return_read = crate::phases::start();
    let execution_result = (|| {
        let remaining = store
            .data()
            .cost_globals
            .ok_or(crate::error::wasm_error(WasmError::GlobalNotFound(
                "cost-*".to_string(),
            )))?
            .remaining_costs(&mut store)
            .map_err(|error| crate::error::wasm_error(WasmError::UnableToLoadModule(error)))?;
        let cost = CostMeter::used_from_remaining(remaining);
        store
            .data_mut()
            .global_context
            .cost_track
            .add_cost(cost.into())?;
        call_result.map_err(|error| {
            error_mapping::resolve_error(error, instance, &mut store, &epoch, &clarity_version)
        })?;
        if let Some(return_offset) = packed_return_offset {
            read_from_wasm_indirect(memory, &mut store, &return_type, return_offset, epoch)
        } else {
            wasm_to_clarity_value(&return_type, 0, &results, memory, &mut &mut store, epoch)?
                .0
                .ok_or(crate::error::wasm_error(WasmError::Expect(
                    "function returned no value".into(),
                )))
        }
    })();
    crate::phases::finish(crate::phases::Phase::ReturnRead, return_read);
    let value = if read_only {
        store.data_mut().global_context.roll_back()?;
        execution_result?
    } else {
        store
            .data_mut()
            .global_context
            .handle_tx_result(execution_result, false)?
    };
    drop(store);
    if let Some(handler) = global_context.database.get_cc_special_cases_handler() {
        handler(
            global_context,
            sender.as_ref(),
            sponsor.as_ref(),
            &contract_context.contract_identifier,
            function_name,
            arguments,
            &value,
        )?;
    }
    Ok(value)
}

/// Re-tag contract principals as callables wherever a trait is expected.
///
/// A transaction argument arrives as consensus-serialized Clarity, where a
/// trait reference is indistinguishable from a contract principal, so it has to
/// be recovered from the type the function declares. Casting only the outermost
/// value left every nested one — a trait inside a tuple inside a list, which is
/// what a router's `(list 5 (tuple ... (pool-trait <trait>) ...))` argument is —
/// failing `admits` and raising a type error on a call the network accepted.
/// A trait already carried by the value is re-tagged, not only a bare principal.
/// `clarity2_implicit_cast` casts "principals to traits **and traits to other
/// traits**", and nano implemented only the first half. A value that reached the
/// callee already tagged as some other trait — an inner call's argument, or a
/// field a caller had cast for its own signature — kept that tag, `admits` saw a
/// trait the callee does not declare, and the call was refused. On mainnet that
/// was a router taking `(list 100 {asset: <ft-trait>, lp-token: <ft-trait>, …})`
/// a field of which arrived tagged `<ft-mint-trait>`: block 8724865 refused a
/// transaction the network executed, and the state roots parted
/// ([[097-cast-a-trait-argument-the-callee-declares-differently]]).
///
/// Each composite is first rebuilt carrying the type the callee declared, just
/// like `clarity2_implicit_cast`. Epoch 4 then sanitizes that intermediate value
/// against the declaration, deriving the runtime shape from what it contains.
fn implicit_contract_cast(
    expected_type: &TypeSignature,
    argument: &Value,
) -> Result<Value, VmExecutionError> {
    Ok(match (expected_type, argument) {
        (
            TypeSignature::CallableType(CallableSubtype::Trait(trait_identifier)),
            Value::CallableContract(callable),
        ) => Value::CallableContract(CallableData {
            contract_identifier: callable.contract_identifier.clone(),
            trait_identifier: Some(Box::new(trait_identifier.clone())),
        }),
        (
            TypeSignature::CallableType(CallableSubtype::Trait(trait_identifier)),
            Value::Principal(PrincipalData::Contract(contract_identifier)),
        ) => Value::CallableContract(CallableData {
            contract_identifier: contract_identifier.clone(),
            trait_identifier: Some(Box::new(trait_identifier.clone())),
        }),
        (
            TypeSignature::SequenceType(SequenceSubtype::ListType(list_type)),
            Value::Sequence(SequenceData::List(list)),
        ) => {
            let entry_type = list_type.get_list_item_type();
            let mut cast = Vec::with_capacity(list.data.len());
            for item in &list.data {
                cast.push(implicit_contract_cast(entry_type, item)?);
            }
            // The declared entry type over the *value's* length, which is what
            // `clarity2_implicit_cast` builds. Deriving the type from the cast
            // elements instead — `cons_list_unsanitized` — answers the least
            // supertype of what happens to be in the list and its actual length,
            // so a shorter list or a heterogeneous one came out carrying a
            // signature the callee never declared.
            Value::Sequence(SequenceData::List(ListData {
                data: cast,
                type_signature: ListTypeData::new_list(
                    entry_type.clone(),
                    list.type_signature.get_max_len(),
                )?,
            }))
        }
        (TypeSignature::TupleType(tuple_type), Value::Tuple(tuple)) => {
            let mut cast = std::collections::BTreeMap::new();
            for (name, value) in &tuple.data_map {
                let Some(field) = tuple_type.field_type(name) else {
                    return Err(clarity::vm::errors::RuntimeCheckErrorKind::TypeValueError(
                        Box::new(expected_type.clone()),
                        argument.to_error_string(),
                    )
                    .into());
                };
                cast.insert(name.clone(), implicit_contract_cast(field, value)?);
            }
            Value::Tuple(TupleData {
                type_signature: tuple_type.clone(),
                data_map: cast,
            })
        }
        (
            TypeSignature::OptionalType(inner),
            Value::Optional(OptionalData { data: Some(value) }),
        ) => Value::Optional(OptionalData {
            data: Some(Box::new(implicit_contract_cast(inner, value)?)),
        }),
        (TypeSignature::ResponseType(inner), Value::Response(response)) => {
            let expected = if response.committed {
                &inner.0
            } else {
                &inner.1
            };
            Value::Response(ResponseData {
                committed: response.committed,
                data: Box::new(implicit_contract_cast(expected, &response.data)?),
            })
        }
        _ => argument.clone(),
    })
}

/// A transaction argument the callee's type refuses.
///
/// Nothing here is epoch-gated, which is why an audit of the epoch-gated runtime
/// predicates found it: `execute_apply` reaches the same refusal through
/// `sanitize_in_function_invocation()`, a predicate new in 4.0, and the two
/// engines were measured to see whether they agreed at 4.0. They did not, and
/// the disagreement was in the two things a refused transaction leaves behind —
/// its `vm_error` string and its cost — neither of which a state root can see,
/// because a refused call writes nothing.
///
/// [`crosscheck_cost`] asserts both: the returned value or error, and all five
/// cost dimensions, for the same call through both engines.
#[cfg(test)]
mod refused_arguments {
    use clarity::vm::types::signatures::CallableSubtype;
    use clarity::vm::types::{
        CallableData, ListTypeData, QualifiedContractIdentifier, SequenceSubtype, TraitIdentifier,
        TupleData, TypeSignature,
    };
    use clarity::vm::{ClarityName, Value};

    use crate::tools::crosscheck_cost;

    fn name(name: &str) -> ClarityName {
        #[allow(clippy::expect_used)]
        ClarityName::try_from(name).expect("a Clarity name")
    }

    /// A tuple carrying a field the parameter does not declare.
    ///
    /// The shape a transaction produces, since a contract-call's arguments are
    /// deserialized without the callee's type in hand. `clarity2_implicit_cast`
    /// refuses it by naming the value; the compiler named its type.
    #[test]
    fn a_wider_tuple_is_refused_the_way_the_interpreter_refuses_it() {
        let wide = Value::Tuple(
            TupleData::from_data(vec![
                (name("x"), Value::UInt(1)),
                (name("y"), Value::UInt(2)),
            ])
            .expect("a tuple"),
        );
        crosscheck_cost(
            "(define-public (f (a {x: uint})) (ok (get x a)))",
            "f",
            &[wide],
        );
    }

    /// A trait argument already tagged as some *other* trait is re-tagged.
    ///
    /// `clarity2_implicit_cast` casts "principals to traits and **traits to
    /// other traits**", and only the first half was implemented: a value that
    /// arrived already tagged kept its tag, `admits` saw a trait the callee does
    /// not declare, and the call was refused where the interpreter runs it.
    /// Mainnet block 8724865 is this, nested exactly as here — the tag on a
    /// trait field of a tuple inside a list
    /// ([[097-cast-a-trait-argument-the-callee-declares-differently]]).
    ///
    /// Asserted on the cast itself, which is how the interpreter tests its own
    /// (`clarity/src/vm/callables.rs::test_implicit_cast`): a snippet cannot
    /// reach it, because `admits` is the only thing downstream that can tell the
    /// two tags apart and both engines agree once the value is past it.
    #[test]
    fn a_trait_tagged_as_another_trait_is_re_tagged_as_the_callee_declares_it() {
        #[allow(clippy::expect_used)]
        let elsewhere = TraitIdentifier::parse_fully_qualified(
            "SP2VCQJGH7PHP2DJK7Z0V48AGBHQAW3R3ZW1QF4N.traits.ft-mint-trait",
        )
        .expect("a trait identifier");
        #[allow(clippy::expect_used)]
        let declared = TraitIdentifier::parse_fully_qualified(
            "SP2VCQJGH7PHP2DJK7Z0V48AGBHQAW3R3ZW1QF4N.traits.ft-trait",
        )
        .expect("a trait identifier");
        #[allow(clippy::expect_used)]
        let token =
            QualifiedContractIdentifier::parse("SP2VCQJGH7PHP2DJK7Z0V48AGBHQAW3R3ZW1QF4N.token")
                .expect("a contract identifier");
        let tagged = |trait_identifier: &TraitIdentifier| {
            Value::CallableContract(CallableData {
                contract_identifier: token.clone(),
                trait_identifier: Some(Box::new(trait_identifier.clone())),
            })
        };
        // A tuple in a list, which is the mainnet router's argument shape and
        // the one a cast that recursed only into the outermost value missed.
        let listed = |value: Value| {
            #[allow(clippy::expect_used)]
            Value::cons_list_unsanitized(vec![Value::Tuple(
                TupleData::from_data(vec![(name("asset"), value)]).expect("a tuple"),
            )])
            .expect("a list")
        };
        let expected_type = TypeSignature::SequenceType(SequenceSubtype::ListType(
            #[allow(clippy::expect_used)]
            ListTypeData::new_list(
                TypeSignature::TupleType(
                    #[allow(clippy::expect_used)]
                    vec![(
                        name("asset"),
                        TypeSignature::CallableType(CallableSubtype::Trait(declared.clone())),
                    )]
                    .try_into()
                    .expect("a tuple type"),
                ),
                4,
            )
            .expect("a list type"),
        ));

        #[allow(clippy::expect_used)]
        let cast = super::implicit_contract_cast(&expected_type, &listed(tagged(&elsewhere)))
            .expect("the cast answers");

        assert_eq!(
            cast,
            listed(tagged(&declared)),
            "a trait tag the callee does not declare survived the cast"
        );
        assert!(
            expected_type
                .admits(&crate::tools::TestConfig::latest_epoch(), &cast)
                .expect("admits answers"),
            "the cast value is one the callee's own type refuses"
        );
    }

    #[test]
    fn an_argument_of_the_wrong_type_is_refused_the_same_way() {
        crosscheck_cost("(define-public (f (a uint)) (ok a))", "f", &[Value::Int(1)]);
    }

    #[test]
    fn a_sequence_longer_than_the_parameter_is_refused_the_same_way() {
        crosscheck_cost(
            "(define-public (f (a (buff 2))) (ok a))",
            "f",
            &[Value::buff_from(vec![1, 2, 3, 4]).expect("a buffer")],
        );
    }

    /// The wrong number of arguments, in both directions.
    ///
    /// `execute_apply` charges the application and one type check per argument
    /// *before* it counts them, so this refusal is not free either — and the
    /// count it charges over is the passed arguments at 4.0 and the declared
    /// parameters before 3.3, which is why the two directions are both here.
    #[test]
    fn too_many_arguments_cost_what_the_interpreter_charges_for_them() {
        crosscheck_cost(
            "(define-public (f (a uint)) (ok a))",
            "f",
            &[Value::UInt(1), Value::UInt(2)],
        );
    }

    #[test]
    fn too_few_arguments_cost_what_the_interpreter_charges_for_them() {
        crosscheck_cost("(define-public (f (a uint)) (ok a))", "f", &[]);
    }
}
