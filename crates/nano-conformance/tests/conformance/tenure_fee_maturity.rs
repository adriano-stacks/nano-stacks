//! A tenure's fees mature one tenure after its coinbase, on the same recipient.
//!
//! stacks-core cannot total a tenure's fees until the next tenure change proves
//! the tenure over, so it records them in the *following* tenure's payment
//! schedule as `MinerPaymentTxFees::Nakamoto { parent_fees }` and pays them, a
//! maturity later, to the recipient of the schedule's parent
//! (`make_scheduled_miner_reward`, `get_parent_matured_miner`). Two tenures
//! therefore pay out at every tenure start: one's coinbase and the previous
//! one's fees.
//!
//! Reading that as "the maturing tenure's own fees, to the previous tenure's
//! recipient" is arithmetically identical for as long as the checkpoint that
//! carried `parent_fees` under its successor's name lasts, and differs first at
//! the earliest tenure the node totalled itself — a hundred tenures further on,
//! where it is a state root mismatch with every receipt matching.
//!
//! The vector is mainnet's own, read off the chain at block 8,673,846:
//! `SP2N4YMH4…` gains exactly the 1,000,000,000 uSTX coinbase of tenure 251,321
//! and `SP70B98…` gains 15,114 uSTX, which is tenure 251,320's fee total.
//! Tenure 251,321's own fee total, 22,539,119, is not paid here at all; it is
//! paid at the next tenure start.

use clarity::vm::types::PrincipalData;
use nano_chainstate::{NativeStxCredit, TenureAccounting};
use nano_primitives::Network;

const MINER_A: &str = "SP2N4YMH4XNWTDCCE4RV0AQ29VKTHGF1F7VQKSCS3";
const MINER_B: &str = "SP70B98HWSFY2M7JB6V6P563TR3JSBWW3S43GS8M";

/// Mainnet's schedules for tenures 251,319 through 251,322, in nano's own form:
/// each tenure's recipient, its coinbase, and the fees *its* transactions paid.
fn mainnet_tenures() -> TenureAccounting {
    let accounting = serde_json::json!({
        "matured_effects": [],
        "tenures": [
            { "coinbase_height": 251_319, "recipient": "SP1GW59G8T3F3SYNB3PY3KT73RPXH7H42HBCGR8M7",
              "coinbase": 1_500_000_000u64, "fees": 1_260u64 },
            { "coinbase_height": 251_320, "recipient": MINER_B,
              "coinbase": 1_000_000_000u64, "fees": 15_114u64 },
            { "coinbase_height": 251_321, "recipient": MINER_A,
              "coinbase": 1_000_000_000u64, "fees": 22_539_119u64 },
            { "coinbase_height": 251_322, "recipient": MINER_B,
              "coinbase": 1_000_000_000u64, "fees": 625_846u64 },
        ],
        "coinbase_schedule": null,
    });
    TenureAccounting::from_json(accounting.to_string().as_bytes())
        .expect("decode the tenure accounting")
}

fn credit(recipient: &str, amount: u128) -> NativeStxCredit {
    NativeStxCredit {
        recipient: PrincipalData::parse(recipient).expect("a mainnet principal"),
        amount,
    }
}

#[test]
fn a_tenure_start_pays_one_coinbase_and_the_previous_tenures_fees() {
    let accounting = mainnet_tenures();

    // Block 8,673,846, which starts tenure 251,421.
    let effects = accounting
        .effects_for_tenure(Network::MAINNET, 251_421)
        .expect("tenure 251,421 pays out from tenures 251,320 and 251,321");
    assert_eq!(
        effects.credits,
        vec![credit(MINER_A, 1_000_000_000), credit(MINER_B, 15_114),],
        "tenure 251,321's coinbase and tenure 251,320's fees"
    );
    // Only the coinbase is new money; the fees already existed.
    assert_eq!(effects.liquid_supply_increase, 1_000_000_000);

    // The next tenure start is where tenure 251,321's own 22,539,119 lands, on
    // its own recipient.
    let next = accounting
        .effects_for_tenure(Network::MAINNET, 251_422)
        .expect("tenure 251,422 pays out from tenures 251,321 and 251,322");
    assert_eq!(
        next.credits,
        vec![credit(MINER_B, 1_000_000_000), credit(MINER_A, 22_539_119),],
        "tenure 251,322's coinbase and tenure 251,321's fees"
    );
}

#[test]
fn a_tenures_fees_are_never_paid_to_the_tenure_that_follows_it() {
    let accounting = mainnet_tenures();
    // The two readings differ by exactly one tenure's fees, and mainnet miners
    // alternate, so the wrong one pays a real recipient a real amount — from the
    // wrong tenure. Naming both sides keeps a future refactor from swapping them
    // back on the strength of a matching receipt.
    for (coinbase_height, wrong) in [(251_421, 22_539_119), (251_422, 625_846)] {
        let effects = accounting
            .effects_for_tenure(Network::MAINNET, coinbase_height)
            .expect("the window covers this tenure");
        assert!(
            effects.credits.iter().all(|credit| credit.amount != wrong),
            "tenure {coinbase_height} paid the fees of the tenure that matures next"
        );
    }
}
