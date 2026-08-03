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
    OpsHash, PoxId, SnapshotChain, SortitionHash, SortitionSnapshot, SortitionWinner,
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
        pox_id: PoxId::initial(),
    }
}

/// How many of the captured window already derive exactly.
///
/// Ten, which is every burn block up to epoch 4.0's boundary at 960,230. The
/// boundary itself is where the operation set changes and nano and the network
/// stop agreeing on which transactions count.
const DERIVED_FLOOR: usize = 10;

/// Mainnet's magic bytes, which decide what counts as a burnchain transaction.
const MAINNET_MAGIC: [u8; 2] = *b"X2";

#[test]
fn mainnet_sortitions_derive_from_mainnet_bitcoin_blocks() {
    let Some(root) = capture() else {
        eprintln!("set NANO_MAINNET_CAPTURE to a capture directory to run this");
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
    let mut chain = SnapshotChain::new(seed_from(&captured[0]));


    let mut checked = 0;
    let mut first_divergence = None;
    for snapshot in captured.iter().skip(1) {
        let block = blocks
            .get(&snapshot.block_height)
            .expect("every captured snapshot has its Bitcoin block");
        let txids: Vec<[u8; 32]> = block
            .operations
            .iter()
            .map(|operation| operation.txid)
            .collect();
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
                PoxId::initial(),
                winner,
            )
            .expect("the chain extends");

        if hex::encode(derived.operations_hash.as_bytes()) != snapshot.ops_hash {
            first_divergence = Some((snapshot.block_height, "operations hash"));
            break;
        }
        // The sortition hash is a chain from the one before it, so a window
        // proves it; the consensus hash mixes prior ones at power-of-two
        // offsets reaching back thousands of blocks, which a window of fifteen
        // cannot supply. That needs a chain replayed from its own genesis, and
        // is what a node building the chain itself will exercise.
        if hex::encode(derived.sortition_hash.as_bytes()) != snapshot.sortition_hash {
            first_divergence = Some((snapshot.block_height, "sortition hash"));
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
