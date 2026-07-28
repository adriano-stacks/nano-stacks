use clarity::vm::{ClarityName, SymbolicExpression};

use super::{ComplexWord, Word};
use crate::check_args;
use crate::wasm_generator::{ArgumentsExt, GeneratorError, WasmGenerator};
use crate::wasm_utils::ArgumentCountCheck;

#[derive(Debug)]
pub struct VerifyMerkleProof;

impl Word for VerifyMerkleProof {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("verify-merkle-proof")
    }
}

impl ComplexWord for VerifyMerkleProof {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 5, args.len(), ArgumentCountCheck::Exact);
        for argument in args {
            generator.traverse_expr(builder, argument)?;
        }
        builder.call(generator.func_by_name("stdlib.verify_merkle_proof"));
        Ok(())
    }
}

#[derive(Debug)]
pub struct GetBitcoinTxOutput;

impl Word for GetBitcoinTxOutput {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("get-bitcoin-tx-output?")
    }
}

impl ComplexWord for GetBitcoinTxOutput {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 2, args.len(), ArgumentCountCheck::Exact);
        generator.traverse_expr(builder, args.get_expr(0)?)?;
        generator.traverse_expr(builder, args.get_expr(1)?)?;
        let return_type = generator
            .get_expr_type(expr)
            .ok_or_else(|| {
                GeneratorError::TypeError("get-bitcoin-tx-output? must be typed".into())
            })?
            .clone();
        let (result, size) = generator.create_call_stack_local(builder, &return_type, true, true);
        builder.local_get(result).i32_const(size);
        builder.call(generator.func_by_name("stdlib.get_bitcoin_tx_output"));
        generator
            .read_from_memory(builder, result, 0, &return_type)
            .map(|_| ())
    }
}
