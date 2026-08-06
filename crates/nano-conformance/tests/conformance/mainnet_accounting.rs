//! The mainnet checkpoint has to owe what mainnet owes, for a hundred tenures.
//!
//! A tenure pays out the one a hundred back. For the first hundred tenures nano
//! executes, that earlier tenure is one it never saw, so the earnings have to
//! travel with the checkpoint. A checkpoint short of them executes fine right
//! up until the first payout it cannot derive, and then fails with
//! `UnknownTenure` — after the node has already run for hours.
//!
//! Point `NANO_MAINNET_CHECKPOINT` at the checkpoint directory to run it.
//!
//! This checks the *artifact*. `xtask capture-fixtures` refuses to write a short
//! or holed one in the first place — contiguity rather than a count, because a
//! count is what it used to check and a count cannot see the failure that
//! happened: the live ledger held 193 tenures spanning 201 heights with eight
//! missing in the middle, so its outer bounds said complete and long. The
//! refusal itself is unit-tested in `xtask`; the queries that feed it need the
//! 505 GB stacks-core archive, and this file is what catches an artifact that
//! got past both.

use std::{env, fs, path::PathBuf};

use nano_chainstate::{MINER_REWARD_MATURITY, TenureAccounting};
use nano_primitives::Network;

fn checkpoint() -> Option<PathBuf> {
    env::var("NANO_MAINNET_CHECKPOINT").ok().map(PathBuf::from)
}

#[test]
fn the_mainnet_checkpoint_owes_a_full_maturity_window() {
    let Some(directory) = checkpoint() else {
        nano_conformance::skip_gate("NANO_MAINNET_CHECKPOINT to a checkpoint directory to run this");
        return;
    };
    let bytes = fs::read(directory.join("native-effects.json")).expect("read the accounting");
    let accounting = TenureAccounting::from_json(&bytes).expect("decode the accounting");

    let (first, last) = accounting
        .known_earnings_span()
        .expect("the checkpoint seeds tenure earnings");
    assert!(
        last - first >= MINER_REWARD_MATURITY,
        "the seeded window spans {} tenures, which is short of the {} a node needs \
         before its own tenures mature",
        last - first + 1,
        MINER_REWARD_MATURITY + 1
    );
    for coinbase_height in first..=last {
        assert!(
            accounting.earnings_at(coinbase_height).is_some(),
            "tenure {coinbase_height} is missing from the seeded window"
        );
    }

    // The schedule has to name the network the entries were exported for. Without
    // it a node cannot price the coinbase of a tenure it executes itself, and
    // priced against the *wrong* network it prices every one of them wrongly:
    // mainnet and testnet have different emission intervals and a different first
    // burn height, so the first tenure start past the checkpoint diverges and
    // nothing says why.
    let schedule = accounting
        .schedule()
        .expect("the checkpoint carries a coinbase schedule");
    assert!(
        schedule.mainnet,
        "a mainnet checkpoint carries a schedule that says it is not mainnet"
    );
    assert_eq!(
        schedule.first_bitcoin_height,
        666_050,
        "mainnet's first burn block is what every emission interval is measured from"
    );

    // Every tenure the node executes before its own mature must derive a payout
    // from what the checkpoint carries, and none may fail.
    for offset in 1..=MINER_REWARD_MATURITY {
        let coinbase_height = last + offset;
        accounting
            .effects_for_tenure(Network::MAINNET, coinbase_height)
            .unwrap_or_else(|error| {
                panic!("tenure {coinbase_height} cannot be paid from the checkpoint: {error:?}")
            });
    }
}
