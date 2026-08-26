use std::collections::HashMap;
use std::io::{Cursor, Write as _};
use std::sync::Mutex;

use clarity::util::secp256r1::{secp256r1_verify, secp256r1_verify_digest};
use clarity::vm::callables::{DefineType, DefinedFunction};
use clarity::vm::contexts::AssetMap;
use clarity::vm::costs::cost_functions::ClarityCostFunction;
use clarity::vm::costs::{
    constants as cost_constants, runtime_cost, CostOverflowingMath, CostTracker,
};
use clarity::vm::database::{ClarityDatabase, STXBalance, StoreType};
use clarity::vm::errors::{RuntimeCheckErrorKind, RuntimeError, VmExecutionError, VmInternalError};
#[cfg(any())]
use clarity::vm::functions::crypto::{pubkey_to_address_v1, pubkey_to_address_v2};
#[cfg(any())]
use clarity::vm::functions::post_conditions::{
    check_allowances, Allowance, FtAllowance, NftAllowance, StackingAllowance, StxAllowance,
};
use clarity::vm::types::{
    AssetIdentifier, BuffData, BufferLength, FunctionType, ListTypeData, PrincipalData,
    SequenceData, SequenceSubtype, StacksAddressExtensions, TraitIdentifier, TupleData,
    TupleTypeSignature, TypeSignature,
};
use clarity::vm::{ClarityName, ClarityVersion, SymbolicExpression, Value};
use clarity_types::types::ResponseData;
use stacks_common::address::{
    AddressHashMode, C32_ADDRESS_VERSION_MAINNET_SINGLESIG, C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
};
use stacks_common::consts::CHAIN_ID_TESTNET;
use stacks_common::types::chainstate::StacksAddress;
use stacks_common::types::chainstate::StacksBlockId;
use stacks_common::types::StacksEpochId;
use stacks_common::util::ed25519::ed25519_verify;
use stacks_common::util::hash::{Keccak256Hash, Sha512Sum, Sha512Trunc256Sum};
use stacks_common::util::secp256k1::{
    secp256k1_decompress, secp256k1_recover, secp256k1_verify, Secp256k1PublicKey,
};
use wasmtime::{
    AsContextMut, Caller, Engine, ExternRef, Global, GlobalType, Instance, Linker, Memory, Module,
    Rooted, Store, Trap, Val,
};

use crate::cost::{Cost, CostGlobals};
use crate::error::WasmError;
use crate::error_mapping::ErrorMap;
use crate::initialize::{
    admit_function_argument, call_function_with_argument_sizes,
    StaticClarityWasmContext as ClarityWasmContext,
};
use crate::runtime_shape::RuntimeShapeStore;
use crate::wasm_utils::*;

fn pubkey_to_address_v1(public_key: Secp256k1PublicKey) -> Result<StacksAddress, VmExecutionError> {
    StacksAddress::from_public_keys(
        C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
        &AddressHashMode::SerializeP2PKH,
        1,
        &vec![public_key],
    )
    .ok_or_else(|| {
        VmInternalError::Expect("failed to create address from public key".into()).into()
    })
}

fn pubkey_to_address_v2(
    public_key: Secp256k1PublicKey,
    mainnet: bool,
) -> Result<StacksAddress, VmExecutionError> {
    let version = if mainnet {
        C32_ADDRESS_VERSION_MAINNET_SINGLESIG
    } else {
        C32_ADDRESS_VERSION_TESTNET_SINGLESIG
    };
    StacksAddress::from_public_keys(
        version,
        &AddressHashMode::SerializeP2PKH,
        1,
        &vec![public_key],
    )
    .ok_or_else(|| {
        VmInternalError::Expect("failed to create address from public key".into()).into()
    })
}

#[derive(Debug)]
struct StxAllowance {
    amount: u128,
}

#[derive(Debug)]
struct FtAllowance {
    asset: AssetIdentifier,
    amount: u128,
}

#[derive(Debug)]
struct NftAllowance {
    asset: AssetIdentifier,
    asset_ids: Vec<Value>,
}

#[derive(Debug)]
struct StackingAllowance {
    amount: u128,
}

#[derive(Debug)]
enum Allowance {
    Stx(StxAllowance),
    Ft(FtAllowance),
    Nft(NftAllowance),
    Stacking(StackingAllowance),
    Pox,
    All,
}

fn check_allowances(
    owner: &PrincipalData,
    allowances: Vec<Allowance>,
    assets: &AssetMap,
    epoch: StacksEpochId,
) -> Result<Option<u128>, VmExecutionError> {
    const MAX_ALLOWANCES: u128 = 128;
    let mut earliest = None;
    let mut record = |index| {
        if earliest.is_none_or(|current| index < current) {
            earliest = Some(index);
        }
    };
    let mut stx = Vec::new();
    let mut ft: HashMap<AssetIdentifier, Vec<(usize, u128)>> = HashMap::new();
    let mut nft: HashMap<AssetIdentifier, (usize, Vec<Value>)> = HashMap::new();
    let mut stacking = Vec::new();
    let mut has_pox = false;

    for (index, allowance) in allowances.into_iter().enumerate() {
        match allowance {
            Allowance::All => return Ok(None),
            Allowance::Stx(value) => stx.push((index, value.amount)),
            Allowance::Ft(value) => ft
                .entry(value.asset)
                .or_default()
                .push((index, value.amount)),
            Allowance::Nft(value) => nft
                .entry(value.asset)
                .or_insert_with(|| (index, Vec::new()))
                .1
                .extend(value.asset_ids),
            Allowance::Stacking(value) => stacking.push((index, value.amount)),
            Allowance::Pox => has_pox = true,
        }
    }

    let moved = assets.get_stx(owner);
    let burned = assets.get_stx_burned(owner);
    for amount in [moved, burned].into_iter().flatten() {
        if stx.is_empty() {
            record(MAX_ALLOWANCES);
        } else if let Some((index, _)) = stx.iter().find(|(_, limit)| amount > *limit) {
            record(*index as u128);
        }
    }
    if let Some(tokens) = assets.get_all_fungible_tokens(owner) {
        for (asset, amount) in tokens {
            let mut limits = ft.get(asset).cloned().unwrap_or_default();
            limits.extend(
                ft.get(&AssetIdentifier {
                    contract_identifier: asset.contract_identifier.clone(),
                    asset_name: ClarityName::from_literal("*"),
                })
                .cloned()
                .unwrap_or_default(),
            );
            if limits.is_empty() {
                record(MAX_ALLOWANCES);
            } else {
                for (index, limit) in limits {
                    if *amount > limit {
                        record(index as u128);
                    }
                }
            }
        }
    }
    if let Some(tokens) = assets.get_all_nonfungible_tokens(owner) {
        for (asset, ids) in tokens {
            let mut limits = Vec::new();
            if let Some(limit) = nft.get(asset) {
                limits.push(limit);
            }
            if let Some(limit) = nft.get(&AssetIdentifier {
                contract_identifier: asset.contract_identifier.clone(),
                asset_name: ClarityName::from_literal("*"),
            }) {
                limits.push(limit);
            }
            if limits.is_empty() {
                record(MAX_ALLOWANCES);
            } else {
                for (index, allowed) in limits {
                    if ids.iter().any(|id| !allowed.contains(id)) {
                        record(*index as u128);
                    }
                }
            }
        }
    }
    if let Some(amount) = assets.get_stacking(owner) {
        if stacking.is_empty() {
            record(MAX_ALLOWANCES);
        } else if let Some((index, _)) = stacking.iter().find(|(_, limit)| amount > *limit) {
            record(*index as u128);
        }
    }
    if assets.did_pox_action(owner) && !has_pox {
        record(MAX_ALLOWANCES);
    }
    if let Some(total) = moved.unwrap_or(0).checked_add(burned.unwrap_or(0)) {
        if epoch.handles_with_stx_combined_check() && stx.iter().any(|(_, limit)| total > *limit) {
            if let Some((index, _)) = stx.iter().find(|(_, limit)| total > *limit) {
                record(*index as u128);
            }
        }
    } else {
        return Err(VmInternalError::Expect("STX movement overflowed".into()).into());
    }
    Ok(earliest)
}

pub fn link_cost_globals<T: 'static>(
    linker: &mut Linker<T>,
    store: &mut impl AsContextMut<Data = T>,
) -> Result<CostGlobals, VmExecutionError> {
    let runtime = link_global(linker, store, Cost::Runtime.as_str(), Val::I64(i64::MAX))?;
    let read_count = link_global(linker, store, Cost::ReadCount.as_str(), Val::I64(i64::MAX))?;
    let read_length = link_global(linker, store, Cost::ReadLength.as_str(), Val::I64(i64::MAX))?;
    let write_count = link_global(linker, store, Cost::WriteCount.as_str(), Val::I64(i64::MAX))?;
    let write_length = link_global(
        linker,
        store,
        Cost::WriteLength.as_str(),
        Val::I64(i64::MAX),
    )?;
    Ok(CostGlobals {
        runtime,
        read_count,
        read_length,
        write_count,
        write_length,
    })
}

fn link_global<T: 'static>(
    linker: &mut Linker<T>,
    store: &mut impl AsContextMut<Data = T>,
    name: &str,
    value: Val,
) -> Result<Global, VmExecutionError> {
    let the_global = Global::new(
        store.as_context_mut(),
        GlobalType::new(wasmtime::ValType::I64, wasmtime::Mutability::Var),
        value,
    )
    .map_err(|e| crate::error::wasm_error(WasmError::UnableToLoadModule(e)))?;

    linker
        .define(&mut store.as_context_mut(), "clarity", name, the_global)
        .map_err(|e| crate::error::wasm_error(WasmError::UnableToLoadModule(e)))?;
    Ok(the_global)
}

/// Link the host interface functions into the Wasm module.
pub fn link_host_functions(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    link_save_runtime_shape_fn(linker)?;
    link_save_filtered_runtime_shape_fn(linker)?;
    link_runtime_shape_size_fn(linker)?;
    link_runtime_shape_serialization_size_fn(linker)?;
    link_runtime_value_size_fn(linker)?;
    link_runtime_sequence_element_size_fn(linker)?;
    link_admit_function_argument_fn(linker)?;
    link_runtime_shape_is_equal_fn(linker)?;
    link_merge_runtime_shape_fn(linker)?;
    link_deserialize_runtime_shape_fn(linker)?;
    link_field_runtime_shape_fn(linker)?;
    link_define_function_fn(linker)?;
    link_define_variable_fn(linker)?;
    link_define_ft_fn(linker)?;
    link_define_nft_fn(linker)?;
    link_define_map_fn(linker)?;
    link_define_trait_fn(linker)?;
    link_impl_trait_fn(linker)?;

    link_get_variable_fn(linker)?;
    link_set_variable_fn(linker)?;
    link_tx_sender_fn(linker)?;
    link_contract_caller_fn(linker)?;
    link_current_contract_fn(linker)?;
    link_tx_sponsor_fn(linker)?;
    link_block_height_fn(linker)?;
    link_stacks_block_height_fn(linker)?;
    link_stacks_block_time_fn(linker)?;
    link_tenure_height_fn(linker)?;
    link_burn_block_height_fn(linker)?;
    link_stx_liquid_supply_fn(linker)?;
    link_is_in_regtest_fn(linker)?;
    link_is_in_mainnet_fn(linker)?;
    link_chain_id_fn(linker)?;
    link_enter_as_contract_fn(linker)?;
    link_exit_as_contract_fn(linker)?;
    link_principal_depth_fns(linker)?;
    link_enter_as_contract_safe_fn(linker)?;
    link_exit_as_contract_safe_fn(linker)?;
    link_cleanup_as_contract_safe_fn(linker)?;
    link_enter_restrict_assets_fn(linker)?;
    link_exit_restrict_assets_fn(linker)?;
    link_cleanup_restrict_assets_fn(linker)?;
    link_with_all_assets_unsafe_fn(linker)?;
    link_with_ft_fn(linker)?;
    link_with_nft_fn(linker)?;
    link_with_stacking_fn(linker)?;
    link_with_pox_fn(linker)?;
    link_with_stx_fn(linker)?;
    link_stx_get_balance_fn(linker)?;
    link_stx_account_fn(linker)?;
    link_stx_burn_fn(linker)?;
    link_stx_transfer_fn(linker)?;
    link_ft_get_supply_fn(linker)?;
    link_ft_get_balance_fn(linker)?;
    link_ft_burn_fn(linker)?;
    link_ft_mint_fn(linker)?;
    link_ft_transfer_fn(linker)?;
    link_nft_get_owner_fn(linker)?;
    link_nft_burn_fn(linker)?;
    link_nft_mint_fn(linker)?;
    link_nft_transfer_fn(linker)?;
    link_map_get_fn(linker)?;
    link_map_set_fn(linker)?;
    link_map_insert_fn(linker)?;
    link_map_delete_fn(linker)?;
    link_charge_probe_fn(linker)?;
    link_get_stacks_block_info_header_hash_property_fn(linker)?;
    link_get_stacks_block_info_time_property_fn(linker)?;
    link_get_stacks_block_info_identity_header_hash_property_fn(linker)?;
    link_get_tenure_info_burnchain_header_hash_property_fn(linker)?;
    link_get_tenure_info_miner_address_property_fn(linker)?;
    link_get_tenure_info_vrf_seed_property_fn(linker)?;
    link_get_tenure_info_time_property_fn(linker)?;
    link_get_tenure_info_block_reward_property_fn(linker)?;
    link_get_tenure_info_miner_spend_total_property_fn(linker)?;
    link_get_tenure_info_miner_spend_winner_property_fn(linker)?;
    link_get_block_info_time_property_fn(linker)?;
    link_get_block_info_vrf_seed_property_fn(linker)?;
    link_get_block_info_header_hash_property_fn(linker)?;
    link_get_block_info_burnchain_header_hash_property_fn(linker)?;
    link_get_block_info_identity_header_hash_property_fn(linker)?;
    link_get_block_info_miner_address_property_fn(linker)?;
    link_get_block_info_miner_spend_winner_property_fn(linker)?;
    link_get_block_info_miner_spend_total_property_fn(linker)?;
    link_get_block_info_block_reward_property_fn(linker)?;
    link_get_burn_block_info_header_hash_property_fn(linker)?;
    link_get_burn_block_info_pox_addrs_property_fn(linker)?;
    link_contract_call_fn(linker)?;
    link_check_constant_call_target_fn(linker)?;
    link_contract_hash_fn(linker)?;
    link_begin_public_call_fn(linker)?;
    link_begin_read_only_call_fn(linker)?;
    link_commit_call_fn(linker)?;
    link_roll_back_call_fn(linker)?;
    link_print_fn(linker)?;
    link_enter_at_block_fn(linker)?;
    link_exit_at_block_fn(linker)?;
    link_keccak256_fn(linker)?;
    link_sha512_fn(linker)?;
    link_sha512_256_fn(linker)?;
    link_secp256k1_recover_fn(linker)?;
    link_secp256k1_verify_fn(linker)?;
    link_secp256r1_verify_fn(linker)?;
    link_ed25519_verify_fn(linker)?;
    link_secp256k1_decompress_fn(linker)?;
    link_verify_merkle_proof_fn(linker)?;
    link_get_bitcoin_tx_output_fn(linker)?;
    link_principal_of_fn(linker)?;
    link_save_constant_fn(linker)?;
    link_load_constant_fn(linker)?;
    link_principal_to_string_ascii(linker)?;
    link_skip_list(linker)?;

    link_log(linker)?;
    link_debug_msg(linker)
}

/// One runtime-shape type text, parsed through the per-call cache.
///
/// The coordinates name one data-segment constant, so the encoding (JSON here,
/// type text elsewhere) rides along with them.
fn read_cached_runtime_type(
    caller: &mut Caller<'_, ClarityWasmContext>,
    memory: Memory,
    serialized_ty_offset: i32,
    serialized_ty_length: i32,
) -> Result<TypeSignature, VmExecutionError> {
    if let Some(known) = caller
        .data()
        .parsed_types
        .get(&(serialized_ty_offset, serialized_ty_length))
    {
        return Ok(known.clone());
    }
    let serialized_ty =
        read_identifier_from_wasm(memory, caller, serialized_ty_offset, serialized_ty_length)?;
    let parsed: TypeSignature = serde_json::from_str(&serialized_ty).map_err(|error| {
        crate::error::wasm_error(WasmError::Expect(format!(
            "runtime-shape type cannot be decoded: {error}"
        )))
    })?;
    caller
        .data_mut()
        .parsed_types
        .insert((serialized_ty_offset, serialized_ty_length), parsed.clone());
    Ok(parsed)
}

fn link_save_runtime_shape_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "save_runtime_shape",
            |mut caller: Caller<'_, ClarityWasmContext>,
             value_offset: i32,
             serialized_ty_offset: i32,
             serialized_ty_length: i32| {
                crate::phases::time(crate::phases::Phase::ShapeSave, || {
                    let memory = caller
                        .data()
                        .memory
                        .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;
                    let value_ty = read_cached_runtime_type(
                        &mut caller,
                        memory,
                        serialized_ty_offset,
                        serialized_ty_length,
                    )?;
                    let epoch = caller.data().global_context.epoch_id;
                    // A shape this cannot read is a shape it cannot preserve, and
                    // the only representation that cannot be read is a
                    // placeholder: duck-typing an optional or a response has to
                    // project the payload's slots to the target's shape on both
                    // branches, so the `none` branch offers a tuple whose
                    // principals are the unmaterialised `(0, 0)`. There is no
                    // value there to remember, and the answer for "no shape" is
                    // the same zero handle a value that never crossed the host
                    // carries.
                    //
                    // Mainnet 8,815,026 is the case: a `map-set` whose value
                    // holds `(optional {staker: principal, signer-manager:
                    // principal})` as `none` made nano refuse a transaction the
                    // chain executed.
                    let Ok(value) = read_from_wasm_indirect(
                        memory,
                        &mut caller,
                        &value_ty,
                        value_offset,
                        epoch,
                    ) else {
                        return Ok(0i32);
                    };
                    let handle = caller.data_mut().save_runtime_shape(value)?;
                    Ok(handle)
                })
            },
        )
        .map(|_| ())
        .map_err(|error| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "save_runtime_shape".to_owned(),
                error,
            ))
        })
}

/// Link `save_filtered_runtime_shape`: materialize a `filter` result that
/// inherits its input's list capacity.
///
/// The reference's `filter` mutates its argument in place and hands the same
/// value back, so the result keeps the `max_len` the input carried — and
/// `Value::size()` is `type_signature_size + max_len * entry_size`. Rebuilding
/// the list from the kept elements alone sizes it by the kept count instead,
/// which under-charged every `filter` that dropped anything.
///
/// `input_handle` names the input when it already had a wider shape;
/// `input_count` is its element count, which is its capacity whenever it did
/// not. Zero handle and zero count together mean there is nothing to inherit.
fn link_save_filtered_runtime_shape_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "save_filtered_runtime_shape",
            |mut caller: Caller<'_, ClarityWasmContext>,
             value_offset: i32,
             serialized_ty_offset: i32,
             serialized_ty_length: i32,
             input_handle: i32,
             input_count: i32| {
                crate::phases::time(crate::phases::Phase::ShapeSave, || {
                    let memory = caller
                        .data()
                        .memory
                        .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;
                    let value_ty = read_cached_runtime_type(
                        &mut caller,
                        memory,
                        serialized_ty_offset,
                        serialized_ty_length,
                    )?;
                    let (inherited, max_len) = if input_handle != 0 {
                        (caller.data().runtime_shape_list_type(input_handle)?, 0)
                    } else {
                        (
                            None,
                            u32::try_from(input_count).map_err(|_| {
                                crate::error::wasm_error(WasmError::ValueTypeMismatch)
                            })?,
                        )
                    };
                    let epoch = caller.data().global_context.epoch_id;
                    let Ok(value) = read_from_wasm_indirect(
                        memory,
                        &mut caller,
                        &value_ty,
                        value_offset,
                        epoch,
                    ) else {
                        return Ok(0i32);
                    };
                    let handle = caller
                        .data_mut()
                        .save_runtime_shape_inheriting(value, inherited, max_len)?;
                    Ok(handle)
                })
            },
        )
        .map(|_| ())
        .map_err(|error| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "save_filtered_runtime_shape".to_owned(),
                error,
            ))
        })
}

/// Link `runtime_shape_size`: a handled value's `Value::size()` by handle.
///
/// The generator calls this instead of `runtime_value_size` when the value's
/// representation already names an arena entry — the value is in the arena,
/// so writing its whole representation into a fresh region just to have the
/// host read the handle back out of it was pure ceremony, paid per measurement.
fn link_runtime_shape_size_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "runtime_shape_size",
            |caller: Caller<'_, ClarityWasmContext>, handle: i32| {
                crate::phases::time(crate::phases::Phase::HostShape, || {
                    let size = caller.data().runtime_shape_value_size(handle)?;
                    let size = i32::try_from(size)
                        .map_err(|_| crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
                    Ok(size)
                })
            },
        )
        .map(|_| ())
        .map_err(|error| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "runtime_shape_size".to_owned(),
                error,
            ))
        })
}

fn link_runtime_shape_serialization_size_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "runtime_shape_serialization_size",
            |caller: Caller<'_, ClarityWasmContext>, handle: i32| {
                crate::phases::time(crate::phases::Phase::HostShape, || {
                    let size = caller.data().runtime_shape_serialized_size(handle)?;
                    let size = i32::try_from(size)
                        .map_err(|_| crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
                    Ok(size)
                })
            },
        )
        .map(|_| ())
        .map_err(|error| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "runtime_shape_serialization_size".to_owned(),
                error,
            ))
        })
}

fn link_runtime_value_size_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "runtime_value_size",
            |mut caller: Caller<'_, ClarityWasmContext>,
             value_offset: i32,
             serialized_ty_offset: i32,
             serialized_ty_length: i32| {
                crate::phases::time(crate::phases::Phase::HostShape, || {
                    // `Value::size` is `type_of(value).size()`, and for a
                    // monomorphic primitive the dynamic type is the static type:
                    // materializing the value cannot change the answer. Two
                    // thirds of every host call a mainnet replay makes is this
                    // measurement — a fold measures its accumulator every
                    // iteration — so the type text goes through the per-call
                    // parse cache rather than being re-parsed per measurement,
                    // which was ~90 ms of a heavy read call's ~120.
                    let (memory, value_ty) = runtime_value_type(
                        &mut caller,
                        serialized_ty_offset,
                        serialized_ty_length,
                    )?;
                    // No bare-`principal` fast arm, deliberately: a trait
                    // reference erases to `principal` in the runtime type string,
                    // and `type_of` on a callable value answers with the callee's
                    // own type, whose size is the contract name's, not the
                    // principal maximum. Inside a list that distinction is gone —
                    // materializing reads every element back as
                    // `Value::Principal`, so the invariant-list path answers
                    // exactly what the materializing one would.
                    let size = match &value_ty {
                        TypeSignature::IntType | TypeSignature::UIntType => 16,
                        TypeSignature::BoolType => 1,
                        _ => {
                            if let Some(size) = handled_composite_size(
                                &mut caller,
                                memory,
                                &value_ty,
                                value_offset,
                            )? {
                                return Ok(size);
                            }
                            if let Some(size) =
                                invariant_list_size(&mut caller, memory, &value_ty, value_offset)?
                            {
                                return Ok(size);
                            }
                            let epoch = caller.data().global_context.epoch_id;
                            // Sizing from the declared type alone is unsound for
                            // anything wider than a monomorphic primitive: at a
                            // function entry the runtime shape may be *wider*
                            // than the declaration — duck typing hands `echo` a
                            // `{soft, full}` where `{soft}` is declared — and the
                            // reference charges the value it was actually given.
                            let size = read_from_wasm_indirect(
                                memory,
                                &mut caller,
                                &value_ty,
                                value_offset,
                                epoch,
                            )?
                            .size()
                            .map_err(|_| crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
                            i32::try_from(size).map_err(|_| {
                                crate::error::wasm_error(WasmError::ValueTypeMismatch)
                            })?
                        }
                    };
                    Ok(size)
                })
            },
        )
        .map(|_| ())
        .map_err(|error| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "runtime_value_size".to_owned(),
                error,
            ))
        })
}

fn link_runtime_sequence_element_size_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "runtime_sequence_element_size",
            |mut caller: Caller<'_, ClarityWasmContext>,
             value_offset: i32,
             serialized_ty_offset: i32,
             serialized_ty_length: i32| {
                crate::phases::time(crate::phases::Phase::HostShape, || {
                    let value = read_runtime_value(
                        &mut caller,
                        value_offset,
                        serialized_ty_offset,
                        serialized_ty_length,
                    )?;
                    let sequence = match value {
                        Value::Sequence(sequence) => sequence,
                        _ => Err(crate::error::wasm_error(WasmError::ValueTypeMismatch))?,
                    };
                    let size = sequence
                        .element_size()
                        .map_err(|_| crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
                    let size = i32::try_from(size)
                        .map_err(|_| crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
                    Ok(size)
                })
            },
        )
        .map(|_| ())
        .map_err(|error| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "runtime_sequence_element_size".to_owned(),
                error,
            ))
        })
}

fn link_admit_function_argument_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "admit_function_argument",
            |mut caller: Caller<'_, ClarityWasmContext>,
             value_offset: i32,
             serialized_ty_offset: i32,
             serialized_ty_length: i32,
             function_name_offset: i32,
             function_name_length: i32,
             argument_index: i32| {
                crate::phases::time(crate::phases::Phase::ShapeAdmit, || {
                    let (memory, representation_type) = runtime_value_type(
                        &mut caller,
                        serialized_ty_offset,
                        serialized_ty_length,
                    )?;
                    let function_name = read_identifier_from_wasm(
                        memory,
                        &mut caller,
                        function_name_offset,
                        function_name_length,
                    )?;
                    let argument_index = usize::try_from(argument_index)
                        .map_err(|_| crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
                    let expected_type = {
                        let function = caller
                            .data()
                            .contract_context()
                            .lookup_function(&function_name)
                            .ok_or_else(|| {
                                VmExecutionError::from(RuntimeCheckErrorKind::UndefinedFunction(
                                    function_name.clone(),
                                ))
                            })?;
                        function
                            .get_arg_types()
                            .get(argument_index)
                            .cloned()
                            .ok_or(crate::error::wasm_error(WasmError::ValueTypeMismatch))?
                    };
                    // A value admitted against its own tuple-free type is the
                    // identity, and the callee's argument region already
                    // holds the caller's representation — the generator wrote
                    // it there before this call — so the materialize, the
                    // admission walk and the write-back of the same bytes buy
                    // nothing. Function entries admit every argument (a fold
                    // re-admits its accumulator each iteration), which made
                    // this the hottest measurement in the engine once sizes
                    // were fast.
                    if representation_type == expected_type && admit_preserves(&expected_type) {
                        return Ok(());
                    }
                    // A tuple is admitted for its widening, and a
                    // `TupleData`'s signature is its dynamic type — kept in
                    // step by every constructor. An arena value whose
                    // signature equals the declaration therefore has nothing
                    // to strip anywhere in it, and comparing signatures costs
                    // no clone at all.
                    if let TypeSignature::TupleType(expected_tuple) = &expected_type {
                        let handle = read_i32(memory, &mut caller, value_offset)?;
                        let exact_static_value = representation_type == expected_type
                            && handle == 0
                            && expected_tuple.get_type_map().values().all(admit_preserves);
                        let exact_runtime_value = handle != 0
                            && caller.data().runtime_shapes().is_some_and(|arena| {
                                matches!(
                                    arena.get(handle),
                                    Ok(Value::Tuple(tuple))
                                        if tuple.type_signature == *expected_tuple
                                )
                            });
                        if exact_static_value || exact_runtime_value {
                            return Ok(());
                        }
                    }
                    let epoch = caller.data().global_context.epoch_id;
                    let argument = read_from_wasm_indirect(
                        memory,
                        &mut caller,
                        &representation_type,
                        value_offset,
                        epoch,
                    )?;
                    let admitted = admit_function_argument(&expected_type, &argument, epoch)?;

                    let representation_size = get_type_size(&representation_type);
                    let in_memory_offset = value_offset
                        .checked_add(representation_size)
                        .ok_or(crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
                    let zeroes = vec![
                        0;
                        usize::try_from(representation_size).map_err(|_| {
                            crate::error::wasm_error(WasmError::ValueTypeMismatch)
                        })?
                    ];
                    memory
                        .write(&mut caller, value_offset as usize, &zeroes)
                        .map_err(|error| {
                            crate::error::wasm_error(WasmError::UnableToWriteMemory(error.into()))
                        })?;
                    write_to_wasm(
                        &mut caller,
                        memory,
                        &expected_type,
                        value_offset,
                        in_memory_offset,
                        &admitted,
                        true,
                    )?;
                    Ok(())
                })
            },
        )
        .map(|_| ())
        .map_err(|error| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "admit_function_argument".to_owned(),
                error,
            ))
        })
}

/// The one size every value of this type has, when the type is its own
/// dynamic type.
///
/// `Value::size()` is `type_of(value).size()`, so a measurement only needs the
/// value itself when materializing could change the answer. For these four,
/// it cannot: `type_of` maps every `Int` to `IntType`, every principal —
/// standard or contract — to `PrincipalType`, and so on, and none of their
/// wasm representations carries a runtime-shape handle that could name a
/// widened value.
const fn invariant_value_size(ty: &TypeSignature) -> Option<u32> {
    match ty {
        TypeSignature::IntType | TypeSignature::UIntType => Some(16),
        TypeSignature::BoolType => Some(1),
        TypeSignature::PrincipalType => Some(148),
        _ => None,
    }
}

/// A widened composite's `Value::size()` answered by the arena, clone-free.
///
/// Lists and tuples carry a runtime-shape handle as the first slot of their
/// memory representation, and both read paths return the arena value when it
/// is set — so the size is the arena entry's, which the arena memoizes. The
/// materializing path answered the same number by cloning the whole value out
/// and re-deriving its type, on every iteration of whatever loop is measuring.
fn handled_composite_size(
    caller: &mut Caller<'_, ClarityWasmContext>,
    memory: Memory,
    ty: &TypeSignature,
    value_offset: i32,
) -> Result<Option<i32>, VmExecutionError> {
    if !matches!(
        ty,
        TypeSignature::SequenceType(SequenceSubtype::ListType(_)) | TypeSignature::TupleType(_)
    ) {
        return Ok(None);
    }
    let handle = read_i32(memory, caller, value_offset)?;
    if handle == 0 {
        return Ok(None);
    }
    let size = caller.data().runtime_shape_value_size(handle)?;
    let size =
        i32::try_from(size).map_err(|_| crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
    Ok(Some(size))
}

/// A list's `Value::size()` read from its representation alone, when its
/// element type makes materialization unnecessary.
///
/// The reference computes a list value's size as
/// `ListTypeData(len, least-supertype of element dynamic types).size()`.
/// When every element's dynamic type *is* the declared element type
/// ([`invariant_value_size`]), the supertype fold is the identity and the
/// length is the representation's byte length over the element stride — so
/// deserializing the whole list per measurement (a fold measures per
/// iteration, so quadratic in practice) buys nothing.
///
/// `None` sends the caller down the materializing path: a non-list type, an
/// element type whose values vary, a runtime-shape handle (the arena value
/// governs then), or an empty list (whose entry type the reference derives
/// differently).
fn invariant_list_size(
    caller: &mut Caller<'_, ClarityWasmContext>,
    memory: Memory,
    ty: &TypeSignature,
    value_offset: i32,
) -> Result<Option<i32>, VmExecutionError> {
    let TypeSignature::SequenceType(SequenceSubtype::ListType(list)) = ty else {
        return Ok(None);
    };
    let element = list.get_list_item_type();
    if invariant_value_size(element).is_none() {
        return Ok(None);
    }
    let handle = read_i32(memory, caller, value_offset)?;
    if handle != 0 {
        return Ok(None);
    }
    let length = read_i32(memory, caller, value_offset + 8)?;
    let stride = get_type_size(element);
    if length <= 0 || stride <= 0 || length % stride != 0 {
        return Ok(None);
    }
    let count = (length / stride) as u32;
    let Ok(list_ty) = ListTypeData::new_list(element.clone(), count) else {
        return Ok(None);
    };
    let size = TypeSignature::SequenceType(SequenceSubtype::ListType(list_ty))
        .size()
        .map_err(|_| crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
    let size =
        i32::try_from(size).map_err(|_| crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
    Ok(Some(size))
}

fn runtime_value_type(
    caller: &mut Caller<'_, ClarityWasmContext>,
    serialized_ty_offset: i32,
    serialized_ty_length: i32,
) -> Result<(Memory, TypeSignature), VmExecutionError> {
    let memory = caller
        .data()
        .memory
        .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;
    // The serialized text is a data-segment constant, so within this call the
    // same coordinates always hold the same string; parsing it fresh was the
    // bulk of every size measurement.
    if let Some(known) = caller
        .data()
        .parsed_types
        .get(&(serialized_ty_offset, serialized_ty_length))
    {
        return Ok((memory, known.clone()));
    }
    let serialized_ty = read_identifier_from_wasm(
        memory,
        &mut *caller,
        serialized_ty_offset,
        serialized_ty_length,
    )?;
    let epoch = caller.data().global_context.epoch_id;
    let version = *caller.data().contract_context().get_clarity_version();
    let parsed = signature_from_string(&serialized_ty, version, epoch)?;
    caller
        .data_mut()
        .parsed_types
        .insert((serialized_ty_offset, serialized_ty_length), parsed.clone());
    Ok((memory, parsed))
}

fn read_runtime_value(
    caller: &mut Caller<'_, ClarityWasmContext>,
    value_offset: i32,
    serialized_ty_offset: i32,
    serialized_ty_length: i32,
) -> Result<Value, VmExecutionError> {
    let (memory, value_ty) =
        runtime_value_type(caller, serialized_ty_offset, serialized_ty_length)?;
    let epoch = caller.data().global_context.epoch_id;
    let contract_identifier = {
        let contract = caller.data().contract_context();
        contract.contract_identifier.clone()
    };
    read_from_wasm_indirect(memory, caller, &value_ty, value_offset, epoch).map_err(|error| {
        crate::error::wasm_error(WasmError::Expect(format!(
            "runtime value in {contract_identifier} at offset {value_offset} with outer type \
             {value_ty} could not be read: {error}"
        )))
    })
}

/// Link `merge_runtime_shape`: the arena value a `merge` of an arena value is.
///
/// `TupleData::shallow_merge` keeps the *base's* type signature and overrides
/// only the updated fields' types, so a merge of a value that came out of the
/// database keeps that value's declared widths — `(string-ascii 32)` stays 32
/// wide however short the string in it is. Rebuilding the merged value from
/// nano's representation instead measures the string, which is how
/// `arkadiko-swap-v2-1` charged 623 for a pair the chain charges 646.
///
/// Returns 0 when the base carries no arena value, which is when nano's own
/// measurement is already the right one.
/// Link `deserialize_runtime_shape`: the arena value `from-consensus-buff?`
/// produces.
///
/// `Value::try_deserialize_bytes_exact` builds the value against the type it
/// was given, so a `(buff 32)` field holding twenty bytes still says 32 — the
/// same bytes size 164 built from data and 176 deserialized. Nano's own
/// representation records what it holds, so measuring it charges the 164. The
/// arena keeps the value the reference would have, and the handle is how a
/// measurement finds it.
///
/// Returns 0 when the bytes do not deserialize to the type, which is the
/// `none` result, and nothing then reads the value.
/// Link `field_runtime_shape`: the arena value a tuple field of an arena value
/// is.
///
/// A tuple keeps the type signature it was built with, so the field pulled out
/// of it keeps the width that signature gave it. Nano's representation of the
/// extracted field carries no handle of its own, so without this the field
/// falls back to being measured, which narrows it.
///
/// Returns 0 when the parent carries no arena value or the field is absent.
fn link_field_runtime_shape_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "field_runtime_shape",
            |mut caller: Caller<'_, ClarityWasmContext>,
             base_handle: i32,
             name_offset: i32,
             name_length: i32| {
                crate::phases::time(crate::phases::Phase::ShapeSave, || {
                    if base_handle == 0 {
                        return Ok(0i32);
                    }
                    let memory = caller
                        .data()
                        .memory
                        .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;
                    let name =
                        read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;
                    let base = match caller.data().load_runtime_shape(base_handle)? {
                        Value::Tuple(base) => base,
                        _ => Err(crate::error::wasm_error(WasmError::ValueTypeMismatch))?,
                    };
                    let Some(field) = base.data_map.get(name.as_str()) else {
                        return Ok(0i32);
                    };
                    let field = field.clone();
                    Ok(caller.data_mut().save_runtime_shape(field)?)
                })
            },
        )
        .map(|_| ())
        .map_err(|error| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "field_runtime_shape".to_owned(),
                error,
            ))
        })
}

fn link_deserialize_runtime_shape_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "deserialize_runtime_shape",
            |mut caller: Caller<'_, ClarityWasmContext>,
             bytes_offset: i32,
             bytes_length: i32,
             serialized_ty_offset: i32,
             serialized_ty_length: i32| {
                crate::phases::time(crate::phases::Phase::ShapeSave, || {
                    let (memory, value_ty) = runtime_value_type(
                        &mut caller,
                        serialized_ty_offset,
                        serialized_ty_length,
                    )?;
                    let mut bytes = vec![0u8; usize::try_from(bytes_length).unwrap_or(0)];
                    memory
                        .read(
                            &mut caller,
                            usize::try_from(bytes_offset).unwrap_or(0),
                            &mut bytes,
                        )
                        .map_err(|error| {
                            crate::error::wasm_error(WasmError::UnableToReadMemory(error.into()))
                        })?;
                    let sanitize = caller.data().global_context.epoch_id.value_sanitizing();
                    let Ok(value) = Value::try_deserialize_bytes_exact(&bytes, &value_ty, sanitize)
                    else {
                        return Ok(0i32);
                    };
                    Ok(caller.data_mut().save_runtime_shape(value)?)
                })
            },
        )
        .map(|_| ())
        .map_err(|error| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "deserialize_runtime_shape".to_owned(),
                error,
            ))
        })
}

fn link_merge_runtime_shape_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "merge_runtime_shape",
            |mut caller: Caller<'_, ClarityWasmContext>,
             base_handle: i32,
             updates_offset: i32,
             serialized_ty_offset: i32,
             serialized_ty_length: i32| {
                if base_handle == 0 {
                    return Ok(0i32);
                }
                let base = match caller.data().load_runtime_shape(base_handle)? {
                    Value::Tuple(base) => base,
                    _ => Err(crate::error::wasm_error(WasmError::ValueTypeMismatch))?,
                };
                let updates = match read_runtime_value(
                    &mut caller,
                    updates_offset,
                    serialized_ty_offset,
                    serialized_ty_length,
                )? {
                    Value::Tuple(updates) => updates,
                    _ => Err(crate::error::wasm_error(WasmError::ValueTypeMismatch))?,
                };
                let merged = clarity::vm::types::TupleData::shallow_merge(base, updates);
                Ok(caller.data_mut().save_runtime_shape(Value::Tuple(merged))?)
            },
        )
        .map(|_| ())
        .map_err(|error| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "merge_runtime_shape".to_owned(),
                error,
            ))
        })
}

fn link_runtime_shape_is_equal_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "runtime_shape_is_equal",
            |mut caller: Caller<'_, ClarityWasmContext>,
             first_offset: i32,
             second_offset: i32,
             serialized_ty_offset: i32,
             serialized_ty_length: i32| {
                crate::phases::time(crate::phases::Phase::ShapeEq, || {
                    let (memory, value_ty) = runtime_value_type(
                        &mut caller,
                        serialized_ty_offset,
                        serialized_ty_length,
                    )?;
                    let epoch = caller.data().global_context.epoch_id;
                    let first = read_from_wasm_indirect(
                        memory,
                        &mut caller,
                        &value_ty,
                        first_offset,
                        epoch,
                    )?;
                    let second = read_from_wasm_indirect(
                        memory,
                        &mut caller,
                        &value_ty,
                        second_offset,
                        epoch,
                    )?;
                    Ok(i32::from(first == second))
                })
            },
        )
        .map(|_| ())
        .map_err(|error| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "runtime_shape_is_equal".to_owned(),
                error,
            ))
        })
}

/// Link host interface function, `define_variable`, into the Wasm module.
/// This function is called for all variable definitions (`define-data-var`).
fn link_define_variable_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "define_variable",
            |mut caller: Caller<'_, ClarityWasmContext>,
             name_offset: i32,
             name_length: i32,
             value_offset: i32,
             _value_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let epoch = caller.data_mut().global_context.epoch_id;

                // Read the variable name string from the memory
                let name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;

                // Retrieve the type of this variable
                let value_type = caller
                    .data()
                    .contract_analysis
                    .ok_or(crate::error::wasm_error(
                        WasmError::DefineFunctionCalledInRunMode,
                    ))?
                    .get_persisted_variable_type(name.as_str())
                    .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(
                        "Persisted value".into(),
                    )))?
                    .clone();

                let contract = caller.data().contract_context().contract_identifier.clone();

                // Read the initial value from the memory
                let value =
                    read_from_wasm_indirect(memory, &mut caller, &value_type, value_offset, epoch)?;

                runtime_cost(
                    ClarityCostFunction::CreateVar,
                    caller.data_mut().global_context,
                    value_type.size()?,
                )
                .map_err(VmExecutionError::from)?;

                caller
                    .data_mut()
                    .contract_context_mut()?
                    .persisted_names
                    .insert(ClarityName::try_from(name.clone())?);

                caller
                    .data_mut()
                    .global_context
                    .add_memory(value_type.type_size()? as u64)
                    .map_err(VmExecutionError::from)?;

                caller
                    .data_mut()
                    .global_context
                    .add_memory(value.size()? as u64)
                    .map_err(VmExecutionError::from)?;

                // Create the variable in the global context
                let data_types = caller.data_mut().global_context.database.create_variable(
                    &contract,
                    name.as_str(),
                    value_type,
                )?;

                // Store the variable in the global context
                caller.data_mut().global_context.database.set_variable(
                    &contract,
                    name.as_str(),
                    value,
                    &data_types,
                    &epoch,
                )?;

                // Save the metadata for this variable in the contract context
                caller
                    .data_mut()
                    .contract_context_mut()?
                    .meta_data_var
                    .insert(ClarityName::try_from(name)?, data_types);

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "define_variable".to_string(),
                e,
            ))
        })
}

fn link_define_ft_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "define_ft",
            |mut caller: Caller<'_, ClarityWasmContext>,
             name_offset: i32,
             name_length: i32,
             supply_indicator: i32,
             supply_lo: i64,
             supply_hi: i64| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let contract_identifier = caller
                    .data_mut()
                    .contract_context()
                    .contract_identifier
                    .clone();

                let name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;
                let cname = ClarityName::try_from(name.clone())?;

                let total_supply = if supply_indicator == 1 {
                    Some(((supply_hi as u128) << 64) | supply_lo as u128)
                } else {
                    None
                };

                runtime_cost(
                    ClarityCostFunction::CreateFt,
                    caller.data_mut().global_context,
                    0,
                )
                .map_err(VmExecutionError::from)?;

                caller
                    .data_mut()
                    .contract_context_mut()?
                    .persisted_names
                    .insert(cname.clone());

                caller
                    .data_mut()
                    .global_context
                    .add_memory(TypeSignature::UIntType.type_size()? as u64)
                    .map_err(VmExecutionError::from)?;
                let data_type = caller
                    .data_mut()
                    .global_context
                    .database
                    .create_fungible_token(&contract_identifier, &name, &total_supply)?;

                caller
                    .data_mut()
                    .contract_context_mut()?
                    .meta_ft
                    .insert(cname, data_type);

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "define_ft".to_string(),
                e,
            ))
        })
}

fn link_define_nft_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "define_nft",
            |mut caller: Caller<'_, ClarityWasmContext>, name_offset: i32, name_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let contract_identifier = caller
                    .data_mut()
                    .contract_context()
                    .contract_identifier
                    .clone();

                let name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;
                let cname = ClarityName::try_from(name.clone())?;

                // Get the type of this NFT from the contract analysis
                let asset_type = caller
                    .data()
                    .contract_analysis
                    .ok_or(crate::error::wasm_error(
                        WasmError::DefineFunctionCalledInRunMode,
                    ))?
                    .non_fungible_tokens
                    .get(&cname)
                    .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(
                        "NFT".into(),
                    )))?
                    .clone();

                runtime_cost(
                    ClarityCostFunction::CreateNft,
                    caller.data_mut().global_context,
                    asset_type.size()?,
                )
                .map_err(VmExecutionError::from)?;
                caller
                    .data_mut()
                    .contract_context_mut()?
                    .persisted_names
                    .insert(cname.clone());

                caller
                    .data_mut()
                    .global_context
                    .add_memory(asset_type.type_size()? as u64)
                    .map_err(VmExecutionError::from)?;

                let data_type = caller
                    .data_mut()
                    .global_context
                    .database
                    .create_non_fungible_token(&contract_identifier, &name, &asset_type)?;

                caller
                    .data_mut()
                    .contract_context_mut()?
                    .meta_nft
                    .insert(cname, data_type);

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "define_nft".to_string(),
                e,
            ))
        })
}

fn link_define_map_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "define_map",
            |mut caller: Caller<'_, ClarityWasmContext>, name_offset: i32, name_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let contract_identifier = caller
                    .data_mut()
                    .contract_context()
                    .contract_identifier
                    .clone();

                let name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;
                let cname = ClarityName::try_from(name.clone())?;

                let (key_type, value_type) = caller
                    .data()
                    .contract_analysis
                    .ok_or(crate::error::wasm_error(
                        WasmError::DefineFunctionCalledInRunMode,
                    ))?
                    .get_map_type(&name)
                    .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(
                        "Map".into(),
                    )))?;
                let key_type = key_type.clone();
                let value_type = value_type.clone();
                let total_type_size = u64::from(key_type.size()?)
                    .cost_overflow_add(u64::from(value_type.size()?))
                    .map_err(VmExecutionError::from)?;

                runtime_cost(
                    ClarityCostFunction::CreateMap,
                    caller.data_mut().global_context,
                    total_type_size,
                )
                .map_err(VmExecutionError::from)?;

                caller
                    .data_mut()
                    .contract_context_mut()?
                    .persisted_names
                    .insert(cname.clone());

                caller
                    .data_mut()
                    .global_context
                    .add_memory(key_type.type_size()? as u64)
                    .map_err(VmExecutionError::from)?;
                caller
                    .data_mut()
                    .global_context
                    .add_memory(value_type.type_size()? as u64)
                    .map_err(VmExecutionError::from)?;

                let data_type = caller.data_mut().global_context.database.create_map(
                    &contract_identifier,
                    &name,
                    key_type,
                    value_type,
                )?;

                caller
                    .data_mut()
                    .contract_context_mut()?
                    .meta_data_map
                    .insert(cname, data_type);

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "define_map".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `define_function`, into the Wasm module.
/// This function is called for all function definitions.
fn link_define_function_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "define_function",
            |mut caller: Caller<'_, ClarityWasmContext>,
             kind: i32,
             name_offset: i32,
             name_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                // Read the variable name string from the memory
                let function_name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;
                let function_cname = ClarityName::try_from(function_name.clone())?;

                // Retrieve the kind of function
                let (define_type, function_type) =
                    match kind {
                        0 => (
                            DefineType::ReadOnly,
                            caller
                                .data()
                                .contract_analysis
                                .ok_or(crate::error::wasm_error(
                                    WasmError::DefineFunctionCalledInRunMode,
                                ))?
                                .get_read_only_function_type(&function_name)
                                .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(
                                    format!("Read-only function: {}", function_name),
                                )))?,
                        ),
                        1 => (
                            DefineType::Public,
                            caller
                                .data()
                                .contract_analysis
                                .ok_or(crate::error::wasm_error(
                                    WasmError::DefineFunctionCalledInRunMode,
                                ))?
                                .get_public_function_type(&function_name)
                                .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(
                                    format!("Public function: {}", function_name),
                                )))?,
                        ),
                        2 => (
                            DefineType::Private,
                            caller
                                .data()
                                .contract_analysis
                                .ok_or(crate::error::wasm_error(
                                    WasmError::DefineFunctionCalledInRunMode,
                                ))?
                                .get_private_function(&function_name)
                                .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(
                                    format!("Private function: {}", function_name),
                                )))?,
                        ),
                        _ => Err(crate::error::wasm_error(WasmError::InvalidFunctionKind(
                            format!("Invalid number identifier: {kind}"),
                        )))?,
                    };

                let fixed_type = match function_type {
                    FunctionType::Fixed(fixed_type) => fixed_type,
                    _ => Err(crate::error::wasm_error(WasmError::InvalidFunctionKind(
                        "Expected fixed function for definition".into(),
                    )))?,
                };

                let function = DefinedFunction::new(
                    fixed_type
                        .args
                        .iter()
                        .map(|arg| (arg.name.clone(), arg.signature.clone()))
                        .collect(),
                    // TODO: We don't actually need the body here, so we
                    // should be able to remove it. For now, this is a
                    // placeholder.
                    SymbolicExpression::literal_value(Value::Int(0)),
                    define_type,
                    &function_cname,
                    &caller
                        .data()
                        .contract_context()
                        .contract_identifier
                        .to_string(),
                );

                runtime_cost(
                    ClarityCostFunction::BindName,
                    caller.data_mut().global_context,
                    0,
                )
                .map_err(VmExecutionError::from)?;

                // Insert this function into the context
                caller
                    .data_mut()
                    .contract_context_mut()?
                    .functions
                    .insert(function_cname, function);

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "define_function".to_string(),
                e,
            ))
        })
}

fn link_define_trait_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "define_trait",
            |mut caller: Caller<'_, ClarityWasmContext>, name_offset: i32, name_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;
                let cname = ClarityName::try_from(name.clone())?;

                let trait_def = caller
                    .data()
                    .contract_analysis
                    .ok_or(crate::error::wasm_error(
                        WasmError::DefineFunctionCalledInRunMode,
                    ))?
                    .get_defined_trait(name.as_str())
                    .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(
                        "Trait".into(),
                    )))?;

                caller
                    .data_mut()
                    .contract_context_mut()?
                    .defined_traits
                    .insert(cname, trait_def.clone());

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "define_map".to_string(),
                e,
            ))
        })
}

fn link_impl_trait_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "impl_trait",
            |mut caller: Caller<'_, ClarityWasmContext>, name_offset: i32, name_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let trait_id_string =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;
                let trait_id = TraitIdentifier::parse_fully_qualified(trait_id_string.as_str())?;

                caller
                    .data_mut()
                    .contract_context_mut()?
                    .implemented_traits
                    .insert(trait_id);

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "define_map".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `get_variable`, into the Wasm module.
/// This function is called for all variable lookups (`var-get`).
fn link_get_variable_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_variable",
            |mut caller: Caller<'_, ClarityWasmContext>,
             name_offset: i32,
             name_length: i32,
             return_offset: i32,
             _return_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                // Retrieve the variable name for this identifier
                let var_name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;

                let contract = caller.data().contract_context().contract_identifier.clone();
                let epoch = caller.data_mut().global_context.epoch_id;

                // Retrieve the metadata for this variable
                let data_types = caller
                    .data()
                    .contract_context()
                    .meta_data_var
                    .get(var_name.as_str())
                    .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(format!(
                        "Variable {}",
                        var_name
                    ))))?
                    .clone();

                // We would like to call `lookup_variable_with_size`, but since it
                // returns `Ok(none)` even if the variable is missing, we have no way
                // to distinguish between a valid `none` and a missing variable.
                // So here we replicate `lookup_variable_with_size` impl.
                let key = ClarityDatabase::make_key_for_trip(
                    &contract,
                    StoreType::Variable,
                    var_name.as_str(),
                );
                let fetch_result = crate::phases::time(crate::phases::Phase::HostVar, || {
                    caller.data_mut().global_context.database.get_value(
                        &key,
                        &data_types.value_type,
                        &epoch,
                    )
                })?;

                let value = fetch_result
                    .map(|data| data.value)
                    .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(format!(
                        "Value {var_name}"
                    ))))?;

                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                write_to_wasm(
                    &mut caller,
                    memory,
                    &data_types.value_type,
                    return_offset,
                    return_offset + get_type_size(&data_types.value_type),
                    &value,
                    true,
                )?;

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_variable".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `set_variable`, into the Wasm module.
/// This function is called for all variable assignments (`var-set`).
fn link_set_variable_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "set_variable",
            |mut caller: Caller<'_, ClarityWasmContext>,
             name_offset: i32,
             name_length: i32,
             value_offset: i32,
             _value_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let epoch = caller.data_mut().global_context.epoch_id;

                // Retrieve the variable name for this identifier
                let var_name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;

                let contract = caller.data().contract_context().contract_identifier.clone();

                let data_types = caller
                    .data()
                    .contract_context()
                    .meta_data_var
                    .get(var_name.as_str())
                    .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(format!(
                        "Variable {}",
                        var_name
                    ))))?
                    .clone();

                // Read in the value from the Wasm memory
                let value = read_from_wasm_indirect(
                    memory,
                    &mut caller,
                    &data_types.value_type,
                    value_offset,
                    epoch,
                )?;

                // TODO: Include this cost
                // env.add_memory(value.get_memory_use())?;

                // Store the variable in the global context
                crate::phases::time(crate::phases::Phase::HostVar, || {
                    caller.data_mut().global_context.database.set_variable(
                        &contract,
                        var_name.as_str(),
                        value,
                        &data_types,
                        &epoch,
                    )
                })?;

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "set_variable".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `tx_sender`, into the Wasm module.
/// This function is called for use of the builtin variable, `tx-sender`.
fn link_tx_sender_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "tx_sender",
            |mut caller: Caller<'_, ClarityWasmContext>,
             return_offset: i32,
             _return_length: i32| {
                let sender = caller
                    .data()
                    .sender
                    .clone()
                    .ok_or(VmExecutionError::Runtime(
                        RuntimeError::NoSenderInContext,
                        None,
                    ))?;

                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let (_, bytes_written) = write_to_wasm(
                    &mut caller,
                    memory,
                    &TypeSignature::PrincipalType,
                    return_offset,
                    return_offset,
                    &Value::Principal(sender),
                    false,
                )?;

                Ok((return_offset, bytes_written))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "tx_sender".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `contract_caller`, into the Wasm module.
/// This function is called for use of the builtin variable, `contract-caller`.
fn link_contract_caller_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "contract_caller",
            |mut caller: Caller<'_, ClarityWasmContext>,
             return_offset: i32,
             _return_length: i32| {
                let contract_caller =
                    caller
                        .data()
                        .caller
                        .clone()
                        .ok_or(VmExecutionError::Runtime(
                            RuntimeError::NoCallerInContext,
                            None,
                        ))?;

                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let (_, bytes_written) = write_to_wasm(
                    &mut caller,
                    memory,
                    &TypeSignature::PrincipalType,
                    return_offset,
                    return_offset,
                    &Value::Principal(contract_caller),
                    false,
                )?;

                Ok((return_offset, bytes_written))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "contract_caller".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `current_contract`, into the Wasm module.
/// This function is called for use of the builtin variable, `current-contract`.
fn link_current_contract_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "current_contract",
            |mut caller: Caller<'_, ClarityWasmContext>,
             return_offset: i32,
             _return_length: i32| {
                let contract = caller.data().contract_context().contract_identifier.clone();

                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let (_, bytes_written) = write_to_wasm(
                    &mut caller,
                    memory,
                    &TypeSignature::PrincipalType,
                    return_offset,
                    return_offset,
                    &Value::Principal(PrincipalData::Contract(contract)),
                    false,
                )?;

                Ok((return_offset, bytes_written))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "current_contract".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `tx_sponsor`, into the Wasm module.
/// This function is called for use of the builtin variable, `tx-sponsor`.
fn link_tx_sponsor_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "tx_sponsor",
            |mut caller: Caller<'_, ClarityWasmContext>,
             return_offset: i32,
             _return_length: i32| {
                let opt_sponsor = caller.data().sponsor.clone();
                if let Some(sponsor) = opt_sponsor {
                    let memory = caller
                        .data()
                        .memory
                        .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                    let (_, bytes_written) = write_to_wasm(
                        &mut caller,
                        memory,
                        &TypeSignature::PrincipalType,
                        return_offset,
                        return_offset,
                        &Value::Principal(sponsor),
                        false,
                    )?;

                    Ok((1i32, return_offset, bytes_written))
                } else {
                    Ok((0i32, return_offset, 0i32))
                }
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "tx_sponsor".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `block_height`, into the Wasm module.
/// This function is called for use of the builtin variable, `block-height`.
fn link_block_height_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "block_height",
            |mut caller: Caller<'_, ClarityWasmContext>| {
                // From epoch 3.0, `block-height` is the *tenure* height, not the
                // Stacks block height: the interpreter switched (see
                // `vm::variables`, `NativeVariables::BlockHeight`) so that the
                // value keeps incrementing at roughly its old pace, and a
                // contract that stores it stores a consensus-visible number.
                let epoch = caller.data_mut().global_context.epoch_id;
                let height = if epoch < StacksEpochId::Epoch30 {
                    u128::from(
                        caller
                            .data_mut()
                            .global_context
                            .database
                            .get_current_block_height(),
                    )
                } else {
                    u128::from(
                        caller
                            .data_mut()
                            .global_context
                            .database
                            .get_tenure_height()?,
                    )
                };
                Ok((height as i64, 0i64))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "block_height".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `stacks_block_height`, into the Wasm module.
/// This function is called for use of the builtin variable, `stacks_block-height`.
fn link_stacks_block_height_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "stacks_block_height",
            |mut caller: Caller<'_, ClarityWasmContext>| {
                let height = caller
                    .data_mut()
                    .global_context
                    .database
                    .get_current_block_height();
                Ok((height as i64, 0i64))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "stacks_block_height".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `stacks_block_time`, into the Wasm module.
/// This function is called for use of the builtin variable, `stacks-block-time`.
fn link_stacks_block_time_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "stacks_block_time",
            |mut caller: Caller<'_, ClarityWasmContext>| {
                let block_time = caller
                    .data_mut()
                    .global_context
                    .database
                    .get_current_block_time()?;
                Ok((block_time as i64, 0i64))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "stacks_block_time".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `tenure_height`, into the Wasm module.
/// This function is called for use of the builtin variable, `tenure-height`.
fn link_tenure_height_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "tenure_height",
            |mut caller: Caller<'_, ClarityWasmContext>| {
                let height = caller
                    .data_mut()
                    .global_context
                    .database
                    .get_tenure_height()?;
                Ok((height as i64, 0i64))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "tenure_height".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `burn_block_height`, into the Wasm module.
/// This function is called for use of the builtin variable,
/// `burn-block-height`.
fn link_burn_block_height_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "burn_block_height",
            |mut caller: Caller<'_, ClarityWasmContext>| {
                let height = caller
                    .data_mut()
                    .global_context
                    .database
                    .get_current_burnchain_block_height()?;
                Ok((height as i64, 0i64))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "burn_block_height".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `stx_liquid_supply`, into the Wasm module.
/// This function is called for use of the builtin variable,
/// `stx-liquid-supply`.
fn link_stx_liquid_supply_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "stx_liquid_supply",
            |mut caller: Caller<'_, ClarityWasmContext>| {
                let supply = caller
                    .data_mut()
                    .global_context
                    .database
                    .get_total_liquid_ustx()?;
                let upper = (supply >> 64) as u64;
                let lower = supply as u64;
                Ok((lower as i64, upper as i64))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "stx_liquid_supply".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `is_in_regtest`, into the Wasm module.
/// This function is called for use of the builtin variable,
/// `is-in-regtest`.
fn link_is_in_regtest_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "is_in_regtest",
            |caller: Caller<'_, ClarityWasmContext>| {
                if caller.data().global_context.database.is_in_regtest() {
                    Ok(1i32)
                } else {
                    Ok(0i32)
                }
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "is_in_regtest".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `is_in_mainnet`, into the Wasm module.
/// This function is called for use of the builtin variable,
/// `is-in-mainnet`.
fn link_is_in_mainnet_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "is_in_mainnet",
            |caller: Caller<'_, ClarityWasmContext>| {
                if caller.data().global_context.mainnet {
                    Ok(1i32)
                } else {
                    Ok(0i32)
                }
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "is_in_mainnet".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `chain_id`, into the Wasm module.
/// This function is called for use of the builtin variable,
/// `chain-id`.
fn link_chain_id_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "chain_id",
            |caller: Caller<'_, ClarityWasmContext>| {
                let chain_id = caller.data().global_context.chain_id;
                Ok((chain_id as i64, 0i64))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "chain_id".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `enter_as_contract`, into the Wasm module.
/// This function is called before processing the inner-expression of
/// `as-contract`.
fn link_enter_as_contract_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "enter_as_contract",
            |mut caller: Caller<'_, ClarityWasmContext>| {
                let contract_principal: PrincipalData = caller
                    .data()
                    .contract_context()
                    .contract_identifier
                    .clone()
                    .into();
                caller.data_mut().push_sender(contract_principal.clone());
                caller.data_mut().push_caller(contract_principal);
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "enter_as_contract".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `exit_as_contract`, into the Wasm module.
/// This function is called after processing the inner-expression of
/// `as-contract`, and is used to restore the caller and sender.
fn link_exit_as_contract_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "exit_as_contract",
            |mut caller: Caller<'_, ClarityWasmContext>| {
                caller.data_mut().pop_sender()?;
                caller.data_mut().pop_caller()?;
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "exit_as_contract".to_string(),
                e,
            ))
        })
}

/// Link host interface functions that record and restore how deep the sender
/// and caller stacks are.
///
/// `as-contract` pushes on entry and pops on exit, and an early return out of
/// its body branches straight past the pop — so the sender stays switched and
/// the *next* call inherits it. Mainnet block 8,668,161 is that: a function
/// that `asserts!` its way out of `as-contract`, called twice by `map`, whose
/// second call then paid itself and answered `(err u2)`.
///
/// A function records the depth on entry and unwinds to it on the way out, so
/// no path can leave the stacks deeper than it found them.
fn link_principal_depth_fns(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "principal_depth",
            |caller: Caller<'_, ClarityWasmContext>| {
                let (sender, caller_depth) = caller.data().principal_depth();
                Ok((sender as i32, caller_depth as i32))
            },
        )
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "principal_depth".to_string(),
                e,
            ))
        })?;
    linker
        .func_wrap(
            "clarity",
            "restore_principal_depth",
            |mut caller: Caller<'_, ClarityWasmContext>, sender: i32, callers: i32| {
                caller
                    .data_mut()
                    .restore_principal_depth((sender.max(0) as usize, callers.max(0) as usize));
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "restore_principal_depth".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `enter_as_contract_safe`, into the Wasm module.
/// This function is called before processing the allowances and inner-expressions of
/// `as-contract?`.
fn link_enter_as_contract_safe_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "enter_as_contract_safe",
            |mut caller: Caller<'_, ClarityWasmContext>| {
                let contract_principal: PrincipalData = caller
                    .data()
                    .contract_context()
                    .contract_identifier
                    .clone()
                    .into();
                let allowance = ExternRef::new(&mut caller, AllowanceContext::new())?;
                caller.data_mut().global_context.begin();
                caller.data_mut().push_sender(contract_principal.clone());
                caller.data_mut().push_caller(contract_principal);

                Ok(Some(allowance))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "enter_as_contract_safe".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `exit_as_contract_safe`, into the Wasm module.
/// This function is called after processing the inner-expressions of
/// `as-contract?`, and is used to restore the caller, sender and check allowances.
fn link_exit_as_contract_safe_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "exit_as_contract_safe",
            |mut caller: Caller<'_, ClarityWasmContext>,
             allowance_ref: Option<Rooted<ExternRef>>| {
                let epoch = caller.data().global_context.epoch_id;

                // we need to restore the current caller and sender. We pop both and check if we did set
                // them correctly before. We keep the sender (current-contract) as owner for checking the
                // allowances.
                let owner = {
                    let owner = caller.data_mut().pop_sender();
                    let _ = caller.data_mut().pop_caller()?;
                    owner?
                };

                let allowances = AllowanceContext::extract(&caller, &allowance_ref)?;

                let asset_map = caller.data_mut().global_context.get_readonly_asset_map()?;

                match check_allowances(&owner, allowances, asset_map, epoch)? {
                    None => {
                        caller.data_mut().global_context.commit()?;
                        Ok((0i64, 0i64, 1i32)) // no violation
                    }
                    Some(violation_index) => {
                        caller.data_mut().global_context.roll_back()?;
                        let lo = violation_index as i64;
                        let hi = (violation_index >> 64) as i64;
                        Ok((lo, hi, 0i32)) // violation — Wasm returns (err index)
                    }
                }
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "exit_as_contract_safe".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `cleanup_as_contract_safe`, into the Wasm module.
/// This function is called after processing the inner-expression of
/// `as-contract?`, and is used to restore the caller and sender in the case where
/// an inner-expresion failed.
fn link_cleanup_as_contract_safe_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "cleanup_as_contract_safe",
            |mut caller: Caller<'_, ClarityWasmContext>| {
                // we need to restore the current caller and sender. We pop both and check if we did set
                // them correctly before.
                let sender = caller.data_mut().pop_sender();
                let _ = caller.data_mut().pop_caller()?;
                let _ = sender?;

                caller.data_mut().global_context.roll_back()?;

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "cleanup_as_contract_safe".to_string(),
                e,
            ))
        })
}

fn link_enter_restrict_assets_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "enter_restrict_assets",
            |mut caller: Caller<'_, ClarityWasmContext>| {
                let allowance = ExternRef::new(&mut caller, AllowanceContext::new())?;
                caller.data_mut().global_context.begin();

                Ok(Some(allowance))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "enter_restrict_assets".to_string(),
                e,
            ))
        })
}

fn link_exit_restrict_assets_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "exit_restrict_assets",
            |mut caller: Caller<'_, ClarityWasmContext>,
             asset_owner_offset: i32,
             asset_owner_length: i32,
             allowance_ref: Option<Rooted<ExternRef>>| {
                let memory = caller.data().memory.ok_or(WasmError::MemoryNotFound)?;
                let epoch = caller.data().global_context.epoch_id;
                let owner = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::PrincipalType,
                    asset_owner_offset,
                    asset_owner_length,
                    epoch,
                )?
                .expect_principal()?;
                let allowances = AllowanceContext::extract(&caller, &allowance_ref)?;

                let asset_map = caller.data_mut().global_context.get_readonly_asset_map()?;

                match check_allowances(&owner, allowances, asset_map, epoch)? {
                    None => {
                        caller.data_mut().global_context.commit()?;
                        Ok((0i64, 0i64, 1i32)) // no violation
                    }
                    Some(violation_index) => {
                        caller.data_mut().global_context.roll_back()?;
                        let lo = violation_index as i64;
                        let hi = (violation_index >> 64) as i64;
                        Ok((lo, hi, 0i32)) // violation — Wasm returns (err index)
                    }
                }
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "exit_restrict_assets".to_string(),
                e,
            ))
        })
}

fn link_cleanup_restrict_assets_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "cleanup_restrict_assets",
            |mut caller: Caller<'_, ClarityWasmContext>| {
                caller.data_mut().global_context.roll_back()?;

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "cleanup_restrict_assets".to_string(),
                e,
            ))
        })
}

/// Holds the list of allowances for an `as-contract?` block.
/// Passed through WASM as an `ExternRef` handle.
// Needs a `Mutex` because we use an old version of WasmTime that
// doesn’t allow to use a mutable reference to the inner data.
// TODO: After WasmTime update, remove the mutex.
struct AllowanceContext(std::sync::Mutex<Vec<Allowance>>);

impl AllowanceContext {
    fn new() -> Self {
        Self(std::sync::Mutex::new(Vec::new()))
    }

    fn from_externref<'a>(
        caller: &'a Caller<'_, ClarityWasmContext>,
        externref: &Option<Rooted<ExternRef>>,
    ) -> Result<&'a Self, VmExecutionError> {
        let externref = externref.as_ref().ok_or_else(|| {
            crate::error::wasm_error(WasmError::WasmGeneratorError(
                "allowance context is missing".to_string(),
            ))
        })?;
        externref
            .data(caller)
            .map_err(|error| crate::error::wasm_error(WasmError::Runtime(error)))?
            .ok_or_else(|| {
                crate::error::wasm_error(WasmError::WasmGeneratorError(
                    "allowance context has no host data".to_string(),
                ))
            })?
            .downcast_ref::<AllowanceContext>()
            .ok_or_else(|| {
                crate::error::wasm_error(WasmError::WasmGeneratorError(
                    "allowance context has wrong type".to_string(),
                ))
            })
    }

    fn push(
        caller: &Caller<'_, ClarityWasmContext>,
        externref: &Option<Rooted<ExternRef>>,
        allowance: Allowance,
    ) -> Result<(), VmExecutionError> {
        let ctx = Self::from_externref(caller, externref)?;
        ctx.0
            .lock()
            .map_err(|e| crate::error::wasm_error(WasmError::Expect(e.to_string())))?
            .push(allowance);
        Ok(())
    }

    fn extract(
        caller: &Caller<'_, ClarityWasmContext>,
        externref: &Option<Rooted<ExternRef>>,
    ) -> Result<Vec<Allowance>, VmExecutionError> {
        let ctx = Self::from_externref(caller, externref)?;
        Ok(std::mem::take(&mut *ctx.0.lock().map_err(|e| {
            crate::error::wasm_error(WasmError::Expect(e.to_string()))
        })?))
    }
}

/// Link host interface function, `with_all_assets_unsafe`, into the Wasm module.
/// This function is called before processing the inner-expression of
/// `with-all-assets-unsafe`.
fn link_with_all_assets_unsafe_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "with_all_assets_unsafe",
            |caller: Caller<'_, ClarityWasmContext>, allowance_ref: Option<Rooted<ExternRef>>| {
                AllowanceContext::push(&caller, &allowance_ref, Allowance::All)?;

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "with_all_assets_unsafe".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `with_ft`, into the Wasm module.
/// This function is called before processing the inner-expression of
/// `with-ft`. The asset identifier and allowance should already be written to memory.
fn link_with_ft_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "with_ft",
            |mut caller: Caller<'_, ClarityWasmContext>,
             allowance_ref: Option<Rooted<ExternRef>>,
             contract_id_offset: i32,
             contract_id_length: i32,
             token_name_offset: i32,
             token_name_length: i32,
             amount_lo: i64,
             amount_hi: i64| {
                let memory = caller.data().memory.ok_or(WasmError::MemoryNotFound)?;

                let epoch = caller.data().global_context.epoch_id;
                let token_name_value = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::STRING_ASCII_MAX,
                    token_name_offset,
                    token_name_length,
                    epoch,
                )?;
                let token_name = token_name_value.expect_ascii()?;

                let allowed_amount = ((amount_hi as u128) << 64) | ((amount_lo as u64) as u128);

                let contract_principal = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::PrincipalType,
                    contract_id_offset,
                    contract_id_length,
                    epoch,
                )?;
                let contract_id = match &contract_principal {
                    Value::Principal(PrincipalData::Contract(contract_id)) => contract_id,
                    _ => {
                        return Err(RuntimeCheckErrorKind::ContractCallExpectName.into());
                    }
                };

                // No check that the token exists, for the same reason `with_nft`
                // makes none: the reference builds this allowance out of its
                // three arguments and never reads a contract
                // (`check_allowance_with_ft` type-checks a principal, a string
                // and a `uint`, and that is all). An allowance naming a token
                // that is not there allows nothing, which is a perfectly
                // ordinary thing for a router to write; refusing it fails the
                // whole block, as mainnet 8,671,301 did for the NFT spelling of
                // this. Nothing needed the lookup either — unlike `with-nft`
                // there is no list whose element type has to be recovered.
                AllowanceContext::push(
                    &caller,
                    &allowance_ref,
                    Allowance::Ft(FtAllowance {
                        asset: AssetIdentifier {
                            contract_identifier: contract_id.clone(),
                            asset_name: ClarityName::try_from(token_name)?,
                        },
                        amount: allowed_amount,
                    }),
                )?;

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "with_ft".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `with_nft`, into the Wasm module.
/// This function is called before processing the inner-expression of
/// `with-nft`. The asset identifier and allowance should already be written to memory.
fn link_with_nft_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "with_nft",
            |mut caller: Caller<'_, ClarityWasmContext>,
             allowance_ref: Option<Rooted<ExternRef>>,
             contract_id_offset: i32,
             contract_id_length: i32,
             token_name_offset: i32,
             token_name_length: i32,
             identifiers_shape: i32,
             identifiers_offset: i32,
             identifiers_length: i32,
             identifiers_ty_offset: i32,
             identifiers_ty_length: i32| {
                let memory = caller.data().memory.ok_or(WasmError::MemoryNotFound)?;

                let epoch = caller.data().global_context.epoch_id;
                // we cannot just read an identifier due to the '*' case.
                let token_name = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::STRING_ASCII_MAX,
                    token_name_offset,
                    token_name_length,
                    epoch,
                )?
                .expect_ascii()?;
                let asset_name = ClarityName::try_from(token_name.clone())?;
                // Read the contract principal first — needed for both wildcard
                // and non-wildcard paths.
                let contract_principal = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::PrincipalType,
                    contract_id_offset,
                    contract_id_length,
                    epoch,
                )?;
                let contract_id = match &contract_principal {
                    Value::Principal(PrincipalData::Contract(contract_id)) => contract_id,
                    _ => {
                        return Err(RuntimeCheckErrorKind::ContractCallExpectName.into());
                    }
                };

                // The type of the identifiers list comes from the compiler, not
                // from an NFT definition. The reference builds this allowance
                // out of its three arguments and checks nothing about the asset
                // (`check_allowance_with_nft` requires only that the third is a
                // list, and `special_allowance` reads it as a `Value`) — so an
                // allowance may perfectly well name an asset that exists in
                // neither the calling nor the named contract, and mainnet
                // 8,671,301 does: `xtrata-market-sponsored-stx-v1-1::buy`
                // allows another contract's `xtrata-inscription` and defines no
                // NFT of its own. Taking a key type from the calling contract's
                // `meta_nft` refused that call outright, where the chain accepted
                // it, and reading the named contract at all is a database lookup
                // the reference never makes.
                let (_, identifiers_ty) =
                    runtime_value_type(&mut caller, identifiers_ty_offset, identifiers_ty_length)?;

                // This is a typed list of NFT key values, not a string. A
                // nonzero shape handle is authoritative when the list crossed
                // a preserving run-time-shape boundary.
                let identifiers_value = if identifiers_shape == 0 {
                    read_from_wasm(
                        memory,
                        &mut caller,
                        &identifiers_ty,
                        identifiers_offset,
                        identifiers_length,
                        epoch,
                    )?
                } else {
                    caller.data().load_runtime_shape(identifiers_shape)?
                };
                let allowed_identifiers = identifiers_value.expect_list()?;

                let asset_identifier = AssetIdentifier {
                    contract_identifier: contract_id.clone(),
                    asset_name,
                };

                AllowanceContext::push(
                    &caller,
                    &allowance_ref,
                    Allowance::Nft(NftAllowance {
                        asset: asset_identifier,
                        asset_ids: allowed_identifiers,
                    }),
                )?;

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "with_nft".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `with_stacking`, into the Wasm module.
/// This function is called before processing the inner-expression of
/// `with-stacking`. The allowance should already be written to memory.
fn link_with_stacking_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "with_stacking",
            |caller: Caller<'_, ClarityWasmContext>,
             allowance_ref: Option<Rooted<ExternRef>>,
             allowance_lo: i64,
             allowance_hi: i64| {
                let allowance = ((allowance_hi as u128) << 64) | ((allowance_lo as u64) as u128);

                AllowanceContext::push(
                    &caller,
                    &allowance_ref,
                    Allowance::Stacking(StackingAllowance { amount: allowance }),
                )?;

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "with_stacking".to_string(),
                e,
            ))
        })
}

fn link_with_pox_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "with_pox",
            |caller: Caller<'_, ClarityWasmContext>, allowance_ref: Option<Rooted<ExternRef>>| {
                AllowanceContext::push(&caller, &allowance_ref, Allowance::Pox)?;
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|error| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "with_pox".into(),
                error,
            ))
        })
}

/// Link host interface function, `with_stx`, into the Wasm module.
/// This function is called before processing the inner-expression of
/// `with-stx`. The allowance should already be written to memory.
fn link_with_stx_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "with_stx",
            |caller: Caller<'_, ClarityWasmContext>,
             allowance_ref: Option<Rooted<ExternRef>>,
             amount_lo: i64,
             amount_hi: i64| {
                let allowed_amount = ((amount_hi as u128) << 64) | ((amount_lo as u64) as u128);

                AllowanceContext::push(
                    &caller,
                    &allowance_ref,
                    Allowance::Stx(StxAllowance {
                        amount: allowed_amount,
                    }),
                )?;

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "with_stx".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `stx_get_balance`, into the Wasm module.
/// This function is called for the clarity expression, `stx-get-balance`.
fn link_stx_get_balance_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "stx_get_balance",
            |mut caller: Caller<'_, ClarityWasmContext>,
             principal_offset: i32,
             principal_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let epoch = caller.data_mut().global_context.epoch_id;
                // Read the principal from the Wasm memory
                let value = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::PrincipalType,
                    principal_offset,
                    principal_length,
                    epoch,
                )?;
                let principal = value_as_principal(&value)?;

                let balance = {
                    let mut snapshot = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_stx_balance_snapshot(principal)?;
                    snapshot.get_available_balance()?
                };
                let high = (balance >> 64) as u64;
                let low = (balance & 0xffff_ffff_ffff_ffff) as u64;
                Ok((low, high))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "stx_get_balance".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `stx_account`, into the Wasm module.
/// This function is called for the clarity expression, `stx-account`.
fn link_stx_account_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "stx_account",
            |mut caller: Caller<'_, ClarityWasmContext>,
             principal_offset: i32,
             principal_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let epoch = caller.data_mut().global_context.epoch_id;
                // Read the principal from the Wasm memory
                let value = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::PrincipalType,
                    principal_offset,
                    principal_length,
                    epoch,
                )?;
                let principal = value_as_principal(&value)?;

                let account = {
                    let mut snapshot = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_stx_balance_snapshot(principal)?;
                    snapshot.canonical_balance_repr()?
                };
                let v1_unlock_ht = caller
                    .data_mut()
                    .global_context
                    .database
                    .get_v1_unlock_height();
                let v2_unlock_ht = caller
                    .data_mut()
                    .global_context
                    .database
                    .get_v2_unlock_height()?;
                let v3_unlock_ht = caller
                    .data_mut()
                    .global_context
                    .database
                    .get_v3_unlock_height()?;
                let v4_unlock_ht = caller
                    .data_mut()
                    .global_context
                    .database
                    .get_v4_unlock_height()?;

                let locked = account.amount_locked();
                let locked_high = (locked >> 64) as u64;
                let locked_low = (locked & 0xffff_ffff_ffff_ffff) as u64;
                let unlock_height = account.effective_unlock_height(
                    v1_unlock_ht,
                    v2_unlock_ht,
                    v3_unlock_ht,
                    v4_unlock_ht,
                );
                let unlocked = account.amount_unlocked();
                let unlocked_high = (unlocked >> 64) as u64;
                let unlocked_low = (unlocked & 0xffff_ffff_ffff_ffff) as u64;

                // Return value is a tuple: `{locked: uint, unlock-height: uint, unlocked: uint}`
                Ok((
                    0i32,
                    locked_low,
                    locked_high,
                    unlock_height as i64,
                    0i64,
                    unlocked_low,
                    unlocked_high,
                ))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "stx_account".to_string(),
                e,
            ))
        })
}

fn link_stx_burn_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "stx_burn",
            |mut caller: Caller<'_, ClarityWasmContext>,
             amount_lo: i64,
             amount_hi: i64,
             principal_offset: i32,
             principal_length: i32| {
                let amount = ((amount_hi as u128) << 64) | ((amount_lo as u64) as u128);

                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let epoch = caller.data_mut().global_context.epoch_id;
                // Read the principal from the Wasm memory
                let value = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::PrincipalType,
                    principal_offset,
                    principal_length,
                    epoch,
                )?;
                let from = value_as_principal(&value)?;

                if amount == 0 {
                    return Ok((0i32, 0i32, StxErrorCodes::NON_POSITIVE_AMOUNT as i64, 0i64));
                }

                if Some(from) != caller.data().sender.as_ref() {
                    return Ok((
                        0i32,
                        0i32,
                        StxErrorCodes::SENDER_IS_NOT_TX_SENDER as i64,
                        0i64,
                    ));
                }

                caller
                    .data_mut()
                    .global_context
                    .add_memory(TypeSignature::PrincipalType.size()? as u64)
                    .map_err(VmExecutionError::from)?;
                caller
                    .data_mut()
                    .global_context
                    .add_memory(STXBalance::unlocked_and_v1_size as u64)
                    .map_err(VmExecutionError::from)?;

                let mut burner_snapshot = caller
                    .data_mut()
                    .global_context
                    .database
                    .get_stx_balance_snapshot(from)?;
                if !burner_snapshot.can_transfer(amount)? {
                    return Ok((0i32, 0i32, StxErrorCodes::NOT_ENOUGH_BALANCE as i64, 0i64));
                }

                burner_snapshot.debit(amount)?;
                burner_snapshot.save()?;

                caller
                    .data_mut()
                    .global_context
                    .database
                    .decrement_ustx_liquid_supply(amount)?;

                caller
                    .data_mut()
                    .global_context
                    .log_stx_burn(from, amount)?;
                caller
                    .data_mut()
                    .register_stx_burn_event(from.clone(), amount)?;

                // (ok true)
                Ok((1i32, 1i32, 0i64, 0i64))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "stx_burn".to_string(),
                e,
            ))
        })
}

fn link_stx_transfer_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "stx_transfer",
            |mut caller: Caller<'_, ClarityWasmContext>,
             amount_lo: i64,
             amount_hi: i64,
             sender_offset: i32,
             sender_length: i32,
             recipient_offset: i32,
             recipient_length: i32,
             memo_offset: i32,
             memo_length: i32| {
                let amount = ((amount_hi as u128) << 64) | ((amount_lo as u64) as u128);

                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let epoch = caller.data_mut().global_context.epoch_id;
                // Read the sender principal from the Wasm memory
                let value = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::PrincipalType,
                    sender_offset,
                    sender_length,
                    epoch,
                )?;
                let sender = value_as_principal(&value)?;

                // Read the to principal from the Wasm memory
                let value = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::PrincipalType,
                    recipient_offset,
                    recipient_length,
                    epoch,
                )?;
                let recipient = value_as_principal(&value)?;
                // Read the memo from the Wasm memory
                let memo = if memo_length > 0 {
                    let value = read_from_wasm(
                        memory,
                        &mut caller,
                        &TypeSignature::SequenceType(SequenceSubtype::BufferType(
                            BufferLength::try_from(memo_length as u32)?,
                        )),
                        memo_offset,
                        memo_length,
                        epoch,
                    )?;
                    value_as_buffer(value)?
                } else {
                    BuffData::empty()
                };

                if amount == 0 {
                    return Ok((0i32, 0i32, StxErrorCodes::NON_POSITIVE_AMOUNT as i64, 0i64));
                }

                if sender == recipient {
                    return Ok((0i32, 0i32, StxErrorCodes::SENDER_IS_RECIPIENT as i64, 0i64));
                }

                if Some(sender) != caller.data().sender.as_ref() {
                    return Ok((
                        0i32,
                        0i32,
                        StxErrorCodes::SENDER_IS_NOT_TX_SENDER as i64,
                        0i64,
                    ));
                }

                // loading sender/recipient principals and balances
                caller
                    .data_mut()
                    .global_context
                    .add_memory(TypeSignature::PrincipalType.size()? as u64)
                    .map_err(VmExecutionError::from)?;
                caller
                    .data_mut()
                    .global_context
                    .add_memory(TypeSignature::PrincipalType.size()? as u64)
                    .map_err(VmExecutionError::from)?;
                // loading sender's locked amount and height
                // TODO: this does not count the inner stacks block header load, but arguably,
                // this could be optimized away, so it shouldn't penalize the caller.
                caller
                    .data_mut()
                    .global_context
                    .add_memory(STXBalance::unlocked_and_v1_size as u64)
                    .map_err(VmExecutionError::from)?;
                caller
                    .data_mut()
                    .global_context
                    .add_memory(STXBalance::unlocked_and_v1_size as u64)
                    .map_err(VmExecutionError::from)?;

                let mut sender_snapshot =
                    crate::phases::time(crate::phases::Phase::HostStx, || {
                        caller
                            .data_mut()
                            .global_context
                            .database
                            .get_stx_balance_snapshot(sender)
                    })?;
                if !sender_snapshot.can_transfer(amount)? {
                    return Ok((0i32, 0i32, StxErrorCodes::NOT_ENOUGH_BALANCE as i64, 0i64));
                }

                crate::phases::time(crate::phases::Phase::HostStx, || {
                    sender_snapshot.transfer_to(recipient, amount)
                })?;

                caller
                    .data_mut()
                    .global_context
                    .log_stx_transfer(sender, amount)?;
                caller.data_mut().register_stx_transfer_event(
                    sender.clone(),
                    recipient.clone(),
                    amount,
                    memo,
                )?;

                // (ok true)
                Ok((1i32, 1i32, 0i64, 0i64))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "stx_transfer".to_string(),
                e,
            ))
        })
}

fn link_ft_get_supply_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "ft_get_supply",
            |mut caller: Caller<'_, ClarityWasmContext>, name_offset: i32, name_length: i32| {
                let contract_identifier =
                    caller.data().contract_context().contract_identifier.clone();

                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                // Retrieve the token name
                let token_name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;

                let supply = caller
                    .data_mut()
                    .global_context
                    .database
                    .get_ft_supply(&contract_identifier, &token_name)?;

                let high = (supply >> 64) as u64;
                let low = (supply & 0xffff_ffff_ffff_ffff) as u64;
                Ok((low, high))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "ft_get_supply".to_string(),
                e,
            ))
        })
}

fn link_ft_get_balance_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "ft_get_balance",
            |mut caller: Caller<'_, ClarityWasmContext>,
             name_offset: i32,
             name_length: i32,
             owner_offset: i32,
             owner_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                // Retrieve the token name
                let name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;
                let token_name = ClarityName::try_from(name.clone())?;

                let contract_identifier =
                    caller.data().contract_context().contract_identifier.clone();
                let epoch = caller.data_mut().global_context.epoch_id;
                // Read the owner principal from the Wasm memory
                let value = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::PrincipalType,
                    owner_offset,
                    owner_length,
                    epoch,
                )?;
                let owner = value_as_principal(&value)?;

                let ft_info = caller
                    .data()
                    .contract_context()
                    .meta_ft
                    .get(&token_name)
                    .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(format!(
                        "NFT: {}",
                        token_name
                    ))))?
                    .clone();

                let balance = caller.data_mut().global_context.database.get_ft_balance(
                    &contract_identifier,
                    token_name.as_str(),
                    owner,
                    Some(&ft_info),
                )?;

                let high = (balance >> 64) as u64;
                let low = (balance & 0xffff_ffff_ffff_ffff) as u64;
                Ok((low, high))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "ft_get_balance".to_string(),
                e,
            ))
        })
}

fn link_ft_burn_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "ft_burn",
            |mut caller: Caller<'_, ClarityWasmContext>,
             name_offset: i32,
             name_length: i32,
             amount_lo: i64,
             amount_hi: i64,
             sender_offset: i32,
             sender_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let contract_identifier =
                    caller.data().contract_context().contract_identifier.clone();
                let epoch = caller.data_mut().global_context.epoch_id;

                // Retrieve the token name
                let name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;
                let token_name = ClarityName::try_from(name.clone())?;

                // Compute the amount
                let amount = ((amount_hi as u128) << 64) | ((amount_lo as u64) as u128);
                // Read the sender principal from the Wasm memory
                let value = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::PrincipalType,
                    sender_offset,
                    sender_length,
                    epoch,
                )?;
                let burner = value_as_principal(&value)?;

                if amount == 0 {
                    return Ok((
                        0i32,
                        0i32,
                        BurnTokenErrorCodes::NOT_ENOUGH_BALANCE_OR_NON_POSITIVE as i64,
                        0i64,
                    ));
                }

                let burner_bal = caller.data_mut().global_context.database.get_ft_balance(
                    &contract_identifier,
                    token_name.as_str(),
                    burner,
                    None,
                )?;

                if amount > burner_bal {
                    return Ok((
                        0i32,
                        0i32,
                        BurnTokenErrorCodes::NOT_ENOUGH_BALANCE_OR_NON_POSITIVE as i64,
                        0i64,
                    ));
                }

                caller
                    .data_mut()
                    .global_context
                    .database
                    .checked_decrease_token_supply(
                        &contract_identifier,
                        token_name.as_str(),
                        amount,
                    )?;

                let final_burner_bal = burner_bal - amount;

                caller.data_mut().global_context.database.set_ft_balance(
                    &contract_identifier,
                    token_name.as_str(),
                    burner,
                    final_burner_bal,
                )?;

                let asset_identifier = AssetIdentifier {
                    contract_identifier: contract_identifier.clone(),
                    asset_name: token_name.clone(),
                };
                caller.data_mut().register_ft_burn_event(
                    burner.clone(),
                    amount,
                    asset_identifier,
                )?;

                caller
                    .data_mut()
                    .global_context
                    .add_memory(TypeSignature::PrincipalType.size()? as u64)
                    .map_err(VmExecutionError::from)?;
                caller
                    .data_mut()
                    .global_context
                    .add_memory(TypeSignature::UIntType.size()? as u64)
                    .map_err(VmExecutionError::from)?;

                caller.data_mut().global_context.log_token_transfer(
                    burner,
                    &contract_identifier,
                    &token_name,
                    amount,
                )?;

                // (ok true)
                Ok((1i32, 1i32, 0i64, 0i64))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "ft_burn".to_string(),
                e,
            ))
        })
}

fn link_ft_mint_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "ft_mint",
            |mut caller: Caller<'_, ClarityWasmContext>,
             name_offset: i32,
             name_length: i32,
             amount_lo: i64,
             amount_hi: i64,
             sender_offset: i32,
             sender_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let contract_identifier =
                    caller.data().contract_context().contract_identifier.clone();
                let epoch = caller.data_mut().global_context.epoch_id;

                // Retrieve the token name
                let name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;
                let token_name = ClarityName::try_from(name.clone())?;

                // Compute the amount
                let amount = ((amount_hi as u128) << 64) | ((amount_lo as u64) as u128);
                // Read the sender principal from the Wasm memory
                let value = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::PrincipalType,
                    sender_offset,
                    sender_length,
                    epoch,
                )?;
                let to_principal = value_as_principal(&value)?;

                if amount == 0 {
                    return Ok((
                        0i32,
                        0i32,
                        MintTokenErrorCodes::NON_POSITIVE_AMOUNT as i64,
                        0i64,
                    ));
                }

                let ft_info = caller
                    .data()
                    .contract_context()
                    .meta_ft
                    .get(token_name.as_str())
                    .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(format!(
                        "FT: {}",
                        token_name
                    ))))?
                    .clone();

                caller
                    .data_mut()
                    .global_context
                    .database
                    .checked_increase_token_supply(
                        &contract_identifier,
                        token_name.as_str(),
                        amount,
                        &ft_info,
                    )?;

                let to_bal = caller.data_mut().global_context.database.get_ft_balance(
                    &contract_identifier,
                    token_name.as_str(),
                    to_principal,
                    Some(&ft_info),
                )?;

                let final_to_bal = to_bal.checked_add(amount).ok_or(VmExecutionError::Runtime(
                    RuntimeError::ArithmeticOverflow,
                    None,
                ))?;

                caller
                    .data_mut()
                    .global_context
                    .add_memory(TypeSignature::PrincipalType.size()? as u64)
                    .map_err(VmExecutionError::from)?;
                caller
                    .data_mut()
                    .global_context
                    .add_memory(TypeSignature::UIntType.size()? as u64)
                    .map_err(VmExecutionError::from)?;

                caller.data_mut().global_context.database.set_ft_balance(
                    &contract_identifier,
                    token_name.as_str(),
                    to_principal,
                    final_to_bal,
                )?;

                let asset_identifier = AssetIdentifier {
                    contract_identifier: contract_identifier.clone(),
                    asset_name: token_name.clone(),
                };
                caller.data_mut().register_ft_mint_event(
                    to_principal.clone(),
                    amount,
                    asset_identifier,
                )?;

                // (ok true)
                Ok((1i32, 1i32, 0i64, 0i64))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "ft_mint".to_string(),
                e,
            ))
        })
}

fn link_ft_transfer_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "ft_transfer",
            |mut caller: Caller<'_, ClarityWasmContext>,
             name_offset: i32,
             name_length: i32,
             amount_lo: i64,
             amount_hi: i64,
             sender_offset: i32,
             sender_length: i32,
             recipient_offset: i32,
             recipient_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let contract_identifier =
                    caller.data().contract_context().contract_identifier.clone();

                let epoch = caller.data_mut().global_context.epoch_id;

                // Retrieve the token name
                let name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;
                let token_name = ClarityName::try_from(name.clone())?;

                // Compute the amount
                let amount = ((amount_hi as u128) << 64) | ((amount_lo as u64) as u128);
                // Read the sender principal from the Wasm memory
                let value = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::PrincipalType,
                    sender_offset,
                    sender_length,
                    epoch,
                )?;
                let from_principal = value_as_principal(&value)?;

                // Read the recipient principal from the Wasm memory
                let value = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::PrincipalType,
                    recipient_offset,
                    recipient_length,
                    epoch,
                )?;
                let to_principal = value_as_principal(&value)?;

                if amount == 0 {
                    return Ok((
                        0i32,
                        0i32,
                        TransferTokenErrorCodes::NON_POSITIVE_AMOUNT as i64,
                        0i64,
                    ));
                }

                if from_principal == to_principal {
                    return Ok((
                        0i32,
                        0i32,
                        TransferTokenErrorCodes::SENDER_IS_RECIPIENT as i64,
                        0i64,
                    ));
                }

                let ft_info = caller
                    .data()
                    .contract_context()
                    .meta_ft
                    .get(&token_name)
                    .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(format!(
                        "FT: {}",
                        token_name
                    ))))?
                    .clone();

                let from_bal = caller.data_mut().global_context.database.get_ft_balance(
                    &contract_identifier,
                    token_name.as_str(),
                    from_principal,
                    Some(&ft_info),
                )?;

                if from_bal < amount {
                    return Ok((
                        0i32,
                        0i32,
                        TransferTokenErrorCodes::NOT_ENOUGH_BALANCE as i64,
                        0i64,
                    ));
                }

                let final_from_bal = from_bal - amount;

                let to_bal = caller.data_mut().global_context.database.get_ft_balance(
                    &contract_identifier,
                    token_name.as_str(),
                    to_principal,
                    Some(&ft_info),
                )?;

                let final_to_bal = to_bal
                    .checked_add(amount)
                    .ok_or(RuntimeError::ArithmeticOverflow)?;

                caller
                    .data_mut()
                    .global_context
                    .add_memory(TypeSignature::PrincipalType.size()? as u64)
                    .map_err(VmExecutionError::from)?;
                caller
                    .data_mut()
                    .global_context
                    .add_memory(TypeSignature::PrincipalType.size()? as u64)
                    .map_err(VmExecutionError::from)?;
                caller
                    .data_mut()
                    .global_context
                    .add_memory(TypeSignature::UIntType.size()? as u64)
                    .map_err(VmExecutionError::from)?;
                caller
                    .data_mut()
                    .global_context
                    .add_memory(TypeSignature::UIntType.size()? as u64)
                    .map_err(VmExecutionError::from)?;

                caller.data_mut().global_context.database.set_ft_balance(
                    &contract_identifier,
                    &token_name,
                    from_principal,
                    final_from_bal,
                )?;
                caller.data_mut().global_context.database.set_ft_balance(
                    &contract_identifier,
                    token_name.as_str(),
                    to_principal,
                    final_to_bal,
                )?;

                caller.data_mut().global_context.log_token_transfer(
                    from_principal,
                    &contract_identifier,
                    &token_name,
                    amount,
                )?;

                let asset_identifier = AssetIdentifier {
                    contract_identifier: contract_identifier.clone(),
                    asset_name: token_name.clone(),
                };
                caller.data_mut().register_ft_transfer_event(
                    from_principal.clone(),
                    to_principal.clone(),
                    amount,
                    asset_identifier,
                )?;

                // (ok true)
                Ok((1i32, 1i32, 0i64, 0i64))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "ft_transfer".to_string(),
                e,
            ))
        })
}

fn link_nft_get_owner_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "nft_get_owner",
            |mut caller: Caller<'_, ClarityWasmContext>,
             name_offset: i32,
             name_length: i32,
             asset_offset: i32,
             _asset_length: i32,
             return_offset: i32,
             _return_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let contract_identifier =
                    caller.data().contract_context().contract_identifier.clone();
                let epoch = caller.data_mut().global_context.epoch_id;

                // Retrieve the token name
                let name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;
                let asset_name = ClarityName::try_from(name.clone())?;

                let nft_metadata = caller
                    .data()
                    .contract_context()
                    .meta_nft
                    .get(&asset_name)
                    .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(format!(
                        "NFT: {}",
                        asset_name
                    ))))?
                    .clone();

                let expected_asset_type = &nft_metadata.key_type;

                // Read in the NFT identifier from the Wasm memory
                let asset = read_from_wasm_indirect(
                    memory,
                    &mut caller,
                    expected_asset_type,
                    asset_offset,
                    epoch,
                )?;

                let _asset_size = asset.serialized_size()? as u64;

                if !expected_asset_type.admits(&caller.data().global_context.epoch_id, &asset)? {
                    return Err(RuntimeCheckErrorKind::TypeValueError(
                        Box::new(expected_asset_type.clone()),
                        asset.to_error_string(),
                    )
                    .into());
                }

                match caller.data_mut().global_context.database.get_nft_owner(
                    &contract_identifier,
                    asset_name.as_str(),
                    &asset,
                    expected_asset_type,
                ) {
                    Ok(owner) => {
                        // Write the principal to the return buffer
                        let memory = caller
                            .data()
                            .memory
                            .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                        let (_, bytes_written) = write_to_wasm(
                            caller,
                            memory,
                            &TypeSignature::PrincipalType,
                            return_offset,
                            return_offset,
                            &Value::Principal(owner),
                            false,
                        )?;

                        Ok((1i32, return_offset, bytes_written))
                    }
                    Err(VmExecutionError::Runtime(RuntimeError::NoSuchToken, _)) => {
                        Ok((0i32, 0i32, 0i32))
                    }
                    Err(e) => Err(e)?,
                }
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "nft_get_owner".to_string(),
                e,
            ))
        })
}

fn link_nft_burn_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "nft_burn",
            |mut caller: Caller<'_, ClarityWasmContext>,
             name_offset: i32,
             name_length: i32,
             asset_offset: i32,
             _asset_length: i32,
             sender_offset: i32,
             sender_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let contract_identifier =
                    caller.data().contract_context().contract_identifier.clone();

                let epoch = caller.data_mut().global_context.epoch_id;

                // Retrieve the token name
                let name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;
                let asset_name = ClarityName::try_from(name.clone())?;

                let nft_metadata = caller
                    .data()
                    .contract_context()
                    .meta_nft
                    .get(&asset_name)
                    .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(format!(
                        "NFT: {}",
                        asset_name
                    ))))?
                    .clone();

                let expected_asset_type = &nft_metadata.key_type;

                // Read in the NFT identifier from the Wasm memory
                let asset = read_from_wasm_indirect(
                    memory,
                    &mut caller,
                    expected_asset_type,
                    asset_offset,
                    epoch,
                )?;
                // Read the sender principal from the Wasm memory
                let value = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::PrincipalType,
                    sender_offset,
                    sender_length,
                    epoch,
                )?;
                let sender_principal = value_as_principal(&value)?;

                let asset_size = asset.serialized_size()? as u64;

                if !expected_asset_type.admits(&caller.data().global_context.epoch_id, &asset)? {
                    return Err(RuntimeCheckErrorKind::TypeValueError(
                        Box::new(expected_asset_type.clone()),
                        asset.to_error_string(),
                    )
                    .into());
                }

                let owner = match caller.data_mut().global_context.database.get_nft_owner(
                    &contract_identifier,
                    asset_name.as_str(),
                    &asset,
                    expected_asset_type,
                ) {
                    Err(VmExecutionError::Runtime(RuntimeError::NoSuchToken, _)) => {
                        return Ok((0i32, 0i32, BurnAssetErrorCodes::DOES_NOT_EXIST as i64, 0i64));
                    }
                    Ok(owner) => Ok(owner),
                    Err(e) => Err(e),
                }?;

                if &owner != sender_principal {
                    return Ok((0i32, 0i32, BurnAssetErrorCodes::NOT_OWNED_BY as i64, 0i64));
                }

                caller
                    .data_mut()
                    .global_context
                    .add_memory(TypeSignature::PrincipalType.size()? as u64)
                    .map_err(VmExecutionError::from)?;
                caller
                    .data_mut()
                    .global_context
                    .add_memory(asset_size)
                    .map_err(VmExecutionError::from)?;

                caller.data_mut().global_context.database.burn_nft(
                    &contract_identifier,
                    asset_name.as_str(),
                    &asset,
                    expected_asset_type,
                    &epoch,
                )?;

                caller.data_mut().global_context.log_asset_transfer(
                    sender_principal,
                    &contract_identifier,
                    &asset_name,
                    asset.clone(),
                )?;

                let asset_identifier = AssetIdentifier {
                    contract_identifier,
                    asset_name: asset_name.clone(),
                };
                caller.data_mut().register_nft_burn_event(
                    sender_principal.clone(),
                    asset,
                    asset_identifier,
                )?;

                // (ok true)
                Ok((1i32, 132, 0i64, 0i64))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "nft_burn".to_string(),
                e,
            ))
        })
}

fn link_nft_mint_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "nft_mint",
            |mut caller: Caller<'_, ClarityWasmContext>,
             name_offset: i32,
             name_length: i32,
             asset_offset: i32,
             _asset_length: i32,
             recipient_offset: i32,
             recipient_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let contract_identifier =
                    caller.data().contract_context().contract_identifier.clone();

                let epoch = caller.data_mut().global_context.epoch_id;

                // Retrieve the token name
                let name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;
                let asset_name = ClarityName::try_from(name.clone())?;

                let nft_metadata = caller
                    .data()
                    .contract_context()
                    .meta_nft
                    .get(&asset_name)
                    .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(format!(
                        "NFT: {}",
                        asset_name
                    ))))?
                    .clone();

                let expected_asset_type = &nft_metadata.key_type;

                // Read in the NFT identifier from the Wasm memory
                let asset = read_from_wasm_indirect(
                    memory,
                    &mut caller,
                    expected_asset_type,
                    asset_offset,
                    epoch,
                )?;
                // Read the recipient principal from the Wasm memory
                let value = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::PrincipalType,
                    recipient_offset,
                    recipient_length,
                    epoch,
                )?;
                let to_principal = value_as_principal(&value)?;

                let asset_size = asset.serialized_size()? as u64;

                if !expected_asset_type.admits(&caller.data().global_context.epoch_id, &asset)? {
                    return Err(RuntimeCheckErrorKind::TypeValueError(
                        Box::new(expected_asset_type.clone()),
                        asset.to_error_string(),
                    )
                    .into());
                }

                match caller.data_mut().global_context.database.get_nft_owner(
                    &contract_identifier,
                    asset_name.as_str(),
                    &asset,
                    expected_asset_type,
                ) {
                    Err(VmExecutionError::Runtime(RuntimeError::NoSuchToken, _)) => Ok(()),
                    Ok(_owner) => {
                        return Ok((0i32, 0i32, MintAssetErrorCodes::ALREADY_EXIST as i64, 0i64));
                    }
                    Err(e) => Err(e),
                }?;

                caller
                    .data_mut()
                    .global_context
                    .add_memory(TypeSignature::PrincipalType.size()? as u64)
                    .map_err(VmExecutionError::from)?;
                caller
                    .data_mut()
                    .global_context
                    .add_memory(asset_size)
                    .map_err(VmExecutionError::from)?;

                caller.data_mut().global_context.database.set_nft_owner(
                    &contract_identifier,
                    asset_name.as_str(),
                    &asset,
                    to_principal,
                    expected_asset_type,
                    &epoch,
                )?;

                let asset_identifier = AssetIdentifier {
                    contract_identifier,
                    asset_name: asset_name.clone(),
                };
                caller.data_mut().register_nft_mint_event(
                    to_principal.clone(),
                    asset,
                    asset_identifier,
                )?;

                // (ok true)
                Ok((1i32, 132, 0i64, 0i64))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "nft_mint".to_string(),
                e,
            ))
        })
}

fn link_nft_transfer_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "nft_transfer",
            |mut caller: Caller<'_, ClarityWasmContext>,
             name_offset: i32,
             name_length: i32,
             asset_offset: i32,
             _asset_length: i32,
             sender_offset: i32,
             sender_length: i32,
             recipient_offset: i32,
             recipient_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let contract_identifier =
                    caller.data().contract_context().contract_identifier.clone();

                let epoch = caller.data_mut().global_context.epoch_id;

                // Retrieve the token name
                let name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;
                let asset_name = ClarityName::try_from(name.clone())?;

                let nft_metadata = caller
                    .data()
                    .contract_context()
                    .meta_nft
                    .get(&asset_name)
                    .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(format!(
                        "NFT: {}",
                        asset_name
                    ))))?
                    .clone();

                let expected_asset_type = &nft_metadata.key_type;

                // Read in the NFT identifier from the Wasm memory
                let asset = read_from_wasm_indirect(
                    memory,
                    &mut caller,
                    expected_asset_type,
                    asset_offset,
                    epoch,
                )?;

                // Read the sender principal from the Wasm memory
                let value = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::PrincipalType,
                    sender_offset,
                    sender_length,
                    epoch,
                )?;
                let from_principal = value_as_principal(&value)?;
                // Read the recipient principal from the Wasm memory
                let value = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::PrincipalType,
                    recipient_offset,
                    recipient_length,
                    epoch,
                )?;
                let to_principal = value_as_principal(&value)?;

                let asset_size = asset.serialized_size()? as u64;

                if !expected_asset_type.admits(&caller.data().global_context.epoch_id, &asset)? {
                    return Err(RuntimeCheckErrorKind::TypeValueError(
                        Box::new(expected_asset_type.clone()),
                        asset.to_error_string(),
                    )
                    .into());
                }

                if from_principal == to_principal {
                    return Ok((
                        0i32,
                        0i32,
                        TransferAssetErrorCodes::SENDER_IS_RECIPIENT as i64,
                        0i64,
                    ));
                }

                let current_owner = match caller.data_mut().global_context.database.get_nft_owner(
                    &contract_identifier,
                    asset_name.as_str(),
                    &asset,
                    expected_asset_type,
                ) {
                    Ok(owner) => Ok(owner),
                    Err(VmExecutionError::Runtime(RuntimeError::NoSuchToken, _)) => {
                        return Ok((
                            0i32,
                            0i32,
                            TransferAssetErrorCodes::DOES_NOT_EXIST as i64,
                            0i64,
                        ));
                    }
                    Err(e) => Err(e),
                }?;

                if current_owner != *from_principal {
                    return Ok((
                        0i32,
                        0i32,
                        TransferAssetErrorCodes::NOT_OWNED_BY as i64,
                        0i64,
                    ));
                }

                caller
                    .data_mut()
                    .global_context
                    .add_memory(TypeSignature::PrincipalType.size()? as u64)
                    .map_err(VmExecutionError::from)?;
                caller
                    .data_mut()
                    .global_context
                    .add_memory(asset_size)
                    .map_err(VmExecutionError::from)?;

                caller.data_mut().global_context.database.set_nft_owner(
                    &contract_identifier,
                    asset_name.as_str(),
                    &asset,
                    to_principal,
                    expected_asset_type,
                    &epoch,
                )?;

                caller.data_mut().global_context.log_asset_transfer(
                    from_principal,
                    &contract_identifier,
                    &asset_name,
                    asset.clone(),
                )?;

                let asset_identifier = AssetIdentifier {
                    contract_identifier,
                    asset_name,
                };
                caller.data_mut().register_nft_transfer_event(
                    from_principal.clone(),
                    to_principal.clone(),
                    asset,
                    asset_identifier,
                )?;

                // (ok true)
                Ok((1i32, 132, 0i64, 0i64))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "nft_transfer".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `map_get`, into the Wasm module.
/// This function is called for the `map-get?` expression.
fn link_map_get_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "map_get",
            |mut caller: Caller<'_, ClarityWasmContext>,
             name_offset: i32,
             name_length: i32,
             key_offset: i32,
             _key_length: i32,
             return_offset: i32,
             _return_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                // Retrieve the map name
                let map_name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;

                let contract = caller.data().contract_context().contract_identifier.clone();
                let epoch = caller.data_mut().global_context.epoch_id;

                // Retrieve the metadata for this map
                let data_types = caller
                    .data()
                    .contract_context()
                    .meta_data_map
                    .get(map_name.as_str())
                    .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(format!(
                        "Map: {map_name}"
                    ))))?
                    .clone();

                // Read in the key from the Wasm memory
                let key = read_from_wasm_indirect(
                    memory,
                    &mut caller,
                    &data_types.key_type,
                    key_offset,
                    epoch,
                )?;

                let result = crate::phases::time(crate::phases::Phase::HostMap, || {
                    caller
                        .data_mut()
                        .global_context
                        .database
                        .fetch_entry_with_size(&contract, &map_name, &key, &data_types, &epoch)
                });

                match result {
                    Err(error) => {
                        handle_vm_execution_errors(&mut caller, error)?;
                        Ok(0i32)
                    }

                    Ok(data) => {
                        let serialized_byte_len = i32::try_from(data.serialized_byte_len)
                            .map_err(|_| crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
                        let memory = caller
                            .data()
                            .memory
                            .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                        let ty = TypeSignature::OptionalType(Box::new(data_types.value_type));
                        write_to_wasm(
                            &mut caller,
                            memory,
                            &ty,
                            return_offset,
                            return_offset + get_type_size(&ty),
                            &data.value,
                            true,
                        )?;

                        Ok(serialized_byte_len)
                    }
                }
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "map_get".to_string(),
                e,
            ))
        })
}

/// Link the diagnostic `charge_probe` host function.
///
/// Inert unless a module was compiled with `NANO_TRACE_CHARGES`, which makes
/// every generated charge report its interned label index and scaling input
/// before it decrements the meters. A module compiled without the flag never
/// imports it.
fn link_charge_probe_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap("clarity", "charge_probe", |index: i32, n: i64| {
            eprintln!("probe {index} {n}");
        })
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "charge_probe".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `map_set`, into the Wasm module.
/// This function is called for the `map-set` expression.
fn link_map_set_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "map_set",
            |mut caller: Caller<'_, ClarityWasmContext>,
             name_offset: i32,
             name_length: i32,
             key_offset: i32,
             _key_length: i32,
             value_offset: i32,
             _value_length: i32| {
                if caller.data().global_context.is_read_only() {
                    return Err(crate::error::wasm_error(WasmError::Expect(
                        "Tried to set in a read only context".into(),
                    ))
                    .into());
                }

                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let epoch = caller.data_mut().global_context.epoch_id;

                // Retrieve the map name
                let map_name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;

                let contract = caller.data().contract_context().contract_identifier.clone();

                let data_types = caller
                    .data()
                    .contract_context()
                    .meta_data_map
                    .get(map_name.as_str())
                    .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(format!(
                        "Map name: {map_name}"
                    ))))?
                    .clone();

                // Read in the key from the Wasm memory
                let key = read_from_wasm_indirect(
                    memory,
                    &mut caller,
                    &data_types.key_type,
                    key_offset,
                    epoch,
                )?;

                // Read in the value from the Wasm memory
                let value = read_from_wasm_indirect(
                    memory,
                    &mut caller,
                    &data_types.value_type,
                    value_offset,
                    epoch,
                )?;

                // Store the value in the map in the global context
                let result = crate::phases::time(crate::phases::Phase::HostMap, || {
                    caller.data_mut().global_context.database.set_entry(
                        &contract,
                        map_name.as_str(),
                        key,
                        value,
                        &data_types,
                        &epoch,
                    )
                });

                match result {
                    Err(error) => {
                        handle_vm_execution_errors(&mut caller, error)?;
                        Ok((1i32, 0i32))
                    }

                    Ok(data) => {
                        let serialized_byte_len = i32::try_from(data.serialized_byte_len)
                            .map_err(|_| crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
                        caller
                            .data_mut()
                            .global_context
                            .add_memory(data.serialized_byte_len)
                            .map_err(VmExecutionError::from)?;

                        if let Value::Bool(value) = data.value {
                            Ok((value as i32, serialized_byte_len))
                        } else {
                            Err(
                                VmExecutionError::Internal(VmInternalError::InvariantViolation(
                                    "Unexpected case, a boolean is expected".to_owned(),
                                ))
                                .into(),
                            )
                        }
                    }
                }
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "map_set".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `map_insert`, into the Wasm module.
/// This function is called for the `map-insert` expression.
fn link_map_insert_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "map_insert",
            |mut caller: Caller<'_, ClarityWasmContext>,
             name_offset: i32,
             name_length: i32,
             key_offset: i32,
             _key_length: i32,
             value_offset: i32,
             _value_length: i32| {
                if caller.data().global_context.is_read_only() {
                    return Err(crate::error::wasm_error(WasmError::Expect(
                        "Tried to insert in read only cont".into(),
                    ))
                    .into());
                }

                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let epoch = caller.data_mut().global_context.epoch_id;

                // Retrieve the map name
                let map_name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;

                let contract = caller.data().contract_context().contract_identifier.clone();

                let data_types = caller
                    .data()
                    .contract_context()
                    .meta_data_map
                    .get(map_name.as_str())
                    .ok_or(crate::error::wasm_error(WasmError::Expect(format!(
                        "Map: {map_name}"
                    ))))?
                    .clone();

                // Read in the key from the Wasm memory
                let key = read_from_wasm_indirect(
                    memory,
                    &mut caller,
                    &data_types.key_type,
                    key_offset,
                    epoch,
                )?;

                // Read in the value from the Wasm memory
                let value = read_from_wasm_indirect(
                    memory,
                    &mut caller,
                    &data_types.value_type,
                    value_offset,
                    epoch,
                )?;

                // Insert the value into the map
                let result = crate::phases::time(crate::phases::Phase::HostMap, || {
                    caller.data_mut().global_context.database.insert_entry(
                        &contract,
                        map_name.as_str(),
                        key,
                        value,
                        &data_types,
                        &epoch,
                    )
                });

                match result {
                    Err(error) => {
                        handle_vm_execution_errors(&mut caller, error)?;
                        Ok((1i32, 0i32))
                    }
                    Ok(data) => {
                        let serialized_byte_len = i32::try_from(data.serialized_byte_len)
                            .map_err(|_| crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
                        caller
                            .data_mut()
                            .global_context
                            .add_memory(data.serialized_byte_len)
                            .map_err(VmExecutionError::from)?;

                        if let Value::Bool(value) = data.value {
                            Ok((value as i32, serialized_byte_len))
                        } else {
                            Err(
                                VmExecutionError::Internal(VmInternalError::InvariantViolation(
                                    "Unexpected case, a boolean is expected".to_owned(),
                                ))
                                .into(),
                            )
                        }
                    }
                }
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "map_insert".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `map_delete`, into the Wasm module.
/// This function is called for the `map-delete` expression.
fn link_map_delete_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "map_delete",
            |mut caller: Caller<'_, ClarityWasmContext>,
             name_offset: i32,
             name_length: i32,
             key_offset: i32,
             _key_length: i32| {
                if caller.data().global_context.is_read_only() {
                    return Err(crate::error::wasm_error(WasmError::Expect(
                        "Tried to delete in read only context".into(),
                    ))
                    .into());
                }

                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                // Retrieve the map name
                let map_name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;

                let contract = caller.data().contract_context().contract_identifier.clone();
                let epoch = caller.data_mut().global_context.epoch_id;

                let data_types = caller
                    .data()
                    .contract_context()
                    .meta_data_map
                    .get(map_name.as_str())
                    .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(format!(
                        "Map: {map_name}"
                    ))))?
                    .clone();

                // Read in the key from the Wasm memory
                let key = read_from_wasm_indirect(
                    memory,
                    &mut caller,
                    &data_types.key_type,
                    key_offset,
                    epoch,
                )?;

                // Delete the key from the map in the global context
                let result = crate::phases::time(crate::phases::Phase::HostMap, || {
                    caller.data_mut().global_context.database.delete_entry(
                        &contract,
                        map_name.as_str(),
                        &key,
                        &data_types,
                        &epoch,
                    )
                });

                match result {
                    Err(error) => {
                        handle_vm_execution_errors(&mut caller, error)?;
                        Ok(true as i32)
                    }
                    Ok(data) => {
                        caller
                            .data_mut()
                            .global_context
                            .add_memory(data.serialized_byte_len)
                            .map_err(VmExecutionError::from)?;

                        if let Value::Bool(value) = data.value {
                            Ok(value as i32)
                        } else {
                            Err(
                                VmExecutionError::Internal(VmInternalError::InvariantViolation(
                                    "Unexpected case, a boolean is expected".to_owned(),
                                ))
                                .into(),
                            )
                        }
                    }
                }
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "map_delete".to_string(),
                e,
            ))
        })
}

/// Set the linked error with the error returned
fn handle_vm_execution_errors(
    caller: &mut Caller<'_, ClarityWasmContext>,
    error: VmExecutionError,
) -> Result<(), VmExecutionError> {
    let error = ExternRef::new(&mut *caller, Mutex::new(Some(error)))
        .map_err(|error| crate::error::wasm_error(WasmError::Runtime(error)))?;
    let linked_error = caller
        .get_export("linked-error")
        .ok_or(crate::error::wasm_error(WasmError::GlobalNotFound(
            "runtime-error-linked".to_owned(),
        )))?
        .into_global()
        .ok_or(crate::error::wasm_error(WasmError::GlobalNotFound(
            "runtime-error-linked".to_owned(),
        )))?;
    match linked_error.set(caller.as_context_mut(), Val::ExternRef(Some(error))) {
        Err(error) => Err(crate::error::wasm_error(WasmError::UnableToWriteMemory(
            error,
        ))),
        Ok(_) => Ok(()),
    }
}

/// Write the `none` an out-of-range block-info height answers with.
fn write_block_info_none(
    caller: &mut Caller<'_, ClarityWasmContext>,
    memory: Memory,
    return_offset: i32,
) -> Result<(), VmExecutionError> {
    write_to_wasm(
        caller,
        memory,
        &TypeSignature::BoolType,
        return_offset,
        return_offset + get_type_size(&TypeSignature::BoolType),
        &Value::Bool(false),
        true,
    )?;
    Ok(())
}

fn check_height_valid(
    caller: &mut Caller<'_, ClarityWasmContext>,
    memory: Memory,
    height_lo: i64,
    height_hi: i64,
    return_offset: i32,
) -> Result<Option<u32>, VmExecutionError> {
    let height = ((height_hi as u128) << 64) | ((height_lo as u64) as u128);

    let height_value = match u32::try_from(height) {
        Ok(result) => result,
        _ => {
            write_block_info_none(caller, memory, return_offset)?;
            return Ok(None);
        }
    };

    let current_block_height = caller
        .data_mut()
        .global_context
        .database
        .get_current_block_height();
    if height_value >= current_block_height {
        write_block_info_none(caller, memory, return_offset)?;
        return Ok(None);
    }
    Ok(Some(height_value))
}

/// The Stacks block height a legacy `get-block-info?` is really asking about.
///
/// `get-block-info?` predates Nakamoto, so from epoch 3.0 on the heights a
/// Clarity 1 or Clarity 2 contract passes it are *tenure* heights — the same
/// switch `block-height` made, and for the same reason. `special_get_block_info`
/// translates before the range check and answers `none` for a tenure this fork
/// does not have, so both have to happen in that order here too. Classic
/// primary testnet is excluded there and so is excluded here.
///
/// The Clarity 3 families are not translated: `get-stacks-block-info?` and
/// `get-tenure-info?` both take a Stacks block height, which is why this is
/// separate from `check_height_valid` rather than folded into it.
fn check_block_info_height_valid(
    caller: &mut Caller<'_, ClarityWasmContext>,
    memory: Memory,
    height_lo: i64,
    height_hi: i64,
    return_offset: i32,
) -> Result<Option<u32>, VmExecutionError> {
    let height = ((height_hi as u128) << 64) | ((height_lo as u64) as u128);
    let Ok(height_value) = u32::try_from(height) else {
        write_block_info_none(caller, memory, return_offset)?;
        return Ok(None);
    };
    let data = caller.data();
    let as_tenure_height = *data.contract_context().get_clarity_version()
        < ClarityVersion::Clarity3
        && data.global_context.epoch_id >= StacksEpochId::Epoch30
        && data.global_context.chain_id != CHAIN_ID_TESTNET;
    if !as_tenure_height {
        return check_height_valid(caller, memory, height_lo, height_hi, return_offset);
    }
    let translated = caller
        .data_mut()
        .global_context
        .database
        .get_block_height_for_tenure_height(height_value)?;
    let Some(height_value) = translated else {
        write_block_info_none(caller, memory, return_offset)?;
        return Ok(None);
    };
    check_height_valid(caller, memory, i64::from(height_value), 0, return_offset)
}

/// Link host interface function, `get_block_info_time`, into the Wasm module.
/// This function is called for the `get-block-info? time` expression.
fn link_get_block_info_time_property_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_block_info_time_property",
            |mut caller: Caller<'_, ClarityWasmContext>,
             height_lo: i64,
             height_hi: i64,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                if let Some(height_value) = check_block_info_height_valid(
                    &mut caller,
                    memory,
                    height_lo,
                    height_hi,
                    return_offset,
                )? {
                    // The *burn* block time, not the Stacks block's own
                    // timestamp: `get-block-info? time` is the pre-Nakamoto
                    // word and kept its pre-Nakamoto meaning, so it is
                    // `get_burn_block_time` in `special_get_block_info` where
                    // `get-stacks-block-info? time` is `get_block_time`.
                    let block_time = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_burn_block_time(height_value, None)?;
                    let (result, result_ty) =
                        (Value::UInt(block_time as u128), TypeSignature::UIntType);
                    let ty = TypeSignature::OptionalType(Box::new(result_ty));
                    write_to_wasm(
                        &mut caller,
                        memory,
                        &ty,
                        return_offset,
                        return_offset + get_type_size(&ty),
                        &Value::some(result)?,
                        true,
                    )?;
                }
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_block_info_time_property".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `get_block_info_vrf_seed`, into the Wasm module.
/// This function is called for the `get-block-info? vrf-seed` expression.
fn link_get_block_info_vrf_seed_property_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_block_info_vrf_seed_property",
            |mut caller: Caller<'_, ClarityWasmContext>,
             height_lo: i64,
             height_hi: i64,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                if let Some(height_value) = check_block_info_height_valid(
                    &mut caller,
                    memory,
                    height_lo,
                    height_hi,
                    return_offset,
                )? {
                    let vrf_seed = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_block_vrf_seed(height_value)?;
                    let data = vrf_seed.as_bytes().to_vec();
                    let len = data.len() as u32;
                    let (result, result_ty) = (
                        Value::Sequence(SequenceData::Buffer(BuffData { data })),
                        TypeSignature::SequenceType(SequenceSubtype::BufferType(
                            BufferLength::try_from(len)?,
                        )),
                    );
                    let ty = TypeSignature::OptionalType(Box::new(result_ty));

                    write_to_wasm(
                        &mut caller,
                        memory,
                        &ty,
                        return_offset,
                        return_offset + get_type_size(&ty),
                        &Value::some(result)?,
                        true,
                    )?;
                }
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_block_info_vrf_seed_property".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `get_block_info_header_hash`, into the Wasm module.
/// This function is called for the `get-block-info? header-hash` expression.
fn link_get_block_info_header_hash_property_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_block_info_header_hash_property",
            |mut caller: Caller<'_, ClarityWasmContext>,
             height_lo: i64,
             height_hi: i64,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                if let Some(height_value) = check_block_info_height_valid(
                    &mut caller,
                    memory,
                    height_lo,
                    height_hi,
                    return_offset,
                )? {
                    let header_hash = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_block_header_hash(height_value)?;
                    let data = header_hash.as_bytes().to_vec();
                    let len = data.len() as u32;
                    let (result, result_ty) = (
                        Value::Sequence(SequenceData::Buffer(BuffData { data })),
                        TypeSignature::SequenceType(SequenceSubtype::BufferType(
                            BufferLength::try_from(len)?,
                        )),
                    );
                    let ty = TypeSignature::OptionalType(Box::new(result_ty));

                    write_to_wasm(
                        &mut caller,
                        memory,
                        &ty,
                        return_offset,
                        return_offset + get_type_size(&ty),
                        &Value::some(result)?,
                        true,
                    )?;
                }
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_block_info_header_hash_property".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `get_block_info_burnchain_header_hash`, into the Wasm module.
/// This function is called for the `get-block-info? burnchain-header-hash` expression.
fn link_get_block_info_burnchain_header_hash_property_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_block_info_burnchain_header_hash_property",
            |mut caller: Caller<'_, ClarityWasmContext>,
             height_lo: i64,
             height_hi: i64,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                if let Some(height_value) = check_block_info_height_valid(
                    &mut caller,
                    memory,
                    height_lo,
                    height_hi,
                    return_offset,
                )? {
                    let burnchain_header_hash = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_burnchain_block_header_hash(height_value)?;
                    let data = burnchain_header_hash.as_bytes().to_vec();
                    let len = data.len() as u32;
                    let (result, result_ty) = (
                        Value::Sequence(SequenceData::Buffer(BuffData { data })),
                        TypeSignature::SequenceType(SequenceSubtype::BufferType(
                            BufferLength::try_from(len)?,
                        )),
                    );
                    let ty = TypeSignature::OptionalType(Box::new(result_ty));

                    write_to_wasm(
                        &mut caller,
                        memory,
                        &ty,
                        return_offset,
                        return_offset + get_type_size(&ty),
                        &Value::some(result)?,
                        true,
                    )?;
                }
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_block_info_burnchain_header_hash_property".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `get_block_info_id_header_hash`, into the Wasm module.
/// This function is called for the `get-block-info? id-header-hash` expression.
fn link_get_block_info_identity_header_hash_property_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_block_info_identity_header_hash_property",
            |mut caller: Caller<'_, ClarityWasmContext>,
             height_lo: i64,
             height_hi: i64,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                if let Some(height_value) = check_block_info_height_valid(
                    &mut caller,
                    memory,
                    height_lo,
                    height_hi,
                    return_offset,
                )? {
                    let id_header_hash = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_index_block_header_hash(height_value)?;
                    let data = id_header_hash.as_bytes().to_vec();
                    let len = data.len() as u32;
                    let (result, result_ty) = (
                        Value::Sequence(SequenceData::Buffer(BuffData { data })),
                        TypeSignature::SequenceType(SequenceSubtype::BufferType(
                            BufferLength::try_from(len)?,
                        )),
                    );
                    let ty = TypeSignature::OptionalType(Box::new(result_ty));

                    write_to_wasm(
                        &mut caller,
                        memory,
                        &ty,
                        return_offset,
                        return_offset + get_type_size(&ty),
                        &Value::some(result)?,
                        true,
                    )?;
                }
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_block_info_identity_header_hash_property".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `get_block_info_miner_address`, into the Wasm module.
/// This function is called for the `get-block-info? miner-address` expression.
fn link_get_block_info_miner_address_property_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_block_info_miner_address_property",
            |mut caller: Caller<'_, ClarityWasmContext>,
             height_lo: i64,
             height_hi: i64,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                if let Some(height_value) = check_block_info_height_valid(
                    &mut caller,
                    memory,
                    height_lo,
                    height_hi,
                    return_offset,
                )? {
                    let miner_address = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_miner_address(height_value)?;
                    let (result, result_ty) =
                        (Value::from(miner_address), TypeSignature::PrincipalType);
                    let ty = TypeSignature::OptionalType(Box::new(result_ty));

                    write_to_wasm(
                        &mut caller,
                        memory,
                        &ty,
                        return_offset,
                        return_offset + get_type_size(&ty),
                        &Value::some(result)?,
                        true,
                    )?;
                }
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_block_info_miner_address_property".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `get_block_info_miner_spend_winner`, into the Wasm module.
/// This function is called for the `get-block-info? miner-spend-winner` expression.
fn link_get_block_info_miner_spend_winner_property_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_block_info_miner_spend_winner_property",
            |mut caller: Caller<'_, ClarityWasmContext>,
             height_lo: i64,
             height_hi: i64,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                if let Some(height_value) = check_block_info_height_valid(
                    &mut caller,
                    memory,
                    height_lo,
                    height_hi,
                    return_offset,
                )? {
                    let winner_spend = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_miner_spend_winner(height_value)?;
                    let (result, result_ty) = (Value::UInt(winner_spend), TypeSignature::UIntType);
                    let ty = TypeSignature::OptionalType(Box::new(result_ty));

                    write_to_wasm(
                        &mut caller,
                        memory,
                        &ty,
                        return_offset,
                        return_offset + get_type_size(&ty),
                        &Value::some(result)?,
                        true,
                    )?;
                }
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_block_info_miner_spend_winner_property".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `get_block_info_miner_spend_total`, into the Wasm module.
/// This function is called for the `get-block-info? miner-spend-total` expression.
fn link_get_block_info_miner_spend_total_property_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_block_info_miner_spend_total_property",
            |mut caller: Caller<'_, ClarityWasmContext>,
             height_lo: i64,
             height_hi: i64,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                if let Some(height_value) = check_block_info_height_valid(
                    &mut caller,
                    memory,
                    height_lo,
                    height_hi,
                    return_offset,
                )? {
                    let total_spend = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_miner_spend_total(height_value)?;
                    let (result, result_ty) = (Value::UInt(total_spend), TypeSignature::UIntType);
                    let ty = TypeSignature::OptionalType(Box::new(result_ty));

                    write_to_wasm(
                        &mut caller,
                        memory,
                        &ty,
                        return_offset,
                        return_offset + get_type_size(&ty),
                        &Value::some(result)?,
                        true,
                    )?;
                }
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_block_info_miner_spend_total_property".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `get_block_info_block_reward`, into the Wasm module.
/// This function is called for the `get-block-info? block-reward` expression.
fn link_get_block_info_block_reward_property_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_block_info_block_reward_property",
            |mut caller: Caller<'_, ClarityWasmContext>,
             height_lo: i64,
             height_hi: i64,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                if let Some(height_value) = check_block_info_height_valid(
                    &mut caller,
                    memory,
                    height_lo,
                    height_hi,
                    return_offset,
                )? {
                    let block_reward_opt = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_block_reward(height_value)?;
                    let (result, result_ty) = (
                        match block_reward_opt {
                            Some(x) => Value::UInt(x),
                            None => {
                                // Write a 0 to the return buffer for `none`
                                write_to_wasm(
                                    &mut caller,
                                    memory,
                                    &TypeSignature::BoolType,
                                    return_offset,
                                    return_offset + get_type_size(&TypeSignature::BoolType),
                                    &Value::Bool(false),
                                    true,
                                )?;
                                return Ok(());
                            }
                        },
                        TypeSignature::UIntType,
                    );
                    let ty = TypeSignature::OptionalType(Box::new(result_ty));

                    write_to_wasm(
                        &mut caller,
                        memory,
                        &ty,
                        return_offset,
                        return_offset + get_type_size(&ty),
                        &Value::some(result)?,
                        true,
                    )?;
                }
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_block_info_block_reward_property".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `get_burn_block_info_header_hash_property`, into the Wasm module.
/// This function is called for the `get-burn-block-info? header-hash` expression.
fn link_get_burn_block_info_header_hash_property_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_burn_block_info_header_hash_property",
            |mut caller: Caller<'_, ClarityWasmContext>,
             height_lo: i64,
             height_hi: i64,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;
                let height = ((height_hi as u128) << 64) | ((height_lo as u64) as u128);

                // Note: we assume that we will not have a height bigger than u32::MAX.
                let height_value = match u32::try_from(height) {
                    Ok(result) => result,
                    _ => {
                        // Write a 0 to the return buffer for `none`
                        write_to_wasm(
                            &mut caller,
                            memory,
                            &TypeSignature::BoolType,
                            return_offset,
                            return_offset + get_type_size(&TypeSignature::BoolType),
                            &Value::Bool(false),
                            true,
                        )?;
                        return Ok(());
                    }
                };
                let burnchain_header_hash_opt = caller
                    .data_mut()
                    .global_context
                    .database
                    .get_burnchain_block_header_hash_for_burnchain_height(height_value)?;
                let (result, result_ty) = (
                    match burnchain_header_hash_opt {
                        Some(burnchain_header_hash) => {
                            Value::some(Value::Sequence(SequenceData::Buffer(BuffData {
                                data: burnchain_header_hash.as_bytes().to_vec(),
                            })))?
                        }
                        None => Value::none(),
                    },
                    TypeSignature::OptionalType(Box::new(TypeSignature::BUFFER_32.clone())),
                );

                write_to_wasm(
                    &mut caller,
                    memory,
                    &result_ty,
                    return_offset,
                    return_offset + get_type_size(&result_ty),
                    &result,
                    true,
                )?;
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_burn_block_info_header_hash_property".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `get_burn_block_info_pox_addrs_property`, into the Wasm module.
/// This function is called for the `get-burn-block-info? pox-addrs` expression.
fn link_get_burn_block_info_pox_addrs_property_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_burn_block_info_pox_addrs_property",
            |mut caller: Caller<'_, ClarityWasmContext>,
             height_lo: i64,
             height_hi: i64,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let height = ((height_hi as u128) << 64) | ((height_lo as u64) as u128);

                // Note: we assume that we will not have a height bigger than u32::MAX.
                let height_value = match u32::try_from(height) {
                    Ok(result) => result,
                    _ => {
                        // Write a 0 to the return buffer for `none`
                        write_to_wasm(
                            &mut caller,
                            memory,
                            &TypeSignature::BoolType,
                            return_offset,
                            return_offset + get_type_size(&TypeSignature::BoolType),
                            &Value::Bool(false),
                            true,
                        )?;
                        return Ok(());
                    }
                };

                let pox_addrs_and_payout = caller
                    .data_mut()
                    .global_context
                    .database
                    .get_pox_payout_addrs_for_burnchain_height(height_value)?;
                let addr_ty: TypeSignature = TupleTypeSignature::try_from(vec![
                    (
                        ClarityName::from_literal("hashbytes"),
                        TypeSignature::BUFFER_32.clone(),
                    ),
                    (
                        ClarityName::from_literal("version"),
                        TypeSignature::BUFFER_1.clone(),
                    ),
                ])?
                .into();
                let addrs_ty = TypeSignature::list_of(addr_ty.clone(), 2)?;
                let tuple_ty = TupleTypeSignature::try_from(vec![
                    (ClarityName::from_literal("addrs"), addrs_ty),
                    (ClarityName::from_literal("payout"), TypeSignature::UIntType),
                ])?;
                let value = match pox_addrs_and_payout {
                    Some((addrs, payout)) => {
                        Value::some(Value::Tuple(TupleData::from_data(vec![
                            (
                                ClarityName::from_literal("addrs"),
                                Value::list_with_type(
                                    &caller.data_mut().global_context.epoch_id,
                                    addrs.into_iter().map(Value::Tuple).collect(),
                                    ListTypeData::new_list(addr_ty, 2)?,
                                )?,
                            ),
                            (ClarityName::from_literal("payout"), Value::UInt(payout)),
                        ])?))?
                    }
                    None => Value::none(),
                };
                let ty = TypeSignature::OptionalType(Box::new(tuple_ty.into()));

                write_to_wasm(
                    &mut caller,
                    memory,
                    &ty,
                    return_offset,
                    return_offset + get_type_size(&ty),
                    &value,
                    true,
                )?;
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_burn_block_info_pox_addrs_property".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `get_stacks_block_info_time`, into the Wasm module.
/// This function is called for the `get-stacks-block-info? id-header-hash` expression.
fn link_get_stacks_block_info_time_property_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_stacks_block_info_time_property",
            |mut caller: Caller<'_, ClarityWasmContext>,
             height_lo: i64,
             height_hi: i64,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;
                // Get the memory from the caller
                if let Some(height_value) =
                    check_height_valid(&mut caller, memory, height_lo, height_hi, return_offset)?
                {
                    let block_time = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_block_time(height_value)?;
                    let (result, result_ty) =
                        (Value::UInt(block_time as u128), TypeSignature::UIntType);
                    let ty = TypeSignature::OptionalType(Box::new(result_ty));
                    write_to_wasm(
                        &mut caller,
                        memory,
                        &ty,
                        return_offset,
                        return_offset + get_type_size(&ty),
                        &Value::some(result)?,
                        true,
                    )?;
                }
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_stacks_block_info_time_property".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `get_stacks_block_info_header_hash`, into the Wasm module.
/// This function is called for the `get-stacks-block-info? header-hash` expression.
fn link_get_stacks_block_info_header_hash_property_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_stacks_block_info_header_hash_property",
            |mut caller: Caller<'_, ClarityWasmContext>,
             height_lo: i64,
             height_hi: i64,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                // Get the memory from the caller
                if let Some(height_value) =
                    check_height_valid(&mut caller, memory, height_lo, height_hi, return_offset)?
                {
                    let header_hash = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_block_header_hash(height_value)?;
                    let data = header_hash.as_bytes().to_vec();
                    let len = data.len() as u32;
                    let (result, result_ty) = (
                        Value::Sequence(SequenceData::Buffer(BuffData { data })),
                        TypeSignature::SequenceType(SequenceSubtype::BufferType(
                            BufferLength::try_from(len)?,
                        )),
                    );
                    let ty = TypeSignature::OptionalType(Box::new(result_ty));
                    write_to_wasm(
                        &mut caller,
                        memory,
                        &ty,
                        return_offset,
                        return_offset + get_type_size(&ty),
                        &Value::some(result)?,
                        true,
                    )?;
                }
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_stacks_block_info_header_hash_property".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `get_stacks_block_info_identity_header_hash_`, into the Wasm module.
/// This function is called for the `get-stacks-block-info? time` expression.
fn link_get_stacks_block_info_identity_header_hash_property_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_stacks_block_info_identity_header_hash_property",
            |mut caller: Caller<'_, ClarityWasmContext>,
             height_lo: i64,
             height_hi: i64,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                if let Some(height_value) =
                    check_height_valid(&mut caller, memory, height_lo, height_hi, return_offset)?
                {
                    let id_header_hash = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_index_block_header_hash(height_value)?;
                    let data = id_header_hash.as_bytes().to_vec();
                    let len = data.len() as u32;
                    let (result, result_ty) = (
                        Value::Sequence(SequenceData::Buffer(BuffData { data })),
                        TypeSignature::SequenceType(SequenceSubtype::BufferType(
                            BufferLength::try_from(len)?,
                        )),
                    );
                    let ty = TypeSignature::OptionalType(Box::new(result_ty));

                    write_to_wasm(
                        &mut caller,
                        memory,
                        &ty,
                        return_offset,
                        return_offset + get_type_size(&ty),
                        &Value::some(result)?,
                        true,
                    )?;
                }
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_stacks_block_info_identity_header_hash_property".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `get_tenure_info_burnchain_header_hash`, into the Wasm module.
/// This function is called for the `get-tenure-info? burnchain-header-hash` expression.
fn link_get_tenure_info_burnchain_header_hash_property_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_tenure_info_burnchain_header_hash_property",
            |mut caller: Caller<'_, ClarityWasmContext>,
             height_lo: i64,
             height_hi: i64,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                if let Some(height_value) =
                    check_height_valid(&mut caller, memory, height_lo, height_hi, return_offset)?
                {
                    let burnchain_header_hash = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_burnchain_block_header_hash(height_value)?;
                    let data = burnchain_header_hash.as_bytes().to_vec();
                    let len = data.len() as u32;
                    let (result, result_ty) = (
                        Value::Sequence(SequenceData::Buffer(BuffData { data })),
                        TypeSignature::SequenceType(SequenceSubtype::BufferType(
                            BufferLength::try_from(len)?,
                        )),
                    );
                    let ty = TypeSignature::OptionalType(Box::new(result_ty));

                    write_to_wasm(
                        &mut caller,
                        memory,
                        &ty,
                        return_offset,
                        return_offset + get_type_size(&ty),
                        &Value::some(result)?,
                        true,
                    )?;
                }
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_tenure_info_burnchain_header_hash_property".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `get_tenure_info_miner_address`, into the Wasm module.
/// This function is called for the `get-tenure-info? miner-address` expression.
fn link_get_tenure_info_miner_address_property_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_tenure_info_miner_address_property",
            |mut caller: Caller<'_, ClarityWasmContext>,
             height_lo: i64,
             height_hi: i64,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                if let Some(height_value) =
                    check_height_valid(&mut caller, memory, height_lo, height_hi, return_offset)?
                {
                    let miner_address = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_miner_address(height_value)?;
                    let (result, result_ty) =
                        (Value::from(miner_address), TypeSignature::PrincipalType);
                    let ty = TypeSignature::OptionalType(Box::new(result_ty));

                    write_to_wasm(
                        &mut caller,
                        memory,
                        &ty,
                        return_offset,
                        return_offset + get_type_size(&ty),
                        &Value::some(result)?,
                        true,
                    )?;
                }
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_tenure_info_miner_address_property".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `get_tenure_info_time`, into the Wasm module.
/// This function is called for the `get-tenure-info? time` expression.
fn link_get_tenure_info_time_property_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_tenure_info_time_property",
            |mut caller: Caller<'_, ClarityWasmContext>,
             height_lo: i64,
             height_hi: i64,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                if let Some(height_value) =
                    check_height_valid(&mut caller, memory, height_lo, height_hi, return_offset)?
                {
                    let block_time = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_burn_block_time(height_value, None)?;
                    let (result, result_ty) =
                        (Value::UInt(block_time as u128), TypeSignature::UIntType);
                    let ty = TypeSignature::OptionalType(Box::new(result_ty));
                    write_to_wasm(
                        &mut caller,
                        memory,
                        &ty,
                        return_offset,
                        return_offset + get_type_size(&ty),
                        &Value::some(result)?,
                        true,
                    )?;
                }
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_tenure_info_time_property".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `get_tenure_info_vrf_seed_property`, into the Wasm module.
/// This function is called for the `get-tenure-info? vrf-seed` expression.
fn link_get_tenure_info_vrf_seed_property_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_tenure_info_vrf_seed_property",
            |mut caller: Caller<'_, ClarityWasmContext>,
             height_lo: i64,
             height_hi: i64,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                if let Some(height_value) =
                    check_height_valid(&mut caller, memory, height_lo, height_hi, return_offset)?
                {
                    let vrf_seed = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_block_vrf_seed(height_value)?;
                    let data = vrf_seed.as_bytes().to_vec();
                    let len = data.len() as u32;
                    let (result, result_ty) = (
                        Value::Sequence(SequenceData::Buffer(BuffData { data })),
                        TypeSignature::SequenceType(SequenceSubtype::BufferType(
                            BufferLength::try_from(len)?,
                        )),
                    );
                    let ty = TypeSignature::OptionalType(Box::new(result_ty));

                    write_to_wasm(
                        &mut caller,
                        memory,
                        &ty,
                        return_offset,
                        return_offset + get_type_size(&ty),
                        &Value::some(result)?,
                        true,
                    )?;
                }
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_tenure_info_vrf_seed_property".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `get_tenure_info_block_reward`, into the Wasm module.
/// This function is called for the `get-tenure-info? block-reward` expression.
fn link_get_tenure_info_block_reward_property_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_tenure_info_block_reward_property",
            |mut caller: Caller<'_, ClarityWasmContext>,
             height_lo: i64,
             height_hi: i64,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                if let Some(height_value) =
                    check_height_valid(&mut caller, memory, height_lo, height_hi, return_offset)?
                {
                    let block_reward_opt = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_block_reward(height_value)?;
                    let (result, result_ty) = (
                        match block_reward_opt {
                            Some(x) => Value::UInt(x),
                            None => {
                                // Write a 0 to the return buffer for `none`
                                write_to_wasm(
                                    &mut caller,
                                    memory,
                                    &TypeSignature::BoolType,
                                    return_offset,
                                    return_offset + get_type_size(&TypeSignature::BoolType),
                                    &Value::Bool(false),
                                    true,
                                )?;
                                return Ok(());
                            }
                        },
                        TypeSignature::UIntType,
                    );
                    let ty = TypeSignature::OptionalType(Box::new(result_ty));

                    write_to_wasm(
                        &mut caller,
                        memory,
                        &ty,
                        return_offset,
                        return_offset + get_type_size(&ty),
                        &Value::some(result)?,
                        true,
                    )?;
                }
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_tenure_info_block_reward_property".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `get_tenure_info_miner_spend_total`, into the Wasm module.
/// This function is called for the `get-tenure-info? miner-spend-total` expression.
fn link_get_tenure_info_miner_spend_total_property_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_tenure_info_miner_spend_total_property",
            |mut caller: Caller<'_, ClarityWasmContext>,
             height_lo: i64,
             height_hi: i64,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                if let Some(height_value) =
                    check_height_valid(&mut caller, memory, height_lo, height_hi, return_offset)?
                {
                    let total_spend = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_miner_spend_total(height_value)?;
                    let (result, result_ty) = (Value::UInt(total_spend), TypeSignature::UIntType);
                    let ty = TypeSignature::OptionalType(Box::new(result_ty));

                    write_to_wasm(
                        &mut caller,
                        memory,
                        &ty,
                        return_offset,
                        return_offset + get_type_size(&ty),
                        &Value::some(result)?,
                        true,
                    )?;
                }
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_tenure_info_miner_spend_total_property".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `get_tenure_info_miner_spend_winner`, into the Wasm module.
/// This function is called for the `get-tenure-info? miner-spend-winner` expression.
fn link_get_tenure_info_miner_spend_winner_property_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_tenure_info_miner_spend_winner_property",
            |mut caller: Caller<'_, ClarityWasmContext>,
             height_lo: i64,
             height_hi: i64,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                if let Some(height_value) =
                    check_height_valid(&mut caller, memory, height_lo, height_hi, return_offset)?
                {
                    let winner_spend = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_miner_spend_winner(height_value)?;
                    let (result, result_ty) = (Value::UInt(winner_spend), TypeSignature::UIntType);
                    let ty = TypeSignature::OptionalType(Box::new(result_ty));

                    write_to_wasm(
                        &mut caller,
                        memory,
                        &ty,
                        return_offset,
                        return_offset + get_type_size(&ty),
                        &Value::some(result)?,
                        true,
                    )?;
                }
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_tenure_info_miner_spend_winner_property".to_string(),
                e,
            ))
        })
}

fn sanitize_contract_call_result(
    epoch: &StacksEpochId,
    returns_constraint: Option<&TypeSignature>,
    result: Value,
) -> Result<Value, VmExecutionError> {
    let result_type = TypeSignature::type_of(&result)?;
    let (result, _) = Value::sanitize_value(epoch, &result_type, result)
        .ok_or(RuntimeCheckErrorKind::CouldNotDetermineType)?;
    if let Some(expected) = returns_constraint {
        let actual = TypeSignature::type_of(&result)?;
        if !expected.admits_type(epoch, &actual)? {
            return Err(RuntimeCheckErrorKind::ReturnTypesMustMatch(
                Box::new(expected.clone()),
                Box::new(actual),
            )
            .into());
        }
    }
    Ok(result)
}

/// Link host interface function, `contract_call`, into the Wasm module.
/// This function is called for `contract-call?`s.
#[cfg(any())]
fn link_contract_call_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "contract_call",
            |mut caller: Caller<'_, ClarityWasmContext>,
             trait_id_offset: i32,
             trait_id_length: i32,
             contract_offset: i32,
             contract_length: i32,
             function_offset: i32,
             function_length: i32,
             args_offset: i32,
             args_length: i32,
             return_offset: i32,
             _return_length: i32| {
                (|| -> Result<(), VmExecutionError> {
                    // the second part of the contract_call cost (i.e., the load contract cost)
                    //   is checked in `execute_contract`, and the function _application_ cost
                    //   is checked in callables::DefinedFunction::execute_apply.

                    // Get the memory from the caller
                    let memory = caller
                        .data()
                        .memory
                        .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                    let epoch = caller.data_mut().global_context.epoch_id;

                    // Read the contract identifier from the Wasm memory
                    let contract_val = read_from_wasm(
                        memory,
                        &mut caller,
                        &TypeSignature::PrincipalType,
                        contract_offset,
                        contract_length,
                        epoch,
                    )?;
                    let contract_id = match &contract_val {
                        Value::Principal(PrincipalData::Contract(contract_id)) => contract_id,
                        _ => {
                            return Err(
                                crate::error::wasm_error(WasmError::ValueTypeMismatch).into()
                            );
                        }
                    };

                    // Read the function name from the Wasm memory
                    let function_name = read_identifier_from_wasm(
                        memory,
                        &mut caller,
                        function_offset,
                        function_length,
                    )?;

                    // Retrieve the contract context for the contract we're calling
                    let mut contract = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_contract(contract_id)?;

                    // Retrieve the function we're calling
                    let function = contract.functions.get(function_name.as_str()).ok_or(
                        crate::error::wasm_error(WasmError::Expect(format!(
                        "Contract {contract_id} does not contain public function {function_name}"
                    ))),
                    )?;
                    let mut args = Vec::new();
                    let mut args_sizes = Vec::new();
                    let mut arg_offset = args_offset;
                    // Read the arguments from the Wasm memory
                    for arg_ty in function.get_arg_types() {
                        let arg = read_from_wasm_indirect(
                            memory,
                            &mut caller,
                            arg_ty,
                            arg_offset,
                            epoch,
                        )?;
                        args_sizes.push(arg.size()? as u64);
                        args.push(arg);

                        arg_offset += get_type_size(arg_ty);
                    }

                    let caller_contract: PrincipalData = caller
                        .data()
                        .contract_context()
                        .contract_identifier
                        .clone()
                        .into();
                    caller.data_mut().push_caller(caller_contract.clone());

                    let mut call_stack = caller.data().call_stack.clone();
                    let sender = caller.data().sender.clone();
                    let sponsor = caller.data().sponsor.clone();

                    let short_circuit_cost = caller
                        .data_mut()
                        .global_context
                        .cost_track
                        .short_circuit_contract_call(
                            contract_id,
                            &ClarityName::try_from(function_name.clone())?,
                            &args_sizes,
                        )?;

                    // We get the current cost values from the caller's globals.
                    let mut cost_globals = caller
                        .data()
                        .cost_globals
                        .ok_or(WasmError::GlobalNotFound("cost globals not found".into()))?;
                    // We set the cost meter in the global context. global context which is shared by all environments.
                    caller.data_mut().global_context.cost_meter =
                        cost_globals.remaining_costs(&mut caller.as_context_mut())?;

                    let mut exec_state = ExecutionState {
                        global_context: caller.data_mut().global_context,
                        call_stack: &mut call_stack,
                    };

                    let invoke_ctx = InvocationContext {
                        contract_context: &contract,
                        sender,
                        caller: Some(caller_contract),
                        sponsor,
                    };
                    let result = if short_circuit_cost {
                        exec_state.run_free(&invoke_ctx, |exec_state, free_invoke_ctx| {
                            exec_state.execute_contract_from_wasm(
                                free_invoke_ctx,
                                contract_id,
                                &function_name,
                                &args,
                            )
                        })
                    } else {
                        exec_state.execute_contract_from_wasm(
                            &invoke_ctx,
                            contract_id,
                            &function_name,
                            &args,
                        )
                    }?;

                    // The cost meter in we get back from the global context is updated in stacks-core/clarity/src/vm/clarity_wasm.rs::call_function().
                    // We then simply retrieve it to update the current WASM's cost global with the updated costs.
                    let updated_cost_meter = caller.data_mut().global_context.cost_meter;
                    cost_globals
                        .set_remaining_costs(&mut caller.as_context_mut(), &updated_cost_meter)?;

                    // Write the result to the return buffer
                    let return_ty = if trait_id_length == 0 {
                        // This is a direct call
                        function
                            .get_return_type()
                            .as_ref()
                            .ok_or(crate::error::wasm_error(WasmError::Expect(
                                "Function should be typed".into(),
                            )))?
                    } else {
                        // This is a dynamic call
                        let trait_id = read_bytes_from_wasm(
                            memory,
                            &mut caller,
                            trait_id_offset,
                            trait_id_length,
                        )
                        .and_then(|bs| trait_identifier_from_bytes(&bs))?;
                        contract = if &trait_id.contract_identifier == contract_id {
                            contract
                        } else {
                            caller
                                .data_mut()
                                .global_context
                                .database
                                .get_contract(&trait_id.contract_identifier)?
                        };
                        contract
                            .defined_traits
                            .get(trait_id.name.as_str())
                            .and_then(|trait_functions| trait_functions.get(function_name.as_str()))
                            .map(|f_ty| &f_ty.returns)
                            .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(format!(
                                "Trait: {}",
                                trait_id.name
                            ))))?
                    };

                    write_to_wasm(
                        &mut caller,
                        memory,
                        return_ty,
                        return_offset,
                        return_offset + get_type_size(return_ty),
                        &result,
                        true,
                    )?;

                    Ok(())
                })()
                .map_err(|error| wasmtime::Error::msg(error.to_string()))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "contract_call".to_string(),
                e,
            ))
        })
}

fn link_contract_call_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "contract_call",
            |mut caller: Caller<'_, ClarityWasmContext>,
             trait_id_offset: i32,
             trait_id_length: i32,
             contract_offset: i32,
             contract_length: i32,
             function_offset: i32,
             function_length: i32,
             args_offset: i32,
             args_length: i32,
             return_offset: i32,
             _return_length: i32| {
                let result = (|| -> Result<(), VmExecutionError> {
                    let memory = caller
                        .data()
                        .memory
                        .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;
                    let epoch = caller.data().global_context.epoch_id;
                    let contract_value = read_from_wasm(
                        memory,
                        &mut caller,
                        &TypeSignature::PrincipalType,
                        contract_offset,
                        contract_length,
                        epoch,
                    )?;
                    let contract_id = match contract_value {
                        Value::Principal(PrincipalData::Contract(contract_id)) => contract_id,
                        _ => return Err(crate::error::wasm_error(WasmError::ValueTypeMismatch)),
                    };
                    let function_name = read_identifier_from_wasm(
                        memory,
                        &mut caller,
                        function_offset,
                        function_length,
                    )?;
                    let contract = caller
                        .data_mut()
                        .global_context
                        .database
                        .get_contract(&contract_id)?;
                    let function = contract.functions.get(function_name.as_str()).ok_or(
                        crate::error::wasm_error(WasmError::Expect(format!(
                        "Contract {contract_id} does not contain public function {function_name}"
                    ))),
                    )?;
                    let argument_types = function.get_arg_types().to_vec();
                    let mut arguments = Vec::with_capacity(argument_types.len());
                    let mut argument_sizes = Vec::with_capacity(argument_types.len());
                    let mut argument_offset = args_offset;
                    let argument_sizes_offset = args_offset
                        .checked_add(args_length)
                        .ok_or(crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
                    for (index, argument_type) in argument_types.iter().enumerate() {
                        arguments.push(read_from_wasm_indirect(
                            memory,
                            &mut caller,
                            argument_type,
                            argument_offset,
                            epoch,
                        )?);
                        argument_offset += get_type_size(argument_type);
                        let size_offset = i32::try_from(index)
                            .ok()
                            .and_then(|index| index.checked_mul(4))
                            .and_then(|index| argument_sizes_offset.checked_add(index))
                            .ok_or(crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
                        let size = read_i32(memory, &mut caller, size_offset)?;
                        argument_sizes.push(
                            u64::try_from(size).map_err(|_| {
                                crate::error::wasm_error(WasmError::ValueTypeMismatch)
                            })?,
                        );
                    }

                    let module = caller
                        .data()
                        .module_cache
                        .get(&contract_id)
                        .cloned()
                        .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(format!(
                            "compiled contract {contract_id}"
                        ))))?;
                    let return_type = if trait_id_length == 0 {
                        match module
                            .analysis
                            .get_public_function_type(function_name.as_str())
                            .or_else(|| {
                                module
                                    .analysis
                                    .get_read_only_function_type(function_name.as_str())
                            }) {
                            Some(FunctionType::Fixed(function)) => function.returns.clone(),
                            _ => {
                                return Err(crate::error::wasm_error(WasmError::NotInDatabase(
                                    function_name.to_string(),
                                )));
                            }
                        }
                    } else {
                        let trait_id = read_bytes_from_wasm(
                            memory,
                            &mut caller,
                            trait_id_offset,
                            trait_id_length,
                        )
                        .and_then(|bytes| trait_identifier_from_bytes(&bytes))?;
                        let trait_contract = if trait_id.contract_identifier == contract_id {
                            contract.clone()
                        } else {
                            caller
                                .data_mut()
                                .global_context
                                .database
                                .get_contract(&trait_id.contract_identifier)?
                        };
                        trait_contract
                            .defined_traits
                            .get(trait_id.name.as_str())
                            .and_then(|functions| functions.get(function_name.as_str()))
                            .map(|function| function.returns.clone())
                            .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(format!(
                                "trait {}",
                                trait_id.name
                            ))))?
                    };

                    let caller_contract: PrincipalData = caller
                        .data()
                        .contract_context()
                        .contract_identifier
                        .clone()
                        .into();
                    let sender = caller.data().sender.clone();
                    let sponsor = caller.data().sponsor.clone();
                    let module_cache = caller.data().module_cache;
                    let result = {
                        let context = caller.data_mut();
                        call_function_with_argument_sizes(
                            function_name.as_str(),
                            &arguments,
                            Some(&argument_sizes),
                            &module,
                            context.global_context,
                            &contract,
                            context.call_stack,
                            sender,
                            Some(caller_contract),
                            sponsor,
                            module_cache,
                        )?
                    };
                    let result = sanitize_contract_call_result(
                        &epoch,
                        (trait_id_length != 0).then_some(&return_type),
                        result,
                    )?;
                    write_to_wasm(
                        &mut caller,
                        memory,
                        &return_type,
                        return_offset,
                        return_offset + get_type_size(&return_type),
                        &result,
                        true,
                    )?;
                    Ok(())
                })();

                match result {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        handle_vm_execution_errors(&mut caller, error)
                            .map_err(wasmtime::Error::new)?;
                        let runtime_error_code = caller
                            .get_export("runtime-error-code")
                            .ok_or(crate::error::wasm_error(WasmError::GlobalNotFound(
                                "runtime-error-code".to_owned(),
                            )))?
                            .into_global()
                            .ok_or(crate::error::wasm_error(WasmError::GlobalNotFound(
                                "runtime-error-code".to_owned(),
                            )))?;
                        runtime_error_code
                            .set(
                                caller.as_context_mut(),
                                Val::I32(ErrorMap::ExternError as i32),
                            )
                            .map_err(|error| {
                                wasmtime::Error::new(crate::error::wasm_error(
                                    WasmError::UnableToWriteMemory(error),
                                ))
                            })?;
                        Err(wasmtime::Error::new(Trap::UnreachableCodeReached))
                    }
                }
            },
        )
        .map(|_| ())
        .map_err(|error| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "contract_call".into(),
                error,
            ))
        })
}

/// Link host interface function, `check_constant_call_target`, into the Wasm
/// module. The compiler emits a call to it before every `contract-call?` whose
/// dispatch target it resolved through a `define-constant`.
///
/// `special_contract_call` (`clarity/src/vm/functions/database.rs`) treats a
/// constant as a static target only when the contract's Clarity version
/// `supports_callables()`, the *executing* epoch `supports_call_with_constant()`,
/// and the contract is not deploying; otherwise the atom is neither a callable
/// constant nor a callable variable and the call ends as
/// `ContractCallExpectName`. None of the three can be settled where the module is
/// built: a contract keeps the version and analysis it was published with while
/// the chain moves under it, and the same compiled function body runs once during
/// the deploy and any number of times after it.
fn link_check_constant_call_target_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "check_constant_call_target",
            |caller: Caller<'_, ClarityWasmContext>| {
                let epoch = caller.data().global_context.epoch_id;
                let contract_context = caller.data().contract_context();
                if !contract_context.get_clarity_version().supports_callables()
                    || !epoch.supports_call_with_constant()
                    || contract_context.is_deploying
                {
                    return Err(RuntimeCheckErrorKind::ContractCallExpectName.into());
                }
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "check_constant_call_target".to_string(),
                e,
            ))
        })
}

fn link_contract_hash_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "contract_hash",
            |mut caller: Caller<'_, ClarityWasmContext>,
             contract_offset: i32,
             contract_length: i32,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let epoch = caller.data_mut().global_context.epoch_id;
                // Read the contract identifier from the Wasm memory
                let contract_val = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::PrincipalType,
                    contract_offset,
                    contract_length,
                    epoch,
                )?;

                // (response (buff 32) uint)
                let return_ty = TypeSignature::ResponseType(Box::new((
                    TypeSignature::SequenceType(SequenceSubtype::BufferType(
                        BufferLength::try_from(32u32)?,
                    )),
                    TypeSignature::UIntType,
                )));

                let contract_id = match &contract_val {
                    Value::Principal(PrincipalData::Contract(contract_id)) => contract_id,
                    _ => {
                        let err_val = Value::Response(ResponseData {
                            committed: false,
                            data: Box::new(Value::UInt(1)), // err u1
                        });

                        write_to_wasm(
                            &mut caller,
                            memory,
                            &return_ty,
                            return_offset,
                            return_offset + get_type_size(&return_ty),
                            &err_val,
                            true,
                        )?;
                        return Ok(());
                    }
                };

                let contract_hash = caller
                    .data_mut()
                    .global_context
                    .database
                    .get_contract_hash(contract_id)?;

                let resp_val = match contract_hash {
                    Some(contract_hash) => {
                        // success: (ok <buff-32>)
                        let ok_val = Value::Sequence(SequenceData::Buffer(BuffData {
                            data: contract_hash.0.to_vec(),
                        }));
                        Value::Response(ResponseData {
                            committed: true,
                            data: Box::new(ok_val),
                        })
                    }
                    None => {
                        // contract missing => (err u2)
                        Value::Response(ResponseData {
                            committed: false,
                            data: Box::new(Value::UInt(2)), // err u2
                        })
                    }
                };

                write_to_wasm(
                    &mut caller,
                    memory,
                    &return_ty, // (response (buff 32) uint)
                    return_offset,
                    return_offset + get_type_size(&return_ty),
                    &resp_val,
                    true,
                )?;

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "contract_hash".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `begin_public_call`, into the Wasm module.
/// This function is called before a local call to a public function.
fn link_begin_public_call_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "begin_public_call",
            |mut caller: Caller<'_, ClarityWasmContext>| {
                caller.data_mut().global_context.begin();
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "begin_public_call".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `begin_read_only_call`, into the Wasm module.
/// This function is called before a local call to a public function.
fn link_begin_read_only_call_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "begin_read_only_call",
            |mut caller: Caller<'_, ClarityWasmContext>| {
                caller.data_mut().global_context.begin_read_only();
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "begin_read_only_call".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `commit_call`, into the Wasm module.
/// This function is called after a local call to a public function to commit
/// it's changes into the global context.
fn link_commit_call_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "commit_call",
            |mut caller: Caller<'_, ClarityWasmContext>| {
                caller.data_mut().global_context.commit()?;
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "commit_call".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `roll_back_call`, into the Wasm module.
/// This function is called after a local call to roll back it's changes from
/// the global context. It is called when a public function errors, or a
/// read-only call completes.
fn link_roll_back_call_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "roll_back_call",
            |mut caller: Caller<'_, ClarityWasmContext>| {
                caller.data_mut().global_context.roll_back()?;
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "roll_back_call".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `print`, into the Wasm module.
/// This function is called for all contract print statements (`print`).
fn link_print_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "print",
            |mut caller: Caller<'_, ClarityWasmContext>,
             value_offset: i32,
             _value_length: i32,
             serialized_ty_offset: i32,
             serialized_ty_length: i32| {
                let (memory, value_ty) =
                    runtime_value_type(&mut caller, serialized_ty_offset, serialized_ty_length)?;
                let epoch = caller.data().global_context.epoch_id;
                let clarity_val =
                    read_from_wasm_indirect(memory, &mut caller, &value_ty, value_offset, epoch)?;

                crate::phases::time(crate::phases::Phase::HostEvent, || {
                    caller.data_mut().register_print_event(clarity_val)
                })?;

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction("print".to_string(), e))
        })
}

/// Link host interface function, `enter_at_block`, into the Wasm module.
/// This function is called before evaluating the inner expression of an
/// `at-block` expression.
fn link_enter_at_block_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "enter_at_block",
            |mut caller: Caller<'_, ClarityWasmContext>,
             block_hash_offset: i32,
             block_hash_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;
                let epoch = caller.data_mut().global_context.epoch_id;
                // The *executing* epoch, not the one the module was built under,
                // and this is why the check cannot be a compile-time one: a
                // contract published before 3.4 was analysed with `at-block`
                // available and keeps that analysis forever, while the chain it
                // runs on has taken the word away. stacks-core checks it twice for
                // exactly that reason -- at analysis against the deploy epoch, and
                // here against the current one (`special_at_block`,
                // `clarity/src/vm/functions/database.rs`) -- and 881 contracts in
                // the mainnet checkpoint have an `(at-block` call site.
                //
                // Before the argument count and before the cost, as it is there:
                // the refusal charges no `AtBlock` cost, and the cost is in the
                // receipt.
                if !epoch.supports_at_block() {
                    return Err(RuntimeCheckErrorKind::AtBlockUnavailable.into());
                }

                let block_hash = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::BUFFER_32,
                    block_hash_offset,
                    block_hash_length,
                    epoch,
                )?;

                let bhh = match block_hash {
                    Value::Sequence(SequenceData::Buffer(BuffData { data })) => {
                        if data.len() != 32 {
                            return Err(RuntimeError::BadBlockHash(data).into());
                        }
                        StacksBlockId::from(data.as_slice())
                    }
                    x => {
                        return Err(RuntimeCheckErrorKind::TypeValueError(
                            Box::new(TypeSignature::BUFFER_32.clone()),
                            x.to_error_string(),
                        )
                        .into());
                    }
                };

                caller
                    .data_mut()
                    .global_context
                    .add_memory(cost_constants::AT_BLOCK_MEMORY)
                    .map_err(VmExecutionError::from)?;

                caller.data_mut().global_context.begin_read_only();

                let prev_bhh = caller
                    .data_mut()
                    .global_context
                    .database
                    .set_block_hash(bhh, false)?;

                caller.data_mut().push_at_block(prev_bhh);

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "enter_at_block".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `exit_at_block`, into the Wasm module.
/// This function is called after evaluating the inner expression of an
/// `at-block` expression, resetting the state back to the current block.
fn link_exit_at_block_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "exit_at_block",
            |mut caller: Caller<'_, ClarityWasmContext>| {
                // Pop back to the current block
                let bhh = caller.data_mut().pop_at_block()?;
                caller
                    .data_mut()
                    .global_context
                    .database
                    .set_block_hash(bhh, true)?;

                // Roll back any changes that occurred during the `at-block`
                // expression. This is precautionary, since only read-only
                // operations are allowed during an `at-block` expression.
                caller.data_mut().global_context.roll_back()?;

                caller
                    .data_mut()
                    .global_context
                    .drop_memory(cost_constants::AT_BLOCK_MEMORY)
                    .map_err(|error| {
                        wasmtime::Error::msg(format!("failed to release memory: {error:?}"))
                    })?;

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "exit_at_block".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `keccak256`, into the Wasm module.
/// This function is called for the Clarity expression, `keccak256`.
fn link_keccak256_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "keccak256",
            |mut caller: Caller<'_, ClarityWasmContext>,
             buffer_offset: i32,
             buffer_length: i32,
             return_offset: i32,
             return_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                // Read the bytes from the memory
                let bytes =
                    read_bytes_from_wasm(memory, &mut caller, buffer_offset, buffer_length)?;

                let hash = Keccak256Hash::from_data(&bytes);

                // Write the hash to the return buffer
                memory.write(&mut caller, return_offset as usize, hash.as_bytes())?;

                Ok((return_offset, return_length))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "keccak256".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `sha512`, into the Wasm module.
/// This function is called for the Clarity expression, `sha512`.
fn link_sha512_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "sha512",
            |mut caller: Caller<'_, ClarityWasmContext>,
             buffer_offset: i32,
             buffer_length: i32,
             return_offset: i32,
             return_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                // Read the bytes from the memory
                let bytes =
                    read_bytes_from_wasm(memory, &mut caller, buffer_offset, buffer_length)?;

                let hash = Sha512Sum::from_data(&bytes);

                // Write the hash to the return buffer
                memory.write(&mut caller, return_offset as usize, hash.as_bytes())?;

                Ok((return_offset, return_length))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction("sha512".to_string(), e))
        })
}

/// Link host interface function, `sha512_256`, into the Wasm module.
/// This function is called for the Clarity expression, `sha512/256`.
fn link_sha512_256_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "sha512_256",
            |mut caller: Caller<'_, ClarityWasmContext>,
             buffer_offset: i32,
             buffer_length: i32,
             return_offset: i32,
             return_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                // Read the bytes from the memory
                let bytes =
                    read_bytes_from_wasm(memory, &mut caller, buffer_offset, buffer_length)?;

                let hash = Sha512Trunc256Sum::from_data(&bytes);

                // Write the hash to the return buffer
                memory.write(&mut caller, return_offset as usize, hash.as_bytes())?;

                Ok((return_offset, return_length))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "sha512_256".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `secp256k1_recover`, into the Wasm module.
/// This function is called for the Clarity expression, `secp256k1-recover?`.
fn link_secp256k1_recover_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "secp256k1_recover",
            |mut caller: Caller<'_, ClarityWasmContext>,
             msg_offset: i32,
             msg_length: i32,
             sig_offset: i32,
             sig_length: i32,
             return_offset: i32,
             _return_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let ret_ty = TypeSignature::new_response(
                    TypeSignature::BUFFER_33.clone(),
                    TypeSignature::UIntType,
                )?;
                let repr_size = get_type_size(&ret_ty);

                // Read the message bytes from the memory
                let msg_bytes = read_bytes_from_wasm(memory, &mut caller, msg_offset, msg_length)?;
                // To match the interpreter behavior, if the message is the
                // wrong length, throw a runtime type error.
                if msg_bytes.len() != 32 {
                    return Err(RuntimeCheckErrorKind::TypeValueError(
                        Box::new(TypeSignature::BUFFER_32.clone()),
                        Value::buff_from(msg_bytes)?.to_error_string(),
                    )
                    .into());
                }

                // Read the signature bytes from the memory
                let sig_bytes = read_bytes_from_wasm(memory, &mut caller, sig_offset, sig_length)?;
                // To match the interpreter behavior, if the signature is the
                // wrong length, return a Clarity error.
                if sig_bytes.len() != 65 || sig_bytes[64] > 3 {
                    let result = Value::err_uint(2);
                    write_to_wasm(
                        caller,
                        memory,
                        &ret_ty,
                        return_offset,
                        return_offset + repr_size,
                        &result,
                        true,
                    )?;
                    return Ok(());
                }

                let result = match secp256k1_recover(&msg_bytes, &sig_bytes) {
                    Ok(pubkey) => Value::okay(Value::buff_from(pubkey.to_vec())?)?,
                    _ => Value::err_uint(1),
                };

                // Write the result to the return buffer
                write_to_wasm(
                    caller,
                    memory,
                    &ret_ty,
                    return_offset,
                    return_offset + repr_size,
                    &result,
                    true,
                )?;

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "secp256k1_recover".to_string(),
                e,
            ))
        })
}

/// Link host interface function, `secp256k1_verify`, into the Wasm module.
/// This function is called for the Clarity expression, `secp256k1-verify`.
fn link_secp256k1_verify_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "secp256k1_verify",
            |mut caller: Caller<'_, ClarityWasmContext>,
             msg_offset: i32,
             msg_length: i32,
             sig_offset: i32,
             sig_length: i32,
             pk_offset: i32,
             pk_length: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                // Read the message bytes from the memory
                let msg_bytes = read_bytes_from_wasm(memory, &mut caller, msg_offset, msg_length)?;
                // To match the interpreter behavior, if the message is the
                // wrong length, throw a runtime type error.
                if msg_bytes.len() != 32 {
                    return Err(RuntimeCheckErrorKind::TypeValueError(
                        Box::new(TypeSignature::BUFFER_32.clone()),
                        Value::buff_from(msg_bytes)?.to_error_string(),
                    )
                    .into());
                }

                // Read the signature bytes from the memory
                let sig_bytes = read_bytes_from_wasm(memory, &mut caller, sig_offset, sig_length)?;
                // To match the interpreter behavior, if the signature is the
                // wrong length, return a Clarity error.
                if sig_bytes.len() < 64
                    || sig_bytes.len() > 65
                    || sig_bytes.len() == 65 && sig_bytes[64] > 3
                {
                    return Ok(0i32);
                }

                // Read the public-key bytes from the memory
                let pk_bytes = read_bytes_from_wasm(memory, &mut caller, pk_offset, pk_length)?;
                // To match the interpreter behavior, if the public key is the
                // wrong length, throw a runtime type error.
                if pk_bytes.len() != 33 {
                    return Err(RuntimeCheckErrorKind::TypeValueError(
                        Box::new(TypeSignature::BUFFER_33.clone()),
                        Value::buff_from(pk_bytes)?.to_error_string(),
                    )
                    .into());
                }

                Ok(secp256k1_verify(&msg_bytes, &sig_bytes, &pk_bytes).map_or(0i32, |_| 1i32))
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "secp256k1_verify".to_string(),
                e,
            ))
        })
}

fn link_secp256r1_verify_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "secp256r1_verify",
            |mut caller: Caller<'_, ClarityWasmContext>,
             message_offset: i32,
             message_length: i32,
             signature_offset: i32,
             signature_length: i32,
             public_key_offset: i32,
             public_key_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;
                let message =
                    read_bytes_from_wasm(memory, &mut caller, message_offset, message_length)?;
                let signature =
                    read_bytes_from_wasm(memory, &mut caller, signature_offset, signature_length)?;
                let public_key = read_bytes_from_wasm(
                    memory,
                    &mut caller,
                    public_key_offset,
                    public_key_length,
                )?;
                if message.len() != 32 {
                    return Err(RuntimeCheckErrorKind::TypeValueError(
                        Box::new(TypeSignature::BUFFER_32),
                        Value::buff_from(message)?.to_error_string(),
                    )
                    .into());
                }
                if signature.len() != 64 {
                    return Ok(0i32);
                }
                if public_key.len() != 33 {
                    return Err(RuntimeCheckErrorKind::TypeValueError(
                        Box::new(TypeSignature::BUFFER_33),
                        Value::buff_from(public_key)?.to_error_string(),
                    )
                    .into());
                }
                let version = *caller.data().contract_context().get_clarity_version();
                let valid = if version.uses_secp256r1_double_hashing() {
                    secp256r1_verify(&message, &signature, &public_key).is_ok()
                } else {
                    secp256r1_verify_digest(&message, &signature, &public_key).is_ok()
                };
                Ok(if valid { 1i32 } else { 0i32 })
            },
        )
        .map(|_| ())
        .map_err(|error| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "secp256r1_verify".into(),
                error,
            ))
        })
}

fn link_ed25519_verify_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "ed25519_verify",
            |mut caller: Caller<'_, ClarityWasmContext>,
             message_offset: i32,
             message_length: i32,
             signature_offset: i32,
             signature_length: i32,
             public_key_offset: i32,
             public_key_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;
                let message =
                    read_bytes_from_wasm(memory, &mut caller, message_offset, message_length)?;
                let signature =
                    read_bytes_from_wasm(memory, &mut caller, signature_offset, signature_length)?;
                let public_key = read_bytes_from_wasm(
                    memory,
                    &mut caller,
                    public_key_offset,
                    public_key_length,
                )?;
                if signature.len() != 64 {
                    return Err(RuntimeCheckErrorKind::TypeValueError(
                        Box::new(TypeSignature::BUFFER_64),
                        Value::buff_from(signature)?.to_error_string(),
                    )
                    .into());
                }
                if public_key.len() != 32 {
                    return Err(RuntimeCheckErrorKind::TypeValueError(
                        Box::new(TypeSignature::BUFFER_32),
                        Value::buff_from(public_key)?.to_error_string(),
                    )
                    .into());
                }
                let signature: [u8; 64] = signature.try_into().map_err(|_| {
                    crate::error::wasm_error(WasmError::Expect("signature length changed".into()))
                })?;
                let public_key: [u8; 32] = public_key.try_into().map_err(|_| {
                    crate::error::wasm_error(WasmError::Expect("public key length changed".into()))
                })?;
                Ok(ed25519_verify(&message, &signature, &public_key).is_ok() as i32)
            },
        )
        .map(|_| ())
        .map_err(|error| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "ed25519_verify".into(),
                error,
            ))
        })
}

fn link_secp256k1_decompress_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "secp256k1_decompress",
            |mut caller: Caller<'_, ClarityWasmContext>,
             public_key_offset: i32,
             public_key_length: i32,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;
                let public_key = read_bytes_from_wasm(
                    memory,
                    &mut caller,
                    public_key_offset,
                    public_key_length,
                )?;
                if public_key.len() != 33 {
                    return Err(RuntimeCheckErrorKind::TypeValueError(
                        Box::new(TypeSignature::BUFFER_33),
                        Value::buff_from(public_key)?.to_error_string(),
                    )
                    .into());
                }
                let return_type = TypeSignature::new_response(
                    TypeSignature::SequenceType(SequenceSubtype::BufferType(
                        BufferLength::try_from(65usize)?,
                    )),
                    TypeSignature::UIntType,
                )?;
                let result = match secp256k1_decompress(&public_key) {
                    Ok(key) => Value::okay(Value::buff_from(key.to_vec())?)?,
                    Err(_) => Value::err_uint(1),
                };
                write_to_wasm(
                    caller,
                    memory,
                    &return_type,
                    return_offset,
                    return_offset + get_type_size(&return_type),
                    &result,
                    true,
                )?;
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|error| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "secp256k1_decompress".into(),
                error,
            ))
        })
}

fn link_verify_merkle_proof_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "verify_merkle_proof",
            |mut caller: Caller<'_, ClarityWasmContext>,
             leaf_offset: i32,
             leaf_length: i32,
             root_offset: i32,
             root_length: i32,
             index_low: i64,
             index_high: i64,
             count_low: i64,
             count_high: i64,
             siblings_shape: i32,
             siblings_offset: i32,
             siblings_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;
                let epoch = caller.data().global_context.epoch_id;
                let leaf = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::BUFFER_32,
                    leaf_offset,
                    leaf_length,
                    epoch,
                )?;
                let root = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::BUFFER_32,
                    root_offset,
                    root_length,
                    epoch,
                )?;
                let sibling_type = TypeSignature::list_of(TypeSignature::BUFFER_32, 24)?;
                let siblings = if siblings_shape == 0 {
                    read_from_wasm(
                        memory,
                        &mut caller,
                        &sibling_type,
                        siblings_offset,
                        siblings_length,
                        epoch,
                    )?
                } else {
                    caller.data().load_runtime_shape(siblings_shape)?
                };
                let index = ((index_high as u128) << 64) | index_low as u64 as u128;
                let count = ((count_high as u128) << 64) | count_low as u64 as u128;
                Ok(crate::bitcoin::verify_merkle_proof(leaf, root, index, count, siblings)? as i32)
            },
        )
        .map(|_| ())
        .map_err(|error| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "verify_merkle_proof".into(),
                error,
            ))
        })
}

fn link_get_bitcoin_tx_output_fn(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "get_bitcoin_tx_output",
            |mut caller: Caller<'_, ClarityWasmContext>,
             tx_offset: i32,
             tx_length: i32,
             vout_low: i64,
             vout_high: i64,
             return_offset: i32,
             _return_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;
                let epoch = caller.data().global_context.epoch_id;
                let tx = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::BUFFER_MAX,
                    tx_offset,
                    tx_length,
                    epoch,
                )?;
                let vout = ((vout_high as u128) << 64) | vout_low as u64 as u128;
                let result = crate::bitcoin::get_bitcoin_tx_output(tx, vout)?;
                let tuple_type = TupleTypeSignature::try_from(vec![
                    (
                        ClarityName::from_literal("script"),
                        TypeSignature::SequenceType(SequenceSubtype::BufferType(
                            BufferLength::try_from(1024usize)?,
                        )),
                    ),
                    (ClarityName::from_literal("amount"), TypeSignature::UIntType),
                    (ClarityName::from_literal("txid"), TypeSignature::BUFFER_32),
                ])?;
                let return_type = TypeSignature::new_response(
                    TypeSignature::TupleType(tuple_type),
                    TypeSignature::UIntType,
                )?;
                write_to_wasm(
                    caller,
                    memory,
                    &return_type,
                    return_offset,
                    return_offset + get_type_size(&return_type),
                    &result,
                    true,
                )?;
                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|error| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "get_bitcoin_tx_output".into(),
                error,
            ))
        })
}

/// Link host interface function, `principal_of`, into the Wasm module.
/// This function is called for the Clarity expression, `principal-of?`.
fn link_principal_of_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "principal_of",
            |mut caller: Caller<'_, ClarityWasmContext>,
             key_offset: i32,
             key_length: i32,
             principal_offset: i32| {
                // Get the memory from the caller
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let epoch = caller.data_mut().global_context.epoch_id;
                // Read the public key from the memory
                let key_val = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::BUFFER_33.clone(),
                    key_offset,
                    key_length,
                    epoch,
                )?;

                let pub_key = match key_val {
                    Value::Sequence(SequenceData::Buffer(BuffData { ref data })) => {
                        if data.len() != 33 {
                            return Err(RuntimeCheckErrorKind::TypeValueError(
                                Box::new(TypeSignature::BUFFER_33.clone()),
                                key_val.to_error_string(),
                            )
                            .into());
                        }
                        data
                    }
                    _ => {
                        return Err(RuntimeCheckErrorKind::TypeValueError(
                            Box::new(TypeSignature::BUFFER_33.clone()),
                            key_val.to_error_string(),
                        )
                        .into());
                    }
                };

                if let Ok(pub_key) = Secp256k1PublicKey::from_slice(pub_key) {
                    let clarity_version = *caller.data().contract_context().get_clarity_version();
                    // Note: Clarity1 had a bug in how the address is computed (issues/2619).
                    // We want to preserve the old behavior unless the version is greater.
                    let addr = if clarity_version > ClarityVersion::Clarity1 {
                        pubkey_to_address_v2(pub_key, caller.data().global_context.mainnet)?
                    } else {
                        pubkey_to_address_v1(pub_key)?
                    };
                    let principal = addr.to_account_principal();

                    // Write the principal to the return buffer
                    write_to_wasm(
                        &mut caller,
                        memory,
                        &TypeSignature::PrincipalType,
                        principal_offset,
                        principal_offset,
                        &Value::Principal(principal),
                        false,
                    )?;

                    // (ok principal)
                    Ok((
                        1i32,
                        principal_offset,
                        STANDARD_PRINCIPAL_BYTES as i32,
                        0i64,
                        0i64,
                    ))
                } else {
                    // (err u1)
                    Ok((0i32, 0i32, 0i32, 1i64, 0i64))
                }
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "secp256k1_verify".to_string(),
                e,
            ))
        })
}

fn link_save_constant_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "save_constant",
            |mut caller: Caller<'_, ClarityWasmContext>,
             name_offset: i32,
             name_length: i32,
             value_offset: i32,
             _value_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let epoch = caller.data_mut().global_context.epoch_id;

                // Get constant name from the memory.
                let const_name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;
                let cname = ClarityName::try_from(const_name.clone())?;

                // Get constant value type.
                let value_ty = caller
                    .data()
                    .contract_analysis
                    .ok_or(crate::error::wasm_error(WasmError::DefinesNotFound))?
                    .get_variable_type(const_name.as_str())
                    .ok_or(crate::error::wasm_error(WasmError::DefinesNotFound))?;

                let value =
                    read_from_wasm_indirect(memory, &mut caller, value_ty, value_offset, epoch)?;

                runtime_cost(
                    ClarityCostFunction::BindName,
                    caller.data_mut().global_context,
                    0,
                )
                .map_err(VmExecutionError::from)?;

                // Insert constant name and expression value into a persistent data structure.
                // The value counts towards the contract's data size, which is
                // what a later `contract-call?` pays `LoadContract` for.
                let value_size = value.size()?;
                let context = caller.data_mut().contract_context_mut()?;
                context.variables.insert(cname, value);
                context.data_size = context.data_size.saturating_add(u64::from(value_size));

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "save_constant".to_string(),
                e,
            ))
        })
}

fn link_load_constant_fn(linker: &mut Linker<ClarityWasmContext>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "load_constant",
            |mut caller: Caller<'_, ClarityWasmContext>,
             name_offset: i32,
             name_length: i32,
             value_offset: i32,
             _value_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                // Read constant name from the memory.
                let const_name =
                    read_identifier_from_wasm(memory, &mut caller, name_offset, name_length)?;

                // Constant value
                let value = caller
                    .data()
                    .contract_context()
                    .variables
                    .get(&ClarityName::try_from(const_name.clone())?)
                    .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(format!(
                        "Constant: {const_name}"
                    ))))?
                    .clone();

                // Constant value type
                let ty = TypeSignature::type_of(&value)?;

                write_to_wasm(
                    &mut caller,
                    memory,
                    &ty,
                    value_offset,
                    value_offset + get_type_size(&ty),
                    &value,
                    true,
                )?;

                Ok(())
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "load_constant".to_string(),
                e,
            ))
        })
}

fn link_principal_to_string_ascii(
    linker: &mut Linker<ClarityWasmContext>,
) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "principal_to_string_ascii",
            |mut caller: Caller<'_, ClarityWasmContext>,
             principal_offset: i32,
             principal_length: i32,
             result_offset: i32,
             result_length: i32| {
                let memory = caller
                    .data()
                    .memory
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                let epoch = caller.data().global_context.epoch_id;
                let principal = read_from_wasm(
                    memory,
                    &mut caller,
                    &TypeSignature::PrincipalType,
                    principal_offset,
                    principal_length,
                    epoch,
                )?;

                let result_beg = result_offset as usize;
                let result_end = result_beg + result_length as usize;
                let mut result_buffer = Cursor::new(
                    memory
                        .data_mut(&mut caller)
                        .get_mut(result_beg..result_end)
                        .ok_or(crate::error::wasm_error(WasmError::UnableToWriteMemory(
                            wasmtime::Error::msg("Non-existing addresses in memory"),
                        )))?,
                );

                write!(result_buffer, "{principal}").map_err(|e| {
                    crate::error::wasm_error(WasmError::UnableToWriteMemory(e.into()))
                })?;

                Ok(result_buffer.position() as i32)
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "principal_to_string_ascii".to_string(),
                e,
            ))
        })
}

fn link_skip_list<T: 'static>(linker: &mut Linker<T>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap(
            "clarity",
            "skip_list",
            |mut caller: Caller<'_, T>, offset_beg: i32, offset_end: i32| {
                // Generic over `T`, so the cached handle on the Clarity context
                // is out of reach; this test-only word keeps the name lookup.
                let memory = caller
                    .get_export("memory")
                    .and_then(|export| export.into_memory())
                    .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;

                // we will read the remaining serialized buffer here, and start it with the list type prefix
                let mut serialized_buffer = vec![0u8; (offset_end - offset_beg) as usize + 1];
                serialized_buffer[0] = clarity::vm::types::serialization::TypePrefix::List as u8;
                memory
                    .read(
                        &mut caller,
                        offset_beg as usize,
                        &mut serialized_buffer[1..],
                    )
                    .map_err(|e| crate::error::wasm_error(WasmError::Runtime(e.into())))?;

                match Value::deserialize_read_count(&mut serialized_buffer.as_slice(), None, false)
                {
                    Ok((_, bytes_read)) => Ok(offset_beg + bytes_read as i32 - 1),
                    Err(_) => Ok(0),
                }
            },
        )
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "skip_list".to_string(),
                e,
            ))
        })
}

/// Link host-interface function, `log`, into the Wasm module.
/// This function is used for debugging the Wasm, and should not be called in
/// production.
fn link_log<T: 'static>(linker: &mut Linker<T>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap("", "log", |_: Caller<'_, T>, param: i64| {
            println!("log: {param}");
        })
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction("log".to_string(), e))
        })
}

/// Link host-interface function, `debug_msg`, into the Wasm module.
/// This function is used for debugging the Wasm, and should not be called in
/// production.
fn link_debug_msg<T: 'static>(linker: &mut Linker<T>) -> Result<(), VmExecutionError> {
    linker
        .func_wrap("", "debug_msg", |_caller: Caller<'_, T>, param: i32| {
            crate::debug_msg::recall(param, |s| println!("DEBUG: {s}"))
        })
        .map(|_| ())
        .map_err(|e| {
            crate::error::wasm_error(WasmError::UnableToLinkHostFunction(
                "debug_msg".to_string(),
                e,
            ))
        })
}

pub fn dummy_linker<T: 'static>(engine: &Engine) -> Result<Linker<T>, wasmtime::Error> {
    let mut linker = Linker::new(engine);

    linker.func_wrap(
        "clarity",
        "save_runtime_shape",
        |_value_offset: i32, _type_offset: i32, _type_length: i32| Ok(0i32),
    )?;

    linker.func_wrap(
        "clarity",
        "save_filtered_runtime_shape",
        |_value_offset: i32,
         _type_offset: i32,
         _type_length: i32,
         _input_handle: i32,
         _input_count: i32| Ok(0i32),
    )?;

    linker.func_wrap(
        "clarity",
        "runtime_shape_serialization_size",
        |_handle: i32| Ok(0i32),
    )?;

    linker.func_wrap("clarity", "runtime_shape_size", |_handle: i32| Ok(0i32))?;

    linker.func_wrap(
        "clarity",
        "runtime_value_size",
        |_value_offset: i32, _type_offset: i32, _type_length: i32| Ok(0i32),
    )?;

    linker.func_wrap(
        "clarity",
        "runtime_sequence_element_size",
        |_value_offset: i32, _type_offset: i32, _type_length: i32| Ok(0i32),
    )?;

    linker.func_wrap(
        "clarity",
        "admit_function_argument",
        |_value_offset: i32,
         _type_offset: i32,
         _type_length: i32,
         _function_name_offset: i32,
         _function_name_length: i32,
         _argument_index: i32| Ok(()),
    )?;

    linker.func_wrap(
        "clarity",
        "runtime_shape_is_equal",
        |_first_offset: i32, _second_offset: i32, _type_offset: i32, _type_length: i32| Ok(0i32),
    )?;

    linker.func_wrap(
        "clarity",
        "merge_runtime_shape",
        |_base_handle: i32, _updates_offset: i32, _type_offset: i32, _type_length: i32| Ok(0i32),
    )?;

    linker.func_wrap(
        "clarity",
        "deserialize_runtime_shape",
        |_bytes_offset: i32, _bytes_length: i32, _type_offset: i32, _type_length: i32| Ok(0i32),
    )?;

    linker.func_wrap(
        "clarity",
        "field_runtime_shape",
        |_base_handle: i32, _name_offset: i32, _name_length: i32| Ok(0i32),
    )?;

    link_skip_list(&mut linker)?;

    // Link in the host interface functions.
    linker.func_wrap(
        "clarity",
        "define_function",
        |_kind: i32, _name_offset: i32, _name_length: i32| {
            println!("define-function");
        },
    )?;

    linker.func_wrap(
        "clarity",
        "define_variable",
        |_name_offset: i32, _name_length: i32, _value_offset: i32, _value_length: i32| {
            println!("define-data-var");
        },
    )?;

    linker.func_wrap(
        "clarity",
        "define_ft",
        |_name_offset: i32,
         _name_length: i32,
         _supply_indicator: i32,
         _supply_lo: i64,
         _supply_hi: i64| {
            println!("define-ft");
        },
    )?;

    linker.func_wrap(
        "clarity",
        "define_nft",
        |_name_offset: i32, _name_length: i32| {
            println!("define-ft");
        },
    )?;

    linker.func_wrap(
        "clarity",
        "define_map",
        |_name_offset: i32, _name_length: i32| {
            println!("define-map");
        },
    )?;

    linker.func_wrap(
        "clarity",
        "define_trait",
        |_name_offset: i32, _name_length: i32| {
            println!("define-trait");
        },
    )?;

    linker.func_wrap(
        "clarity",
        "impl_trait",
        |_name_offset: i32, _name_length: i32| {
            println!("impl-trait");
        },
    )?;

    linker.func_wrap(
        "clarity",
        "get_variable",
        |_name_offset: i32, _name_length: i32, _return_offset: i32, _return_length: i32| {
            println!("var-get");
        },
    )?;

    linker.func_wrap(
        "clarity",
        "set_variable",
        |_name_offset: i32, _name_length: i32, _value_offset: i32, _value_length: i32| {
            println!("var-set");
        },
    )?;

    linker.func_wrap(
        "clarity",
        "print",
        |_value_offset: i32,
         _value_length: i32,
         _serialized_ty_offset: i32,
         _serialized_ty_length: i32| {
            println!("print");
        },
    )?;

    linker.func_wrap(
        "clarity",
        "tx_sender",
        |_return_offset: i32, _return_length: i32| {
            println!("tx-sender");
            Ok((0i32, 0i32))
        },
    )?;

    linker.func_wrap(
        "clarity",
        "contract_caller",
        |_return_offset: i32, _return_length: i32| {
            println!("tx-sender");
            Ok((0i32, 0i32))
        },
    )?;

    linker.func_wrap(
        "clarity",
        "current_contract",
        |_return_offset: i32, _return_length: i32| {
            println!("current-contract");
            Ok((0i32, 0i32))
        },
    )?;

    linker.func_wrap(
        "clarity",
        "tx_sponsor",
        |_return_offset: i32, _return_length: i32| {
            println!("tx-sponsor");
            Ok((0i32, 0i32, 0i32))
        },
    )?;

    linker.func_wrap("clarity", "block_height", |_: Caller<'_, T>| {
        println!("block-height");
        Ok((0i64, 0i64))
    })?;

    linker.func_wrap("clarity", "stacks_block_height", |_: Caller<'_, T>| {
        println!("stacks-block-height");
        Ok((0i64, 0i64))
    })?;

    linker.func_wrap("clarity", "stacks_block_time", |_: Caller<'_, T>| {
        println!("stacks_block_time");
        Ok((0i64, 0i64))
    })?;

    linker.func_wrap("clarity", "tenure_height", |_: Caller<'_, T>| {
        println!("tenure-height");
        Ok((0i64, 0i64))
    })?;

    linker.func_wrap("clarity", "burn_block_height", |_: Caller<'_, T>| {
        println!("burn-block-height");
        Ok((0i64, 0i64))
    })?;

    linker.func_wrap("clarity", "stx_liquid_supply", |_: Caller<'_, T>| {
        println!("stx-liquid-supply");
        Ok((0i64, 0i64))
    })?;

    linker.func_wrap("clarity", "is_in_regtest", |_: Caller<'_, T>| {
        println!("is-in-regtest");
        Ok(0i32)
    })?;

    linker.func_wrap("clarity", "is_in_mainnet", |_: Caller<'_, T>| {
        println!("is-in-mainnet");
        Ok(0i32)
    })?;

    linker.func_wrap("clarity", "chain_id", |_: Caller<'_, T>| {
        println!("chain-id");
        Ok((0i64, 0i64))
    })?;

    linker.func_wrap("clarity", "enter_as_contract", |_: Caller<'_, T>| {
        println!("as-contract: enter");
        Ok(())
    })?;

    linker.func_wrap("clarity", "exit_as_contract", |_: Caller<'_, T>| {
        println!("as-contract: exit");
        Ok(())
    })?;

    linker.func_wrap("clarity", "principal_depth", |_: Caller<'_, T>| {
        Ok((0i32, 0i32))
    })?;

    linker.func_wrap(
        "clarity",
        "restore_principal_depth",
        |_: Caller<'_, T>, _sender: i32, _callers: i32| Ok(()),
    )?;

    linker.func_wrap(
        "clarity",
        "enter_as_contract_safe",
        |_: Caller<'_, T>| -> Option<Rooted<ExternRef>> {
            println!("as-contract?: enter");
            None
        },
    )?;

    linker.func_wrap(
        "clarity",
        "exit_as_contract_safe",
        |_: Caller<'_, T>, _allowance_ref: Option<Rooted<ExternRef>>| {
            println!("as-contract?: exit");
            Ok((0i64, 0i64, 0i32))
        },
    )?;

    linker.func_wrap("clarity", "cleanup_as_contract_safe", |_: Caller<'_, T>| {
        println!("as-contract?: cleanup");
        Ok(())
    })?;

    linker.func_wrap(
        "clarity",
        "enter_restrict_assets",
        |_: Caller<'_, T>| -> Option<Rooted<ExternRef>> {
            println!("restrict-assets?: enter");
            None
        },
    )?;

    linker.func_wrap(
        "clarity",
        "exit_restrict_assets",
        |_: Caller<'_, T>,
         _asset_owner_offset: i32,
         _asset_owner_length: i32,
         _allowance_ref: Option<Rooted<ExternRef>>| {
            println!("restrict-assets?: exit");
            Ok((0i64, 0i64, 0i32))
        },
    )?;

    linker.func_wrap("clarity", "cleanup_restrict_assets", |_: Caller<'_, T>| {
        println!("restrict-assets?: cleanup");
        Ok(())
    })?;

    linker.func_wrap(
        "clarity",
        "with_all_assets_unsafe",
        |_: Caller<'_, T>, _allowance_ref: Option<Rooted<ExternRef>>| {
            println!("with_all_assets_unsafe: enter");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "exit_with_all_assets_unsafe",
        |_: Caller<'_, T>| {
            println!("with_all_assets_unsafe: exit");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "with_ft",
        |_allowance_ref: Option<Rooted<ExternRef>>,
         _contract_id_offset: i32,
         _contract_id_length: i32,
         _token_name_offset: i32,
         _token_name_length: i32,
         _amount_lo: i64,
         _amount_hi: i64| {
            println!("with_ft: enter");
            Ok(())
        },
    )?;

    linker.func_wrap("clarity", "exit_with_ft", |_: Caller<'_, T>| {
        println!("with_ft: exit");
        Ok(())
    })?;

    linker.func_wrap(
        "clarity",
        "with_nft",
        |_allowance_ref: Option<Rooted<ExternRef>>,
         _contract_id_offset: i32,
         _contract_id_length: i32,
         _token_name_offset: i32,
         _token_name_length: i32,
         _identifiers_shape: i32,
         _identifiers_offset: i32,
         _identifiers_length: i32,
         _identifiers_ty_offset: i32,
         _identifiers_ty_length: i32| {
            println!("with_nft: enter");
            Ok(())
        },
    )?;

    linker.func_wrap("clarity", "exit_with_nft", |_: Caller<'_, T>| {
        println!("with_nft: exit");
        Ok(())
    })?;

    linker.func_wrap(
        "clarity",
        "with_stacking",
        |_allowance_ref: Option<Rooted<ExternRef>>, _allowance_lo: i64, _allowance_hi: i64| {
            println!("with_stacking: enter");
            Ok(())
        },
    )?;

    linker.func_wrap("clarity", "exit_with_stacking", |_: Caller<'_, T>| {
        println!("with_stacking: exit");
        Ok(())
    })?;

    linker.func_wrap(
        "clarity",
        "with_pox",
        |_allowance_ref: Option<Rooted<ExternRef>>| Ok(()),
    )?;

    linker.func_wrap(
        "clarity",
        "with_stx",
        |_allowance_ref: Option<Rooted<ExternRef>>, _amount_lo: i64, _amount_hi: i64| {
            println!("with_stx: enter");
            Ok(())
        },
    )?;

    linker.func_wrap("clarity", "exit_with_stx", |_: Caller<'_, T>| {
        println!("with_stx: exit");
        Ok(())
    })?;

    linker.func_wrap(
        "clarity",
        "enter_at_block",
        |_block_hash_offset: i32, _block_hash_length: i32| {
            println!("at-block: enter");
            Ok(())
        },
    )?;

    linker.func_wrap("clarity", "exit_at_block", |_: Caller<'_, T>| {
        println!("at-block: exit");
        Ok(())
    })?;

    linker.func_wrap(
        "clarity",
        "stx_get_balance",
        |_principal_offset: i32, _principal_length: i32| Ok((0i64, 0i64)),
    )?;

    linker.func_wrap(
        "clarity",
        "stx_account",
        |_principal_offset: i32, _principal_length: i32| {
            Ok((0i32, 0i64, 0i64, 0i64, 0i64, 0i64, 0i64))
        },
    )?;

    linker.func_wrap(
        "clarity",
        "stx_burn",
        |_amount_lo: i64, _amount_hi: i64, _principal_offset: i32, _principal_length: i32| {
            Ok((0i32, 0i32, 0i64, 0i64))
        },
    )?;

    linker.func_wrap(
        "clarity",
        "stx_transfer",
        |_amount_lo: i64,
         _amount_hi: i64,
         _from_offset: i32,
         _from_length: i32,
         _to_offset: i32,
         _to_length: i32,
         _memo_offset: i32,
         _memo_length: i32| { Ok((0i32, 0i32, 0i64, 0i64)) },
    )?;

    linker.func_wrap(
        "clarity",
        "ft_get_supply",
        |_name_offset: i32, _name_length: i32| Ok((0i64, 0i64)),
    )?;

    linker.func_wrap(
        "clarity",
        "ft_get_balance",
        |_name_offset: i32, _name_length: i32, _owner_offset: i32, _owner_length: i32| {
            Ok((0i64, 0i64))
        },
    )?;

    linker.func_wrap(
        "clarity",
        "ft_burn",
        |_name_offset: i32,
         _name_length: i32,
         _amount_lo: i64,
         _amount_hi: i64,
         _sender_offset: i32,
         _sender_length: i32| { Ok((0i32, 0i32, 0i64, 0i64)) },
    )?;

    linker.func_wrap(
        "clarity",
        "ft_mint",
        |_name_offset: i32,
         _name_length: i32,
         _amount_lo: i64,
         _amount_hi: i64,
         _sender_offset: i32,
         _sender_length: i32| { Ok((0i32, 0i32, 0i64, 0i64)) },
    )?;

    linker.func_wrap(
        "clarity",
        "ft_transfer",
        |_name_offset: i32,
         _name_length: i32,
         _amount_lo: i64,
         _amount_hi: i64,
         _sender_offset: i32,
         _sender_length: i32,
         _recipient_offset: i32,
         _recipient_length: i32| { Ok((0i32, 0i32, 0i64, 0i64)) },
    )?;

    linker.func_wrap(
        "clarity",
        "nft_get_owner",
        |_name_offset: i32,
         _name_length: i32,
         _asset_offset: i32,
         _asset_length: i32,
         _return_offset: i32,
         _return_length: i32| { Ok((0i32, 0i32, 0i32)) },
    )?;

    linker.func_wrap(
        "clarity",
        "nft_burn",
        |_name_offset: i32,
         _name_length: i32,
         _asset_offset: i32,
         _asset_length: i32,
         _sender_offset: i32,
         _sender_length: i32| { Ok((0i32, 0i32, 0i64, 0i64)) },
    )?;

    linker.func_wrap(
        "clarity",
        "nft_mint",
        |_name_offset: i32,
         _name_length: i32,
         _asset_offset: i32,
         _asset_length: i32,
         _recipient_offset: i32,
         _recipient_length: i32| { Ok((0i32, 0i32, 0i64, 0i64)) },
    )?;

    linker.func_wrap(
        "clarity",
        "nft_transfer",
        |_name_offset: i32,
         _name_length: i32,
         _asset_offset: i32,
         _asset_length: i32,
         _sender_offset: i32,
         _sender_length: i32,
         _recipient_offset: i32,
         _recipient_length: i32| { Ok((0i32, 0i32, 0i64, 0i64)) },
    )?;

    linker.func_wrap(
        "clarity",
        "map_get",
        |_name_offset: i32,
         _name_length: i32,
         _key_offset: i32,
         _key_length: i32,
         _return_offset: i32,
         _return_length: i32| { Ok(0i32) },
    )?;

    linker.func_wrap(
        "clarity",
        "map_set",
        |_name_offset: i32,
         _name_length: i32,
         _key_offset: i32,
         _key_length: i32,
         _value_offset: i32,
         _value_length: i32| { Ok((0i32, 0i32)) },
    )?;

    linker.func_wrap(
        "clarity",
        "map_insert",
        |_name_offset: i32,
         _name_length: i32,
         _key_offset: i32,
         _key_length: i32,
         _value_offset: i32,
         _value_length: i32| { Ok((0i32, 0i32)) },
    )?;

    linker.func_wrap(
        "clarity",
        "map_delete",
        |_name_offset: i32, _name_length: i32, _key_offset: i32, _key_length: i32| Ok(0i32),
    )?;

    linker.func_wrap(
        "clarity",
        "get_block_info_time_property",
        |_height_lo: i64, _height_hi: i64, _return_offset: i32, _return_length: i32| {
            println!("get_block_info_time_property");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "get_block_info_vrf_seed_property",
        |_height_lo: i64, _height_hi: i64, _return_offset: i32, _return_length: i32| {
            println!("get_block_info_vrf_seed_property");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "get_block_info_header_hash_property",
        |_height_lo: i64, _height_hi: i64, _return_offset: i32, _return_length: i32| {
            println!("get_block_info_header_hash_property");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "get_block_info_burnchain_header_hash_property",
        |_height_lo: i64, _height_hi: i64, _return_offset: i32, _return_length: i32| {
            println!("get_block_info_burnchain_header_hash_property");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "get_block_info_identity_header_hash_property",
        |_height_lo: i64, _height_hi: i64, _return_offset: i32, _return_length: i32| {
            println!("get_block_info_identity_header_hash_property");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "get_block_info_miner_address_property",
        |_height_lo: i64, _height_hi: i64, _return_offset: i32, _return_length: i32| {
            println!("get_block_info_miner_address_property");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "get_block_info_miner_spend_winner_property",
        |_height_lo: i64, _height_hi: i64, _return_offset: i32, _return_length: i32| {
            println!("get_block_info_miner_spend_winner_property");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "get_block_info_miner_spend_total_property",
        |_height_lo: i64, _height_hi: i64, _return_offset: i32, _return_length: i32| {
            println!("get_block_info_miner_spend_total_property");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "get_block_info_block_reward_property",
        |_height_lo: i64, _height_hi: i64, _return_offset: i32, _return_length: i32| {
            println!("get_block_info_block_reward_property");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "get_burn_block_info_header_hash_property",
        |_height_lo: i64, _height_hi: i64, _return_offset: i32, _return_length: i32| {
            println!("get_burn_block_info_header_hash_property");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "get_burn_block_info_pox_addrs_property",
        |_height_lo: i64, _height_hi: i64, _return_offset: i32, _return_length: i32| {
            println!("get_burn_block_info_pox_addrs_property");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "get_stacks_block_info_time_property",
        |_height_lo: i64, _height_hi: i64, _return_offset: i32, _return_length: i32| {
            println!("get_stacks_block_info_time_property");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "get_stacks_block_info_header_hash_property",
        |_height_lo: i64, _height_hi: i64, _return_offset: i32, _return_length: i32| {
            println!("get_stacks_block_info_header_hash_property");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "get_stacks_block_info_identity_header_hash_property",
        |_height_lo: i64, _height_hi: i64, _return_offset: i32, _return_length: i32| {
            println!("get_stacks_block_info_identity_header_hash_property");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "get_tenure_info_burnchain_header_hash_property",
        |_height_lo: i64, _height_hi: i64, _return_offset: i32, _return_length: i32| {
            println!("get_tenure_info_burnchain_header_hash_property");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "get_tenure_info_miner_address_property",
        |_height_lo: i64, _height_hi: i64, _return_offset: i32, _return_length: i32| {
            println!("get_tenure_info_miner_address_property");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "get_tenure_info_time_property",
        |_height_lo: i64, _height_hi: i64, _return_offset: i32, _return_length: i32| {
            println!("get_tenure_info_time_property");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "get_tenure_info_vrf_seed_property",
        |_height_lo: i64, _height_hi: i64, _return_offset: i32, _return_length: i32| {
            println!("get_tenure_info_vrf_seed_property");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "get_tenure_info_block_reward_property",
        |_height_lo: i64, _height_hi: i64, _return_offset: i32, _return_length: i32| {
            println!("get_tenure_info_block_reward_property");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "get_tenure_info_miner_spend_total_property",
        |_height_lo: i64, _height_hi: i64, _return_offset: i32, _return_length: i32| {
            println!("get_tenure_info_miner_spend_total_property");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "get_tenure_info_miner_spend_winner_property",
        |_height_lo: i64, _height_hi: i64, _return_offset: i32, _return_length: i32| {
            println!("get_tenure_info_miner_spend_winner_property");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "contract_call",
        |_contract_trait_offset: i32,
         _contract_trait_length: i32,
         _contract_offset: i32,
         _contract_length: i32,
         _function_offset: i32,
         _function_length: i32,
         _args_offset: i32,
         _args_length: i32,
         _return_offset: i32,
         _return_length: i32| {
            println!("contract_call");
            Ok(())
        },
    )?;

    linker.func_wrap("clarity", "check_constant_call_target", || {
        println!("check_constant_call_target");
        Ok(())
    })?;

    linker.func_wrap(
        "clarity",
        "contract_hash",
        |_contract_offset: i32, _contract_length: i32, _return_offset: i32, _return_length: i32| {
            println!("contract_hash");
            Ok(())
        },
    )?;

    linker.func_wrap("clarity", "begin_public_call", || {
        println!("begin_public_call");
        Ok(())
    })?;

    linker.func_wrap("clarity", "begin_read_only_call", || {
        println!("begin_read_only_call");
        Ok(())
    })?;

    linker.func_wrap("clarity", "commit_call", || {
        println!("commit_call");
        Ok(())
    })?;

    linker.func_wrap("clarity", "roll_back_call", || {
        println!("roll_back_call");
        Ok(())
    })?;

    linker.func_wrap(
        "clarity",
        "keccak256",
        |_buffer_offset: i32, _buffer_length: i32, _return_offset: i32, _return_length: i32| {
            println!("keccak256");
            Ok((_return_offset, _return_length))
        },
    )?;

    linker.func_wrap(
        "clarity",
        "sha512",
        |_buffer_offset: i32, _buffer_length: i32, _return_offset: i32, _return_length: i32| {
            println!("sha512");
            Ok((_return_offset, _return_length))
        },
    )?;

    linker.func_wrap(
        "clarity",
        "sha512_256",
        |_buffer_offset: i32, _buffer_length: i32, _return_offset: i32, _return_length: i32| {
            println!("sha512_256");
            Ok((_return_offset, _return_length))
        },
    )?;

    linker.func_wrap(
        "clarity",
        "secp256k1_recover",
        |_msg_offset: i32,
         _msg_length: i32,
         _sig_offset: i32,
         _sig_length: i32,
         _return_offset: i32,
         _return_length: i32| {
            println!("secp256k1_recover");
            Ok(())
        },
    )?;

    linker.func_wrap(
        "clarity",
        "secp256k1_verify",
        |_msg_offset: i32,
         _msg_length: i32,
         _sig_offset: i32,
         _sig_length: i32,
         _pk_offset: i32,
         _pk_length: i32| {
            println!("secp256k1_verify");
            Ok(0i32)
        },
    )?;

    linker.func_wrap(
        "clarity",
        "secp256r1_verify",
        |_msg_offset: i32,
         _msg_length: i32,
         _sig_offset: i32,
         _sig_length: i32,
         _pk_offset: i32,
         _pk_length: i32| Ok(0i32),
    )?;

    linker.func_wrap(
        "clarity",
        "ed25519_verify",
        |_msg_offset: i32,
         _msg_length: i32,
         _sig_offset: i32,
         _sig_length: i32,
         _pk_offset: i32,
         _pk_length: i32| Ok(0i32),
    )?;

    linker.func_wrap(
        "clarity",
        "secp256k1_decompress",
        |_pk_offset: i32, _pk_length: i32, _return_offset: i32, _return_length: i32| Ok(()),
    )?;

    linker.func_wrap(
        "clarity",
        "verify_merkle_proof",
        |_leaf_offset: i32,
         _leaf_length: i32,
         _root_offset: i32,
         _root_length: i32,
         _index_low: i64,
         _index_high: i64,
         _count_low: i64,
         _count_high: i64,
         _siblings_shape: i32,
         _siblings_offset: i32,
         _siblings_length: i32| Ok(0i32),
    )?;

    linker.func_wrap(
        "clarity",
        "get_bitcoin_tx_output",
        |_tx_offset: i32,
         _tx_length: i32,
         _vout_low: i64,
         _vout_high: i64,
         _return_offset: i32,
         _return_length: i32| Ok(()),
    )?;

    linker.func_wrap(
        "clarity",
        "principal_of",
        |_key_offset: i32, _key_length: i32, _principal_offset: i32| {
            println!("secp256k1_verify");
            Ok((0i32, 0i32, 0i32, 0i64, 0i64))
        },
    )?;

    // Create a log function for debugging.
    linker.func_wrap("", "log", |param: i64| {
        println!("log: {param}");
    })?;

    // Create another log function for debugging.
    linker.func_wrap("", "debug_msg", |param: i32| {
        println!("log: {param}");
    })?;

    linker.func_wrap(
        "clarity",
        "save_constant",
        |_name_offset: i32, _name_length: i32, _value_offset: i32, _value_length: i32| {
            println!("save constant");
        },
    )?;

    linker.func_wrap(
        "clarity",
        "load_constant",
        |_name_offset: i32, _name_length: i32, _value_offset: i32, _value_length: i32| {
            println!("load constant");
        },
    )?;

    linker.func_wrap(
        "clarity",
        "principal_to_string_ascii",
        |_principal_offset: i32,
         _principal_length: i32,
         _result_offset: i32,
         _result_length: i32| {
            println!("principal to string ascii");
            Ok(0)
        },
    )?;

    Ok(linker)
}

/// Load the compiled standard library and link in all host interface functions.
pub fn load_stdlib() -> Result<(Instance, Store<()>), wasmtime::Error> {
    let standard_lib = include_bytes!(concat!(env!("OUT_DIR"), "/standard.wasm"));
    let engine = crate::consensus_engine()?;
    let mut store = Store::new(&engine, ());

    let mut linker = dummy_linker(&engine)?;
    link_cost_globals(&mut linker, &mut store.as_context_mut())?;

    let module = Module::new(&engine, standard_lib)?;
    let instance = linker.instantiate(&mut store, &module)?;
    Ok((instance, store))
}
