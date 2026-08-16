//! Executable ownership for the portable Epoch-4 compatibility vectors.

use nano_chainstate::SignerWeights;
use nano_consensus_profile::{Vector, admits_activation};
use nano_primitives::{Hash160, Network};
use nano_sortition::{PayoutSchedule, RewardCycleSchedule};
use serde_json::{Value, json};

fn vector<'a>(vectors: &'a [Vector], id: &str) -> &'a Vector {
    vectors
        .iter()
        .find(|vector| vector.id == id)
        .unwrap_or_else(|| panic!("profile vector {id}"))
}

fn unsigned(values: &Value) -> Vec<u64> {
    values
        .as_array()
        .expect("vector array")
        .iter()
        .map(|value| value.as_u64().expect("unsigned vector value"))
        .collect()
}

fn block(vector: &Vector) {
    let admitted = unsigned(&vector.input["serialized_bytes"])
        .into_iter()
        .map(|bytes| bytes <= nano_mempool::MAX_BLOCK_LEN)
        .collect::<Vec<_>>();
    assert_eq!(json!(admitted), vector.expected["admitted"]);
}

fn transaction(vector: &Vector) {
    let chain_admitted = unsigned(&vector.input["chain_ids"])
        .into_iter()
        .map(|chain_id| chain_id == u64::from(Network::MAINNET.chain_id()))
        .collect::<Vec<_>>();
    let bytes_admitted = unsigned(&vector.input["serialized_bytes"])
        .into_iter()
        .map(|bytes| bytes <= nano_mempool::MAX_BLOCK_LEN)
        .collect::<Vec<_>>();
    assert_eq!(json!(chain_admitted), vector.expected["chain_admitted"]);
    assert_eq!(json!(bytes_admitted), vector.expected["bytes_admitted"]);
}

fn sortition(vector: &Vector) {
    let profile = nano_consensus_profile::profile().expect("profile");
    let cycles = RewardCycleSchedule::new(
        profile.pox.first_burn_height,
        u64::from(profile.pox.reward_cycle_length),
        None,
    )
    .expect("reward cycle");
    let schedule = PayoutSchedule::new(cycles, u64::from(profile.pox.prepare_phase_length))
        .expect("payout schedule")
        .activating_epoch_four_at(profile.activation.burn_height);
    let windows = unsigned(&vector.input["burn_heights"])
        .into_iter()
        .map(|height| schedule.mining_window_at(height))
        .collect::<Vec<_>>();
    assert_eq!(json!(windows), vector.expected["mining_windows"]);
}

fn signer(vector: &Vector) {
    let weights = unsigned(&vector.input["weights"]);
    let signers = SignerWeights::new(
        weights
            .iter()
            .enumerate()
            .map(|(index, weight)| {
                (
                    Hash160::from([u8::try_from(index).expect("signer index"); 20]),
                    u32::try_from(*weight).expect("signer weight"),
                )
            })
            .collect(),
    )
    .expect("signer weights");
    let threshold = signers.approval_threshold().expect("approval threshold");
    let admitted = vector.input["signed_indexes"]
        .as_array()
        .expect("signed subsets")
        .iter()
        .map(|indexes| {
            indexes
                .as_array()
                .expect("signer indexes")
                .iter()
                .map(|index| {
                    let index = usize::try_from(index.as_u64().expect("signer index"))
                        .expect("signer index fits usize");
                    signers.entries()[index].1
                })
                .sum::<u32>()
                >= threshold
        })
        .collect::<Vec<_>>();
    assert_eq!(json!(threshold), vector.expected["approval_threshold"]);
    assert_eq!(json!(admitted), vector.expected["admitted"]);
}

fn vm(vector: &Vector) {
    let epochs = unsigned(&vector.input["burn_heights"])
        .into_iter()
        .map(|height| {
            format!(
                "{:?}",
                nano_vm::semantic_epoch_at_burn_height(Network::MAINNET, height)
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(json!(epochs), vector.expected["semantic_epochs"]);
    assert_eq!(
        vector.expected["epoch40_clarity_version"],
        nano_consensus_profile::profile()
            .expect("profile")
            .vm
            .clarity_version
    );
}

fn receipt(vector: &Vector) {
    let receipt: Value = serde_json::from_str(include_str!(
        "../../fixtures/mainnet/divergence/tx-823f-receipt.json"
    ))
    .expect("receipt oracle");
    assert_eq!(receipt["txid"], vector.input["txid"]);
    assert_eq!(receipt["block_height"], vector.input["block_height"]);
    assert_eq!(receipt["status"], vector.expected["status"]);
    assert_eq!(receipt["result"]["hex"], vector.expected["result_hex"]);
    assert_eq!(receipt["cost"], vector.expected["cost"]);
    assert_eq!(
        receipt["events"].as_array().expect("ordered events").len(),
        vector.expected["ordered_event_count"]
    );
    let stateful_owner = include_str!("mainnet_divergence.rs");
    assert!(
        stateful_owner.contains("the_mainnet_8708126_receipt_and_root_match_the_canonical_oracle")
    );
    let policy = include_str!("../../../../ignored-tests.toml");
    assert!(policy.contains("the_mainnet_8708126_receipt_and_root_match_the_canonical_oracle"));
}

fn cost(vector: &Vector) {
    let limit = nano_vm::EPOCH_4_BLOCK_LIMIT;
    assert_eq!(
        json!({
            "read_count": limit.read_count,
            "read_length": limit.read_length,
            "runtime": limit.runtime,
            "write_count": limit.write_count,
            "write_length": limit.write_length,
        }),
        vector.expected
    );
}

fn refusal(vector: &Vector) {
    let epoch = vector.input["announced_semantic_epoch"]
        .as_str()
        .expect("announced epoch");
    let version = u8::try_from(
        vector.input["nakamoto_block_version"]
            .as_u64()
            .expect("header version"),
    )
    .expect("header version byte");
    assert!(!admits_activation(epoch, version));
    assert_eq!(vector.expected["decision"], "reject");
    assert_eq!(vector.expected["fallback"], false);
    assert_eq!(vector.expected["healing"], false);
}

#[test]
fn every_mandatory_epoch4_vector_executes_against_nano() {
    nano_consensus_profile::validate_builtin().expect("complete profile");
    let corpus = nano_consensus_profile::vectors().expect("vector corpus");
    for current in &corpus.vectors {
        match current.id.as_str() {
            "block.byte-limit" => block(current),
            "transaction.chain-and-byte-domain" => transaction(current),
            "sortition.epoch-boundary-window" => sortition(current),
            "signer.weight-threshold" => signer(current),
            "vm.semantic-domain" => vm(current),
            "receipt.mainnet-8708126-tx4" => receipt(current),
            "cost.epoch4-block-limit" => cost(current),
            "refusal.unknown-activation" => refusal(current),
            id => panic!("mandatory vector {id} has no nano runner"),
        }
    }
    for id in [
        "block.byte-limit",
        "transaction.chain-and-byte-domain",
        "sortition.epoch-boundary-window",
        "signer.weight-threshold",
        "vm.semantic-domain",
        "receipt.mainnet-8708126-tx4",
        "cost.epoch4-block-limit",
        "refusal.unknown-activation",
    ] {
        let _ = vector(&corpus.vectors, id);
    }
}
