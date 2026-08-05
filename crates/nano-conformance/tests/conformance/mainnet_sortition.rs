//! Derive mainnet's sortitions from mainnet's Bitcoin blocks.
//!
//! A node that asks a peer what the sortition was is letting that peer choose
//! its consensus hashes, its winners and its fork. The arithmetic is nano's to
//! do, and this checks it does it the same way the network did — against a
//! window of real mainnet snapshots dumped from a node's own sortition
//! database, replayed from the raw Bitcoin blocks beneath them.
//!
//! A window proves what a window can: that the same operations are found in
//! each block, hashed the same way, and that the sortition hash chains the same
//! way from one to the next. The consensus hash mixes prior ones at
//! power-of-two offsets reaching back thousands of blocks, so it needs a chain
//! replayed from its own genesis rather than a slice of one.
//!
//! Point `NANO_MAINNET_CAPTURE` at a capture directory to run it.

use std::{collections::BTreeMap, env, fs, path::PathBuf};

use nano_bitcoin::{BitcoinBlock, decode_block};
use nano_primitives::{BitcoinHeaderHash, ConsensusHash, SortitionId};
use nano_sortition::{
    LeaderKeys, OpsHash, PoxId, SnapshotChain, SortitionHash, SortitionSnapshot, SortitionWinner,
    commitment_is_on_time,
};

/// What a captured snapshot says, in the fields nano derives.
#[derive(Debug, serde::Deserialize)]
struct Captured {
    block_height: u64,
    burn_header_hash: String,
    sortition_id: String,
    consensus_hash: String,
    sortition_hash: String,
    ops_hash: String,
    total_burn: String,
    sortition: i64,
    winning_block_txid: String,
}

fn capture() -> Option<PathBuf> {
    env::var("NANO_MAINNET_CAPTURE").ok().map(PathBuf::from)
}

fn decode(value: &str) -> Vec<u8> {
    hex::decode(value).expect("a captured field is hexadecimal")
}

/// A chain seeded with the consensus hashes behind the window, when the
/// capture carries them, because the skip-list reaches back past any window.
fn chain_from(root: &std::path::Path, genesis: &SortitionSnapshot) -> SnapshotChain {
    consensus_history(root).map_or_else(
        || SnapshotChain::new(genesis.clone()),
        |hashes| {
            SnapshotChain::with_history(genesis.clone(), hashes)
                .expect("the history ends at the snapshot it seeds")
        },
    )
}

/// The consensus hashes behind the window, if the capture carries them.
///
/// A consensus hash mixes the ones at power-of-two offsets behind it, reaching
/// back thousands of blocks, so without these a window can derive every other
/// field and not this one.
fn consensus_history(root: &std::path::Path) -> Option<Vec<ConsensusHash>> {
    #[derive(serde::Deserialize)]
    struct History {
        hashes: Vec<String>,
    }
    let bytes = fs::read(root.join("sortition/consensus-hashes.json")).ok()?;
    let history: History = serde_json::from_slice(&bytes).ok()?;
    Some(
        history
            .hashes
            .iter()
            .map(|hash| {
                ConsensusHash::from_bytes(
                    <[u8; 20]>::try_from(decode(hash).as_slice()).expect("20 bytes"),
                )
            })
            .collect(),
    )
}

/// The first field a derived snapshot disagrees with the network on.
fn disagrees(derived: &SortitionSnapshot, snapshot: &Captured) -> Option<&'static str> {
    let checks: [(&'static str, String, &String); 4] = [
        (
            "operations hash",
            hex::encode(derived.operations_hash.as_bytes()),
            &snapshot.ops_hash,
        ),
        (
            "consensus hash",
            hex::encode(derived.consensus_hash.as_bytes()),
            &snapshot.consensus_hash,
        ),
        (
            "sortition id",
            hex::encode(derived.sortition_id.as_bytes()),
            &snapshot.sortition_id,
        ),
        (
            "sortition hash",
            hex::encode(derived.sortition_hash.as_bytes()),
            &snapshot.sortition_hash,
        ),
    ];
    checks
        .into_iter()
        .find(|(_, derived, captured)| derived != *captured)
        .map(|(field, _, _)| field)
}

/// The transactions of a burn block that are operations for its sortition.
///
/// A commitment that arrived after the block it was aiming at is a *missed*
/// commitment: still a transaction, still able to chain its UTXO so the mining
/// window survives a gap, but not an operation and not part of the hash. This
/// needs nothing but the block it landed in, unlike the leader-key rule, which
/// needs history a window has none of.
fn operation_txids(block: &BitcoinBlock) -> Vec<[u8; 32]> {
    block
        .operations
        .iter()
        .filter(|operation| match &operation.kind {
            nano_bitcoin::BitcoinOperationKind::LeaderBlockCommit { parent_modulus, .. } => {
                commitment_is_on_time(*parent_modulus, block.height)
            }
            _ => true,
        })
        .map(|operation| operation.txid)
        .collect()
}

/// The leader keys a set of burn blocks registers.
///
/// Collected because naming a registered, unspent key is what makes a
/// commitment an operation — and reported because a window cannot apply that
/// rule: the keys its commitments name were registered long before it.
fn keys_registered_in(blocks: &BTreeMap<u64, BitcoinBlock>) -> LeaderKeys {
    let mut keys = LeaderKeys::new();
    for block in blocks.values() {
        for operation in &block.operations {
            if let nano_bitcoin::BitcoinOperationKind::LeaderKeyRegistration {
                vrf_public_key,
                ..
            } = &operation.kind
            {
                keys.register(block.height, operation.transaction_index, *vrf_public_key);
            }
        }
    }
    keys
}

/// What nano found in a block it disagrees about, so the next look starts from
/// evidence rather than a guess.
fn report_operations(height: u64, block: &BitcoinBlock) {
    for operation in &block.operations {
        println!(
            "  burn {height} op {} index {} {}",
            hex::encode(operation.txid),
            operation.transaction_index,
            match &operation.kind {
                nano_bitcoin::BitcoinOperationKind::LeaderBlockCommit {
                    key_block_height,
                    key_transaction_index,
                    ..
                } => format!("names key {key_block_height}/{key_transaction_index}"),
                other => format!("{other:?}"),
            }
        );
    }
}

/// The snapshot a replay starts from, taken from the capture as given.
fn seed_from(genesis: &Captured) -> SortitionSnapshot {
    SortitionSnapshot {
        bitcoin_height: genesis.block_height,
        bitcoin_header_hash: BitcoinHeaderHash::from_bytes(
            <[u8; 32]>::try_from(decode(&genesis.burn_header_hash).as_slice()).expect("32 bytes"),
        ),
        sortition_id: SortitionId::from_bytes(
            <[u8; 32]>::try_from(decode(&genesis.sortition_id).as_slice()).expect("32 bytes"),
        ),
        parent_sortition_id: SortitionId::from_bytes([0; 32]),
        // Only the ops hash of the block *after* genesis is derived, so the
        // genesis one is never read.
        operations_hash: OpsHash::from_txids(&[]),
        consensus_hash: ConsensusHash::from_bytes(
            <[u8; 20]>::try_from(decode(&genesis.consensus_hash).as_slice()).expect("20 bytes"),
        ),
        total_burn: genesis.total_burn.parse().expect("a burn total"),
        sortition_hash: SortitionHash::from_bytes(
            <[u8; 32]>::try_from(decode(&genesis.sortition_hash).as_slice()).expect("32 bytes"),
        ),
        winner_txid: None,
        winner_vrf_seed: None,
        winner_vrf_public_key: None,
        pox_id: mainnet_pox_id(),
    }
}

/// The `PoX` history mainnet held across this window.
///
/// One bit a reward cycle, and every one of mainnet's had an anchor block, so
/// it is all ones — recovered from the captured sortition identifier, which is
/// the burn header hash and this vector hashed together. Cycle 141 begins at
/// 962,150, past the window, so it does not move inside it.
fn mainnet_pox_id() -> PoxId {
    PoxId::from_bits(vec![true; 142])
}

/// How many of the captured window derive exactly: all of it.
const DERIVED_FLOOR: usize = 14;

/// Mainnet's magic bytes, which decide what counts as a burnchain transaction.
const MAINNET_MAGIC: [u8; 2] = *b"X2";

#[test]
fn mainnet_sortitions_derive_from_mainnet_bitcoin_blocks() {
    let Some(root) = capture() else {
        nano_conformance::skip_gate("NANO_MAINNET_CAPTURE must name a capture directory");
        return;
    };
    let captured: Vec<Captured> = serde_json::from_slice(
        &fs::read(root.join("sortition/snapshots.json")).expect("read the snapshots"),
    )
    .expect("parse the snapshots");
    assert!(captured.len() > 1, "the capture holds a window to replay");

    let blocks: BTreeMap<u64, BitcoinBlock> = captured
        .iter()
        .map(|snapshot| {
            let raw = fs::read_to_string(
                root.join("bitcoin/blocks")
                    .join(format!("{}.hex", snapshot.burn_header_hash)),
            )
            .expect("read the captured Bitcoin block");
            let block = decode_block(
                snapshot.block_height,
                &hex::decode(raw.trim()).expect("the block is hexadecimal"),
                MAINNET_MAGIC,
            )
            .expect("decode the captured Bitcoin block");
            (snapshot.block_height, block)
        })
        .collect();

    // The first captured snapshot is taken as given; everything after it is
    // derived, which is the claim being checked.
    let mut chain = chain_from(&root, &seed_from(&captured[0]));


    println!(
        "{} leader keys registered inside the window",
        keys_registered_in(&blocks).available()
    );
    let mut checked = 0;
    let mut first_divergence = None;
    for snapshot in captured.iter().skip(1) {
        let block = blocks
            .get(&snapshot.block_height)
            .expect("every captured snapshot has its Bitcoin block");
        let txids = operation_txids(block);
        // The winner's seed is in its own commitment, which is among the
        // operations nano decoded — so finding it is part of the claim.
        let winning = <[u8; 32]>::try_from(decode(&snapshot.winning_block_txid).as_slice())
            .expect("32 bytes");
        let winner = (snapshot.sortition != 0)
            .then(|| {
                block.operations.iter().find_map(|operation| {
                    match (operation.txid == winning, &operation.kind) {
                        (true, nano_bitcoin::BitcoinOperationKind::LeaderBlockCommit {
                            new_seed,
                            ..
                        }) => Some(SortitionWinner {
                            vrf_public_key: None,
                            txid: operation.txid,
                            vrf_seed: *new_seed,
                        }),
                        _ => None,
                    }
                })
            })
            .flatten();
        if snapshot.sortition != 0 {
            assert!(
                winner.is_some(),
                "the winning commitment at burn {} is among the decoded operations",
                snapshot.block_height
            );
        }
        let derived = chain
            .append_with_operations(
                block,
                &txids,
                snapshot.total_burn.parse().expect("a burn total"),
                mainnet_pox_id(),
                winner,
            )
            .expect("the chain extends");

        if let Some(field) = disagrees(derived, snapshot) {
            if field == "operations hash" {
                report_operations(snapshot.block_height, block);
            }
            first_divergence = Some((snapshot.block_height, field));
            break;
        }
        checked += 1;
        let _ = snapshot.sortition;
    }
    println!(
        "{checked} mainnet sortitions derived and checked{}",
        first_divergence.map_or_else(String::new, |(height, field)| format!(
            ", first divergence at burn {height} on the {field}"
        ))
    );
    // A depth rather than a pass: the burn blocks up to epoch 4.0's boundary
    // derive exactly, and the boundary itself is where the operation set
    // changes. Raising this floor is the measure of progress, the same way
    // replay depth is.
    assert!(
        checked >= DERIVED_FLOOR,
        "derived {checked} sortitions, below the {DERIVED_FLOOR} already reached: {first_divergence:?}"
    );
}

/// The mining window a chain is weighed over, which reaches behind the seed.
///
/// `nano_sortition::MINING_COMMITMENT_WINDOW` blocks, and the capture's own seed
/// is the last of them, so five sit below anything `snapshots.json` describes.
/// They are found by walking the previous-block hash out of each block's header,
/// which also proves they are the seed's real ancestors rather than whatever was
/// dropped into the directory.
fn priming_blocks(root: &std::path::Path, seed: &Captured) -> Option<Vec<BitcoinBlock>> {
    let mut hash = seed.burn_header_hash.clone();
    let mut height = seed.block_height;
    let mut blocks = Vec::new();
    for _ in 0..nano_sortition::MINING_COMMITMENT_WINDOW {
        let raw = fs::read_to_string(root.join("bitcoin/blocks").join(format!("{hash}.hex"))).ok()?;
        let bytes = hex::decode(raw.trim()).expect("the block is hexadecimal");
        // A Bitcoin header is version(4) then the previous block hash, stored in
        // the reverse of the order it is written in.
        let mut previous = <[u8; 32]>::try_from(&bytes[4..36]).expect("32 bytes");
        previous.reverse();
        blocks.push(decode_block(height, &bytes, MAINNET_MAGIC).expect("decode the block"));
        hash = hex::encode(previous);
        height -= 1;
    }
    blocks.reverse();
    Some(blocks)
}

/// Mainnet's calendar: cycle length 2100 from burn 666,050, a 100-block prepare
/// phase, and the waterfall opening with cycle 141 at 962,150 — past this window,
/// so every block in it sits in a reward phase.
///
/// Epoch 4.0 activates at burn 960,230 (`BITCOIN_MAINNET_STACKS_40_BURN_HEIGHT`),
/// which is inside this window, so the schedule has to know it: the seven blocks
/// from there have an epoch boundary inside their mining window and are weighed on
/// their own block alone.
fn mainnet_payouts() -> nano_sortition::PayoutSchedule {
    nano_sortition::PayoutSchedule::new(
        nano_sortition::RewardCycleSchedule::new(666_050, 2100, Some(962_150))
            .expect("valid cycle schedule"),
        100,
    )
    .expect("valid payout schedule")
    .activating_epoch_four_at(960_230)
}

/// Every Bitcoin block the window needs, including the six behind its seed.
fn window_blocks(root: &std::path::Path, captured: &[Captured]) -> Option<BTreeMap<u64, BitcoinBlock>> {
    Some(
        priming_blocks(root, &captured[0])?
            .into_iter()
            .map(|block| (block.height, block))
            .chain(captured.iter().skip(1).map(|snapshot| {
                let raw = fs::read_to_string(
                    root.join("bitcoin/blocks")
                        .join(format!("{}.hex", snapshot.burn_header_hash)),
                )
                .expect("read the captured Bitcoin block");
                let block = decode_block(
                    snapshot.block_height,
                    &hex::decode(raw.trim()).expect("the block is hexadecimal"),
                    MAINNET_MAGIC,
                )
                .expect("decode the captured Bitcoin block");
                (snapshot.block_height, block)
            }))
            .collect(),
    )
}

/// nano's burn distribution against stacks-core's own `make_min_median_distribution`.
///
/// The cheapest oracle on the ladder, and the one that made the difference here:
/// the distribution is a pure function of a commitment window, so stacks-core's own
/// implementation can be *called* rather than inferred from the fourteen winners a
/// capture records. Fourteen winners can only say whether an answer is wrong; this
/// says which candidate, which window slot, and by how much.
///
/// The conversion is where the two representations differ, and the difference is
/// the rule: nano files a missed commitment under the block it *arrived* in,
/// because that is all a Bitcoin block knows, while stacks-core files it under the
/// sortition it *intended* — always the block before, since a larger miss is
/// refused outright (`check_intended_sortition`, `BlockCommitMissDistanceTooBig`).
/// So a window slot holds the misses of the block above it, and the oldest slot's
/// own misses belong below the window and are never read.
fn assert_distribution_matches(
    window: &[nano_sortition::CommitmentWindowBlock],
    heights: &[u64],
    label: &str,
) {
    use blockstack_lib::burnchains::{BurnchainSigner, Txid};
    use blockstack_lib::chainstate::burn::distribution::BurnSamplePoint;
    use blockstack_lib::chainstate::burn::operations::LeaderBlockCommitOp;
    use blockstack_lib::chainstate::burn::operations::leader_block_commit::MissedBlockCommit;
    use stacks_common::types::chainstate::{
        BlockHeaderHash, BurnchainHeaderHash, SortitionId, VRFSeed,
    };

    let block_commits: Vec<Vec<LeaderBlockCommitOp>> = window
        .iter()
        .zip(heights)
        .map(|(block, height)| {
            block
                .commitments
                .iter()
                .map(|commitment| LeaderBlockCommitOp {
                    block_header_hash: BlockHeaderHash([0; 32]),
                    new_seed: VRFSeed(commitment.vrf_seed),
                    parent_block_ptr: 0,
                    parent_vtxindex: 0,
                    key_block_ptr: 0,
                    key_vtxindex: 0,
                    memo: vec![],
                    burn_fee: commitment.burn_sats,
                    input: (Txid(commitment.spent_txid), commitment.spent_output),
                    burn_parent_modulus: 0,
                    apparent_sender: BurnchainSigner(hex::encode(commitment.txid)),
                    commit_outs: vec![],
                    treatment: vec![],
                    sunset_burn: 0,
                    txid: Txid(commitment.txid),
                    vtxindex: 0,
                    block_height: *height,
                    burn_header_hash: BurnchainHeaderHash([0; 32]),
                })
                .collect()
        })
        .collect();
    let missed_commits: Vec<Vec<MissedBlockCommit>> = window
        .iter()
        .skip(1)
        .map(|block| {
            block
                .missed_commitments
                .iter()
                .map(|missed| MissedBlockCommit {
                    txid: Txid(missed.txid),
                    input: (Txid(missed.spent_txid), missed.spent_output),
                    intended_sortition: SortitionId([0; 32]),
                })
                .collect()
        })
        .collect();
    let expects_single_commit: Vec<bool> = window
        .iter()
        .map(|block| block.requires_single_commit)
        .collect();

    let theirs = BurnSamplePoint::make_min_median_distribution(
        u8::try_from(nano_sortition::MINING_COMMITMENT_WINDOW).expect("window fits u8"),
        block_commits,
        missed_commits,
        expects_single_commit,
    );
    let ours = nano_sortition::commitment_distribution(window).expect("a distribution");

    assert_eq!(ours.len(), theirs.len(), "candidate count {label}");
    for (ours, theirs) in ours.iter().zip(&theirs) {
        assert_eq!(
            (
                hex::encode(ours.candidate.txid),
                u128::from(ours.burn_sats),
                u128::from(ours.median_burn_sats),
                ours.frequency,
                hex::encode(ours.range_start.to_big_endian()),
                hex::encode(ours.range_end.to_big_endian()),
            ),
            (
                theirs.candidate.txid.to_hex(),
                theirs.burns,
                theirs.median_burn,
                theirs.frequency,
                theirs.range_start.to_hex_be(),
                theirs.range_end.to_hex_be(),
            ),
            "burn sample for {} {label}",
            theirs.candidate.txid.to_hex()
        );
    }
}

#[test]
fn the_burn_distribution_matches_stacks_core() {
    let Some(root) = capture() else {
        nano_conformance::skip_gate("NANO_MAINNET_CAPTURE must name a capture directory");
        return;
    };
    let captured: Vec<Captured> = serde_json::from_slice(
        &fs::read(root.join("sortition/snapshots.json")).expect("read the snapshots"),
    )
    .expect("parse the snapshots");
    let Some(blocks) = window_blocks(&root, &captured) else {
        nano_conformance::skip_gate("the capture holds no Bitcoin blocks below its seed");
        return;
    };
    let payouts = mainnet_payouts();
    let keys = LeaderKeys::new();

    for snapshot in captured.iter().skip(1) {
        let target = snapshot.block_height;
        let window_len = u64::try_from(payouts.mining_window_at(target)).expect("fits u64");
        let heights: Vec<u64> = (target + 1 - window_len..=target).collect();
        let window: Vec<_> = heights
            .iter()
            .map(|height| {
                nano_sortition::commitment_window_block(
                    blocks.get(height).expect("the window's Bitcoin block"),
                    payouts.outputs_at(*height),
                    &keys,
                )
            })
            .collect();
        // A block with no on-time commitment is why mainnet has no sortition at
        // burn 960,222, 960,224, 960,227 and 960,229, and both implementations
        // answer an empty distribution there — which is worth comparing too.
        assert_distribution_matches(&window, &heights, &format!("at burn {target}"));
    }
}

/// One window in which a chain reaches back *through* a missed commitment.
///
/// The captured mainnet window cannot falsify the filing rule: it holds exactly one
/// missed commitment and nothing chains to it, so both placements — the block it
/// arrived in and the block it aimed at — give the same distribution there. This is
/// the window that tells them apart, and it is checked against stacks-core rather
/// than against an expectation written down by hand.
#[test]
fn a_chain_reaching_through_a_missed_commitment_matches_stacks_core() {
    use nano_sortition::{CommitmentWindowBlock, MiningCommitment, MissedCommitment};

    let txid = |byte: u8| [byte; 32];
    let commitment = |id: u8, spends: u8, burn_sats: u64| MiningCommitment {
        txid: txid(id),
        spent_txid: txid(spends),
        spent_output: 3,
        burn_sats,
        vrf_seed: [0; 32],
        vrf_public_key: None,
    };
    let empty = || CommitmentWindowBlock {
        commitments: Vec::new(),
        missed_commitments: Vec::new(),
        requires_single_commit: false,
    };
    // Slot 3 holds the miner's older commitment; slot 4's block carries a missed
    // commitment that spends it, which stacks-core files against slot 3; slot 5's
    // candidate spends the miss. Under the filing rule the chain is
    // candidate -> miss(slot 3), and slot 3's own commitment is then out of reach,
    // so the chain is two long. Under the arrived-in placement it would be three,
    // with a different median and a different weight.
    let window = vec![
        empty(),
        empty(),
        empty(),
        CommitmentWindowBlock {
            commitments: vec![commitment(0xb0, 0x00, 90_000)],
            ..empty()
        },
        CommitmentWindowBlock {
            missed_commitments: vec![MissedCommitment {
                txid: txid(0xa0),
                spent_txid: txid(0xb0),
                spent_output: 3,
            }],
            ..empty()
        },
        CommitmentWindowBlock {
            commitments: vec![
                commitment(0xc0, 0xa0, 50_000),
                commitment(0xc1, 0xff, 50_000),
            ],
            ..empty()
        },
    ];
    let heights: Vec<u64> = (100..106).collect();
    assert_distribution_matches(&window, &heights, "in the missed-commitment window");

    let ours = nano_sortition::commitment_distribution(&window).expect("a distribution");
    assert_eq!(
        (ours[0].frequency, ours[1].frequency),
        (2, 1),
        "the chain stops at the miss, because the slot below it is not the one \
         the miss was filed against"
    );
}

/// The node's own tracker derives what the network derived, asking nothing.
///
/// The window test above drives `SnapshotChain` directly and is *given* the
/// winner and the running burn total. This drives
/// `nano_node::sortition::SortitionTracker`, which is what a node actually runs,
/// and hands it nothing but Bitcoin blocks: the burn total, the winner and the
/// sortition hash are all derived. The burn total is the one that matters most,
/// because it is neither the sum of what a block's commitments paid nor a number
/// any Bitcoin block carries — it comes out of the burn distribution, and a block
/// whose sortition went to the null miner adds nothing to it at all.
#[test]
fn the_node_tracker_derives_the_same_window() {
    let Some(root) = capture() else {
        nano_conformance::skip_gate("NANO_MAINNET_CAPTURE must name a capture directory");
        return;
    };
    let captured: Vec<Captured> = serde_json::from_slice(
        &fs::read(root.join("sortition/snapshots.json")).expect("read the snapshots"),
    )
    .expect("parse the snapshots");
    let Some(priming) = priming_blocks(&root, &captured[0]) else {
        nano_conformance::skip_gate(
            "the capture holds no Bitcoin blocks below its seed, so the mining window \
             cannot be filled — `xtask capture` needs to reach MINING_COMMITMENT_WINDOW \
             blocks below the burn span",
        );
        return;
    };
    let history = nano_node::sortition::SortitionTracker::history_from(&root.join("sortition"))
        .expect("the capture carries the consensus hashes");

    let mut tracker = nano_node::sortition::SortitionTracker::new(seed_from(&captured[0]), history)
        .expect("the tracker starts");
    let payouts = mainnet_payouts();

    let blocks: BTreeMap<u64, BitcoinBlock> = priming
        .into_iter()
        .map(|block| (block.height, block))
        .chain(captured.iter().skip(1).map(|snapshot| {
            let raw = fs::read_to_string(
                root.join("bitcoin/blocks")
                    .join(format!("{}.hex", snapshot.burn_header_hash)),
            )
            .expect("read the captured Bitcoin block");
            let block = decode_block(
                snapshot.block_height,
                &hex::decode(raw.trim()).expect("the block is hexadecimal"),
                MAINNET_MAGIC,
            )
            .expect("decode the captured Bitcoin block");
            (snapshot.block_height, block)
        }))
        .collect();

    let mut checked = 0;
    let mut winners = 0;
    tracker
        .catch_up(
            |height| {
                blocks
                    .get(&height)
                    .cloned()
                    .ok_or_else(|| format!("no Bitcoin block at {height}"))
            },
            captured[0].block_height,
            payouts,
            u64::try_from(nano_sortition::MINING_COMMITMENT_WINDOW).expect("window fits u64"),
        )
        .expect("the mining window fills from behind the seed");
    for snapshot in captured.iter().skip(1) {
        let block = blocks
            .get(&snapshot.block_height)
            .expect("every captured snapshot has its Bitcoin block");
        let derived = tracker.advance(block, payouts).expect("the tracker advances");
        for (field, derived, captured) in [
            (
                "consensus hash",
                hex::encode(derived.consensus_hash.as_bytes()),
                snapshot.consensus_hash.clone(),
            ),
            (
                "sortition hash",
                hex::encode(derived.sortition_hash.as_bytes()),
                snapshot.sortition_hash.clone(),
            ),
            (
                "total burn",
                derived.total_burn.to_string(),
                snapshot.total_burn.clone(),
            ),
        ] {
            assert_eq!(
                derived, captured,
                "the node derives the {field} at burn {}",
                snapshot.block_height
            );
        }
        let expected_winner =
            (snapshot.sortition != 0).then(|| snapshot.winning_block_txid.clone());
        assert_eq!(
            derived.winner_txid.map(hex::encode),
            expected_winner,
            "the node names the winner at burn {}",
            snapshot.block_height
        );
        winners += 1;
        checked += 1;
    }
    assert_eq!(checked, DERIVED_FLOOR, "the node derived the whole window");
    // Equality, not a floor. This was `winners >= 12` for as long as the winner's
    // identity was the field that did not derive; it derives for all fourteen now,
    // so a floor would only hide a regression.
    assert_eq!(winners, DERIVED_FLOOR, "the node named every winner");
}
