//! clarity-wasm executes mainnet, and the interpreter is not allowed to.
//!
//! The interpreter is the differential oracle clarity-wasm is checked against.
//! Letting it *execute* means the chain advances on results the engine under
//! test never produced — which is exactly what happened here: a replay reported
//! depth 8,673,863 while the compiler had actually stopped at 8,668,161, and
//! the blocks in between were the interpreter's answers, not nano's.
//!
//! So the switch is refused on mainnet rather than trusted to be unset.

use nano_primitives::Network;
use nano_vm::interpreter_allowed;

/// Off mainnet the oracle stays available: a divergence costs nothing there and
/// localizing one is the whole reason the interpreter is in the tree.
#[test]
fn the_oracle_is_available_off_mainnet() {
    assert!(interpreter_allowed(Network::TESTNET, true));
    assert!(!interpreter_allowed(Network::TESTNET, false));
}

/// Not asking for it is always fine, including on mainnet.
#[test]
fn mainnet_runs_the_compiler_without_complaint() {
    assert!(!interpreter_allowed(Network::MAINNET, false));
}

/// Asking for it on mainnet is refused, and says why.
#[test]
#[should_panic(expected = "clarity-wasm is the consensus engine")]
fn mainnet_refuses_to_run_on_the_interpreter() {
    let _ = interpreter_allowed(Network::MAINNET, true);
}
