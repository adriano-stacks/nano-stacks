//! The mainnet compiler regression slice: roots, receipts, costs and events.
//!
//! Not an oracle, and the distinction is the whole reason this file has a long
//! comment. `/home/aldur/mainnet-capture` declares `receipts = false` and holds no
//! `new_block` events at all, because that stream is what a stacks-core node's
//! *event observer* emits while it executes: it exists only if somebody was
//! listening at the time, and no public API serves it for a historical block. The
//! hacknet capture has receipts precisely because nano's own harness was the
//! observer while that chain ran.
//!
//! So the frozen digests here are **nano's own** receipts, from blocks whose
//! `state_index_root` mainnet's own signed headers verified before nano sealed
//! them. What that makes them is a regression gate rather than a conformance
//! result — and it catches the one failure a state root cannot see: a compiler
//! change that alters a receipt, a cost dimension or an event without altering any
//! state. That is exactly the shape of a refused contract call, which writes
//! nothing and seals the root an untouched block seals, and it is the case
//! [[060-make-the-consensus-execution-engine-explicit-and-r]] discovered a week of
//! work after assuming roots were enough.
//!
//! Refresh it against a run whose roots the chain verified:
//!
//! ```sh
//! cargo xtask freeze-receipts <observer-dir> \
//!   crates/nano-conformance/fixtures/mainnet/receipts.json 0 500
//! ```

use std::{collections::BTreeMap, fs, path::PathBuf};

use nano_conformance::ReceiptDigest;

#[derive(serde::Deserialize)]
struct Frozen {
    first_height: u64,
    last_height: u64,
    blocks: Vec<ReceiptDigest>,
}

fn frozen() -> Frozen {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/mainnet/receipts.json");
    // Read rather than skipped when absent, deliberately. A missing regression
    // fixture is the failure this gate exists to prevent being invisible: the
    // release gate would otherwise report green on a slice nobody pinned.
    let body = fs::read(&path)
        .unwrap_or_else(|error| panic!("the mainnet receipt slice at {}: {error}", path.display()));
    serde_json::from_slice(&body).expect("the mainnet receipt slice is JSON")
}

/// The slice is a contiguous run of mainnet blocks, and every block says something.
///
/// Three ways a frozen fixture can be useless without being absent, all of which
/// would let it pass a looser check: a hole in the heights means the run it came
/// from was not the run it claims; a block with no digest means the payload was
/// unreadable when it was frozen; and a slice of one block pins nothing.
#[test]
fn the_frozen_mainnet_slice_is_a_run_of_blocks() {
    let frozen = frozen();
    assert!(
        frozen.blocks.len() >= 100,
        "a slice of {} blocks is too short to catch a compiler regression",
        frozen.blocks.len()
    );
    assert_eq!(frozen.blocks.first().map(|block| block.height), Some(frozen.first_height));
    assert_eq!(frozen.blocks.last().map(|block| block.height), Some(frozen.last_height));
    let mut previous = None;
    let mut transactions = 0;
    let mut events = 0;
    for block in &frozen.blocks {
        assert_eq!(block.block.len(), 64, "block {} has no identity", block.height);
        assert_eq!(block.digest.len(), 64, "block {} has no digest", block.height);
        if let Some(previous) = previous {
            assert!(
                block.height > previous,
                "the slice goes backwards at {}",
                block.height
            );
        }
        previous = Some(block.height);
        transactions += block.transactions;
        events += block.events;
    }
    assert!(
        transactions > 0 && events > 0,
        "a slice with {transactions} transactions and {events} events pins nothing about execution"
    );
    println!(
        "{} mainnet blocks frozen, {} to {}, {transactions} transactions and {events} events",
        frozen.blocks.len(),
        frozen.first_height,
        frozen.last_height
    );
}

/// A run's receipts are the frozen ones, block for block.
///
/// `NANO_MAINNET_RECEIPTS` names an event observer's output directory — the layout
/// `hacknet/event-sink.py` writes and `xtask freeze-receipts` reads. A run over the
/// same heights must produce the same digest for each of them; the first
/// disagreement is reported with what differed, because "the digest moved" is not
/// an answer and the counts beside it usually are.
#[test]
fn a_run_reproduces_the_frozen_mainnet_receipts() {
    let Some(directory) = std::env::var_os("NANO_MAINNET_RECEIPTS").map(PathBuf::from) else {
        nano_conformance::skip_gate(
            "NANO_MAINNET_RECEIPTS must name an event observer's directory for the mainnet \
             receipt slice to be compared against a run",
        );
        return;
    };
    let frozen = frozen();
    let expected: BTreeMap<u64, &ReceiptDigest> = frozen
        .blocks
        .iter()
        .map(|block| (block.height, block))
        .collect();
    let mut compared = 0;
    for entry in fs::read_dir(directory.join("new_block")).expect("the observer's new_block") {
        let path = entry.expect("a payload").path();
        let Ok(body) = fs::read(&path) else { continue };
        let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&body) else {
            continue;
        };
        let digest = nano_conformance::receipt_digest(&payload);
        let Some(frozen) = expected.get(&digest.height) else {
            continue;
        };
        assert_eq!(
            (&digest.block, digest.transactions, digest.events, &digest.digest),
            (
                &frozen.block,
                frozen.transactions,
                frozen.events,
                &frozen.digest
            ),
            "block {} was executed again and its receipts differ: {} transactions and {} events \
             now against {} and {} frozen. The block's own hash commits to its state root, so a \
             matching hash with a different digest is a receipt, a cost dimension or an event \
             moving without any state moving -- which is what a compile refusal at a call looks \
             like.",
            digest.height,
            digest.transactions,
            digest.events,
            frozen.transactions,
            frozen.events
        );
        compared += 1;
    }
    assert!(
        compared >= 100,
        "only {compared} of the slice's {} blocks were in the run, which is not enough of it to \
         have been checked",
        frozen.blocks.len()
    );
    println!("{compared} blocks' receipts reproduce the frozen slice");
}
