//! A command that says it is reading a state must leave the filesystem alone.
//!
//! The bug these pin is not a crash. `MarfStore::open` creates the directory and
//! both databases when they are absent, so an inspection pointed one directory
//! too high used to *succeed*: it built an empty store there, found no value in
//! it, and said so. That answer is indistinguishable from the same answer given
//! about the real state, which is how a path typo becomes believable evidence.
//!
//! Each test therefore compares the complete directory tree — every path and
//! every byte, so a new metadata row counts — before and after the command.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use nano_primitives::Network;
use nano_vm::{BlockCommit, BlockHeader, Vm};

/// Every file under `root`, by relative path, with its contents.
///
/// Contents rather than lengths: an `engine_identity` row appended to a `SQLite`
/// page changes bytes without changing the file's size, and "creates no metadata
/// row" is one of the things being asserted.
fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut found = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            if path.is_dir() {
                // Recorded as well as descended into: a command that creates an
                // empty directory has still written to the state.
                found.insert(relative, Vec::new());
                pending.push(path);
            } else {
                found.insert(relative, std::fs::read(&path).unwrap_or_default());
            }
        }
    }
    found
}

fn xtask(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(arguments)
        .output()
        .expect("run xtask")
}

/// A state directory of the shape every one of these commands takes: one sealed
/// block under `<state>/chainstate`.
fn sealed_state(state: &Path) -> [u8; 32] {
    let block = [7; 32];
    let mut vm = Vm::open(Network::MAINNET, state.join("chainstate")).expect("create a state");
    vm.begin_block(None, block).expect("begin");
    vm.commit_block(
        block,
        &BlockCommit {
            header: BlockHeader {
                burn_block_height: 960_240,
                tenure_height: 251_400,
                tenure_start_height: 1,
                ..BlockHeader::default()
            },
            ledger: b"one block".to_vec(),
            decision: None,
        },
    )
    .expect("commit");
    block
}

/// The inspections, each spelled the way an operator would type it.
fn inspections(state: &str) -> Vec<Vec<String>> {
    [
        vec!["state-value", state, "tip", "vm-epoch::epoch-version"],
        vec!["check-module", state, "SP000000000000000000002Q6VF78.pox-5"],
        vec!["block-info", state, "1"],
        vec!["probe-header", state, &"11".repeat(32)],
        vec!["eval", state, "(+ u1 u1)"],
    ]
    .into_iter()
    .map(|arguments| arguments.into_iter().map(str::to_owned).collect())
    .collect()
}

fn assert_refused_without_writing(root: &Path, state: &str, why: &str) {
    for arguments in inspections(state) {
        let before = tree(root);
        let borrowed: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let output = xtask(&borrowed);
        assert!(
            !output.status.success(),
            "`xtask {}` succeeded against {why}:\n{}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stdout)
        );
        assert_eq!(
            tree(root),
            before,
            "`xtask {}` changed the filesystem while failing on {why}",
            arguments.join(" ")
        );
    }
}

#[test]
fn a_path_that_is_not_there_creates_nothing() {
    let root = tempfile::tempdir().expect("a directory");
    let absent = root.path().join("no-such-state");
    assert_refused_without_writing(
        root.path(),
        absent.to_str().expect("utf-8"),
        "a path that is not there",
    );
    assert!(
        !absent.exists(),
        "the inspection created {}",
        absent.display()
    );
}

#[test]
fn one_directory_too_high_creates_nothing() {
    let root = tempfile::tempdir().expect("a directory");
    let state = root.path().join("state");
    sealed_state(&state);

    // The real state is `<root>/state`, whose databases are under
    // `<root>/state/chainstate`. Naming `<root>` is the mistake that opened this
    // task: it resolves to `<root>/chainstate`, which does not exist.
    assert_refused_without_writing(
        root.path(),
        root.path().to_str().expect("utf-8"),
        "a path one directory above the state",
    );
    assert!(
        !root.path().join("chainstate").exists(),
        "the inspection created a second chainstate beside the real one"
    );
}

#[test]
fn an_empty_chainstate_is_named_rather_than_answered() {
    let root = tempfile::tempdir().expect("a directory");
    let state = root.path().join("state");
    std::fs::create_dir_all(state.join("chainstate")).expect("an empty chainstate");
    assert_refused_without_writing(
        root.path(),
        state.to_str().expect("utf-8"),
        "an empty chainstate",
    );

    let output = xtask(&[
        "state-value",
        state.to_str().expect("utf-8"),
        "tip",
        "vm-epoch::epoch-version",
    ]);
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(
        said.contains("marf.sqlite"),
        "an empty chainstate has to be named, not answered: {said}"
    );
    assert!(
        !said.contains("no value"),
        "an empty chainstate answered as an absence: {said}"
    );
}

#[test]
fn a_chainstate_that_is_not_one_is_named() {
    let root = tempfile::tempdir().expect("a directory");
    let chainstate = root.path().join("state").join("chainstate");
    std::fs::create_dir_all(&chainstate).expect("a directory");
    // Two files with the right names and nothing a state would recognise inside.
    std::fs::write(chainstate.join("marf.sqlite"), b"not a database").expect("write");
    std::fs::write(chainstate.join("clarity.sqlite"), b"not a database either").expect("write");
    assert_refused_without_writing(
        root.path(),
        root.path().join("state").to_str().expect("utf-8"),
        "files that are not databases",
    );
}

#[test]
fn reading_a_real_state_changes_nothing() {
    let root = tempfile::tempdir().expect("a directory");
    let state = root.path().join("state");
    sealed_state(&state);

    let before = tree(root.path());
    for arguments in inspections(state.to_str().expect("utf-8")) {
        let borrowed: Vec<&str> = arguments.iter().map(String::as_str).collect();
        // Whether the state can answer is not the point — `check-module` has no
        // pox-5 to compile here and says so. Not writing is the point, and it
        // holds on the paths that answer and the paths that do not.
        xtask(&borrowed);
        assert_eq!(
            tree(root.path()),
            before,
            "`xtask {}` wrote to the state it was reading",
            arguments.join(" ")
        );
    }
}

#[test]
fn a_state_a_node_still_owns_is_refused() {
    let root = tempfile::tempdir().expect("a directory");
    let state = root.path().join("state");
    sealed_state(&state);
    // What a running node leaves beside its database, and what a killed one
    // leaves behind: frames nothing has folded into the pages. Reading past them
    // would answer with superseded state.
    std::fs::write(state.join("chainstate").join("marf.sqlite-wal"), [0; 32]).expect("a wal");

    let output = xtask(&[
        "state-value",
        state.to_str().expect("utf-8"),
        "tip",
        "vm-epoch::epoch-version",
    ]);
    assert!(
        !output.status.success(),
        "a state with an uncommitted wal was read"
    );
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(
        said.contains("committed"),
        "the refusal has to say a node owns this state: {said}"
    );
}
