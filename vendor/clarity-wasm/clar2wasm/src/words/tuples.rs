use std::collections::BTreeMap;

use clarity::types::StacksEpochId;
use clarity::vm::types::{TupleTypeSignature, TypeSignature};
use clarity::vm::{ClarityName, SymbolicExpression};
use walrus::ir::BinaryOp;
use walrus::ValType;

use super::{ComplexWord, Word};
use crate::check_args;
use crate::cost::WordCharge;
use crate::wasm_generator::{clar2wasm_ty, drop_value, GeneratorError, WasmGenerator};
use crate::wasm_utils::{check_argument_count, ArgumentCountCheck};

#[derive(Debug)]
pub struct TupleCons;

impl Word for TupleCons {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("tuple")
    }
}

impl ComplexWord for TupleCons {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        let args_len = args.len();

        check_argument_count(generator, builder, 1, args_len, ArgumentCountCheck::AtLeast)?;

        let result_ty = generator
            .get_expr_type(expr)
            .ok_or_else(|| GeneratorError::TypeError("tuple expression must be typed".to_string()))?
            .clone();

        let mut tuple_ty = match &result_ty {
            TypeSignature::TupleType(ref tuple) => tuple.get_type_map().clone(),
            _ => return Err(GeneratorError::TypeError("expected tuple type".to_string())),
        };

        // The args for `tuple` should be pairs of values, with the first value
        // being the key and the second being the value.
        let mut values = Vec::with_capacity(args.len());
        for arg in args {
            let list = arg.match_list().ok_or_else(|| {
                GeneratorError::InternalError("expected key-value pairs in tuple".to_string())
            })?;
            if list.len() != 2 {
                return Err(GeneratorError::InternalError(
                    "expected key-value pairs in tuple".to_string(),
                ));
            }

            let key = list[0].match_atom().ok_or_else(|| {
                GeneratorError::InternalError("expected key-value pairs in tuple".to_string())
            })?;
            values.push((key, &list[1]));
        }

        // Since we have to evaluate the fields in the order of definition but the result will be
        // in the lexicographic order of the keys, we'll add locals to store all evaluated fields.
        let mut locals_map = BTreeMap::new();

        // Now we can iterate over the fields and evaluate them.
        for (key, value) in values {
            let Some(value_ty) = tuple_ty.remove(key) else {
                // Some operations, such as `append`, sanitize a wider tuple
                // literal to their narrower result type. Its extra fields are
                // still evaluated before being discarded.
                let value_ty = generator.get_expr_type(value).cloned().ok_or_else(|| {
                    GeneratorError::TypeError("tuple field expression must be typed".to_string())
                })?;
                generator.traverse_expr(builder, value)?;
                drop_value(builder, &value_ty);
                continue;
            };

            let mut source_ty = generator.value_type_before_context(value).ok_or_else(|| {
                GeneratorError::TypeError("tuple field expression must be typed".to_string())
            })?;
            // A bare `none` is analysed as `NoType`, whose one-slot Wasm
            // placeholder cannot represent the field's contextual layout.
            if source_ty == TypeSignature::NoType {
                source_ty = value_ty;
                generator.set_expr_type(value, source_ty.clone())?;
            }
            generator.traverse_expr(builder, value)?;
            let locals = generator.save_to_locals(builder, &source_ty, true);
            locals_map.insert(key, (source_ty, locals));
        }

        // Charged after the operands, as `dispatch_args` does; see `ok`.
        self.charge(generator, builder, args_len as u32)?;

        // Make sure that all the tuples keys were defined
        if !tuple_ty.is_empty() {
            return Err(GeneratorError::TypeError(
                "Tuple should define each of its fields".to_owned(),
            ));
        }

        // Finally load the locals onto the stack
        let source_ty = TypeSignature::TupleType(
            TupleTypeSignature::try_from(
                locals_map
                    .iter()
                    .map(|(name, (ty, _))| ((*name).clone(), ty.clone()))
                    .collect::<Vec<_>>(),
            )
            .map_err(|error| GeneratorError::TypeError(error.to_string()))?,
        );
        let locals: Vec<_> = locals_map
            .into_values()
            .flat_map(|(_, locals)| locals)
            .collect();
        builder.i32_const(0);
        for local in &locals {
            builder.local_get(*local);
        }
        // The fields are on the stack; the slots they were saved in are dead.
        generator.release_locals(locals);

        if source_ty != result_ty || generator.type_for_serialization(&source_ty) != source_ty {
            generator.capture_runtime_shape(builder, &source_ty)?;
            generator.duck_type_preserve(builder, &source_ty, &result_ty, None)?;
        }

        Ok(())
    }
}

#[derive(Debug)]
pub struct TupleGet;

impl Word for TupleGet {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("get")
    }
}

impl ComplexWord for TupleGet {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 2, args.len(), ArgumentCountCheck::Exact);

        let target_field_name = args[0]
            .match_atom()
            .ok_or_else(|| GeneratorError::InternalError("expected key name".into()))?;

        let (tuple_ty, tuple_is_optional) = generator
            .get_expr_type(&args[1])
            .ok_or_else(|| GeneratorError::TypeError("tuple expression must be typed".to_string()))
            .and_then(|lhs_ty| match lhs_ty {
                TypeSignature::TupleType(tuple) => Ok((tuple, false)),
                TypeSignature::OptionalType(boxed) => match **boxed {
                    TypeSignature::TupleType(ref tuple) => Ok((tuple, true)),
                    _ => Err(GeneratorError::TypeError("expected tuple type".to_string())),
                },
                _ => Err(GeneratorError::TypeError("expected tuple type".to_string())),
            })?;
        let tuple_ty = tuple_ty.clone();

        // Traverse the tuple argument, leaving it on top of the stack.
        generator.traverse_expr(builder, &args[1])?;

        // Determine the wasm types for each field of the tuple
        let field_types = tuple_ty.get_type_map();

        self.charge(generator, builder, field_types.iter().len() as u32)?;

        // Create locals for the target field
        let field_ty = field_types
            .get(target_field_name)
            .ok_or_else(|| {
                GeneratorError::InternalError(format!(
                    "missing field '{target_field_name}' in tuple"
                ))
            })?
            .clone();
        let wasm_types = clar2wasm_ty(&field_ty);
        let mut val_locals = Vec::with_capacity(wasm_types.len());
        for local_ty in wasm_types.iter().rev() {
            let local = generator.alloc_local(*local_ty);
            val_locals.push(local);
        }

        // Loop through the fields of the tuple, in reverse order. When we find
        // the target field, we'll store it in the locals we created above. All
        // other fields will be dropped.
        for (field_name, field_ty) in field_types.iter().rev() {
            // If this is the target field, store it in the locals we created
            // above.
            if field_name == target_field_name {
                for local in val_locals.iter() {
                    builder.local_set(*local);
                }
            } else {
                drop_value(builder, field_ty);
            }
        }
        // Drop the tuple's root runtime-shape handle. The extracted field's
        // own handle, if composite, remains part of that field's slots.
        builder.drop();

        // Load the target field from the locals we created above.
        for local in val_locals.iter().rev() {
            builder.local_get(*local);
        }
        generator.release_locals(val_locals);

        let result_ty = generator
            .get_expr_type(expr)
            .ok_or_else(|| GeneratorError::TypeError("get expression must be typed".to_owned()))?
            .clone();
        let extracted_ty = if tuple_is_optional {
            TypeSignature::OptionalType(Box::new(field_ty))
        } else {
            field_ty
        };
        generator.duck_type(builder, &extracted_ty, &result_ty, None)?;

        Ok(())
    }
}

#[derive(Debug)]
pub struct TupleMerge;

impl Word for TupleMerge {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("merge")
    }
}

impl ComplexWord for TupleMerge {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 2, args.len(), ArgumentCountCheck::Exact);
        let serialization_size = generator.borrow_local(ValType::I32);

        if generator.contract_analysis.epoch < StacksEpochId::Epoch2_05 {
            self.charge(generator, builder, args.len() as u32)?;
        }
        let lhs_tuple_ty = generator
            .get_expr_type(&args[0])
            .ok_or_else(|| GeneratorError::TypeError("tuple expression must be typed".to_string()))
            .and_then(|lhs_ty| match lhs_ty {
                TypeSignature::TupleType(tuple) => Ok(tuple),
                _ => Err(GeneratorError::TypeError("expected tuple type".to_string())),
            })?
            .clone();

        let result_ty = generator
            .get_expr_type(expr)
            .cloned()
            .ok_or_else(|| GeneratorError::TypeError("merge expression must be typed".to_owned()));

        // The overriding tuple has to be built as the *result's* field types,
        // not its own. `(merge t { f: none })` analyses `none` as
        // `(optional NoType)`, and laying that out where the result's
        // `(optional uint)` belongs writes an i32 where the value is an
        // indicator and two i64s — a module that compiles and will not load.
        let coerced_rhs = match (result_ty.as_ref().ok(), generator.get_expr_type(&args[1])) {
            (Some(TypeSignature::TupleType(result_tuple)), Some(TypeSignature::TupleType(rhs))) => {
                let fields: Vec<_> = rhs
                    .get_type_map()
                    .keys()
                    .filter_map(|name| Some((name.clone(), result_tuple.field_type(name)?.clone())))
                    .collect();
                clarity::vm::types::TupleTypeSignature::try_from(fields).ok()
            }
            _ => None,
        };
        if let Some(coerced) = coerced_rhs {
            generator.set_expr_type(&args[1], TypeSignature::TupleType(coerced))?;
        }

        let rhs_tuple_ty = generator
            .get_expr_type(&args[1])
            .ok_or_else(|| GeneratorError::TypeError("tuple expression must be typed".to_string()))
            .and_then(|lhs_ty| match lhs_ty {
                TypeSignature::TupleType(tuple) => Ok(tuple),
                _ => Err(GeneratorError::TypeError("expected tuple type".to_string())),
            })?
            .clone();

        // Those locals will contain the resulting tuple after the merge operation
        let result_locals: BTreeMap<_, Vec<_>> = result_ty
            .and_then(|expr_ty| match expr_ty {
                TypeSignature::TupleType(tuple) => Ok(tuple),
                _ => Err(GeneratorError::TypeError("expected tuple type".to_string())),
            })
            .map(|tuple| tuple.get_type_map().clone())?
            .into_iter()
            .map(|(name, ty_)| {
                (
                    name,
                    clar2wasm_ty(&ty_)
                        .into_iter()
                        .map(|local_ty| generator.alloc_local(local_ty))
                        .collect(),
                )
            })
            .collect();

        // Traverse the LHS tuple argument, leaving it on top of the stack.
        generator.traverse_expr(builder, &args[0])?;

        if generator.contract_analysis.epoch >= StacksEpochId::Epoch2_05 {
            generator.serialization_size(builder, &lhs_tuple_ty.clone().into())?;
            // STACK: [LHS, item_serialization_size]

            builder.local_set(*serialization_size);
            // STACK: [LHS]
        }

        // We will copy the values from LHS into the result locals iff the key is not
        // present in RHS. Otherwise, we drop the values.
        for (name, ty_) in lhs_tuple_ty.get_type_map().iter().rev() {
            if !rhs_tuple_ty.get_type_map().contains_key(name) {
                result_locals
                    .get(name)
                    .ok_or_else(|| {
                        GeneratorError::InternalError(
                            "merge result tuple should contain all the keys of LHS".to_owned(),
                        )
                    })?
                    .iter()
                    .rev()
                    .for_each(|local| {
                        builder.local_set(*local);
                    });
            } else {
                drop_value(builder, ty_);
            }
        }
        builder.drop();

        // Traverse the RHS tuple argument, leaving it on top of the stack.
        generator.traverse_expr(builder, &args[1])?;

        if generator.contract_analysis.epoch >= StacksEpochId::Epoch2_05 {
            generator.serialization_size(builder, &rhs_tuple_ty.clone().into())?;
            // STACK: [RHS, item_serialization_size]

            builder
                .local_get(*serialization_size)
                .binop(BinaryOp::I32Add)
                .local_set(*serialization_size);

            // STACK: [RHS]
            self.charge(generator, builder, *serialization_size)?;
            // STACK: [RHS]
        }

        // We will copy all values of RHS into the result locals
        for (name, _) in rhs_tuple_ty.get_type_map().iter().rev() {
            result_locals
                .get(name)
                .ok_or_else(|| {
                    GeneratorError::InternalError(
                        "merge result tuple should contain all the keys of RHS".to_owned(),
                    )
                })?
                .iter()
                .rev()
                .for_each(|local| {
                    builder.local_set(*local);
                });
        }
        builder.drop();

        // Now we load the result locals onto the stack
        builder.i32_const(0);
        result_locals.into_values().flatten().for_each(|local| {
            builder.local_get(local);
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clarity::vm::types::{PrincipalData, TupleData};
    use clarity::vm::{ClarityName, Value};

    use crate::tools::{crosscheck, crosscheck_cost_multi_contract, evaluate};

    #[test]
    fn test_get_optional() {
        let preamble = "
(define-read-only (get-optional-tuple (o (optional { a: int })))
  (get a o))";

        crosscheck(
            &format!("{preamble} (get-optional-tuple none)"),
            Ok(Some(Value::none())),
        );

        crosscheck(
            &format!("{preamble} (get-optional-tuple (some {{ a: 3 }} ))"),
            Ok(Some(Value::some(Value::Int(3)).unwrap())),
        );
    }

    #[test]
    fn merge_same_key_different_type() {
        let snippet = r#"(merge {a: 42} {a: "Hello, World!"})"#;

        let expected = Value::from(
            clarity::vm::types::TupleData::from_data(vec![(
                clarity::vm::ClarityName::from_literal("a"),
                Value::Sequence(clarity::vm::types::SequenceData::String(
                    clarity::vm::types::CharType::ASCII(clarity::vm::types::ASCIIData {
                        data: "Hello, World!".bytes().collect(),
                    }),
                )),
            )])
            .unwrap(),
        );

        crosscheck(snippet, Ok(Some(expected)));
    }

    #[test]
    fn merge_multiple_same_key_different_type() {
        let snippet =
            r#"(merge {a: 42, b: 0x24, c: 0xdeadbeef} {a: "Hello, World!", b: u789, d: 123})"#;

        let expected = Value::from(
            clarity::vm::types::TupleData::from_data(vec![
                (
                    clarity::vm::ClarityName::from_literal("a"),
                    Value::Sequence(clarity::vm::types::SequenceData::String(
                        clarity::vm::types::CharType::ASCII(clarity::vm::types::ASCIIData {
                            data: "Hello, World!".bytes().collect(),
                        }),
                    )),
                ),
                (
                    clarity::vm::ClarityName::from_literal("b"),
                    Value::UInt(789),
                ),
                (
                    clarity::vm::ClarityName::from_literal("c"),
                    Value::Sequence(clarity::vm::types::SequenceData::Buffer(
                        clarity::vm::types::BuffData {
                            data: vec![0xde, 0xad, 0xbe, 0xef],
                        },
                    )),
                ),
                (clarity::vm::ClarityName::from_literal("d"), Value::Int(123)),
            ])
            .unwrap(),
        );

        crosscheck(snippet, Ok(Some(expected)));
    }

    #[test]
    fn tuple_check_evaluation_order() {
        let snippet = r#"
        (define-data-var foo int 1)
        {
            b: (var-set foo 2),
            a: (var-get foo)
        }
    "#;

        let expected = Value::from(
            TupleData::from_data(vec![
                (ClarityName::from_literal("b"), Value::Bool(true)),
                (ClarityName::from_literal("a"), Value::Int(2)),
            ])
            .unwrap(),
        );

        crosscheck(snippet, Ok(Some(expected)));
    }

    //
    // Module with tests that should only be executed
    // when running Clarity::V2 or Clarity::v3.
    //
    #[cfg(not(feature = "test-clarity-v1"))]
    #[cfg(test)]
    mod clarity_v2_v3 {
        use super::*;

        #[test]
        fn merge_real_example() {
            let snippet = r#"
    (define-read-only (read-buff-1 (cursor { bytes: (buff 8192), pos: uint }))
        (ok {
            value: (unwrap! (as-max-len? (unwrap! (slice? (get bytes cursor) (get pos cursor) (+ (get pos cursor) u1)) (err u1)) u1) (err u1)),
            next: { bytes: (get bytes cursor), pos: (+ (get pos cursor) u1) }
        }))

    (define-read-only (read-uint-8 (cursor { bytes: (buff 8192), pos: uint }))
        (let ((cursor-bytes (try! (read-buff-1 cursor))))
            (ok (merge cursor-bytes { value: (buff-to-uint-be (get value cursor-bytes)) }))))
                "#;

            crosscheck(snippet, Ok(None));
        }
    }

    #[test]
    fn tuple_less_than_one_arg() {
        let result = evaluate("(tuple)");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting >= 1 arguments, got 0"));
    }

    #[test]
    fn get_less_than_two_args() {
        let result = evaluate("(get id)");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 2 arguments, got 1"));
    }

    #[test]
    fn get_more_than_two_args() {
        let result = evaluate("(get id 2 3)");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 2 arguments, got 3"));
    }

    #[test]
    fn merge_less_than_two_args() {
        let result = evaluate("(merge)");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 2 arguments, got 0"));
    }

    #[test]
    fn merge_more_than_two_args() {
        let result = evaluate("(merge {a: 1} {b: 2} {c: 3})");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 2 arguments, got 3"));
    }

    /// A bound trait keeps its callable value shape when a tuple field is
    /// contextually widened to `principal`. The pool vault prints this exact
    /// event shape; losing the callable shape crosses a print-cost bucket.
    #[test]
    fn a_trait_inside_a_printed_tuple_keeps_its_value_shape() {
        let trait_definition = "(define-trait ft ())";
        let token = "(impl-trait .trait-definition.ft)";
        let caller = "(use-trait ft .trait-definition.ft)
                      (define-public (f (amount uint) (recipient principal) (asset <ft>))
                        (ok (print
                          { type: \"transfer-pool-vault\",
                            payload: { amount: amount,
                                       recipient: recipient,
                                       asset: asset } })))";
        let principal = |name| {
            Value::Principal(
                PrincipalData::parse_qualified_contract_principal(&format!(
                    "S1G2081040G2081040G2081040G208105NK8PE5.{name}"
                ))
                .expect("contract principal"),
            )
        };
        crosscheck_cost_multi_contract(
            &[
                ("trait-definition", trait_definition),
                ("token", token),
                ("caller", caller),
            ],
            "f",
            &[
                Value::UInt(1),
                Value::Principal(PrincipalData::Standard(
                    clarity::vm::types::StandardPrincipalData::transient(),
                )),
                principal("token"),
            ],
        );
    }
}
