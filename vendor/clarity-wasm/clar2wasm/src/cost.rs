//! Functionality to track the costs of running Clarity.
//!
//! The cost computations in this module are meant to be a full match with the interpreter
//! implementation of the Clarity runtime.

mod clar1;
mod clar2;
mod clar3;
mod clar4;
mod clar5;

use clarity::types::StacksEpochId;
use clarity::vm::ClarityName;
use clarity_types::ClarityTypeError;
use walrus::ir::{BinaryOp, Instr, UnaryOp, Unop};
use walrus::{FunctionId, GlobalId, InstrSeqBuilder, LocalId, Module};
use wasmtime::{AsContextMut, Global, Val};

use crate::error_mapping::ErrorMap;
use crate::wasm_generator::{GeneratorError, WasmGenerator};
use crate::words::{ComplexWord, Word};

type Result<T, E = GeneratorError> = std::result::Result<T, E>;

/// helper function to either charge a cost inside a result
/// or throw a signature type size check error
pub fn charge_ok_or_throw_runtime_error(
    cost: &Result<u32, ClarityTypeError>,
    generator: &mut WasmGenerator,
    builder: &mut walrus::InstrSeqBuilder,
    word: &dyn ComplexWord,
) -> Result<(), GeneratorError> {
    if let Ok(cost) = cost {
        word.charge(generator, builder, *cost)?;
    } else {
        builder
            .i32_const(ErrorMap::SignatureTypeSizeCheckError as i32)
            .call(generator.func_by_name("stdlib.runtime-error"));
    }
    Ok(())
}

#[derive(Debug)]
pub enum Cost {
    Runtime,
    ReadCount,
    ReadLength,
    WriteCount,
    WriteLength,
}

impl Cost {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Runtime => "cost-runtime",
            Self::ReadCount => "cost-read-count",
            Self::ReadLength => "cost-read-length",
            Self::WriteCount => "cost-write-count",
            Self::WriteLength => "cost-write-length",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostMeter {
    pub runtime: i64,
    pub read_count: i64,
    pub read_length: i64,
    pub write_count: i64,
    pub write_length: i64,
}

impl CostMeter {
    pub const INIT: Self = Self {
        runtime: i64::MAX,
        read_count: i64::MAX,
        read_length: i64::MAX,
        write_count: i64::MAX,
        write_length: i64::MAX,
    };

    pub const ZERO: Self = Self {
        runtime: 0,
        read_count: 0,
        read_length: 0,
        write_count: 0,
        write_length: 0,
    };

    /// The cost between two meter readings, clamped at zero per dimension.
    #[must_use]
    pub fn saturating_sub(self, earlier: Self) -> Self {
        Self {
            runtime: self.runtime.saturating_sub(earlier.runtime),
            read_count: self.read_count.saturating_sub(earlier.read_count),
            read_length: self.read_length.saturating_sub(earlier.read_length),
            write_count: self.write_count.saturating_sub(earlier.write_count),
            write_length: self.write_length.saturating_sub(earlier.write_length),
        }
    }

    pub fn used_from_remaining(remaining: Self) -> Self {
        Self {
            runtime: Self::INIT.runtime - remaining.runtime,
            read_count: Self::INIT.read_count - remaining.read_count,
            read_length: Self::INIT.read_length - remaining.read_length,
            write_count: Self::INIT.write_count - remaining.write_count,
            write_length: Self::INIT.write_length - remaining.write_length,
        }
    }
}

impl From<CostMeter> for clarity::vm::costs::ExecutionCost {
    fn from(meter: CostMeter) -> Self {
        Self {
            write_length: meter.write_length as u64,
            write_count: meter.write_count as u64,
            read_length: meter.read_length as u64,
            read_count: meter.read_count as u64,
            runtime: meter.runtime as u64,
        }
    }
}

impl From<clarity::vm::costs::ExecutionCost> for CostMeter {
    fn from(cost: clarity::vm::costs::ExecutionCost) -> Self {
        Self {
            runtime: cost.runtime as i64,
            read_count: cost.read_count as i64,
            read_length: cost.read_length as i64,
            write_count: cost.write_count as i64,
            write_length: cost.write_length as i64,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CostGlobals {
    pub runtime: Global,
    pub read_count: Global,
    pub read_length: Global,
    pub write_count: Global,
    pub write_length: Global,
}

impl CostGlobals {
    /// One instance's meters, resolved from its exports.
    ///
    /// The globals are defined by the standard library and initialized to
    /// `i64::MAX` per instance, exactly as the linker-created imports were.
    pub fn from_instance<T>(
        instance: &wasmtime::Instance,
        store: &mut impl AsContextMut<Data = T>,
    ) -> wasmtime::Result<Self> {
        let mut global = |name: &str| {
            instance
                .get_global(store.as_context_mut(), name)
                .ok_or_else(|| wasmtime::Error::msg(format!("missing {name} export")))
        };
        Ok(Self {
            runtime: global("cost-runtime")?,
            read_count: global("cost-read-count")?,
            read_length: global("cost-read-length")?,
            write_count: global("cost-write-count")?,
            write_length: global("cost-write-length")?,
        })
    }

    pub fn remaining_costs<T>(
        &self,
        store: &mut impl AsContextMut<Data = T>,
    ) -> wasmtime::Result<CostMeter> {
        Ok(CostMeter {
            runtime: self
                .runtime
                .get(store.as_context_mut())
                .i64()
                .ok_or_else(|| wasmtime::Error::msg("missing cost-runtime"))?,
            read_count: self
                .read_count
                .get(store.as_context_mut())
                .i64()
                .ok_or_else(|| wasmtime::Error::msg("missing cost-read-count"))?,
            read_length: self
                .read_length
                .get(store.as_context_mut())
                .i64()
                .ok_or_else(|| wasmtime::Error::msg("missing cost-read-length"))?,
            write_count: self
                .write_count
                .get(store.as_context_mut())
                .i64()
                .ok_or_else(|| wasmtime::Error::msg("missing cost-write-count"))?,
            write_length: self
                .write_length
                .get(store.as_context_mut())
                .i64()
                .ok_or_else(|| wasmtime::Error::msg("missing cost-write-length"))?,
        })
    }

    pub fn set_remaining_costs<T>(
        &self,
        store: &mut impl AsContextMut<Data = T>,
        meter: &CostMeter,
    ) -> wasmtime::Result<()> {
        self.runtime
            .set(store.as_context_mut(), Val::I64(meter.runtime))?;
        self.read_count
            .set(store.as_context_mut(), Val::I64(meter.read_count))?;
        self.read_length
            .set(store.as_context_mut(), Val::I64(meter.read_length))?;
        self.write_count
            .set(store.as_context_mut(), Val::I64(meter.write_count))?;
        self.write_length
            .set(store.as_context_mut(), Val::I64(meter.write_length))?;
        Ok(())
    }
}

/// Extension trait allowing for words to generate cost tracking code
/// during traversal.
pub trait WordCharge {
    /// Generate cost tracking code for this word.
    ///
    /// See [`ChargeGenerator::charge`] for more details.
    fn charge<C: ChargeGenerator>(
        &self,
        generator: &C,
        instrs: &mut InstrSeqBuilder,
        n: impl Into<Scalar>,
    ) -> Result<()>;
}

impl<W: ?Sized + Word> WordCharge for W {
    fn charge<C: ChargeGenerator>(
        &self,
        generator: &C,
        instrs: &mut InstrSeqBuilder,
        n: impl Into<Scalar>,
    ) -> Result<()> {
        generator.charge(instrs, self.name(), n)
    }
}

/// Generators of cost tracking code.
pub trait ChargeGenerator {
    /// The cost tracking context. Only present if charging code should be emitted.
    fn cost_context(&self) -> Option<(&ChargeContext, &Module)>;

    /// Generate code that charges the appropriate cost for the given word.
    ///
    /// `n` is a scaling factor that depends on the word being charged, but can only be known
    /// during traversal. The value *must* be either a `u32` or a `LocalId` representing a local
    /// with type `I32`.
    /// If the word has a constant cost, the value will be ignored. This is useful in words where
    /// the cost is known to be constant during traversal.
    ///
    /// Code will be generated iff [`cost_context`] returns `Some`.
    fn charge(
        &self,
        instrs: &mut InstrSeqBuilder,
        word_name: ClarityName,
        n: impl Into<Scalar>,
    ) -> Result<()> {
        let n = n.into();

        if let Some((ctx, module)) = self.cost_context() {
            match ctx.word_cost(&word_name) {
                Some(cost) => ctx.emit(instrs, module, cost, n)?,
                None => {
                    return Err(GeneratorError::InternalError(format!(
                        "'{word_name}' does not exist in costs table for epoch '{}'",
                        ctx.epoch
                    )));
                }
            }
        }

        Ok(())
    }

    /// Generate code that charges one of the costs the interpreter pays around
    /// a word rather than for it. See [`EvalCosts`].
    #[doc(hidden)]
    fn charge_eval(
        &self,
        instrs: &mut InstrSeqBuilder,
        cost: impl Fn(&EvalCosts) -> Caf,
        n: impl Into<Scalar>,
    ) -> Result<()> {
        if let Some((ctx, module)) = self.cost_context() {
            ctx.emit_with_caf(
                instrs,
                module,
                cost(ctx.eval_costs()),
                ctx.runtime,
                ErrorMap::CostOverrunRuntime as _,
                n,
            )?;
        }
        Ok(())
    }

    /// Charge resolving the head of an application to a function.
    fn charge_function_lookup(&self, instrs: &mut InstrSeqBuilder) -> Result<()> {
        self.charge_eval(instrs, |costs| costs.function_lookup, 0_u32)
    }

    /// Charge entering a user-defined function with `arguments` parameters.
    fn charge_user_function_application(
        &self,
        instrs: &mut InstrSeqBuilder,
        arguments: u32,
    ) -> Result<()> {
        self.charge_eval(instrs, |costs| costs.user_function_application, arguments)
    }

    /// Charge type-checking one argument of a user-defined function.
    fn charge_inner_type_check(&self, instrs: &mut InstrSeqBuilder, size: LocalId) -> Result<()> {
        self.charge_eval(instrs, |costs| costs.inner_type_check, size)
    }

    /// Charge searching the binding scopes for a name.
    fn charge_variable_lookup(&self, instrs: &mut InstrSeqBuilder, depth: u32) -> Result<()> {
        self.charge_eval(instrs, |costs| costs.variable_depth, depth)
    }

    /// Charge copying a bound value out of its binding.
    fn charge_variable_copy(&self, instrs: &mut InstrSeqBuilder, size: LocalId) -> Result<()> {
        self.charge_eval(instrs, |costs| costs.variable_size, size)
    }
}

/// The runtime the interpreter charges for evaluation itself, rather than for
/// any one word: resolving names, entering user-defined functions, and copying
/// bound values out of their bindings.
#[derive(Debug, Clone, Copy)]
pub struct EvalCosts {
    function_lookup: Caf,
    variable_depth: Caf,
    variable_size: Caf,
    user_function_application: Caf,
    inner_type_check: Caf,
}

/// `costs-1`.
const EVAL_COSTS_1: EvalCosts = EvalCosts {
    function_lookup: Caf::Constant(1_000),
    variable_depth: Caf::Linear { a: 1_000, b: 1_000 },
    variable_size: Caf::Linear { a: 1_000, b: 0 },
    user_function_application: Caf::Linear { a: 1_000, b: 1_000 },
    inner_type_check: Caf::Linear { a: 1_000, b: 1_000 },
};

/// `costs-2`, on both networks.
const EVAL_COSTS_2: EvalCosts = EvalCosts {
    function_lookup: Caf::Constant(16),
    variable_depth: Caf::Linear { a: 2, b: 14 },
    variable_size: Caf::Linear { a: 2, b: 1 },
    user_function_application: Caf::Linear { a: 26, b: 140 },
    inner_type_check: Caf::Linear { a: 2, b: 9 },
};

/// `costs-3`, which `costs-4` and `costs-5` leave alone.
const EVAL_COSTS_3: EvalCosts = EvalCosts {
    function_lookup: Caf::Constant(16),
    variable_depth: Caf::Linear { a: 1, b: 1 },
    variable_size: Caf::Linear { a: 2, b: 1 },
    user_function_application: Caf::Linear { a: 26, b: 5 },
    inner_type_check: Caf::Linear { a: 2, b: 5 },
};

impl ChargeGenerator for WasmGenerator {
    fn cost_context(&self) -> Option<(&ChargeContext, &Module)> {
        self.cost_context.as_ref().map(|ctx| (ctx, &self.module))
    }
}

/// A 32-bit unsigned integer to be resolved at either compile-time or run-time.
#[derive(Clone, Copy)]
pub enum Scalar {
    Compile(u32),
    Run(LocalId),
}

impl From<u32> for Scalar {
    fn from(n: u32) -> Self {
        Self::Compile(n)
    }
}

impl From<LocalId> for Scalar {
    fn from(n: LocalId) -> Self {
        Self::Run(n)
    }
}

/// Trait for allowing us to not repeat ourselves in resolving a scalar.
trait ScalarGet {
    fn scalar_get(&mut self, module: &Module, scalar: Scalar) -> Result<&mut Self>;
}

impl ScalarGet for InstrSeqBuilder<'_> {
    fn scalar_get(&mut self, module: &Module, scalar: Scalar) -> Result<&mut Self> {
        Ok(match scalar {
            Scalar::Compile(c) => self.i64_const(c as _),
            Scalar::Run(l) => {
                let local = module.locals.get(l);

                match local.ty() {
                    walrus::ValType::I32 => {}
                    ty => {
                        return Err(GeneratorError::InternalError(format!(
                            "cost local should be of type i32 but is of type {ty}"
                        )));
                    }
                }

                self.local_get(l)
                    // this is so we don't have to repeat this code in the `caf` functions
                    .instr(Instr::Unop(Unop {
                        op: UnaryOp::I64ExtendUI32,
                    }))
            }
        })
    }
}

/// Context required from a generator to emit cost tracking code.
#[derive(Debug)]
pub struct ChargeContext {
    pub epoch: StacksEpochId,
    pub runtime: GlobalId,
    pub read_count: GlobalId,
    pub read_length: GlobalId,
    pub write_count: GlobalId,
    pub write_length: GlobalId,
    pub runtime_error: FunctionId,
}

impl ChargeContext {
    fn word_cost(&self, name: &ClarityName) -> Option<&WordCost> {
        match self.epoch {
            StacksEpochId::Epoch10 => panic!("clarity did not exist in epoch 1"),
            StacksEpochId::Epoch20 => clar1::WORD_COSTS.get(name),
            StacksEpochId::Epoch2_05 => clar2::WORD_COSTS.get(name),
            StacksEpochId::Epoch21
            | StacksEpochId::Epoch22
            | StacksEpochId::Epoch23
            | StacksEpochId::Epoch24
            | StacksEpochId::Epoch25
            | StacksEpochId::Epoch30
            | StacksEpochId::Epoch31
            | StacksEpochId::Epoch32 => clar3::WORD_COSTS.get(name),
            StacksEpochId::Epoch33 | StacksEpochId::Epoch34 => clar4::WORD_COSTS.get(name),
            StacksEpochId::Epoch40 => clar5::WORD_COSTS.get(name),
        }
    }

    const fn eval_costs(&self) -> &'static EvalCosts {
        match self.epoch {
            StacksEpochId::Epoch10 => panic!("clarity did not exist in epoch 1"),
            StacksEpochId::Epoch20 => &EVAL_COSTS_1,
            StacksEpochId::Epoch2_05 => &EVAL_COSTS_2,
            _ => &EVAL_COSTS_3,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WordCost {
    runtime: Caf,
    read_count: Caf,
    read_length: Caf,
    write_count: Caf,
    write_length: Caf,
}

/// Cost assessment function
#[derive(Debug, Clone, Copy)]
pub enum Caf {
    /// Constant cost
    Constant(u32),
    /// Linear cost, scaling with `n`
    ///
    /// a * n + b
    Linear { a: u64, b: u64 },
    /// a * (n >> shift) + b
    LinearShift { a: u64, b: u64, shift: u32 },
    /// Logarithmic cost, scaling with `n`
    ///
    /// a * log2(n) + b
    LogN { a: u64, b: u64 },
    /// Linear logarithmic cost, scaling with `n`
    ///
    /// a * n * log2(n) + b
    NLogN { a: u64, b: u64 },
    /// Zero cost - equivalent to `Constant(0)`
    None,
}

impl ChargeContext {
    fn emit(
        &self,
        instrs: &mut InstrSeqBuilder,
        module: &Module,
        cost: &WordCost,
        n: Scalar,
    ) -> Result<()> {
        self.emit_with_caf(
            instrs,
            module,
            cost.runtime,
            self.runtime,
            ErrorMap::CostOverrunRuntime as _,
            n,
        )?;
        self.emit_with_caf(
            instrs,
            module,
            cost.read_count,
            self.read_count,
            ErrorMap::CostOverrunReadCount as _,
            n,
        )?;
        self.emit_with_caf(
            instrs,
            module,
            cost.read_length,
            self.read_length,
            ErrorMap::CostOverrunReadLength as _,
            n,
        )?;
        self.emit_with_caf(
            instrs,
            module,
            cost.write_count,
            self.write_count,
            ErrorMap::CostOverrunWriteCount as _,
            n,
        )?;
        self.emit_with_caf(
            instrs,
            module,
            cost.write_length,
            self.write_length,
            ErrorMap::CostOverrunWriteLength as _,
            n,
        )?;

        Ok(())
    }

    fn emit_with_caf(
        &self,
        instrs: &mut InstrSeqBuilder,
        module: &Module,
        params: Caf,
        global: GlobalId,
        err_code: i32,
        n: impl Into<Scalar>,
    ) -> Result<()> {
        match params {
            Caf::Constant(cost) => {
                caf_const(instrs, module, global, self.runtime_error, err_code, cost)
            }
            Caf::Linear { a, b } => caf_linear(
                instrs,
                module,
                global,
                self.runtime_error,
                err_code,
                n,
                a,
                b,
            ),
            Caf::LinearShift { a, b, shift } => caf_linear_shift(
                instrs,
                module,
                global,
                self.runtime_error,
                err_code,
                n,
                a,
                b,
                shift,
            ),
            Caf::LogN { a, b } => caf_logn(
                instrs,
                module,
                global,
                self.runtime_error,
                err_code,
                n,
                a,
                b,
            ),
            Caf::NLogN { a, b } => caf_nlogn(
                instrs,
                module,
                global,
                self.runtime_error,
                err_code,
                n,
                a,
                b,
            ),
            Caf::None => Ok(()),
        }
    }
}

fn caf_const(
    instrs: &mut InstrSeqBuilder,
    module: &Module,
    global: GlobalId,
    error: FunctionId,
    err_code: i32,
    cost: impl Into<Scalar>,
) -> Result<()> {
    let cost = cost.into();

    // global pushed onto the stack to subtract from later
    instrs.global_get(global);

    // cost
    instrs.scalar_get(module, cost)?;

    // global -= cost
    instrs
        .binop(BinaryOp::I64Sub)
        .global_set(global)
        .global_get(global)
        .i64_const(0)
        .binop(BinaryOp::I64LtS)
        .if_else(
            None,
            |builder| {
                builder.i32_const(err_code);
                builder.call(error);
            },
            |_| {},
        );

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn caf_linear(
    instrs: &mut InstrSeqBuilder,
    module: &Module,
    global: GlobalId,
    error: FunctionId,
    err_code: i32,
    n: impl Into<Scalar>,
    a: u64,
    b: u64,
) -> Result<()> {
    let n = n.into();

    // global pushed onto the stack to subtract from later
    instrs.global_get(global);

    // cost = a * n + b
    instrs
        // n
        .scalar_get(module, n)?
        // a *
        .i64_const(a as _)
        .binop(BinaryOp::I64Mul)
        // b +
        .i64_const(b as _)
        .binop(BinaryOp::I64Add);

    // global -= cost
    instrs
        .binop(BinaryOp::I64Sub)
        .global_set(global)
        .global_get(global)
        .i64_const(0)
        .binop(BinaryOp::I64LtS)
        .if_else(
            None,
            |builder| {
                builder.i32_const(err_code);
                builder.call(error);
            },
            |_| {},
        );

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn caf_linear_shift(
    instrs: &mut InstrSeqBuilder,
    module: &Module,
    global: GlobalId,
    error: FunctionId,
    err_code: i32,
    n: impl Into<Scalar>,
    a: u64,
    b: u64,
    shift: u32,
) -> Result<()> {
    let n = n.into();

    instrs
        .global_get(global)
        .scalar_get(module, n)?
        .i64_const(i64::from(shift))
        .binop(BinaryOp::I64ShrU)
        .i64_const(a as _)
        .binop(BinaryOp::I64Mul)
        .i64_const(b as _)
        .binop(BinaryOp::I64Add)
        .binop(BinaryOp::I64Sub)
        .global_set(global)
        .global_get(global)
        .i64_const(0)
        .binop(BinaryOp::I64LtS)
        .if_else(
            None,
            |builder| {
                builder.i32_const(err_code);
                builder.call(error);
            },
            |_| {},
        );

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn caf_logn(
    instrs: &mut InstrSeqBuilder,
    module: &Module,
    global: GlobalId,
    error: FunctionId,
    err_code: i32,
    n: impl Into<Scalar>,
    a: u64,
    b: u64,
) -> Result<()> {
    let n = n.into();

    // global pushed onto the stack to subtract from later
    instrs.global_get(global);

    // cost = a * log2(n) + b
    instrs
        // log2(n)
        // 63 minus leading zeros in `n`
        // n *must* be larger than 0
        .i64_const(63)
        .scalar_get(module, n)?
        .unop(UnaryOp::I64Clz)
        .binop(BinaryOp::I64Sub)
        // a *
        .i64_const(a as _)
        .binop(BinaryOp::I64Mul)
        // b +
        .i64_const(b as _)
        .binop(BinaryOp::I64Add);

    // global -= cost
    instrs
        .binop(BinaryOp::I64Sub)
        .global_set(global)
        .global_get(global)
        .i64_const(0)
        .binop(BinaryOp::I64LtS)
        .if_else(
            None,
            |builder| {
                builder.i32_const(err_code);
                builder.call(error);
            },
            |_| {},
        );

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn caf_nlogn(
    instrs: &mut InstrSeqBuilder,
    module: &Module,
    global: GlobalId,
    error: FunctionId,
    err_code: i32,
    n: impl Into<Scalar>,
    a: u64,
    b: u64,
) -> Result<()> {
    let n = n.into();

    // global pushed onto the stack to subtract from later
    instrs.global_get(global);

    // cost = a * n * log2(n) + b
    instrs
        // log2(n)
        // 63 minus leading zeros in `n`
        // n *must* be larger than 0
        .i64_const(63)
        .scalar_get(module, n)?
        .unop(UnaryOp::I64Clz)
        .binop(BinaryOp::I64Sub)
        // n *
        .scalar_get(module, n)?
        .binop(BinaryOp::I64Mul)
        // a *
        .i64_const(a as _)
        .binop(BinaryOp::I64Mul)
        // b +
        .i64_const(b as _)
        .binop(BinaryOp::I64Add);

    // global -= cost
    instrs
        .binop(BinaryOp::I64Sub)
        .global_set(global)
        .global_get(global)
        .i64_const(0)
        .binop(BinaryOp::I64LtS)
        .if_else(
            None,
            |builder| {
                builder.i32_const(err_code);
                builder.call(error);
            },
            |_| {},
        );

    Ok(())
}

#[cfg(test)]
mod caf {
    //! The code in this module tests that the code generation in the `caf_*` functions is correct,
    //! *not* that the code generation of each word is correct.

    use super::*;
    use crate::linker::link_cost_globals;

    #[test]
    fn constant() {
        let initial_cost_val = 1000000;

        for cost in 1..100 {
            let final_cost_val =
                execute_with_caf(0, initial_cost_val, |local| (Caf::Constant(cost), local))
                    .expect("execution with enough fuel should succeed");

            assert_eq!(
                final_cost_val,
                initial_cost_val - cost as i64,
                "should decrement accurately"
            );
        }
    }

    #[test]
    fn linear() {
        let initial_val = 1000000;

        let a = 2;
        let b = 3;

        for n in 0..100 {
            let cost = a * n + b;

            let final_val = execute_with_caf(n, initial_val, |local| {
                (
                    Caf::Linear {
                        a: a as _,
                        b: b as _,
                    },
                    local,
                )
            })
            .expect("execution with enough fuel should succeed");

            assert_eq!(
                final_val,
                initial_val - cost as i64,
                "should decrement accurately"
            );
        }
    }

    #[test]
    fn linear_shift() {
        let initial = 1_000_000;
        let a = 125;
        let b = 291;

        for n in [0, 1, 1_023, 1_024, 4_095, 4_096] {
            let cost = a * (n >> 10) + b;
            let final_cost = execute_with_caf(n, initial, |local| {
                (
                    Caf::LinearShift {
                        a: a as _,
                        b: b as _,
                        shift: 10,
                    },
                    local,
                )
            })
            .expect("execution with enough fuel should succeed");

            assert_eq!(final_cost, initial - cost as i64);
        }
    }

    #[test]
    fn logn() {
        let initial_val = 1000000;

        let a = 2;
        let b = 3;

        // cost = (+ (* a (log2 n)) b))

        for n in 1..100u32 {
            let cost = a * n.ilog2() + b;

            let final_val = execute_with_caf(n as _, initial_val, |local| {
                (
                    Caf::LogN {
                        a: a as _,
                        b: b as _,
                    },
                    local,
                )
            })
            .expect("execution with enough fuel should succeed");

            assert_eq!(
                final_val,
                initial_val - cost as i64,
                "should decrement accurately"
            );
        }
    }

    #[test]
    fn nlogn() {
        let initial_val = 1000000;

        let a = 2;
        let b = 3;

        // cost = (+ (* a (* n (log2 n))) b))

        for n in 1..100u32 {
            let cost = a * n * n.ilog2() + b;

            let final_val = execute_with_caf(n as _, initial_val, |local| {
                (
                    Caf::NLogN {
                        a: a as _,
                        b: b as _,
                    },
                    local,
                )
            })
            .expect("execution with enough fuel should succeed");

            assert_eq!(
                final_val,
                initial_val - cost as i64,
                "should decrement accurately"
            );
        }
    }

    #[test]
    fn none() {
        let initial_val = 2;
        let fn_arg = 0;

        let final_val = execute_with_caf(fn_arg, initial_val, |local| (Caf::None, local))
            .expect("execution with enough fuel should succeed");

        assert_eq!(final_val, initial_val, "none caf should not cost");
    }

    const ERR_CODE: i32 = -42;

    fn execute_with_caf<S: Into<Scalar>>(
        arg: i32,
        initial: i64,
        caf: impl FnOnce(LocalId) -> (Caf, S),
    ) -> Result<i64, i64> {
        use wasmtime::{Engine, Linker, Module, Store};

        let engine = Engine::default();
        let binary = module_with_caf(caf);
        let module = Module::from_binary(&engine, &binary).unwrap();

        let mut linker = Linker::<()>::new(&engine);
        let mut store = Store::new(&engine, ());

        let cost_globals =
            link_cost_globals(&mut linker, &mut store).expect("host globals should be linked");
        cost_globals
            .set_remaining_costs(
                &mut store,
                &CostMeter {
                    runtime: initial,
                    read_count: 0,
                    read_length: 0,
                    write_count: 0,
                    write_length: 0,
                },
            )
            .unwrap();

        let instance = linker.instantiate(&mut store, &module).unwrap();

        let func = instance
            .get_typed_func::<i32, i32>(&mut store, "identity")
            .unwrap();
        let err_code = instance.get_global(&mut store, "err-code").unwrap();

        match func.call(&mut store, arg) {
            Ok(_) => Ok(cost_globals.remaining_costs(&mut store).unwrap().runtime),
            Err(_) => Err(err_code.get(&mut store).unwrap_i64()),
        }
    }

    // The functions generated here is extremely simple (a: i32) -> a, but still allows for
    // understanding the runtime characteristics of any `Caf`.
    fn module_with_caf<S: Into<Scalar>>(caf: impl FnOnce(LocalId) -> (Caf, S)) -> Vec<u8> {
        use walrus::ir::Value;
        use walrus::{FunctionBuilder, InitExpr, Module, ValType};

        let mut module = Module::default();

        // we put in all the globals, but we only use `cost-runtime`
        let (cost_global, _) =
            module.add_import_global("clarity", "cost-runtime", ValType::I64, true);
        module.add_import_global("clarity", "cost-read-count", ValType::I64, true);
        module.add_import_global("clarity", "cost-read-length", ValType::I64, true);
        module.add_import_global("clarity", "cost-write-count", ValType::I64, true);
        module.add_import_global("clarity", "cost-write-length", ValType::I64, true);

        let error_global =
            module
                .globals
                .add_local(ValType::I32, true, InitExpr::Value(Value::I32(0)));

        let arg = module.locals.add(ValType::I32);

        // runtime error that takes an I32 and traps, similar to the stdlib
        let mut error = FunctionBuilder::new(&mut module.types, &[ValType::I32], &[]);
        let mut body = error.func_body();
        body.local_get(arg);
        body.global_set(error_global);
        body.unreachable();
        let error = error.finish(vec![arg], &mut module.funcs);

        let mut identity =
            FunctionBuilder::new(&mut module.types, &[ValType::I32], &[ValType::I32]);

        let mut body = identity.func_body();

        let (caf, scalar) = caf(arg);
        match caf {
            Caf::Constant(n) => {
                caf_const(&mut body, &module, cost_global, error, ERR_CODE, n).unwrap()
            }
            Caf::Linear { a, b } => caf_linear(
                &mut body,
                &module,
                cost_global,
                error,
                ERR_CODE,
                scalar,
                a,
                b,
            )
            .unwrap(),
            Caf::LinearShift { a, b, shift } => caf_linear_shift(
                &mut body,
                &module,
                cost_global,
                error,
                ERR_CODE,
                scalar,
                a,
                b,
                shift,
            )
            .unwrap(),
            Caf::LogN { a, b } => caf_logn(
                &mut body,
                &module,
                cost_global,
                error,
                ERR_CODE,
                scalar,
                a,
                b,
            )
            .unwrap(),
            Caf::NLogN { a, b } => caf_nlogn(
                &mut body,
                &module,
                cost_global,
                error,
                ERR_CODE,
                scalar,
                a,
                b,
            )
            .unwrap(),
            Caf::None => {}
        }
        body.local_get(arg);
        let identity = identity.finish(vec![arg], &mut module.funcs);

        module.exports.add("identity", identity);
        module.exports.add("err-code", error_global);

        module.emit_wasm()
    }
}

#[cfg(test)]
mod word {
    use clarity::vm::ClarityVersion;

    use super::*;
    use crate::tools::TestEnvironment;

    #[inline(always)]
    fn execute_snippet(
        epoch: StacksEpochId,
        version: ClarityVersion,
        snippet: &str,
        expected_cost: Option<CostMeter>,
    ) {
        execute_snippets(epoch, version, &[("snippet", snippet)], expected_cost);
    }

    fn execute_snippets(
        epoch: StacksEpochId,
        version: ClarityVersion,
        snippets: &[(&str, &str)],
        expected_cost: Option<CostMeter>,
    ) {
        let mut env = if expected_cost.is_some() {
            TestEnvironment::new_with_cost(epoch, version)
        } else {
            TestEnvironment::new(epoch, version)
        };

        snippets.iter().for_each(|(contract_name, snippet)| {
            env.init_contract_with_snippet(contract_name, snippet)
                .expect("init_contract should succeed");
        });
        let cost_tracker = env.cost_tracker;

        let cost = CostMeter::from(cost_tracker.get_total());
        if let Some(expected_cost) = expected_cost {
            assert_eq!(cost, expected_cost, "'cost' should match 'expected_cost'");
        } else {
            assert_eq!(
                cost,
                CostMeter::ZERO,
                "'cost' should be at zero when not used"
            );
        }
    }

    #[test]
    fn clarity6_new_word_costs_match_costs_5() {
        use crate::cost::clar5;
        use crate::words::bitcoin::{GetBitcoinTxOutput, VerifyMerkleProof};
        use crate::words::secp256k1::{Decompress, Ed25519Verify};
        use crate::words::secp256r1::Verify as Secp256r1Verify;

        let runtime = |word: &dyn Word| clar5::WORD_COSTS.get(&word.name()).unwrap().runtime;

        assert!(matches!(runtime(&Secp256r1Verify), Caf::Constant(38)));
        assert!(matches!(
            runtime(&Ed25519Verify),
            Caf::LinearShift {
                a: 1,
                b: 39,
                shift: 9
            }
        ));
        assert!(matches!(runtime(&Decompress), Caf::Constant(39)));
        assert!(matches!(
            runtime(&VerifyMerkleProof),
            Caf::LinearShift {
                a: 1,
                b: 38,
                shift: 2
            }
        ));
        assert!(matches!(
            runtime(&GetBitcoinTxOutput),
            Caf::LinearShift {
                a: 1,
                b: 38,
                shift: 9
            }
        ));
    }

    macro_rules! epoch_for_cost_version {
        (1) => {
            StacksEpochId::Epoch20
        };
        (2) => {
            StacksEpochId::Epoch2_05
        };
        (3) => {
            StacksEpochId::Epoch31
        };
        (4) => {
            StacksEpochId::Epoch33
        };
    }

    macro_rules! decl_test {
        ($cost_version:literal, $name:literal, $snippet:literal, $expected_cost:expr) => {
            paste::paste! {
                #[test]
                fn [<$name _ v $cost_version _with_cost>]() {
                    let epoch = epoch_for_cost_version!($cost_version);
                    let version = ClarityVersion::default_for_epoch(epoch);
                    execute_snippet(epoch, version, $snippet, Some($expected_cost));
                }
                #[test]
               fn [<$name _ v $cost_version _without_cost>]() {
                    let epoch = epoch_for_cost_version!($cost_version);
                    let version = ClarityVersion::default_for_epoch(epoch);
                    execute_snippet(epoch, version, $snippet, None);
                }
            }
        };
    }

    macro_rules! decl_tests {
        ($name:literal, $snippet:literal, { $($cost_version:literal => $cost:expr),* $(,)? }) => {
            $(
                decl_test!($cost_version, $name, $snippet, $cost);
            )*
        }
    }

    macro_rules! decl_test_with_contract_call {
        ($cost_version:literal, $name:literal, ($callee_name:literal, $callee_snippet:literal), ($caller_name:literal , $caller_snippet:literal), $expected_cost:expr) => {
            paste::paste! {
                #[test]
                fn [<$name _ v $cost_version _with_cost>]() {
                    let epoch = epoch_for_cost_version!($cost_version);
                    let version = ClarityVersion::default_for_epoch(epoch);
                    execute_snippets(epoch, version, &[($callee_name, $callee_snippet), ($caller_name, $caller_snippet)], Some($expected_cost));
                }
                #[test]
                fn [<$name _ v $cost_version _without_cost>]() {
                    let epoch = epoch_for_cost_version!($cost_version);
                    let version = ClarityVersion::default_for_epoch(epoch);
                    execute_snippets(epoch, version, &[($callee_name, $callee_snippet), ($caller_name, $caller_snippet)], None);
                }
            }
        };
    }

    macro_rules! decl_tests_with_contract_call{
        ($name:literal, ($callee_name:literal, $callee_snippet:literal), ($caller_name:literal, $caller_snippet:literal), { $($cost_version:literal => $cost:expr),* $(,)? }) => {
            $(
                decl_test_with_contract_call!($cost_version, $name, ($callee_name, $callee_snippet), ($caller_name, $caller_snippet), $cost);
            )*
        }
    }

    // TODO: need serialization of values for computation (variable length)

    // TODO: need a change in the cost computation (maps)

    // TODO: test `contract-call` and `contract-of`

    decl_tests!("add", "(+ 1 2 3)", {
        1 => CostMeter { runtime: 5000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 215,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 174,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("sub", "(- 10 9 1)", {
        1 => CostMeter { runtime: 5000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 215,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 174,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("mul", "(* 2 5 10)", {
        1 => CostMeter { runtime: 5000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 215,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 180,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("div", "(/ 10 5 2)", {
        1 => CostMeter { runtime: 5000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 215,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 180,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("log2", "(log2 1000)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 186,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 149,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("mod", "(mod 2 3)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 186,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 157,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("pow", "(pow 2 3)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 186,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 159,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("sqrti", "(sqrti 11)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 186,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 158,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("bitwise_and", "(bit-and 24 16)", {
        3 => CostMeter { runtime: 175,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("bitwise_or", "(bit-or 24 16)", {
        3 => CostMeter { runtime: 175,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("bitwise_xor", "(bit-xor 1 2)", {
        3 => CostMeter { runtime: 175,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("bitwise_not", "(bit-not 3)", {
        3 => CostMeter { runtime: 163,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("bitwise_lshift", "(bit-shift-left 2 u1)", {
        3 => CostMeter { runtime: 183,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("bitwise_rshift", "(bit-shift-right 2 u1)", {
        3 => CostMeter { runtime: 183,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("buf_to_int_be", "(buff-to-int-be 0x01)", {
        3 => CostMeter { runtime: 157,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("buf_to_int_le", "(buff-to-int-le 0x01)", {
        3 => CostMeter { runtime: 157,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("buf_to_uint_be", "(buff-to-uint-be 0x01)", {
        3 => CostMeter { runtime: 157,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("buf_to_uint_le", "(buff-to-uint-le 0x01)", {
        3 => CostMeter { runtime: 157,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("gt_int", "(> 1 2)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 256,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 256,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("gte_int", "(>= 1 2)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 256,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 256,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("lt_int", "(< 1 2)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 256,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 256,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("lte_int", "(<= 1 2)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 256,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 256,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("gt_buf", "(> 0xffff 0x4242)", {
        3 => CostMeter { runtime: 186,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("gte_buf", "(>= 0xffff 0x4242)", {
        3 => CostMeter { runtime: 186,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("lt_buf", "(< 0xffff 0x4242)", {
        3 => CostMeter { runtime: 186,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("lte_buf", "(<= 0xffff 0x4242)", {
        3 => CostMeter { runtime: 186,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("or", "(or true false)", {
        1 => CostMeter { runtime: 4000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 171,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 142,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("and", "(and true false)", {
        1 => CostMeter { runtime: 4000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 171,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 142,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("not", "(not true)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 186,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 154,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("to_int", "(to-int u238)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 186,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 151,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("to_uint", "(to-uint 238)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 186,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 151,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("int_to_ascii", "(int-to-ascii 1)", {
        3 => CostMeter { runtime: 163,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("int_to_utf8", "(int-to-utf8 1)", {
        3 => CostMeter { runtime: 197,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("string_to_int", "(string-to-int? \"1\")", {
        3 => CostMeter { runtime: 184,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("string_to_uint", "(string-to-uint? \"1\")", {
        3 => CostMeter { runtime: 184,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("hash160_int", "(hash160 0)", {
        1 => CostMeter { runtime: 3000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 234,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 221,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("keccak256_int", "(keccak256 0)", {
        1 => CostMeter { runtime: 3000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 254,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 160,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("sha256_int", "(sha256 0)", {
        1 => CostMeter { runtime: 3000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 133,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 133,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("sha512_int", "(sha512 0)", {
        1 => CostMeter { runtime: 3000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 209,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 209,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("sha512_256_int", "(sha512/256 0)", {
        1 => CostMeter { runtime: 3000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 221,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 89,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("hash160_buf", "(hash160 0xffff)", {
        1 => CostMeter { runtime: 3000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 224,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 211,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("keccak256_buf", "(keccak256 0xffff)", {
        1 => CostMeter { runtime: 3000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 244,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 150,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("sha256_buf", "(sha256 0xffff)", {
        1 => CostMeter { runtime: 3000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 123,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 123,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("sha512_buf", "(sha512 0xffff)", {
        1 => CostMeter { runtime: 3000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 199,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 199,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("sha512_256_buf", "(sha512/256 0xffff)", {
        1 => CostMeter { runtime: 3000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 211,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 79,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("stx_burn", "(stx-burn? u100 'S1G2081040G2081040G2081040G208105NK8PE5)", {
        1 => CostMeter { runtime: 2000, read_count: 2, read_length: 1, write_count: 2, write_length: 1 },
        2 => CostMeter { runtime: 628,  read_count: 2, read_length: 1, write_count: 2, write_length: 1 },
        3 => CostMeter { runtime: 565,  read_count: 2, read_length: 1, write_count: 2, write_length: 1 },
    });
    decl_tests!("stx_get_balance", "(stx-get-balance 'S1G2081040G2081040G2081040G208105NK8PE5)", {
        1 => CostMeter { runtime: 2000, read_count: 1, read_length: 1, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 1401,  read_count: 1, read_length: 1, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 4310,  read_count: 1, read_length: 1, write_count: 0, write_length: 0 },
    });
    decl_tests!("stx_get_account", "(stx-account 'SZ2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQ9H6DPR)", {
        3 => CostMeter { runtime: 4670,  read_count: 1, read_length: 1, write_count: 0, write_length: 0 },
    });
    decl_tests!("principal_construct", "(principal-construct? 0x1a 0xfa6bf38ed557fe417333710d6033e9419391a320)", {
        3 => CostMeter { runtime: 414,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("principal_destruct", "(principal-destruct? 'STB44HYPYAT2BB2QE513NSP81HTMYWBJP02HPGK6)", {
        3 => CostMeter { runtime: 330,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("let", "(let ((a 42) (b 24)) a)", {
        1 => CostMeter { runtime: 22000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 1219,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 463,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("at_block", "(at-block 0x0000000000000000000000000000000000000000000000000000000000000000 1)", {
        1 => CostMeter { runtime: 2000, read_count: 1, read_length: 1, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 226,  read_count: 1, read_length: 1, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 1343,  read_count: 1, read_length: 1, write_count: 0, write_length: 0 },
    });
    decl_tests!("get_block_info", "(get-block-info? time u0)", {
        1 => CostMeter { runtime: 2000, read_count: 1, read_length: 1, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 6337,  read_count: 1, read_length: 1, write_count: 0, write_length: 0 },
    });
    decl_tests!("get_burn_block_info", "(get-burn-block-info? header-hash u677050)", {
        3 => CostMeter { runtime: 96495,  read_count: 1, read_length: 1, write_count: 0, write_length: 0 },
    });
    decl_tests!("get_stacks_block_info", "(get-stacks-block-info? time u0)", {
        3 => CostMeter { runtime: 6337,  read_count: 1, read_length: 1, write_count: 0, write_length: 0 },
    });
    decl_tests!("get_tenure_info", "(get-tenure-info? time u0)", {
        3 => CostMeter { runtime: 16,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("asserts", "(asserts! true 1)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 186,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 144,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("filter", "(filter not (list true false true false))", {
        1 => CostMeter { runtime: 13000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 1442,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 1227,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("if", "(if true 1 2)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 216,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 184,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("match", "(match (some 1) value 1 2)", {
        1 => CostMeter { runtime: 4000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 549,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 495,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("try", "(try! (some 1))", {
        1 => CostMeter { runtime: 4000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 549,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 471,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("unwrap", "(unwrap! (ok 1) 1)", {
        1 => CostMeter { runtime: 4000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 549,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 483,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("unwrap_err", "(unwrap-err! (err 1) false)", {
        1 => CostMeter { runtime: 4000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 549,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 479,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("from_consensus_buff", "(from-consensus-buff? int 0x0000000000000000000000000000000001)", {
        3 => CostMeter { runtime: 405,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("to_consensus_buff", "(to-consensus-buff? 1)", {
        3 => CostMeter { runtime: 266,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("as_contract", "(as-contract 1)", {
        3 => CostMeter { runtime: 154,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("begin", "(begin 1)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 218,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 167,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("unwrap_err_panic", "(unwrap-err-panic (err 1))", {
        1 => CostMeter { runtime: 4000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 601,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 533,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("unwrap_panic", "(unwrap-panic (some 1))", {
        1 => CostMeter { runtime: 4000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 601,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 505,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("get_data_var", "(define-data-var i int 0) \
                                 (var-get i)", {
        1 => CostMeter { runtime: 18000, read_count: 1, read_length: 17, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 576,  read_count: 1, read_length: 18, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 184,  read_count: 1, read_length: 18, write_count: 0, write_length: 0 },
    });
    decl_tests!("set_data_var", "(define-data-var i int 0) \
                                 (var-set i 1)", {
        1 => CostMeter { runtime: 18000, read_count: 0, read_length: 0, write_count: 1, write_length: 17 },
        2 => CostMeter { runtime: 792,  read_count: 0, read_length: 0, write_count: 1, write_length: 18 },
        3 => CostMeter { runtime: 756,  read_count: 0, read_length: 0, write_count: 1, write_length: 18 },
    });
    decl_tests!("default_to", "(default-to 0 none)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 303,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 284,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("err", "(err true)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 246,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 215,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("ok", "(ok true)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 246,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 215,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("some", "(some true)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 246,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 215,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("index_of_list", "(index-of (list 1 2 3) 2)", {
        1 => CostMeter { runtime: 54000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 1218,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 1152,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("index_of_string_utf8", r#"(index-of u"hello" u"l")"#, {
        1 => CostMeter { runtime: 4000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 275,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 243,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("index_of_buff", r#"(index-of 0x1234567890 0x34)"#, {
        1 => CostMeter { runtime: 4000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 275,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 243,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("is_eq_signed_int", "(is-eq 1 1)", {
        1 => CostMeter { runtime: 4000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 426,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 201,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("is_eq_unsigned_int", "(is-eq u1 u1)", {
        1 => CostMeter { runtime: 4000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 426,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 201,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("is_eq_bool", "(is-eq true true)", {
        1 => CostMeter { runtime: 4000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 202,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 169,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("is_eq_principal", "(is-eq 'ST1HTBVD3JG9C05J7HBJTHGR0GGW7KXW28M5JS8QE 'ST1HTBVD3JG9C05J7HBJTHGR0GGW7KXW28M5JS8QE)", {
        1 => CostMeter { runtime: 4000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 496,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 211,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("is_eq_buffer", "(is-eq 0x68656c6c6f21 0x68656c6c6f21)", {
        1 => CostMeter { runtime: 4000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 342,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 189,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("is_eq_ascii_string", "(is-eq \"This is an ASCII string\" \"This is an ASCII string\")", {
        1 => CostMeter { runtime: 4000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 580,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 223,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("is_eq_utf8_string", "(is-eq u\"And this is an UTF-8 string \\u{1f601}\" u\"And this is an UTF-8 string \\u{1f601}\")", {
        1 => CostMeter { runtime: 4000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 706,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 241,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("is_eq_list", "(is-eq (list 1 2 3) (list 1 2 3))", {
        1 => CostMeter { runtime: 104000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 2744,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 1983,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("is_eq_optional", "(is-eq (some u5) (some u5))", {
        1 => CostMeter { runtime: 8000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 932,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 633,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("is_eq_tuple", "(is-eq {field1: 1, field2: (list 1 2 3)} {field1: 1, field2: (list 1 2 3)})", {
        1 => CostMeter { runtime: 112000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 5526,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 5879,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("is_eq_error", "(is-eq (err u5) (err u5))", {
        1 => CostMeter { runtime: 8000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 932,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 633,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("map_definition", "(define-map squares { x: int } { y: int })", {
        1 => CostMeter { runtime: 0, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 0,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 0,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("map_delete_existing", "(define-map squares { x: int } { y: int }) \
                                        (map-set squares {x: 1} {y : 0})
                                        (map-delete squares { x: 1 })", {
        1 => CostMeter { runtime: 79000, read_count: 2, read_length: 0, write_count: 2, write_length: 71 },
        2 => CostMeter { runtime: 8087,  read_count: 2, read_length: 0, write_count: 2, write_length: 76 },
        3 => CostMeter { runtime: 9802,  read_count: 2, read_length: 0, write_count: 2, write_length: 76 },
    });
    decl_tests!("map_delete_non_existing", "(define-map squares { x: int } { y: int }) \
                                            (map-set squares {x: 1} {y : 0})
                                            (map-delete squares { x: 0 })", {
        1 => CostMeter { runtime: 79000, read_count: 2, read_length: 0, write_count: 2, write_length: 71 },
        2 => CostMeter { runtime: 8083,  read_count: 2, read_length: 0, write_count: 2, write_length: 75 },
        3 => CostMeter { runtime: 9798,  read_count: 2, read_length: 0, write_count: 2, write_length: 75 },
    });
    decl_tests!("map_get_non_existing", "(define-map squares { x: int } { y: int }) \
                                         (map-set squares {x: 1} {y : 0})
                                         (map-get? squares { x: 0 })", {
        1 => CostMeter { runtime: 102000, read_count: 2, read_length: 47, write_count: 1, write_length: 47},
        2 => CostMeter { runtime: 7346,  read_count: 2, read_length: 25, write_count: 1, write_length: 50 },
        3 => CostMeter { runtime: 8852,  read_count: 2, read_length: 25, write_count: 1, write_length: 50 },
    });
    decl_tests!("map_get_existing", "(define-map squares { x: int } { y: int }) \
                                     (map-set squares {x: 1} {y : 0})
                                     (map-get? squares { x : 1 } )", {
        1 => CostMeter { runtime: 102000, read_count: 2, read_length: 47, write_count: 1, write_length: 47},
        2 => CostMeter { runtime: 7371,  read_count: 2, read_length: 50, write_count: 1, write_length: 50 },
        3 => CostMeter { runtime: 8877,  read_count: 2, read_length: 50, write_count: 1, write_length: 50 },
    });
    decl_tests!("map_insert_existing", "(define-map squares { x: int } { y: int }) \
                                        (map-set squares {x: 1} {y : 0})
                                        (map-insert squares { x: 1 } { y: 1 })", {
        1 => CostMeter { runtime: 104000, read_count: 2, read_length: 0, write_count: 2, write_length: 94 },
        2 => CostMeter { runtime: 9200,  read_count: 2, read_length: 0, write_count: 2, write_length: 75 },
        3 => CostMeter { runtime: 11690,  read_count: 2, read_length: 0, write_count: 2, write_length: 75 },
    });
    decl_tests!("map_insert_non_existing", "(define-map squares { x: int } { y: int }) \
                                            (map-set squares {x: 1} {y : 0})
                                            (map-insert squares { x: 0 } { y: 1 })", {
        1 => CostMeter { runtime: 104000, read_count: 2, read_length: 0, write_count: 2, write_length: 94 },
        2 => CostMeter { runtime: 9296,  read_count: 2, read_length: 0, write_count: 2, write_length: 99 },
        3 => CostMeter { runtime: 11786,  read_count: 2, read_length: 0, write_count: 2, write_length: 99 },
    });
    decl_tests!("map_set_existing", "(define-map squares { x: int } { y: int }) \
                                     (map-set squares {x: 1} {y : 0})
                                     (map-set squares { x: 1 } { y: 1 })", {
        1 => CostMeter { runtime: 104000, read_count: 2, read_length: 0, write_count: 2, write_length: 94 },
        2 => CostMeter { runtime: 9300,  read_count: 2, read_length: 0, write_count: 2, write_length: 100 },
        3 => CostMeter { runtime: 11790,  read_count: 2, read_length: 0, write_count: 2, write_length: 100 },
    });
    decl_tests!("map_set_non_existing", "(define-map squares { x: int } { y: int }) \
                                         (map-set squares {x: 1} {y : 0})
                                         (map-set squares { x: 0 } { y: 1 })", {
        1 => CostMeter { runtime: 104000, read_count: 2, read_length: 0, write_count: 2, write_length: 94 },
        2 => CostMeter { runtime: 9300,  read_count: 2, read_length: 0, write_count: 2, write_length: 100 },
        3 => CostMeter { runtime: 11790,  read_count: 2, read_length: 0, write_count: 2, write_length: 100 },
    });
    decl_tests!("is_none", "(is-none none)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 303,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 230,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("is_some", "(is-some (some 1))", {
        1 => CostMeter { runtime: 4000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 549,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 426,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("is_standard", "(is-standard 'STB44HYPYAT2BB2QE513NSP81HTMYWBJP02HPGK6)", {
        3 => CostMeter { runtime: 143,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("principal_of", "(principal-of? 0x03adb8de4bfb65db2cfd6120d55c6526ae9c52e675db7e47308636534ba7786110)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 1015,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 1000,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("print", "(print 0x1234567890)", {
        1 => CostMeter { runtime: 11000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 1456,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 1609,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("is_err", "(is-err (err 1))", {
        1 => CostMeter { runtime: 4000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 549,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 476,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("is_ok", "(is-ok (ok 1))", {
        1 => CostMeter { runtime: 4000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 549,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 489,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("recover", "(secp256k1-recover? 0xde5b9eb9e7c5592930eb2e30a01369c36586d872082ed8181ee83d2a0ec20f04 0x8738487ebe69b93d8e51583be8eee50bb4213fc49c767d329632730cc193b873554428fc936ca3569afc15f1c9365f6591d6251a89fee9c9ac661116824d3a1301)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 14360,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 8671,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("verify", "(secp256k1-verify 0xde5b9eb9e7c5592930eb2e30a01369c36586d872082ed8181ee83d2a0ec20f04 0x8738487ebe69b93d8e51583be8eee50bb4213fc49c767d329632730cc193b873554428fc936ca3569afc15f1c9365f6591d6251a89fee9c9ac661116824d3a1301 0x03adb8de4bfb65db2cfd6120d55c6526ae9c52e675db7e47308636534ba7786110)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 13556,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 8365,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });

    decl_tests!("append", "(append (list 1 2 3 4) 5)", {
        1 => CostMeter { runtime: 84000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 2438,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 2545,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("as_max_len", "(as-max-len? 0x1234567890 u2)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 491,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 491,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("concat", "(concat 0x0102 0x0304)", {
        1 => CostMeter { runtime: 6000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 560,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 384,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("element_at", "(element-at 0x1234567890 u2)", {
        1 => CostMeter { runtime: 2000,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 635,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 514,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("element_at_alias", "(element-at? 0x1234567890 u2)", {
        3 => CostMeter { runtime: 514,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    // `fold` resolves its function argument before the loop, as every other
    // application does; these carry that lookup.
    decl_tests!("fold", "(fold * (list 2 2 2) 1)", {
        1 => CostMeter { runtime: 62000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 1956,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 1797,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("len", "(len 0x010203)", {
        1 => CostMeter { runtime: 2000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 502,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 445,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("list_cons", "(list 1 2 3)", {
        1 => CostMeter { runtime: 50000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 886,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 852,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("map", "(define-private (zero-or-one (char (buff 1))) \
                          (if (is-eq char 0x00) 0x00 0x01)) \
                        (map zero-or-one 0x000102)", {
        1 => CostMeter { runtime: 65000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 7860,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 6758,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("replace_at", "(replace-at? 0x00112233 u2 0x44)", {
        3 => CostMeter { runtime: 581,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("slice", "(slice? 0x1234567890 u1 u3)", {
        3 => CostMeter { runtime: 514,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("stx_transfer", "(stx-transfer? u100 'S1G2081040G2081040G2081040G208105NK8PE5 'ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM)", {
        1 => CostMeter { runtime: 2000, read_count: 1, read_length: 1, write_count: 1, write_length: 1 },
        2 => CostMeter { runtime: 1446,  read_count: 1, read_length: 1, write_count: 1, write_length: 1 },
        3 => CostMeter { runtime: 4656,  read_count: 1, read_length: 1, write_count: 1, write_length: 1 },
    });
    decl_tests!("stx_transfer_memo", "(stx-transfer-memo? u100 'S1G2081040G2081040G2081040G208105NK8PE5 'ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM 0x12345678)", {
        3 => CostMeter { runtime: 4725,  read_count: 1, read_length: 1, write_count: 1, write_length: 1 },
    });
    decl_tests!("ft_burn", "(define-fungible-token st) \
                            (ft-mint? st u100 'SPAXYA5XS51713FDTQ8H94EJ4V579CXMTRNBZKSF)", {
        1 => CostMeter { runtime: 2000, read_count: 2, read_length: 1, write_count: 2, write_length: 1 },
        2 => CostMeter { runtime: 1661, read_count: 2, read_length: 1, write_count: 2, write_length: 1 },
        3 => CostMeter { runtime: 1495, read_count: 2, read_length: 1, write_count: 2, write_length: 1 },
    });
    decl_tests!("nft_burn", "(define-non-fungible-token st int) \
                             (nft-mint? st 1 'SPAXYA5XS51713FDTQ8H94EJ4V579CXMTRNBZKSF)", {
        1 => CostMeter { runtime: 18000, read_count: 1, read_length: 1, write_count: 1, write_length: 1 },
        2 => CostMeter { runtime: 964, read_count: 1, read_length: 1, write_count: 1, write_length: 1 },
        3 => CostMeter { runtime: 744, read_count: 1, read_length: 1, write_count: 1, write_length: 1 },
    });
    decl_tests!("ft_get_balance", "(define-fungible-token st) \
                                   (ft-get-balance st 'SZ2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQ9H6DPR)", {
        1 => CostMeter { runtime: 2000, read_count: 1, read_length: 1, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 563, read_count: 1, read_length: 1, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 495, read_count: 1, read_length: 1, write_count: 0, write_length: 0 },
    });
    decl_tests!("nft_get_owner", "(define-non-fungible-token st int) \
                                  (nft-mint? st 1 'SPAXYA5XS51713FDTQ8H94EJ4V579CXMTRNBZKSF) \
                                  (nft-get-owner? st 1)", {
        1 => CostMeter { runtime: 36000, read_count: 2, read_length: 2, write_count: 1, write_length: 1 },
        2 => CostMeter { runtime: 1928,  read_count: 2, read_length: 2, write_count: 1, write_length: 1 },
        3 => CostMeter { runtime: 1708,  read_count: 2, read_length: 2, write_count: 1, write_length: 1 },
    });
    decl_tests!("ft_get_supply", "(define-fungible-token st) \
                                  (ft-get-supply st)", {
        1 => CostMeter { runtime: 2000, read_count: 1, read_length: 1, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 499, read_count: 1, read_length: 1, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 436, read_count: 1, read_length: 1, write_count: 0, write_length: 0 },
    });
    decl_tests!("ft_mint", "(define-fungible-token st) \
                            (ft-mint? st u100 'SPAXYA5XS51713FDTQ8H94EJ4V579CXMTRNBZKSF)", {
        1 => CostMeter { runtime: 2000, read_count: 2, read_length: 1, write_count: 2, write_length: 1 },
        2 => CostMeter { runtime: 1661, read_count: 2, read_length: 1, write_count: 2, write_length: 1 },
        3 => CostMeter { runtime: 1495, read_count: 2, read_length: 1, write_count: 2, write_length: 1 },
    });
    decl_tests!("nft_mint", "(define-non-fungible-token st int) \
                             (nft-mint? st 1 'SPAXYA5XS51713FDTQ8H94EJ4V579CXMTRNBZKSF)", {
        1 => CostMeter { runtime: 18000, read_count: 1, read_length: 1, write_count: 1, write_length: 1 },
        2 => CostMeter { runtime: 964, read_count: 1, read_length: 1, write_count: 1, write_length: 1 },
        3 => CostMeter { runtime: 744, read_count: 1, read_length: 1, write_count: 1, write_length: 1 },
    });
    decl_tests!("ft_transfer", "(define-fungible-token st) \
                                (ft-mint? st u100 'SZ2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQ9H6DPR) \
                                (ft-transfer? st u50 'SZ2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQ9H6DPR 'SPAXYA5XS51713FDTQ8H94EJ4V579CXMTRNBZKSF) \
                                (ft-transfer? st u60 'SZ2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQ9H6DPR 'SPAXYA5XS51713FDTQ8H94EJ4V579CXMTRNBZKSF)", {
        1 => CostMeter { runtime: 6000, read_count: 6, read_length: 3, write_count: 6, write_length: 3 },
        2 => CostMeter { runtime: 2917, read_count: 6, read_length: 3, write_count: 6, write_length: 3 },
        3 => CostMeter { runtime: 2625, read_count: 6, read_length: 3, write_count: 6, write_length: 3 },
    });
    decl_tests!("nft_transfer", "(define-non-fungible-token st int) \
                                 (nft-mint? st 1 'SZ2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQ9H6DPR) \
                                 (nft-transfer? st 1 'SZ2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQ9H6DPR 'SPAXYA5XS51713FDTQ8H94EJ4V579CXMTRNBZKSF)", {
        1 => CostMeter { runtime: 36000, read_count: 2, read_length: 2, write_count: 2, write_length: 2 },
        2 => CostMeter { runtime: 1928, read_count: 2, read_length: 2, write_count: 2, write_length: 2 },
        3 => CostMeter { runtime: 1485, read_count: 2, read_length: 2, write_count: 2, write_length: 2 },
    });

    #[test]
    fn nft_operation_costs_match_the_interpreter_at_every_cost_version() {
        let snippets = [
            "(define-non-fungible-token st int)
             (define-public (f) (nft-mint? st 1 tx-sender))",
            "(define-non-fungible-token st int)
             (nft-mint? st 1 tx-sender)
             (define-public (f) (ok (nft-get-owner? st 1)))",
            "(define-non-fungible-token st int)
             (nft-mint? st 1 tx-sender)
             (define-public (f)
               (nft-transfer? st 1 tx-sender
                 'SPAXYA5XS51713FDTQ8H94EJ4V579CXMTRNBZKSF))",
            "(define-non-fungible-token st int)
             (nft-mint? st 1 tx-sender)
             (define-public (f) (nft-burn? st 1 tx-sender))",
        ];
        for epoch in [
            StacksEpochId::Epoch20,
            StacksEpochId::Epoch2_05,
            StacksEpochId::Epoch31,
        ] {
            let version = ClarityVersion::default_for_epoch(epoch);
            for snippet in snippets {
                let mut compiled = TestEnvironment::new_with_cost(epoch, version);
                let mut interpreted = compiled.clone();
                assert_eq!(
                    compiled.init_contract_with_snippet("nft", snippet),
                    interpreted.interpret_contract_with_snippet("nft", snippet),
                    "deployment result diverged at {epoch:?} for {snippet}"
                );
                let compiled_before = CostMeter::from(compiled.cost_tracker.get_total());
                let interpreted_before = CostMeter::from(interpreted.cost_tracker.get_total());
                assert_eq!(
                    compiled.call_contract("nft", "f", &[]),
                    interpreted.interpret_call_contract("nft", "f", &[]),
                    "operation result diverged at {epoch:?} for {snippet}"
                );
                assert_eq!(
                    CostMeter::from(compiled.cost_tracker.get_total())
                        .saturating_sub(compiled_before),
                    CostMeter::from(interpreted.cost_tracker.get_total())
                        .saturating_sub(interpreted_before),
                    "operation cost diverged at {epoch:?} for {snippet}"
                );
            }
        }
    }
    decl_tests!("tuple_cons", "(tuple (b 0x0102) (id 1337))", {
        1 => CostMeter { runtime: 4000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 1139,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 1912,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("tuple_get", "(get id (tuple (b 0x0102) (id 1337)))", {
        1 => CostMeter { runtime: 7000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 2943,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 3672,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });
    decl_tests!("tuple_merge", "(merge {a: 1} {b: 2})", {
        1 => CostMeter { runtime: 8000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        2 => CostMeter { runtime: 3088,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
        3 => CostMeter { runtime: 4400,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    });

    decl_tests_with_contract_call!(
        "contract_call",
        (
            "callee",
            "(define-public (foo (a (response bool int)) (b int) (c int) (d int)) (ok 1))"
        ),
        ("caller", "(contract-call? .callee foo (ok true) 2 3 4)"),
        {
                 1 => CostMeter { runtime: 142000, read_count: 3, read_length: 77, write_count: 0, write_length: 0 },
                 2 => CostMeter { runtime: 1274, read_count: 3, read_length: 77, write_count: 0, write_length: 0 },
                 3 => CostMeter { runtime: 965, read_count: 3, read_length: 77, write_count: 0, write_length: 0 },
             }
    );

    // decl_tests!("contract_of", "(contract-of contract)", {
    //     1 => CostMeter { runtime: 4000, read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    //     2 => CostMeter { runtime: 199,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    //     3 => CostMeter { runtime: 164,  read_count: 0, read_length: 0, write_count: 0, write_length: 0 },
    // });
}

/// Charging regressions reduced from the captured chain, each asserted against
/// the interpreter rather than a hand-written expectation.
#[cfg(test)]
mod crosscheck {
    use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier, TupleData};
    use clarity::vm::{ClarityName, Value};

    use crate::tools::{crosscheck_cost, crosscheck_cost_multi_contract};

    #[test]
    fn charges_every_application() {
        for snippet in [
            "(define-public (f) (ok u1))",
            "(define-public (f) (begin (ok u1)))",
            "(define-public (f) (ok (+ u1 u2 u3)))",
            "(define-public (f) (ok tx-sender))",
            "(define-public (f) (ok (if true u1 u2)))",
        ] {
            crosscheck_cost(snippet, "f", &[]);
        }
    }

    #[test]
    fn charges_panic_unwraps_only_after_their_inputs_return() {
        for snippet in [
            "(define-public (f)
               (ok (unwrap-panic
                 (try! (if true (err u1) (ok (some u2)))))))",
            "(define-public (f)
               (ok (unwrap-err-panic
                 (try! (if true (err u1) (ok (err u2)))))))",
        ] {
            crosscheck_cost(snippet, "f", &[]);
        }
    }

    /// From epoch 3.3 a function's arguments are type-checked at the size of
    /// the values passed, not of the types declared, so a short buffer in a
    /// wide parameter costs what it is rather than what it could have been
    /// (`callables.rs`, `uses_arg_size_for_cost`).
    #[test]
    fn charges_an_argument_for_what_it_holds() {
        for (snippet, arguments) in [
            (
                "(define-public (f (a (buff 100))) (ok a))",
                vec![Value::buff_from(vec![1, 2, 3, 4]).expect("buffer")],
            ),
            (
                "(define-public (f (a (buff 100))) (ok a))",
                vec![Value::buff_from(vec![7; 90]).expect("buffer")],
            ),
            (
                "(define-public (f (a (string-ascii 128))) (ok a))",
                vec![Value::string_ascii_from_bytes(b"short".to_vec()).expect("ascii")],
            ),
            (
                "(define-private (g (a (buff 64))) a) \
                 (define-public (f (a (buff 64))) (ok (g a)))",
                vec![Value::buff_from(vec![9; 3]).expect("buffer")],
            ),
            (
                "(define-public (f (a (list 20 uint))) (ok a))",
                vec![
                    Value::cons_list_unsanitized(vec![Value::UInt(1), Value::UInt(2)])
                        .expect("list"),
                ],
            ),
            (
                "(define-public (f (a (string-utf8 40))) (ok a))",
                vec![Value::string_utf8_from_bytes("hi".into()).expect("utf8")],
            ),
            (
                "(define-public (f (a (optional (buff 500)))) (ok a))",
                vec![Value::none()],
            ),
            (
                "(define-public (f (a (optional (buff 500)))) (ok a))",
                vec![Value::some(Value::buff_from(vec![3, 4]).expect("buffer")).expect("optional")],
            ),
            (
                "(define-public (f (a (response uint (buff 400)))) (ok a))",
                vec![Value::okay(Value::UInt(7)).expect("response")],
            ),
        ] {
            crosscheck_cost(snippet, "f", &arguments);
        }
    }

    /// A trait argument declares 276 and a contract principal is 148, so a
    /// function taking one is where a declared size and a value's size differ
    /// most. `.pox-5 stake-update` takes two.
    #[test]
    fn charges_a_trait_argument_for_what_it_holds() {
        crosscheck_cost_multi_contract(
            &[
                ("callee", "(define-public (go) (ok u1))"),
                (
                    "snippet",
                    "(define-trait go-trait ((go () (response uint uint)))) \
                     (define-public (f (a <go-trait>)) (ok (contract-of a)))",
                ),
            ],
            "f",
            &[Value::Principal(
                clarity::vm::types::PrincipalData::parse_qualified_contract_principal(
                    "S1G2081040G2081040G2081040G208105NK8PE5.callee",
                )
                .expect("contract principal"),
            )],
        );
    }

    /// A private call charges its values before declared trait casts, and a
    /// dynamic call through the forwarded trait reads the value as a contract
    /// principal. This is the shape incentives-v2-2 uses when it forwards one
    /// token and supplies two token constants to `claim-rewards-priv`.
    #[test]
    fn charges_private_and_dynamic_trait_values_at_their_call_boundaries() {
        crosscheck_cost_multi_contract(
            &[
                (
                    "token",
                    "(define-public (get-balance (who principal)) (ok u0))",
                ),
                (
                    "snippet",
                    "(define-trait ft ((get-balance (principal) (response uint uint))))
                     (define-constant token-b .token)
                     (define-constant token-c .token)
                     (define-private (g (a <ft>) (b <ft>) (c <ft>) (who principal))
                       (contract-call? a get-balance who))
                     (define-public (f (a <ft>) (who principal))
                       (g a token-b token-c who))",
                ),
            ],
            "f",
            &[
                Value::Principal(
                    PrincipalData::parse_qualified_contract_principal(
                        "S1G2081040G2081040G2081040G208105NK8PE5.token",
                    )
                    .expect("contract principal"),
                ),
                Value::Principal(PrincipalData::Standard(
                    clarity::vm::types::StandardPrincipalData::transient(),
                )),
            ],
        );
    }

    /// Constructs `.pox-5 stake-update` is built from, one at a time.
    ///
    /// Its cost still diverges by a fraction of a percent and the divergence
    /// is not in how its arguments are handled, so this walks its body.
    #[test]
    fn charges_what_stake_update_is_made_of() {
        for snippet in [
            // `let` over reads of a map entry, which is most of its prelude.
            "(define-map m principal { a: uint, b: uint }) \
             (define-public (f) (let ((e (unwrap! (map-get? m tx-sender) (err u1))) \
                                      (x (+ (get a e) (get b e)))) (ok x)))",
            // The unlocked-balance check.
            "(define-public (f) (ok (get unlocked (stx-account tx-sender))))",
            // `try!` through a private function returning a response. The
            // condition is a literal, because a computed one has its own open
            // divergence above.
            "(define-private (g (a uint)) (if true (ok a) (err u1))) \
             (define-public (f) (begin (try! (g u1)) (ok u2)))",
            // `asserts!` on an equality, which it does throughout.
            "(define-public (f) (begin (asserts! (is-eq u1 u1) (err u1)) (ok u2)))",
            // A tuple written back into a map.
            "(define-map m principal { a: uint, b: uint }) \
             (define-public (f) (begin (map-set m tx-sender { a: u1, b: u2 }) (ok true)))",
        ] {
            crosscheck_cost(snippet, "f", &[]);
        }
    }

    /// Words that read a bound operand where it is, rather than copying it.
    ///
    /// Each of these charged a copy nano's interpreter never makes — small per
    /// occurrence, but paid once per bound name, which is how a contract that
    /// reads its arguments as often as pox-5 accumulates a divergence.
    #[test]
    fn charges_words_that_read_a_bound_operand_in_place() {
        for (snippet, args) in [
            (
                "(define-public (f (a bool)) (ok (and a true)))",
                vec![Value::Bool(true)],
            ),
            (
                "(define-public (f (a bool)) (ok (or a false)))",
                vec![Value::Bool(true)],
            ),
            (
                "(define-map m uint uint) \
                 (define-public (f (a uint)) (ok (map-get? m a)))",
                vec![Value::UInt(3)],
            ),
            (
                "(define-map m principal bool) \
                 (define-public (f (a principal)) (ok (map-delete m a)))",
                vec![Value::Principal(
                    clarity::vm::types::PrincipalData::parse(
                        "S1G2081040G2081040G2081040G208105NK8PE5",
                    )
                    .expect("principal"),
                )],
            ),
            (
                "(define-public (f (a bool)) (begin (asserts! a (err u1)) (ok u2)))",
                vec![Value::Bool(true)],
            ),
            (
                "(define-public (f (a (buff 4))) (ok (list a 0x01)))",
                vec![Value::buff_from(vec![1, 2]).expect("buffer")],
            ),
            (
                "(define-public (f (a (list 4 uint))) (ok (append a u1)))",
                vec![Value::cons_list_unsanitized(vec![Value::UInt(1)]).expect("list")],
            ),
            (
                "(define-public (f (a principal)) (ok (get unlocked (stx-account a))))",
                vec![Value::Principal(
                    clarity::vm::types::PrincipalData::parse(
                        "S1G2081040G2081040G2081040G208105NK8PE5",
                    )
                    .expect("principal"),
                )],
            ),
        ] {
            crosscheck_cost(snippet, "f", &args);
        }
    }

    #[test]
    fn charges_fungible_token_operands_in_place() {
        let principal = || {
            Value::Principal(
                clarity::vm::types::PrincipalData::parse("S1G2081040G2081040G2081040G208105NK8PE5")
                    .expect("principal"),
            )
        };
        for (snippet, arguments) in [
            (
                "(define-fungible-token token)
                 (define-public (f (owner principal))
                   (ok (ft-get-balance token owner)))",
                vec![principal()],
            ),
            (
                "(define-fungible-token token)
                 (define-public (f (amount uint) (recipient principal))
                   (ft-mint? token amount recipient))",
                vec![Value::UInt(1), principal()],
            ),
            (
                "(define-fungible-token token)
                 (define-public (f (amount uint) (sender principal) (recipient principal))
                   (begin
                     (try! (ft-mint? token amount sender))
                     (ft-transfer? token amount sender recipient)))",
                vec![Value::UInt(1), principal(), principal()],
            ),
            (
                "(define-fungible-token token)
                 (define-public (f (amount uint) (owner principal))
                   (begin
                     (try! (ft-mint? token amount owner))
                     (ft-burn? token amount owner)))",
                vec![Value::UInt(1), principal()],
            ),
        ] {
            crosscheck_cost(snippet, "f", &arguments);
        }
    }

    /// NFT costs scale with the serialized identifier in the current epoch,
    /// not with the token name. The setup mints run during deployment and are
    /// excluded from the measured get, transfer, and burn calls.
    #[test]
    fn charges_non_fungible_token_operations() {
        for snippet in [
            "(define-non-fungible-token token uint)
             (define-public (f) (nft-mint? token u1 tx-sender))",
            "(define-non-fungible-token token uint)
             (nft-mint? token u1 tx-sender)
             (define-public (f) (ok (nft-get-owner? token u1)))",
            "(define-non-fungible-token token uint)
             (nft-mint? token u1 tx-sender)
             (define-public (f)
               (nft-transfer? token u1 tx-sender current-contract))",
            "(define-non-fungible-token token uint)
             (nft-mint? token u1 tx-sender)
             (define-public (f) (nft-burn? token u1 tx-sender))",
        ] {
            crosscheck_cost(snippet, "f", &[]);
        }
    }

    #[test]
    fn charges_stx_and_non_fungible_token_operands_in_place() {
        let principal = || {
            Value::Principal(
                clarity::vm::types::PrincipalData::parse("S1G2081040G2081040G2081040G208105NK8PE5")
                    .expect("principal"),
            )
        };
        for (snippet, arguments) in [
            (
                "(define-public (f (amount uint) (recipient principal))
                   (stx-transfer? amount tx-sender recipient))",
                vec![Value::UInt(1), principal()],
            ),
            (
                "(define-public (f (amount uint) (recipient principal) (memo (buff 34)))
                   (stx-transfer-memo? amount tx-sender recipient memo))",
                vec![
                    Value::UInt(1),
                    principal(),
                    Value::buff_from(vec![1]).expect("buffer"),
                ],
            ),
        ] {
            crosscheck_cost(snippet, "f", &arguments);
        }
        crosscheck_cost(
            "(define-non-fungible-token token uint)
             (define-public (f)
               (let ((id u1) (owner tx-sender) (recipient current-contract))
                 (begin
                   (try! (nft-mint? token id owner))
                   (nft-get-owner? token id)
                   (try! (nft-transfer? token id owner recipient))
                   (nft-burn? token id recipient))))",
            "f",
            &[],
        );
    }

    /// A comparison or a branch over a bound name costs what the interpreter
    /// charges.
    ///
    /// The ordered comparisons and `if` read their operands where they are
    /// rather than copying them out of a binding, so charging those reads as
    /// copies cost more than the interpreter charges — once per bound name,
    /// which compounds through a contract that compares as often as pox-5.
    #[test]
    fn charges_a_comparison_or_branch_over_a_bound_name() {
        for (snippet, args) in [
            (
                "(define-public (f (a uint)) (ok (> a u0)))",
                vec![Value::UInt(3)],
            ),
            (
                "(define-public (f (a uint) (b uint)) (ok (and (< a b) (>= b a))))",
                vec![Value::UInt(3), Value::UInt(1)],
            ),
            (
                "(define-private (g (a uint)) (> a u0)) (define-public (f) (ok (g u1)))",
                vec![],
            ),
            ("(define-public (f) (let ((a u1)) (ok (> a u0))))", vec![]),
            (
                "(define-private (g (a uint)) (if (is-eq a u0) u1 u2)) \
                 (define-public (f) (ok (g u1)))",
                vec![],
            ),
        ] {
            crosscheck_cost(snippet, "f", &args);
        }
    }

    /// `slice?` reads its positions where they are, as `element-at?` does.
    ///
    /// This is `.pox-5 remove-staker-from-cycles` reduced: it slices a
    /// ninety-six element list by a *bound* count and folds over the result.
    /// nano charged a copy of that count out of its binding — 33 for a uint —
    /// where the interpreter reads it in place, so the same fold cost 33 more
    /// inside a function, which passes the count, than inlined with a literal.
    ///
    /// `element-at?` beside it was already exact, which is what showed the
    /// charge was `slice?`'s rather than the fold's.
    #[test]
    fn charges_a_wrapped_fold_like_an_inlined_one() {
        let accumulator = "{staker: principal, first-reward-cycle: uint, is-stx-staking: bool}";
        let initial = "{staker: tx-sender, first-reward-cycle: u1, is-stx-staking: true}";
        let list = "u0 u1 u2 u3 u4 u5 u6 u7 u8 u9 u10 u11 u12 u13 u14 u15 u16 u17 u18 u19 u20 u21 u22 u23 u24 u25 u26 u27 u28 u29 u30 u31 u32 u33 u34 u35 u36 u37 u38 u39 u40 u41 u42 u43 u44 u45 u46 u47 u48 u49 u50 u51 u52 u53 u54 u55 u56 u57 u58 u59 u60 u61 u62 u63 u64 u65 u66 u67 u68 u69 u70 u71 u72 u73 u74 u75 u76 u77 u78 u79 u80 u81 u82 u83 u84 u85 u86 u87 u88 u89 u90 u91 u92 u93 u94 u95";
        let snippet = format!(
            "(define-private (g (i uint) (a (response {accumulator} uint))) a)
             (define-private (h (n uint))
               (ok (try! (fold g (unwrap-panic (slice? (list {list}) u0 n)) (ok {initial})))))
             (define-public (f) (begin (try! (h u0)) (ok true)))"
        );
        crosscheck_cost(&snippet, "f", &[]);
    }

    /// A computed `slice?` bound evaluates its own operands normally, and a
    /// successful slice is charged for the bytes it returns.
    #[test]
    fn charges_a_slice_with_computed_bounds() {
        crosscheck_cost(
            "(define-public (f) (ok (slice? 0x001122334455 u1 u3)))",
            "f",
            &[],
        );
        crosscheck_cost(
            "(define-public (f (bytes (buff 8192)) (left uint)) (ok u1))",
            "f",
            &[
                Value::buff_from(vec![0x5a; 2_007]).expect("buffer"),
                Value::UInt(3),
            ],
        );
        crosscheck_cost(
            "(define-public (f (bytes (buff 8)) (left uint))
                 (ok (slice? bytes (+ left u0) u4)))",
            "f",
            &[
                Value::buff_from(vec![0, 1, 2, 3, 4, 5]).expect("buffer"),
                Value::UInt(1),
            ],
        );
        crosscheck_cost(
            "(define-public (f (bytes (buff 8)))
                 (ok (slice? bytes u1 (len bytes))))",
            "f",
            &[Value::buff_from(vec![0, 1, 2, 3, 4, 5]).expect("buffer")],
        );
        crosscheck_cost(
            "(define-public (f (bytes (buff 8192)))
                 (ok (slice? bytes u3 (len bytes))))",
            "f",
            &[Value::buff_from(vec![0x5a; 2_007]).expect("buffer")],
        );
        crosscheck_cost(
            "(define-public (f (bytes (buff 8192)) (left uint))
                 (ok (slice? bytes (+ left u0) (len bytes))))",
            "f",
            &[
                Value::buff_from(vec![0x5a; 2_007]).expect("buffer"),
                Value::UInt(3),
            ],
        );
        crosscheck_cost(
            "(define-public (f (bytes (buff 8192)))
                 (ok (slice?
                   (list
                     (default-to 0x (slice? bytes u0 u2))
                     (default-to 0x (slice? bytes u2 u4)))
                   u0 u1)))",
            "f",
            &[Value::buff_from(vec![0, 1, 2, 3]).expect("buffer")],
        );
    }

    #[test]
    fn charges_a_list_of_runtime_shaped_tuples() {
        let update = Value::Tuple(
            TupleData::from_data(vec![
                (
                    ClarityName::from_literal("proof"),
                    Value::cons_list_unsanitized(vec![
                        Value::buff_from(vec![0x5a; 3]).expect("proof node")
                    ])
                    .expect("proof list"),
                ),
                (ClarityName::from_literal("value"), Value::UInt(42)),
            ])
            .expect("update tuple"),
        );
        crosscheck_cost(
            "(define-public (f
                 (update { proof: (list 8 (buff 20)), value: uint }))
               (ok (list update update update)))",
            "f",
            &[update],
        );
    }

    #[test]
    fn charges_secp256k1_recover_arguments_in_place() {
        let message =
            hex::decode("de5b9eb9e7c5592930eb2e30a01369c36586d872082ed8181ee83d2a0ec20f04")
                .expect("message hash");
        let signature = hex::decode(
            "8738487ebe69b93d8e51583be8eee50bb4213fc49c767d329632730cc193b8735\
             54428fc936ca3569afc15f1c9365f6591d6251a89fee9c9ac661116824d3a1301",
        )
        .expect("signature");
        crosscheck_cost(
            "(define-public (f (message (buff 32)) (signature (buff 65)))
                 (secp256k1-recover? message signature))",
            "f",
            &[
                Value::buff_from(message).expect("message buffer"),
                Value::buff_from(signature).expect("signature buffer"),
            ],
        );
    }

    /// A constant counts towards the contract's size, which is what a
    /// `contract-call?` pays `LoadContract` for.
    ///
    /// `save_constant` inserted the value and left `data_size` alone, so every
    /// constant a contract defines was free to load — `.pox-5` defines around
    /// sixty and was under-charged by 1,032 on every call into it. The deficit
    /// is exactly the value's size: a bool 1, a uint 16, a response 17.
    #[test]
    fn charges_a_contract_for_the_constants_it_holds() {
        let kinds: [fn(usize) -> String; 6] = [
            |k| format!("(define-constant C{k} (err u{k}))\n"),
            |k| format!("(define-constant C{k} (ok u{k}))\n"),
            |k| format!("(define-constant C{k} u{k})\n"),
            |k| format!("(define-constant C{k} true)\n"),
            |k| format!("(define-constant C{k} (some u{k}))\n"),
            |k| format!("(define-constant C{k} {{a: u{k}}})\n"),
        ];
        for make in kinds {
            let body: String = (0..40).map(make).collect();
            let callee = format!("{body}(define-read-only (h) u1)");
            let caller = "(define-public (f) (ok (contract-call? \
                'S1G2081040G2081040G2081040G208105NK8PE5.callee h)))";
            crosscheck_cost_multi_contract(&[("callee", &callee), ("caller", caller)], "f", &[]);
        }
    }

    /// `print` costs what the value costs, not what its type is to write down.
    ///
    /// `special_print` charges for `input.size()`. nano charged for the length
    /// of the type's textual form, which is a different quantity that happens
    /// to be close for simple values and drifts as soon as a tuple carries
    /// long field names — the shape every `.pox-5` event print has.
    #[test]
    fn charges_print_for_the_value() {
        for snippet in [
            "(define-public (f) (ok (print {a: u0, b: u1})))",
            "(define-public (f) (ok (print {aaaaaaaaaaaaaaaaaaa: u0, bbbbbbbbbbbbbbbb: u1})))",
            "(define-public (f) (ok (print tx-sender)))",
            "(define-public (f) (ok (print 0x0011223344)))",
            "(define-public (f) (ok (print (if false (some 0x00) none))))",
            "(define-public (f) (ok (print {a: (list u1 u2), b: {c: \"xy\", d: (some tx-sender)}})))",
            "(define-public (f) (let ((r {aaaaaaaaaaaaaaaaaaa: u1, bbbbbbbbbbbbbbbb: tx-sender}))
               (begin (print (merge {topic: \"stake-update\"} r)) (ok r))))",
        ] {
            crosscheck_cost(snippet, "f", &[]);
        }
    }

    /// An expression that aborts part-way pays only for what it did.
    ///
    /// A word the interpreter treats as a native function is charged through
    /// `dispatch_args`, once its arguments have been evaluated. Charging
    /// before them costs nothing while everything succeeds — the same charges
    /// land, in a different order — and overcharges the moment an operand
    /// aborts, because the enclosing work is paid for and never done.
    ///
    /// It compounds with nesting: an abort under three enclosing operations
    /// was charged for all three. Special forms such as `map` and `if` are
    /// different and do charge first, which is why this is per word rather
    /// than a rule about all of them.
    #[test]
    fn charges_an_aborted_expression_for_what_it_did() {
        for snippet in [
            "(define-public (f) (ok (- u0 u1)))",
            "(define-public (f) (ok (+ (- u0 u1) u1)))",
            "(define-public (f) (ok (+ (* (- u0 u1) u2) u1)))",
            "(define-data-var a uint u0) (define-public (f) (ok (/ u1 (var-get a))))",
            "(define-public (f) (ok (+ u340282366920938463463374607431768211455 u1)))",
            "(define-public (f) (begin (asserts! false (err u47)) (ok true)))",
            "(define-public (f) (ok (unwrap! (if false (some u1) none) (err u1))))",
            "(define-public (f) (begin (- u0 u1) (ok (* u2 u3))))",
            "(define-public (f) (begin (- u0 u1) (* u2 u3) (ok true)))",
            "(define-public (f) (ok (some (- u0 u1))))",
            "(define-public (f) (ok (err (- u0 u1))))",
            // Every word that takes an operand has to survive that operand
            // aborting, so the sweep is wide rather than pointed: `try!`,
            // `tuple`, `default-to` and `append` were each charging first, and
            // `try!` is the one `.pox-5 stake` runs into.
            "(define-public (f) (ok (not (is-eq (- u0 u1) u0))))",
            "(define-public (f) (ok (is-eq (- u0 u1) u0)))",
            "(define-public (f) (ok (< (- u0 u1) u0)))",
            "(define-public (f) (ok (and true (is-eq (- u0 u1) u0))))",
            "(define-public (f) (ok (or false (is-eq (- u0 u1) u0))))",
            "(define-public (f) (ok (to-int (- u0 u1))))",
            "(define-public (f) (ok (list u1 (- u0 u1))))",
            "(define-public (f) (ok {a: (- u0 u1)}))",
            "(define-public (f) (ok (default-to u0 (some (- u0 u1)))))",
            "(define-public (f) (ok (append (list u1) (- u0 u1))))",
            "(define-public (f) (ok (concat (list u1) (list (- u0 u1)))))",
            "(define-map m uint uint) (define-public (f) (ok (map-set m u1 (- u0 u1))))",
            "(define-data-var v uint u0) (define-public (f) (ok (var-set v (- u0 u1))))",
            "(define-public (f) (ok (if true (- u0 u1) u0)))",
            "(define-public (f) (let ((a (- u0 u1))) (ok a)))",
            "(define-public (f) (ok (print (- u0 u1))))",
            "(define-public (f) (ok (sha256 (- u0 u1))))",
            "(define-public (f) (stx-transfer? (- u0 u1) tx-sender tx-sender))",
            "(define-map m principal {x: uint, y: uint})
             (define-private (g (p principal) (a {x: uint, y: uint})) (if true (ok true) (err u1)))
             (define-public (f)
               (begin (try! (g tx-sender (unwrap! (map-get? m tx-sender) (err u1)))) (ok true)))",
        ] {
            crosscheck_cost(snippet, "f", &[]);
        }
    }

    /// Applying a word to each element of a `fold`, `map` or `filter` costs
    /// what applying it anywhere else costs.
    ///
    /// Two charges were missing. A *variadic* word is charged by its caller
    /// rather than by its own `visit`, and these three call `visit` directly,
    /// so `(fold * ...)` never paid for the multiply — 31 an element. And
    /// resolving the applied function's name, which `fold` charged and the
    /// other two did not, is a flat 16.
    #[test]
    fn charges_a_native_fold() {
        for snippet in [
            "(define-public (f) (ok (fold * (list u2 u2 u2) u1)))",
            "(define-public (f) (ok (fold + (list u1 u1 u1) u0)))",
            "(define-public (f) (ok (map + (list u1 u1) (list u2 u2))))",
            "(define-public (f) (ok (map not (list true false true))))",
            "(define-public (f) (ok (filter not (list true false true))))",
            "(define-private (g (i uint) (a uint)) (* a i))
             (define-public (f) (ok (fold g (list u2 u2 u2) u1)))",
        ] {
            crosscheck_cost(snippet, "f", &[]);
        }
    }

    /// A binding with a placeholder list keeps its actual, empty-list size
    /// when `fold` widens the occurrence to the callback's accumulator type.
    #[test]
    fn charges_copying_a_widened_fold_accumulator() {
        crosscheck_cost(
            "(define-private (step (value uint) (acc {word: uint, fields: (list 8 uint)}))
                 {word: (get word acc),
                  fields: (unwrap-panic (as-max-len? (append (get fields acc) value) u8))})
             (define-public (f)
                 (let ((init {word: u0, fields: (list)}))
                     (ok (fold step (list u1) init))))",
            "f",
            &[],
        );
    }

    /// `let` is scaled by its number of bindings.
    #[test]
    fn charges_let_by_binding_count() {
        for snippet in [
            "(define-public (f) (let ((a u1)) (ok u0)))",
            "(define-public (f)
                 (let ((a u1) (b u2) (c u3) (d u4) (e u5) (g u6) (h u7) (i u8))
                     (ok u0)))",
            "(define-public (f)
                 (let ((a u1) (b u2) (c u3) (d u4) (e u5) (g u6) (h u7) (i u8)
                       (j u9) (k u10) (l u11) (m u12) (n u13) (o u14) (p u15) (q u16))
                     (ok u0)))",
        ] {
            crosscheck_cost(snippet, "f", &[]);
        }
    }

    /// A trait value forwarded through `contract-call?` is charged at the size
    /// of the callable value that the receiving public function sees.
    #[test]
    fn charges_a_forwarded_trait_argument_for_what_the_callee_receives() {
        let trait_definition = "(define-trait route-trait ((route () (response bool uint))))";
        let callee = "(use-trait route-trait .trait-definition.route-trait)
                      (define-public (route (target <route-trait>)) (ok true))";
        let caller = "(use-trait route-trait .trait-definition.route-trait)
                      (define-public (f (target <route-trait>))
                        (contract-call? .callee route target))";
        let target = Value::Principal(
            clarity::vm::types::PrincipalData::parse_qualified_contract_principal(
                "S1G2081040G2081040G2081040G208105NK8PE5.trait-definition",
            )
            .expect("contract principal"),
        );
        crosscheck_cost_multi_contract(
            &[
                ("trait-definition", trait_definition),
                ("callee", callee),
                ("caller", caller),
            ],
            "f",
            &[target],
        );
    }

    /// A contract literal is still a principal when the caller evaluates it,
    /// even when the receiving function declares a trait parameter.
    #[test]
    fn charges_a_cross_contract_trait_literal_as_a_principal() {
        let trait_definition = "(define-trait route-trait ())";
        let target = "(impl-trait .trait-definition.route-trait)";
        let callee = "(use-trait route-trait .trait-definition.route-trait)
                      (define-public (take (target <route-trait>)) (ok true))";
        let caller = "(define-public (f)
                        (contract-call? .callee take .target))";
        crosscheck_cost_multi_contract(
            &[
                ("trait-definition", trait_definition),
                ("target", target),
                ("callee", callee),
                ("caller", caller),
            ],
            "f",
            &[],
        );
    }

    /// Higher-order public/read-only calls carry the actual size of each value
    /// into the callee just like an ordinary call. The list's entry type allows
    /// eight bytes, but each string below has a different runtime size.
    #[test]
    fn charges_higher_order_arguments_by_their_values() {
        for (snippet, function) in [
            (
                "(define-read-only (size (item (string-ascii 8))) (len item))
                 (define-public (mapped)
                   (ok (map size (list \"a\" \"abc\" \"abcdefgh\"))))",
                "mapped",
            ),
            (
                "(define-read-only (step (item (string-ascii 8)) (acc uint))
                   (+ acc (len item)))
                 (define-public (folded)
                   (ok (fold step
                     (list \"a\" \"abc\" \"abcdefgh\")
                     u0)))",
                "folded",
            ),
        ] {
            crosscheck_cost(snippet, function, &[]);
        }
    }

    /// `filter` passes each element to its predicate before any declared-type
    /// widening, just like an ordinary private call.
    #[test]
    fn charges_filter_predicate_arguments_by_their_values() {
        let entry = Value::Tuple(
            TupleData::from_data(vec![(
                ClarityName::from_literal("payload"),
                Value::buff_from(vec![0x5a; 7]).expect("payload buffer"),
            )])
            .expect("entry tuple"),
        );
        let entries = Value::cons_list_unsanitized(vec![entry.clone(), entry.clone(), entry])
            .expect("entries list");
        crosscheck_cost(
            "(define-private (keep (entry {payload: (buff 100)})) true)
             (define-public (f (entries (list 3 {payload: (buff 100)})))
               (ok (filter keep entries)))",
            "f",
            &[entries],
        );
    }

    /// A transaction argument reaches a public function as a serialized
    /// principal, is cast to the trait in the declared list-entry type, and is
    /// then read back out of the tuple by `element-at?` and `get`.
    #[test]
    fn reads_a_trait_from_a_tuple_inside_a_list() {
        let trait_definition = "(define-trait decoder-trait ((decode () (response uint uint))))";
        let decoder_contract = "(impl-trait .trait-definition.decoder-trait)
                                (define-public (decode) (ok u42))";
        let reader = "(use-trait decoder-trait .trait-definition.decoder-trait)
                      (define-public (read
                          (plans (list 4 { decoder: <decoder-trait>, version: uint })))
                        (let ((plan (unwrap-panic (element-at? plans u0)))
                              (decoder (get decoder plan)))
                          (contract-call? decoder decode)))";
        let decoder =
            QualifiedContractIdentifier::parse("S1G2081040G2081040G2081040G208105NK8PE5.decoder")
                .expect("contract identifier");
        let plan = Value::Tuple(
            TupleData::from_data(vec![
                (
                    ClarityName::from_literal("decoder"),
                    Value::Principal(PrincipalData::Contract(decoder)),
                ),
                (ClarityName::from_literal("version"), Value::UInt(4)),
            ])
            .expect("execution-plan tuple"),
        );
        let plans = Value::cons_list_unsanitized(vec![plan]).expect("execution-plan list");

        crosscheck_cost_multi_contract(
            &[
                ("trait-definition", trait_definition),
                ("decoder", decoder_contract),
                ("reader", reader),
            ],
            "read",
            &[plans],
        );
    }

    /// The failing mainnet path first passes a tuple of literal contract
    /// principals across a contract boundary after a large bound buffer, then
    /// reads one field as the trait the callee declared.
    #[test]
    fn reads_a_trait_from_a_cross_contract_tuple() {
        let trait_definition = "(define-trait decoder-trait ((decode () (response uint uint))))
             (define-trait storage-trait ((store () (response uint uint))))
             (define-trait core-trait ((verify () (response uint uint))))";
        let decoder = "(impl-trait .trait-definition.decoder-trait)
                       (impl-trait .trait-definition.storage-trait)
                       (impl-trait .trait-definition.core-trait)
                       (define-public (decode) (ok u42))
                       (define-public (store) (ok u42))
                       (define-public (verify) (ok u42))";
        let reader = "(use-trait decoder-trait .trait-definition.decoder-trait)
                      (use-trait storage-trait .trait-definition.storage-trait)
                      (use-trait core-trait .trait-definition.core-trait)
                      (define-public (read (bytes (buff 8192))
                          (plan { decoder: <decoder-trait>,
                                  storage: <storage-trait>,
                                  core: <core-trait> }))
                        (let ((decoder (get decoder plan)))
                          (contract-call? decoder decode)))";
        let caller = "(define-public (f (bytes (buff 8192)))
                        (contract-call? .reader read bytes
                          { decoder: .decoder,
                            storage: .decoder,
                            core: .decoder }))";
        let bytes = Value::buff_from(vec![0x5a; 2_007]).expect("price-feed bytes");

        crosscheck_cost_multi_contract(
            &[
                ("trait-definition", trait_definition),
                ("decoder", decoder),
                ("reader", reader),
                ("caller", caller),
            ],
            "f",
            &[bytes],
        );
    }

    /// The mainnet refusal forwards the bound execution plan through a second
    /// contract call as an optional tuple before reading its trait fields.
    #[test]
    fn forwards_a_bound_trait_tuple_inside_an_optional() {
        let trait_definition = "(define-trait decoder-trait ((decode () (response uint uint))))
             (define-trait storage-trait ((store () (response uint uint))))
             (define-trait core-trait ((verify () (response uint uint))))";
        let decoder = "(impl-trait .trait-definition.decoder-trait)
                       (impl-trait .trait-definition.storage-trait)
                       (impl-trait .trait-definition.core-trait)
                       (define-public (decode) (ok u42))
                       (define-public (store) (ok u42))
                       (define-public (verify) (ok u42))";
        let governance = "(use-trait decoder-trait .trait-definition.decoder-trait)
                          (use-trait storage-trait .trait-definition.storage-trait)
                          (use-trait core-trait .trait-definition.core-trait)
                          (define-read-only (check
                              (former principal)
                              (plan-opt (optional {
                                  decoder: <decoder-trait>,
                                  storage: <storage-trait>,
                                  core: <core-trait> })))
                            (let ((plan (unwrap! plan-opt (err u1)))
                                  (decoder-contract (get decoder plan)))
                              (if (is-eq former (contract-of decoder-contract))
                                  (ok true)
                                  (err u2))))";
        let oracle = "(use-trait decoder-trait .trait-definition.decoder-trait)
                      (use-trait storage-trait .trait-definition.storage-trait)
                      (use-trait core-trait .trait-definition.core-trait)
                      (define-public (read
                          (bytes (buff 8192))
                          (plan { decoder: <decoder-trait>,
                                  storage: <storage-trait>,
                                  core: <core-trait> }))
                        (begin
                          (try! (contract-call? .governance check
                              contract-caller (some plan)))
                          (ok (len bytes))))";
        let caller = "(define-public (f (bytes (buff 8192)))
                        (contract-call? .oracle read bytes
                          { decoder: .decoder,
                            storage: .decoder,
                            core: .decoder }))";
        let bytes = Value::buff_from(vec![0x5a; 2_007]).expect("price-feed bytes");

        crosscheck_cost_multi_contract(
            &[
                ("trait-definition", trait_definition),
                ("decoder", decoder),
                ("governance", governance),
                ("oracle", oracle),
                ("caller", caller),
            ],
            "f",
            &[bytes],
        );
    }

    /// Cross-contract results are sanitized before the caller copies them.
    /// In particular, `none` narrows the nested optional metadata while a
    /// present buffer keeps it.
    #[test]
    fn charges_copying_a_sanitized_cross_contract_result() {
        let callee = r#"
            (define-map registry uint
                { id: uint, oracle: { callcode: (optional (buff 1)) } })
            (map-set registry u0 { id: u0, oracle: { callcode: none } })
            (map-set registry u1 { id: u1, oracle: { callcode: (some 0x02) } })
            (define-read-only (get-asset (which uint))
                (match (map-get? registry which)
                    asset (ok asset)
                    (err u1)))
        "#;
        let caller = r#"
            (define-public (f (which uint))
                (let ((asset (try! (contract-call? .callee get-asset which))))
                    (ok (get id asset))))
        "#;

        for which in [0, 1] {
            crosscheck_cost_multi_contract(
                &[("callee", callee), ("caller", caller)],
                "f",
                &[Value::UInt(which)],
            );
        }
    }

    /// A contract principal literal is charged before the public callee casts
    /// it to the trait the parameter declares.
    #[test]
    fn charges_a_local_public_trait_literal_as_a_principal() {
        let trait_definition = "(define-trait route-trait ((route () (response bool uint))))";
        let target = "(impl-trait .trait-definition.route-trait)
                      (define-public (route) (if true (ok true) (err u1)))";
        let caller = "(use-trait route-trait .trait-definition.route-trait)
                      (define-public (route-local (target <route-trait>)) (ok true))
                      (define-public (f) (route-local .target))";
        crosscheck_cost_multi_contract(
            &[
                ("trait-definition", trait_definition),
                ("target", target),
                ("caller", caller),
            ],
            "f",
            &[],
        );
    }

    #[test]
    fn charges_entering_a_function_once() {
        for (snippet, arguments) in [
            ("(define-public (f (a uint)) (ok a))", vec![Value::UInt(1)]),
            (
                "(define-public (f (a uint) (b uint)) (ok (+ a b)))",
                vec![Value::UInt(1), Value::UInt(2)],
            ),
            (
                "(define-private (g (a uint)) a) (define-public (f) (ok (g u1)))",
                vec![],
            ),
        ] {
            crosscheck_cost(snippet, "f", &arguments);
        }
    }

    #[test]
    fn charges_reading_a_bound_value() {
        for snippet in [
            "(define-public (f) (ok (let ((a { x: u1, y: u2 })) (get x a))))",
            "(define-private (g (a { x: uint })) (get x a)) (define-public (f) (ok (g { x: u1 })))",
        ] {
            crosscheck_cost(snippet, "f", &[]);
        }
    }

    #[test]
    fn charges_refusing_a_wide_runtime_shape_at_local_function_entry() {
        let wide = Value::some(Value::Tuple(
            TupleData::from_data(vec![
                (
                    ClarityName::try_from("full").expect("field name"),
                    Value::Bool(true),
                ),
                (
                    ClarityName::try_from("soft").expect("field name"),
                    Value::Bool(true),
                ),
            ])
            .expect("wide tuple"),
        ))
        .expect("optional tuple");
        crosscheck_cost(
            "(define-private (echo (entry {soft: bool})) entry) \
             (define-public (f (entry (optional {soft: bool, full: bool}))) \
               (ok (echo (default-to {soft: false} entry))))",
            "f",
            &[wide],
        );
    }

    #[test]
    fn charges_what_a_data_word_serializes_to() {
        for snippet in [
            "(define-data-var v uint u0) (define-public (f) (ok (var-get v)))",
            "(define-data-var v uint u0) (define-public (f) (ok (var-set v u7)))",
            "(define-data-var v (buff 100) 0x00) (define-public (f) (begin (var-set v 0x0102030405) (ok (var-get v))))",
            "(define-map m uint { a: uint, b: principal }) (define-public (f) (begin (map-set m u1 { a: u2, b: tx-sender }) (ok (map-get? m u1))))",
        ] {
            crosscheck_cost(snippet, "f", &[]);
        }
    }

    #[test]
    fn charges_a_cross_contract_call_once() {
        crosscheck_cost_multi_contract(
            &[
                ("callee", "(define-public (h (a uint)) (ok a))"),
                (
                    "caller",
                    "(define-public (f) (contract-call? .callee h u1))",
                ),
            ],
            "f",
            &[],
        );
    }

    #[test]
    fn charges_as_contract_safe_by_allowance_count() {
        let snippet = r#"
            (define-fungible-token token)
            (define-public (one)
                (as-contract? ((with-ft current-contract "token" u0)) u1))
            (define-public (two)
                (as-contract? ((with-stx u0)
                               (with-ft current-contract "token" u0)) u1))
        "#;

        crosscheck_cost(snippet, "one", &[]);
        crosscheck_cost(snippet, "two", &[]);
    }

    #[test]
    fn charges_as_contract_safe_when_an_allowance_is_violated() {
        crosscheck_cost_multi_contract(
            &[
                (
                    "callee",
                    "(define-public (send)
                        (as-contract? ((with-stx u0))
                            (try! (stx-transfer? u1 current-contract tx-sender))))",
                ),
                (
                    "caller",
                    "(define-public (f)
                        (begin
                            (try! (stx-transfer? u1 tx-sender .callee))
                            (contract-call? .callee send)))",
                ),
            ],
            "f",
            &[],
        );
    }
}
