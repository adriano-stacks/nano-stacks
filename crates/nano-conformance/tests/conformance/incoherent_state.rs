//! A state directory that is not whole is refused by name, not by panic.
//!
//! The case that produced this: a reflink copy of a *running* node's working
//! directory. That is not an atomic snapshot — the MARF, its write-ahead log and the
//! Clarity side store are three files a node writes independently — and opening one
//! turned a storage inconsistency into `panic!("trie storage")` thousands of blocks
//! away from the cause.
//!
//! It is not evidence against
//! [[057-commit-and-recover-accepted-block-state-atomically]]: repeated hard kills of
//! a real directory recover coherently, and `kill_during_replay` and `binary_restart`
//! say so on every commit. It is an operator error the binary has to name.
//!
//! Reproduced deterministically rather than by racing a copy: the rows a torn copy
//! loses are deleted outright, which is the same absence with none of the timing.

use std::path::Path;

use nano_chainstate::ChainState;
use nano_primitives::Network;

/// The captured chain, whose network the checkpoint belongs to.
const fn captured_network() -> Network {
    Network::testnet_with_chain_id(0x8000_0000)
}

/// Copy the checked-in checkpoint into a state directory a node could run on.
fn opened_state(directory: &Path) -> ChainState {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let (source, root) = nano_conformance::checkpoint_state(&fixture).expect("checkpoint metadata");
    ChainState::open_from_checkpoint(
        captured_network(),
        directory,
        fixture.join("chainstate/checkpoint-H/marf.sqlite"),
        source,
        root,
    )
    .expect("the checkpoint opens into a fresh directory")
}

/// The tip's own trie rows removed: the store opens and cannot be read from.
///
/// Two assertions, and the second is the one that matters. That the open *fails* is
/// half of it; that it fails with a message naming the file and saying nothing was
/// written is what an operator needs, and what distinguishes this from the panic it
/// replaces.
#[test]
fn a_state_missing_its_tips_trie_is_refused_by_name() {
    let directory = tempfile::tempdir().expect("a state directory");
    let state = directory.path().join("chainstate");
    let chainstate = opened_state(&state);
    let tip = chainstate
        .tip()
        .expect("read the checkpoint tip")
        .expect("the checkpoint sealed a state");
    drop(chainstate);

    // The rows a copy taken mid-write loses: the trie nodes of the sealed tip. The
    // block record stays, which is exactly the shape that used to panic — the store
    // knows the state exists and cannot produce it.
    let marf = state.join("marf.sqlite");
    let connection = rusqlite::Connection::open(&marf).expect("the MARF opens");
    let removed = connection
        .execute(
            "DELETE FROM marf_node WHERE block = (SELECT id FROM marf_block WHERE hash = ?1)",
            rusqlite::params![&tip[..]],
        )
        .expect("the tip's nodes are removable");
    assert!(removed > 0, "the tip had no trie nodes to remove");
    drop(connection);

    let refused = ChainState::open(captured_network(), &state);
    let error = match refused {
        Ok(_) => panic!("a state directory missing its tip's trie was accepted"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("not whole"),
        "the refusal must say what is wrong: {error}"
    );
    assert!(
        error.contains("marf.sqlite"),
        "the refusal must name the file: {error}"
    );
    assert!(
        error.contains("stop the node, then copy"),
        "the refusal must name the supported procedure: {error}"
    );
    println!("refused: {error}");
}

/// Refusing reads and writes nothing, so the directory is no worse afterwards.
///
/// A startup check that repaired, truncated or vacuumed anything would be a second
/// way to lose state, and an operator's next move after this error is usually to
/// copy the directory somewhere and look at it.
#[test]
fn refusing_an_incoherent_state_changes_no_file() {
    let directory = tempfile::tempdir().expect("a state directory");
    let state = directory.path().join("chainstate");
    let chainstate = opened_state(&state);
    let tip = chainstate
        .tip()
        .expect("read the checkpoint tip")
        .expect("the checkpoint sealed a state");
    drop(chainstate);
    let connection = rusqlite::Connection::open(state.join("marf.sqlite")).expect("the MARF opens");
    connection
        .execute(
            "DELETE FROM marf_node WHERE block = (SELECT id FROM marf_block WHERE hash = ?1)",
            rusqlite::params![&tip[..]],
        )
        .expect("the tip's nodes are removable");
    drop(connection);

    let before = fingerprint(&state);
    assert!(ChainState::open(captured_network(), &state).is_err());
    assert_eq!(
        before,
        fingerprint(&state),
        "refusing an incoherent state directory modified it"
    );
}

/// Every file's length and modification time, which is enough to catch a write.
fn fingerprint(state: &Path) -> Vec<(String, u64, std::time::SystemTime)> {
    let mut files: Vec<_> = std::fs::read_dir(state)
        .expect("the state directory reads")
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let metadata = entry.metadata().ok()?;
            metadata.is_file().then(|| {
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    metadata.len(),
                    metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
                )
            })
        })
        .collect();
    files.sort();
    files
}

/// A clean directory still opens, which is the half of this that could regress.
///
/// The check runs on every start, so a check that was too strict would refuse the
/// state every node has. The crash-injection suites (`kill_during_replay`,
/// `kill_during_import`, `binary_restart`) are the other half of that argument and
/// run unchanged.
#[test]
fn a_clean_state_directory_still_opens() {
    let directory = tempfile::tempdir().expect("a state directory");
    let state = directory.path().join("chainstate");
    let tip = opened_state(&state)
        .tip()
        .expect("read the sealed tip")
        .expect("a sealed state");
    let reopened = ChainState::open(captured_network(), &state).expect("a clean directory opens");
    assert_eq!(
        reopened.tip().expect("read the reopened tip"),
        Some(tip),
        "the reopened tip is the sealed one"
    );
}
