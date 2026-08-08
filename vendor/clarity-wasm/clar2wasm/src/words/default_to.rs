use clarity::vm::types::TypeSignature;
use clarity::vm::{ClarityName, SymbolicExpression};
use walrus::ir::IfElse;
use walrus::ValType;

use super::{ComplexWord, Word};
use crate::check_args;
use crate::cost::WordCharge;
use crate::duck_type::{dt_needed_workspace, need_ducktyping};
use crate::wasm_generator::{
    clar2wasm_ty, drop_value, uses_packed_value, ArgumentsExt, GeneratorError, WasmGenerator,
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

        if uses_packed_value(&expr_type) {
            generator.note_control_arity(
                clar2wasm_ty(&expr_type).len(),
                clar2wasm_ty(&expr_type).len(),
            );
            let indicator = generator.module.locals.add(ValType::I32);
            builder.local_set(indicator);
            let default_locals = generator.save_to_locals(builder, &expr_type, true);
            let (result_offset, _) =
                generator.create_call_stack_local(builder, &expr_type, true, false);
            let mut then = builder.dangling_instr_seq(None);
            for local in &opt_val_locals {
                then.local_get(*local);
            }
            generator.write_to_memory(&mut then, result_offset, 0, &expr_type)?;
            let then = then.id();
            let mut else_ = builder.dangling_instr_seq(None);
            for local in &default_locals {
                else_.local_get(*local);
            }
            generator.write_to_memory(&mut else_, result_offset, 0, &expr_type)?;
            let else_ = else_.id();
            builder.local_get(indicator).instr(IfElse {
                consequent: then,
                alternative: else_,
            });
            generator.release_locals(opt_val_locals);
            generator.release_locals(default_locals);
            generator.read_from_memory(builder, result_offset, 0, &expr_type)?;
            return Ok(());
        }

        // Params and result types for the if_else branch
        let out_types = clar2wasm_ty(&expr_type);
        let block_type = generator.bounded_control_type(&out_types, &out_types)?;

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
    use clarity::vm::errors::VmExecutionError;
    use clarity::vm::types::TupleData;
    use clarity::vm::{ClarityName, Value};

    use crate::tools::{crosscheck_compare_only, evaluate, interpret};

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

    /// The tuple-supertype asymmetry, in the word that reaches it.
    ///
    /// `least_supertype` walks the *default's* fields and drops the payload's
    /// extras, so `(default-to { soft: false } entry)` over an `(optional {
    /// soft: bool, full: bool })` analyses as the one-field tuple.
    /// `native_default_to` then hands back whichever value its branch produced,
    /// unconverted — so on the `some` branch the answer's shape is the payload's
    /// and not the expression's analysed type, and on the `none` branch it is the
    /// default's. One expression, two runtime shapes.
    ///
    /// `traverse` above converts the payload to the analysed type instead,
    /// because a wasm value's representation is fixed by one static type. There
    /// is no third choice: narrowing reproduces the `none` branch and loses a
    /// field on the `some` branch, and widening would reproduce the `some` branch
    /// and have to invent a field for the other.
    ///
    /// Measured on `(some { soft: true, full: true })`, consensus serialization
    /// included:
    ///
    /// ```text
    /// returned  compiled    0c0000000104736f667403
    ///           interpreted 0c000000020466756c6c0304736f667403
    /// var-set   compiled    (ok { soft: true }), the var written
    ///           interpreted RuntimeCheck(TypeValueError), nothing written
    /// argument  compiled    { soft: true }
    ///           interpreted RuntimeCheck(TypeValueError)
    /// ```
    ///
    /// The last two are *state* divergences and not only receipt ones: both a
    /// narrow `define-data-var` and a narrow function parameter type-check at run
    /// time and refuse a differing field count, so the reference aborts the
    /// transaction where this commits a write and carries on. The parameter check
    /// is `clarity2_implicit_cast`, whose own comment calls the case "unreachable
    /// if the type-checker has already run successfully" — which is as close as
    /// the reference comes to saying that this type should not have been handed
    /// out.
    ///
    /// Asserted rather than `#[ignore]`d, and asserted on *both* engines. An
    /// ignored equality is a divergence nobody measures; the same divergence
    /// pinned in both directions cannot move — in either engine — without turning
    /// this red, and it is the reference's half that decides the chain. The `none`
    /// branch must still agree, because there the value *is* its analysed type.
    ///
    /// Accounted for in nano-stacks task 068, which records why no choice inside
    /// this word closes it: the conformant engine here is one whose values carry
    /// their shape at run time.
    #[test]
    fn clar_default_to_narrowing_answers_with_the_branch_the_reference_took() {
        const NARROWING: &str =
            "(define-read-only (whole (entry (optional { soft: bool, full: bool })))
               (default-to { soft: false } entry))";

        // The `some` branch: the payload's own two-field tuple in the reference,
        // the analysed one-field tuple here.
        let entry = "(whole (some { soft: true, full: true }))";
        let wide = interpret(&format!("{NARROWING} {entry}"));
        let narrow = evaluate(&format!("{NARROWING} {entry}"));
        assert_ne!(
            format!("{wide:?}"),
            format!("{narrow:?}"),
            "if this has closed, close the accounting in nano-stacks task 068 with it"
        );
        assert_eq!(
            format!("{wide:?}"),
            format!(
                "{:?}",
                Ok::<_, VmExecutionError>(Some(Value::Tuple(
                    TupleData::from_data(vec![
                        (ClarityName::from_literal("full"), Value::Bool(true)),
                        (ClarityName::from_literal("soft"), Value::Bool(true)),
                    ])
                    .unwrap()
                )))
            ),
            "the reference hands back the payload's own tuple"
        );
        assert_eq!(
            format!("{narrow:?}"),
            format!(
                "{:?}",
                Ok::<_, VmExecutionError>(Some(Value::Tuple(
                    TupleData::from_data(vec![(
                        ClarityName::from_literal("soft"),
                        Value::Bool(true)
                    )])
                    .unwrap()
                )))
            ),
            "and this hands back the analysed one"
        );

        // The `none` branch: the default's own value, which is the analysed type in
        // both, so it must agree.
        crosscheck_compare_only(&format!("{NARROWING} (whole none)"));
    }
}
