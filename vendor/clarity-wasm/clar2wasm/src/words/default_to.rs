use clarity::vm::types::TypeSignature;
use clarity::vm::{ClarityName, SymbolicExpression};
use walrus::ir::InstrSeqType;

use super::{ComplexWord, Word};
use crate::check_args;
use crate::cost::WordCharge;
use crate::duck_type::{dt_needed_workspace, need_ducktyping};
use crate::wasm_generator::{
    clar2wasm_ty, drop_value, ArgumentsExt, GeneratorError, WasmGenerator,
};
use crate::wasm_utils::ArgumentCountCheck;

#[derive(Debug)]
pub struct DefaultTo;

impl Word for DefaultTo {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("default-to")
    }
}

impl ComplexWord for DefaultTo {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 2, args.len(), ArgumentCountCheck::Exact);

        // There are a `default` value and an `optional` arguments.
        // (default-to 767 (some 1))
        // i64              i64               i32        i64           i64
        // default-val-low, default-val-high, indicator, plc-val-low, plc-val-high
        let default = args.get_expr(0)?;
        let optional = args.get_expr(1)?;

        let Some(expr_type) = generator.get_expr_type(expr).cloned() else {
            return Err(GeneratorError::TypeError(
                "default-to expression should be typed".to_owned(),
            ));
        };

        // The default is this expression's value whenever the optional is
        // `none`, so it is laid out for the expression's type. Saying so is what
        // lets a placeholder literal — `none` standing in for an `(optional
        // uint)` — know how many slots to fill.
        generator.set_expr_type(default, expr_type.clone())?;

        // The optional is *not* told what to be. `default-to`'s type is
        // `least_supertype(default, inner)`, which walks the *default's* fields,
        // so `(default-to { soft: false } (map-get? m k))` over a map whose value
        // is `{ soft: bool, full: bool }` analyses as the one-field tuple while
        // `map-get?` reads the map's own value type and cannot produce anything
        // else. Asking it to left the dropped field on the stack — "values
        // remaining on stack at end of block", mainnet block 8,667,509's
        // `blacklist-susdh-v1`. So take the optional as analysed and convert its
        // payload below, where both layouts are in hand.
        let opt_ty = generator
            .get_expr_type(optional)
            .ok_or_else(|| {
                GeneratorError::TypeError("optional expression must be typed".to_owned())
            })?
            .clone();
        let TypeSignature::OptionalType(opt_val_ty) = opt_ty else {
            return Err(GeneratorError::TypeError(format!(
                "Expected an Optional type. Found {opt_ty:?}"
            )));
        };

        generator.traverse_args(builder, args)?;
        // Charged after the operands, as `dispatch_args` does; see `ok`.
        self.charge(generator, builder, 0)?;

        // The payload sits on top of the stack with the indicator underneath it,
        // so it converts in place before either is consumed. A `none` converts
        // its placeholder along with it, which costs a few instructions and
        // cannot be read: the indicator says the value is not there.
        if need_ducktyping(&opt_val_ty, &expr_type) {
            let workspace = match dt_needed_workspace(&expr_type) {
                0 => None,
                size => Some(generator.create_call_stack_bytes(builder, size as i32).0),
            };
            generator.duck_type(builder, &opt_val_ty, &expr_type, workspace)?;
        }
        let opt_val_locals = generator.save_to_locals(builder, &expr_type, true);

        // Params and result types for the if_else branch
        let out_types = clar2wasm_ty(&expr_type);
        let block_type = InstrSeqType::new(&mut generator.module.types, &out_types, &out_types);

        builder.if_else(
            block_type,
            |then| {
                drop_value(then, &expr_type);

                for opt_val_local in opt_val_locals {
                    then.local_get(opt_val_local);
                }
            },
            |_| {},
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::tools::evaluate;

    #[test]
    fn default_to_less_than_two_args() {
        let result = evaluate("(default-to 0)");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 2 arguments, got 1"));
    }

    #[test]
    fn default_to_more_than_two_args() {
        let result = evaluate("(default-to 0 1 2)");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 2 arguments, got 3"));
    }
}
