//! Every sortition's coinbase, against stacks-core's own schedule.
//!
//! This is a pure function of the Bitcoin height, so the cheapest oracle
//! there is: `StacksEpochId::coinbase_reward` is the number the chain uses, and
//! it can be called directly.
//!
//! It is worth an oracle because the failure is silent and late. A tenure's
//! coinbase is recorded when the tenure starts and paid out a hundred tenures
//! later, so a wrong emission produces a block whose every receipt matches and
//! whose state root does not — a hundred tenures after the mistake.
//!
//! nano's table was written with *absolute* Bitcoin heights where the schedule
//! compares *effective* ones (the height above the chain's first burn block), so
//! it answered 1,000 STX for the whole 500-STX era, and it had no entry at all
//! for SIP-045 restoring 1,000 STX at the epoch 4.0 boundary.

use nano_chainstate::CoinbaseSchedule;
use stacks_common::types::StacksEpochId;

/// Heights worth asking about: both chains' genesis, both 4.0 boundaries, every
/// interval edge and the block either side of it.
fn heights(first: u64, boundary: u64) -> Vec<u64> {
    let mut heights = vec![0, 1, first.saturating_sub(1), first, first + 1];
    for offset in [
        77_777_u64,
        77_777 * 7,
        77_777 * 14,
        77_777 * 21,
        278_950,
        boundary - first,
    ] {
        for step in [-1_i64, 0, 1] {
            let height = first.saturating_add(offset);
            heights.push(height.saturating_add_signed(step));
        }
    }
    // And a sweep, so an interval nobody thought of still gets asked.
    heights.extend((0..400).map(|step| first + step * 997));
    heights.sort_unstable();
    heights.dedup();
    heights
}

fn check(mainnet: bool, first: u64, boundary: u64) {
    let schedule = CoinbaseSchedule {
        mainnet,
        first_bitcoin_height: first,
        initial_mining_bonus: 0,
    };
    for height in heights(first, boundary) {
        // Epoch 4.0 is the only epoch this node runs, and every epoch from 3.1
        // uses the SIP-029 schedule.
        let expected = StacksEpochId::Epoch40.coinbase_reward(mainnet, first, height);
        assert_eq!(
            schedule.emission_at(height),
            expected,
            "coinbase at burn height {height} ({})",
            if mainnet { "mainnet" } else { "testnet" }
        );
    }
}

#[test]
fn the_mainnet_emission_matches_the_chain() {
    check(true, 666_050, 960_230);
}

#[test]
fn the_testnet_emission_matches_the_chain() {
    check(false, 2_000_000, 40_000_000);
}

/// The one number this was wrong about, named so a regression is legible.
///
/// A tenure starting just before the epoch 4.0 boundary emits 500 STX; one at or
/// after it emits 1,000. Both matter to the same block, because the tenure that
/// matures a hundred tenures after the boundary started before it.
#[test]
fn the_epoch_four_boundary_is_where_the_emission_doubles() {
    let schedule = CoinbaseSchedule {
        mainnet: true,
        first_bitcoin_height: 666_050,
        initial_mining_bonus: 0,
    };
    assert_eq!(schedule.emission_at(960_229), 500_000_000);
    assert_eq!(schedule.emission_at(960_230), 1_000_000_000);
}
