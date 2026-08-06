//! nano's `new_block` payloads against the ones stacks-core published.
//!
//! An event observer is how everything downstream of a node reads a chain, and
//! the captured `events/new_block` stream is exactly what stacks-core sent for
//! the blocks nano replays. Rebuilding those payloads from nano's own executed
//! receipts and diffing them is the strongest offline check there is that an
//! observer cannot tell the two nodes apart.
//!
//! What the capture still supplies as context is the burn block: the sortition
//! that elected the block, its identifier and time, and the parent's. Those come
//! from a burnchain and a sortition chain that a replay of block bytes does not
//! stand up — the node reads them from its own, and `mainnet_sortition` is where
//! that reading is checked.
//!
//! Everything else is nano's own, and two of them are the point of this file:
//! **the rewards a block matured** and **the reward set it computed** come out of
//! `AppliedBlock`, which is to say out of the tenure accounting and the pox-5
//! walk this node ran. So this compares nano's answer about who was paid what,
//! and who may sign the next cycle, against the answer stacks-core published for
//! the same 340 blocks.

use std::path::{Path, PathBuf};

use nano_chainstate::{AppliedBlock, NakamotoBlock, starts_new_tenure};
use nano_conformance::replay_captured_blocks;
use nano_primitives::{BitcoinHeaderHash, BlockHeaderHash, Sha256Sum};
use nano_rpc::{BlockEventContext, RewardSetEvent, RewardSetSigner, new_block_payload};
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

fn context(
    event: &Value,
    applied: &AppliedBlock,
    parent_block_hash: BlockHeaderHash,
    tenure_height: u64,
) -> BlockEventContext {
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
        // nano's own, out of the block it just executed: the credits its tenure
        // accounting matured, and the set its prepare-phase walk of pox-5 wrote.
        // No source for them: naming the tenure a payout matured *from* means
        // reading its start block back, and that tenure is a hundred tenures below
        // the block being replayed — outside this capture altogether, and below the
        // checkpoint it starts at. `a_matured_payout_names_the_tenure_that_earned_it`
        // is where the rule the node fills them by is checked instead.
        matured_rewards: nano_rpc::matured_rewards(&applied.matured_rewards, None),
        reward_set: applied
            .reward_set
            .as_ref()
            .map(RewardSetEvent::from_derived),
    }
}

/// Whether a payload still reports what a block and its transactions cost.
///
/// The dimensions themselves are the scoreboard's `replay: costs` row, which
/// is a known-open divergence in the VM; this only holds the payload to
/// carrying them.
fn costs_are_reported(payload: &Value) -> bool {
    let dimensions = |cost: &Value| {
        [
            "read_count",
            "read_length",
            "runtime",
            "write_count",
            "write_length",
        ]
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

/// The three fields of a matured reward that name the tenure it matured *from*.
///
/// A replaying node cannot fill them, and that is a property of the replay rather
/// than of the payload: the tenure a payout matured from is a hundred tenures
/// below the block being replayed, so it is below this capture's checkpoint and
/// there is no start block to read back. The node fills them from the blocks it
/// kept — `CheckpointExecutor::matured_reward_source` — which is a window this
/// replay does not have. Taken out of the comparison rather than
/// quietly compared as zeros, so that the amounts and the recipients either side
/// of them are compared for real.
const UNKNOWN_PROVENANCE: [&str; 3] = [
    "miner_address",
    "from_stacks_block_hash",
    "from_index_consensus_hash",
];

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
    if let Some(rewards) = object
        .get_mut("matured_miner_rewards")
        .and_then(Value::as_array_mut)
    {
        for reward in rewards {
            let reward = reward.as_object_mut().expect("a matured reward object");
            for field in UNKNOWN_PROVENANCE {
                reward.remove(field);
            }
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
    // The two payload fields nano now derives rather than reads. Counted because
    // a capture window without a maturing payout or a prepare phase in it would
    // compare neither and agree about both.
    let mut matured = 0;
    let mut reward_sets = 0;

    let compare = |block: &NakamotoBlock, applied: &AppliedBlock| {
        let event = captured_event(&root, block);
        // Both of these nano derives itself once replay is under way; the
        // capture only seeds the block replay starts from.
        let parent_block_hash = parent
            .unwrap_or_else(|| BlockHeaderHash::from_bytes(bytes32(&event["parent_block_hash"])));
        tenure_height = match parent {
            None => event["tenure_height"].as_u64().expect("tenure height"),
            Some(_) if starts_new_tenure(block) => tenure_height + 1,
            Some(_) => tenure_height,
        };

        let mut payload = new_block_payload(
            block,
            applied,
            &context(&event, applied, parent_block_hash, tenure_height),
        );
        matured += applied.matured_rewards.len();
        reward_sets += usize::from(applied.reward_set.is_some());
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
    assert!(matured > 0, "no matured miner reward was compared");
    assert!(reward_sets > 0, "no derived reward set was compared");
}

/// The payouts one captured event reports, in the order it reports them.
fn matured_rewards_of(event: &Value) -> &[Value] {
    event["matured_miner_rewards"]
        .as_array()
        .expect("matured rewards")
}

/// Every captured event that matured a payout, by the tenure it started.
fn maturing_events(root: &Path) -> Vec<(u64, Value)> {
    let mut events: Vec<(u64, Value)> = Vec::new();
    for entry in std::fs::read_dir(root.join("events/new_block")).expect("read the capture") {
        let path = entry.expect("a captured event").path();
        let event: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read it")).expect("decode it");
        if !matured_rewards_of(&event).is_empty() {
            events.push((
                event["tenure_height"].as_u64().expect("tenure height"),
                event,
            ));
        }
    }
    events.sort_by_key(|(tenure, _)| *tenure);
    events
}

/// Which tenure each matured payout names, read off stacks-core's own stream.
///
/// The three fields the replay above takes out are not arbitrary — they follow a
/// rule, and the rule is what the node fills them by: **both** payouts of one
/// event name the maturing tenure's start block, while `miner_address` is each
/// payout's own tenure's miner. The capture settles the rule without reaching a
/// hundred tenures back for it, because consecutive maturing events overlap — the
/// fees maturing with tenure N are the fees of the tenure whose coinbase matured
/// with N−1, so the miner they name is that event's coinbase miner.
#[test]
fn a_matured_payout_names_the_tenure_that_earned_it() {
    let events = maturing_events(&fixtures());
    assert!(
        events.len() > 1,
        "the capture matured no payouts to read a rule off"
    );

    let mut overlaps = 0;
    for (tenure, event) in &events {
        // One block for the whole event, whichever tenure each payout belongs to.
        let rewards = matured_rewards_of(event);
        assert_eq!(
            rewards.len(),
            2,
            "tenure {tenure} matured other than two payouts"
        );
        assert_eq!(
            rewards[0]["from_stacks_block_hash"], rewards[1]["from_stacks_block_hash"],
            "tenure {tenure} names two different blocks"
        );
        assert_eq!(
            rewards[0]["from_index_consensus_hash"], rewards[1]["from_index_consensus_hash"],
            "tenure {tenure} names two different block identifiers"
        );
        // The coinbase is the first entry and the fees the second, which is the
        // order nano's credits arrive in.
        assert_ne!(
            rewards[0]["coinbase_amount"], "0",
            "tenure {tenure} matured no coinbase"
        );
        assert_eq!(rewards[1]["coinbase_amount"], "0");

        // The fees are the previous tenure's, and so is the miner they are paid to.
        let Some((_, previous)) = events.iter().find(|(earlier, _)| *earlier + 1 == *tenure) else {
            continue;
        };
        let previous = matured_rewards_of(previous);
        assert_eq!(
            rewards[1]["miner_address"], previous[0]["miner_address"],
            "the fees maturing at tenure {tenure} name a miner other than the previous tenure's"
        );
        assert_eq!(rewards[1]["recipient"], previous[0]["recipient"]);
        overlaps += 1;
    }
    assert!(
        overlaps > 0,
        "no two consecutive maturing tenures were compared"
    );
}

/// nano's builder on that same rule, which is the half the capture cannot show.
///
/// The credits are what nano's tenure accounting produces, in its order; the
/// source is what the node reads back out of the blocks it kept. This says the two
/// are put together the way the stream above is written — and that a node without
/// the history leaves the miner plainly absent rather than reporting a zero
/// address, which would read as a real principal.
#[test]
fn a_matured_payout_is_built_from_the_tenures_that_earned_it() {
    let matured: Vec<nano_chainstate::NativeStxCredit> = [
        ("ST2FW15NGB4H76FMVXKHYYSM865YVS6V3SA1GNABC", 1_020_400_000),
        ("ST2MES40ZEXTX9M4YXW9QSWHRVC9HYT419S198VPM", 3_000_300),
    ]
    .into_iter()
    .map(|(recipient, amount)| nano_chainstate::NativeStxCredit {
        recipient: clarity::vm::types::PrincipalData::parse(recipient).expect("a principal"),
        amount,
    })
    .collect();
    let source = nano_rpc::MaturedRewardSource {
        from_stacks_block_hash: BlockHeaderHash::from_bytes([1; 32]),
        from_index_consensus_hash: nano_primitives::StacksBlockId::from_bytes([2; 32]),
        coinbase_miner: "ST22RBMZ4CMXYAVBED3KTMZEWRMA0ST6XSGBSX10H".to_owned(),
        fee_miner: "ST4DZ2J4VWYBEQC0319V7CN8JDYE2WMESPSWMGDE".to_owned(),
    };

    let built = nano_rpc::matured_rewards(&matured, Some(&source));
    let [coinbase, fees] = built.as_slice() else {
        panic!("two payouts were built, not {}", built.len())
    };
    assert_eq!(
        coinbase.recipient,
        "ST2FW15NGB4H76FMVXKHYYSM865YVS6V3SA1GNABC"
    );
    assert_eq!(coinbase.miner_address, source.coinbase_miner);
    assert_eq!(coinbase.coinbase, 1_020_400_000);
    assert_eq!(coinbase.tx_fees_streamed_produced, 0);
    assert_eq!(fees.recipient, "ST2MES40ZEXTX9M4YXW9QSWHRVC9HYT419S198VPM");
    assert_eq!(fees.miner_address, source.fee_miner);
    assert_eq!(fees.coinbase, 0);
    assert_eq!(fees.tx_fees_streamed_produced, 3_000_300);
    for reward in &built {
        assert_eq!(reward.from_stacks_block_hash, source.from_stacks_block_hash);
        assert_eq!(
            reward.from_index_consensus_hash,
            source.from_index_consensus_hash
        );
    }

    let without = nano_rpc::matured_rewards(&matured, None);
    assert!(without.iter().all(|reward| reward.miner_address.is_empty()));
    assert_eq!(without[0].coinbase, 1_020_400_000);
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
        (
            nano_rpc::ProposalRejectCode::BadBlockHash,
            ValidateRejectCode::BadBlockHash,
        ),
        (
            nano_rpc::ProposalRejectCode::BadTransaction,
            ValidateRejectCode::BadTransaction,
        ),
        (
            nano_rpc::ProposalRejectCode::InvalidBlock,
            ValidateRejectCode::InvalidBlock,
        ),
        (
            nano_rpc::ProposalRejectCode::ChainstateError,
            ValidateRejectCode::ChainstateError,
        ),
        (
            nano_rpc::ProposalRejectCode::UnknownParent,
            ValidateRejectCode::UnknownParent,
        ),
        (
            nano_rpc::ProposalRejectCode::NonCanonicalTenure,
            ValidateRejectCode::NonCanonicalTenure,
        ),
        (
            nano_rpc::ProposalRejectCode::NoSuchTenure,
            ValidateRejectCode::NoSuchTenure,
        ),
        (
            nano_rpc::ProposalRejectCode::InvalidTransactionReplay,
            ValidateRejectCode::InvalidTransactionReplay,
        ),
        (
            nano_rpc::ProposalRejectCode::InvalidParentBlock,
            ValidateRejectCode::InvalidParentBlock,
        ),
        (
            nano_rpc::ProposalRejectCode::InvalidTimestamp,
            ValidateRejectCode::InvalidTimestamp,
        ),
        (
            nano_rpc::ProposalRejectCode::NetworkChainMismatch,
            ValidateRejectCode::NetworkChainMismatch,
        ),
        (
            nano_rpc::ProposalRejectCode::NotFoundError,
            ValidateRejectCode::NotFoundError,
        ),
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
/// check is that stacks-core's own `RewardSet` reader takes it — and takes it as
/// the *shape* nano meant, which is the part a field-by-field eye cannot see:
/// the variant is chosen by a `reward_set_version` key read out of a flat object,
/// so a document that merely looks like a waterfall set is read as a V0 one and
/// its `sbtc_address` silently ignored.
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
    let payout = nano_address::PoxAddress::Addr32 {
        mainnet: false,
        address_type: nano_address::PoxAddressType32::P2tr,
        bytes: [9; 32],
    };
    let document = nano_rpc::stacker_set_payload(&signers, 50_000_000_000, Some(&payout));

    let read: RewardSet = serde_json::from_value(document).expect("stacks-core reads the set");
    let waterfall = match &read {
        RewardSet::Waterfall(set) => set,
        RewardSet::V0(set) => panic!("read as a version 0 set: {set:?}"),
    };
    assert_eq!(
        waterfall.sbtc_address,
        blockstack_lib::chainstate::stacks::address::PoxAddress::Addr32(
            false,
            blockstack_lib::chainstate::stacks::address::PoxAddressType32::P2TR,
            [9; 32],
        )
    );
    let entries = read.signers().expect("the set names its signers").clone();
    assert_eq!(entries.len(), 3);
    assert_eq!(read.pox_ustx_threshold(), Some(50_000_000_000));
    for (entry, signer) in entries.iter().zip(&signers) {
        assert_eq!(entry.signing_key, signer.signing_key);
        assert_eq!(entry.weight, signer.weight);
        assert_eq!(entry.stacked_amt, signer.stacked_amount);
    }

    // Without the payout address there is no waterfall document to serve, and the
    // version every reader accepts is served instead of a version 1 one that
    // would not deserialize at all.
    let v0 = nano_rpc::stacker_set_payload(&signers, 50_000_000_000, None);
    let read: RewardSet = serde_json::from_value(v0).expect("stacks-core reads the set");
    assert!(matches!(read, RewardSet::V0(_)), "{read:?}");
    assert_eq!(read.signers().map(Vec::len), Some(3));
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
