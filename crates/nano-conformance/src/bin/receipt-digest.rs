//! Reduce a `new_block` observer payload to its bounded receipt commitment.
//!
//! The 24-hour mainnet hold compares the follower artifact's archived receipt
//! commitment with the digest of an independently executing node's `new_block`
//! payload for the same block. The digest lives in `nano_conformance` next to
//! the fixture gates that already use it; this makes it callable from the hold
//! watcher without a test harness around it.

use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let [payload] = arguments.as_slice() else {
        eprintln!("usage: receipt-digest <new_block-payload.json>");
        return ExitCode::FAILURE;
    };
    let bytes = match std::fs::read(payload) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("{payload}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let payload: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(payload) => payload,
        Err(error) => {
            eprintln!("the payload is not JSON: {error}");
            return ExitCode::FAILURE;
        }
    };
    let digest = nano_conformance::receipt_digest(&payload);
    match serde_json::to_string(&digest) {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("the digest did not serialize: {error}");
            ExitCode::FAILURE
        }
    }
}
