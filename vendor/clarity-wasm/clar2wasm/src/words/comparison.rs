use clarity::vm::types::{SequenceSubtype, StringSubtype, TypeSignature};
use clarity::vm::{ClarityName, SymbolicExpression};

use super::{ComplexWord, Word};
use crate::check_args;
use crate::cost::WordCharge;
use crate::wasm_generator::{GeneratorError, WasmGenerator};
use crate::wasm_utils::ArgumentCountCheck;

trait CmpWord: ComplexWord {
    fn fn_name(&self) -> &'static str;
}

fn traverse_comparison(
    word: &impl CmpWord,
    generator: &mut WasmGenerator,
    builder: &mut walrus::InstrSeqBuilder,
    args: &[SymbolicExpression],
) -> Result<(), GeneratorError> {
    check_args!(generator, builder, 2, args.len(), ArgumentCountCheck::Exact);
    let arg_types = args
        .iter()
        .map(|arg| {
            generator.get_expr_type(arg).cloned().ok_or_else(|| {
                GeneratorError::TypeError("comparison argument must be typed".to_owned())
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let cost_size = arg_types
        .iter()
        .map(|ty| {
            ty.size()
                .map_err(|error| GeneratorError::TypeError(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .min()
        .ok_or_else(|| GeneratorError::InternalError("comparison requires arguments".to_owned()))?;
    word.charge(generator, builder, cost_size)?;

    let name = word.fn_name();

    let ty = &arg_types[0];

    let type_suffix = match ty {
        TypeSignature::IntType => "int",
        TypeSignature::UIntType => "uint",
        // same function for buffer and string-ascii
        TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::ASCII(_)))
        | TypeSignature::SequenceType(SequenceSubtype::BufferType(_)) => "buff",
        TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(_))) => {
            // For `string-utf8`, comparison is done on a codepoint-by-codepoint basis.
            // Comparing two codepoints is the act of comparing them on a byte-by-byte basis.
            // Since we already have 32-bit unicode scalars, we can just compare them with buff.
            "buff"
        }
        _ => {
            return Err(GeneratorError::TypeError(
                "invalid type for comparison".to_string(),
            ))
        }
    };

    let func = generator
        .module
        .funcs
        .by_name(&format!("stdlib.{name}-{type_suffix}"))
        .ok_or_else(|| {
            GeneratorError::InternalError(format!("function not found: {name}-{type_suffix}"))
        })?;

    for arg in args {
        generator.traverse_expr_as_borrowed_value(builder, arg)?;
    }
    builder.call(func);

    Ok(())
}

#[derive(Debug)]
pub struct CmpLess;

impl Word for CmpLess {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("<")
    }
}

impl ComplexWord for CmpLess {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        traverse_comparison(self, generator, builder, args)
    }
}

impl CmpWord for CmpLess {
    fn fn_name(&self) -> &'static str {
        "lt"
    }
}

#[derive(Debug)]
pub struct CmpLeq;

impl Word for CmpLeq {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("<=")
    }
}

impl ComplexWord for CmpLeq {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        traverse_comparison(self, generator, builder, args)
    }
}

impl CmpWord for CmpLeq {
    fn fn_name(&self) -> &'static str {
        "le"
    }
}

#[derive(Debug)]
pub struct CmpGreater;

impl Word for CmpGreater {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal(">")
    }
}

impl ComplexWord for CmpGreater {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        traverse_comparison(self, generator, builder, args)
    }
}

impl CmpWord for CmpGreater {
    fn fn_name(&self) -> &'static str {
        "gt"
    }
}

#[derive(Debug)]
pub struct CmpGeq;

impl Word for CmpGeq {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal(">=")
    }
}

impl ComplexWord for CmpGeq {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        traverse_comparison(self, generator, builder, args)
    }
}

impl CmpWord for CmpGeq {
    fn fn_name(&self) -> &'static str {
        "ge"
    }
}
