use clarity::vm::{ClarityName, SymbolicExpression};

use super::{ComplexWord, Word};
use crate::check_args;
use crate::cost::WordCharge;
use crate::wasm_generator::{GeneratorError, WasmGenerator};
use crate::wasm_utils::ArgumentCountCheck;

#[derive(Debug)]
pub struct Verify;

impl Word for Verify {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("secp256r1-verify")
    }
}

impl ComplexWord for Verify {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 3, args.len(), ArgumentCountCheck::Exact);
        self.charge(generator, builder, 0)?;
        for argument in args {
            generator.traverse_expr(builder, argument)?;
        }
        builder.call(generator.func_by_name("stdlib.secp256r1_verify"));
        Ok(())
    }
}
