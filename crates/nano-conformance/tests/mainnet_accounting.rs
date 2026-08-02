//! The mainnet checkpoint has to owe what mainnet owes, for a hundred tenures.
//!
//! A tenure pays out the one a hundred back. For the first hundred tenures nano
//! executes, that earlier tenure is one it never saw, so the earnings have to
//! travel with the checkpoint. A checkpoint short of them executes fine right
//! up until the first payout it cannot derive, and then fails with
//! `UnknownTenure` — after the node has already run for hours.
//!
//! Point `NANO_MAINNET_CHECKPOINT` at the checkpoint directory to run it.

use std::{env, fs, path::PathBuf};

use nano_chainstate::{MINER_REWARD_MATURITY, TenureAccounting};
use nano_primitives::Network;

fn checkpoint() -> Option<PathBuf> {
    env::var("NANO_MAINNET_CHECKPOINT").ok().map(PathBuf::from)
}

#[test]
fn the_mainnet_checkpoint_owes_a_full_maturity_window() {
    let Some(directory) = checkpoint() else {
        eprintln!("set NANO_MAINNET_CHECKPOINT to a checkpoint directory to run this");
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
