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
/// How many blocks the capture in the tree holds, read from its own manifest
/// so a recapture with a different window needs no change here.
fn replay_blocks(root: &std::path::Path) -> u64 {
    nano_conformance::FixtureManifest::load(&root.join("manifest.toml"))
        .expect("fixture manifest")
        .replay_blocks
}

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

    let blocks = replay_blocks(&root);
    let depth = replay_captured_blocks(&root, blocks, &mut { compare });

    assert_eq!(
        depth.completed, blocks,
        "replay stopped early: {:?}",
        depth.first_divergence
    );
    assert!(divergences.is_empty(), "{}", divergences.join("\n"));
    // A payload stream with nothing in it would agree with anything.
    assert!(transactions > 0, "no receipts were compared");
    assert!(events > 0, "no Clarity events were compared");
}

/// nano's `proposal_response` payloads against stacks-core's own type.
///
/// A signer branches on this payload — it decides whether to sign — and it
/// decides by deserializing it into `BlockValidateResponse`. So the check is not
/// that the fields look right but that stacks-core's own reader accepts them and
/// reads back what nano meant: the tag, the hash, the cost, and the reject code
/// out of the thirteen it has names for.
///
/// The cheapest oracle in the ladder, and the one that matters most here: the
/// shape is hand-written on nano's side, and a `result` tag or a `reason_code`
/// spelling that stacks-core cannot parse is a signer that ignores the answer.
#[test]
fn a_proposal_verdict_is_read_back_by_stacks_cores_own_reader() {
    use blockstack_lib::net::api::postblock_proposal::{BlockValidateResponse, ValidateRejectCode};

    let hash = Sha256Sum::from_bytes([7; 32]);
    let accepted = nano_rpc::proposal_response_payload(
        hash,
        &nano_rpc::ProposalOutcome::Accepted {
            cost: clarity::vm::costs::ExecutionCost {
                write_length: 1,
                write_count: 2,
                read_length: 3,
                read_count: 4,
                runtime: 5,
            },
            size: 4_096,
            validation_time_ms: 12,
        },
    );
    match serde_json::from_value(accepted).expect("stacks-core reads nano's acceptance") {
        BlockValidateResponse::Ok(ok) => {
            assert_eq!(ok.signer_signature_hash.to_hex(), hash.to_string());
            assert_eq!(ok.size, 4_096);
            assert_eq!(ok.validation_time_ms, 12);
            assert_eq!(ok.cost.runtime, 5);
            assert_eq!(ok.cost.read_count, 4);
            assert!(!ok.replay_tx_exhausted);
            assert_eq!(ok.replay_tx_hash, None);
        }
        BlockValidateResponse::Reject(reject) => panic!("read as a rejection: {reject:?}"),
    }

    // Every code nano can answer with, against the names stacks-core knows. A
    // code it cannot parse would make the whole verdict unreadable, so this
    // walks all of them rather than the one the routes happen to use today.
    for (code, expected) in [
        (nano_rpc::ProposalRejectCode::BadBlockHash, ValidateRejectCode::BadBlockHash),
        (nano_rpc::ProposalRejectCode::BadTransaction, ValidateRejectCode::BadTransaction),
        (nano_rpc::ProposalRejectCode::InvalidBlock, ValidateRejectCode::InvalidBlock),
        (nano_rpc::ProposalRejectCode::ChainstateError, ValidateRejectCode::ChainstateError),
        (nano_rpc::ProposalRejectCode::UnknownParent, ValidateRejectCode::UnknownParent),
        (
            nano_rpc::ProposalRejectCode::NonCanonicalTenure,
            ValidateRejectCode::NonCanonicalTenure,
        ),
        (nano_rpc::ProposalRejectCode::NoSuchTenure, ValidateRejectCode::NoSuchTenure),
        (
            nano_rpc::ProposalRejectCode::InvalidTransactionReplay,
            ValidateRejectCode::InvalidTransactionReplay,
        ),
        (
            nano_rpc::ProposalRejectCode::InvalidParentBlock,
            ValidateRejectCode::InvalidParentBlock,
        ),
        (nano_rpc::ProposalRejectCode::InvalidTimestamp, ValidateRejectCode::InvalidTimestamp),
        (
            nano_rpc::ProposalRejectCode::NetworkChainMismatch,
            ValidateRejectCode::NetworkChainMismatch,
        ),
        (nano_rpc::ProposalRejectCode::NotFoundError, ValidateRejectCode::NotFoundError),
        (
            nano_rpc::ProposalRejectCode::ProblematicTransaction,
            ValidateRejectCode::ProblematicTransaction,
        ),
    ] {
        let payload = nano_rpc::proposal_response_payload(
            hash,
            &nano_rpc::ProposalOutcome::Rejected {
                reason: "because".to_owned(),
                code,
            },
        );
        match serde_json::from_value(payload).expect("stacks-core reads nano's rejection") {
            BlockValidateResponse::Reject(reject) => {
                assert_eq!(reject.reason_code, expected, "{}", code.name());
                assert_eq!(reject.reason, "because");
                assert_eq!(reject.signer_signature_hash.to_hex(), hash.to_string());
                assert_eq!(reject.failed_txid, None);
            }
            BlockValidateResponse::Ok(ok) => panic!("read as an acceptance: {ok:?}"),
        }
    }
}

/// The reward set nano serves against the one stacks-core's signer reads.
///
/// `/v3/stacker_set` is how a signer learns its own weight and how a node learns
/// whose signatures to count, and the document is hand-written here too. So the
/// check is that stacks-core's own `RewardSet` reader takes it.
#[test]
fn a_served_reward_set_is_read_back_by_stacks_cores_own_reader() {
    use blockstack_lib::chainstate::stacks::boot::RewardSet;

    let signers: Vec<RewardSetSigner> = (1..=3_u8)
        .map(|seed| RewardSetSigner {
            signing_key: nano_crypto::StacksPrivateKey::from_seed(&[seed])
                .public_key()
                .to_bytes_compressed(),
            stacked_amount: u128::from(seed) * 1_000_000_000,
            weight: u32::from(seed),
        })
        .collect();
    let document = nano_rpc::stacker_set_payload(&signers, 50_000_000_000);

    let read: RewardSet = serde_json::from_value(document).expect("stacks-core reads the set");
    let entries = read.signers().expect("the set names its signers").clone();
    assert_eq!(entries.len(), 3);
    assert_eq!(read.pox_ustx_threshold(), Some(50_000_000_000));
    for (entry, signer) in entries.iter().zip(&signers) {
        assert_eq!(entry.signing_key, signer.signing_key);
        assert_eq!(entry.weight, signer.weight);
        assert_eq!(entry.stacked_amt, signer.stacked_amount);
    }
}

/// The three payloads a hosted signer's event listener reads, against its reader.
///
/// A stock signer's listener is not tolerant: it deserializes each event into a
/// stackslib type and drops the whole event when a field is the wrong shape, so
/// a payload that is merely close is a payload that never arrives. This is the
/// cheapest oracle for the ones nano writes by hand, and it found a real defect —
/// `stackerdb_chunks` named its contract with the `address.name` string the route
/// is keyed by, where the reader wants Clarity's `QualifiedContractIdentifier`.
#[test]
fn the_events_a_signer_listens_for_are_read_back_by_stacks_cores_own_readers() {
    use blockstack_lib::chainstate::stacks::events::{BurnBlockEvent, StackerDBChunksEvent};
    use nano_crypto::StacksPrivateKey;
    use nano_stackerdb::Chunk;

    let key = StacksPrivateKey::from_seed(b"writer");
    let mut chunk = Chunk::new(1, 7, b"a response".to_vec());
    chunk.sign(&key).expect("sign the chunk");
    let payload = nano_rpc::stackerdb_chunks_payload(
        "ST000000000000000000002AMW42H.signers-0-1",
        std::slice::from_ref(&chunk),
    );
    let read: StackerDBChunksEvent =
        serde_json::from_value(payload).expect("stacks-core reads nano's chunk event");
    assert_eq!(read.contract_id.name.to_string(), "signers-0-1");
    assert_eq!(read.modified_slots.len(), 1);
    assert_eq!(read.modified_slots[0].slot_id, 1);
    assert_eq!(read.modified_slots[0].slot_version, 7);
    assert_eq!(read.modified_slots[0].data, b"a response".to_vec());

    let burn = nano_rpc::new_burn_block_payload(
        BitcoinHeaderHash::from_bytes([3; 32]),
        900_000,
        nano_primitives::ConsensusHash::from_bytes([4; 20]),
        BitcoinHeaderHash::from_bytes([5; 32]),
        1_500,
    );
    let read: BurnBlockEvent =
        serde_json::from_value(burn).expect("stacks-core reads nano's burn block event");
    assert_eq!(read.burn_block_height, 900_000);
    assert_eq!(read.burn_amount, 1_500);
}
