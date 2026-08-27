use std::cell::Cell;

use crate::layout::get_type_size;
use clarity::vm::types::signatures::CallableSubtype;
use clarity::vm::types::{PrincipalData, TypeSignature};
use clarity::vm::{ClarityName, SymbolicExpression, SymbolicExpressionType, Value};
use walrus::ir::{BinaryOp, Block, IfElse, MemArg, StoreKind};
use walrus::{InstrSeqBuilder, LocalId, ValType};

use super::{ComplexWord, Word};
use crate::check_args;
use crate::cost::WordCharge;
use crate::wasm_generator::{
    add_placeholder_for_clarity_type, clar2wasm_ty, uses_packed_value, ArgumentsExt,
    GeneratorError, WasmGenerator,
};
use crate::wasm_utils::ArgumentCountCheck;
use crate::words::SimpleWord;

// The WASM local holding the ExternRef for the current `as-contract?`
// allowance context. Set by `AsContractSafe::traverse` so the `With*`
// words can load it onto the stack before calling their host functions.
//
// Stored in a thread_local because it is only relevant during code
// generation (not at runtime) and is only used within this module.
thread_local! {
    static ALLOWANCE_CONTEXT: Cell<Option<LocalId>> = const { Cell::new(None) };
}

/// Turn the allowance host's `(success, error)` result into the Clarity
/// response, keeping a wide response out of the Wasm `if` signature.
fn finish_safe_contract_result(
    generator: &mut WasmGenerator,
    builder: &mut InstrSeqBuilder,
    return_ty: &TypeSignature,
    inner_ty: &TypeSignature,
    result_locals: &[LocalId],
    result_offset: Option<LocalId>,
) -> Result<(), GeneratorError> {
    let wasm_return = clar2wasm_ty(return_ty);
    if let Some(result_offset) = result_offset {
        generator.note_control_arity(2, wasm_return.len());
        let condition = generator.alloc_local(ValType::I32);
        let hi = generator.alloc_local(ValType::I64);
        let lo = generator.alloc_local(ValType::I64);
        builder.local_set(condition).local_set(hi).local_set(lo);

        let mut success = builder.dangling_instr_seq(None);
        success.i32_const(1);
        for local in result_locals {
            success.local_get(*local);
        }
        success.i64_const(0).i64_const(0);
        generator.write_to_memory(&mut success, result_offset, 0, return_ty)?;
        let success = success.id();

        let mut error = builder.dangling_instr_seq(None);
        error.i32_const(0);
        add_placeholder_for_clarity_type(&mut error, inner_ty);
        error.local_get(lo).local_get(hi);
        generator.write_to_memory(&mut error, result_offset, 0, return_ty)?;
        let error = error.id();

        builder.local_get(condition).instr(IfElse {
            consequent: success,
            alternative: error,
        });
    } else {
        let block_type =
            generator.bounded_control_type(&[ValType::I64, ValType::I64], &wasm_return)?;
        builder.if_else(
            block_type,
            |then| {
                then.drop().drop().i32_const(1);
                for local in result_locals {
                    then.local_get(*local);
                }
                then.i64_const(0).i64_const(0);
            },
            |else_| {
                let hi = generator.alloc_local(ValType::I64);
                let lo = generator.alloc_local(ValType::I64);
                else_.local_set(hi).local_set(lo).i32_const(0);
                add_placeholder_for_clarity_type(else_, inner_ty);
                else_.local_get(lo).local_get(hi);
            },
        );
    }
    Ok(())
}

/// Runs `f` with the [`LocalId`] of the current `as-contract?` allowance
/// context, which is loaded from the [`ALLOWANCE_CONTEXT`] thread-local.
///
/// This is the accessor used by the `With*` allowance words during code
/// generation: it takes the local out of [`ALLOWANCE_CONTEXT`], passes it to
/// `f`, and restores it afterwards so subsequent words in the same
/// `as-contract?` body can read it too.
///
/// Returns a [`GeneratorError::InternalError`] if the context has not been set
/// (i.e. an allowance word was generated outside of an `as-contract?` body).
fn with_allowance_context<T, F>(mut f: F) -> Result<T, GeneratorError>
where
    F: FnMut(LocalId) -> Result<T, GeneratorError>,
{
    let allowance_context = ALLOWANCE_CONTEXT.take().ok_or_else(|| {
        GeneratorError::InternalError("Uninitialized allowance context".to_owned())
    })?;
    let res = f(allowance_context)?;
    ALLOWANCE_CONTEXT.set(Some(allowance_context));
    Ok(res)
}

/// The size a *constant* argument is charged at, when it is one and when that
/// differs from what its declared type says.
///
/// From epoch 3.3 the reference charges an argument at the value's own size,
/// and `type_of` on a contract principal answers `CallableType(Principal(..))`
/// — 148 — however the callee declares the parameter. A trait's 276 belongs to
/// a callable that carries a trait identifier, which nothing in nano's
/// representation records, so a constant's own recorded type is what tells the
/// two apart. `mia-smart-faktory` reaches Velar's pool through six such
/// constants and was charged 256 too much for each.
fn constant_callable_size(generator: &WasmGenerator, arg: &SymbolicExpression) -> Option<u32> {
    let ty = arg
        .match_atom()
        .and_then(|name| generator.constants.get(name.as_str()))?;
    matches!(
        ty,
        TypeSignature::CallableType(CallableSubtype::Principal(_))
    )
    .then(|| ty.size().ok())
    .flatten()
}

#[derive(Debug)]
pub struct AsContract;

impl Word for AsContract {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("as-contract")
    }
}

impl ComplexWord for AsContract {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 1, args.len(), ArgumentCountCheck::Exact);

        self.charge(generator, builder, 0)?;

        let inner = args.get_expr(0)?;

        // The inner expression is what this evaluates to, so it has to be built
        // as this type rather than the one it was analysed with. `(ok u1)` on
        // its own is `(response uint NoType)`, and laying that out where a
        // `(response uint uint)` is expected puts an i32 where the error is two
        // i64s — a module that compiles and will not load. `begin` and
        // `as-contract?` both already do this.
        let expected = generator
            .get_expr_type(expr)
            .ok_or_else(|| {
                GeneratorError::TypeError("as-contract expression must be typed".to_owned())
            })?
            .clone();
        generator.set_expr_type(inner, expected)?;

        // Call the host interface function, `enter_as_contract`
        builder.call(generator.func_by_name("stdlib.enter_as_contract"));

        // Traverse the inner expression
        generator.traverse_expr(builder, inner)?;

        // Call the host interface function, `exit_as_contract`
        builder.call(generator.func_by_name("stdlib.exit_as_contract"));

        Ok(())
    }
}

#[derive(Debug)]
pub struct AsContractSafe;

impl Word for AsContractSafe {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("as-contract?")
    }
}

impl ComplexWord for AsContractSafe {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(
            generator,
            builder,
            2,
            args.len(),
            ArgumentCountCheck::AtLeast
        );

        let [allowances, inners @ ..] = args else {
            unreachable!()
        };

        let allowance_list = allowances.match_list().ok_or_else(|| {
            GeneratorError::TypeError("as-contract?'s allowances should be a list".to_owned())
        })?;
        self.charge(generator, builder, allowance_list.len() as u32)?;

        let return_ty = generator
            .get_expr_type(expr)
            .ok_or_else(|| {
                GeneratorError::TypeError("as-contract? expression must be typed".to_owned())
            })?
            .clone();

        let inner_ty = match &return_ty {
            TypeSignature::ResponseType(resp) => &resp.0,
            _ => {
                return Err(GeneratorError::TypeError(
                    "Invalid return type for as-contract? expression".to_owned(),
                ));
            }
        };

        // workaround on the expression type for the last inner
        if let Some(last) = inners.last() {
            generator.set_expr_type(last, inner_ty.clone())?;
        }

        // Call the host interface function, `enter_as_contract_safe`
        builder.call(generator.func_by_name("stdlib.enter_as_contract_safe"));

        // Stash the allowance handle so With* words can reference it.
        let allowance_ref_local = generator.borrow_local(ValType::Externref);
        builder.local_set(*allowance_ref_local);

        // Set and make sure we are not overwriting an existing allowance context local
        let former_allowance_ctx = ALLOWANCE_CONTEXT.replace(Some(*allowance_ref_local));

        // Register each allowance (e.g. with-stx, with-stacking).
        for allowance in allowance_list {
            generator.traverse_allowance_expr(builder, allowance)?;
        }

        let result_offset = uses_packed_value(&return_ty).then(|| {
            generator
                .create_call_stack_local(builder, &return_ty, true, false)
                .0
        });

        // Block that will contain the entire traversal of the inner expressions.
        let exprs_block_id = {
            let exprs_type = if result_offset.is_some() {
                generator.lowered_control_type(&[], &clar2wasm_ty(&return_ty))
            } else {
                generator.bounded_control_type(&[], &clar2wasm_ty(&return_ty))?
            };
            let mut exprs_block = builder.dangling_instr_seq(exprs_type);
            let exprs_id = exprs_block.id();

            // In this subblock, we traverse the inner expressions. If one of them fail, we jump to the end of it
            // to execute a cleanup of the current context.
            let fail_block_id = {
                let fail_block_ty = match generator.current_function_wasm_return_types() {
                    Some(return_ty) => generator.bounded_control_type(&[], &return_ty)?,
                    None => None.into(),
                };
                let mut fail_block = exprs_block.dangling_instr_seq(fail_block_ty);
                let fail_id = fail_block.id();

                // we set the jump in case of failure to fail_id, so that a failure in an evaluated expression would jump to
                // the failure handling and would rollback the current context.
                let old_early_return = generator.early_return_block_id.replace(fail_id);

                // Run the body expression.
                generator.traverse_statement_list(&mut fail_block, inners)?;

                // Stash the body result before calling exit (exit pushes its own values).
                let result_locals = generator.save_to_locals(&mut fail_block, inner_ty, true);

                // Validate allowances and commit or abort the transaction.
                fail_block.local_get(*allowance_ref_local);
                fail_block.call(generator.func_by_name("stdlib.exit_as_contract_safe"));

                // We can put back the former allowance context
                ALLOWANCE_CONTEXT.set(former_allowance_ctx);

                // Now on stack, we have either (int - 0) if an error occured with int the error index, or (0int - 1) if
                // allowances returned no error
                finish_safe_contract_result(
                    generator,
                    &mut fail_block,
                    &return_ty,
                    inner_ty,
                    &result_locals,
                    result_offset,
                )?;

                // If we arrived here, we need to skip the cleanup and set back the early_return.
                generator.early_return_block_id = old_early_return;
                fail_block.br(exprs_id);

                fail_id
            };

            // we insert the fail block_id
            exprs_block.instr(Block { seq: fail_block_id });

            if let Some(early_return) = generator.early_return_block_id {
                // TODO: this will never be called if we are in top-level. This shouldn’t be a problem as
                // the host would discard the context on trap. However, it could lead to issues in the future
                // so a cleaner implementation should be provided here.
                exprs_block.call(generator.func_by_name("stdlib.cleanup_as_contract_safe"));

                exprs_block.br(early_return);
            } else {
                // in a top-level context, the runtime-error function would have been called already by this point.
                exprs_block.unreachable();
            }

            exprs_id
        };

        builder.instr(Block {
            seq: exprs_block_id,
        });
        if let Some(result_offset) = result_offset {
            generator.read_from_memory(builder, result_offset, 0, &return_ty)?;
        }

        Ok(())
    }
}

#[derive(Debug)]
pub struct RestrictAssets;

impl Word for RestrictAssets {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("restrict-assets?")
    }
}

impl ComplexWord for RestrictAssets {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(
            generator,
            builder,
            3,
            args.len(),
            ArgumentCountCheck::AtLeast
        );

        let [asset_owner, allowances, inners @ ..] = args else {
            unreachable!()
        };

        let return_ty = generator
            .get_expr_type(expr)
            .ok_or_else(|| {
                GeneratorError::TypeError("restrict-access? expression must be typed".to_owned())
            })?
            .clone();

        let inner_ty = match &return_ty {
            TypeSignature::ResponseType(resp) => &resp.0,
            _ => {
                return Err(GeneratorError::TypeError(
                    "Invalid return type for restrict-access? expression".to_owned(),
                ));
            }
        };

        // workaround on the expression type for the last inner
        if let Some(last) = inners.last() {
            generator.set_expr_type(last, inner_ty.clone())?;
        }

        // evaluate the asset owner and save it to locals
        generator.traverse_expr(builder, asset_owner)?;
        let asset_owner_locals =
            generator.save_to_locals(builder, &TypeSignature::PrincipalType, true);

        builder.call(generator.func_by_name("stdlib.enter_restrict_assets"));

        // Stash the allowance handle so With* words can reference it.
        let allowance_ref_local = generator.borrow_local(ValType::Externref);
        builder.local_set(*allowance_ref_local);

        // Set and make sure we are not overwriting an existing allowance context local
        let former_allowance_ctx = ALLOWANCE_CONTEXT.replace(Some(*allowance_ref_local));

        let allowance_list = allowances.match_list().ok_or(GeneratorError::TypeError(
            "restrict-assets?'s allowances should be a list".to_owned(),
        ))?;
        // Thanks to static type check we know we have less than 128 allowances
        self.charge(generator, builder, allowance_list.len() as u32)?;
        // Register each allowance (e.g. with-stx, with-stacking).
        //
        // Through `traverse_allowance_expr`, as `as-contract?` does: the
        // reference reads an allowance with `eval_allowance`, which matches the
        // form and evaluates only its operands, so there is no name to look up
        // and no `cost_lookup_function` to charge for it. Traversing it as an
        // ordinary application charged 16 per allowance — the one difference
        // between the two words' allowance handling, and `as-contract?` had it
        // right.
        for allowance in allowance_list {
            generator.traverse_allowance_expr(builder, allowance)?;
        }

        let result_offset = uses_packed_value(&return_ty).then(|| {
            generator
                .create_call_stack_local(builder, &return_ty, true, false)
                .0
        });

        // Block that will contain the entire traversal of the inner expressions.
        let exprs_block_id = {
            let exprs_type = if result_offset.is_some() {
                generator.lowered_control_type(&[], &clar2wasm_ty(&return_ty))
            } else {
                generator.bounded_control_type(&[], &clar2wasm_ty(&return_ty))?
            };
            let mut exprs_block = builder.dangling_instr_seq(exprs_type);
            let exprs_id = exprs_block.id();

            // In this subblock, we traverse the inner expressions. If one of them fail, we jump to the end of it
            // to execute a cleanup of the current context.
            let fail_block_id = {
                let fail_block_ty = match generator.current_function_wasm_return_types() {
                    Some(return_ty) => generator.bounded_control_type(&[], &return_ty)?,
                    None => None.into(),
                };
                let mut fail_block = exprs_block.dangling_instr_seq(fail_block_ty);
                let fail_id = fail_block.id();

                // we set the jump in case of failure to fail_id, so that a failure in an evaluated expression would jump to
                // the failure handling and would rollback the current context.
                let old_early_return = generator.early_return_block_id.replace(fail_id);

                // Run the body expression.
                generator.traverse_statement_list(&mut fail_block, inners)?;

                // Stash the body result before calling exit (exit pushes its own values).
                let result_locals = generator.save_to_locals(&mut fail_block, inner_ty, true);

                // Validate allowances and commit or abort the transaction.
                for l in asset_owner_locals {
                    fail_block.local_get(l);
                }
                fail_block.local_get(*allowance_ref_local);
                fail_block.call(generator.func_by_name("stdlib.exit_restrict_assets"));

                // We can put back the former allowance context
                ALLOWANCE_CONTEXT.set(former_allowance_ctx);

                // Now on stack, we have either (int - 0) if an error occured with int the error index, or (0int - 1) if
                // allowances returned no error
                finish_safe_contract_result(
                    generator,
                    &mut fail_block,
                    &return_ty,
                    inner_ty,
                    &result_locals,
                    result_offset,
                )?;

                // If we arrived here, we need to skip the cleanup and set back the early_return.
                generator.early_return_block_id = old_early_return;
                fail_block.br(exprs_id);

                fail_id
            };

            // we insert the fail block_id
            exprs_block.instr(Block { seq: fail_block_id });

            if let Some(early_return) = generator.early_return_block_id {
                // TODO: this will never be called if we are in top-level. This shouldn’t be a problem as
                // the host would discard the context on trap. However, it could lead to issues in the future
                // so a cleaner implementation should be provided here.
                exprs_block.call(generator.func_by_name("stdlib.cleanup_restrict_assets"));

                exprs_block.br(early_return);
            } else {
                // in a top-level context, the runtime-error function would have been called already by this point.
                exprs_block.unreachable();
            }

            exprs_id
        };

        builder.instr(Block {
            seq: exprs_block_id,
        });
        if let Some(result_offset) = result_offset {
            generator.read_from_memory(builder, result_offset, 0, &return_ty)?;
        }

        Ok(())
    }
}

#[derive(Debug)]
pub struct WithAllAssetsUnsafe;

impl Word for WithAllAssetsUnsafe {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("with-all-assets-unsafe")
    }
}

impl ComplexWord for WithAllAssetsUnsafe {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 0, args.len(), ArgumentCountCheck::Exact);

        // An allowance form costs nothing of its own: stacks-core never
        // evaluates one as an expression — `special_allowance` is unreachable
        // there — and charges for the whole list inside `restrict-assets?` and
        // `as-contract?`, scaled by its length. Its siblings here already do
        // not charge; this one did, and had no entry in the epoch 4.0 table to
        // charge from, so any contract using it failed to compile at all.
        with_allowance_context(|allowance_context| {
            builder.local_get(allowance_context);
            builder.call(generator.func_by_name("stdlib.with_all_assets_unsafe"));
            Ok(())
        })
    }
}

#[derive(Debug)]
pub struct WithFt;

impl Word for WithFt {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("with-ft")
    }
}

impl ComplexWord for WithFt {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 3, args.len(), ArgumentCountCheck::Exact);

        let token_contract = args.get_expr(0)?;
        let token_name = args.get_expr(1)?;
        let allowance = args.get_expr(2)?;

        with_allowance_context(|allowance_context| {
            // Load the externref allowance context (first param)
            builder.local_get(allowance_context);

            // Traverse the contract principal
            generator.traverse_expr(builder, token_contract)?;

            // Traverse the token name
            generator.traverse_expr(builder, token_name)?;

            // Traverse the allowance amount (uint)
            generator.traverse_expr(builder, allowance)?;

            // Call the host interface function, `with_ft`
            builder.call(generator.func_by_name("stdlib.with_ft"));

            Ok(())
        })
    }
}

#[derive(Debug)]
pub struct WithNft;

impl Word for WithNft {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("with-nft")
    }
}

impl ComplexWord for WithNft {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 3, args.len(), ArgumentCountCheck::Exact);

        let token_contract = args.get_expr(0)?;
        let token_name = args.get_expr(1)?;
        let allowance = args.get_expr(2)?;

        with_allowance_context(|allowance_context| {
            // Load the externref allowance context (first param)
            builder.local_get(allowance_context);

            // Traverse the contract principal
            generator.traverse_expr(builder, token_contract)?;

            // Traverse the token name
            generator.traverse_expr(builder, token_name)?;

            // Traverse the allowances list, which leaves (offset, length) on the
            // data stack.
            generator.traverse_expr(builder, allowance)?;

            // And say what type is at that offset. The host has to know to read
            // the list back, and the compiler is the only place that knows:
            // the reference makes no requirement that the asset named here
            // exists anywhere (`check_allowance_with_nft` checks only that this
            // argument is a list), so there is no NFT definition to take a key
            // type from. Reading one out of the *calling* contract is what
            // mainnet 8,671,301 fell over — `xtrata-market-sponsored-stx-v1-1`
            // allows an NFT belonging to another contract and defines none of
            // its own.
            let (ty_offset, ty_length) = generator.serialized_type_of(allowance)?;
            builder.i32_const(ty_offset).i32_const(ty_length);

            // Call the host interface function, `with_nft`
            builder.call(generator.func_by_name("stdlib.with_nft"));

            Ok(())
        })
    }
}

#[derive(Debug)]
pub struct WithStacking;

impl Word for WithStacking {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("with-stacking")
    }
}

impl ComplexWord for WithStacking {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 1, args.len(), ArgumentCountCheck::Exact);

        let allowance = args.get_expr(0)?;

        with_allowance_context(|allowance_context| {
            // Load the externref allowance context (first param)
            builder.local_get(allowance_context);

            // Traverse the allowance amount (uint)
            generator.traverse_expr(builder, allowance)?;

            // Call the host interface function, `with_stacking`
            builder.call(generator.func_by_name("stdlib.with_stacking"));

            Ok(())
        })
    }
}

#[derive(Debug)]
pub struct WithStaking;

impl Word for WithStaking {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("with-staking")
    }
}

impl ComplexWord for WithStaking {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        WithStacking.traverse(generator, builder, _expr, args)
    }
}

#[derive(Debug)]
pub struct WithPox;

impl Word for WithPox {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("with-pox")
    }
}

impl ComplexWord for WithPox {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 0, args.len(), ArgumentCountCheck::Exact);

        with_allowance_context(|allowance_context| {
            builder
                .local_get(allowance_context)
                .call(generator.func_by_name("stdlib.with_pox"));
            Ok(())
        })
    }
}

#[derive(Debug)]
pub struct WithStx;

impl Word for WithStx {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("with-stx")
    }
}

impl ComplexWord for WithStx {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 1, args.len(), ArgumentCountCheck::Exact);

        let allowance = args.get_expr(0)?;

        with_allowance_context(|allowance_context| {
            // Load the externref allowance context (first param)
            builder.local_get(allowance_context);

            // Traverse the allowance amount (uint). Alone among the allowances,
            // the reference reads `with-stx`'s amount through `as_ref`, so a
            // binding read here is never cloned and never pays
            // `LookupVariableSize`; `with-ft`, `with-nft` and `with-staking`
            // do clone theirs.
            generator.traverse_expr_as_borrowed_value(builder, allowance)?;

            // Call the host interface function, `with_stx`
            builder.call(generator.func_by_name("stdlib.with_stx"));
            Ok(())
        })
    }
}

#[derive(Debug)]
pub struct ContractCall;

impl Word for ContractCall {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("contract-call?")
    }
}

impl ComplexWord for ContractCall {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(
            generator,
            builder,
            2,
            args.len(),
            ArgumentCountCheck::AtLeast
        );

        self.charge(generator, builder, 0)?;

        let function_name = args.get_name(1)?;
        let contract_expr = args.get_expr(0)?;
        // Whether the target was resolved through a `define-constant`, which is
        // the only one of the three static forms the reference gates at run time.
        let mut target_is_constant = false;
        if let SymbolicExpressionType::LiteralValue(Value::Principal(PrincipalData::Contract(
            ref contract_identifier,
        ))) = contract_expr.expr
        {
            // This is a static contract call.
            // Push an empty trait name first
            builder.i32_const(0).i32_const(0);
            // Push the contract identifier onto the stack
            // TODO(#111): These should be tracked for reuse, similar to the string literals
            let (id_offset, id_length) =
                generator.add_literal(&contract_identifier.clone().into())?;
            builder
                .i32_const(id_offset as i32)
                .i32_const(id_length as i32);
        } else if let Some((
            TypeSignature::CallableType(CallableSubtype::Principal(contract_identifier)),
            from_constant,
        )) = contract_expr
            .match_atom()
            .and_then(|name| generator.constants.get(name.as_str()))
            .map(|ty| (ty, true))
            .or_else(|| generator.get_expr_type(contract_expr).map(|ty| (ty, false)))
            .map(|(ty, from_constant)| (ty.clone(), from_constant))
        {
            // A name that resolves to a contract principal — a constant, most
            // often — is as static a call as a literal is. Only the literal
            // form was recognised, so `(contract-call? SOME_CONSTANT f)` was
            // taken for a trait dispatch and refused for not being one.
            //
            // A `let`-bound or parameter-bound callable reaches the same branch
            // through its analysed type, and the reference dispatches through
            // those unconditionally — so only the constant carries the run-time
            // gate below.
            target_is_constant = from_constant;
            builder.i32_const(0).i32_const(0);
            let (id_offset, id_length) = generator.add_literal(&contract_identifier.into())?;
            builder
                .i32_const(id_offset as i32)
                .i32_const(id_length as i32);
        } else {
            // This is a dynamic contract call (via a trait).
            // Push the trait name on the stack
            let dynamic_arg = contract_expr.match_atom().ok_or_else(|| {
                GeneratorError::TypeError(
                    "Dynamic contract-call? argument should be a name".to_owned(),
                )
            })?;
            // Check if the name is in local bindings first, then in current function arguments.
            let trait_id = generator
                .bindings
                .get_trait_identifier(dynamic_arg)
                .or_else(|| {
                    generator
                        .get_current_function_arg_type(dynamic_arg)
                        .and_then(|ty| match ty {
                            TypeSignature::CallableType(CallableSubtype::Trait(trait_id)) => {
                                Some(trait_id)
                            }
                            TypeSignature::TraitReferenceType(trait_id) => Some(trait_id),
                            _ => None,
                        })
                })
                .ok_or_else(|| {
                    GeneratorError::TypeError(
                        "Dynamic argument of contract-call? should be a trait".to_owned(),
                    )
                })?;

            let (offset, len) = generator.used_traits.get(trait_id).ok_or_else(|| {
                GeneratorError::TypeError(format!(
                    "Usage of an unimported trait: {}",
                    trait_id.name
                ))
            })?;
            builder.i32_const(*offset as i32).i32_const(*len as i32);
            generator.traverse_callable_reference(builder, contract_expr)?;
        }

        // shadow args
        let args = if args.len() >= 2 { &args[2..] } else { &[] };
        let args_ty: Vec<_> = args
            .iter()
            .map(|arg| {
                generator
                    .get_expr_type(arg)
                    .cloned()
                    .or_else(|| {
                        // A contract principal written as a literal where the
                        // callee expects a trait carries no annotation from the
                        // type checker. On the wire it is a principal either
                        // way, which is what its size and layout depend on.
                        matches!(
                            arg.expr,
                            SymbolicExpressionType::LiteralValue(Value::Principal(
                                PrincipalData::Contract(_)
                            ))
                        )
                        .then_some(TypeSignature::PrincipalType)
                    })
                    .ok_or_else(|| {
                        GeneratorError::TypeError(
                            "contract-call? argument must be typed".to_owned(),
                        )
                    })
            })
            .collect::<Result<_, _>>()?;

        // Push the function name onto the stack
        let (fn_offset, fn_length) = generator.add_string_literal(function_name)?;
        builder
            .i32_const(fn_offset as i32)
            .i32_const(fn_length as i32);

        // Write the arguments to the call stack, to be read by the host
        let arg_offset = generator.alloc_local(ValType::I32);
        let total_args_size: i32 = args_ty.iter().map(get_type_size).sum();
        let argument_sizes_length = i32::try_from(args.len())
            .ok()
            .and_then(|length| length.checked_mul(4))
            .ok_or_else(|| {
                GeneratorError::InternalError("contract-call? argument sizes overflow".into())
            })?;
        let call_arguments_size = total_args_size
            .checked_add(argument_sizes_length)
            .ok_or_else(|| {
                GeneratorError::InternalError("contract-call? argument memory overflow".into())
            })?;
        builder
            .global_get(generator.stack_pointer)
            .local_tee(arg_offset)
            .i32_const(call_arguments_size)
            .binop(BinaryOp::I32Add)
            .global_set(generator.stack_pointer);
        let argument_sizes_offset = generator.alloc_local(ValType::I32);
        builder
            .local_get(arg_offset)
            .i32_const(total_args_size)
            .binop(BinaryOp::I32Add)
            .local_set(argument_sizes_offset);
        let memory = generator.get_memory()?;

        let mut arg_length = 0;
        for (index, (arg, arg_ty)) in args.iter().zip(args_ty).enumerate() {
            // Traverse the argument, pushing it onto the stack
            generator.traverse_expr(builder, arg)?;

            // `DefinedFunction::execute_apply` charges the value as the caller
            // evaluated it, before the callee re-tags principals as traits.
            // Preserve that size beside the representations so the host does
            // not have to infer it from the callee's declared types.
            let argument_size = generator.borrow_local(ValType::I32);
            if let SymbolicExpressionType::LiteralValue(value) = &arg.expr {
                let value_size = i32::try_from(
                    value
                        .size()
                        .map_err(|error| GeneratorError::TypeError(error.to_string()))?,
                )
                .map_err(|_| {
                    GeneratorError::InternalError(
                        "literal contract-call? argument size exceeds i32".to_owned(),
                    )
                })?;
                builder.i32_const(value_size).local_set(*argument_size);
            } else if let Some(value_size) = constant_callable_size(generator, arg) {
                let value_size = i32::try_from(value_size).map_err(|_| {
                    GeneratorError::InternalError(
                        "constant contract-call? argument size exceeds i32".to_owned(),
                    )
                })?;
                builder.i32_const(value_size).local_set(*argument_size);
            } else {
                generator.clarity_value_size_on_stack(builder, &arg_ty)?;
                builder.local_set(*argument_size);
            }

            arg_length += generator.write_to_memory(builder, arg_offset, arg_length, &arg_ty)?;
            let size_offset = u32::try_from(index)
                .ok()
                .and_then(|index| index.checked_mul(4))
                .ok_or_else(|| {
                    GeneratorError::InternalError(
                        "contract-call? argument size offset overflow".into(),
                    )
                })?;
            builder
                .local_get(argument_sizes_offset)
                .local_get(*argument_size)
                .store(
                    memory,
                    StoreKind::I32 { atomic: false },
                    MemArg {
                        align: 4,
                        offset: size_offset,
                    },
                );
        }

        // Push the arguments offset and length onto the data stack
        builder.local_get(arg_offset).i32_const(arg_length as i32);

        // Reserve space for the return value
        let return_ty = generator
            .get_expr_type(expr)
            .ok_or_else(|| {
                GeneratorError::TypeError("contract-call? expression must be typed".to_owned())
            })?
            .clone();
        let (return_offset, return_size) =
            generator.create_call_stack_local(builder, &return_ty, true, true);

        // Push the return offset and size to the data stack
        builder.local_get(return_offset).i32_const(return_size);

        // After the arguments and before the callee, where the reference decides
        // it: `special_contract_call` evaluates every argument — charging their
        // costs, which land in the receipt of the failing transaction — and only
        // then asks whether the atom names a dispatchable target.
        if target_is_constant {
            builder.call(generator.func_by_name("stdlib.check_constant_call_target"));
        }

        // Call the host interface function, `contract_call`
        builder.call(generator.func_by_name("stdlib.contract_call"));

        // Host interface fills the result into the specified memory. Read it
        // back out, and place the value on the data stack.
        generator.read_from_memory(builder, return_offset, 0, &return_ty)?;

        Ok(())
    }
}

#[derive(Debug)]
pub struct ContractHash;

impl Word for ContractHash {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("contract-hash?")
    }
}

impl SimpleWord for ContractHash {
    fn visit(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        arg_types: &[TypeSignature],
        return_type: &TypeSignature,
    ) -> Result<(), GeneratorError> {
        check_args!(
            generator,
            builder,
            1,
            arg_types.len(),
            ArgumentCountCheck::Exact
        );

        self.charge(generator, builder, 0)?;

        // Reserve space for the return value (response (buff 32) uint)
        let (return_offset, return_size) =
            generator.create_call_stack_local(builder, return_type, true, true);

        // Push the return offset and size to the data stack
        builder.local_get(return_offset).i32_const(return_size);

        // Call the host interface function, `contract_hash`
        builder.call(generator.func_by_name("stdlib.contract_hash"));

        // Host interface fills the result into the specified memory. Read it
        // back out, and place the value on the data stack.
        generator.read_from_memory(builder, return_offset, 0, return_type)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clarity::vm::Value;
    use clarity_types::ContractName;

    use crate::tools::{
        crosscheck_multi_contract, crosscheck_multi_contract_with_env, TestEnvironment,
    };

    #[cfg(not(feature = "test-clarity-v4"))]
    mod clarity_v1_v2_v3 {
        use clarity::types::StacksEpochId;
        use clarity::vm::ClarityVersion;

        use crate::tools::evaluate_at;

        #[test]
        fn as_contract_less_than_one_arg() {
            let result = evaluate_at(
                "(as-contract)",
                StacksEpochId::Epoch32,
                ClarityVersion::Clarity3,
            );
            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("expecting 1 arguments, got 0"));
        }

        #[test]
        fn as_contract_more_than_one_arg() {
            let result = evaluate_at(
                "(as-contract 1 2)",
                StacksEpochId::Epoch32,
                ClarityVersion::Clarity3,
            );
            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("expecting 1 arguments, got 2"));
        }
    }

    #[test]
    fn contract_call_less_than_two_args() {
        let mut env = TestEnvironment::default();
        env.init_contract_with_snippet(
            "contract-callee",
            r#"
(define-public (no-args)
    (ok u42)
)
            "#,
        )
        .expect("Failed to init contract.");
        let result =
            env.init_contract_with_snippet("contract-caller", "(contract-call? .contract-callee)");

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("expecting >= 2 arguments, got 1"));
    }

    #[test]
    fn static_no_args() {
        let mut env = TestEnvironment::default();
        env.init_contract_with_snippet(
            "contract-callee",
            r#"
(define-public (no-args)
    (ok u42)
)
            "#,
        )
        .expect("Failed to init contract.");
        let val = env
            .init_contract_with_snippet(
                "contract-caller",
                "(contract-call? .contract-callee no-args)",
            )
            .expect("Failed to init contract.");

        assert_eq!(val.unwrap(), Value::okay(Value::UInt(42)).unwrap());
    }

    #[test]
    fn static_one_simple_arg() {
        let mut env = TestEnvironment::default();
        env.init_contract_with_snippet(
            "contract-callee",
            r#"
(define-public (one-simple-arg (x int))
    (ok x)
)
            "#,
        )
        .expect("Failed to init contract.");
        let val = env
            .init_contract_with_snippet(
                "contract-caller",
                "(contract-call? .contract-callee one-simple-arg 42)",
            )
            .expect("Failed to init contract.");

        assert_eq!(val.unwrap(), Value::okay(Value::Int(42)).unwrap());
    }

    #[test]
    fn static_call_widens_a_tuple_field_argument() {
        crosscheck_multi_contract(
            &[
                (
                    ContractName::from_literal("callee"),
                    "(define-public (take (value (optional uint))) (ok value))",
                ),
                (
                    ContractName::from_literal("caller"),
                    "(let ((order { min-amount-out-fixed: none }))
                       (contract-call? .callee take
                         (get min-amount-out-fixed order)))",
                ),
            ],
            Ok(Some(Value::okay(Value::none()).unwrap())),
        );
    }

    #[test]
    fn static_one_arg() {
        let mut env = TestEnvironment::default();
        env.init_contract_with_snippet(
            "contract-callee",
            r#"
(define-public (one-arg (x (string-ascii 16)))
    (ok x)
)
            "#,
        )
        .expect("Failed to init contract.");
        let val = env
            .init_contract_with_snippet(
                "contract-caller",
                r#"(contract-call? .contract-callee one-arg "hello")"#,
            )
            .expect("Failed to init contract.");

        assert_eq!(
            val.unwrap(),
            Value::okay(Value::string_ascii_from_bytes("hello".to_string().into_bytes()).unwrap())
                .unwrap()
        );
    }

    #[test]
    fn static_two_simple_args() {
        let mut env = TestEnvironment::default();
        env.init_contract_with_snippet(
            "contract-callee",
            r#"
(define-public (two-simple-args (x int) (y int))
    (ok (+ x y))
)
            "#,
        )
        .expect("Failed to init contract.");
        let val = env
            .init_contract_with_snippet(
                "contract-caller",
                r#"(contract-call? .contract-callee two-simple-args 17 42)"#,
            )
            .expect("Failed to init contract.");

        assert_eq!(val.unwrap(), Value::okay(Value::Int(17 + 42)).unwrap());
    }

    #[test]
    fn static_two_args() {
        let mut env = TestEnvironment::default();
        env.init_contract_with_snippet(
            "contract-callee",
            r#"
(define-public (two-args (x (string-ascii 16)) (y (string-ascii 16)))
    (ok (concat x y))
)
            "#,
        )
        .expect("Failed to init contract.");
        let val = env
            .init_contract_with_snippet(
                "contract-caller",
                r#"(contract-call? .contract-callee two-args "hello " "world")"#,
            )
            .expect("Failed to init contract.");

        assert_eq!(
            val.unwrap(),
            Value::okay(
                Value::string_ascii_from_bytes("hello world".to_string().into_bytes()).unwrap()
            )
            .unwrap()
        );
    }

    #[test]
    fn dynamic_no_args() {
        let mut env = TestEnvironment::default();
        env.init_contract_with_snippet(
            "contract-callee",
            r#"
(define-trait test-trait ((no-args () (response uint uint))))
(define-public (no-args)
    (ok u42)
)
            "#,
        )
        .expect("Failed to init contract.");
        let val = env
            .init_contract_with_snippet(
                "contract-caller",
                r#"
(use-trait test-trait .contract-callee.test-trait)
(define-private (call-it (t <test-trait>))
    (contract-call? t no-args)
)
(call-it .contract-callee)
            "#,
            )
            .expect("Failed to init contract.");

        assert_eq!(val.unwrap(), Value::okay(Value::UInt(42)).unwrap());
    }

    #[test]
    fn dynamic_one_simple_arg() {
        let mut env = TestEnvironment::default();
        env.init_contract_with_snippet(
            "contract-callee",
            r#"
(define-trait test-trait ((one-simple-arg (int) (response int uint))))
(define-public (one-simple-arg (x int))
    (ok x)
)
            "#,
        )
        .expect("Failed to init contract.");
        let val = env
            .init_contract_with_snippet(
                "contract-caller",
                r#"
(use-trait test-trait .contract-callee.test-trait)
(define-private (call-it (t <test-trait>) (x int))
    (contract-call? t one-simple-arg x)
)
(call-it .contract-callee 42)
            "#,
            )
            .expect("Failed to init contract.");

        assert_eq!(val.unwrap(), Value::okay(Value::Int(42)).unwrap());
    }

    #[test]
    fn dynamic_one_arg() {
        let mut env = TestEnvironment::default();
        env.init_contract_with_snippet(
            "contract-callee",
            r#"
(define-trait test-trait ((one-arg ((string-ascii 16)) (response (string-ascii 16) uint))))
(define-public (one-arg (x (string-ascii 16)))
    (ok x)
)
            "#,
        )
        .expect("Failed to init contract.");
        let val = env
            .init_contract_with_snippet(
                "contract-caller",
                r#"
(use-trait test-trait .contract-callee.test-trait)
(define-private (call-it (t <test-trait>) (x (string-ascii 16)))
    (contract-call? t one-arg x)
)
(call-it .contract-callee "hello")
            "#,
            )
            .expect("Failed to init contract.");

        assert_eq!(
            val.unwrap(),
            Value::okay(Value::string_ascii_from_bytes("hello".to_string().into_bytes()).unwrap())
                .unwrap()
        );
    }

    #[test]
    fn dynamic_two_simple_args() {
        let mut env = TestEnvironment::default();
        env.init_contract_with_snippet(
            "contract-callee",
            r#"
(define-trait test-trait ((two-simple-args (int int) (response int uint))))
(define-public (two-simple-args (x int) (y int))
    (ok (+ x y))
)
            "#,
        )
        .expect("Failed to init contract.");
        let val = env
            .init_contract_with_snippet(
                "contract-caller",
                r#"
(use-trait test-trait .contract-callee.test-trait)
(define-private (call-it (t <test-trait>) (x int) (y int))
    (contract-call? t two-simple-args x y)
)
(call-it .contract-callee 17 42)
            "#,
            )
            .expect("Failed to init contract.");

        assert_eq!(val.unwrap(), Value::okay(Value::Int(17 + 42)).unwrap());
    }

    #[test]
    fn dynamic_two_args() {
        let mut env = TestEnvironment::default();
        env.init_contract_with_snippet(
            "contract-callee",
            r#"
(define-trait test-trait ((two-args ((string-ascii 16) (string-ascii 16)) (response (string-ascii 32) uint))))
(define-public (two-args (x (string-ascii 16)) (y (string-ascii 16)))
    (ok (concat x y))
)
            "#,
        ).expect("Failed to init contract.");
        let val = env
            .init_contract_with_snippet(
                "contract-caller",
                r#"
(use-trait test-trait .contract-callee.test-trait)
(define-private (call-it (t <test-trait>) (x (string-ascii 16)) (y (string-ascii 16)))
    (contract-call? t two-args x y)
)
(call-it .contract-callee "hello " "world")
            "#,
            )
            .expect("Failed to init contract.");

        assert_eq!(
            val.unwrap(),
            Value::okay(
                Value::string_ascii_from_bytes("hello world".to_string().into_bytes()).unwrap()
            )
            .unwrap()
        );
    }

    #[test]
    /// Call the erroring function directly and verify that the changes are
    /// rolled back.
    fn err_rollback_direct() {
        let mut env = TestEnvironment::default();
        env.init_contract_with_snippet(
            "contract-callee",
            r#"
(define-data-var my-val int 111)
(define-public (set-err (val int))
    (begin
        (var-set my-val val)
        (err u1)
    )
)
(define-read-only (get-val)
    (var-get my-val)
)
            "#,
        )
        .expect("Failed to init contract.");

        // Expect this call to return an error
        let res = env
            .init_contract_with_snippet(
                "contract-caller",
                "(contract-call? .contract-callee set-err -42)",
            )
            .expect("Failed to init contract.");
        assert_eq!(res.unwrap(), Value::err_uint(1));

        // Expect the data-var to be unchanged
        let val = env
            .init_contract_with_snippet("check-value", "(contract-call? .contract-callee get-val)")
            .expect("Failed to init contract.");
        assert_eq!(val.unwrap(), Value::Int(111));
    }

    #[test]
    /// Call the erroring function indirectly, through another contract's
    /// function which also fails, and verify that the changes are rolled back.
    fn err_rollback() {
        let mut env = TestEnvironment::default();
        env.init_contract_with_snippet(
            "contract-callee",
            r#"
(define-data-var my-val int 111)
(define-public (set-err (val int))
    (begin
        (var-set my-val val)
        (err u1)
    )
)
(define-read-only (get-val)
    (var-get my-val)
)
            "#,
        )
        .expect("Failed to init contract.");

        env.init_contract_with_snippet(
            "contract-caller",
            r#"
(define-public (call-set-err)
    (contract-call? .contract-callee set-err -42)
)
              "#,
        )
        .expect("Failed to init contract.");

        // Expect this call to return an err
        let res = env
            .init_contract_with_snippet("call-it", "(contract-call? .contract-caller call-set-err)")
            .expect("Failed to init contract.");
        assert_eq!(res.unwrap(), Value::err_uint(1));

        // Expect the data-var to be unchanged
        let val = env
            .init_contract_with_snippet("check-value", "(contract-call? .contract-callee get-val)")
            .expect("Failed to init contract.");
        assert_eq!(val.unwrap(), Value::Int(111));
    }

    #[test]
    /// Call the erroring function indirectly, through another contract's
    /// function which returns ok, but verify that the erroring functions'
    /// changes are still rolled back.
    fn err_rollback_ok() {
        let mut env = TestEnvironment::default();
        env.init_contract_with_snippet(
            "contract-callee",
            r#"
(define-data-var my-val int 111)
(define-public (set-err (val int))
    (begin
        (var-set my-val val)
        (err u1)
    )
)
(define-read-only (get-val)
    (var-get my-val)
)
            "#,
        )
        .expect("Failed to init contract.");

        env.init_contract_with_snippet(
            "contract-caller",
            r#"
(define-public (call-set-err-ok)
    (ok (unwrap-err-panic (contract-call? .contract-callee set-err -42)))
)
              "#,
        )
        .expect("Failed to init contract.");

        // Expect this call to return an okay.
        let res = env
            .init_contract_with_snippet(
                "call-it",
                "(contract-call? .contract-caller call-set-err-ok)",
            )
            .expect("Failed to init contract.");
        assert_eq!(res.unwrap(), Value::okay(Value::UInt(1)).unwrap());

        // Expect the data-var to be unchanged
        let val = env
            .init_contract_with_snippet("check-value", "(contract-call? .contract-callee get-val)")
            .expect("Failed to init contract.");
        assert_eq!(val.unwrap(), Value::Int(111));
    }

    #[test]
    /// Call the erroring function indirectly, through another contract's
    /// function which returns ok, but verify that the erroring functions'
    /// changes are still rolled back, while the ok function's changes are
    /// preserved.
    fn err_rollback_ok_preserve_changes() {
        let mut env = TestEnvironment::default();
        env.init_contract_with_snippet(
            "contract-callee",
            r#"
(define-data-var my-val int 111)
(define-public (set-err (val int))
    (begin
        (var-set my-val val)
        (err u1)
    )
)
(define-read-only (get-val)
    (var-get my-val)
)
            "#,
        )
        .expect("Failed to init contract.");

        env.init_contract_with_snippet(
            "contract-caller",
            r#"
(define-data-var my-val int 3)
(define-public (call-set-err-ok)
    (begin
        (var-set my-val 123)
        (ok (unwrap-err-panic (contract-call? .contract-callee set-err -42)))
    )
)
(define-read-only (get-val)
    (var-get my-val)
)
              "#,
        )
        .expect("Failed to init contract.");

        // Expect this call to return an okay.
        let res = env
            .init_contract_with_snippet(
                "call-it",
                "(contract-call? .contract-caller call-set-err-ok)",
            )
            .expect("Failed to init contract.");
        assert_eq!(res.unwrap(), Value::okay(Value::UInt(1)).unwrap());

        // Expect the callee data-var to be unchanged
        let val = env
            .init_contract_with_snippet("check-value", "(contract-call? .contract-callee get-val)")
            .expect("Failed to init contract.");
        assert_eq!(val.unwrap(), Value::Int(111));

        // Expect the caller data-var to be changed.
        let val = env
            .init_contract_with_snippet(
                "check-value-2",
                "(contract-call? .contract-caller get-val)",
            )
            .expect("Failed to init contract.");
        assert_eq!(val.unwrap(), Value::Int(123));
    }

    #[test]
    /// Call the erroring function via an intra-contract function call (not
    /// using `contract-call?`), and verify that the changes are rolled back.
    fn err_rollback_intra_contract_call() {
        let mut env = TestEnvironment::default();
        env.init_contract_with_snippet(
            "contract-callee",
            r#"
(define-data-var my-val int 111)
(define-public (set-err (val int))
    (begin
        (var-set my-val val)
        (err u1)
    )
)
(define-public (set-it)
    (ok (unwrap-err-panic (set-err -123)))
)
(define-read-only (get-val)
    (var-get my-val)
)
            "#,
        )
        .expect("Failed to init contract.");

        // Expect this call to return an okay.
        let res = env
            .init_contract_with_snippet(
                "contract-caller",
                "(contract-call? .contract-callee set-it)",
            )
            .expect("Failed to init contract.");
        assert_eq!(res.unwrap(), Value::okay(Value::UInt(1)).unwrap());

        // Expect the data-var to be unchanged
        let val = env
            .init_contract_with_snippet("check-value", "(contract-call? .contract-callee get-val)")
            .expect("Failed to init contract.");
        assert_eq!(val.unwrap(), Value::Int(111));
    }

    #[test]
    /// Call the erroring function via an intra-contract function call (not
    /// using `contract-call?`), and verify that the changes are rolled back
    /// because the erroring function is private.
    fn err_no_rollback_intra_contract_call() {
        let mut env = TestEnvironment::default();
        env.init_contract_with_snippet(
            "contract-callee",
            r#"
(define-data-var my-val int 111)
(define-private (set-err (val int))
    (begin
        (var-set my-val val)
        (err u1)
    )
)
(define-public (set-it)
    (ok (unwrap-err-panic (set-err -123)))
)
(define-read-only (get-val)
    (var-get my-val)
)
            "#,
        )
        .expect("Failed to init contract.");

        // Expect this call to return an okay.
        let res = env
            .init_contract_with_snippet(
                "contract-caller",
                "(contract-call? .contract-callee set-it)",
            )
            .expect("Failed to init contract.");
        assert_eq!(res.unwrap(), Value::okay(Value::UInt(1)).unwrap());

        // Expect the data-var to be unchanged
        let val = env
            .init_contract_with_snippet("check-value", "(contract-call? .contract-callee get-val)")
            .expect("Failed to init contract.");
        assert_eq!(val.unwrap(), Value::Int(-123));
    }

    // Not run in v1 because at that point traits could not be used in all the places where a built-in type could
    #[cfg(not(feature = "test-clarity-v1"))]
    #[test]
    fn multi_dynamic_define_impl_call() {
        let foo_trait = "
            (define-trait foo
                (
                    (do-it () (response bool uint))
                )
            )
            ";

        let foo_impl = "
            (impl-trait .foo.foo)

            (define-public (do-it)
                (ok true)
            )
            ";

        let call_foo = "
            (use-trait foo .foo.foo)

            (define-public (call-do-it (opt-f (optional <foo>)))
                (match opt-f
                    f (contract-call? f do-it)
                    (ok false)
                )
            )

            (call-do-it (some .foo-impl))
            ";

        crate::tools::crosscheck_multi_contract(
            &[
                (ContractName::from_literal("foo"), foo_trait),
                (ContractName::from_literal("foo-impl"), foo_impl),
                (ContractName::from_literal("call-foo"), call_foo),
            ],
            Ok(Some(Value::okay_true())),
        );
    }

    /// This is the same test as [multi_dynamic_define_impl_call], but it checks that it still works
    /// when we deal with the linked functions defined in stacks-core (duplication issue).
    // Not run in v1 because at that point traits could not be used in all the places where a built-in type could
    #[cfg(not(feature = "test-clarity-v1"))]
    #[test]
    fn multi_dynamic_define_impl_call_duplication_issue() {
        let foo_trait = "
            (define-trait foo
                (
                    (do-it () (response bool uint))
                )
            )
            ";

        let foo_impl = "
            (impl-trait .foo.foo)

            (define-public (do-it)
                (ok true)
            )
            ";

        let call_foo = "
            (use-trait foo .foo.foo)

            (define-public (call-do-it (opt-f (optional <foo>)))
                (match opt-f
                    f (contract-call? f do-it)
                    (ok false)
                )
            )
            ";

        let bar = "(contract-call? .call-foo call-do-it (some .foo-impl))";

        crate::tools::crosscheck_multi_contract(
            &[
                (ContractName::from_literal("foo"), foo_trait),
                (ContractName::from_literal("foo-impl"), foo_impl),
                (ContractName::from_literal("call-foo"), call_foo),
                (ContractName::from_literal("bar"), bar),
            ],
            Ok(Some(Value::okay_true())),
        );
    }

    #[test]
    fn contract_call_dynamic_traitreferencetype() {
        let foo = "
        (define-trait t
            ((foo () (response bool uint)))
        )

        (define-public (foo) (ok true))
    ";

        let bar = r#"
        (use-trait foo-trait .foo.t)

        (define-private (call-it (tt <foo-trait>))
            (contract-call? tt foo)
        )

        (call-it .foo)
    "#;

        crosscheck_multi_contract_with_env(
            &[
                (ContractName::from_literal("foo"), foo),
                (ContractName::from_literal("bar"), bar),
            ],
            Ok(Some(Value::okay_true())),
            TestEnvironment::new(
                clarity::types::StacksEpochId::Epoch20,
                clarity::vm::ClarityVersion::Clarity1,
            ),
        );
    }

    #[cfg(not(any(
        feature = "test-clarity-v1",
        feature = "test-clarity-v2",
        feature = "test-clarity-v3"
    )))]
    mod clarity_v4 {
        use clarity::util::hash::Sha512Trunc256Sum;
        use clarity::vm::types::PrincipalData;
        use clarity_types::types::StandardPrincipalData;
        use clarity_types::ClarityName;

        use super::*;
        use crate::tools::{crosscheck, crosscheck_multi_contract, evaluate};

        #[test]
        fn as_contract_safe_switches_sender_and_caller() {
            crosscheck(
                r#"
                    (let (
                        (original-sender tx-sender)
                        (original-caller contract-caller)
                    )
                        (try! (as-contract? ()
                            (asserts! (is-eq tx-sender current-contract)
                                (err "tx-sender was not switched to current-contract"))
                            (asserts! (is-eq contract-caller current-contract)
                                (err "contract-caller was not switched to current-contract"))
                            (asserts! (not (is-eq tx-sender original-sender))
                                (err "tx-sender still equals the original sender"))
                            (asserts! (not (is-eq contract-caller original-caller))
                                (err "contract-caller still equals the original caller"))
                        ))
                        (asserts! (is-eq tx-sender original-sender)
                            (err "tx-sender was not restored after as-contract?"))
                        (asserts! (is-eq contract-caller original-caller)
                            (err "contract-caller was not restored after as-contract?"))
                    )
                "#,
                Ok(Some(Value::Bool(true))),
            );
        }

        #[test]
        fn safe_contract_controls_return_a_value_past_the_wasm_block_limit() {
            const FIELD_COUNT: u32 = 499;
            let fields = (0..FIELD_COUNT)
                .map(|index| format!("f{index}: {index}"))
                .collect::<Vec<_>>()
                .join(", ");
            let tuple = Value::Tuple(
                clarity::vm::types::TupleData::from_data(
                    (0..FIELD_COUNT)
                        .map(|index| {
                            (
                                ClarityName::try_from(format!("f{index}"))
                                    .expect("generated field name is valid"),
                                Value::Int(index.into()),
                            )
                        })
                        .collect(),
                )
                .expect("generated tuple is valid"),
            );
            let expected = Value::okay(tuple).expect("an ok response");

            crosscheck(
                &format!("(as-contract? () {{{fields}}})"),
                Ok(Some(expected.clone())),
            );
            crosscheck(
                &format!("(restrict-assets? tx-sender () {{{fields}}})"),
                Ok(Some(expected)),
            );
        }

        #[test]
        fn contract_hash_ok_returns_buff32() {
            let callee = "
(define-read-only (something)
    (ok u1)
)";
            let caller = "(contract-hash? .callee)";

            let expected = Sha512Trunc256Sum::from_data(callee.as_bytes());

            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(
                    Value::okay(Value::buff_from(expected.0.to_vec()).unwrap()).unwrap(),
                )),
            );
        }

        #[test]
        fn contract_hash_ok_returns_buff32_with_full_addr() {
            let callee = "
(define-read-only (something)
    (ok u1)
)";
            let callee_address = StandardPrincipalData::transient().to_address();
            let caller = &format!("(contract-hash? '{}.callee)", callee_address);

            let expected = Sha512Trunc256Sum::from_data(callee.as_bytes());

            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(
                    Value::okay(Value::buff_from(expected.0.to_vec()).unwrap()).unwrap(),
                )),
            );
        }

        #[test]
        fn contract_hash_err_u1_if_not_contract_principal() {
            crosscheck(
                "(contract-hash? tx-sender)",
                Ok(Some(Value::error(Value::UInt(1)).unwrap())),
            );
        }

        #[test]
        fn contract_hash_err_u2_if_contract_missing() {
            crosscheck(
                "(contract-hash? .does-not-exist)",
                Ok(Some(Value::error(Value::UInt(2)).unwrap())),
            );
        }

        // ==================== argument count checks ====================

        #[test]
        fn with_all_assets_unsafe_too_many_args() {
            let result = evaluate("(as-contract? ((with-all-assets-unsafe u1)) (ok true))");
            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("expecting 0 arguments, got 1"));
        }

        #[test]
        fn with_stx_no_args() {
            let result = evaluate("(as-contract? ((with-stx)) (ok true))");
            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("expecting 1 arguments, got 0"));
        }

        #[test]
        fn with_stx_too_many_args() {
            let result = evaluate("(as-contract? ((with-stx u100 u200)) (ok true))");
            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("expecting 1 arguments, got 2"));
        }

        #[test]
        fn with_stacking_no_args() {
            let result = crate::tools::evaluate_at(
                "(as-contract? ((with-stacking)) (ok true))",
                clarity::types::StacksEpochId::Epoch33,
                clarity::vm::ClarityVersion::Clarity4,
            );
            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("expecting 1 arguments, got 0"));
        }

        #[test]
        fn with_stacking_too_many_args() {
            let result = crate::tools::evaluate_at(
                "(as-contract? ((with-stacking u100 u200)) (ok true))",
                clarity::types::StacksEpochId::Epoch33,
                clarity::vm::ClarityVersion::Clarity4,
            );
            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("expecting 1 arguments, got 2"));
        }

        #[cfg(not(any(feature = "test-clarity-v4", feature = "test-clarity-v5")))]
        #[test]
        fn with_pox_too_many_args() {
            let result = evaluate("(as-contract? ((with-pox u100)) (ok true))");
            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("expecting 0 arguments, got 1"));
        }

        #[test]
        fn with_ft_no_args() {
            let result = evaluate("(as-contract? ((with-ft)) (ok true))");
            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("expecting 3 arguments, got 0"));
        }

        #[test]
        fn with_ft_too_few_args() {
            let result = evaluate(r#"(as-contract? ((with-ft .contract "token")) (ok true))"#);
            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("expecting 3 arguments, got 2"));
        }

        #[test]
        fn with_ft_too_many_args() {
            let result =
                evaluate(r#"(as-contract? ((with-ft .contract "token" u100 u200)) (ok true))"#);
            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("expecting 3 arguments, got 4"));
        }

        #[test]
        fn with_nft_no_args() {
            let result = evaluate("(as-contract? ((with-nft)) (ok true))");
            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("expecting 3 arguments, got 0"));
        }

        #[test]
        fn with_nft_too_few_args() {
            let result = evaluate(r#"(as-contract? ((with-nft .contract "token")) (ok true))"#);
            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("expecting 3 arguments, got 2"));
        }

        #[test]
        fn with_nft_too_many_args() {
            let result = evaluate(
                r#"(as-contract? ((with-nft .contract "token" (list u1) u99)) (ok true))"#,
            );
            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("expecting 3 arguments, got 4"));
        }

        // ==================== with-all-assets-unsafe ====================

        #[test]
        fn as_contract_safe_unsafe_nft_transfer() {
            let callee: &str = r#"
                (define-non-fungible-token token uint)

                (define-public (mint-nft (asset uint))
                    (nft-mint? token asset current-contract)
                )

                (define-public (transfer-token (asset uint))
                    (let ((recipient tx-sender))
                        (as-contract? ((with-all-assets-unsafe))
                            (try! (nft-transfer? token asset current-contract recipient))
                        )
                    )
                )
            "#;
            let caller = "
                (contract-call? .callee mint-nft u1)
                (contract-call? .callee transfer-token u1)
            ";
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(Value::okay_true())),
            );
        }

        #[test]
        fn as_contract_safe_unsafe_stx_transfer() {
            let callee = r#"
                (define-public (send-stx (amount uint) (recipient principal))
                    (as-contract? ((with-all-assets-unsafe))
                        (try! (stx-transfer? amount current-contract recipient))
                    )
                )
            "#;
            let caller = "
                (stx-transfer? u500 tx-sender .callee)
                (contract-call? .callee send-stx u50 tx-sender)
            ";
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(Value::okay_true())),
            );
        }

        // ==================== with-stx ====================

        #[test]
        fn as_contract_safe_stx_ok() {
            let callee = r#"
                (define-public (send-stx (amount uint) (recipient principal))
                    (as-contract? ((with-stx u100))
                        (try! (stx-transfer? amount current-contract recipient))
                    )
                )
            "#;
            let caller = "
                (stx-transfer? u500 tx-sender .callee)
                (let ((result (contract-call? .callee send-stx u100 'ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM)))
                    {result: result,
                     sender-balance: (stx-get-balance .callee),
                     recipient-balance: (stx-get-balance 'ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM)}
                )
            ";
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (ClarityName::from_literal("result"), Value::okay_true()),
                    (
                        ClarityName::from_literal("sender-balance"),
                        Value::UInt(400),
                    ),
                    (
                        ClarityName::from_literal("recipient-balance"),
                        Value::UInt(100),
                    ),
                ])
                .unwrap(),
            );
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        #[test]
        fn as_contract_safe_stx_exceeds_allowance() {
            let callee = r#"
                (define-public (send-stx (amount uint) (recipient principal))
                    (as-contract? ((with-stx u10))
                        (try! (stx-transfer? amount current-contract recipient))
                    )
                )
            "#;
            let caller = "
                (stx-transfer? u500 tx-sender .callee)
                (let
                    (
                        (result (contract-call? .callee send-stx u50 tx-sender))
                    )
                    {error-code: result, balance: (stx-get-balance .callee)}
                )
            ";
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (ClarityName::from_literal("balance"), Value::UInt(500)),
                    (
                        ClarityName::from_literal("error-code"),
                        Value::error(Value::UInt(0)).unwrap(),
                    ),
                ])
                .unwrap(),
            );
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        #[test]
        fn as_contract_safe_stx_no_allowance() {
            let callee = r#"
                (define-public (send-stx (amount uint) (recipient principal))
                    (as-contract? ()
                        (try! (stx-transfer? amount current-contract recipient))
                    )
                )
            "#;
            let caller = "
                (stx-transfer? u500 tx-sender .callee)
                (let
                    (
                        (result (contract-call? .callee send-stx u50 tx-sender))
                    )
                    {error-code: result, balance: (stx-get-balance .callee)}
                )
            ";
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (ClarityName::from_literal("balance"), Value::UInt(500)),
                    (
                        ClarityName::from_literal("error-code"),
                        Value::error(Value::UInt(128)).unwrap(),
                    ),
                ])
                .unwrap(),
            );
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        // ==================== with-ft ====================

        #[test]
        fn as_contract_safe_ft_ok() {
            let callee = r#"
                (define-fungible-token my-token)

                (define-public (mint-ft (amount uint))
                    (ft-mint? my-token amount current-contract)
                )

                (define-public (transfer-ft (amount uint) (recipient principal))
                    (as-contract? ((with-ft current-contract "my-token" u100))
                        (try! (ft-transfer? my-token amount current-contract recipient))
                    )
                )

                (define-read-only (get-ft-balance (who principal))
                    (ft-get-balance my-token who)
                )
            "#;
            let caller = "
                (contract-call? .callee mint-ft u100)
                (let ((result (contract-call? .callee transfer-ft u100 tx-sender)))
                    {result: result,
                     sender-balance: (contract-call? .callee get-ft-balance .callee),
                     recipient-balance: (contract-call? .callee get-ft-balance tx-sender)}
                )
            ";
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (ClarityName::from_literal("result"), Value::okay_true()),
                    (ClarityName::from_literal("sender-balance"), Value::UInt(0)),
                    (
                        ClarityName::from_literal("recipient-balance"),
                        Value::UInt(100),
                    ),
                ])
                .unwrap(),
            );
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        #[test]
        fn as_contract_safe_ft_exceeds_allowance() {
            let callee = r#"
                (define-fungible-token my-token)

                (define-public (mint-ft (amount uint))
                    (ft-mint? my-token amount current-contract)
                )

                (define-public (transfer-ft (amount uint) (recipient principal))
                    (as-contract? ((with-ft current-contract "my-token" u10))
                        (try! (ft-transfer? my-token amount current-contract recipient))
                    )
                )

                (define-read-only (get-ft-balance)
                    (ft-get-balance my-token current-contract)
                )
            "#;
            let caller = "
                (contract-call? .callee mint-ft u100)
                (let
                    (
                        (result (contract-call? .callee transfer-ft u50 tx-sender))
                    )
                    {error-code: result, balance: (contract-call? .callee get-ft-balance)}
                )
            ";
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (ClarityName::from_literal("balance"), Value::UInt(100)),
                    (
                        ClarityName::from_literal("error-code"),
                        Value::error(Value::UInt(0)).unwrap(),
                    ),
                ])
                .unwrap(),
            );
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        #[test]
        fn as_contract_safe_ft_no_allowance() {
            let callee = r#"
                (define-fungible-token my-token)

                (define-public (mint-ft (amount uint))
                    (ft-mint? my-token amount current-contract)
                )

                (define-public (transfer-ft (amount uint) (recipient principal))
                    (as-contract? ()
                        (try! (ft-transfer? my-token amount current-contract recipient))
                    )
                )

                (define-read-only (get-ft-balance)
                    (ft-get-balance my-token current-contract)
                )
            "#;
            let caller = "
                (contract-call? .callee mint-ft u100)
                (let
                    (
                        (result (contract-call? .callee transfer-ft u50 tx-sender))
                    )
                    {error-code: result, balance: (contract-call? .callee get-ft-balance)}
                )
            ";
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (ClarityName::from_literal("balance"), Value::UInt(100)),
                    (
                        ClarityName::from_literal("error-code"),
                        Value::error(Value::UInt(128)).unwrap(),
                    ),
                ])
                .unwrap(),
            );
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        // ==================== with-ft wildcard ====================

        #[test]
        fn as_contract_safe_ft_wildcard_ok() {
            let callee = r#"
                (define-fungible-token my-token)

                (define-public (mint-ft (amount uint))
                    (ft-mint? my-token amount current-contract)
                )

                (define-public (transfer-ft (amount uint) (recipient principal))
                    (as-contract? ((with-ft current-contract "*" u100))
                        (try! (ft-transfer? my-token amount current-contract recipient))
                    )
                )
            "#;
            let caller = "
                (contract-call? .callee mint-ft u100)
                (contract-call? .callee transfer-ft u100 tx-sender)
            ";
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(Value::okay_true())),
            );
        }

        #[test]
        fn as_contract_safe_ft_wildcard_exceeds() {
            let callee = r#"
                (define-fungible-token my-token)

                (define-public (mint-ft (amount uint))
                    (ft-mint? my-token amount current-contract)
                )

                (define-public (transfer-ft (amount uint) (recipient principal))
                    (as-contract? ((with-ft current-contract "*" u10))
                        (try! (ft-transfer? my-token amount current-contract recipient))
                    )
                )

                (define-read-only (get-ft-balance)
                    (ft-get-balance my-token current-contract)
                )
            "#;
            let caller = "
                (contract-call? .callee mint-ft u100)
                (let
                    (
                        (result (contract-call? .callee transfer-ft u50 tx-sender))
                    )
                    {error-code: result, balance: (contract-call? .callee get-ft-balance)}
                )
            ";
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (ClarityName::from_literal("balance"), Value::UInt(100)),
                    (
                        ClarityName::from_literal("error-code"),
                        Value::error(Value::UInt(0)).unwrap(),
                    ),
                ])
                .unwrap(),
            );
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        #[test]
        fn as_contract_safe_ft_wildcard_with_exact() {
            let callee = r#"
                (define-fungible-token my-token)

                (define-public (mint-ft (amount uint))
                    (ft-mint? my-token amount current-contract)
                )

                (define-public (transfer-ft (amount uint) (recipient principal))
                    (as-contract? (
                            (with-ft current-contract "*" u100)
                            (with-ft current-contract "my-token" u100)
                        )
                        (try! (ft-transfer? my-token amount current-contract recipient))
                    )
                )
            "#;
            let caller = "
                (contract-call? .callee mint-ft u100)
                (contract-call? .callee transfer-ft u50 tx-sender)
            ";
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(Value::okay_true())),
            );
        }

        #[test]
        fn as_contract_safe_ft_wildcard_with_exact_first_violated() {
            let callee = r#"
                (define-fungible-token my-token)

                (define-public (mint-ft (amount uint))
                    (ft-mint? my-token amount current-contract)
                )

                (define-public (transfer-ft (amount uint) (recipient principal))
                    (as-contract? (
                            (with-ft current-contract "*" u20)
                            (with-ft current-contract "my-token" u100)
                        )
                        (try! (ft-transfer? my-token amount current-contract recipient))
                    )
                )

                (define-read-only (get-ft-balance)
                    (ft-get-balance my-token current-contract)
                )
            "#;
            let caller = "
                (contract-call? .callee mint-ft u100)
                (let
                    (
                        (result (contract-call? .callee transfer-ft u50 tx-sender))
                    )
                    {error-code: result, balance: (contract-call? .callee get-ft-balance)}
                )
            ";
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (ClarityName::from_literal("balance"), Value::UInt(100)),
                    (
                        ClarityName::from_literal("error-code"),
                        Value::error(Value::UInt(0)).unwrap(),
                    ),
                ])
                .unwrap(),
            );
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        // ==================== with-nft ====================

        #[test]
        fn as_contract_safe_nft_ok() {
            let callee = r#"
                (define-non-fungible-token token uint)

                (define-public (mint-nft (asset uint))
                    (nft-mint? token asset current-contract)
                )

                (define-public (transfer-token (asset uint))
                    (let ((recipient tx-sender))
                        (as-contract? ((with-nft current-contract "token" (list u1)))
                            (try! (nft-transfer? token asset current-contract recipient))
                        )
                    )
                )

                (define-read-only (get-nft-owner (asset uint))
                    (nft-get-owner? token asset)
                )
            "#;
            let caller = "
                (contract-call? .callee mint-nft u1)
                (let ((result (contract-call? .callee transfer-token u1)))
                    {
                        result: result, 
                        owner: (contract-call? .callee get-nft-owner u1)
                    }
                )
            ";
            let tx_sender =
                Value::Principal(PrincipalData::Standard(StandardPrincipalData::transient()));
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (ClarityName::from_literal("result"), Value::okay_true()),
                    (
                        ClarityName::from_literal("owner"),
                        Value::some(tx_sender).unwrap(),
                    ),
                ])
                .unwrap(),
            );
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        #[test]
        fn as_contract_safe_nft_wrong_id() {
            let callee = r#"
                (define-non-fungible-token token uint)

                (define-public (mint-nft (asset uint))
                    (nft-mint? token asset current-contract)
                )

                (define-public (transfer-token (asset uint))
                    (let ((recipient tx-sender))
                        (as-contract? ((with-nft current-contract "token" (list u999)))
                            (try! (nft-transfer? token asset current-contract recipient))
                        )
                    )
                )

                (define-read-only (get-nft-owner (asset uint))
                    (nft-get-owner? token asset)
                )
            "#;
            let caller = "
                (contract-call? .callee mint-nft u1)
                (let ((result (contract-call? .callee transfer-token u1)))
                    {error-code: result, owner: (contract-call? .callee get-nft-owner u1)}
                )
            ";
            let callee_principal = Value::Principal(PrincipalData::Contract(
                clarity::vm::types::QualifiedContractIdentifier::new(
                    StandardPrincipalData::transient(),
                    ContractName::from_literal("callee"),
                ),
            ));
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (
                        ClarityName::from_literal("error-code"),
                        Value::error(Value::UInt(0)).unwrap(),
                    ),
                    (
                        ClarityName::from_literal("owner"),
                        Value::some(callee_principal).unwrap(),
                    ),
                ])
                .unwrap(),
            );
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        #[test]
        fn as_contract_safe_nft_no_allowance() {
            let callee = r#"
                (define-non-fungible-token token uint)

                (define-public (mint-nft (asset uint))
                    (nft-mint? token asset current-contract)
                )

                (define-public (transfer-token (asset uint))
                    (let ((recipient tx-sender))
                        (as-contract? ()
                            (try! (nft-transfer? token asset current-contract recipient))
                        )
                    )
                )

                (define-read-only (get-nft-owner (asset uint))
                    (nft-get-owner? token asset)
                )
            "#;
            let caller = "
                (contract-call? .callee mint-nft u1)
                (let ((result (contract-call? .callee transfer-token u1)))
                    {error-code: result, owner: (contract-call? .callee get-nft-owner u1)}
                )
            ";
            let callee_principal = Value::Principal(PrincipalData::Contract(
                clarity::vm::types::QualifiedContractIdentifier::new(
                    StandardPrincipalData::transient(),
                    ContractName::from_literal("callee"),
                ),
            ));
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (
                        ClarityName::from_literal("error-code"),
                        Value::error(Value::UInt(128)).unwrap(),
                    ),
                    (
                        ClarityName::from_literal("owner"),
                        Value::some(callee_principal).unwrap(),
                    ),
                ])
                .unwrap(),
            );
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        // ==================== with-nft wildcard ====================

        #[test]
        fn as_contract_safe_nft_wildcard_ok() {
            let callee = r#"
                (define-non-fungible-token token uint)

                (define-public (mint-nft (asset uint))
                    (nft-mint? token asset current-contract)
                )

                (define-public (transfer-token (asset uint))
                    (let ((recipient tx-sender))
                        (as-contract? ((with-nft current-contract "*" (list u1)))
                            (try! (nft-transfer? token asset current-contract recipient))
                        )
                    )
                )
            "#;
            let caller = "
                (contract-call? .callee mint-nft u1)
                (contract-call? .callee transfer-token u1)
            ";
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(Value::okay_true())),
            );
        }

        #[test]
        fn as_contract_safe_nft_wildcard_wrong_id() {
            let callee = r#"
                (define-non-fungible-token token uint)

                (define-public (mint-nft (asset uint))
                    (nft-mint? token asset current-contract)
                )

                (define-public (transfer-token (asset uint))
                    (let ((recipient tx-sender))
                        (as-contract? ((with-nft current-contract "*" (list u999)))
                            (try! (nft-transfer? token asset current-contract recipient))
                        )
                    )
                )

                (define-read-only (get-nft-owner (asset uint))
                    (nft-get-owner? token asset)
                )
            "#;
            let caller = "
                (contract-call? .callee mint-nft u1)
                (let ((result (contract-call? .callee transfer-token u1)))
                    {error-code: result, owner: (contract-call? .callee get-nft-owner u1)}
                )
            ";
            let callee_principal = Value::Principal(PrincipalData::Contract(
                clarity::vm::types::QualifiedContractIdentifier::new(
                    StandardPrincipalData::transient(),
                    ContractName::from_literal("callee"),
                ),
            ));
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (
                        ClarityName::from_literal("error-code"),
                        Value::error(Value::UInt(0)).unwrap(),
                    ),
                    (
                        ClarityName::from_literal("owner"),
                        Value::some(callee_principal).unwrap(),
                    ),
                ])
                .unwrap(),
            );
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        // ==================== with-stacking ====================

        #[test]
        fn as_contract_safe_stacking_ok() {
            let pox4_code =
                std::fs::read_to_string("tests/contracts/boot-contracts/pox-4.clar").unwrap();
            let wrapper = r#"
                (define-public (do-delegate (amount uint) (delegate-to principal))
                    (as-contract? ((with-stacking u1000000))
                        (unwrap-panic (contract-call? .pox-4 delegate-stx
                            amount delegate-to none none))
                    )
                )
            "#;
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("pox-4"), &pox4_code),
                    (ContractName::from_literal("wrapper"), wrapper),
                    (
                        ContractName::from_literal("test"),
                        "(contract-call? .wrapper do-delegate u1000 tx-sender)",
                    ),
                ],
                Ok(Some(Value::okay_true())),
            );
        }

        #[test]
        fn as_contract_safe_stacking_pox_indirect() {
            let pox4_code =
                std::fs::read_to_string("tests/contracts/boot-contracts/pox-4.clar").unwrap();
            let intermediary = r#"
                (define-public (do-delegate (amount uint) (delegate-to principal))
                    (contract-call? .pox-4 delegate-stx amount delegate-to none none)
                )
            "#;
            // setup-allowance grants the intermediary permission to call pox-4
            // on behalf of the wrapper (as-contract? changes tx-sender to wrapper)
            let wrapper = r#"
                (define-public (setup-allowance)
                    (as-contract? ((with-all-assets-unsafe))
                        (unwrap-panic (contract-call? .pox-4 allow-contract-caller .intermediary none))
                    )
                )

                (define-public (delegate-via-intermediary (amount uint) (delegate-to principal))
                    (as-contract? ((with-stacking u1000000))
                        (unwrap-panic (contract-call? .intermediary do-delegate
                            amount delegate-to))
                    )
                )
            "#;
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("pox-4"), &pox4_code),
                    (ContractName::from_literal("intermediary"), intermediary),
                    (ContractName::from_literal("wrapper"), wrapper),
                    (
                        ContractName::from_literal("test"),
                        "(contract-call? .wrapper setup-allowance)
                (contract-call? .wrapper delegate-via-intermediary u1000 tx-sender)",
                    ),
                ],
                Ok(Some(Value::okay_true())),
            );
        }

        #[test]
        fn as_contract_safe_stacking_and_stx_pox() {
            let pox4_code =
                std::fs::read_to_string("tests/contracts/boot-contracts/pox-4.clar").unwrap();
            let wrapper = r#"
                (define-public (delegate-and-send-stx (delegate-amount uint) (stx-amount uint) (recipient principal))
                    (as-contract? ((with-stacking u1000000) (with-stx u500))
                        (unwrap-panic (contract-call? .pox-4 delegate-stx
                            delegate-amount recipient none none))
                        (try! (stx-transfer? stx-amount current-contract recipient))
                    )
                )
            "#;
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("pox-4"), &pox4_code),
                    (ContractName::from_literal("wrapper"), wrapper),
                    (
                        ContractName::from_literal("test"),
                        "
                            (stx-transfer? u1000 tx-sender .wrapper)
                            (contract-call? .wrapper delegate-and-send-stx u5000 u200 tx-sender)
                        ",
                    ),
                ],
                Ok(Some(Value::okay_true())),
            );
        }

        // ==================== with-pox ====================

        // ==================== mixed / multiple allowances ====================

        #[test]
        fn as_contract_safe_wrong_allowance_type() {
            let callee = r#"
                (define-fungible-token token)

                (define-public (send-stx (amount uint) (recipient principal))
                    (as-contract? ((with-ft current-contract "token" u100))
                        (try! (stx-transfer? amount current-contract recipient))
                    )
                )
            "#;
            let caller = "
                (stx-transfer? u500 tx-sender .callee)
                (let ((result (contract-call? .callee send-stx u50 tx-sender)))
                    {error-code: result, balance: (stx-get-balance .callee)}
                )
            ";
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (ClarityName::from_literal("balance"), Value::UInt(500)),
                    (
                        ClarityName::from_literal("error-code"),
                        Value::error(Value::UInt(128)).unwrap(),
                    ),
                ])
                .unwrap(),
            );
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        #[test]
        fn as_contract_safe_multiple_stx_second_violation() {
            let callee = r#"
                (define-public (send-stx (amount uint) (recipient principal))
                    (as-contract? ((with-stx u100) (with-stx u20))
                        (try! (stx-transfer? amount current-contract recipient))
                    )
                )
            "#;
            let caller = "
                (stx-transfer? u500 tx-sender .callee)
                (let ((result (contract-call? .callee send-stx u40 tx-sender)))
                    {error-code: result, balance: (stx-get-balance .callee)}
                )
            ";
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (ClarityName::from_literal("balance"), Value::UInt(500)),
                    (
                        ClarityName::from_literal("error-code"),
                        Value::error(Value::UInt(1)).unwrap(),
                    ),
                ])
                .unwrap(),
            );
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        #[test]
        fn as_contract_safe_mixed_stx_ft_nft() {
            let callee = r#"
                (define-fungible-token my-token)
                (define-non-fungible-token my-nft uint)

                (define-public (mint-ft (amount uint))
                    (ft-mint? my-token amount current-contract)
                )

                (define-public (mint-nft (asset uint))
                    (nft-mint? my-nft asset current-contract)
                )

                (define-public (transfer-all (ft-amount uint) (nft-id uint) (stx-amount uint))
                    (let ((recipient tx-sender))
                        (as-contract?
                            (
                                (with-stx u500)
                                (with-ft current-contract "my-token" u200)
                                (with-nft current-contract "my-nft" (list u1 u2))
                            )
                            (try! (stx-transfer? stx-amount current-contract recipient))
                            (try! (ft-transfer? my-token ft-amount current-contract recipient))
                            (try! (nft-transfer? my-nft nft-id current-contract recipient))
                        )
                    )
                )
            "#;
            let caller = "
                (stx-transfer? u1000 tx-sender .callee)
                (contract-call? .callee mint-ft u500)
                (contract-call? .callee mint-nft u1)
                (contract-call? .callee transfer-all u100 u1 u200)
            ";
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(Value::okay_true())),
            );
        }

        // ==================== nested as-contract? ====================

        #[test]
        fn as_contract_safe_nested_unsafe_outer_nft_inner() {
            let callee = r#"
                (define-non-fungible-token token uint)

                (define-public (mint-nft (asset uint))
                    (nft-mint? token asset current-contract)
                )

                (define-public (transfer-token (asset uint))
                    (let ((recipient tx-sender))
                        (as-contract? ((with-all-assets-unsafe))
                            (try!
                                (as-contract? ((with-nft current-contract "token" (list u1)))
                                    (try! (nft-transfer? token asset current-contract recipient))
                                )
                            )
                        )
                    )
                )
            "#;
            let caller = "
                (contract-call? .callee mint-nft u1)
                (contract-call? .callee transfer-token u1)
            ";
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(Value::okay_true())),
            );
        }

        #[test]
        fn as_contract_safe_nested_inner_nft_violation() {
            let callee = r#"
                (define-non-fungible-token token uint)

                (define-public (mint-nft (asset uint))
                    (nft-mint? token asset current-contract)
                )

                (define-public (transfer-token (asset uint))
                    (let ((recipient tx-sender))
                        (as-contract? ((with-all-assets-unsafe))
                            (try!
                                (as-contract? ((with-nft current-contract "token" (list u999)))
                                    (try! (nft-transfer? token asset current-contract recipient))
                                )
                            )
                        )
                    )
                )
            "#;
            let caller = "
                (contract-call? .callee mint-nft u1)
                (contract-call? .callee transfer-token u1)
            ";
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(Value::err_uint(0))),
            );
        }

        #[test]
        fn as_contract_safe_nested_cross_contract() {
            let callee = r#"
                (define-non-fungible-token token uint)

                (define-public (mint-nft (asset uint))
                    (nft-mint? token asset current-contract)
                )

                (define-public (transfer-nft (asset uint) (recipient principal))
                    (nft-transfer? token asset current-contract recipient)
                )
            "#;
            let caller = r#"
                (define-public (do-transfer (asset uint))
                    (let ((recipient tx-sender))
                        (as-contract? ((with-all-assets-unsafe))
                            (try! (contract-call? .callee transfer-nft asset recipient))
                        )
                    )
                )
            "#;
            let test = "
                (contract-call? .callee mint-nft u1)
                (contract-call? .caller do-transfer u1)
            ";
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                    (ContractName::from_literal("test"), test),
                ],
                Ok(Some(Value::okay_true())),
            );
        }

        #[test]
        fn as_contract_safe_nested_stx_outer_ft_inner() {
            let callee = r#"
                (define-fungible-token my-token)

                (define-public (mint-ft (amount uint))
                    (ft-mint? my-token amount current-contract)
                )

                (define-public (transfer-both (stx-amount uint) (ft-amount uint) (recipient principal))
                    (as-contract? ((with-stx u200) (with-ft current-contract "my-token" u100))
                        (try! (stx-transfer? stx-amount current-contract recipient))
                        (try!
                            (as-contract? ((with-ft current-contract "my-token" u100))
                                (try! (ft-transfer? my-token ft-amount current-contract recipient))
                            )
                        )
                    )
                )

                (define-read-only (get-ft-balance (who principal))
                    (ft-get-balance my-token who)
                )
            "#;
            let caller = "
                (stx-transfer? u500 tx-sender .callee)
                (contract-call? .callee mint-ft u200)
                (let ((result (contract-call? .callee transfer-both u100 u50 'ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM)))
                    {
                        result: result,
                        sender-stx-balance: (stx-get-balance .callee),
                        sender-ft-balance: (contract-call? .callee get-ft-balance .callee),
                        recipient-stx-balance: (stx-get-balance 'ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM),
                        recipient-ft-balance: (contract-call? .callee get-ft-balance 'ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM)
                    }
                )
            ";
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (ClarityName::from_literal("result"), Value::okay_true()),
                    (
                        ClarityName::from_literal("sender-stx-balance"),
                        Value::UInt(400),
                    ),
                    (
                        ClarityName::from_literal("sender-ft-balance"),
                        Value::UInt(150),
                    ),
                    (
                        ClarityName::from_literal("recipient-stx-balance"),
                        Value::UInt(100),
                    ),
                    (
                        ClarityName::from_literal("recipient-ft-balance"),
                        Value::UInt(50),
                    ),
                ])
                .unwrap(),
            );
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        #[test]
        fn as_contract_safe_nested_inner_ft_violation_rollback() {
            let callee = r#"
                (define-fungible-token my-token)

                (define-public (mint-ft (amount uint))
                    (ft-mint? my-token amount current-contract)
                )

                (define-public (transfer-both (stx-amount uint) (ft-amount uint) (recipient principal))
                    (begin
                        (as-contract? ((with-stx u200))
                            (begin
                                (try! (stx-transfer? stx-amount current-contract recipient))
                                (try!
                                    (as-contract? ((with-ft current-contract "my-token" u10))
                                        (try! (ft-transfer? my-token ft-amount current-contract recipient))
                                    )
                                )
                            )
                        )
                    )
                )

                (define-read-only (get-ft-balance)
                    (ft-get-balance my-token current-contract)
                )
            "#;
            let caller = "
                (stx-transfer? u500 tx-sender .callee)
                (contract-call? .callee mint-ft u200)
                (let ((result (contract-call? .callee transfer-both u100 u50 tx-sender)))
                    {error-code: result,
                     stx-balance: (stx-get-balance .callee),
                     ft-balance: (contract-call? .callee get-ft-balance)}
                )
            ";
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (
                        ClarityName::from_literal("error-code"),
                        Value::error(Value::UInt(0)).unwrap(),
                    ),
                    (ClarityName::from_literal("stx-balance"), Value::UInt(500)),
                    (ClarityName::from_literal("ft-balance"), Value::UInt(200)),
                ])
                .unwrap(),
            );
            // Inner FT allowance u10 is too low for u50 transfer.
            // The inner violation (err u0) propagates via try!, causing full rollback.
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        #[test]
        fn as_contract_safe_nested_nft_outer_stx_inner() {
            let callee = r#"
                (define-non-fungible-token token uint)

                (define-public (mint-nft (asset uint))
                    (nft-mint? token asset current-contract)
                )

                (define-public (transfer-nft-and-stx (asset uint) (stx-amount uint))
                    (let ((recipient tx-sender))
                        (as-contract? ((with-nft current-contract "token" (list u1)) (with-stx u200))
                            (try! (nft-transfer? token asset current-contract recipient))
                            (try!
                                (as-contract? ((with-stx u200))
                                    (try! (stx-transfer? stx-amount current-contract recipient))
                                )
                            )
                        )
                    )
                )
            "#;
            let caller = "
                (stx-transfer? u500 tx-sender .callee)
                (contract-call? .callee mint-nft u1)
                (contract-call? .callee transfer-nft-and-stx u1 u100)
            ";
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(Value::okay_true())),
            );
        }

        // ==================== restrict-assets? ====================

        #[test]
        fn restrict_assets_no_args() {
            let result = evaluate("(restrict-assets?)");
            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("expecting >= 3 arguments, got 0"));
        }

        #[test]
        fn restrict_assets_one_arg() {
            let result = evaluate("(restrict-assets? tx-sender)");
            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("expecting >= 3 arguments, got 1"));
        }

        #[test]
        fn restrict_assets_two_args() {
            let result = evaluate("(restrict-assets? tx-sender ())");
            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("expecting >= 3 arguments, got 2"));
        }

        #[test]
        fn restrict_assets_no_transfer_no_allowance_returns_ok_value() {
            // Body does not touch any asset and final expr returns u3.
            // restrict-assets? should wrap the result as (ok u3).
            crosscheck(
                "(restrict-assets? tx-sender () (+ u1 u2))",
                Ok(Some(Value::okay(Value::UInt(3)).unwrap())),
            );
        }

        #[test]
        fn restrict_assets_returns_final_body_value() {
            // Last body expression value is what's wrapped in (ok ...).
            crosscheck(
                "(restrict-assets? tx-sender () u1 u2 u42)",
                Ok(Some(Value::okay(Value::UInt(42)).unwrap())),
            );
        }

        // ---------- with-pox ----------

        // ---------- with-stx ----------

        #[test]
        fn restrict_assets_stx_ok() {
            let callee = r#"
                (define-public (send-stx (amount uint) (recipient principal))
                    (restrict-assets? tx-sender ((with-stx u100))
                        (try! (stx-transfer? amount tx-sender recipient))
                    )
                )
            "#;
            let caller = "(contract-call? .callee send-stx u100 .callee)";
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(Value::okay_true())),
            );
        }

        #[test]
        fn restrict_assets_stx_exceeds_allowance() {
            let callee = r#"
                (define-public (send-stx (amount uint) (recipient principal))
                    (restrict-assets? tx-sender ((with-stx u10))
                        (try! (stx-transfer? amount tx-sender recipient))
                    )
                )

                (define-read-only (callee-balance)
                    (stx-get-balance .callee)
                )
            "#;
            let caller = "
                (let ((result (contract-call? .callee send-stx u50 .callee)))
                    {error-code: result, callee-balance: (contract-call? .callee callee-balance)}
                )
            ";
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (ClarityName::from_literal("callee-balance"), Value::UInt(0)),
                    (
                        ClarityName::from_literal("error-code"),
                        Value::error(Value::UInt(0)).unwrap(),
                    ),
                ])
                .unwrap(),
            );
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        #[test]
        fn restrict_assets_stx_no_allowance() {
            // No allowance granted but the body transfers STX from
            // asset-owner — expect (err u128).
            let callee = r#"
                (define-public (send-stx (amount uint) (recipient principal))
                    (restrict-assets? tx-sender ()
                        (try! (stx-transfer? amount tx-sender recipient))
                    )
                )

                (define-read-only (callee-balance)
                    (stx-get-balance .callee)
                )
            "#;
            let caller = "
                (let ((result (contract-call? .callee send-stx u50 .callee)))
                    {error-code: result, callee-balance: (contract-call? .callee callee-balance)}
                )
            ";
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (ClarityName::from_literal("callee-balance"), Value::UInt(0)),
                    (
                        ClarityName::from_literal("error-code"),
                        Value::error(Value::UInt(128)).unwrap(),
                    ),
                ])
                .unwrap(),
            );
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        // ---------- mixed / multiple allowances ----------

        #[test]
        fn restrict_assets_wrong_allowance_type() {
            // An FT allowance does not authorize STX outflow → (err u128).
            let callee = r#"
                (define-fungible-token token)

                (define-public (send-stx (amount uint) (recipient principal))
                    (restrict-assets? tx-sender ((with-ft current-contract "token" u100))
                        (try! (stx-transfer? amount tx-sender recipient))
                    )
                )

                (define-read-only (callee-balance)
                    (stx-get-balance .callee)
                )
            "#;
            let caller = "
                (let ((result (contract-call? .callee send-stx u50 .callee)))
                    {error-code: result, callee-balance: (contract-call? .callee callee-balance)}
                )
            ";
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (ClarityName::from_literal("callee-balance"), Value::UInt(0)),
                    (
                        ClarityName::from_literal("error-code"),
                        Value::error(Value::UInt(128)).unwrap(),
                    ),
                ])
                .unwrap(),
            );
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        #[test]
        fn restrict_assets_multiple_stx_second_violation() {
            // Two stx allowances; first is large enough, second is not →
            // (err u1) for the 0-based index of the second allowance.
            let callee = r#"
                (define-public (send-stx (amount uint) (recipient principal))
                    (restrict-assets? tx-sender ((with-stx u100) (with-stx u20))
                        (try! (stx-transfer? amount tx-sender recipient))
                    )
                )

                (define-read-only (callee-balance)
                    (stx-get-balance .callee)
                )
            "#;
            let caller = "
                (let ((result (contract-call? .callee send-stx u40 .callee)))
                    {error-code: result, callee-balance: (contract-call? .callee callee-balance)}
                )
            ";
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (ClarityName::from_literal("callee-balance"), Value::UInt(0)),
                    (
                        ClarityName::from_literal("error-code"),
                        Value::error(Value::UInt(1)).unwrap(),
                    ),
                ])
                .unwrap(),
            );
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        #[test]
        fn restrict_assets_mixed_stx_ft_nft() {
            let callee = r#"
                (define-fungible-token my-token)
                (define-non-fungible-token my-nft uint)

                (define-public (mint-ft (amount uint))
                    (ft-mint? my-token amount tx-sender)
                )

                (define-public (mint-nft (asset uint))
                    (nft-mint? my-nft asset tx-sender)
                )

                (define-public (transfer-all (ft-amount uint) (nft-id uint) (stx-amount uint) (recipient principal))
                    (restrict-assets? tx-sender
                        (
                            (with-stx u500)
                            (with-ft current-contract "my-token" u200)
                            (with-nft current-contract "my-nft" (list u1 u2))
                        )
                        (try! (stx-transfer? stx-amount tx-sender recipient))
                        (try! (ft-transfer? my-token ft-amount tx-sender recipient))
                        (try! (nft-transfer? my-nft nft-id tx-sender recipient))
                    )
                )
            "#;
            let caller = "
                (contract-call? .callee mint-ft u500)
                (contract-call? .callee mint-nft u1)
                (contract-call? .callee transfer-all u100 u1 u200 .callee)
            ";
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(Value::okay_true())),
            );
        }

        // ---------- nested restrict-assets? ----------

        #[test]
        fn restrict_assets_nested_inner_violation_rolls_back() {
            // Inner allowance violation must propagate via try! and roll
            // back the outer STX transfer.
            let callee = r#"
                (define-fungible-token my-token)

                (define-public (mint-ft (amount uint))
                    (ft-mint? my-token amount tx-sender)
                )

                (define-public (transfer-both (stx-amount uint) (ft-amount uint) (recipient principal))
                    (restrict-assets? tx-sender ((with-stx u200))
                        (try! (stx-transfer? stx-amount tx-sender recipient))
                        (try!
                            (restrict-assets? tx-sender ((with-ft current-contract "my-token" u10))
                                (try! (ft-transfer? my-token ft-amount tx-sender recipient))
                            )
                        )
                    )
                )

                (define-read-only (callee-balance)
                    (stx-get-balance .callee)
                )

                (define-read-only (sender-ft-balance (who principal))
                    (ft-get-balance my-token who)
                )
            "#;
            let caller = "
                (contract-call? .callee mint-ft u200)
                (let ((result (contract-call? .callee transfer-both u100 u50 .callee)))
                    {error-code: result,
                     callee-stx: (contract-call? .callee callee-balance),
                     sender-ft: (contract-call? .callee sender-ft-balance tx-sender)}
                )
            ";
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (ClarityName::from_literal("callee-stx"), Value::UInt(0)),
                    (
                        ClarityName::from_literal("error-code"),
                        Value::error(Value::UInt(0)).unwrap(),
                    ),
                    (ClarityName::from_literal("sender-ft"), Value::UInt(200)),
                ])
                .unwrap(),
            );
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        #[test]
        fn restrict_assets_nested_outer_stx_inner_stx_both_ok() {
            // Both allowances are respected; outer + inner outflow each
            // satisfy their own limits → (ok true).
            let callee = r#"
                (define-public (send-twice (a uint) (b uint) (recipient principal))
                    (restrict-assets? tx-sender ((with-stx u200))
                        (try! (stx-transfer? a tx-sender recipient))
                        (try!
                            (restrict-assets? tx-sender ((with-stx u100))
                                (try! (stx-transfer? b tx-sender recipient))
                            )
                        )
                    )
                )
            "#;
            let caller = "(contract-call? .callee send-twice u50 u30 .callee)";
            crosscheck_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(Value::okay_true())),
            );
        }
    }

    /// A constant naming a contract as a `contract-call?` dispatch target.
    ///
    /// `special_contract_call` accepts one only when three things hold
    /// (`clarity/src/vm/functions/database.rs:100`): the Clarity version
    /// `supports_callables()`, the *executing* epoch
    /// `supports_call_with_constant()`, and `!contract_context.is_deploying`.
    /// Miss any one and the atom is neither a callable constant nor a callable
    /// variable, and the call ends as `ContractCallExpectName`.
    ///
    /// None of the three can be settled where the module is built. A contract
    /// keeps the version and the analysis it was published with while the chain
    /// moves under it — the same reason `at-block` is checked twice — and the same
    /// compiled function body runs once during the deploy and any number of times
    /// after it. clar2wasm had no counterpart at all and dispatched in every case,
    /// which is a *state root* divergence and not only a receipt one: the
    /// reference's refused deploy publishes nothing.
    ///
    /// `initialize_contract` now brackets `.top-level` with the flag as
    /// `Contract::initialize_from_ast` does, and the compiler emits
    /// `check_constant_call_target` ahead of the host call — after the arguments,
    /// where the reference decides it. A literal `.callee` and a `let`- or
    /// parameter-bound callable reach other branches of the same word and are
    /// left ungated, exactly as the reference leaves them.
    /// The allowance words that exist from Clarity 6, which the module above
    /// cannot reach: it runs at Clarity 4, where `with-pox` and `with-staking`
    /// are not yet functions and `with-stacking` still is.
    mod clarity_v6_allowances {
        use clarity::vm::errors::VmExecutionError;
        use clarity::vm::{ClarityVersion, Value};
        use clarity_types::ClarityName;
        use stacks_common::types::StacksEpochId;

        use super::*;
        use crate::tools::{crosscheck_multi_contract_with_env, TestEnvironment};

        fn clarity6_multi_contract(
            contracts: &[(ContractName, &str)],
            expected: Result<Option<Value>, VmExecutionError>,
        ) {
            crosscheck_multi_contract_with_env(
                contracts,
                expected,
                TestEnvironment::new(StacksEpochId::Epoch40, ClarityVersion::Clarity6),
            );
        }

        #[test]
        fn as_contract_safe_pox_ok() {
            let callee = r#"
                (define-public (do-nothing)
                    (as-contract? ((with-pox))
                        true
                    )
                )
            "#;
            let caller = "(contract-call? .callee do-nothing)";
            clarity6_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(Value::okay_true())),
            );
        }

        #[test]
        fn as_contract_safe_pox_does_not_allow_stx() {
            let callee = r#"
                (define-public (send-stx (amount uint) (recipient principal))
                    (as-contract? ((with-pox))
                        (try! (stx-transfer? amount current-contract recipient))
                    )
                )
            "#;
            let caller = "
                (stx-transfer? u500 tx-sender .callee)
                (let ((result (contract-call? .callee send-stx u50 tx-sender)))
                    {error-code: result, balance: (stx-get-balance .callee)}
                )
            ";
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (ClarityName::from_literal("balance"), Value::UInt(500)),
                    (
                        ClarityName::from_literal("error-code"),
                        Value::error(Value::UInt(128)).unwrap(),
                    ),
                ])
                .unwrap(),
            );
            clarity6_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        #[test]
        fn as_contract_safe_pox_and_stx_ok() {
            let callee = r#"
                (define-public (send-stx (amount uint) (recipient principal))
                    (as-contract? ((with-pox) (with-stx u100))
                        (try! (stx-transfer? amount current-contract recipient))
                    )
                )
            "#;
            let caller = "
                (stx-transfer? u500 tx-sender .callee)
                (let ((result (contract-call? .callee send-stx u50 tx-sender)))
                    {error-code: result, balance: (stx-get-balance .callee)}
                )
            ";
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (ClarityName::from_literal("balance"), Value::UInt(450)),
                    (ClarityName::from_literal("error-code"), Value::okay_true()),
                ])
                .unwrap(),
            );
            clarity6_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        #[test]
        fn as_contract_safe_pox_then_stx_violation_index() {
            let callee = r#"
                (define-public (send-stx (amount uint) (recipient principal))
                    (as-contract? ((with-pox) (with-stx u10))
                        (try! (stx-transfer? amount current-contract recipient))
                    )
                )
            "#;
            let caller = "
                (stx-transfer? u500 tx-sender .callee)
                (let ((result (contract-call? .callee send-stx u50 tx-sender)))
                    {error-code: result, balance: (stx-get-balance .callee)}
                )
            ";
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (ClarityName::from_literal("balance"), Value::UInt(500)),
                    (
                        ClarityName::from_literal("error-code"),
                        Value::error(Value::UInt(1)).unwrap(),
                    ),
                ])
                .unwrap(),
            );
            clarity6_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }

        #[test]
        fn as_contract_safe_pox_and_staking_pox() {
            let pox4_code =
                std::fs::read_to_string("tests/contracts/boot-contracts/pox-4.clar").unwrap();
            let wrapper = r#"
                (define-public (do-delegate (amount uint) (delegate-to principal))
                    (as-contract? ((with-staking u1000000) (with-pox))
                        (unwrap-panic (contract-call? .pox-4 delegate-stx
                            amount delegate-to none none))
                    )
                )
            "#;
            clarity6_multi_contract(
                &[
                    (ContractName::from_literal("pox-4"), &pox4_code),
                    (ContractName::from_literal("wrapper"), wrapper),
                    (
                        ContractName::from_literal("test"),
                        "(contract-call? .wrapper do-delegate u1000 tx-sender)",
                    ),
                ],
                Ok(Some(Value::okay_true())),
            );
        }

        #[test]
        fn restrict_assets_pox_no_asset_movement() {
            let callee = r#"
                (define-public (do-nothing)
                    (restrict-assets? tx-sender ((with-pox))
                        true
                    )
                )
            "#;
            let caller = "(contract-call? .callee do-nothing)";
            clarity6_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(Value::okay_true())),
            );
        }

        #[test]
        fn restrict_assets_pox_does_not_allow_stx() {
            // A PoX allowance does not authorize STX outflow → (err u128).
            let callee = r#"
                (define-public (send-stx (amount uint) (recipient principal))
                    (restrict-assets? tx-sender ((with-pox))
                        (try! (stx-transfer? amount tx-sender recipient))
                    )
                )

                (define-read-only (callee-balance)
                    (stx-get-balance .callee)
                )
            "#;
            let caller = "
                (let ((result (contract-call? .callee send-stx u50 .callee)))
                    {error-code: result, callee-balance: (contract-call? .callee callee-balance)}
                )
            ";
            let expected = Value::Tuple(
                clarity::vm::types::TupleData::from_data(vec![
                    (ClarityName::from_literal("callee-balance"), Value::UInt(0)),
                    (
                        ClarityName::from_literal("error-code"),
                        Value::error(Value::UInt(128)).unwrap(),
                    ),
                ])
                .unwrap(),
            );
            clarity6_multi_contract(
                &[
                    (ContractName::from_literal("callee"), callee),
                    (ContractName::from_literal("caller"), caller),
                ],
                Ok(Some(expected)),
            );
        }
    }

    mod constant_call_targets {
        use clarity::types::StacksEpochId;
        use clarity::vm::errors::{RuntimeCheckErrorKind, VmExecutionError};
        use clarity::vm::ClarityVersion;

        use super::*;

        const CALLEE: &str = "(define-public (foo) (ok u1))";

        fn refused() -> Result<Option<Value>, VmExecutionError> {
            Err(VmExecutionError::RuntimeCheck(
                RuntimeCheckErrorKind::ContractCallExpectName,
            ))
        }

        fn dispatched() -> Result<Option<Value>, VmExecutionError> {
            Ok(Some(Value::okay(Value::UInt(1)).unwrap()))
        }

        /// Deployed in order, at an epoch and version named rather than inherited:
        /// two of the three conditions *are* the epoch and the version, so a
        /// default would hide which one answered.
        fn deploy(
            epoch: StacksEpochId,
            version: ClarityVersion,
            contracts: &[(&'static str, &'static str)],
            expected: Result<Option<Value>, VmExecutionError>,
        ) {
            let named: Vec<_> = contracts
                .iter()
                .map(|(name, source)| (ContractName::from_literal(name), *source))
                .collect();
            crosscheck_multi_contract_with_env(
                &named,
                expected,
                TestEnvironment::new(epoch, version),
            );
        }

        #[test]
        fn refused_while_the_contract_is_deploying() {
            deploy(
                StacksEpochId::Epoch40,
                ClarityVersion::Clarity6,
                &[
                    ("callee", CALLEE),
                    (
                        "caller",
                        "(define-constant target .callee) (contract-call? target foo)",
                    ),
                ],
                refused(),
            );
        }

        /// The flag belongs to the contract, not to its top level, so the refusal
        /// follows the constant into a function the deploy calls.
        #[test]
        fn refused_inside_a_function_the_deploy_calls() {
            deploy(
                StacksEpochId::Epoch40,
                ClarityVersion::Clarity6,
                &[
                    ("callee", CALLEE),
                    (
                        "caller",
                        "(define-constant target .callee)
                         (define-private (go) (contract-call? target foo))
                         (go)",
                    ),
                ],
                refused(),
            );
        }

        /// The same constant in the same function, reached once the deploy that
        /// defined it has finished. This is the half that says the gate is a
        /// run-time one and not a refusal of the syntax.
        #[test]
        fn dispatched_once_the_deploy_has_finished() {
            deploy(
                StacksEpochId::Epoch40,
                ClarityVersion::Clarity6,
                &[
                    ("callee", CALLEE),
                    (
                        "middle",
                        "(define-constant target .callee)
                         (define-public (go) (contract-call? target foo))",
                    ),
                    ("caller", "(contract-call? .middle go)"),
                ],
                dispatched(),
            );
        }

        /// The epoch half, which is the one the original measurement was taken at:
        /// before 3.4 a constant is not a dispatch target even outside a deploy.
        #[test]
        fn refused_before_the_epoch_that_allows_it() {
            deploy(
                StacksEpochId::Epoch33,
                ClarityVersion::Clarity4,
                &[
                    ("callee", CALLEE),
                    (
                        "middle",
                        "(define-constant target .callee)
                         (define-public (go) (contract-call? target foo))",
                    ),
                    ("caller", "(contract-call? .middle go)"),
                ],
                refused(),
            );
        }

        /// A contract name written out, in the place the constant was refused. The
        /// reference's first match arm takes a literal without consulting the
        /// epoch, the version or the flag, so the gate must not reach it.
        #[test]
        fn a_written_out_name_is_never_gated() {
            deploy(
                StacksEpochId::Epoch40,
                ClarityVersion::Clarity6,
                &[
                    ("callee", CALLEE),
                    ("caller", "(contract-call? .callee foo)"),
                ],
                dispatched(),
            );
        }
    }
}
