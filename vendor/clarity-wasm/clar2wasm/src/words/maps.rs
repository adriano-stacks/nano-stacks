use std::collections::BTreeMap;

use clarity::vm::types::{TypeSignature, TypeSignatureExt};
use clarity::vm::{ClarityName, SymbolicExpression};
use walrus::ir::{BinaryOp, IfElse};
use walrus::ValType;

use super::{ComplexWord, Word};
use crate::check_args;
use crate::cost::{charge_ok_or_throw_runtime_error, ChargeGenerator, WordCharge};
use crate::error_mapping::ErrorMap;
use crate::wasm_generator::{
    clar2wasm_ty, uses_packed_value, ArgumentsExt, GeneratorError, LiteralMemoryEntry,
    WasmGenerator,
};
use crate::wasm_utils::ArgumentCountCheck;

#[derive(Debug)]
pub struct MapDefinition;

impl Word for MapDefinition {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("define-map")
    }
}

impl ComplexWord for MapDefinition {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 3, args.len(), ArgumentCountCheck::Exact);

        let name = args.get_name(0)?;
        // Making sure if name is not reserved
        if generator.is_reserved_name(name) {
            return Err(GeneratorError::InternalError(format!(
                "Name already used {name:?}"
            )));
        }

        let key_type_repr = args.get_expr(1)?;
        let value_type_repr = args.get_expr(2)?;
        generator.charge_type_parse(builder, key_type_repr)?;
        generator.charge_type_parse(builder, value_type_repr)?;
        let key_type = TypeSignature::parse_type_repr(
            generator.contract_analysis.epoch,
            key_type_repr,
            &mut (),
        )
        .map_err(|e| GeneratorError::TypeError(format!("invalid type for map key: {e}")))?;
        let value_type = TypeSignature::parse_type_repr(
            generator.contract_analysis.epoch,
            value_type_repr,
            &mut (),
        )
        .map_err(|e| GeneratorError::TypeError(format!("invalid type for map value: {e}")))?;

        // Store the identifier as a string literal in the memory
        let (name_offset, name_length) = generator.add_string_literal(name)?;

        // Push the name onto the data stack
        builder
            .i32_const(name_offset as i32)
            .i32_const(name_length as i32);

        builder.call(generator.func_by_name("stdlib.define_map"));

        // Add the map types to generator
        generator
            .maps_types
            .insert(name.clone(), (key_type, value_type));

        Ok(())
    }
}

#[derive(Debug)]
pub struct MapGet;

impl Word for MapGet {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("map-get?")
    }
}

impl ComplexWord for MapGet {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 2, args.len(), ArgumentCountCheck::Exact);

        let name = args.get_name(0)?;
        let key = args.get_expr(1)?;

        let (key_ty, value_type) = generator
            .maps_types
            .get(name)
            .ok_or_else(|| {
                GeneratorError::TypeError("Type should have been set in map creation".to_owned())
            })?
            .clone();

        // Get the offset and length for this identifier in the literal memory
        let id_offset = *generator
            .literal_memory_offset
            .get(&LiteralMemoryEntry::Ascii(name.as_str().into()))
            .ok_or_else(|| GeneratorError::InternalError(format!("map not found: {name}")))?;
        let id_length = name.len();

        // Push the identifier offset and length onto the data stack
        builder
            .i32_const(id_offset as i32)
            .i32_const(id_length as i32);

        let (key_offset, _) = generator.create_call_stack_local(builder, &key_ty, true, false);

        // In epoch >= 2.05, the host reports the serialized bytes that the
        // database actually read. This distinguishes an absent entry from a
        // persisted one-byte deletion marker, even though both answer `none`.
        // In epoch < 2.05, the charge is immediately computed like it is in the interpreter.
        let post205_cost_local = if generator.charges_serialized_sizes() {
            Some(generator.borrow_local(ValType::I32))
        } else {
            let (key_ty, value_ty) = get_original_types(&generator.map_types_original, name)?;
            charge_ok_or_throw_runtime_error(
                &value_ty.size().and_then(|a| key_ty.size().map(|b| a + b)),
                generator,
                builder,
                self,
            )?;
            None
        };

        // Push the key to the data stack
        generator.set_expr_type(key, key_ty.clone())?;
        // The key is read where it is and serialised, not copied out of
        // its binding, so a bound name here does not pay to be copied.
        generator.traverse_expr_as_borrowed_value(builder, key)?;
        // Write the key to the memory (it's already on the data stack)
        let key_size = generator.write_to_memory(builder, key_offset, 0, &key_ty)?;

        // Push the key offset and size to the data stack
        builder.local_get(key_offset).i32_const(key_size as i32);

        let return_type = TypeSignature::OptionalType(Box::new(value_type.clone()));
        let (return_offset, size) =
            generator.create_call_stack_local(builder, &return_type, true, true);

        let return_size = generator.alloc_local(ValType::I32);
        builder.i32_const(size).local_set(return_size);

        // Push the return value offset and size to the data stack
        builder.local_get(return_offset).local_get(return_size);

        // Call the host-interface function, `map_get`
        builder.call(generator.func_by_name("stdlib.map_get"));
        if let Some(cost_local) = &post205_cost_local {
            builder.local_set(**cost_local);
        } else {
            builder.drop();
        }

        // Host interface fills the result into the specified memory. Read it
        // back out, and place the value on the data stack.
        generator.read_from_memory(builder, return_offset, 0, &return_type)?;

        let ty = clar2wasm_ty(&return_type);
        let packed_return = uses_packed_value(&return_type);
        let return_locals =
            packed_return.then(|| generator.save_to_locals(builder, &return_type, true));
        let block_ty = if packed_return {
            generator.lowered_control_type(&ty, &ty)
        } else {
            generator.bounded_control_type(&ty, &ty)?
        };
        // In > 2.05 we have three different costs depending if
        //      - an error occurred in the interpreter
        //      - no error occurred
        //          - and the value the operation is performed on is found
        //          - and the value the operation is performed on is not found
        let success_block_id = {
            // When the linked operation does not fail due to an interpreter error
            let mut success_block = builder.dangling_instr_seq(block_ty);
            if let Some(cost_local) = &post205_cost_local {
                self.charge(generator, &mut success_block, **cost_local)?;
            }
            success_block.id()
        };

        let error_block_id = {
            // When the linked operation fails due to an interpreter error
            let mut error_block = builder.dangling_instr_seq(None);
            if post205_cost_local.is_some() {
                let (key_ty, value_ty) = get_original_types(&generator.map_types_original, name)?;
                charge_ok_or_throw_runtime_error(
                    &value_ty.size().and_then(|a| key_ty.size().map(|b| a + b)),
                    generator,
                    &mut error_block,
                    self,
                )?;
            }

            // Throws back the runtime error that occurred in the interpreter after charging the cost
            error_block
                .i32_const(ErrorMap::ExternError as i32)
                .call(generator.func_by_name("stdlib.runtime-error"));
            error_block.id()
        };

        builder
            .global_get(generator.linked_error)
            .ref_is_null()
            .instr(IfElse {
                consequent: success_block_id,
                alternative: error_block_id,
            });
        if let Some(return_locals) = return_locals {
            for local in &return_locals {
                builder.local_get(*local);
            }
            generator.release_locals(return_locals);
        }

        Ok(())
    }
}

#[derive(Debug)]
pub struct MapSet;

impl Word for MapSet {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("map-set")
    }
}

impl ComplexWord for MapSet {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 3, args.len(), ArgumentCountCheck::Exact);

        let name = args.get_name(0)?;
        let key = args.get_expr(1)?;
        let value = args.get_expr(2)?;

        let (key_ty, value_type) = generator
            .maps_types
            .get(name)
            .ok_or_else(|| {
                GeneratorError::TypeError("Types should have been set in map creation".to_owned())
            })?
            .clone();

        // Get the offset and length for this identifier in the literal memory
        let id_offset = *generator
            .literal_memory_offset
            .get(&LiteralMemoryEntry::Ascii(name.as_str().into()))
            .ok_or_else(|| GeneratorError::InternalError(format!("map not found: {name}")))?;
        let id_length = name.len();

        // Push the identifier offset and length onto the data stack
        builder
            .i32_const(id_offset as i32)
            .i32_const(id_length as i32);

        let (key_offset, _) = generator.create_call_stack_local(builder, &key_ty, true, false);

        // In epoch >= 2.05, we generate a local to compute intermediary results used in the
        // cost tracking. In this case, the cost tracking charge is applied after the delete operation.
        // In epoch < 2.05, the charge is immediately computed like it is in the interpreter.
        let post205_cost_local = if generator.charges_serialized_sizes() {
            Some(generator.borrow_local(ValType::I32))
        } else {
            let (key_ty, value_ty) = get_original_types(&generator.map_types_original, name)?;
            charge_ok_or_throw_runtime_error(
                &value_ty.size().and_then(|a| key_ty.size().map(|b| a + b)),
                generator,
                builder,
                self,
            )?;
            None
        };

        // Push the key to the data stack
        generator.set_expr_type(key, key_ty.clone())?;
        generator.traverse_expr(builder, key)?;

        // Write the key to the memory (it's already on the data stack)
        let key_size = generator.write_to_memory(builder, key_offset, 0, &key_ty)?;

        // Push the key offset and size to the data stack
        builder.local_get(key_offset).i32_const(key_size as i32);

        // Create space on the call stack to write the value
        let (val_offset, _) = generator.create_call_stack_local(builder, &value_type, true, false);

        // Push the value to the data stack
        generator.set_expr_type(value, value_type.clone())?;
        generator.traverse_expr(builder, value)?;
        // Write the value to the memory (it's already on the data stack)
        let val_size = generator.write_to_memory(builder, val_offset, 0, &value_type)?;

        // Push the value offset and size to the data stack
        builder.local_get(val_offset).i32_const(val_size as i32);

        // Call the host interface function, `map_set`
        builder.call(generator.func_by_name("stdlib.map_set"));
        if let Some(cost_local) = &post205_cost_local {
            builder.local_set(**cost_local);
        } else {
            builder.drop();
        }

        // In > 2.05 we have two different costs depending if
        //      - an error occurred in the interpreter
        //      - no error occurred
        let success_block_id = {
            // When the linked operation does not fail due to an interpreter error
            let mut success_block = builder.dangling_instr_seq(None);
            if let Some(cost_local) = &post205_cost_local {
                self.charge(generator, &mut success_block, **cost_local)?;
            }
            success_block.id()
        };

        let error_block_id = {
            // When the linked operation fails due to an interpreter error
            let mut error_block = builder.dangling_instr_seq(None);

            if post205_cost_local.is_some() {
                // The cost in < 2.05 has already been handled before
                let (key_ty, value_ty) = get_original_types(&generator.map_types_original, name)?;
                charge_ok_or_throw_runtime_error(
                    &value_ty.size().and_then(|a| key_ty.size().map(|b| a + b)),
                    generator,
                    &mut error_block,
                    self,
                )?;
            }

            // Throws back the runtime error that occurred in the interpreter after charging the cost
            error_block
                .i32_const(ErrorMap::ExternError as i32)
                .call(generator.func_by_name("stdlib.runtime-error"));

            error_block.id()
        };

        builder
            .global_get(generator.linked_error)
            .ref_is_null()
            .instr(IfElse {
                consequent: success_block_id,
                alternative: error_block_id,
            });

        Ok(())
    }
}

#[derive(Debug)]
pub struct MapInsert;

impl Word for MapInsert {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("map-insert")
    }
}

impl ComplexWord for MapInsert {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 3, args.len(), ArgumentCountCheck::Exact);

        let name = args.get_name(0)?;
        let key = args.get_expr(1)?;
        let value = args.get_expr(2)?;

        let (key_ty, value_type) = generator
            .maps_types
            .get(name)
            .ok_or_else(|| {
                GeneratorError::TypeError("Types should have been set in map creation".to_owned())
            })?
            .clone();

        // Get the offset and length for this identifier in the literal memory
        let id_offset = *generator
            .literal_memory_offset
            .get(&LiteralMemoryEntry::Ascii(name.as_str().into()))
            .ok_or_else(|| GeneratorError::InternalError(format!("map not found: {name}")))?;
        let id_length = name.len();

        // Push the identifier offset and length onto the data stack
        builder
            .i32_const(id_offset as i32)
            .i32_const(id_length as i32);

        let (key_offset, _) = generator.create_call_stack_local(builder, &key_ty, true, false);

        // In epoch >= 2.05, we generate a local to compute intermediary results used in the
        // cost tracking. In this case, the cost tracking charge is applied after the delete operation.
        // In epoch < 2.05, the charge is immediately computed like it is in the interpreter.
        let post205_cost_local = if generator.charges_serialized_sizes() {
            Some(generator.borrow_local(ValType::I32))
        } else {
            let (key_ty, value_ty) = get_original_types(&generator.map_types_original, name)?;
            charge_ok_or_throw_runtime_error(
                &value_ty.size().and_then(|a| key_ty.size().map(|b| a + b)),
                generator,
                builder,
                self,
            )?;
            None
        };

        // Push the key to the data stack
        generator.set_expr_type(key, key_ty.clone())?;
        generator.traverse_expr(builder, key)?;

        // Write the key to the memory (it's already on the data stack)
        let key_size = generator.write_to_memory(builder, key_offset, 0, &key_ty)?;

        // Push the key offset and size to the data stack
        builder.local_get(key_offset).i32_const(key_size as i32);

        // Create space on the call stack to write the value
        let (val_offset, _) = generator.create_call_stack_local(builder, &value_type, true, false);

        // Push the value to the data stack
        generator.set_expr_type(value, value_type.clone())?;
        generator.traverse_expr(builder, value)?;
        // Write the value to the memory (it's already on the data stack)
        let val_size = generator.write_to_memory(builder, val_offset, 0, &value_type)?;

        // Push the value offset and size to the data stack
        builder.local_get(val_offset).i32_const(val_size as i32);

        // Call the host interface function, `map_insert`
        builder.call(generator.func_by_name("stdlib.map_insert"));
        if let Some(cost_local) = &post205_cost_local {
            builder.local_set(**cost_local);
        } else {
            builder.drop();
        }

        let block_ty = generator.bounded_control_type(&[ValType::I32], &[ValType::I32])?;

        // In > 2.05 we have three different costs depending if
        //      - an error occurred in the interpreter
        //      - no error occurred
        //          - and the value the operation is performed on is found
        //          - and the value the operation is performed on is not found
        let success_block_id = {
            // When the linked operation does not fail due to an interpreter error
            let mut success_block = builder.dangling_instr_seq(block_ty);
            // The cost in < 2.05 has already been handled before
            if let Some(cost_local) = &post205_cost_local {
                let entry_status = generator.borrow_local(ValType::I32);
                success_block.local_set(*entry_status);
                self.charge(generator, &mut success_block, **cost_local)?;
                success_block.local_get(*entry_status);
            }
            success_block.id()
        };

        let error_block_id = {
            // When the linked operation fails due to an interpreter error
            let mut error_block = builder.dangling_instr_seq(None);
            if post205_cost_local.is_some() {
                let (key_ty, value_ty) = get_original_types(&generator.map_types_original, name)?;
                charge_ok_or_throw_runtime_error(
                    &value_ty.size().and_then(|a| key_ty.size().map(|b| a + b)),
                    generator,
                    &mut error_block,
                    self,
                )?;
            }

            // Throws back the runtime error that occurred in the interpreter after charging the cost
            error_block
                .i32_const(ErrorMap::ExternError as i32)
                .call(generator.func_by_name("stdlib.runtime-error"));

            error_block.id()
        };

        builder
            .global_get(generator.linked_error)
            .ref_is_null()
            .instr(IfElse {
                consequent: success_block_id,
                alternative: error_block_id,
            });

        Ok(())
    }
}

#[derive(Debug)]
pub struct MapDelete;

impl Word for MapDelete {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("map-delete")
    }
}

impl ComplexWord for MapDelete {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 2, args.len(), ArgumentCountCheck::Exact);

        let name = args.get_name(0)?;
        let key = args.get_expr(1)?;

        let (key_ty, _) = generator
            .maps_types
            .get(name)
            .ok_or_else(|| {
                GeneratorError::TypeError("Types should have been set in map creation".to_owned())
            })?
            .clone();

        // In epoch >= 2.05, we generate a local to compute intermediary results used in the
        // cost tracking. In this case, the cost tracking charge is applied after the delete operation.
        // In epoch < 2.05, the charge is immediately computed like it is in the interpreter.
        let post205_cost_local = if generator.charges_serialized_sizes() {
            Some(generator.borrow_local(ValType::I32))
        } else {
            let (key_ty, _) = get_original_types(&generator.map_types_original, name)?;
            charge_ok_or_throw_runtime_error(&key_ty.size(), generator, builder, self)?;
            None
        };

        // Get the offset and length for this identifier in the literal memory
        let id_offset = *generator
            .literal_memory_offset
            .get(&LiteralMemoryEntry::Ascii(name.as_str().into()))
            .ok_or_else(|| GeneratorError::InternalError(format!("map not found: {name}")))?;

        let id_length = name.len();

        // Push the identifier offset and length onto the data stack
        builder
            .i32_const(id_offset as i32)
            .i32_const(id_length as i32);

        // Create space on the call stack to write the key
        let (key_offset, _) = generator.create_call_stack_local(builder, &key_ty, true, false);

        // Push the key to the data stack
        generator.set_expr_type(key, key_ty.clone())?;
        generator.traverse_expr_as_borrowed_value(builder, key)?;

        // for epoch >= 2.05, we compute the serialization size of the key.
        if let Some(cost_local) = &post205_cost_local {
            generator.serialization_size(builder, &key_ty)?;
            builder.local_set(**cost_local);
        }

        // Write the key to the memory (it's already on the data stack)
        let key_size = generator.write_to_memory(builder, key_offset, 0, &key_ty)?;

        // Push the key offset and size to the data stack
        builder.local_get(key_offset).i32_const(key_size as i32);

        // Call the host interface function, `map_delete`
        builder.call(generator.func_by_name("stdlib.map_delete"));

        let result = generator.borrow_local(ValType::I32);
        builder.local_set(*result);

        // In > 2.05 we have three different costs depending if
        //      - an error occurred in the interpreter
        //      - no error occurred
        //          - and the value the operation is performed on is found
        //          - and the value the operation is performed on is not found
        let success_block_id = {
            // When the linked operation does not fail due to an interpreter error
            let mut success_block = builder.dangling_instr_seq(None);

            if let Some(cost_local) = &post205_cost_local {
                // the cost here will be the serialization size of the key (already in cost_local)
                //  + the size of a None if the operation succeeds. Fortunately, this size is 1 when
                // a value is found, which is the same as the value inside result. If no value was
                // deleted, we add 0, which is the value of result.
                success_block
                    .local_get(**cost_local)
                    .local_get(*result)
                    .binop(BinaryOp::I32Add)
                    .local_set(**cost_local);
                self.charge(generator, &mut success_block, **cost_local)?;
            }

            success_block.id()
        };

        let error_block_id = {
            // When the linked operation fails due to an interpreter error
            let mut error_block = builder.dangling_instr_seq(None);

            // in epoch >= 2.05, we charge depending on the size of the key.
            if post205_cost_local.is_some() {
                let (key_ty, _) = get_original_types(&generator.map_types_original, name)?;
                charge_ok_or_throw_runtime_error(
                    &key_ty.size(),
                    generator,
                    &mut error_block,
                    self,
                )?;
            }

            // Throws back the runtime error that occurred in the interpreter after charging the cost
            error_block
                .i32_const(ErrorMap::ExternError as i32)
                .call(generator.func_by_name("stdlib.runtime-error"));

            error_block.id()
        };

        builder
            .global_get(generator.linked_error)
            .ref_is_null()
            .instr(IfElse {
                consequent: success_block_id,
                alternative: error_block_id,
            });

        builder.local_get(*result);

        Ok(())
    }
}

type MapTypes = BTreeMap<ClarityName, (TypeSignature, TypeSignature)>;
fn get_original_types(
    map_types: &MapTypes,
    name: &str,
) -> Result<(TypeSignature, TypeSignature), GeneratorError> {
    map_types
        .get(name)
        .ok_or_else(|| {
            GeneratorError::TypeError("Types should have been set in contract analysis".to_owned())
        })
        .map(|(t1, t2)| (t1.clone(), t2.clone()))
}

#[cfg(test)]
mod tests {
    // use clarity::vm::errors::{CheckErrors, Error};

    use clarity::vm::errors::{RuntimeCheckErrorKind, VmExecutionError};
    use clarity::vm::types::{PrincipalData, TupleData};
    use clarity::vm::{ClarityName, Value};

    use crate::tools::{
        crosscheck, crosscheck_cost, crosscheck_cost_multi_contract, crosscheck_expect_failure,
        evaluate,
    };

    //
    // Module with tests that should only be executed
    // when running Clarity::V1.
    //
    #[cfg(feature = "test-clarity-v1")]
    mod clarity_v1 {
        use clarity::types::StacksEpochId;

        use crate::tools::crosscheck_with_epoch;

        #[test]
        fn validate_define_map_epoch() {
            // Epoch
            crosscheck_with_epoch(
                "(define-map index-of? {x: int} {square: int})",
                Ok(None),
                StacksEpochId::Epoch20,
            );
        }
    }

    #[test]
    fn map_define_get() {
        crosscheck(
            r#"(define-map counters principal uint) (map-get? counters tx-sender)"#,
            Ok(Some(Value::none())),
        )
    }

    #[test]
    fn map_define_set() {
        crosscheck("(define-map approved-contracts principal bool) (map-set approved-contracts tx-sender true)", Ok(Some(Value::Bool(true))));
    }

    #[test]
    fn map_define_insert() {
        crosscheck("(define-map approved-contracts principal bool) (map-insert approved-contracts tx-sender true)", Ok(Some(Value::Bool(true))));
    }

    #[test]
    fn map_define_set_delete() {
        crosscheck("(define-map approved-contracts principal bool) (map-insert approved-contracts tx-sender true) (map-delete approved-contracts tx-sender)", Ok(Some(Value::Bool(true))));
    }

    #[test]
    fn map_define_set_get() {
        crosscheck("(define-map approved-contracts principal bool) (map-insert approved-contracts tx-sender true) (map-get? approved-contracts tx-sender)", Ok(Some(Value::some(Value::Bool(true)).unwrap())));
    }

    #[test]
    fn validate_define_map() {
        // Reserved keyword
        crosscheck_expect_failure("(define-map map {x: int} {square: int})");

        // Custom map name
        crosscheck("(define-map a {x: int} {square: int})", Ok(None));

        // Custom map name duplicate
        crosscheck_expect_failure(
            "(define-map a {x: int} {square: int}) (define-map a {x: int} {square: int})",
        );
    }

    #[test]
    fn define_map_less_than_three_args() {
        let result = evaluate("(define-map some-map)");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 3 arguments, got 1"));
    }

    #[test]
    fn define_map_more_than_three_args() {
        let result = evaluate("(define-map some-map int 5 6)");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 3 arguments, got 4"));
    }

    #[test]
    fn map_get_less_than_two_args() {
        let result = evaluate("(map-get? some-map)");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 2 arguments, got 1"));
    }

    #[test]
    fn map_set_less_than_two_args() {
        let result = evaluate("(map-set some-map)");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting >= 3 arguments, got 1"));
    }

    #[test]
    fn map_insert_less_than_two_args() {
        let result = evaluate("(map-insert some-map)");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting >= 3 arguments, got 1"));
    }

    #[test]
    fn map_delete_less_than_two_args() {
        let snippet = "
        (define-map some-map int {x: int})
        (map-insert some-map 21 {x: 21})
        (map-delete some-map)";
        let result = evaluate(snippet);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting >= 2 arguments, got 1"));
    }

    #[test]
    fn map_get_more_than_two_args() {
        let snippet = "
        (define-map some-map int {x: int})
        (map-insert some-map 21 {x: 21})
        (map-get? some-map 21 21)";
        let result = evaluate(snippet);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 2 arguments, got 3"));
    }

    #[test]
    fn map_set_more_than_two_args() {
        // TODO: see issue #488
        // The inconsistency in function arguments should have been caught by the typechecker.
        // The runtime error below is being used as a workaround for a typechecker issue
        // where certain errors are not properly handled.
        // This test should be re-worked once the typechecker is fixed
        // and can correctly detect all argument inconsistencies.
        let snippet = "(define-map some-map int {x: int})
        (map-set some-map 21 {x: 21} {x: 21})";
        let expected = Err(VmExecutionError::RuntimeCheck(
            RuntimeCheckErrorKind::IncorrectArgumentCount(3, 4),
        ));
        crosscheck(snippet, expected);
    }

    #[test]
    fn map_insert_more_than_three_args() {
        // TODO: see issue #488
        // The inconsistency in function arguments should have been caught by the typechecker.
        // The runtime error below is being used as a workaround for a typechecker issue
        // where certain errors are not properly handled.
        // This test should be re-worked once the typechecker is fixed
        // and can correctly detect all argument inconsistencies.
        let snippet = "
        (define-map some-map int {x: int})
        (map-insert some-map 21 {x: 21} {x: 21})";
        let expected = Err(VmExecutionError::RuntimeCheck(
            RuntimeCheckErrorKind::IncorrectArgumentCount(3, 4),
        ));
        crosscheck(snippet, expected);
    }

    #[test]
    fn map_delete_more_than_two_args() {
        // TODO: see issue #488
        // The inconsistency in function arguments should have been caught by the typechecker.
        // The runtime error below is being used as a workaround for a typechecker issue
        // where certain errors are not properly handled.
        // This test should be re-worked once the typechecker is fixed
        // and can correctly detect all argument inconsistencies.
        let snippet = "
        (define-map some-map int {x: int})
        (map-insert some-map 21 {x: 21})
        (map-delete some-map 21 21)";
        let expected = Err(VmExecutionError::RuntimeCheck(
            RuntimeCheckErrorKind::IncorrectArgumentCount(2, 3),
        ));
        crosscheck(snippet, expected);
    }

    /// A written map entry is charged for the exact bytes the database
    /// stored: the serialized key plus the persisted `(some value)` envelope.
    #[test]
    fn a_written_entry_charges_the_bytes_the_database_wrote() {
        let snippet = "(define-map entries uint { amount: uint, note: (buff 32) })
             (define-public (mutate (key uint) (amount uint) (note (buff 32)) (replace bool))
               (ok (if replace
                     (map-set entries key { amount: amount, note: note })
                     (map-insert entries key { amount: amount, note: note }))))";
        for replace in [false, true] {
            crosscheck_cost(
                snippet,
                "mutate",
                &[
                    Value::UInt(0),
                    Value::UInt(1),
                    Value::buff_from(vec![]).expect("empty note"),
                    Value::Bool(replace),
                ],
            );
        }
    }

    /// A deleted map entry is charged for its persisted one-byte tombstone as
    /// well as its serialized key. A truly absent entry has no tombstone.
    #[test]
    fn a_deleted_entry_charges_the_bytes_the_database_read() {
        let user = Value::Principal(
            PrincipalData::parse("SP2WA4AAQKK4K1FJNEMZB01FHXTZNF8EWEXPX5VC0")
                .expect("user principal"),
        );
        let asset = Value::Principal(
            PrincipalData::parse_qualified_contract_principal(
                "SM3VDXK3WZZSA84XXFKAFAF15NNZX32CTSG82JFQ4.sbtc-token",
            )
            .expect("asset contract principal"),
        );
        let key = Value::Tuple(
            TupleData::from_data(vec![
                (ClarityName::from_literal("asset"), asset.clone()),
                (ClarityName::from_literal("user"), user.clone()),
            ])
            .expect("map key"),
        );
        assert_eq!(key.serialize_to_vec().expect("serialized key").len(), 71);
        crosscheck_cost_multi_contract(
            &[
                (
                    "data",
                    "(define-map m { user: principal, asset: principal } uint)
                     (define-public (read (user principal) (asset principal))
                       (begin
                         (map-set m { user: user, asset: asset } u1)
                         (map-delete m { user: user, asset: asset })
                         (ok (map-get? m { user: user, asset: asset }))))",
                ),
                (
                    "snippet",
                    "(define-public (f (user principal) (asset principal))
                       (contract-call? .data read user asset))",
                ),
            ],
            "f",
            &[user, asset],
        );
    }
}

#[cfg(test)]
mod recorded_semantics_charges {
    use clarity::types::StacksEpochId;
    use clarity::vm::ClarityVersion;

    use crate::tools::{crosscheck_cost, crosscheck_cost_recorded_semantics};

    #[test]
    fn an_old_contracts_unlist_shape_charges_current_epoch_style() {
        crosscheck_cost_recorded_semantics(
            &[
                (
                    "tradable-trait",
                    "(define-trait tradables-trait
                       ((get-owner (uint) (response (optional principal) uint))
                        (transfer (uint principal principal) (response bool uint))))",
                ),
                (
                    "bulls",
                    "(define-non-fungible-token bulls uint)
                     (define-public (setup (escrow principal))
                       (match (nft-mint? bulls u157 escrow) ok-mint (ok true) err-mint (err u1)))
                     (define-read-only (get-owner (id uint))
                       (ok (nft-get-owner? bulls id)))
                     (define-public (transfer (id uint) (sender principal) (recipient principal))
                       (if (is-eq tx-sender sender)
                           (match (nft-transfer? bulls id sender recipient)
                             success (ok success)
                             error (err u2))
                           (err u500)))",
                ),
                (
                    "market",
                    "(use-trait tradables-trait .tradable-trait.tradables-trait)
                     (define-map on-sale
                       {tradables: principal, tradable-id: uint}
                       {price: uint, commission: uint, owner: principal,
                        royalty-address: principal, royalty-percent: uint})
                     (define-public (seed)
                       (if (map-insert on-sale
                             {tradables: .bulls, tradable-id: u157}
                             {price: u25000000, commission: u200, owner: tx-sender,
                              royalty-address: tx-sender, royalty-percent: u500})
                           (match (contract-call? .bulls setup (as-contract tx-sender))
                             minted (ok true)
                             bad (err u3))
                           (err u4)))
                     (define-private (transfer-tradable-from-escrow
                                       (tradables <tradables-trait>) (tradable-id uint))
                       (let ((owner tx-sender))
                         (as-contract
                           (contract-call? tradables transfer tradable-id
                                           (as-contract tx-sender) owner))))
                     (define-public (unlist-asset (tradables <tradables-trait>) (tradable-id uint))
                       (match (map-get? on-sale
                                {tradables: (contract-of tradables), tradable-id: tradable-id})
                         nft-data
                         (if (is-eq (get owner nft-data) tx-sender)
                             (match (transfer-tradable-from-escrow tradables tradable-id)
                               success
                               (begin
                                 (map-delete on-sale
                                   {tradables: (contract-of tradables), tradable-id: tradable-id})
                                 (ok true))
                               error (begin (print error) (err u2)))
                             (err u3))
                         (err u5)))",
                ),
                (
                    "driver",
                    "(define-public (probe)
                       (begin
                         (try! (contract-call? .market seed))
                         (contract-call? .market unlist-asset .bulls u157)))",
                ),
            ],
            "probe",
            &[],
            StacksEpochId::Epoch20,
            ClarityVersion::Clarity1,
        );
    }

    /// A binding read that a borrowing special function consumes pays the
    /// reference no `LookupVariableSize`: the reference evaluates such an
    /// operand itself and reads it through `as_ref`, never cloning it.
    #[test]
    fn a_parameter_a_special_function_borrows_pays_no_copy() {
        crosscheck_cost(
            "(define-read-only (header (height uint))
               (get-burn-block-info? header-hash height))
             (define-public (poke (height uint))
               (ok (header height)))",
            "poke",
            &[clarity::vm::Value::UInt(1)],
        );
    }

    #[test]
    fn an_old_contracts_trait_dispatch_charges_like_the_reference() {
        crosscheck_cost_recorded_semantics(
            &[
                (
                    "tradable-trait",
                    "(define-trait tradables-trait
                       ((get-owner (uint) (response (optional principal) uint))))",
                ),
                (
                    "bulls",
                    "(define-non-fungible-token bulls uint)
                     (define-read-only (get-owner (id uint))
                       (ok (nft-get-owner? bulls id)))",
                ),
                (
                    "market",
                    "(use-trait tradables-trait .tradable-trait.tradables-trait)
                     (define-public (poke (tradables <tradables-trait>))
                       (match (contract-call? tradables get-owner u157)
                         owner (ok true)
                         bad (err u1)))",
                ),
                (
                    "driver",
                    "(define-public (probe)
                       (contract-call? .market poke .bulls))",
                ),
            ],
            "probe",
            &[],
            StacksEpochId::Epoch20,
            ClarityVersion::Clarity1,
        );
    }

    #[test]
    fn an_old_contracts_private_trait_argument_charges_like_the_reference() {
        crosscheck_cost_recorded_semantics(
            &[
                (
                    "tradable-trait",
                    "(define-trait tradables-trait
                       ((get-owner (uint) (response (optional principal) uint))))",
                ),
                (
                    "bulls",
                    "(define-non-fungible-token bulls uint)
                     (define-read-only (get-owner (id uint))
                       (ok (nft-get-owner? bulls id)))",
                ),
                (
                    "market",
                    "(use-trait tradables-trait .tradable-trait.tradables-trait)
                     (define-private (inner (tradables <tradables-trait>) (id uint))
                       (let ((owner tx-sender))
                         (as-contract
                           (contract-call? tradables get-owner id))))
                     (define-public (poke (tradables <tradables-trait>))
                       (match (inner tradables u157)
                         owner (ok true)
                         bad (err u1)))",
                ),
                (
                    "driver",
                    "(define-public (probe)
                       (contract-call? .market poke .bulls))",
                ),
            ],
            "probe",
            &[],
            StacksEpochId::Epoch20,
            ClarityVersion::Clarity1,
        );
    }

    #[test]
    fn a_trait_argument_pays_no_copy_and_sizes_as_its_value() {
        crosscheck_cost_recorded_semantics(
            &[
                (
                    "tradable-trait",
                    "(define-trait tradables-trait
                       ((get-owner (uint) (response (optional principal) uint))))",
                ),
                (
                    "bulls",
                    "(define-non-fungible-token bulls uint)
                     (define-read-only (get-owner (id uint))
                       (ok (nft-get-owner? bulls id)))",
                ),
                (
                    "market",
                    "(use-trait tradables-trait .tradable-trait.tradables-trait)
                     (define-private (inner (tradables <tradables-trait>) (id uint))
                       (contract-call? tradables get-owner id))
                     (define-public (poke (tradables <tradables-trait>))
                       (match (inner tradables u157)
                         owner (ok true)
                         bad (err u1)))",
                ),
                (
                    "driver",
                    "(define-public (probe)
                       (contract-call? .market poke .bulls))",
                ),
            ],
            "probe",
            &[],
            StacksEpochId::Epoch20,
            ClarityVersion::Clarity1,
        );
    }

    #[test]
    fn an_old_contracts_is_eq_charges_current_epoch_style() {
        crosscheck_cost_recorded_semantics(
            &[(
                "market",
                "(define-public (probe (a principal) (b principal))
                   (ok (is-eq a b)))",
            )],
            "probe",
            &[
                clarity::vm::Value::Principal(
                    clarity::vm::types::PrincipalData::parse(
                        "SPNWZ5V2TPWGQGVDR6T7B6RQ4XMGZ4PXTEE0VQ0S.marketplace-v4",
                    )
                    .expect("contract principal"),
                ),
                clarity::vm::Value::Principal(
                    clarity::vm::types::PrincipalData::parse(
                        "SPNWZ5V2TPWGQGVDR6T7B6RQ4XMGZ4PXTEE0VQ0S.marketplace-v4",
                    )
                    .expect("contract principal"),
                ),
            ],
            StacksEpochId::Epoch20,
            ClarityVersion::Clarity1,
        );
    }

    #[test]
    fn an_old_contracts_nft_transfer_charges_current_epoch_style() {
        crosscheck_cost_recorded_semantics(
            &[(
                "market",
                "(define-non-fungible-token bulls uint)
                 (define-public (probe (recipient principal))
                   (begin
                     (unwrap-panic (nft-mint? bulls u157 tx-sender))
                     (nft-transfer? bulls u157 tx-sender recipient)))",
            )],
            "probe",
            &[clarity::vm::Value::Principal(
                clarity::vm::types::PrincipalData::parse(
                    "SP2WA4AAQKK4K1FJNEMZB01FHXTZNF8EWEXPX5VC0",
                )
                .expect("recipient"),
            )],
            StacksEpochId::Epoch20,
            ClarityVersion::Clarity1,
        );
    }

    #[test]
    fn an_old_contracts_map_ops_charge_current_epoch_style() {
        crosscheck_cost_recorded_semantics(
            &[(
                "market",
                "(define-map verified-contracts
                   {tradables: principal}
                   {royalty-address: principal, royalty-percent: uint})
                 (define-public (seed (contract principal))
                   (if (map-set verified-contracts {tradables: contract}
                        {royalty-address: tx-sender, royalty-percent: u500})
                       (ok true) (err u1)))
                 (define-read-only (get-royalty-amount (contract principal))
                   (match (map-get? verified-contracts {tradables: contract})
                     royalty-data
                     (get royalty-percent royalty-data)
                     u0))
                 (define-public (probe (contract principal))
                   (begin
                     (try! (seed contract))
                     (ok (get-royalty-amount contract))))",
            )],
            "probe",
            &[clarity::vm::Value::Principal(
                clarity::vm::types::PrincipalData::parse(
                    "SP2KAF9RF86PVX3NEE27DFV1CQX0T4WGR41X3S45C.byzantion-bitcoin-bulls",
                )
                .expect("contract principal"),
            )],
            StacksEpochId::Epoch20,
            ClarityVersion::Clarity1,
        );
    }
}
