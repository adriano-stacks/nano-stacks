use clarity::vm::types::{FixedFunction, FunctionType, SequenceSubtype, TypeSignature};
use clarity::vm::{ClarityName, SymbolicExpression};
use walrus::ir::{self, Block, IfElse, Loop, UnaryOp};
use walrus::{InstrSeqBuilder, LocalId, ValType};

use super::{ComplexWord, SimpleWord, Word};
use crate::cost::{ChargeGenerator, WordCharge};
use crate::duck_type::dt_needed_workspace;
use crate::error_mapping::ErrorMap;
use crate::wasm_generator::{
    add_placeholder_for_clarity_type, drop_value, uses_packed_value, ArgumentsExt, GeneratorError,
    SequenceElementType, WasmGenerator,
};
use crate::wasm_utils::ArgumentCountCheck;
use crate::{check_args, words};

/// Handles Wasm values that can be short-returned in functions such as
/// `try!`, `asserts!`, or `unwrap!`.
enum ShortReturnable<'a> {
    /// Inner value of a wasm optional
    Optional {
        inner_type: &'a TypeSignature,
        value: Vec<LocalId>,
    },
    /// Inner values of a wasm response
    Response {
        err_type: &'a TypeSignature,
        ok_value: Vec<LocalId>,
        err_value: Vec<LocalId>,
    },
    /// Any kind of value in Wasm
    Any {
        ty: &'a TypeSignature,
        value: Vec<LocalId>,
        err_kind: ErrorMap,
    },
}

impl<'a> ShortReturnable<'a> {
    /// Creates a handler for an optional or a response that could be short-returned.
    ///
    /// Returns the local containing the variant of the optional or response.
    fn new(
        generator: &mut WasmGenerator,
        builder: &mut InstrSeqBuilder,
        ty: &'a TypeSignature,
    ) -> Result<(Self, LocalId), GeneratorError> {
        match ty {
            TypeSignature::OptionalType(opt) => {
                let value = generator.save_to_locals(builder, opt, true);
                let variant = generator.alloc_local(ValType::I32);
                builder.local_set(variant);
                Ok((
                    Self::Optional {
                        inner_type: opt,
                        value,
                    },
                    variant,
                ))
            }
            TypeSignature::ResponseType(resp) => {
                let (ok_type, err_type) = resp.as_ref();
                let err_value = generator.save_to_locals(builder, err_type, true);
                let ok_value = generator.save_to_locals(builder, ok_type, true);
                let variant = generator.alloc_local(ValType::I32);
                builder.local_set(variant);
                Ok((
                    Self::Response {
                        err_type,
                        ok_value,
                        err_value,
                    },
                    variant,
                ))
            }
            _ => Err(GeneratorError::TypeError(format!(
                "Invalid type for assertion: {ty}"
            ))),
        }
    }

    /// Creates a handler for any value that could be short-returned.
    ///
    /// Takes as an argument the kind of [ErrorMap] that should be returned at short-return.
    /// It should be one of [ErrorMap::ShortReturnAssertionFailure],
    /// [ErrorMap::ShortReturnExpectedValue], [ErrorMap::ShortReturnExpectedValueResponse]
    /// or [ErrorMap::ShortReturnExpectedValueOptional].
    fn new_any(
        generator: &mut WasmGenerator,
        builder: &mut InstrSeqBuilder,
        ty: &'a TypeSignature,
        err_kind: ErrorMap,
    ) -> Self {
        let value = generator.save_to_locals(builder, ty, true);
        Self::Any {
            ty,
            value,
            err_kind,
        }
    }

    /// Push a value onto the stack:
    ///
    /// - the value inside `some` for an optional
    /// - the value inside `ok` for a response
    /// - the whole value otherwise
    fn push_success_value(&self, builder: &mut InstrSeqBuilder) {
        match self {
            ShortReturnable::Optional { value, .. } => value.iter().for_each(|&l| {
                builder.local_get(l);
            }),
            ShortReturnable::Response { ok_value, .. } => ok_value.iter().for_each(|&l| {
                builder.local_get(l);
            }),
            ShortReturnable::Any { value, .. } => value.iter().for_each(|&l| {
                builder.local_get(l);
            }),
        }
    }

    /// Push the value contained inside the `err` of a response.
    ///
    /// Can fail if we don't have a response.
    fn push_err_value(&self, builder: &mut InstrSeqBuilder) -> Result<(), GeneratorError> {
        if let ShortReturnable::Response { err_value, .. } = self {
            err_value.iter().for_each(|&l| {
                builder.local_get(l);
            });
            Ok(())
        } else {
            Err(GeneratorError::TypeError("Expected a response".to_owned()))
        }
    }

    fn release(self, generator: &mut WasmGenerator) {
        let locals = match self {
            Self::Optional { value, .. } | Self::Any { value, .. } => value,
            Self::Response {
                mut ok_value,
                err_value,
                ..
            } => {
                ok_value.extend(err_value);
                ok_value
            }
        };
        generator.release_locals(locals);
    }

    /// Generates the handling of a ShortReturn error.
    fn handle_short_return(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut InstrSeqBuilder,
        condition: impl FnMut(&mut InstrSeqBuilder),
    ) -> Result<(), GeneratorError> {
        let return_ty = generator.get_current_function_return_type().cloned();
        match return_ty.as_ref() {
            Some(return_ty) => {
                self.handle_short_return_function(generator, builder, return_ty, condition)
            }
            None => self.handle_short_return_top_level(generator, builder, condition),
        }
    }

    /// Generates the handling of a ShortReturn error when we are not in a function.
    ///
    /// This is part of [ShortReturnable::handle_short_return] and shouldn't be used directly.
    fn handle_short_return_top_level(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut InstrSeqBuilder,
        mut condition: impl FnMut(&mut InstrSeqBuilder),
    ) -> Result<(), GeneratorError> {
        let short_return_id = {
            let mut sr = builder.dangling_instr_seq(None);
            match self {
                // for an optional, nothing to do but short-return.
                ShortReturnable::Optional { inner_type, .. } => {
                    generator.short_return_error(
                        &mut sr,
                        inner_type,
                        ErrorMap::ShortReturnExpectedValueOptional,
                    )?;
                }
                // for a response, we need to push the value inside `err` to the stack then short-return.
                ShortReturnable::Response {
                    err_type,
                    err_value,
                    ..
                } => {
                    for &l in err_value {
                        sr.local_get(l);
                    }
                    generator.short_return_error(
                        &mut sr,
                        err_type,
                        ErrorMap::ShortReturnExpectedValueResponse,
                    )?;
                }
                // for any other value, we push it to the stack then short-return.
                ShortReturnable::Any {
                    ty,
                    value,
                    err_kind,
                } => {
                    for &l in value {
                        sr.local_get(l);
                    }
                    generator.short_return_error(&mut sr, ty, *err_kind)?;
                }
            }
            sr.id()
        };

        let empty_id = builder.dangling_instr_seq(None).id();

        condition(builder);
        builder.instr(IfElse {
            consequent: short_return_id,
            alternative: empty_id,
        });

        Ok(())
    }

    /// Generates the handling of a ShortReturn error when we are in a function.
    ///
    /// This is part of [ShortReturnable::handle_short_return] and shouldn't be used directly.
    fn handle_short_return_function(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut InstrSeqBuilder,
        expected_type: &TypeSignature,
        mut condition: impl FnMut(&mut InstrSeqBuilder),
    ) -> Result<(), GeneratorError> {
        match self {
            // for an optional, we need to push the full value to the stack
            ShortReturnable::Optional { .. } => {
                let TypeSignature::OptionalType(expected_inner) = expected_type else {
                    return Err(GeneratorError::TypeError(format!(
                        "Expected Optional type in short return, got {expected_type}"
                    )));
                };
                builder.i32_const(0);
                add_placeholder_for_clarity_type(builder, expected_inner);
            }
            // for a response, we need to create the full value:
            // - 0 for err
            // - a placeholder for the ok value with the type of the function return type
            // - the err value
            ShortReturnable::Response { err_value, .. } => {
                builder.i32_const(0);
                let TypeSignature::ResponseType(expected_resp) = expected_type else {
                    return Err(GeneratorError::TypeError(format!(
                        "Expected Response type in assertion, got {expected_type}"
                    )));
                };
                let (expected_ok_type, _expected_err_type) = expected_resp.as_ref();
                add_placeholder_for_clarity_type(builder, expected_ok_type);
                for &l in err_value {
                    builder.local_get(l);
                }
            }
            // for any value, we just push the value on the stack
            Self::Any { value, .. } => {
                for &l in value {
                    builder.local_get(l);
                }
            }
        }

        let early_return_block_id = generator.early_return_block_id.ok_or_else(|| {
            GeneratorError::InternalError(
                "Expected a block id for returning after an assertion".to_owned(),
            )
        })?;

        if let Some(return_offset) = generator.packed_return_offset {
            // An `if` body cannot consume values below its control-frame
            // boundary. Save the assembled return value before entering it,
            // then reload it only on the returning path.
            let return_value = generator.save_to_locals(builder, expected_type, true);
            let return_id = {
                let mut return_ = builder.dangling_instr_seq(None);
                for local in &return_value {
                    return_.local_get(*local);
                }
                generator.write_to_memory(&mut return_, return_offset, 0, expected_type)?;
                return_.br(early_return_block_id);
                return_.id()
            };
            let continue_id = builder.dangling_instr_seq(None).id();
            condition(builder);
            builder.instr(IfElse {
                consequent: return_id,
                alternative: continue_id,
            });
            generator.release_locals(return_value);
            return Ok(());
        }

        // we check if we should short-return, and if yes we br_if to the current early-return block id.
        condition(builder);
        builder.br_if(early_return_block_id);

        // if we didn't short return, we need to drop the value that was pushed to the stack.
        drop_value(builder, expected_type);

        Ok(())
    }
}

#[derive(Debug)]
pub struct If;

impl Word for If {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("if")
    }
}

impl ComplexWord for If {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 3, args.len(), ArgumentCountCheck::Exact);

        self.charge(generator, builder, 0)?;

        let conditional = args.get_expr(0)?;
        let true_branch = args.get_expr(1)?;
        let false_branch = args.get_expr(2)?;

        // WORKAROUND: have to set the expression result type to the true and false branch
        let expr_ty = generator
            .get_expr_type(expr)
            .ok_or_else(|| GeneratorError::TypeError("if expression must be typed".to_owned()))?
            .clone();
        generator.set_expr_type(true_branch, expr_ty.clone())?;
        generator.set_expr_type(false_branch, expr_ty.clone())?;

        // The condition is generated first because it *runs* first, and a
        // binding's locals are freed at its last generated read: branches
        // generated ahead of the condition made the condition's read look
        // like the last one, so the slots were reused while a branch that
        // had not run yet still had to read them. Mainnet block 8,716,986
        // divided by a bin liquidity value the condition had just tested
        // against zero, and got `DivisionByZero` where the chain got a
        // quotient. Generation order is execution order here.
        //
        // The interpreter reads the condition where it is rather than
        // copying it out of its binding, so a bound name here does not pay
        // to be copied.
        generator.traverse_expr_as_borrowed_value(builder, conditional)?;

        let packed_result = uses_packed_value(&expr_ty);
        let result_offset = packed_result.then(|| {
            generator
                .create_call_stack_local(builder, &expr_ty, true, false)
                .0
        });
        let (id_true, id_false) = if let Some(result_offset) = result_offset {
            (
                generator.block_from_expr_into_memory(
                    builder,
                    true_branch,
                    result_offset,
                    &expr_ty,
                )?,
                generator.block_from_expr_into_memory(
                    builder,
                    false_branch,
                    result_offset,
                    &expr_ty,
                )?,
            )
        } else {
            (
                generator.block_from_expr(builder, true_branch)?,
                generator.block_from_expr(builder, false_branch)?,
            )
        };

        builder.instr(ir::IfElse {
            consequent: id_true,
            alternative: id_false,
        });
        if let Some(result_offset) = result_offset {
            generator.read_from_memory(builder, result_offset, 0, &expr_ty)?;
        }

        Ok(())
    }
}

#[derive(Debug)]
pub struct Match;

impl Word for Match {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("match")
    }
}

impl ComplexWord for Match {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        self.charge(generator, builder, 0)?;

        // WORKAROUND: we'll have to set the types of arguments to the type of expression,
        //             since the typechecker didn't do it for us
        let expr_ty = generator
            .get_expr_type(_expr)
            .ok_or_else(|| {
                GeneratorError::TypeError("match expression should have a type".to_owned())
            })?
            .clone();

        let match_on = args.get_expr(0)?;
        let success_binding = args.get_name(1)?;
        let success_body = args.get_expr(2)?;
        // WORKAROND: type set on some/ok body
        generator.set_expr_type(success_body, expr_ty.clone())?;

        // save the current set of named locals, for later restoration
        let saved_bindings = generator.bindings.clone();

        // Asked before the branch's own binding is in scope, or every branch
        // would look like it shadows itself.
        let success_name_used = generator.binding_name_already_used(success_binding);

        generator.traverse_expr(builder, match_on)?;
        let result_offset = uses_packed_value(&expr_ty).then(|| {
            generator
                .create_call_stack_local(builder, &expr_ty, true, false)
                .0
        });
        generator.bindings.enter_scope()?;
        // Both arms of a response match bind a name, so both are evaluated one
        // scope deeper. Restoring between them has to put back the names the
        // first arm bound without also putting back the scope, or the error
        // arm is charged its lookups as if the `match` were not there — one
        // unit short per read. An optional's `none` arm binds nothing and is
        // evaluated in the enclosing context, so it restores all the way out.
        let scoped_bindings = generator.bindings.clone();

        match generator.get_expr_type(match_on).cloned() {
            Some(TypeSignature::OptionalType(inner_type)) => {
                check_args!(generator, builder, 4, args.len(), ArgumentCountCheck::Exact);

                let none_body = args.get_expr(3)?;

                // WORKAROUND: set type on none body
                generator.set_expr_type(none_body, expr_ty.clone())?;

                let success_name = args.get_expr(1)?;
                let (some_storage, some_binding) =
                    generator.capture_binding_value(builder, success_name, &inner_type)?;
                generator.bindings.insert_spilled(
                    success_binding.clone(),
                    *inner_type,
                    some_storage,
                    some_binding,
                );

                let some_block = if let Some(result_offset) = result_offset {
                    generator.block_from_bound_expr_into_memory(
                        builder,
                        success_body,
                        success_binding,
                        success_name_used,
                        result_offset,
                        &expr_ty,
                    )?
                } else {
                    generator.block_from_bound_expr(
                        builder,
                        success_body,
                        success_binding,
                        success_name_used,
                    )?
                };

                // We can restore early, and all the way out of the match's
                // scope: the reference evaluates a `none` branch in the
                // context the match was called in, because that branch binds
                // no name for it to extend with.
                generator.bindings = saved_bindings;

                let none_block = if let Some(result_offset) = result_offset {
                    generator.block_from_expr_into_memory(
                        builder,
                        none_body,
                        result_offset,
                        &expr_ty,
                    )?
                } else {
                    generator.block_from_expr(builder, none_body)?
                };

                builder.instr(ir::IfElse {
                    consequent: some_block,
                    alternative: none_block,
                });
                if let Some(result_offset) = result_offset {
                    generator.read_from_memory(builder, result_offset, 0, &expr_ty)?;
                }

                Ok(())
            }
            Some(TypeSignature::ResponseType(inner_types)) => {
                check_args!(generator, builder, 5, args.len(), ArgumentCountCheck::Exact);

                let (ok_ty, err_ty) = &*inner_types;

                let err_binding = args.get_name(3)?;
                let err_body = args.get_expr(4)?;
                // Workaround: set type on err body
                generator.set_expr_type(err_body, expr_ty.clone())?;

                let err_name_used = generator.binding_name_already_used(err_binding);

                let err_name = args.get_expr(3)?;
                let (err_storage, err_binding_id) =
                    generator.capture_binding_value(builder, err_name, err_ty)?;
                let success_name = args.get_expr(1)?;
                let (ok_storage, ok_binding_id) =
                    generator.capture_binding_value(builder, success_name, ok_ty)?;

                generator.bindings.insert_spilled(
                    success_binding.clone(),
                    ok_ty.clone(),
                    ok_storage,
                    ok_binding_id,
                );
                let ok_block = if let Some(result_offset) = result_offset {
                    generator.block_from_bound_expr_into_memory(
                        builder,
                        success_body,
                        success_binding,
                        success_name_used,
                        result_offset,
                        &expr_ty,
                    )?
                } else {
                    generator.block_from_bound_expr(
                        builder,
                        success_body,
                        success_binding,
                        success_name_used,
                    )?
                };

                // restore named locals, inside the match's own scope
                generator.bindings.clone_from(&scoped_bindings);

                // bind err branch local
                generator.bindings.insert_spilled(
                    err_binding.clone(),
                    err_ty.clone(),
                    err_storage,
                    err_binding_id,
                );

                let err_block = if let Some(result_offset) = result_offset {
                    generator.block_from_bound_expr_into_memory(
                        builder,
                        err_body,
                        err_binding,
                        err_name_used,
                        result_offset,
                        &expr_ty,
                    )?
                } else {
                    generator.block_from_bound_expr(
                        builder,
                        err_body,
                        err_binding,
                        err_name_used,
                    )?
                };

                // restore named locals again
                generator.bindings = saved_bindings;

                builder.instr(ir::IfElse {
                    consequent: ok_block,
                    alternative: err_block,
                });
                if let Some(result_offset) = result_offset {
                    generator.read_from_memory(builder, result_offset, 0, &expr_ty)?;
                }

                Ok(())
            }
            _ => Err(GeneratorError::TypeError("Invalid type for match".into())),
        }
    }
}

#[derive(Debug)]
pub struct Filter;

impl Word for Filter {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("filter")
    }
}

impl ComplexWord for Filter {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 2, args.len(), ArgumentCountCheck::Exact);
        self.charge(generator, builder, 0)?;
        // The name of the applied function is resolved once before the loop,
        // as `special_fold` does; see `Fold`.
        generator.charge_function_lookup(builder)?;

        let memory = generator.get_memory()?;

        let discriminator = args.get_name(0)?;
        let sequence = args.get_expr(1)?;

        let expr_ty = generator
            .get_expr_type(expr)
            .ok_or_else(|| GeneratorError::TypeError("filter expression must be typed".to_owned()))?
            .clone();
        generator.set_expr_type(sequence, expr_ty)?;

        generator.traverse_expr(builder, sequence)?;

        // Get the type of the sequence
        let ty = generator
            .get_expr_type(sequence)
            .ok_or_else(|| {
                GeneratorError::TypeError("sequence expression must be typed".to_owned())
            })?
            .clone();

        let elem_ty = generator.get_sequence_element_type(sequence)?;
        let element_value_ty = match &ty {
            TypeSignature::SequenceType(sequence) => sequence.unit_type(),
            _ => {
                return Err(GeneratorError::TypeError(
                    "filter input must be a sequence".to_owned(),
                ))
            }
        };
        let is_list = matches!(
            &ty,
            TypeSignature::SequenceType(SequenceSubtype::ListType(_))
        );

        // Setup neccesary locals for the operations.
        let input_len = generator.alloc_local(ValType::I32);
        let input_offset = generator.alloc_local(ValType::I32);
        let output_len = generator.alloc_local(ValType::I32);
        builder.i32_const(0).local_set(output_len);

        // save list (offset, length) to locals
        builder.local_set(input_len).local_set(input_offset);
        // The input's shape handle is kept rather than dropped: the result
        // inherits the capacity it names. See
        // `WasmGenerator::capture_filtered_runtime_shape`.
        let input_handle = if is_list {
            let handle = generator.alloc_local(ValType::I32);
            builder.local_set(handle);
            Some(handle)
        } else {
            None
        };

        // reserve space for the output list
        let (output_offset, _) = generator.create_call_stack_local(builder, &ty, false, true);

        // An empty sequence filters to an empty sequence, and the loop below cannot
        // be asked that question: it is a do-while, so its body always runs once and
        // its end check subtracts an element size from the remaining length. At
        // length zero that reads an element which is not there and leaves the length
        // *negative*, so `br_if` keeps looping -- down through every multiple of the
        // element size until the counter wraps back to zero, two hundred and
        // sixty-eight million iterations later for a `uint`. In practice it does not
        // get there: it walks off linear memory and traps, or it spends the whole
        // tenure's cost budget first.
        //
        // Both were seen. `(filter f <empty stored list>)` traps with an
        // out-of-bounds memory access where the interpreter answers the empty list,
        // and mainnet block 8,832,029 charged `unstake-lp-tokens` 303,863
        // `read_count` against a 30,000 limit for a filter over an empty
        // `cycles-to-unstake`, where the network charged 7 ([[149]]).
        //
        // `fold` and `map` already guard their loops this way; this one did not.
        let then = builder.dangling_instr_seq(None);
        let then_id = then.id();
        let mut else_ = builder.dangling_instr_seq(None);
        let else_id = else_.id();

        let mut loop_ = else_.dangling_instr_seq(None);
        let loop_id = loop_.id();

        // Load an element from the sequence
        elem_ty.load(generator, &mut loop_, input_offset)?;
        let elem_size = elem_ty.type_size();

        if let Some(simple) = words::lookup_simple(discriminator) {
            // Call simple builtin
            simple.visit(
                generator,
                &mut loop_,
                &[TypeSignature::BoolType],
                &TypeSignature::BoolType,
            )?;
        } else {
            // In the case of a user defined function for a list element, we need to support the case where
            // the discriminant argument is more complete than the type of the list elements.
            // e.g:
            // ```
            // (define-private (foo (a (response int bool))) (and (is-ok a) (< (unwrap-panic a) 100)))
            // (filter foo (list (ok 1) (ok 2)))
            // ```
            // The function expects a `response int bool` but the type of the element is `response int UNKNOWN`.
            // This is something we can't fix with a regulare "workaround" since the type of the expression is identical
            // to the type of the sequence.
            let argument_size = generator.take_argument_size(&mut loop_, &element_value_ty)?;
            if let SequenceElementType::Other(list_elem_ty) = &elem_ty {
                let arg_ty = match generator
                    .get_function_type(discriminator.as_str())
                    .ok_or_else(|| {
                        GeneratorError::InternalError(format!(
                            "Couldn't find discriminant function {discriminator} for filter"
                        ))
                    })? {
                    FunctionType::Fixed(FixedFunction { args, .. }) if args.len() == 1 => {
                        args[0].signature.clone()
                    }
                    _ => {
                        return Err(GeneratorError::TypeError(
                            "Invalid function type for a filter discriminant".to_owned(),
                        ))
                    }
                };
                // We need a preallocated space for the duck-typed argument, because we don't know if it will be used immediately
                // in the discriminator call.
                // Since an element of a sequence will always have the same size as all the other elements, and since
                // the type of the argument of the discriminator is fixed, we can allocate a static space in memory
                // where we can store any duck-typed element for all calls to discriminator.
                let ducktype_offset = generator.reserve_static_memory(dt_needed_workspace(&arg_ty));
                let l = generator.borrow_local(ValType::I32);
                loop_.i32_const(ducktype_offset as _).local_set(*l);
                generator.duck_type(&mut loop_, list_elem_ty, &arg_ty, Some(*l))?;
            }
            generator.visit_call_user_defined(
                &mut loop_,
                discriminator,
                &TypeSignature::BoolType,
                Some(&TypeSignature::BoolType),
                None,
                Some(std::slice::from_ref(&argument_size)),
            )?;
        }
        // [ Discriminator result (bool) ]

        loop_.if_else(
            None,
            |then| {
                // copy value to result sequence
                then.local_get(output_offset)
                    .local_get(output_len)
                    .binop(ir::BinaryOp::I32Add)
                    .local_get(input_offset)
                    .i32_const(elem_size)
                    .memory_copy(memory, memory);

                // increment the size of result sequence
                then.local_get(output_len)
                    .i32_const(elem_size)
                    .binop(ir::BinaryOp::I32Add)
                    .local_set(output_len);
            },
            |_else| {},
        );

        // increment offset, leaving the new offset on the stack for the end check
        loop_
            .local_get(input_offset)
            .i32_const(elem_size)
            .binop(ir::BinaryOp::I32Add)
            .local_set(input_offset);

        // Loop if we haven't reached the end of the sequence
        loop_
            .local_get(input_len)
            .i32_const(elem_size)
            .binop(ir::BinaryOp::I32Sub)
            .local_tee(input_len)
            .br_if(loop_id);

        else_.instr(Loop { seq: loop_id });

        builder
            .local_get(input_len)
            .unop(UnaryOp::I32Eqz)
            .instr(IfElse {
                consequent: then_id,
                alternative: else_id,
            });

        if let Some(input_handle) = input_handle {
            builder.i32_const(0);
            builder.local_get(output_offset);
            builder.local_get(output_len);
            generator.capture_filtered_runtime_shape(
                builder,
                &ty,
                input_handle,
                input_len,
                elem_size,
            )?;
        } else {
            builder.local_get(output_offset);
            builder.local_get(output_len);
        }

        Ok(())
    }
}

/// Default implementation for `and` that handles the evaluation of its arguments.
#[derive(Debug)]
pub struct And;

impl Word for And {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("and")
    }
}

impl ComplexWord for And {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        let args_len = args.len();

        check_args!(generator, builder, 1, args_len, ArgumentCountCheck::AtLeast);

        self.charge(generator, builder, args_len as u32)?;

        let block_id = {
            let mut block = builder.dangling_instr_seq(ValType::I32);
            let block_id = block.id();

            // we push a false on the stack for the case where we break early
            block.i32_const(0);

            for arg in args {
                // Read in place: `and` and `or` evaluate an operand without
                // copying it out of its binding.
                generator.traverse_expr_as_borrowed_value(&mut block, arg)?;
                // if argument is false, we break early
                block.unop(UnaryOp::I32Eqz).br_if(block_id);
            }

            // if we reach this point, result is true, so we drop the current false on the stack and push true.
            block.drop().i32_const(1);

            block_id
        };

        builder.instr(Block { seq: block_id });

        Ok(())
    }
}

/// Implementation of `and` that doesn't evaluate its arguments.
/// This version of `and` is a variadic word.
///
/// An example of usage would be in `(map and (list true) (list false))`.
/// Since both lists are already evaluated, the `and` cannot re-evaluate its arguments.
#[derive(Debug)]
pub struct SimpleAnd;

impl Word for SimpleAnd {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("and")
    }
}

impl SimpleWord for SimpleAnd {
    fn visit(
        &self,
        _generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        arg_types: &[TypeSignature],
        _return_type: &TypeSignature,
    ) -> Result<(), GeneratorError> {
        if arg_types.len() > 1 {
            builder.binop(ir::BinaryOp::I32And);
        }

        Ok(())
    }
}

/// Default implementation for `or` that handles the evaluation of its arguments.
#[derive(Debug)]
pub struct Or;

impl Word for Or {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("or")
    }
}

impl ComplexWord for Or {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        let args_len = args.len();

        check_args!(generator, builder, 1, args_len, ArgumentCountCheck::AtLeast);

        self.charge(generator, builder, args_len as u32)?;

        let block_id = {
            let mut block = builder.dangling_instr_seq(ValType::I32);
            let block_id = block.id();

            // we push a true on the stack for the case where we break early
            block.i32_const(1);

            for arg in args {
                // Read in place, as `and` does.
                generator.traverse_expr_as_borrowed_value(&mut block, arg)?;
                // if argument is true, we break early
                block.br_if(block_id);
            }

            // if we reach this point, result is false, so we drop the current true on the stack and push true.
            block.drop().i32_const(0);

            block_id
        };

        builder.instr(Block { seq: block_id });

        Ok(())
    }
}

/// Implementation of `or` that doesn't evaluate its arguments.
/// This version of `or` is a variadic word.
///
/// An example of usage would be in `(map or (list true) (list false))`.
/// Since both lists are already evaluated, the `or` cannot re-evaluate its arguments.
#[derive(Debug)]
pub struct SimpleOr;

impl Word for SimpleOr {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("or")
    }
}

impl SimpleWord for SimpleOr {
    fn visit(
        &self,
        _generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        arg_types: &[TypeSignature],
        _return_type: &TypeSignature,
    ) -> Result<(), GeneratorError> {
        if arg_types.len() > 1 {
            builder.binop(ir::BinaryOp::I32Or);
        }

        Ok(())
    }
}

#[derive(Debug)]
pub struct Unwrap;

impl Word for Unwrap {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("unwrap!")
    }
}

impl ComplexWord for Unwrap {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 2, args.len(), ArgumentCountCheck::Exact);

        self.charge(generator, builder, 0)?;

        let expr_ty = generator
            .get_expr_type(expr)
            .ok_or_else(|| {
                GeneratorError::TypeError("Unwrap expression should have a type".to_owned())
            })
            .cloned()?;

        let input = args.get_expr(0)?;
        let throw = args.get_expr(1)?;

        // we need a workaround for the input: we need to make sure that the `some` or `ok` value
        // will have the same type as the type of the whole expression.
        let input_ty = match generator.get_expr_type(input).ok_or_else(|| {
            GeneratorError::TypeError("Input value for unwrap! should be typed".to_owned())
        })? {
            TypeSignature::OptionalType(_) => TypeSignature::OptionalType(Box::new(expr_ty)),
            TypeSignature::ResponseType(resp) => {
                TypeSignature::ResponseType(Box::new((expr_ty, resp.as_ref().1.clone())))
            }
            _ => {
                return Err(GeneratorError::TypeError(
                    "Unwrap expects an optional or response input".to_owned(),
                ))
            }
        };
        generator.set_expr_type(input, input_ty.clone())?;

        // if we are in a function, we should make sure the thrown value is the same type as the return type.
        if let Some(ty) = generator.get_current_function_return_type().cloned() {
            generator.set_expr_type(throw, ty)?;
        }
        let throw_ty = generator
            .get_expr_type(throw)
            .ok_or_else(|| {
                GeneratorError::TypeError("Thrown value for unwrap! should be typed".to_owned())
            })
            .cloned()?;

        generator.traverse_expr(builder, input)?;

        // we save the input as a short-returnable by convenience: we have accesse to [ShortReturnable::push_success_value]
        let (short_returnable_input, variant) =
            ShortReturnable::new(generator, builder, &input_ty)?;

        generator.traverse_expr(builder, throw)?;

        // we save the thrown value as a short returnable and handle a short-return
        let short_returnable_throw = ShortReturnable::new_any(
            generator,
            builder,
            &throw_ty,
            ErrorMap::ShortReturnExpectedValue,
        );

        short_returnable_throw.handle_short_return(generator, builder, |instrs| {
            // we need to short-return if the variant is `none` or `err`
            instrs.local_get(variant).unop(UnaryOp::I32Eqz);
        })?;
        short_returnable_throw.release(generator);
        generator.release_locals(vec![variant]);

        // if we didn't short-return, we push the inner value of the input.
        short_returnable_input.push_success_value(builder);
        short_returnable_input.release(generator);

        Ok(())
    }
}

#[derive(Debug)]
pub struct UnwrapErr;

impl Word for UnwrapErr {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("unwrap-err!")
    }
}

impl ComplexWord for UnwrapErr {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 2, args.len(), ArgumentCountCheck::Exact);

        self.charge(generator, builder, 0)?;

        let expr_ty = generator
            .get_expr_type(expr)
            .ok_or_else(|| {
                GeneratorError::TypeError("Unwrap-err expression should have a type".to_owned())
            })
            .cloned()?;

        let input = args.get_expr(0)?;
        let throw = args.get_expr(1)?;

        // we need a workaround for the input: we need to make sure that the `err` value
        // will have the same type as the type of the whole expression.
        let input_ty = match generator.get_expr_type(input).ok_or_else(|| {
            GeneratorError::TypeError("Input value for unwrap-err! should be typed".to_owned())
        })? {
            TypeSignature::ResponseType(resp) => {
                TypeSignature::ResponseType(Box::new((resp.as_ref().0.clone(), expr_ty)))
            }
            _ => {
                return Err(GeneratorError::TypeError(
                    "Unwrap-err expects a response input".to_owned(),
                ))
            }
        };
        generator.set_expr_type(input, input_ty.clone())?;

        // if we are in a function, we should make sure the thrown value is the same type as the return type.
        if let Some(ty) = generator.get_current_function_return_type().cloned() {
            generator.set_expr_type(throw, ty)?;
        }
        let throw_ty = generator
            .get_expr_type(throw)
            .ok_or_else(|| {
                GeneratorError::TypeError("Thrown value for unwrap-err! should be typed".to_owned())
            })
            .cloned()?;

        generator.traverse_expr(builder, input)?;

        // we save the input as a short-returnable by convenience: we have accesse to [ShortReturnable::push_err_value]
        let (short_returnable_input, variant) =
            ShortReturnable::new(generator, builder, &input_ty)?;

        generator.traverse_expr(builder, throw)?;

        // we save the thrown value as a short returnable and handle a short-return
        let short_returnable_throw = ShortReturnable::new_any(
            generator,
            builder,
            &throw_ty,
            ErrorMap::ShortReturnExpectedValue,
        );

        short_returnable_throw.handle_short_return(generator, builder, |instrs| {
            // we need to short-return if the variant is `ok`
            instrs.local_get(variant);
        })?;
        short_returnable_throw.release(generator);
        generator.release_locals(vec![variant]);

        // if we didn't short-return, we push the `err` value of the input.
        short_returnable_input.push_err_value(builder)?;
        short_returnable_input.release(generator);

        Ok(())
    }
}

#[derive(Debug)]
pub struct Asserts;

impl Word for Asserts {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("asserts!")
    }
}

impl ComplexWord for Asserts {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 2, args.len(), ArgumentCountCheck::Exact);

        self.charge(generator, builder, 0)?;

        let predicate_expr = args.get_expr(0)?;
        let thrown = args.get_expr(1)?;

        // The interpreter keeps the evaluated predicate rather than reading it
        // in place, so a bound name here does pay to be copied.
        generator.traverse_expr(builder, predicate_expr)?;
        let predicate = generator.alloc_local(ValType::I32);
        builder.local_set(predicate);

        let thrown_type = generator
            .get_current_function_return_type()
            .or_else(|| generator.get_expr_type(thrown))
            .ok_or_else(|| {
                GeneratorError::TypeError("Thrown value in an asserts! should be typed".to_owned())
            })
            .cloned()?;
        let mut failure = builder.dangling_instr_seq(None);
        generator.set_expr_type(thrown, thrown_type.clone())?;
        generator.traverse_expr(&mut failure, thrown)?;
        let short_returnable_thrown = ShortReturnable::new_any(
            generator,
            &mut failure,
            &thrown_type,
            ErrorMap::ShortReturnAssertionFailure,
        );
        short_returnable_thrown.handle_short_return(generator, &mut failure, |instrs| {
            instrs.i32_const(1);
        })?;
        let failure = failure.id();
        let success = builder.dangling_instr_seq(None).id();
        builder.local_get(predicate).instr(IfElse {
            consequent: success,
            alternative: failure,
        });
        short_returnable_thrown.release(generator);
        generator.release_locals(vec![predicate]);

        // if we didn't short-return, the result is always `true`
        builder.i32_const(1);

        Ok(())
    }
}

#[derive(Debug)]
pub struct Try;

impl Word for Try {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("try!")
    }
}

impl ComplexWord for Try {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 1, args.len(), ArgumentCountCheck::Exact);

        let input = args.get_expr(0)?;
        let input_ty = generator.get_expr_type(input).cloned().ok_or_else(|| {
            GeneratorError::TypeError("The argument in try! should be typed".to_owned())
        })?;

        generator.traverse_expr(builder, input)?;
        // `try!` is a native function: the interpreter evaluates its argument,
        // then charges through `dispatch_args`. Charging first makes an
        // argument that aborts pay for a `try!` that never ran. See `ok`.
        self.charge(generator, builder, 0)?;

        // we save the input as a short-returnable and handle the short-return
        let (short_returnable_value, variant) =
            ShortReturnable::new(generator, builder, &input_ty)?;

        short_returnable_value.handle_short_return(generator, builder, |instrs| {
            // we need to short-return if the variant is `none` or `err`
            instrs.local_get(variant).unop(UnaryOp::I32Eqz);
        })?;

        // if no short-return, we push the success value to the stack.
        short_returnable_value.push_success_value(builder);
        short_returnable_value.release(generator);
        generator.release_locals(vec![variant]);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clarity::vm::errors::{EarlyReturnError, VmExecutionError};
    use clarity::vm::types::ResponseData;
    use clarity::vm::Value;

    use crate::tools::{
        crosscheck, crosscheck_cost, crosscheck_expect_failure, evaluate, interpret,
    };

    #[test]
    fn trivial() {
        crosscheck("true", Ok(Some(Value::Bool(true))));
    }

    /// A binding read once in the condition and once in a branch. The read in
    /// the branch runs last, so the slots must still hold the value there.
    #[test]
    fn binding_read_in_condition_survives_into_the_branch() {
        crosscheck(
            "(define-read-only (f (x uint))
               (let ((d (* u5 u7)))
                 (if (is-eq d u0) u0 (/ x d))))
             (f u70)",
            Ok(Some(Value::UInt(2))),
        );
    }

    /// The same, through `or`: mainnet's `dlmm-core-v-1-1.add-liquidity`
    /// guards its division with `(or (is-eq shares u0) (is-eq value u0))`
    /// and then divides by that same `value`.
    #[test]
    fn binding_read_in_a_disjunction_survives_into_the_branch() {
        crosscheck(
            "(define-read-only (f (x uint))
               (let ((shares u33619060)
                     (value (* u14073473 x))
                     (bin-value (* u14073473 u419249642)))
                 (if (or (is-eq shares u0) (is-eq bin-value u0))
                     (sqrti value)
                     (/ (* value shares) bin-value))))
             (f u2204130835)",
            Ok(Some(Value::UInt(176_746_261))),
        );
    }

    /// A binding whose only read is in the branch, with the condition
    /// reading another binding of the same shape: the branch still runs
    /// after the condition.
    #[test]
    fn binding_read_only_in_a_branch_survives_the_condition() {
        crosscheck(
            "(define-read-only (f (x uint))
               (let ((guard (> x u0)) (d (+ x u1)))
                 (if guard (/ (* d d) d) u0)))
             (f u41)",
            Ok(Some(Value::UInt(42))),
        );
    }

    #[test]
    fn if_less_than_three_args() {
        let result = evaluate("(if true true)");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 3 arguments, got 2"));
    }

    #[test]
    fn if_more_than_three_args() {
        let result = evaluate("(if true true true true)");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 3 arguments, got 4"));
    }

    #[test]
    fn what_if() {
        crosscheck("(if true true false)", Ok(Some(Value::Bool(true))));
    }

    #[test]
    fn what_if_complex() {
        crosscheck("(if true (+ 1 1) (+ 2 2))", Ok(Some(Value::Int(2))));
        crosscheck("(if false (+ 1 1) (+ 2 2))", Ok(Some(Value::Int(4))));
    }

    #[test]
    fn what_if_extensive_condition() {
        crosscheck(
            "(if (> 9001 9000) (+ 1 1) (+ 2 2))",
            Ok(Some(Value::Int(2))),
        );
    }

    #[test]
    fn filter_less_than_two_args() {
        let result = evaluate("(filter (x int))");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 2 arguments, got 1"));
    }

    #[test]
    fn filter_more_than_two_args() {
        let result = evaluate("(filter (x int) (list 1 2 3 4) (list 1 2 3 4))");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 2 arguments, got 3"));
    }

    #[test]
    fn filter() {
        crosscheck(
            "
(define-private (is-great (number int))
  (> number 2))

(filter is-great (list 1 2 3 4))
",
            evaluate("(list 3 4)"),
        );
    }

    #[test]
    fn filter_builtin() {
        crosscheck(
            "(filter not (list false false true false true true false))",
            evaluate("(list false false false false)"),
        );
    }

    #[test]
    fn filter_responses() {
        let snippet = "
(define-private (is-great (x (response int int)))
  (match x
    number (> number 2)
    number (> number 2)))

(filter is-great
  (list
    (ok 2)
    (ok 3)
    (err 4)
    (err 0)
    (ok -3)))";
        crosscheck(snippet, evaluate("(list (ok 3) (err 4))"));
    }

    #[test]
    fn filter_result_read_only_double_workaround() {
        let snippet = "
(define-read-only (is-even? (x int))
        (is-eq (* (/ x 2) 2) x))

(define-private (grob (x (response int int)))
  (match x
    a (is-even? a)
    b (not (is-even? b))))

(default-to
    (list)
    (some (filter grob (list (err 1) (err 1))))
)";

        crosscheck(snippet, evaluate("(list (err 1) (err 1))"));
    }

    #[test]
    fn filter_buff() {
        crosscheck(
            "
(define-private (is-dash (char (buff 1)))
    (is-eq char 0x2d) ;; -
)
(filter is-dash 0x612d62)",
            Ok(Some(Value::buff_from_byte(0x2d))),
        );
    }

    #[test]
    fn filter_with_different_types_for_predicates() {
        crosscheck(
            "
            (define-private (foo (a (response int bool))) (and (is-ok a) (< (unwrap-panic a) 100)))
            (define-private (bar (a (response int uint))) (and (is-ok a) (> (unwrap-panic a) 42)))

            (filter bar (filter foo (list (ok 1) (ok 50))))
        ",
            Ok(Some(
                Value::cons_list_unsanitized(vec![Value::okay(Value::Int(50)).unwrap()]).unwrap(),
            )),
        );
    }

    #[test]
    fn nested_logical() {
        crosscheck(
            r#"
 (begin (not (or (and true true true) (or true true false false))))
                "#,
            Ok(Some(Value::Bool(false))),
        );
    }

    #[test]
    fn and_less_than_one_arg() {
        let result = evaluate("(and)");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting >= 1 arguments, got 0"));
    }

    #[test]
    fn and() {
        crosscheck(
            r#"
(define-data-var cursor int 6)
(and
  (var-set cursor (+ (var-get cursor) 1))
  true
  (var-set cursor (+ (var-get cursor) 1))
  false
  (var-set cursor (+ (var-get cursor) 1)))
(var-get cursor)
                "#,
            evaluate("8"),
        );
    }

    #[test]
    fn or_less_than_one_arg() {
        let result = evaluate("(or)");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting >= 1 arguments, got 0"));
    }

    #[test]
    fn or() {
        crosscheck(
            r#"
(define-data-var cursor int 6)
(or
  (begin
    (var-set cursor (+ (var-get cursor) 1))
    false)
  false
  (var-set cursor (+ (var-get cursor) 1))
  (var-set cursor (+ (var-get cursor) 1)))
(var-get cursor)
                "#,
            evaluate("8"),
        );
    }

    #[test]
    fn match_less_than_two_args() {
        crosscheck_expect_failure(
            "
(define-private (add-10 (x (response int int)))
 (match x
   val (+ val 10)
    ))",
        );
    }

    #[test]
    fn match_more_than_five_args() {
        crosscheck_expect_failure(
            "
(define-private (add-10 (x (response int int)))
 (match x
   val (+ val 10)
   error (+ error 107)
   error2
   ))",
        );
    }

    #[test]
    fn clar_match_a() {
        const ADD_10: &str = "
(define-private (add-10 (x (response int int)))
 (match x
   val (+ val 10)
   error (+ error 107)))";

        crosscheck(
            &format!("{ADD_10} (add-10 (ok 115))"),
            Ok(Some(Value::Int(125))),
        );
        crosscheck(
            &format!("{ADD_10} (add-10 (err 18))"),
            Ok(Some(Value::Int(125))),
        );
    }

    #[test]
    fn clar_match_disallow_builtin_names() {
        // It's not allowed to use names of user-defined functions as bindings
        const ERR: &str = "
(define-private (test (x (response int int)))
 (match x
   val (+ val 10)
   err (+ err 107)))";

        crosscheck_expect_failure(&format!("{ERR} (test (err 18))"));
    }

    #[test]
    fn clar_match_builtin_name_binds_only_on_its_own_branch() {
        // The interpreter checks a `match` binding's name when it *binds* it,
        // so a branch that never runs never rejects. Refusing the contract at
        // compile time instead made every call into it fail: mainnet
        // 8,668,096's `auto-alex-v3-endpoint-v2-02` binds `err` in an error
        // branch `rebase` does not take, and the chain answers `(ok u390)`.
        const ERR: &str = "
(define-private (test (x (response int int)))
 (match x
   val (+ val 10)
   err (+ err 107)))";

        crosscheck(&format!("{ERR} (test (ok 115))"), Ok(Some(Value::Int(125))));
        crosscheck_expect_failure(&format!("{ERR} (test (err 18))"));
    }

    #[test]
    fn clar_match_optional_builtin_name_binds_only_on_its_own_branch() {
        // Same rule for `match` on an optional: the `none` branch binds
        // nothing, so a reserved name on the `some` side is only reached when
        // there is a value.
        const ERR: &str = "
(define-private (test (x (optional int)))
 (match x
   err (+ err 10)
   107))";

        crosscheck(&format!("{ERR} (test none)"), Ok(Some(Value::Int(107))));
        crosscheck_expect_failure(&format!("{ERR} (test (some 115))"));
    }

    /// A *read-only* function's name is the one function name the analyzer's
    /// `match` check does not consult.
    ///
    /// `check_name_used` lists `private_function_types` and
    /// `public_function_types` and not `read_only_function_types`, so this
    /// contract passes analysis and deploys; the interpreter's runtime check is
    /// `contract_context.lookup_function`, which has all three, so it raises
    /// `NameAlreadyUsed` on the branch that binds the name. Same shape as
    /// mainnet 8,668,096's reserved `err`, one map over.
    #[test]
    fn clar_match_read_only_function_name_binds_only_on_its_own_branch() {
        const SHADOW: &str = "
(define-read-only (total) 107)
(define-private (test (x (response int int)))
 (match x
   val (+ val 10)
   total (+ total 107)))";

        crosscheck(
            &format!("{SHADOW} (test (ok 115))"),
            Ok(Some(Value::Int(125))),
        );
        crosscheck_expect_failure(&format!("{SHADOW} (test (err 18))"));
    }

    /// A binding that shadows an enclosing local never reaches either engine.
    ///
    /// The interpreter checks it at run time, but the analyzer checks it too --
    /// `inner_context.lookup_variable_type` in `check_special_match` -- so the
    /// contract does not deploy and the branch is never taken. Recorded as a
    /// test because reading only the interpreter suggests a divergence that the
    /// deploy path makes unreachable.
    #[test]
    fn clar_match_shadowing_an_enclosing_local_is_refused_by_analysis() {
        const SHADOW: &str = "
(define-private (test (x (response int int)))
 (let ((val 1))
  (match x
    val (+ val 10)
    e (+ e 107))))";

        for taken in ["(test (ok 115))", "(test (err 18))"] {
            crosscheck_expect_failure(&format!("{SHADOW} {taken}"));
        }
        let refused = interpret(&format!("{SHADOW} (test (ok 115))"))
            .expect_err("analysis refuses the contract")
            .to_string();
        assert!(
            refused.contains("NameAlreadyUsed") || refused.contains("Name already used"),
            "refused for another reason: {refused}"
        );
    }

    #[test]
    fn clar_match_cursed() {
        // It's not allowed to use names of user-defined functions as bindings
        const CURSED: &str = "
(define-private (cursed (x (response int int)))
 (match x
   val (+ val 10)
   cursed (+ cursed 107)))";

        crosscheck_expect_failure(&format!("{CURSED} (cursed (err 18))"));
    }

    #[test]
    fn match_optional_less_than_four_args() {
        let result = evaluate("(define-private (add-10 (x (optional int))) (match x val val))");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 4 arguments, got 3"));
    }

    #[test]
    fn match_optional_more_than_four_args() {
        let result =
            evaluate("(define-private (add-10 (x (optional int))) (match x val val 1001 1010))");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 4 arguments, got 5"));
    }

    #[test]
    fn clar_match_b() {
        const ADD_10: &str = "
(define-private (add-10 (x (optional int)))
 (match x
   val val
   1001))";

        crosscheck(
            &format!("{ADD_10} (add-10 none)"),
            Ok(Some(Value::Int(1001))),
        );

        crosscheck(
            &format!("{ADD_10} (add-10 (some 10))"),
            Ok(Some(Value::Int(10))),
        );
    }

    #[test]
    fn unwrap_less_than_two_args() {
        let result = evaluate("(define-private (unwrapper (x (optional int))) (+ (unwrap! x) 10))");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 2 arguments, got 1"));
    }

    #[test]
    fn unwrap_more_than_two_args() {
        let result =
            evaluate("(define-private (unwrapper (x (optional int))) (+ (unwrap! x 23 23) 10))");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 2 arguments, got 3"));
    }

    #[test]
    fn unwrap_a() {
        const FN: &str = "
(define-private (unwrapper (x (optional int)))
  (+ (unwrap! x 23) 10))";

        crosscheck(&format!("{FN} (unwrapper none)"), Ok(Some(Value::Int(23))));

        crosscheck(
            &format!("{FN} (unwrapper (some 10))"),
            Ok(Some(Value::Int(20))),
        );
    }

    #[test]
    fn unwrap_b() {
        const FN: &str = "
(define-private (unwrapper (x (response int int)))
  (+ (unwrap! x 23) 10))";

        crosscheck(
            &format!("{FN} (unwrapper (err 9999))"),
            Ok(Some(Value::Int(23))),
        );

        crosscheck(
            &format!("{FN} (unwrapper (ok 10))"),
            Ok(Some(Value::Int(20))),
        );
    }

    #[test]
    fn unwrap_err_less_than_two_args() {
        let result =
            evaluate("(define-private (unwrapper (x (response int int))) (+ (unwrap-err! x) 10))");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 2 arguments, got 1"));
    }

    #[test]
    fn unwrap_err_more_than_two_args() {
        let result = evaluate(
            "(define-private (unwrapper (x (response int int))) (+ (unwrap-err! x 23 23) 10))",
        );
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert!(error.to_string().contains("expecting 2 arguments, got 3"));
    }

    #[test]
    fn unwrap_err() {
        const FN: &str = "
(define-private (unwrapper (x (response int int)))
  (+ (unwrap-err! x 23) 10))";

        crosscheck(
            &format!("{FN} (unwrapper (err 9999))"),
            Ok(Some(Value::Int(10009))),
        );

        crosscheck(
            &format!("{FN} (unwrapper (ok 10))"),
            Ok(Some(Value::Int(23))),
        );
    }

    /// Verify that the full response type is set correctly for the throw
    /// expression.
    #[test]
    fn response_type_bug() {
        crosscheck(
            "
(define-private (foo)
    (ok u1)
)
(define-read-only (get-count-at-block (block uint))
    (ok (unwrap! (foo) (err u100)))
)
            ",
            Ok(None),
        )
    }

    /// Verify that the full response type is set correctly for the throw
    /// expression.
    #[test]
    fn response_type_err_bug() {
        crosscheck(
            "
(define-private (foo)
    (err u1)
)

(define-read-only (get-count-at-block (block uint))
    (ok (unwrap-err! (foo) (err u100)))
)
            ",
            Ok(None),
        )
    }

    const TRY_FN: &str = "
(define-private (tryhard (x (response int int)))
  (ok (+ (try! x) 10)))";

    #[test]
    fn try_a() {
        assert_eq!(
            evaluate(&format!("{TRY_FN} (tryhard (ok 1))")),
            evaluate("(ok 11)"),
        );
    }

    #[test]
    fn try_b() {
        assert_eq!(
            evaluate(&format!("{TRY_FN} (tryhard (err 1))")),
            evaluate("(err 1)"),
        );
    }

    const TRY_FN2: &str = "
(define-private (tryhard (x (response bool int)))
  (ok (if (try! x) u1 u2))
)";

    #[test]
    fn try_2a() {
        assert_eq!(
            evaluate(&format!("{TRY_FN2} (tryhard (ok true))")),
            evaluate("(ok u1)"),
        );
    }

    #[test]
    fn try_2b() {
        assert_eq!(
            evaluate(&format!("{TRY_FN2} (tryhard (err 1))")),
            evaluate("(err 1)"),
        );
    }

    const TRY_FN_OPT: &str = "
(define-private (tryharder (x (optional int)))
  (some (+ (try! x) 10)))";

    #[test]
    fn try_c() {
        assert_eq!(
            evaluate(&format!("{TRY_FN_OPT} (tryharder (some 1))")),
            evaluate("(some 11)"),
        );
    }

    #[test]
    fn try_d() {
        crosscheck(
            &format!("{TRY_FN_OPT} (tryharder none)"),
            Ok(Some(Value::none())),
        );
    }

    #[test]
    fn try_optional_uses_the_enclosing_functions_layout() {
        crosscheck(
            "(define-private (narrow (x (optional { a: uint, b: uint })))
               (some (get a (try! x))))
             (list (narrow (some { a: u1, b: u2 })) (narrow none))",
            evaluate("(list (some u1) none)"),
        );
    }

    #[test]
    fn try_less_than_one_arg() {
        let result =
            evaluate("(define-private (tryharder (x (optional int))) (some (+ (try!) 10)))");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 1 arguments, got 0"));
    }

    #[test]
    fn try_more_than_one_arg() {
        let result =
            evaluate("(define-private (tryharder (x (optional int))) (some (+ (try! x 23) 10)))");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 1 arguments, got 2"));
    }

    const ASSERT: &str = "
      (define-private (is-even (x int))
        (is-eq (* (/ x 2) 2) x))

      (define-private (assert-even (x int))
        (begin
          (asserts! (is-even x) (+ x 10))
          99))
    ";

    #[test]
    fn asserts_a() {
        crosscheck(
            &format!("{ASSERT} (assert-even 2)"),
            Ok(Some(Value::Int(99))),
        );
    }

    #[test]
    fn asserts_b() {
        crosscheck(
            &format!("{ASSERT} (assert-even 1)"),
            Ok(Some(Value::Int(11))),
        );
    }

    #[test]
    fn asserts_top_level_true() {
        crosscheck("(asserts! true (err u1))", Ok(Some(Value::Bool(true))));
    }

    #[test]
    fn asserts_top_level_false() {
        crosscheck(
            "(asserts! false (err u1))",
            Err(VmExecutionError::EarlyReturn(
                EarlyReturnError::AssertionFailed(Box::new(Value::Response(ResponseData {
                    committed: false,
                    data: Box::new(Value::UInt(1)),
                }))),
            )),
        )
    }

    #[test]
    fn asserts_less_than_two_args() {
        let result = evaluate("(asserts! true)");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 2 arguments, got 1"));
    }

    #[test]
    fn asserts_more_than_two_args_false() {
        let result = evaluate("(asserts! true true true)");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting 2 arguments, got 3"));
    }

    #[test]
    fn try_response_false() {
        crosscheck(
            "(try! (if false (ok u1) (err u42)))",
            Err(VmExecutionError::EarlyReturn(
                EarlyReturnError::UnwrapFailed(Box::new(Value::Response(ResponseData {
                    committed: false,
                    data: Box::new(Value::UInt(42)),
                }))),
            )),
        )
    }

    #[test]
    fn try_optional_false() {
        crosscheck(
            "(try! (if false (some u1) none))",
            Err(VmExecutionError::EarlyReturn(
                EarlyReturnError::UnwrapFailed(Box::new(Value::Optional(
                    clarity::vm::types::OptionalData { data: None },
                ))),
            )),
        )
    }

    #[test]
    fn try_something() {
        let snippet = "(ok (try! (if true (ok true) (err u3))))";

        crosscheck(snippet, Ok(Some(Value::okay_true())));
    }

    #[test]
    fn try_something_begin() {
        let snippet = "(begin (ok (try! (if true (ok true) (err u3)))))";

        crosscheck(snippet, Ok(Some(Value::okay_true())));
    }

    #[test]
    fn try_something_in_fn_ok() {
        let snippet = "
        (define-public (foo)
            (ok (try! (if true (ok true) (err u3))))
        )

        (foo)
        ";

        crosscheck(snippet, Ok(Some(Value::okay_true())));
    }

    #[test]
    fn try_something_in_fn_err() {
        let snippet = "
        (define-public (foo)
            (ok (try! (if false (ok true) (err u3))))
        )

        (foo)
        ";

        crosscheck(snippet, Ok(Some(Value::err_uint(3))));
    }

    #[test]
    fn try_reponse_true() {
        crosscheck(
            "(try! (if true (ok true) (err u3)))",
            Ok(Some(Value::Bool(true))),
        )
    }

    #[test]
    fn try_stx_transfer() {
        crosscheck(
            "(try! (stx-transfer? u100 'S1G2081040G2081040G2081040G208105NK8PE5 'ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM))",
            Ok(Some(Value::Bool(true))),
        )
    }

    #[test]
    fn try_nested_response_true() {
        crosscheck(
            "(try! (if true (ok (try! (if true (ok true) (err u3)))) (err false)))",
            Ok(Some(Value::Bool(true))),
        )
    }

    #[test]
    fn try_begin_nested() {
        crosscheck(
            "(begin (try! (if true (ok (try! (if true (ok true) (err u3)))) (err false))))",
            Ok(Some(Value::Bool(true))),
        )
    }

    #[test]
    fn try_reponse_inside_funtion() {
        crosscheck(
            "(define-public (foo) (ok (try! (if true (ok true) (err u3))))) (foo)",
            Ok(Some(Value::okay_true())),
        )
    }

    #[test]
    fn try_begin_response_inside_function() {
        crosscheck(
            "(define-public (foo) (begin (+ 1 2) (ok (try! (if true (ok true) (err u3)))))) (foo)",
            Ok(Some(Value::okay_true())),
        )
    }

    #[test]
    fn try_mint_ft() {
        crosscheck(
            "(define-fungible-token wasm-token) (try! (ft-mint? wasm-token u1000 'ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM))",
            Ok(Some(Value::Bool(true))),
        )
    }

    #[test]
    fn unwrap_needs_workaround_optional() {
        let snippet = "
            (define-private (foo)
                (unwrap! (if true none (some none)) (some (err u1)))
            )
            (foo)
        ";

        crosscheck(snippet, Ok(Some(Value::some(Value::err_uint(1)).unwrap())));
    }

    #[test]
    fn unwrap_needs_workaround_response() {
        let snippet = "
            (define-private (foo)
                (unwrap! (if true (err none) (ok none)) (some (err u1)))
            )
            (foo)
        ";

        crosscheck(snippet, Ok(Some(Value::some(Value::err_uint(1)).unwrap())));
    }

    #[test]
    fn unwrap_err_needs_workaround() {
        let snippet = "
            (define-private (foo)
                (unwrap-err! (if true (ok none) (err none)) (some (err u1)))
            )
            (foo)
        ";

        crosscheck(snippet, Ok(Some(Value::some(Value::err_uint(1)).unwrap())));
    }

    #[test]
    fn nested_begin_with_try() {
        let snippet = r#"
            (define-private (foo)
                (begin
                    (begin
                        (try! (if false (ok "hello") (err u5555)))
                    )
                    (ok true)
                )
            )
            (foo)
        "#;

        crosscheck(snippet, Ok(Some(Value::err_uint(5555))));
    }

    /// `filter` over an empty sequence answers the empty sequence, and costs what
    /// the interpreter costs for it.
    ///
    /// The loop is a do-while whose end check subtracts an element size from the
    /// remaining length, so at length zero it read an element that was not there and
    /// left the length negative -- and then kept looping. `(filter f <empty stored
    /// list>)` trapped with an out-of-bounds memory access, and mainnet block
    /// 8,832,029 charged a filter over an empty `(list 12000 uint)` 303,863
    /// `read_count` against a 30,000 limit where the network charged 7. Both
    /// dimensions are asserted here, because the value alone was already right
    /// wherever the run happened to survive.
    #[test]
    fn a_filter_over_an_empty_stored_sequence_costs_what_it_answers() {
        let list = r#"
(define-data-var helper uint u486)
(define-map holder uint {items: (list 12000 uint)})
(define-private (le (v uint)) (<= v (var-get helper)))
(map-set holder u1 {items: (list )})
(define-public (run)
  (let ((d (unwrap-panic (map-get? holder u1))))
    (ok (len (filter le (get items d))))))
"#;
        crosscheck_cost(list, "run", &[]);

        // The same for a buffer, whose element size is one byte: the wrap-around is
        // longer and the trap is the same.
        let buffer = r#"
(define-map holder uint {items: (buff 1000)})
(define-private (keep (b (buff 1))) (is-eq b 0x00))
(map-set holder u1 {items: 0x})
(define-public (run)
  (let ((d (unwrap-panic (map-get? holder u1))))
    (ok (len (filter keep (get items d))))))
"#;
        crosscheck_cost(buffer, "run", &[]);
    }

    /// A non-empty sequence is unchanged by the guard.
    #[test]
    fn a_filter_over_a_short_stored_sequence_is_unchanged() {
        let snippet = r#"
(define-data-var helper uint u486)
(define-map holder uint {items: (list 12000 uint)})
(define-private (le (v uint)) (<= v (var-get helper)))
(map-set holder u1 {items: (list u1 u500 u3)})
(define-public (run)
  (let ((d (unwrap-panic (map-get? holder u1))))
    (ok (filter le (get items d)))))
"#;
        crosscheck_cost(snippet, "run", &[]);
    }

    /// A `filter` result is sized by the capacity it inherited, not by what it
    /// kept.
    ///
    /// The reference's `filter` mutates its argument in place and returns the
    /// same value (`special_filter` → `SequenceData::filter`), so the result
    /// keeps the input's `type_signature`; and a list value's size is
    /// `type_signature_size + max_len × entry.size()`. Rebuilding the result
    /// from the kept elements sized it by the kept count instead, so every
    /// filter that dropped anything under-charged every later measurement of
    /// its result — silently, since the value was right.
    #[test]
    fn a_filter_result_is_sized_by_the_capacity_it_inherited() {
        let snippet = r#"
(define-data-var helper uint u1)
(define-private (le (v uint)) (<= v (var-get helper)))
(define-public (run (l (list 100 uint)))
  (let ((f (filter le l))) (ok (len f))))
"#;
        let list =
            Value::cons_list_unsanitized(vec![Value::UInt(1), Value::UInt(2), Value::UInt(3)])
                .expect("three elements");
        crosscheck_cost(snippet, "run", &[list]);
    }

    /// The same for a list read from storage, whose capacity is wider than its
    /// length: the entry type has to come across too, because an emptied list's
    /// own entry type is `NoType`, sized 1 where a `uint` is 16.
    ///
    /// Mainnet 8,832,029 is the case, on a `(list 12000 uint)` holding nothing.
    #[test]
    fn a_filter_result_inherits_a_stored_lists_capacity() {
        let stored = r#"
(define-data-var helper uint u486)
(define-map holder uint {items: (list 12000 uint)})
(define-private (le (v uint)) (<= v (var-get helper)))
(map-set holder u1 {items: (list u1 u9999 u2)})
(define-public (run)
  (let (
    (d (unwrap-panic (map-get? holder u1)))
    (f (filter le (get items d)))
  ) (ok (len f))))
"#;
        crosscheck_cost(stored, "run", &[]);
    }

    /// A tuple built from a widened field keeps that field's capacity.
    ///
    /// `runtime_size` reads a zero shape handle as "nothing widened this value —
    /// widening is a preservation or host crossing, and crossings assign
    /// handles". A tuple constructed *out of* a widened field breaks that: the
    /// constructor pushes a literal zero, the inline sum measures the field by
    /// its run-time length, and the capacity it carried is gone. On mainnet
    /// 8,832,029 that was `cost_print [192534]` against the compiler's `[534]`
    /// — the whole declared size of one `(list 12000 uint)`.
    #[test]
    fn a_tuple_bound_from_a_widened_field_keeps_its_capacity() {
        crosscheck_cost(
            r#"
(define-map holder uint {items: (list 12000 uint)})
(map-set holder u1 {items: (list )})
(define-public (run)
  (let (
    (d (unwrap-panic (map-get? holder u1)))
    (items (get items d))
    (t {n: u0, items: items})
  )
    (begin (print (get items t)) (ok u0))))
"#,
            "run",
            &[],
        );
    }

    /// The same through a `fold` accumulator, which is how the mainnet
    /// transaction reached it: the fold ran zero times and handed its initial
    /// tuple straight back.
    #[test]
    fn a_fold_accumulator_keeps_a_widened_fields_capacity() {
        crosscheck_cost(
            r#"
(define-map holder uint {items: (list 12000 uint)})
(map-set holder u1 {items: (list )})
(define-private (f (v uint) (acc {n: uint, items: (list 12000 uint)})) acc)
(define-public (run)
  (let (
    (d (unwrap-panic (map-get? holder u1)))
    (items (get items d))
    (acc (fold f items {n: u0, items: items}))
  )
    (begin (print (get items acc)) (ok u0))))
"#,
            "run",
            &[],
        );
    }

    /// A `list` built from a widened element is already measured correctly.
    ///
    /// Asserted rather than assumed: the audit for task 150 started from the
    /// premise that a list constructor loses its elements' capacity, and
    /// measuring said otherwise. The outer `max_len` is the element count,
    /// which is what `cons_list_unsanitized` gives the reference, and the entry
    /// type comes across because `read_from_wasm` reads each element through
    /// `read_from_wasm_indirect`, which honours the element's own handle. What
    /// *is* wrong is extracting one again — see
    /// `element_at_does_not_widen_the_element_it_extracts`.
    #[test]
    fn a_list_of_a_widened_element_is_measured_at_its_declared_width() {
        crosscheck_cost(
            r#"
(define-map holder uint {items: (list 12000 uint), n: uint})
(map-set holder u1 {items: (list ), n: u0})
(define-public (run)
  (let ((d (unwrap-panic (map-get? holder u1))))
    (begin (print (list (get items d))) (ok u0))))
"#,
            "run",
            &[],
        );
    }

    /// `append`'s result is sized by `input max_len + 1`
    /// (`special_append`: `ListTypeData::new_list(next_entry_type, size + 1)`),
    /// so it inherits the input's capacity the way `filter` inherits it.
    #[test]
    fn an_appended_list_keeps_the_capacity_it_grew_from() {
        crosscheck_cost(
            r#"
(define-map holder uint {items: (list 12000 uint), n: uint})
(map-set holder u1 {items: (list u1 u2), n: u0})
(define-public (run)
  (let (
    (d (unwrap-panic (map-get? holder u1)))
    (grown (append (get items d) u3))
  ) (ok (len grown))))
"#,
            "run",
            &[],
        );
    }

    /// `as-max-len?` reduces the capacity rather than replacing it
    /// (`special_as_max_len`: `type_signature.reduce_max_len(expected)`), so its
    /// result is sized by `min(input max_len, expected)` and not by its length.
    #[test]
    fn as_max_len_keeps_the_capacity_it_reduced() {
        crosscheck_cost(
            r#"
(define-map holder uint {items: (list 12000 uint), n: uint})
(map-set holder u1 {items: (list ), n: u0})
(define-public (run)
  (let ((d (unwrap-panic (map-get? holder u1))))
    (begin (print (unwrap-panic (as-max-len? (get items d) u12000))) (ok u0))))
"#,
            "run",
            &[],
        );
    }

    /// An element extracted from a list is measured as what it holds.
    ///
    /// The one place in this family where the reference *narrows*: `list_cons`
    /// builds its result with `Value::cons_list`, the sanitizing constructor, so
    /// each element is rebuilt against the derived entry type and any capacity
    /// it was not using is dropped. An empty `(list 12000 uint)` element is
    /// stored as `(list 0 NoType)` — `cost_print [6]`, not 192,006 — and
    /// `element-at?` hands that back.
    ///
    /// The compiler kept the element's shape handle when writing it into the
    /// list, so extraction returned the widened value and *over*-charged. That
    /// is the direction that refuses a block the network accepted, which is why
    /// this one mattered more than its size.
    ///
    /// The reference says two things at once — the list is measured at its
    /// elements' declared width, and an element read back out is only as big as
    /// what it holds — and one shape handle cannot say both. `ListCons` says
    /// them in order instead: capture the list while its elements still carry
    /// their handles, then narrow what is left in memory. Narrowing only *list*
    /// elements, because a handle on a response or an optional also carries what
    /// a `NoType` branch cannot represent inline, and dropping it there loses the
    /// value rather than its width.
    #[test]
    fn element_at_does_not_widen_the_element_it_extracts() {
        crosscheck_cost(
            r#"
(define-map holder uint {items: (list 12000 uint), n: uint})
(map-set holder u1 {items: (list ), n: u0})
(define-public (run)
  (let ((d (unwrap-panic (map-get? holder u1))))
    (begin (print (unwrap-panic (element-at? (list (get items d)) u0))) (ok u0))))
"#,
            "run",
            &[],
        );
    }
}
