//! A block that fails must leave nothing behind, however often it is retried.
//!
//! A node retries a block it cannot execute for as long as it is running.
//! Aborting the MARF is not a rollback on its own: fees reach the tenure
//! accounting before the state root is checked, and that accounting lives
//! outside the MARF and is persisted on its own. Left alone, a block rejected
//! 1,417 times added its fee 1,417 times — a tenure's earnings inflated by four
//! orders of magnitude, and the miner rewards that mature from them wrong, while
//! the MARF tip looked perfectly clean.
//!
//! So this rejects the same block many times over and asserts that everything
//! the node keeps is byte-for-byte what it was before the first attempt.
//!
//! This one is a weak witness on its own, and deliberately kept anyway. The
//! captured checkpoint names no started tenure, so `add_fees` counts for nothing
//! and the accounting a rejection here moves is zero — which means this test
//! would have passed before the bug was fixed. It asserts the right invariant on
//! the real replay path, and that is what it is for.
//!
//! The witness that bites on fees is `nano-chainstate`'s own
//! `retrying_a_rejected_block_leaves_no_state_beside_the_marf`: it seeds earnings
//! for the tenure the checkpoint stands in so the captured block's 300 uSTX
//! actually land, and asserts the whole ledger, the accounting bytes a restart
//! would read, and the absence of any header or state for the rejected block.

use std::{fs, path::Path};

use nano_chainstate::{ChainState, TenureAccounting};
use nano_conformance::{FixtureManifest, FixtureMode, reject_captured_block, replay_into};

/// How many blocks to execute before the one that gets rejected.
const BLOCKS: u64 = 1;

/// How many times to retry the failing block.
const ATTEMPTS: usize = 25;

fn fixtures() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

const fn manifest(blocks: u64) -> FixtureManifest {
    FixtureManifest {
        mode: FixtureMode::Captured,
        replay_blocks: blocks,
        receipts: true,
    }
}

/// Open a durable chainstate over the captured checkpoint, in `directory`.
fn open(directory: &Path) -> (ChainState, [u8; 32]) {
    let fixtures = fixtures();
    let checkpoint = fixtures.join("chainstate/checkpoint-H");
    let manifest = fs::read_to_string(checkpoint.join("checkpoint.toml"))
        .expect("read the checkpoint manifest");
    let field = |name: &str| -> String {
        manifest
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name} = "))?.strip_prefix('"'))
            .and_then(|value| value.strip_suffix('"'))
            .unwrap_or_else(|| panic!("the checkpoint names {name}"))
            .to_owned()
    };
    let decode = |value: &str| -> [u8; 32] {
        <[u8; 32]>::try_from(hex::decode(value).expect("hexadecimal").as_slice()).expect("32 bytes")
    };
    let source = decode(&field("source_state_id"));
    let root = nano_primitives::TrieHash::from_bytes(decode(&field("published_state_index_root")));

    let mut chainstate = ChainState::open_from_checkpoint(
        nano_primitives::Network::TESTNET,
        directory,
        checkpoint.join("marf.sqlite"),
        source,
        root,
    )
    .expect("open the checkpoint durably");
    let accounting = fs::read(checkpoint.join("native-effects.json"))
        .ok()
        .and_then(|contents| TenureAccounting::from_json(&contents).ok())
        .expect("the checkpoint carries accounting");
    *chainstate.accounting_mut() = accounting;
    (chainstate, source)
}

#[test]
fn retrying_a_rejected_block_changes_nothing() {
    let directory = tempfile::tempdir().expect("a directory");
    let (mut chainstate, source) = open(directory.path());

    let progress = replay_into(
        &mut chainstate,
        source,
        &fixtures(),
        manifest(BLOCKS),
        0,
        &mut |_, _| {},
    );
    assert_eq!(
        progress.completed, BLOCKS,
        "the run reaches the block that will be rejected: {:?}",
        progress.first_divergence
    );

    let tip = chainstate.tip().expect("read the pre-rejection tip");
    let owed = chainstate
        .accounting_mut()
        .to_json()
        .expect("encode the accounting");

    // The next block, with a state root its header does not commit to: every
    // transaction runs, the fees are added, and it is rejected in
    // `settle_state_root` — which is where a real divergence is rejected.
    for attempt in 0..ATTEMPTS {
        assert!(
            reject_captured_block(
                &mut chainstate,
                &fixtures(),
                manifest(1),
                usize::try_from(BLOCKS).expect("a small count"),
            )
            .expect("read the state while rejecting the block"),
            "attempt {attempt} is rejected, so the assertions below mean something"
        );
        assert_eq!(
            chainstate.tip().expect("read the post-rejection tip"),
            tip,
            "attempt {attempt} leaves the sealed tip alone"
        );
        assert_eq!(
            chainstate
                .accounting_mut()
                .to_json()
                .expect("encode the accounting"),
            owed,
            "attempt {attempt} leaves the tenure accounting alone"
        );
    }
}
