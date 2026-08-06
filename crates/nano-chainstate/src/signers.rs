//! The signer set a prepare phase writes into the `.signers` boot contract.
//!
//! At the first block of a prepare phase the node itself, not any transaction,
//! computes the next reward cycle's signer set from `PoX-5` and stores it. Those
//! writes are consensus state, so a node that skips them diverges from the
//! reward cycle onwards.

use std::collections::BTreeMap;

use clarity::vm::{
    ClarityName, Value,
    types::{PrincipalData, QualifiedContractIdentifier, StandardPrincipalData, TupleData},
};
use nano_address::{PoxAddress, PoxAddressType32};
use nano_crypto::StacksPublicKey;
use nano_primitives::Network;
use nano_primitives::{Hash160, hash160};
use nano_vm::{BitcoinBlockContext, Vm};

use crate::{ChainStateError, SignerSet, SignerWeights};

/// Reward addresses paid by one Bitcoin commitment.
const OUTPUTS_PER_COMMIT: u32 = 2;

/// The sBTC registry a waterfall cycle's payout address is derived from.
///
/// Not a boot contract, so `boot_code_id` cannot name it. Mainnet's is fixed and
/// non-negotiable; every other chain deploys its own, which is why stacks-core
/// makes that one configurable (`NodeConfig::pox_5_sbtc_registry_contract`) — and
/// why nano asks for it rather than defaulting. The captured hacknet chain's is
/// at `ST2SBXRBJJTH7GV5J93HJ62W2NRRQ46XYBK92Y039`, not at the testnet
/// deployment, so a default would have been wrong on the first chain tried.
const SBTC_REGISTRY_MAINNET: &str = "SM3VDXK3WZZSA84XXFKAFAF15NNZX32CTSG82JFQ4.sbtc-registry";

/// The Clarity type prefix of a contract principal, which is how the sBTC
/// deposit script names the recipient (`clarity-types`, `PrincipalData`'s
/// `inner_consensus_serialize`: the prefix, the issuer's version and hash, then
/// the contract name length-prefixed).
const CONTRACT_PRINCIPAL_PREFIX: u8 = 6;

/// Address versions for a standard single-signature principal.
///
/// `handle_signer_stackerdb_update` renders each signer with
/// `StacksAddress::p2pkh_from_hash(is_mainnet, ..)`, so the version follows the
/// network. It was the testnet one unconditionally here, which is invisible on a
/// testnet chain and would have written a principal mainnet does not for every
/// signer of every cycle nano sets up — a state root divergence at the first
/// prepare phase, with nothing else to show for it.
const MAINNET_SINGLE_SIGNATURE_VERSION: u8 = 22;
const TESTNET_SINGLE_SIGNATURE_VERSION: u8 = 26;

/// The reward cycle a prepare-phase block computes the signer set for.
///
/// The last block of a cycle counts towards that cycle; every other
/// prepare-phase block counts towards the next one.
#[must_use]
pub fn prepare_phase_reward_cycle(context: BitcoinBlockContext) -> Option<u64> {
    let length = u64::from(context.prepare_phase_length + context.reward_phase_length);
    // The *tenure's* burn height, not the block's burn view. stacks-core drives
    // this from `tenure_block_snapshot.block_height` (`setup_block`,
    // `chainstate/nakamoto/mod.rs`), and the two part company the moment a tenure
    // is extended: the extend moves the view forward while the tenure keeps the
    // burn block that elected it.
    //
    // Reading the view set a cycle's signer set for a prepare phase the tenure had
    // not reached. On the live pox-5 chain, whose sortitions had stopped at burn
    // 393, block 931's view jumped to burn 399 -- inside cycle 19's prepare phase --
    // while its tenure stayed at 392. nano set cycle 20's set and stacks-core did
    // not: `last-set-cycle` on that chain is still 19. Four keys of difference,
    // identical receipts, identical costs, and the state roots parted.
    let height = context.tenure_burn_height();
    if height <= context.first_height || length == 0 {
        return None;
    }
    let effective = height - context.first_height;
    let index = effective % length;
    if index != 0 && index <= length - u64::from(context.prepare_phase_length) {
        return None;
    }
    let cycle = effective / length;
    Some(if index == 0 { cycle } else { cycle + 1 })
}

/// The reward cycle a burn height falls in.
#[must_use]
pub const fn reward_cycle_at(context: BitcoinBlockContext) -> Option<u64> {
    let length = context.prepare_phase_length as u64 + context.reward_phase_length as u64;
    if context.height <= context.first_height || length == 0 {
        return None;
    }
    Some((context.height - context.first_height) / length)
}

/// The signer set that attests to blocks in `context`'s reward cycle, as this
/// chain's own executed state records it.
///
/// Read out of `.signers` rather than re-derived from the pox-5 linked list, for
/// three reasons that all point the same way:
///
/// - **It is the same answer, already checked.** The node writes those entries
///   itself, in a prepare phase, from that same list — and the writes are
///   consensus state, so a wrong set fails the state root of the block that
///   wrote it. Re-deriving would repeat work the MARF root has already agreed
///   with the network about.
/// - **It reaches back before the checkpoint.** A cycle set up by stacks-core
///   before the state was exported has no pox-5 positions to walk — mainnet's
///   cycle 140 was stacked in pox-4 — but its `.signers` entries came across
///   with the state. That is the difference between checking mainnet's signer
///   weight today and not checking it at all.
/// - **One contract call instead of forty-five.** The walk costs three calls per
///   staker; on the captured chain that was 120 ms a block, twenty-five times
///   what the rest of the block's validation costs.
///
/// A cycle nobody has set up yet answers `NoSignerSet`, which is not a fault in
/// the block: the chain is well-formed and there is simply nothing recorded to
/// check it against.
pub fn recorded_signer_set(
    vm: &mut Vm,
    context: BitcoinBlockContext,
) -> Result<SignerWeights, ChainStateError> {
    let cycle = reward_cycle_at(context).ok_or(ChainStateError::NoSignerSet(0))?;
    let signers = boot_contract(vm.network(), "signers");
    let recorded = read_optional(
        vm,
        &signers,
        "get-signers",
        &[Value::UInt(u128::from(cycle))],
    )
    .map_err(|_| ChainStateError::NoSignerSet(cycle))?
    .ok_or(ChainStateError::NoSignerSet(cycle))?;
    let entries = recorded
        .expect_list()
        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))?
        .into_iter()
        .map(signer_entry)
        .collect::<Result<Vec<_>, ChainStateError>>()?;
    if entries.is_empty() {
        return Err(ChainStateError::NoSignerSet(cycle));
    }
    SignerWeights::new(entries)
        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))
}

/// One `{ signer: principal, weight: uint }` as the contract stores it.
fn signer_entry(value: Value) -> Result<(Hash160, u32), ChainStateError> {
    let bad = |what: &str| ChainStateError::InvalidTransaction(format!("a signer entry {what}"));
    let mut tuple = value.expect_tuple().map_err(|_| bad("is not a tuple"))?;
    let signer = tuple
        .data_map
        .remove("signer")
        .ok_or_else(|| bad("names no signer"))?
        .expect_principal()
        .map_err(|_| bad("names no principal"))?;
    let weight = tuple
        .data_map
        .remove("weight")
        .ok_or_else(|| bad("carries no weight"))?
        .expect_u128()
        .map_err(|_| bad("carries no weight"))?;
    // Only the hash: the version byte in front of it says which network the
    // principal was rendered for, and the same signer is the same signer on
    // either. Matching on the whole principal would refuse every entry a
    // stacks-core node wrote on mainnet.
    let PrincipalData::Standard(address) = signer else {
        return Err(bad("is a contract, not a signer"));
    };
    Ok((
        Hash160::from_bytes(address.1),
        u32::try_from(weight).map_err(|_| bad("weighs more than a signer can"))?,
    ))
}

/// A reward cycle's signer set as its own `PoX-5` positions derive it.
///
/// `SignerSet` alone is what consensus needs — a key and a weight — and it is
/// deliberately no more than that. Everything else here exists only in the
/// derivation and cannot be recovered from a weight afterwards: the per-slot
/// threshold, and what each signer actually stacked. Both are fields of the
/// `/v3/stacker_set` document a signer reads, so a node that discards them
/// serves zeros.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DerivedRewardSet {
    pub reward_cycle: u64,
    /// The weighted signers, in the order the signature check and the signer
    /// bitvec read them.
    pub signers: SignerSet,
    /// What each signer stacked, in `signers` order.
    pub stacked: Vec<u128>,
    pub pox_ustx_threshold: u128,
}

/// The signer set derived from the pox-5 positions stacked for a reward cycle.
///
/// This is the *derivation*; `recorded_signer_set` is what the chain wrote down
/// from it, and reading that is what block validation does. This one stays
/// because `update_signer_set` writes from it, because the two agreeing is what
/// makes reading the recorded one safe — and because the threshold and the
/// stacked amounts exist only here.
pub fn active_signer_set(
    vm: &mut Vm,
    context: BitcoinBlockContext,
) -> Result<DerivedRewardSet, ChainStateError> {
    let cycle = reward_cycle_at(context).ok_or(ChainStateError::NoSignerSet(0))?;
    derive_signer_set(vm, cycle, context.reward_phase_length * OUTPUTS_PER_COMMIT)
}

/// Walk a cycle's positions and apportion them over its reward slots.
fn derive_signer_set(
    vm: &mut Vm,
    reward_cycle: u64,
    reward_slots: u32,
) -> Result<DerivedRewardSet, ChainStateError> {
    let stakers = stake_entries(vm, reward_cycle)?;
    if stakers.is_empty() {
        return Err(ChainStateError::NoSignerSet(reward_cycle));
    }
    // Summed per key exactly as the apportionment sums it: one signer may hold
    // more than one position, and the amount served has to be the amount weighed.
    let mut amounts: BTreeMap<[u8; 33], u128> = BTreeMap::new();
    for (key, amount) in &stakers {
        let entry = amounts.entry(key.to_bytes_compressed()).or_default();
        *entry = entry.saturating_add(*amount);
    }
    let (signers, pox_ustx_threshold) = SignerSet::from_reward_slots(stakers, reward_slots)
        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))?;
    let stacked = signers
        .signers()
        .iter()
        .map(|signer| {
            amounts
                .get(&signer.public_key.to_bytes_compressed())
                .copied()
                .unwrap_or_default()
        })
        .collect();
    Ok(DerivedRewardSet {
        reward_cycle,
        signers,
        stacked,
        pox_ustx_threshold,
    })
}

/// The single Bitcoin output a waterfall reward cycle pays.
///
/// It is not an address anyone chose: it is the taproot output key of the sBTC
/// deposit script for the registry's current aggregate key, paying `.pox-5`
/// (`chainstate/nakamoto/signer_set.rs`, `pox_5_compute_and_update_signers`).
/// A chain with no registry to ask has no waterfall address, and says so.
pub fn sbtc_payout_address(
    vm: &mut Vm,
    configured: Option<&str>,
) -> Result<PoxAddress, ChainStateError> {
    let network = vm.network();
    let registry = sbtc_registry_contract(network, configured)?;
    let sender = PrincipalData::Standard(registry.issuer.clone());
    let value = vm.call_contract_values(&sender, &registry, "get-current-aggregate-pubkey", &[])?;
    let value = match value {
        Value::Response(response) if response.committed => *response.data,
        other => other,
    };
    let key: [u8; 33] = expect_buffer(value)?.try_into().map_err(|_| {
        ChainStateError::InvalidTransaction(
            "the sBTC registry's aggregate key is not 33 bytes".to_owned(),
        )
    })?;
    let recipient = contract_principal_bytes(&boot_contract(network, "pox-5"));
    let output = nano_bitcoin::sbtc::sbtc_pox5_deposit_taproot_output_key(&key, &recipient)
        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))?;
    Ok(PoxAddress::Addr32 {
        mainnet: network.is_mainnet(),
        address_type: PoxAddressType32::P2tr,
        bytes: output,
    })
}

/// The registry contract this chain reads its aggregate key from.
///
/// Mainnet's is the constant whatever an operator says, because a mainnet node
/// reading another contract's aggregate key would derive a payout address the
/// network does not pay. Anywhere else there is nothing to fall back on.
fn sbtc_registry_contract(
    network: Network,
    configured: Option<&str>,
) -> Result<QualifiedContractIdentifier, ChainStateError> {
    let name = if network.is_mainnet() {
        SBTC_REGISTRY_MAINNET
    } else {
        configured.ok_or_else(|| {
            ChainStateError::InvalidTransaction(
                "this chain's sBTC registry contract is not configured".to_owned(),
            )
        })?
    };
    QualifiedContractIdentifier::parse(name)
        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))
}

/// A contract principal in the encoding the deposit script commits to.
fn contract_principal_bytes(contract: &QualifiedContractIdentifier) -> Vec<u8> {
    let name = contract.name.as_bytes();
    let mut bytes = Vec::with_capacity(23 + name.len());
    bytes.push(CONTRACT_PRINCIPAL_PREFIX);
    bytes.push(contract.issuer.version());
    bytes.extend_from_slice(&contract.issuer.1);
    bytes.push(u8::try_from(name.len()).unwrap_or(u8::MAX));
    bytes.extend_from_slice(name);
    bytes
}

/// Compute and store the next cycle's signer set, if this block starts a prepare
/// phase, and answer with the set it wrote.
///
/// The answer is what a `new_block` event reports as the cycle a block anchored:
/// this is the one place that knows a set was computed rather than merely
/// readable, and stacks-core publishes it from the same transition.
pub fn update_signer_set(
    vm: &mut Vm,
    context: BitcoinBlockContext,
    coinbase_height: u64,
) -> Result<Option<DerivedRewardSet>, ChainStateError> {
    let Some(reward_cycle) = prepare_phase_reward_cycle(context) else {
        return Ok(None);
    };
    let network = vm.network();
    let signers = boot_contract(network, "signers");
    // A cycle is only set up once, by whichever block reaches the prepare phase first.
    let Ok(last_set_cycle) = read_u128(vm, &signers, "get-last-set-cycle", &[]) else {
        return Ok(None);
    };
    if last_set_cycle >= u128::from(reward_cycle) {
        return Ok(None);
    }

    let reward_slots = context.reward_phase_length * OUTPUTS_PER_COMMIT;
    let derived = derive_signer_set(vm, reward_cycle, reward_slots)?;
    let set = &derived.signers;
    let principals = set
        .signers()
        .iter()
        .map(|signer| signing_principal(network, &signer.public_key))
        .collect::<Vec<_>>();

    let slots = tuple_list(
        &principals,
        set,
        &ClarityName::from_literal("num-slots"),
        |_| Value::UInt(1),
    )?;
    let weights = tuple_list(
        &principals,
        set,
        &ClarityName::from_literal("weight"),
        |weight| Value::UInt(u128::from(weight)),
    )?;
    vm.call_contract_values(
        &boot_sender(network),
        &signers,
        "stackerdb-set-signer-slots",
        &[
            slots,
            Value::UInt(u128::from(reward_cycle)),
            Value::UInt(u128::from(coinbase_height)),
        ],
    )?;
    vm.call_contract_values(
        &boot_sender(network),
        &signers,
        "set-signers",
        &[Value::UInt(u128::from(reward_cycle)), weights],
    )?;
    Ok(Some(derived))
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

fn signing_principal(network: Network, public_key: &StacksPublicKey) -> PrincipalData {
    let version = if network.is_mainnet() {
        MAINNET_SINGLE_SIGNATURE_VERSION
    } else {
        TESTNET_SINGLE_SIGNATURE_VERSION
    };
    PrincipalData::Standard(
        StandardPrincipalData::new(
            version,
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
    let value = vm.call_contract_values(&sender, contract, function, arguments)?;
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
    vm.call_contract_values(&sender, contract, function, arguments)?
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
        let mut context = BitcoinBlockContext::at_height(height);
        context.first_height = 0;
        context.prepare_phase_length = 5;
        context.reward_phase_length = 15;
        context
    }

    /// A block whose tenure sat at one burn block while an extend moved its view to
    /// another, which is the only way the two part.
    fn extended(tenure: u64, view: u64) -> BitcoinBlockContext {
        let mut context = context(tenure);
        context.extend_view_to(view);
        context
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

    /// An extended tenure whose *view* has reached a prepare phase its *tenure* has
    /// not sets up nothing.
    ///
    /// stacks-core drives this from `tenure_block_snapshot.block_height`
    /// (`setup_block`), so the two answers part the moment a tenure is extended: the
    /// extend moves the Clarity burn view forward and the tenure keeps the burn block
    /// that elected it.
    ///
    /// This is pox-5 height 931, minimised. There the tenure sat at burn 392 -- that
    /// chain's sortitions had stopped at 393 -- and one extend moved the view to burn
    /// 399, inside cycle 19's prepare phase. Reading the view set cycle 20's signer
    /// set where stacks-core set nothing: `last-set-cycle` on that chain is still 19.
    /// Four keys of difference, identical receipts, identical costs, and the state
    /// roots parted.
    #[test]
    fn an_extended_view_does_not_set_up_the_tenures_next_cycle() {
        // The view is in the prepare phase and the tenure is not: nothing.
        assert_eq!(prepare_phase_reward_cycle(extended(312, 316)), None);
        assert_eq!(prepare_phase_reward_cycle(extended(392, 399)), None);
        // Both in it: the tenure's own cycle, and the view is not consulted.
        assert_eq!(prepare_phase_reward_cycle(extended(316, 319)), Some(16));
        // The tenure in it and the view past it, which an extend also produces.
        assert_eq!(prepare_phase_reward_cycle(extended(316, 322)), Some(16));
        // An extend that moves nothing is the ordinary block.
        assert_eq!(prepare_phase_reward_cycle(extended(316, 316)), Some(16));
    }
}
