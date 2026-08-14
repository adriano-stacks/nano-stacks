//! The `pox-5` lock window in the capture, replayed under nano's own handler.
//!
//! `pox_locking.rs` puts nano's handler beside stacks-core's on synthetic
//! responses, which is the sharper instrument but proves nothing about the
//! responses a chain actually produces. This is the other half: the captured
//! chain really does stake, and every lock it applied is a `stx_lock_event` the
//! network published, so replaying those blocks and diffing the events is the
//! handler against the chain rather than against a second implementation of the
//! same idea.
//!
//! Mainnet cannot supply this. It is still on `pox-4` — task 050 records the
//! consequence, that a pox-5 reward set does not exist to check against — so the
//! only chain with `pox-5` positions is the captured one, and the window in the
//! tree is the oracle. Hence the coverage assertion below: a recapture that lost
//! the stake window would leave every other test in this suite green while this
//! one silently compared nothing, which is the failure mode the whole
//! ground-truth strategy is built to refuse.

use std::{
    fs,
    path::{Path, PathBuf},
};

use nano_chainstate::{AppliedBlock, NakamotoBlock};
use nano_conformance::replay_captured_blocks;
use serde_json::Value;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// One lock the chain applied: who, how much, until when.
///
/// Compared as strings because that is how both sides publish them — a
/// `u128` of micro-STX and a burn height, rendered by the same rules — and
/// parsing them to compare would only add a way to disagree.
#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Lock {
    contract: String,
    address: String,
    amount: String,
    unlock_height: String,
}

impl Lock {
    /// Read one out of a captured `new_block` event, if that is what it is.
    fn from_captured(entry: &Value) -> Option<Self> {
        let lock = entry.get("stx_lock_event")?;
        let field = |name: &str| lock.get(name)?.as_str().map(str::to_owned);
        Some(Self {
            contract: field("contract_identifier")?,
            address: field("locked_address")?,
            amount: field("locked_amount")?,
            unlock_height: field("unlock_height")?,
        })
    }
}

/// Every lock the capture's `new_block` stream published, block by block.
fn captured_locks(root: &Path) -> Vec<Lock> {
    let mut directory: Vec<PathBuf> = fs::read_dir(root.join("events").join("new_block"))
        .expect("the capture's new_block stream")
        .map(|entry| entry.expect("a new_block file").path())
        .collect();
    // Named by height, so lexical order is chain order.
    directory.sort();
    directory
        .iter()
        .flat_map(|path| {
            let block: Value =
                serde_json::from_slice(&fs::read(path).expect("a new_block payload"))
                    .expect("a JSON new_block payload");
            let mut events = block
                .get("events")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            // The capture's array is not in event order, and nano's is: it comes
            // from the receipts in the order the block executed them. So the
            // ordering has to be taken from the field that states it, the same
            // way `compare_receipts` does.
            events.sort_by_key(|entry| entry.get("event_index").and_then(Value::as_u64));
            events
        })
        .filter_map(|entry| Lock::from_captured(&entry))
        .collect()
}

/// Every lock nano applied while replaying, read out of its own receipts.
fn replayed_locks(root: &Path, blocks: u64) -> Vec<Lock> {
    let mut locks = Vec::new();
    let mut collect = |_: &NakamotoBlock, applied: &AppliedBlock| {
        for receipt in &applied.receipts {
            for (index, event) in receipt.result.events.iter().enumerate() {
                let published = event
                    .json_serialize(index, &receipt.txid, receipt.committed)
                    .expect("nano's own event serializes");
                if let Some(lock) = Lock::from_captured(&published) {
                    locks.push(lock);
                }
            }
        }
    };
    let depth = replay_captured_blocks(root, blocks, &mut collect);
    assert!(
        depth.first_failure.is_none(),
        "the capture replays: {:?}",
        depth.first_failure
    );
    locks
}

/// The capture stakes, and nano's handler applies exactly the locks the chain did.
#[test]
fn the_captured_pox_five_window_locks_the_same_stx() {
    let root = fixtures();
    let expected = captured_locks(&root);

    // The coverage assertion. Not a formality: without a stake window in the
    // capture this test passes by comparing two empty lists, and nano's PoX
    // handler would have no chain-level oracle at all.
    assert!(
        !expected.is_empty(),
        "the capture has no stx_lock_event, so it cannot check the PoX handler at all"
    );
    assert!(
        expected
            .iter()
            .all(|lock| lock.contract.ends_with(".pox-5")),
        "a lock came from a contract other than pox-5: {expected:?}"
    );

    let blocks = nano_conformance::FixtureManifest::load(&root.join("manifest.toml"))
        .expect("fixture manifest")
        .replay_blocks;
    assert_eq!(replayed_locks(&root, blocks), expected);
}
