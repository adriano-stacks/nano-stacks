//! The signer set a prepare phase writes into the `.signers` boot contract.
//!
//! At the first block of a prepare phase the node itself, not any transaction,
//! computes the next reward cycle's signer set from `PoX-5` and stores it. Those
//! writes are consensus state, so a node that skips them diverges from the
//! reward cycle onwards.

use clarity::vm::{
    ClarityName, Value,
    types::{PrincipalData, QualifiedContractIdentifier, StandardPrincipalData, TupleData},
};
use nano_crypto::StacksPublicKey;
use nano_primitives::Network;
use nano_primitives::hash160;
use nano_vm::{BitcoinBlockContext, Vm};

use crate::{ChainStateError, SignerSet};

/// Reward addresses paid by one Bitcoin commitment.
const OUTPUTS_PER_COMMIT: u32 = 2;

/// Testnet address version for a standard single-signature principal.
const TESTNET_SINGLE_SIGNATURE_VERSION: u8 = 26;

/// The reward cycle a prepare-phase block computes the signer set for.
///
/// The last block of a cycle counts towards that cycle; every other
/// prepare-phase block counts towards the next one.
#[must_use]
pub fn prepare_phase_reward_cycle(context: BitcoinBlockContext) -> Option<u64> {
    let length = u64::from(context.prepare_phase_length + context.reward_phase_length);
    if context.height <= context.first_height || length == 0 {
        return None;
    }
    let effective = context.height - context.first_height;
    let index = effective % length;
    if index != 0 && index <= length - u64::from(context.prepare_phase_length) {
        return None;
    }
    let cycle = effective / length;
    Some(if index == 0 { cycle } else { cycle + 1 })
}

/// Compute and store the next cycle's signer set, if this block starts a prepare phase.
pub fn update_signer_set(
    vm: &mut Vm,
    context: BitcoinBlockContext,
    coinbase_height: u64,
) -> Result<(), ChainStateError> {
    let Some(reward_cycle) = prepare_phase_reward_cycle(context) else {
        return Ok(());
    };
    let network = vm.network();
    let signers = boot_contract(network, "signers");
    // A cycle is only set up once, by whichever block reaches the prepare phase first.
    let Ok(last_set_cycle) = read_u128(vm, &signers, "get-last-set-cycle", &[]) else {
        return Ok(());
    };
    if last_set_cycle >= u128::from(reward_cycle) {
        return Ok(());
    }

    let reward_slots = context.reward_phase_length * OUTPUTS_PER_COMMIT;
    let stakers = stake_entries(vm, reward_cycle)?;
    if stakers.is_empty() {
        return Err(ChainStateError::NoSignerSet(reward_cycle));
    }
    let (set, _) = SignerSet::from_reward_slots(stakers, reward_slots)
        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))?;
    let principals = set
        .signers()
        .iter()
        .map(|signer| signing_principal(&signer.public_key))
        .collect::<Vec<_>>();

    let slots = tuple_list(
        &principals,
        &set,
        &ClarityName::from_literal("num-slots"),
        |_| Value::UInt(1),
    )?;
    let weights = tuple_list(
        &principals,
        &set,
        &ClarityName::from_literal("weight"),
        |weight| Value::UInt(u128::from(weight)),
    )?;
    vm.call_contract_values(
        boot_sender(network),
        &signers,
        "stackerdb-set-signer-slots",
        &[
            slots,
            Value::UInt(u128::from(reward_cycle)),
            Value::UInt(u128::from(coinbase_height)),
        ],
    )?;
    vm.call_contract_values(
        boot_sender(network),
        &signers,
        "set-signers",
        &[Value::UInt(u128::from(reward_cycle)), weights],
    )?;
    Ok(())
}

/// Walk the `PoX-5` linked list of signers registered for a reward cycle.
fn stake_entries(
    vm: &mut Vm,
    reward_cycle: u64,
) -> Result<Vec<(StacksPublicKey, u128)>, ChainStateError> {
    let pox = boot_contract(vm.network(), "pox-5");
    let cycle = Value::UInt(u128::from(reward_cycle));
    let mut current = read_optional(
        vm,
        &pox,
        "get-signer-set-first-item-for-cycle",
        std::slice::from_ref(&cycle),
    )?
    .map(expect_principal)
    .transpose()?;
    let mut entries = Vec::new();
    while let Some(signer) = current {
        let lookup = Value::Principal(signer);
        current = read_optional(
            vm,
            &pox,
            "get-signer-set-next-item-for-cycle",
            &[lookup.clone(), cycle.clone()],
        )?
        .map(expect_principal)
        .transpose()?;

        // A malformed entry is skipped, but must not stall the walk.
        let Some(key) = read_optional(vm, &pox, "get-signer-info", std::slice::from_ref(&lookup))
            .ok()
            .flatten()
        else {
            continue;
        };
        let Ok(key) = expect_buffer(key).and_then(|bytes| {
            StacksPublicKey::from_bytes(&bytes)
                .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))
        }) else {
            continue;
        };
        let Ok(amount) = read_u128(
            vm,
            &pox,
            "get-amount-delegated-for-signer",
            &[lookup, cycle.clone()],
        ) else {
            continue;
        };
        if amount != 0 {
            entries.push((key, amount));
        }
    }
    Ok(entries)
}

fn tuple_list(
    principals: &[PrincipalData],
    set: &SignerSet,
    field: &ClarityName,
    value: impl Fn(u32) -> Value,
) -> Result<Value, ChainStateError> {
    let entries = principals
        .iter()
        .zip(set.signers())
        .map(|(principal, signer)| {
            TupleData::from_data(vec![
                (
                    ClarityName::from_literal("signer"),
                    Value::Principal(principal.clone()),
                ),
                (field.clone(), value(signer.weight)),
            ])
            .map(Value::Tuple)
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))?;
    Value::cons_list_unsanitized(entries)
        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))
}

fn signing_principal(public_key: &StacksPublicKey) -> PrincipalData {
    PrincipalData::Standard(
        StandardPrincipalData::new(
            TESTNET_SINGLE_SIGNATURE_VERSION,
            *hash160(&public_key.to_bytes_compressed()).as_bytes(),
        )
        .expect("a standard address version is valid"),
    )
}

fn boot_contract(network: Network, name: &str) -> QualifiedContractIdentifier {
    clarity::boot_util::boot_code_id(name, network.is_mainnet())
}

fn boot_sender(network: Network) -> PrincipalData {
    PrincipalData::Standard(boot_contract(network, "signers").issuer)
}

fn read_u128(
    vm: &mut Vm,
    contract: &QualifiedContractIdentifier,
    function: &str,
    arguments: &[Value],
) -> Result<u128, ChainStateError> {
    let sender = boot_sender(vm.network());
    let value = vm.call_contract_values(sender, contract, function, arguments)?;
    let value = match value {
        Value::Response(response) if response.committed => *response.data,
        other => other,
    };
    value
        .expect_u128()
        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))
}

fn read_optional(
    vm: &mut Vm,
    contract: &QualifiedContractIdentifier,
    function: &str,
    arguments: &[Value],
) -> Result<Option<Value>, ChainStateError> {
    let sender = boot_sender(vm.network());
    vm.call_contract_values(sender, contract, function, arguments)?
        .expect_optional()
        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))
}

fn expect_principal(value: Value) -> Result<PrincipalData, ChainStateError> {
    value
        .expect_principal()
        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))
}

fn expect_buffer(value: Value) -> Result<Vec<u8>, ChainStateError> {
    value
        .expect_buff(33)
        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))
}

#[cfg(test)]
mod tests {
    use nano_vm::BitcoinBlockContext;

    use super::prepare_phase_reward_cycle;

    fn context(height: u64) -> BitcoinBlockContext {
        BitcoinBlockContext {
            height,
            first_height: 0,
            prepare_phase_length: 5,
            reward_phase_length: 15,
            ..BitcoinBlockContext::at_height(height)
        }
    }

    #[test]
    fn only_prepare_phase_blocks_set_up_a_cycle() {
        assert_eq!(prepare_phase_reward_cycle(context(314)), None);
        assert_eq!(prepare_phase_reward_cycle(context(315)), None);
        assert_eq!(prepare_phase_reward_cycle(context(316)), Some(16));
        assert_eq!(prepare_phase_reward_cycle(context(319)), Some(16));
        // The last block of a cycle belongs to that cycle, not the next one.
        assert_eq!(prepare_phase_reward_cycle(context(320)), Some(16));
        assert_eq!(prepare_phase_reward_cycle(context(321)), None);
    }
}
