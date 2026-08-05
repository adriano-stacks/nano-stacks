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
    commit_lands_in_block,
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
                commit_lands_in_block(*parent_modulus, block.height)
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

/// The node's own tracker derives what the network derived.
///
/// The window test above drives `SnapshotChain` directly, which proves the
/// arithmetic. This drives `nano_node::sortition::SortitionTracker`, which is
/// what a node actually runs — the same blocks through the code path that will
/// replace asking a peer.
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
    let history = nano_node::sortition::SortitionTracker::history_from(&root.join("sortition"))
        .expect("the capture carries the consensus hashes");

    let mut tracker =
        nano_node::sortition::SortitionTracker::new(seed_from(&captured[0]), history)
            .expect("the tracker starts");

    let mut checked = 0;
    for snapshot in captured.iter().skip(1) {
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

        let derived = tracker
            .advance(&block, snapshot.total_burn.parse().expect("a burn total"))
            .expect("the tracker advances");
        assert_eq!(
            hex::encode(derived.consensus_hash.as_bytes()),
            snapshot.consensus_hash,
            "the node derives the consensus hash at burn {}",
            snapshot.block_height
        );
        checked += 1;
    }
    assert_eq!(checked, DERIVED_FLOOR, "the node derived the whole window");
}
