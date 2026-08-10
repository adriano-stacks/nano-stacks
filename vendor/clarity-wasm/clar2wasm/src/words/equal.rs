use clarity::types::StacksEpochId;
use clarity::vm::types::signatures::CallableSubtype;
use clarity::vm::types::{SequenceSubtype, TupleTypeSignature, TypeSignature};
use clarity::vm::{ClarityName, SymbolicExpression};
use walrus::ir::{BinaryOp, Block, IfElse, Loop, UnaryOp};
use walrus::{InstrSeqBuilder, LocalId, ValType};

use super::{ComplexWord, Word};
use crate::check_args;
use crate::cost::WordCharge;
use crate::layout::get_type_size;
use crate::wasm_generator::{
    clar2wasm_ty, drop_value, has_runtime_shape, GeneratorError, WasmGenerator,
};
use crate::wasm_utils::ArgumentCountCheck;

#[derive(Debug)]
pub struct IsEq;

impl Word for IsEq {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("is-eq")
    }
}

impl ComplexWord for IsEq {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        let args_len = args.len();
        let serialization_size_sum = generator.alloc_local(ValType::I32);
        builder.i32_const(0).local_set(serialization_size_sum);

        check_args!(generator, builder, 1, args_len, ArgumentCountCheck::AtLeast);
        if generator.contract_analysis.epoch < StacksEpochId::Epoch2_05 {
            self.charge(generator, builder, args_len as u32)?;
        }

        let operand_types = args
            .iter()
            .map(|arg| {
                generator.get_expr_type(arg).cloned().ok_or_else(|| {
                    GeneratorError::TypeError("Is-eq argument should be typed".to_owned())
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        // Match the analyzer's operand order. Tuple least-supertype is
        // asymmetric: the next operand is the left-hand side.
        let mut types = operand_types.iter();
        let mut unified_ty = types.next().cloned().ok_or_else(|| {
            GeneratorError::InternalError("is-eq must have at least one operand".to_owned())
        })?;
        for operand_ty in types {
            unified_ty = TypeSignature::least_supertype(
                &generator.contract_analysis.epoch,
                operand_ty,
                &unified_ty,
            )
            .map_err(|error| {
                GeneratorError::TypeError(format!("Incompatible types in is-eq: {error}"))
            })?;
        }

        // Tuple values with different field sets cannot compare equal. Keep
        // their original layouts, but still evaluate and charge every operand.
        // Narrowing both to the analyzer's common type would turn unequal
        // TupleData values into equal ones when their shared fields match.
        if operand_types
            .iter()
            .skip(1)
            .any(|ty| tuples_are_statically_distinct(&operand_types[0], ty))
        {
            for (operand, ty) in args.iter().zip(&operand_types) {
                generator.traverse_expr(builder, operand)?;
                if generator.contract_analysis.epoch >= StacksEpochId::Epoch2_05 {
                    generator.serialization_size(builder, ty)?;
                    builder
                        .local_get(serialization_size_sum)
                        .binop(BinaryOp::I32Add)
                        .local_set(serialization_size_sum);
                }
                drop_value(builder, ty);
            }
            if generator.contract_analysis.epoch >= StacksEpochId::Epoch2_05 {
                self.charge(generator, builder, serialization_size_sum)?;
            }
            builder.i32_const(0);
            return Ok(());
        }

        // Compatible operands use one representation for their value comparison.
        for a in args {
            generator.set_expr_type(a, unified_ty.clone())?;
        }

        let ty = unified_ty;

        for operand in args.iter() {
            generator.traverse_expr(builder, operand)?;
            // STACK: [operand]
            if generator.contract_analysis.epoch >= StacksEpochId::Epoch2_05 {
                generator.serialization_size(builder, &ty)?;
                // STACK: [operand, serialization_size]
                builder
                    .local_get(serialization_size_sum)
                    .binop(BinaryOp::I32Add)
                    .local_set(serialization_size_sum);
                // STACK: [operand]
            }
        }
        // STACK: [operand1, ..., operandN]

        if generator.contract_analysis.epoch >= StacksEpochId::Epoch2_05 {
            self.charge(generator, builder, serialization_size_sum)?;
        }

        // No need to go further if there is only one argument
        if args.len() == 1 {
            drop_value(builder, &ty);
            builder.i32_const(1); // TRUE
        } else {
            let equality_accumulator = generator.alloc_local(ValType::I32);
            // Initialize boolean result accumulator to TRUE
            builder.i32_const(1).local_set(equality_accumulator);

            let last_locals = generator.save_to_locals(builder, &ty, true);

            // Loop through n-1 remainder operands
            // n-1 as one operand is already stored in last_locals
            for _ in args.iter().skip(1) {
                let top_of_stack_locals = generator.save_to_locals(builder, &ty, true);

                wasm_equal(&ty, generator, builder, &last_locals, &top_of_stack_locals)?;

                // Do an "and" operation with the result from the previous function call
                // And store it in the equality accumulator
                builder
                    .local_get(equality_accumulator)
                    .binop(BinaryOp::I32And)
                    .local_set(equality_accumulator);
            }
            builder.local_get(equality_accumulator);
        }

        Ok(())
    }
}

fn tuples_are_statically_distinct(left: &TypeSignature, right: &TypeSignature) -> bool {
    let (TypeSignature::TupleType(left), TypeSignature::TupleType(right)) = (left, right) else {
        return false;
    };
    let left = left.get_type_map();
    let right = right.get_type_map();
    left.len() != right.len()
        || left.iter().any(|(name, left)| {
            right
                .get(name)
                .is_none_or(|right| tuples_are_statically_distinct(left, right))
        })
}

pub(super) fn wasm_equal(
    ty: &TypeSignature,
    generator: &mut WasmGenerator,
    builder: &mut InstrSeqBuilder,
    first_op: &[LocalId],
    nth_op: &[LocalId],
) -> Result<(), GeneratorError> {
    if has_runtime_shape(ty) {
        return wasm_equal_runtime_shape(ty, generator, builder, first_op, nth_op);
    }

    match ty {
        // we should never compare NoType
        TypeSignature::NoType => {
            builder.unreachable();
            Ok(())
        }
        TypeSignature::BoolType => {
            builder
                .local_get(first_op[0])
                .local_get(nth_op[0])
                .binop(BinaryOp::I32Eq);
            Ok(())
        }
        // is-eq-int function can be reused to both int and uint types.
        TypeSignature::IntType | TypeSignature::UIntType => {
            wasm_equal_int128(generator, builder, first_op, nth_op)
        }
        // is-eq-bytes function can be used for types with (offset, length)
        TypeSignature::SequenceType(SequenceSubtype::BufferType(_))
        | TypeSignature::SequenceType(SequenceSubtype::StringType(_)) => {
            wasm_equal_bytes(generator, builder, first_op, nth_op)
        }
        // A trait reference is a principal at run time -- `wasm_generator` says so
        // where it lowers one ("a public function receives a trait argument as a
        // bare principal") and `contract-of` is the read of it -- so comparing two
        // is comparing the contracts they name, byte for byte, exactly as the
        // `Principal` callable beside it already does.
        //
        // Only `Trait` was missing from this arm, and three contracts mainnet
        // deployed and accepted refuse to compile for it: `amm-swap003` and two
        // `.pool`s, each answering "Not implemented: equality over
        // CallableType(Trait(..))". See task 093.
        TypeSignature::PrincipalType
        | TypeSignature::CallableType(CallableSubtype::Principal(_) | CallableSubtype::Trait(_))
        | TypeSignature::ListUnionType(_) => wasm_equal_bytes(generator, builder, first_op, nth_op),
        TypeSignature::OptionalType(some_ty) => {
            wasm_equal_optional(generator, builder, first_op, nth_op, some_ty)
        }

        TypeSignature::ResponseType(ok_err_ty) => wasm_equal_response(
            generator,
            builder,
            first_op,
            nth_op,
            &ok_err_ty.0,
            &ok_err_ty.1,
        ),
        TypeSignature::TupleType(tuple_ty) => {
            wasm_equal_tuple(generator, builder, first_op, nth_op, tuple_ty)
        }

        TypeSignature::SequenceType(SequenceSubtype::ListType(list_ty)) => wasm_equal_list(
            generator,
            builder,
            first_op,
            nth_op,
            list_ty.get_list_item_type(),
        ),

        _ => Err(GeneratorError::NotImplemented(format!(
            "equality over {ty:?}"
        ))),
    }
}

fn wasm_equal_runtime_shape(
    ty: &TypeSignature,
    generator: &mut WasmGenerator,
    builder: &mut InstrSeqBuilder,
    first_op: &[LocalId],
    nth_op: &[LocalId],
) -> Result<(), GeneratorError> {
    let value_size = u32::try_from(get_type_size(ty)).map_err(|_| {
        GeneratorError::InternalError("runtime-shaped value has a negative size".to_owned())
    })?;
    let first_offset_value = generator.reserve_static_memory(value_size);
    let second_offset_value = generator.reserve_static_memory(value_size);
    let first_offset = generator.alloc_local(ValType::I32);
    let second_offset = generator.alloc_local(ValType::I32);

    builder
        .i32_const(first_offset_value as i32)
        .local_set(first_offset);
    for local in first_op {
        builder.local_get(*local);
    }
    generator.write_to_memory(builder, first_offset, 0, ty)?;

    builder
        .i32_const(second_offset_value as i32)
        .local_set(second_offset);
    for local in nth_op {
        builder.local_get(*local);
    }
    generator.write_to_memory(builder, second_offset, 0, ty)?;

    let (type_offset, type_length) = generator.serialized_type(ty)?;
    builder
        .i32_const(first_offset_value as i32)
        .i32_const(second_offset_value as i32)
        .i32_const(type_offset)
        .i32_const(type_length)
        .call(generator.func_by_name("stdlib.runtime_shape_is_equal"));
    Ok(())
}

fn wasm_equal_int128(
    _generator: &mut WasmGenerator,
    builder: &mut InstrSeqBuilder,
    first_op: &[LocalId],
    nth_op: &[LocalId],
) -> Result<(), GeneratorError> {
    let [a_lo, a_hi] = first_op else {
        return Err(GeneratorError::InternalError(
            "wrong representation of int for equality".to_owned(),
        ));
    };
    let [b_lo, b_hi] = nth_op else {
        return Err(GeneratorError::InternalError(
            "wrong representation of int for equality".to_owned(),
        ));
    };

    builder
        .local_get(*a_lo)
        .local_get(*b_lo)
        .binop(BinaryOp::I64Eq);
    builder
        .local_get(*a_hi)
        .local_get(*b_hi)
        .binop(BinaryOp::I64Eq);
    builder.binop(BinaryOp::I32And);

    Ok(())
}

fn wasm_equal_bytes(
    generator: &mut WasmGenerator,
    builder: &mut InstrSeqBuilder,
    first_op: &[LocalId],
    nth_op: &[LocalId],
) -> Result<(), GeneratorError> {
    let [offset_a, len_a] = first_op else {
        return Err(GeneratorError::InternalError(
            "wrong representation of sequence for equality".to_owned(),
        ));
    };
    let [offset_b, len_b] = nth_op else {
        return Err(GeneratorError::InternalError(
            "wrong representation of sequence for equality".to_owned(),
        ));
    };

    let memory = generator.get_memory()?;

    let len = generator.borrow_local(ValType::I32);
    let current_a = generator.borrow_local(ValType::I32);
    let current_b = generator.borrow_local(ValType::I32);

    builder.block(None, |block| {
        let block_id = block.id();
        // if the sizes are different, we can exit immediately.
        block
            .local_get(*len_a)
            .local_get(*len_b)
            .binop(BinaryOp::I32Xor)
            .local_tee(*len)
            .br_if(block_id);

        // if size equal 0, we don't loop
        block
            .local_get(*len_a)
            .local_tee(*len)
            .unop(UnaryOp::I32Eqz)
            .br_if(block_id);

        // we loop through bytes until we have a difference or we have
        // gone through all bytes.
        block.local_get(*offset_a).local_set(*current_a);
        block.local_get(*offset_b).local_set(*current_b);
        block.loop_(None, |loop_| {
            let loop_id = loop_.id();
            // we load the current byte of both sequences and check for equality
            loop_.local_get(*current_a).load(
                memory,
                walrus::ir::LoadKind::I32_8 {
                    kind: walrus::ir::ExtendedLoad::ZeroExtend,
                },
                walrus::ir::MemArg {
                    align: 1,
                    offset: 0,
                },
            );
            loop_.local_get(*current_b).load(
                memory,
                walrus::ir::LoadKind::I32_8 {
                    kind: walrus::ir::ExtendedLoad::ZeroExtend,
                },
                walrus::ir::MemArg {
                    align: 1,
                    offset: 0,
                },
            );
            loop_.binop(BinaryOp::I32Ne).br_if(block_id);

            // we update our current variables and loop if we still have elements.
            loop_
                .local_get(*current_a)
                .i32_const(1)
                .binop(BinaryOp::I32Add)
                .local_set(*current_a);
            loop_
                .local_get(*current_b)
                .i32_const(1)
                .binop(BinaryOp::I32Add)
                .local_set(*current_b);
            loop_
                .local_get(*len)
                .i32_const(1)
                .binop(BinaryOp::I32Sub)
                .local_tee(*len)
                .br_if(loop_id);
        });
    });

    // if we reached len == 0, it means that all bytes are equal
    builder.local_get(*len).unop(UnaryOp::I32Eqz);

    Ok(())
}

fn wasm_equal_optional(
    generator: &mut WasmGenerator,
    builder: &mut InstrSeqBuilder,
    first_op: &[LocalId],
    nth_op: &[LocalId],
    some_ty: &TypeSignature,
) -> Result<(), GeneratorError> {
    let Some((first_variant, first_inner)) = first_op.split_first() else {
        return Err(GeneratorError::InternalError(
            "Optional operand should have at least one argument".into(),
        ));
    };
    let Some((nth_variant, nth_inner)) = nth_op.split_first() else {
        return Err(GeneratorError::InternalError(
            "Optional operand should have at least one argument".into(),
        ));
    };

    // check if we have (some x, some x) or (none, none)
    builder
        .local_get(*first_variant)
        .local_get(*nth_variant)
        .binop(BinaryOp::I32Eq);

    // if both operands are identical,
    // [then]: we check if we have a `none` (automatic true) or if the `some` inner_type are equal
    // [else]: we push "false" on the stack
    let then_id = {
        let mut then = builder.dangling_instr_seq(ValType::I32);

        let none_case_id = {
            let mut none_ = then.dangling_instr_seq(ValType::I32);
            none_.i32_const(1);
            none_.id()
        };

        let some_case_id = {
            let mut some_ = then.dangling_instr_seq(ValType::I32);
            wasm_equal(some_ty, generator, &mut some_, first_inner, nth_inner)?;
            some_.id()
        };

        // put those in an if statement (true if `some`, false if `none`)
        then.local_get(*first_variant).instr(IfElse {
            consequent: some_case_id,
            alternative: none_case_id,
        });

        then.id()
    };

    let else_id = {
        let mut else_ = builder.dangling_instr_seq(ValType::I32);
        else_.i32_const(0);
        else_.id()
    };

    builder.instr(IfElse {
        consequent: then_id,
        alternative: else_id,
    });

    Ok(())
}

fn wasm_equal_response(
    generator: &mut WasmGenerator,
    builder: &mut InstrSeqBuilder,
    first_op: &[LocalId],
    nth_op: &[LocalId],
    ok_ty: &TypeSignature,
    err_ty: &TypeSignature,
) -> Result<(), GeneratorError> {
    let Some((first_variant, first_ok, first_err)) =
        first_op.split_first().and_then(|(variant, rest)| {
            let split_ok_err_idx = clar2wasm_ty(ok_ty).len();
            let (ok, err) = rest.split_at_checked(split_ok_err_idx)?;
            Some((variant, ok, err))
        })
    else {
        return Err(GeneratorError::InternalError(
            "Response operand should have at least one argument".into(),
        ));
    };
    let Some((nth_variant, nth_ok, nth_err)) = nth_op.split_first().and_then(|(variant, rest)| {
        let split_ok_err_idx = clar2wasm_ty(ok_ty).len();
        let (ok, err) = rest.split_at_checked(split_ok_err_idx)?;
        Some((variant, ok, err))
    }) else {
        return Err(GeneratorError::InternalError(
            "Response operand should have at least one argument".into(),
        ));
    };

    // We will have a three branch if:
    // [ok] is the (ok, ok) case, we have to compare if both ok values are identical
    // [err] is the (err, err) case, we have to compare if both err values are identical
    // [else] is the (ok, err) or (err, ok) case, it is directly false

    let ok_id = {
        let mut ok_case = builder.dangling_instr_seq(ValType::I32);
        wasm_equal(ok_ty, generator, &mut ok_case, first_ok, nth_ok)?;
        ok_case.id()
    };

    let err_id = {
        let mut err_case = builder.dangling_instr_seq(ValType::I32);
        wasm_equal(err_ty, generator, &mut err_case, first_err, nth_err)?;
        err_case.id()
    };

    let else_id = {
        let mut else_ = builder.dangling_instr_seq(ValType::I32);
        else_.i32_const(0);
        else_.id()
    };

    // inner if is checking if both are err (consequent) or ok (alternative)
    let inner_if_id = {
        let mut inner_if = builder.dangling_instr_seq(ValType::I32);
        inner_if.local_get(*first_variant).instr(IfElse {
            consequent: ok_id,
            alternative: err_id,
        });
        inner_if.id()
    };

    // outer if checks if both variants are identical (consequent) or not (alternative)
    builder
        .local_get(*first_variant)
        .local_get(*nth_variant)
        .binop(BinaryOp::I32Eq)
        .instr(IfElse {
            consequent: inner_if_id,
            alternative: else_id,
        });

    Ok(())
}

fn wasm_equal_tuple(
    generator: &mut WasmGenerator,
    builder: &mut InstrSeqBuilder,
    first_op: &[LocalId],
    nth_op: &[LocalId],
    tuple_ty: &TupleTypeSignature,
) -> Result<(), GeneratorError> {
    let tuple_inner_ty: Vec<_> = tuple_ty.get_type_map().values().collect();

    // if this is a 1-tuple, we can just check for equality of element
    if let &[ty] = tuple_inner_ty.as_slice() {
        return wasm_equal(ty, generator, builder, &first_op[1..], &nth_op[1..]);
    }

    // we'll compare tuple lazily field by field, so that
    // `(is-eq {x: a1, y: a2, z: a3} {x: b1, y: b2, z: b3})` becomes
    // ```
    // (block
    //     br_if (a1 != b1)
    //     br_if (a2 != b2)
    //     br_if (a3 != b3)
    // )
    // ```

    let result = generator.borrow_local(ValType::I32);

    let block_id = {
        let mut block = builder.dangling_instr_seq(None);
        let block_id = block.id();

        // we will check for the equality of each element, and exit the block if one is unequal
        let mut first_op_rest = &first_op[1..];
        let mut nth_op_rest = &nth_op[1..];
        for ty in tuple_inner_ty {
            let size = clar2wasm_ty(ty).len();

            let first_op_elem = if let Some((elem, rest)) = first_op_rest.split_at_checked(size) {
                first_op_rest = rest;
                elem
            } else {
                return Err(GeneratorError::InternalError(
                    "Not enough values for equality of tuples first operand".to_owned(),
                ));
            };

            let nth_op_elem = if let Some((elem, rest)) = nth_op_rest.split_at_checked(size) {
                nth_op_rest = rest;
                elem
            } else {
                return Err(GeneratorError::InternalError(
                    "Not enough values for equality of tuples nth operand".to_owned(),
                ));
            };

            wasm_equal(ty, generator, &mut block, first_op_elem, nth_op_elem)?;
            block
                .local_tee(*result)
                .unop(UnaryOp::I32Eqz)
                .br_if(block_id);
        }

        block_id
    };

    builder.instr(Block { seq: block_id }).local_get(*result);

    Ok(())
}

fn wasm_equal_list(
    generator: &mut WasmGenerator,
    builder: &mut InstrSeqBuilder,
    first_op: &[LocalId],
    nth_op: &[LocalId],
    list_ty: &TypeSignature,
) -> Result<(), GeneratorError> {
    let [_shape_a, offset_a, len_a] = first_op else {
        return Err(GeneratorError::InternalError(
            "List type should have shape, offset and length locals".to_string(),
        ));
    };
    let [_shape_b, offset_b, len_b] = nth_op else {
        return Err(GeneratorError::InternalError(
            "List type should have shape, offset and length locals".to_string(),
        ));
    };

    // need offset_delta for both types = clar2wasm_ty(list_ty).len()
    // those are the result of `generator.read_from_memory`, which is computed
    // in a block later, hence the declaration here.
    let offset_delta_a;
    let offset_delta_b;

    // if len_a != len_b { false } else if len_a == 0 { true } else LOOP

    let not_equal_sizes = {
        let mut instr = builder.dangling_instr_seq(ValType::I32);
        instr.i32_const(0);
        instr.id()
    };

    let empty_lists = {
        let mut instr = builder.dangling_instr_seq(ValType::I32);
        instr.i32_const(1);
        instr.id()
    };

    let comparison_loop = {
        let mut instr = builder.dangling_instr_seq(ValType::I32);

        let loop_id = {
            let mut loop_ = instr.dangling_instr_seq(None);
            let loop_id = loop_.id();

            // read an element from first list and assign it to locals
            offset_delta_a = generator.read_from_memory(&mut loop_, *offset_a, 0, list_ty)?;
            let first_locals = generator.save_to_locals(&mut loop_, list_ty, true);

            // same for nth list
            offset_delta_b = generator.read_from_memory(&mut loop_, *offset_b, 0, list_ty)?;
            let nth_locals = generator.save_to_locals(&mut loop_, list_ty, true);

            // compare both elements
            wasm_equal(list_ty, generator, &mut loop_, &first_locals, &nth_locals)?;

            // if there is equality, we update the variables and we loop
            loop_.if_else(
                None,
                |then| {
                    // increment the lists offsets
                    then.local_get(*offset_a)
                        .i32_const(offset_delta_a)
                        .binop(BinaryOp::I32Add)
                        .local_set(*offset_a);
                    then.local_get(*offset_b)
                        .i32_const(offset_delta_b)
                        .binop(BinaryOp::I32Add)
                        .local_set(*offset_b);

                    // loop while we still have elements
                    then.local_get(*len_b)
                        .i32_const(offset_delta_b)
                        .binop(BinaryOp::I32Sub)
                        .local_tee(*len_b)
                        .br_if(loop_id);
                },
                |_| {},
            );

            loop_id
        };

        // Now that we have our comparison loop, we add it to the instructions.
        // After it, we just have to check if the counter `len_b` is at 0, indicating
        // we looped through all elements and everything is equal
        // In case we have 3 or more operands for `is-eq`, we also should make sure that
        // *offset_a* is reset at the end of the loop. We accomplish that by putting its original
        // value on the stack before the loop and setting it back after the loop.
        instr
            .local_get(*offset_a)
            .instr(Loop { seq: loop_id })
            .local_set(*offset_a)
            .local_get(*len_b)
            .unop(UnaryOp::I32Eqz);
        instr.id()
    };

    // if-else when sizes are identical
    let equal_size_id = {
        let mut instr = builder.dangling_instr_seq(ValType::I32);
        // consequent when size is 0; alternative when size > 0
        instr.local_get(*len_b).unop(UnaryOp::I32Eqz).instr(IfElse {
            consequent: empty_lists,
            alternative: comparison_loop,
        });
        instr.id()
    };

    // if-else sizes are equal or not?
    builder
        .local_get(*len_a)
        .local_get(*len_b)
        .binop(BinaryOp::I32Eq)
        // consequent when same sizes, alternative for different sizes
        .instr(IfElse {
            consequent: equal_size_id,
            alternative: not_equal_sizes,
        });

    Ok(())
}

#[cfg(test)]
mod tests {
    use clarity::vm::Value;

    use crate::tools::{crosscheck, crosscheck_cost, evaluate};

    #[test]
    fn is_eq_less_than_one_arg() {
        let result = evaluate("(is-eq)");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting >= 1 arguments, got 0"));
    }

    #[test]
    fn is_eq_equal_buffers_with_different_max_len() {
        let snippet = "
        (define-data-var a (buff 2) 0x00)
        (define-data-var b (buff 3) 0x00)
        (is-eq (var-get a) (var-get b))";
        crosscheck(snippet, Ok(Some(clarity::vm::Value::Bool(true))));
    }

    #[test]
    fn is_eq_equal_ascii_strings_with_different_max_len() {
        let snippet = "
        (define-data-var a (string-ascii 3) \"lol\")
        (define-data-var b (string-ascii 4) \"lol\")
        (is-eq (var-get a) (var-get b))";
        crosscheck(snippet, Ok(Some(clarity::vm::Value::Bool(true))));
    }

    #[test]
    fn is_eq_equal_utf8_strings_with_different_max_len() {
        let snippet = r#"
        (define-data-var a (string-utf8 22) u"lol")
        (define-data-var b (string-utf8 21) u"lol")
        (is-eq (var-get a) (var-get b))"#;
        crosscheck(snippet, Ok(Some(clarity::vm::Value::Bool(true))));
    }

    #[test]
    fn is_eq_equal_lists_with_different_max_len() {
        let snippet = "
        (define-data-var a (list 3 int) (list 1 2 3))
        (define-data-var b (list 4 int) (list 1 2 3))
        (is-eq (var-get a) (var-get b))";
        crosscheck(snippet, Ok(Some(clarity::vm::Value::Bool(true))));
    }

    #[test]
    fn is_eq_with_different_operands_types() {
        let snippet = "(is-eq (err false) (if true (ok u1) (err true)))";

        crosscheck(snippet, Ok(Some(Value::Bool(false))));
    }

    #[test]
    fn is_eq_preserves_asymmetric_tuple_shapes() {
        const SOURCE: &str = "
            (define-data-var narrow { common: uint } { common: u1 })
            (define-data-var nested-narrow
                { outer: { common: uint } }
                { outer: { common: u1 } })
            (define-read-only (different-shape (wide { common: uint, extra: bool }))
                (is-eq (print wide) (print (var-get narrow))))
            (define-read-only (different-nested-shape
                    (wide { outer: { common: uint, extra: bool } }))
                (is-eq (print wide) (print (var-get nested-narrow))))
        ";

        crosscheck(
            &format!("{SOURCE} (different-shape {{ common: u1, extra: false }})"),
            Ok(Some(Value::Bool(false))),
        );
        crosscheck(
            &format!(
                "{SOURCE} (different-nested-shape \
                 {{ outer: {{ common: u1, extra: false }} }})"
            ),
            Ok(Some(Value::Bool(false))),
        );

        let wide = evaluate("{ common: u1, extra: false }")
            .expect("the argument evaluates")
            .expect("the argument has a value");
        crosscheck_cost(SOURCE, "different-shape", &[wide]);
    }

    #[test]
    fn is_eq_reads_nested_narrowed_runtime_shapes() {
        const SOURCE: &str = "
            (define-read-only (same-nested
                    (entry (optional { soft: bool, full: bool })))
                (is-eq
                    (ok (some (default-to { soft: true } entry)))
                    (ok (some { soft: true }))))
        ";

        crosscheck(
            &format!("{SOURCE} (same-nested (some {{ soft: true, full: true }}))"),
            Ok(Some(Value::Bool(false))),
        );
        crosscheck(
            &format!("{SOURCE} (same-nested none)"),
            Ok(Some(Value::Bool(true))),
        );

        let wide = evaluate("(some { soft: true, full: true })")
            .expect("the argument evaluates")
            .expect("the argument has a value");
        crosscheck_cost(SOURCE, "same-nested", &[wide]);
    }
}
