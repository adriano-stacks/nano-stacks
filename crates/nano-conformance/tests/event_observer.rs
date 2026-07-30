//! nano's `new_block` payloads against the ones stacks-core published.
//!
//! An event observer is how everything downstream of a node reads a chain, and
//! the captured `events/new_block` stream is exactly what stacks-core sent for
//! the blocks nano replays. Rebuilding those payloads from nano's own executed
//! receipts and diffing them is the strongest offline check there is that an
//! observer cannot tell the two nodes apart.
//!
//! What the capture supplies as context — the sortition that elected the
//! block, its parent's burn view, the rewards that matured under it, the
//! reward set it anchored — nano's chain state does not expose to the RPC yet.
//! Everything else, including every transaction receipt and every Clarity
//! event, is nano's own.

use std::path::{Path, PathBuf};

use nano_chainstate::{AppliedBlock, NakamotoBlock, starts_new_tenure};
use nano_conformance::replay_captured_blocks;
use nano_primitives::{BitcoinHeaderHash, BlockHeaderHash, Sha256Sum, StacksBlockId};
use nano_rpc::{
    BlockEventContext, MaturedReward, RewardSetEvent, RewardSetSigner, new_block_payload,
};
use serde_json::Value;

/// How deep the payload diff replays, which is the whole capture.
const REPLAY_BLOCKS: u64 = 600;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn bytes32(value: &Value) -> [u8; 32] {
    hex::decode(value.as_str().expect("hash").trim_start_matches("0x"))
        .expect("hexadecimal hash")
        .try_into()
        .expect("32-byte hash")
}

fn amount(value: &Value) -> u128 {
    value
        .as_str()
        .expect("amount")
        .parse()
        .expect("decimal amount")
}

/// The `new_block` event the capture recorded for this block.
fn captured_event(root: &Path, block: &NakamotoBlock) -> Value {
    let name = format!(
        "{:08}-{}.json",
        block.header.chain_length,
        block.header.block_hash()
    );
    let path = root.join("events/new_block").join(&name);
    serde_json::from_slice(&std::fs::read(&path).unwrap_or_else(|_| panic!("read {name}")))
        .unwrap_or_else(|_| panic!("decode {name}"))
}

fn matured_rewards(event: &Value) -> Vec<MaturedReward> {
    event["matured_miner_rewards"]
        .as_array()
        .expect("matured rewards")
        .iter()
        .map(|reward| MaturedReward {
            recipient: reward["recipient"].as_str().expect("recipient").to_owned(),
            miner_address: reward["miner_address"].as_str().expect("miner").to_owned(),
            coinbase: amount(&reward["coinbase_amount"]),
            tx_fees_anchored: amount(&reward["tx_fees_anchored"]),
            tx_fees_streamed_confirmed: amount(&reward["tx_fees_streamed_confirmed"]),
            tx_fees_streamed_produced: amount(&reward["tx_fees_streamed_produced"]),
            from_stacks_block_hash: BlockHeaderHash::from_bytes(bytes32(
                &reward["from_stacks_block_hash"],
            )),
            from_index_consensus_hash: StacksBlockId::from_bytes(bytes32(
                &reward["from_index_consensus_hash"],
            )),
        })
        .collect()
}

fn reward_set(event: &Value) -> Option<RewardSetEvent> {
    let set = event["reward_set"].as_object()?;
    Some(RewardSetEvent {
        cycle_number: event["cycle_number"].as_u64().expect("cycle number"),
        signers: set["signers"]
            .as_array()
            .expect("signers")
            .iter()
            .map(|signer| RewardSetSigner {
                signing_key: hex::decode(signer["signing_key"].as_str().expect("signing key"))
                    .expect("hexadecimal signing key")
                    .try_into()
                    .expect("33-byte signing key"),
                stacked_amount: amount(&signer["stacked_amt"]),
                weight: u32::try_from(signer["weight"].as_u64().expect("weight")).expect("weight"),
            })
            .collect(),
        pox_ustx_threshold: set["pox_ustx_threshold"].as_str().map(|threshold| {
            threshold
                .parse()
                .expect("decimal proof-of-transfer threshold")
        }),
    })
}

fn context(event: &Value, parent_block_hash: BlockHeaderHash, tenure_height: u64) -> BlockEventContext {
    let height = |name: &str| u32::try_from(event[name].as_u64().expect(name)).expect(name);
    BlockEventContext {
        parent_block_hash,
        bitcoin_block_hash: BitcoinHeaderHash::from_bytes(bytes32(&event["burn_block_hash"])),
        bitcoin_height: event["burn_block_height"].as_u64().expect("burn height"),
        bitcoin_timestamp: event["burn_block_time"].as_u64().expect("burn time"),
        parent_bitcoin_block_hash: BitcoinHeaderHash::from_bytes(bytes32(
            &event["parent_burn_block_hash"],
        )),
        parent_bitcoin_height: event["parent_burn_block_height"]
            .as_u64()
            .expect("parent burn height"),
        parent_bitcoin_timestamp: event["parent_burn_block_timestamp"]
            .as_u64()
            .expect("parent burn time"),
        miner_txid: Sha256Sum::from_bytes(bytes32(&event["miner_txid"])),
        tenure_height,
        v1_unlock_height: height("pox_v1_unlock_height"),
        v2_unlock_height: height("pox_v2_unlock_height"),
        v3_unlock_height: height("pox_v3_unlock_height"),
        pox_5_activation_height: height("pox_v4_unlock_height"),
        matured_rewards: matured_rewards(event),
        reward_set: reward_set(event),
    }
}

/// Whether a payload still reports what a block and its transactions cost.
///
/// The dimensions themselves are the scoreboard's `replay: costs` row, which
/// is a known-open divergence in the VM; this only holds the payload to
/// carrying them.
fn costs_are_reported(payload: &Value) -> bool {
    let dimensions = |cost: &Value| {
        ["read_count", "read_length", "runtime", "write_count", "write_length"]
            .iter()
            .all(|dimension| cost.get(dimension).is_some_and(Value::is_u64))
    };
    dimensions(&payload["anchored_cost"])
        && payload["transactions"]
            .as_array()
            .expect("transactions")
            .iter()
            .all(|transaction| dimensions(&transaction["execution_cost"]))
}

/// Put a payload in the shape both nodes agree on.
///
/// stacks-core builds its event list out of a `HashSet` of event indices, so
/// the order it publishes them in is not the order it numbered them; the
/// index is the meaning. The cost dimensions come out because they are the
/// scoreboard's own row.
fn normalize(payload: &mut Value) {
    let object = payload.as_object_mut().expect("payload object");
    object.remove("anchored_cost");
    if let Some(events) = object.get_mut("events").and_then(Value::as_array_mut) {
        events.sort_by_key(|event| event["event_index"].as_u64());
    }
    if let Some(transactions) = object.get_mut("transactions").and_then(Value::as_array_mut) {
        for transaction in transactions {
            transaction
                .as_object_mut()
                .expect("transaction object")
                .remove("execution_cost");
        }
    }
}

#[test]
fn new_block_payloads_match_the_ones_stacks_core_published() {
    let root = fixtures();
    let mut parent: Option<BlockHeaderHash> = None;
    let mut tenure_height = 0;
    let mut divergences: Vec<String> = Vec::new();
    let mut transactions = 0;
    let mut events = 0;

    let compare = |block: &NakamotoBlock, applied: &AppliedBlock| {
        let event = captured_event(&root, block);
        // Both of these nano derives itself once replay is under way; the
        // capture only seeds the block replay starts from.
        let parent_block_hash = parent.unwrap_or_else(|| {
            BlockHeaderHash::from_bytes(bytes32(&event["parent_block_hash"]))
        });
        tenure_height = match parent {
            None => event["tenure_height"].as_u64().expect("tenure height"),
            Some(_) if starts_new_tenure(block) => tenure_height + 1,
            Some(_) => tenure_height,
        };

        let mut payload =
            new_block_payload(block, applied, &context(&event, parent_block_hash, tenure_height));
        transactions += applied.receipts.len();
        events += payload["events"].as_array().map_or(0, Vec::len);
        assert!(
            costs_are_reported(&payload),
            "block {} reports no execution costs",
            block.header.chain_length
        );

        let mut expected = event;
        normalize(&mut payload);
        normalize(&mut expected);
        for key in expected.as_object().expect("event object").keys() {
            if payload.get(key) != expected.get(key) {
                divergences.push(format!(
                    "block {}: {key} differs:\n  stacks-core: {}\n  nano:        {}",
                    block.header.chain_length,
                    expected[key],
                    payload.get(key).unwrap_or(&Value::Null)
                ));
            }
        }
        parent = Some(block.header.block_hash());
    };

    let depth = replay_captured_blocks(&root, REPLAY_BLOCKS, &mut { compare });

    assert_eq!(
        depth.completed, REPLAY_BLOCKS,
        "replay stopped early: {:?}",
        depth.first_divergence
    );
    assert!(divergences.is_empty(), "{}", divergences.join("\n"));
    // A payload stream with nothing in it would agree with anything.
    assert!(transactions > 0, "no receipts were compared");
    assert!(events > 0, "no Clarity events were compared");
}
