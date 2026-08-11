//! Cross a reward cycle boundary and still derive the chain's own identifiers.
//!
//! A consensus hash mixes the `PoX` history, and that history gains a bit every
//! time a reward cycle opens: whether the cycle selected an anchor block. Get the
//! bit wrong and every hash from the boundary on is wrong, silently — so the
//! tracker used to refuse to cross a boundary at all, and a follower stopped dead
//! at the first one it met. That is [[082]]. This focused gate derives across the
//! final five reward-cycle boundaries the capture carries.
//!
//! **In Nakamoto the bit is one, and that is a rule rather than an observation.**
//! `load_nakamoto_reward_set` builds exactly one anchor status,
//! `PoxAnchorBlockStatus::SelectedAndKnown` (stacks-core
//! `chainstate/nakamoto/coordinator/mod.rs:543`), so `is_reward_info_known` is
//! unconditionally true and `make_next_pox_id` unconditionally calls
//! `extend_with_present_block`. Its own comment says why: *"In Nakamoto, every
//! reward cycle must have a `PoX` anchor block; otherwise, the chain halts."* The
//! other outcome that exists there is `Ok(None)` — the anchor is not processed
//! *yet* — which is a wait and not a zero. `NotSelected` and `SelectedAndUnknown`
//! are reachable only through the epoch-2.x path and the first cycle of epoch 3.0,
//! neither of which a node starting at or after the 4.0 boundary can be asked
//! about.
//!
//! The rule was previously called "reasoned rather than measured", and the earlier
//! decider — "the cycle recorded a signer set, so it selected an anchor" — could
//! not be verified on any state available offline. This measures it instead, and
//! does not have to trust the reasoning: the capture states the answer twice over.
//! Its `sortition_id` is the burn header hash and the `PoX` vector hashed
//! together, so the vector at every height is recoverable, and its
//! `consensus_hash` mixes the vector directly. Deriving forward across five
//! boundaries and comparing both at every block is a differential against what
//! stacks-core actually wrote. A wrong bit diverges at the first and stays diverged.
//!
//! No skip gate and no environment variable: the fixture carries every Bitcoin
//! block and snapshot this test selects, so it runs everywhere the suite does.

use std::{collections::BTreeMap, fs, path::PathBuf};

use nano_bitcoin::{BitcoinBlock, decode_block};
use nano_node::sortition::SortitionTracker;
use nano_primitives::{BitcoinHeaderHash, SortitionId};
use nano_sortition::unbroken_pox_id_for;

use crate::follow_path::pox;
use crate::mainnet_sortition::{Captured, consensus_history, seed_from};

/// The capture's magic bytes, which decide what counts as a burnchain transaction.
const MAGIC: [u8; 2] = *b"T3";

const BOUNDARIES: usize = 5;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn decode32(value: &str) -> [u8; 32] {
    <[u8; 32]>::try_from(
        hex::decode(value.trim_start_matches("0x"))
            .expect("hexadecimal")
            .as_slice(),
    )
    .expect("32 bytes")
}

fn captured() -> Vec<Captured> {
    serde_json::from_slice(
        &fs::read(fixtures().join("sortition/snapshots.json")).expect("read the snapshots"),
    )
    .expect("parse the snapshots")
}

fn bitcoin_blocks(snapshots: &[Captured]) -> BTreeMap<u64, BitcoinBlock> {
    snapshots
        .iter()
        .map(|snapshot| {
            let raw = fs::read_to_string(
                fixtures()
                    .join("bitcoin/blocks")
                    .join(format!("{}.hex", snapshot.burn_header_hash)),
            )
            .expect("read the captured Bitcoin block");
            let block = decode_block(
                snapshot.block_height,
                &hex::decode(raw.trim()).expect("the block is hexadecimal"),
                MAGIC,
            )
            .expect("decode the captured Bitcoin block");
            (snapshot.block_height, block)
        })
        .collect()
}

fn boundary_window(snapshots: &[Captured]) -> (u64, u64, Vec<u64>) {
    let payouts = nano_node::payout_schedule(&pox()).expect("a payout schedule");
    let boundaries = snapshots
        .iter()
        .map(|snapshot| snapshot.block_height)
        .filter(|height| payouts.starts_reward_cycle(*height))
        .collect::<Vec<_>>();
    let selected = boundaries
        .get(boundaries.len().saturating_sub(BOUNDARIES)..)
        .expect("the capture crosses five reward-cycle boundaries")
        .to_vec();
    assert_eq!(selected.len(), BOUNDARIES);
    let first = selected[0];
    let seed = snapshots
        .iter()
        .rev()
        .find(|snapshot| snapshot.block_height < first && snapshot.sortition != 0)
        .expect("the capture has a winning seed before its final five boundaries")
        .block_height;
    let last = snapshots
        .last()
        .expect("the capture has snapshots")
        .block_height;
    (seed, last, selected)
}

fn seeded_tracker(
    snapshots: &[Captured],
    blocks: &BTreeMap<u64, BitcoinBlock>,
    seed_height: u64,
) -> SortitionTracker {
    let seed = snapshots
        .iter()
        .find(|snapshot| snapshot.block_height == seed_height)
        .expect("the capture holds the seed");
    let mut history =
        consensus_history(&fixtures()).expect("the capture carries its consensus hashes");
    let seed_hash = seed.consensus_hash.trim_start_matches("0x");
    let seed_index = history
        .iter()
        .position(|hash| hash.to_string() == seed_hash)
        .expect("the consensus history contains the selected seed");
    history.truncate(seed_index + 1);
    let mut snapshot = seed_from(seed);
    snapshot.pox_id = unbroken_pox_id_for(snapshot.bitcoin_header_hash, snapshot.sortition_id, 64)
        .expect("the seed's identifier states an unbroken PoX history");
    let winning = decode32(&seed.winning_block_txid);
    snapshot.winner_vrf_seed = blocks
        .get(&seed_height)
        .expect("the capture holds the seed's Bitcoin block")
        .operations
        .iter()
        .find_map(
            |operation| match (operation.txid == winning, &operation.kind) {
                (true, nano_bitcoin::BitcoinOperationKind::LeaderBlockCommit { new_seed, .. }) => {
                    Some(*new_seed)
                }
                _ => None,
            },
        );
    assert!(
        snapshot.winner_vrf_seed.is_some(),
        "the seed's winning commitment is in its own Bitcoin block"
    );
    SortitionTracker::new(snapshot, history).expect("seed the tracker")
}

/// What the capture says the `PoX` history was at each of its boundaries.
///
/// Read off the `sortition_id`, which is the burn header hash and the vector
/// hashed together — so this is the chain's own statement of the answer and not a
/// second opinion about it.
#[test]
fn the_capture_states_the_pox_bit_at_every_boundary_it_crosses() {
    let snapshots = captured();
    let payouts = nano_node::payout_schedule(&pox()).expect("a payout schedule");
    let (seed, last, expected_boundaries) = boundary_window(&snapshots);
    let mut boundaries = 0;
    let mut previous: Option<usize> = None;
    for snapshot in &snapshots {
        if !(seed..=last).contains(&snapshot.block_height) {
            continue;
        }
        let bits = unbroken_pox_id_for(
            BitcoinHeaderHash::from_bytes(decode32(&snapshot.burn_header_hash)),
            SortitionId::from_bytes(decode32(&snapshot.sortition_id)),
            64,
        )
        .unwrap_or_else(|| {
            panic!(
                "burn {} has a sortition identifier that is not an unbroken PoX history",
                snapshot.block_height
            )
        })
        .as_consensus_bytes()
        .len();
        if let Some(before) = previous {
            let opened = payouts.starts_reward_cycle(snapshot.block_height);
            if opened {
                boundaries += 1;
            }
            // One bit per boundary and no bit anywhere else. Every one of them is a
            // 1, because an unbroken history is the only shape that resolves at all
            // and every height here resolves.
            assert_eq!(
                bits,
                before + usize::from(opened),
                "burn {} {} a reward cycle, so the PoX history has to gain exactly {} bit",
                snapshot.block_height,
                if opened { "opens" } else { "does not open" },
                usize::from(opened)
            );
        }
        previous = Some(bits);
    }
    assert_eq!(boundaries, expected_boundaries.len());
}

/// Derive across five reward-cycle boundaries and match every captured block.
///
/// The whole claim in one assertion: nano's own arithmetic, over nothing but the
/// raw Bitcoin blocks and the consensus hashes behind the seed, produces the
/// sortition identifier and the consensus hash stacks-core wrote — across five
/// reward cycle boundaries. The identifier is what carries the `PoX` vector, so a
/// wrong anchor bit cannot hide in it.
#[test]
fn a_derived_chain_crosses_five_boundaries_and_stays_on_the_chain() {
    let snapshots = captured();
    let blocks = bitcoin_blocks(&snapshots);
    let payouts = nano_node::payout_schedule(&pox()).expect("a payout schedule");
    let (seed_height, last, expected_boundaries) = boundary_window(&snapshots);

    let mut tracker = seeded_tracker(&snapshots, &blocks, seed_height);

    let seed_pox = tracker.tip().pox_id.clone();

    // Primed and walked through `catch_up`, which is the production path: priming
    // reads the six blocks behind the seed that the burn distribution is weighed
    // over, and without them the running total is short from the first block.
    // `keep_from` holds every derived snapshot so all 119 can be compared —
    // the same call a follower makes so a batch does not drop what it is standing
    // on.
    tracker.keep_from(seed_height);
    tracker
        .catch_up(
            |height| {
                blocks
                    .get(&height)
                    .cloned()
                    .ok_or_else(|| format!("the capture holds no Bitcoin block at {height}"))
            },
            last,
            payouts,
            4_096,
        )
        .expect("derive the whole capture, boundaries and all");
    assert_eq!(tracker.tip().bitcoin_height, last);

    let mut crossed = Vec::new();
    let mut checked = 0;
    for snapshot in &snapshots {
        if snapshot.block_height <= seed_height || snapshot.block_height > last {
            continue;
        }
        if payouts.starts_reward_cycle(snapshot.block_height) {
            crossed.push(snapshot.block_height);
        }
        let derived = tracker
            .snapshot_at(snapshot.block_height)
            .unwrap_or_else(|| panic!("burn {} was derived", snapshot.block_height));
        assert_eq!(
            derived.sortition_id,
            SortitionId::from_bytes(decode32(&snapshot.sortition_id)),
            "burn {} derives the sortition identifier the chain recorded, which is the \
             burn header hash and the PoX history hashed together",
            snapshot.block_height
        );
        assert_eq!(
            derived.consensus_hash.to_string(),
            snapshot.consensus_hash.trim_start_matches("0x"),
            "burn {} derives the consensus hash the chain recorded",
            snapshot.block_height
        );
        let expected_winner =
            (snapshot.sortition != 0).then(|| decode32(&snapshot.winning_block_txid));
        assert_eq!(
            derived.winner_txid, expected_winner,
            "burn {} derives the winner the chain recorded",
            snapshot.block_height
        );
        checked += 1;
    }

    assert_eq!(crossed, expected_boundaries);
    assert_eq!(
        u64::try_from(checked).expect("a count fits u64"),
        last - seed_height
    );
    let captured_tip = snapshots
        .iter()
        .find(|snapshot| snapshot.block_height == last)
        .expect("the capture holds its final snapshot");
    let expected_tip = unbroken_pox_id_for(
        BitcoinHeaderHash::from_bytes(decode32(&captured_tip.burn_header_hash)),
        SortitionId::from_bytes(decode32(&captured_tip.sortition_id)),
        64,
    )
    .expect("the final identifier states an unbroken PoX history");
    assert_eq!(
        tracker.tip().pox_id,
        expected_tip,
        "the five derived boundaries produce the capture's final PoX history"
    );
    assert_eq!(
        tracker.tip().pox_id.as_consensus_bytes().len(),
        seed_pox.as_consensus_bytes().len() + BOUNDARIES,
        "five boundaries add five bits"
    );
}
