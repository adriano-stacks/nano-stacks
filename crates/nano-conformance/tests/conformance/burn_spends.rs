//! How many of a commitment's outputs are payouts, and what a sortition cost.
//!
//! One number decides both, and nothing else does: a commitment's burn is the sum
//! of its payout outputs, and everything after them is the miner's change — the
//! output the next commitment spends to chain through the mining window. Count one
//! too many and every candidate's weight becomes the size of its wallet. The same
//! number then decides the two Clarity-visible burn spends of a sortition,
//! `miner-spend-total` and `miner-spend-winner`.
//!
//! The rule was believed to be the size of the cycle's reward set, capped at two,
//! because the captured hacknet chain pays one output and has one stacker. It is
//! not, and this file is the reading that settled it: stacks-core writes the count
//! down once, as a function of the *height*, and pads a short reward set with burn
//! addresses to reach it. The captured chain pays one output because it is past the
//! waterfall — which nano already knew how to answer; what it had never been told
//! is where the waterfall began.
//!
//! Two oracles, cheapest first. `RewardSetInfo::commit_outs_for` is a pure function
//! of a reward set and a phase, so it can be *called*; and the archive's own
//! `pox_payouts` column states the count for every burn block a capture holds — on
//! a chain that pays two and on a chain that pays one.

use std::{collections::BTreeMap, fs, path::Path};

use nano_bitcoin::BitcoinBlock;

use crate::{follow_path, mainnet_sortition};

/// A captured snapshot, in the fields the payout rule is checked against.
///
/// The column names are stacks-core's own, so these are its rows rather than a
/// translation of them.
#[derive(Debug, serde::Deserialize)]
struct Snapshot {
    block_height: u64,
    burn_header_hash: String,
    sortition_id: String,
    consensus_hash: String,
    sortition_hash: String,
    total_burn: String,
    sortition: i64,
    winning_block_txid: String,
    /// `(addresses, amount-per-address)`, JSON, exactly as the column holds it.
    pox_payouts: String,
}

impl Snapshot {
    /// The payout-output count this block's commitments carried, and what each paid.
    ///
    /// The address list is padded with burn addresses to
    /// `SortitionHandleTx::get_num_pox_payouts` before it is written
    /// (`index_add_fork_info`), so its length *is* that count and needs no
    /// interpreting.
    fn payouts(&self) -> (usize, u128) {
        let (addresses, per_output): (Vec<serde_json::Value>, u128) =
            serde_json::from_str(&self.pox_payouts).expect("the pox_payouts column is JSON");
        (addresses.len(), per_output)
    }

    /// The block's whole payout burn: every eligible commitment's, summed.
    fn payout_burn(&self) -> u128 {
        let (outputs, per_output) = self.payouts();
        per_output * outputs as u128
    }

    fn total_burn(&self) -> u128 {
        self.total_burn.parse().expect("a burn total is a number")
    }
}

fn snapshots(root: &Path) -> Vec<Snapshot> {
    let mut rows: Vec<Snapshot> = serde_json::from_slice(
        &fs::read(root.join("sortition/snapshots.json")).expect("read the snapshots"),
    )
    .expect("parse the snapshots");
    rows.sort_by_key(|snapshot| snapshot.block_height);
    rows
}

/// The captured chain's Bitcoin blocks, keyed by height.
///
/// Decoded through the `PreStx` cache in height order, because that is the only way
/// an operation authorised by an earlier block's output is recognised at all.
fn captured_blocks(root: &Path, rows: &[Snapshot], magic: [u8; 2]) -> BTreeMap<u64, BitcoinBlock> {
    let mut cache = nano_bitcoin::PreStxCache::new();
    rows.iter()
        .filter(|snapshot| snapshot.block_height > 0)
        .filter_map(|snapshot| {
            let encoded = fs::read_to_string(
                root.join("bitcoin/blocks")
                    .join(format!("{}.hex", snapshot.burn_header_hash)),
            )
            .ok()?;
            let raw = hex::decode(encoded.trim()).expect("the block is hexadecimal");
            let block = nano_bitcoin::decode_block_with_pre_stx(
                snapshot.block_height,
                &raw,
                magic,
                &mut cache,
            )
            .expect("decode the captured Bitcoin block");
            Some((snapshot.block_height, block))
        })
        .collect()
}

/// The count stacks-core expects, for every shape of reward set and every phase.
///
/// The oracle is `RewardSetInfo::commit_outs_for`, which stacks-core's own test
/// calls "the single source of truth shared by the miner (relayer) and the parser".
/// Calling it is what makes this a fact rather than a reading of one:
/// `PayoutSchedule::outputs_at` has to return the length of what that function
/// produces, at every height.
///
/// The case that matters is the **one-recipient** reward set, because that is the
/// shape the hacknet capture was thought to have and the reason the count was
/// thought to follow the reward set. stacks-core pads it — "If the number of
/// recipients in the set was odd, we need to pad with a burn address", in
/// `check_pox_pre_waterfall`, and the same padding in `into_commit_outs` — so a
/// cycle with one stacker still pays two outputs, the second of them a burn, and
/// the count never moves with the recipients at all.
#[test]
fn the_payout_output_count_is_the_one_stacks_core_builds() {
    use blockstack_lib::chainstate::burn::operations::leader_block_commit::{
        RewardSetInfo, RewardSetInfoV0, RewardSetInfoWaterfall,
    };
    use blockstack_lib::chainstate::stacks::address::PoxAddress;
    use stacks_common::types::chainstate::{BlockHeaderHash, StacksAddress};
    use stacks_common::util::hash::Hash160;

    let recipient = |byte: u8| {
        PoxAddress::Standard(
            StacksAddress::new(26, Hash160([byte; 20])).expect("a valid address"),
            None,
        )
    };
    let reward_set = |recipients: usize| {
        RewardSetInfo::V0(RewardSetInfoV0 {
            anchor_block: BlockHeaderHash([0; 32]),
            recipients: (0..recipients)
                .map(|index| (recipient(u8::try_from(index).expect("small") + 1), 0))
                .collect(),
            allow_nakamoto_punishment: true,
        })
    };
    let waterfall = RewardSetInfo::Waterfall(RewardSetInfoWaterfall {
        anchor_block: BlockHeaderHash([0; 32]),
        sbtc_address: recipient(9),
    });

    // A small explicit calendar keeps this unit test about the three output-count
    // rules. Capture-specific agreement is checked against every archived row below.
    let calendar = nano_sync::PoxInfo {
        first_bitcoin_height: 0,
        bitcoin_height: 0,
        prepare_phase_length: 5,
        reward_phase_length: 15,
        reward_slots: 30,
        rejection_fraction: None,
        pox_5_activation_height: Some(262),
        v1_unlock_height: None,
        v2_unlock_height: None,
        v3_unlock_height: None,
    };
    let schedule = nano_node::payout_schedule(&calendar).expect("a payout schedule");

    // A reward phase, one recipient: stacks-core pays two, so nano must count two.
    let outs = RewardSetInfo::commit_outs_for(Some(reward_set(1)), false, false);
    assert_eq!(
        outs.len(),
        2,
        "a one-recipient reward set is padded to two outputs, not paid as one"
    );
    assert!(
        outs[1].is_burn(),
        "the padding output of a short reward set is a burn address"
    );
    // Burn 262 is in a reward phase of the cycle before the waterfall opens — which
    // is also where pox-5 activates on this chain, so it is the last classic block
    // a real commitment could stand in.
    assert!(!schedule.is_in_prepare_phase(262));
    assert_eq!(
        schedule.outputs_at(262),
        outs.len(),
        "nano counts a reward-phase commitment's payout outputs"
    );
    // A full recipient set: the same count, arrived at without padding.
    assert_eq!(
        RewardSetInfo::commit_outs_for(Some(reward_set(2)), false, false).len(),
        schedule.outputs_at(262),
    );

    // A prepare phase: one burn output, whatever the reward set holds. Burn 276 is
    // offset 16 of a twenty-block cycle whose last four and whose mod-0 block are
    // the prepare phase.
    assert!(schedule.is_in_prepare_phase(276));
    for from in [Some(reward_set(2)), None] {
        assert_eq!(
            RewardSetInfo::commit_outs_for(from, true, false).len(),
            schedule.outputs_at(276),
        );
    }

    // The waterfall: the one sBTC address, in a reward phase and in a prepare phase
    // alike — the phase stops deciding anything once it is on.
    for in_prepare_phase in [false, true] {
        assert_eq!(
            RewardSetInfo::commit_outs_for(Some(waterfall.clone()), in_prepare_phase, false).len(),
            1,
        );
    }
    assert_eq!(schedule.outputs_at(280), 1);
    assert!(schedule.is_in_prepare_phase(296));
    assert_eq!(
        schedule.outputs_at(296),
        1,
        "past the waterfall a prepare phase changes nothing"
    );
}

/// The archive states the payout-output count, and nano's schedule agrees.
///
/// Two things follow from `pox_payouts` that need no interpretation: the address
/// list's length is the count nano must return, and `amount × length` is the
/// block's whole payout burn — which is exactly the step the running `total_burn`
/// takes wherever a block elected somebody.
///
/// Both captures are checked and they disagree about the count, which is the point.
/// The hacknet capture sits past the waterfall and pays **one**; the mainnet window
/// sits in a classic reward phase and pays **two**. A rule that followed the reward
/// set would have to explain mainnet's two, and a rule fixed at two would have to
/// explain hacknet's one. The height explains both.
#[test]
fn the_captured_snapshots_state_the_payout_output_count() {
    let hacknet = (
        "hacknet",
        follow_path::fixtures(),
        nano_node::payout_schedule(&follow_path::pox()).expect("a payout schedule"),
    );
    let mut captures = vec![hacknet];
    if let Some(root) = mainnet_sortition::capture() {
        captures.push(("mainnet", root, mainnet_sortition::mainnet_payouts()));
    } else {
        nano_conformance::skip_gate(
            "NANO_MAINNET_CAPTURE would add the chain that pays two outputs",
        );
    }
    for (label, root, schedule) in captures {
        let rows = snapshots(&root);
        let mut checked = 0;
        for (behind, snapshot) in rows.iter().zip(rows.iter().skip(1)) {
            let (outputs, _) = snapshot.payouts();
            assert_eq!(
                outputs,
                schedule.outputs_at(snapshot.block_height),
                "{label}: the payout-output count at burn {}",
                snapshot.block_height
            );
            // The running total steps by the block's payout burn only where the
            // block elected somebody; a block that elected nobody adds nothing to
            // it however much its miners spent.
            if snapshot.sortition == 0 || behind.block_height + 1 != snapshot.block_height {
                continue;
            }
            assert_eq!(
                snapshot.total_burn() - behind.total_burn(),
                snapshot.payout_burn(),
                "{label}: the running burn total's step at burn {} is the block's payout burn",
                snapshot.block_height
            );
            checked += 1;
        }
        assert!(
            checked > 0,
            "{label}: no captured block had both a sortition and its parent, so the payout \
             burn was compared against nothing"
        );
        println!("{label}: {checked} burn blocks state their own payout burn");
    }
}

/// The two Clarity-visible burn spends, derived rather than borrowed.
///
/// The sortition tracker already holds the commitment window the distribution was
/// weighed over, so it answers both without reading a Bitcoin block again — and
/// this says its answer is the archive's, on the capture whose window used to
/// derive nothing at all. The mainnet side is asserted per block inside
/// `mainnet_sortition::the_node_tracker_derives_the_same_window`, where the chain is
/// already being walked.
#[test]
fn the_derived_burn_spends_are_the_archives() {
    let root = follow_path::fixtures();
    let rows = snapshots(&root);
    let blocks = captured_blocks(&root, &rows, *b"T3");
    let calendar = follow_path::pox();
    let schedule = nano_node::payout_schedule(&calendar).expect("a schedule");
    let cycle = u64::from(calendar.prepare_phase_length + calendar.reward_phase_length);
    let last = rows
        .last()
        .expect("the capture has a last snapshot")
        .block_height;
    let seed = rows
        .iter()
        .rev()
        .find(|snapshot| {
            let start = snapshot.block_height;
            snapshot.sortition == 1
                && start >= 6
                && start + 5 <= last
                && start.saturating_sub(calendar.first_bitcoin_height) / cycle
                    == (start + 5).saturating_sub(calendar.first_bitcoin_height) / cycle
        })
        .map(|snapshot| snapshot.block_height)
        .expect("the capture holds a winning seed with five successors in its reward cycle");
    let directory = tempfile::tempdir().expect("a directory for the seed");
    let mut chain = seeded_chain(&rows, seed, directory.path());
    let read = |height: u64| {
        blocks
            .get(&height)
            .cloned()
            .ok_or_else(|| format!("no Bitcoin block at {height}"))
    };
    chain
        .catch_up(read, seed, schedule, nano_node::sortition::CATCH_UP_LIMIT)
        .expect("the mining window fills from behind the seed");

    let mut sortitions = 0;
    let mut empty = 0;
    for snapshot in rows
        .iter()
        .filter(|snapshot| (seed + 1..=seed + 5).contains(&snapshot.block_height))
    {
        chain
            .advance(
                &read(snapshot.block_height).expect("a captured block"),
                schedule,
            )
            .expect("the chain extends");
        let spends = chain.tip().burn_spends;
        if snapshot.sortition == 0 {
            assert!(
                spends.is_none(),
                "burn {} elected nobody, so there is no winner's spend to report",
                snapshot.block_height
            );
            empty += 1;
            continue;
        }
        let spends = spends.unwrap_or_else(|| {
            panic!(
                "burn {} elected somebody but reported no burn spends",
                snapshot.block_height
            )
        });
        assert_eq!(
            u128::from(spends.total),
            snapshot.payout_burn(),
            "the total spend derived at burn {}",
            snapshot.block_height
        );
        assert!(
            spends.winner > 0 && spends.winner <= spends.total,
            "the winner's spend at burn {} is {} against a total of {}, and the Clarity \
             documentation promises the first is a positive number no larger than the second",
            snapshot.block_height,
            spends.winner,
            spends.total
        );
        sortitions += 1;
    }
    assert!(
        sortitions >= 3,
        "only {sortitions} sortitions were compared"
    );
    println!(
        "{sortitions} sortitions' burn spends derive from the captured Bitcoin blocks, \
         and {empty} burn blocks that elected nobody report none"
    );
}

/// A tracker seeded at one captured burn block, through the production loader.
///
/// Written out and read back with `SortitionTracker::from_capture` rather than built
/// by hand, because that is the path a node takes: the seed's `PoX` history comes
/// from its own sortition identifier and its winner from the `winning_block_txid`
/// column, and a hand-built seed would prove neither.
fn seeded_chain(
    rows: &[Snapshot],
    seed_height: u64,
    directory: &Path,
) -> nano_node::sortition::SortitionTracker {
    let hashes: Vec<&str> = rows
        .iter()
        .filter(|snapshot| snapshot.block_height <= seed_height)
        .map(|snapshot| snapshot.consensus_hash.as_str())
        .collect();
    let seed = rows
        .iter()
        .find(|snapshot| snapshot.block_height == seed_height)
        .expect("the capture holds the seed");
    assert_ne!(
        seed.sortition, 0,
        "the seed must have elected somebody, or its own winner's spend is unknowable"
    );
    // The seed's own winning VRF seed, which the sampling of the block after it
    // mixes. A chain that derived its snapshots states it; the `snapshots` table
    // does not, and recovering it from the seed block's commitments only works where
    // they agree about it — which on this capture they do not, because its miners
    // bid for different parent tenures. It is read out of the winning commitment in
    // the captured Bitcoin block, which is where a chain would have got it.
    let winner_vrf_seed = nano_conformance::captured_bitcoin_snapshots(&follow_path::fixtures())
        .expect("the captured Bitcoin snapshots read")
        .get(&seed.consensus_hash)
        .map(|context| hex::encode(context.vrf_seed))
        .expect("the seed's burn block names a winning commitment");
    fs::write(
        directory.join("consensus-hashes.json"),
        serde_json::to_vec(&serde_json::json!({ "hashes": hashes })).expect("encodes"),
    )
    .expect("the history is written");
    let snapshots = rows
        .iter()
        .filter(|snapshot| snapshot.block_height <= seed_height)
        .map(|snapshot| {
            serde_json::json!({
                "block_height": snapshot.block_height,
                "burn_header_hash": snapshot.burn_header_hash,
                "sortition_id": snapshot.sortition_id,
                "consensus_hash": snapshot.consensus_hash,
                "sortition_hash": snapshot.sortition_hash,
                "total_burn": snapshot.total_burn,
                "sortition": snapshot.sortition,
                "winning_block_txid": snapshot.winning_block_txid,
                "winner_vrf_seed": (snapshot.block_height == seed_height)
                    .then_some(&winner_vrf_seed),
                "pox_payouts": snapshot.pox_payouts,
            })
        })
        .collect::<Vec<_>>();
    fs::write(
        directory.join("snapshots.json"),
        serde_json::to_vec(&snapshots).expect("encodes"),
    )
    .expect("the seed is written");
    fs::copy(
        follow_path::fixtures()
            .join("sortition")
            .join(nano_node::sortition::LEADER_KEY_FILE),
        directory.join(nano_node::sortition::LEADER_KEY_FILE),
    )
    .expect("the capture's leader-key registry is copied");
    nano_node::sortition::SortitionTracker::from_capture(directory)
        .expect("the seed starts a chain")
}
