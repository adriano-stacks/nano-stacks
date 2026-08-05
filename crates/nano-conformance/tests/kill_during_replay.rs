//! What a hard kill leaves in a state directory, over and over.
//!
//! An accepted block is durable across two stores: one side-store transaction
//! carrying its header, its Clarity metadata and the ledger the node keeps beside
//! the MARF, and then the MARF's own commit. A process that dies between them must
//! leave the complete parent — never a sealed root whose ledger is a block behind,
//! and never accounting for a block that never sealed.
//!
//! Which needs a process, and a signal that runs no destructor: dropping a
//! chainstate closes its stores cleanly, and a clean close is the case the restart
//! test already covered. So this spawns `replay-blocks`, sends it SIGKILL at an
//! arbitrary moment, reopens the directory and asks what survived — repeatedly,
//! because a window one block wide is not hit reliably by killing once.

use std::{
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, SystemTime},
};

use nano_chainstate::ChainState;
use nano_conformance::{
    FixtureManifest, FixtureMode, captured_blocks_sealed, durable_replay_chainstate, replay_into,
};

/// How many times the replay is killed. A window one block wide shows up
/// sometimes, so once proves nothing.
const KILLS: usize = 20;

/// Blocks each killed run is asked for: more than it will ever reach, so the kill
/// always lands inside the replay rather than after it finished.
const BLOCKS_PER_RUN: u64 = 64;

/// Blocks the reference run establishes fingerprints for. Larger than the killed
/// runs can reach together, so the comparison never runs out of reference.
const REFERENCE_BLOCKS: usize = 160;

fn fixtures() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// Everything a block leaves outside the MARF, as one comparable value.
#[derive(Debug, Eq, PartialEq)]
struct Ledger {
    accounting: Vec<u8>,
    executed: Vec<[u8; 32]>,
    parent_tenure_proof: Option<[u8; 80]>,
    tenure_start: Option<u32>,
}

fn ledger_of(chainstate: &mut ChainState) -> Ledger {
    let tenure_start = chainstate
        .tip()
        .and_then(|tip| chainstate.recorded_header(tip))
        .and_then(|header| chainstate.tenure_start_height(header.tenure_height));
    Ledger {
        accounting: chainstate
            .accounting_mut()
            .to_json()
            .expect("the accounting encodes"),
        executed: chainstate.executed_blocks(),
        parent_tenure_proof: chainstate.parent_tenure_proof(),
        tenure_start,
    }
}

/// What one uninterrupted process holds after each block, by the block it sealed.
fn reference(directory: &Path) -> Vec<([u8; 32], Ledger, Option<nano_primitives::TrieHash>)> {
    let fixtures = fixtures();
    let (mut chainstate, source) =
        durable_replay_chainstate(&fixtures, directory).expect("open the reference state");
    let mut fingerprints = Vec::new();
    for offset in 0..REFERENCE_BLOCKS {
        let depth = replay_into(
            &mut chainstate,
            source,
            &fixtures,
            FixtureManifest {
                mode: FixtureMode::Captured,
                replay_blocks: 1,
                receipts: true,
            },
            offset,
            &mut |_, _| {},
        );
        assert_eq!(
            depth.completed, 1,
            "the reference run replays block {}: {:?}",
            offset + 1,
            depth.first_divergence
        );
        let tip = chainstate.tip().expect("the reference run sealed a block");
        let root = chainstate.state_content_root(tip);
        fingerprints.push((tip, ledger_of(&mut chainstate), root));
    }
    fingerprints
}

/// Replay in a child process and wait for it to finish on its own.
fn replay(directory: &Path, blocks: u64) -> bool {
    replay_process(directory, blocks, Stdio::null())
        .wait()
        .expect("reap the replay")
        .success()
}

/// Replay in a child process and kill it while it is replaying.
///
/// The kill waits for the first block to be sealed. Killing on a wall-clock delay
/// alone tested the wrong thing: as the state grew, reopening it took longer than
/// the delay, so the later kills all landed during the open and the run sealed
/// nothing — twenty iterations that looked like twenty kills and were four.
fn kill_mid_replay(directory: &Path, blocks: u64, after: Duration) -> bool {
    let mut child = replay_process(directory, blocks, Stdio::piped());
    let sealing = child.stdout.take().map(|stdout| {
        let mut lines = BufReader::new(stdout).lines();
        lines.next().is_some()
    });
    std::thread::sleep(after);
    // SIGKILL: no destructor runs, no store is closed, nothing is flushed that
    // was not already committed. Which is the whole point — SIGTERM would
    // exercise the orderly path the restart test already covers.
    drop(child.kill());
    child.wait().expect("reap the replay");
    sealing == Some(true)
}

fn replay_process(directory: &Path, blocks: u64, stdout: Stdio) -> Child {
    Command::new(env!("CARGO_BIN_EXE_replay-blocks"))
        .arg(fixtures())
        .arg(directory)
        .arg(blocks.to_string())
        .stdout(stdout)
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn the replay")
}

/// A delay that lands somewhere different every time, so the kills spread through
/// a block rather than always at the same instant in one.
fn scatter(iteration: usize) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| u64::from(since.subsec_nanos()));
    let iteration = u64::try_from(iteration).unwrap_or(0);
    Duration::from_micros((nanos / 251 + iteration * 3_571) % 80_000)
}

#[test]
fn a_kill_between_the_two_durability_boundaries_leaves_the_complete_parent() {
    if !fixtures().join("nakamoto/blocks").is_dir() {
        nano_conformance::skip_gate("the captured blocks are unavailable");
        return;
    }
    let reference_directory = tempfile::tempdir().expect("a directory");
    let reference = reference(reference_directory.path());

    let killed = tempfile::tempdir().expect("a directory");
    // The first run imports the checkpoint, which is not what is being tested
    // and is far longer than a block, so it is allowed to finish.
    assert!(
        replay(killed.path(), 2),
        "the first run imports the checkpoint and replays two blocks"
    );

    let mut sealed_at_each_kill = Vec::new();
    for iteration in 0..KILLS {
        assert!(
            kill_mid_replay(killed.path(), BLOCKS_PER_RUN, scatter(iteration)),
            "the kill landed while the replay was sealing blocks, not before it started"
        );

        let (mut chainstate, source) =
            durable_replay_chainstate(&fixtures(), killed.path()).expect("reopen after the kill");
        let tip = chainstate.tip().expect("the state has a tip");
        assert_ne!(tip, source, "the run before the kills sealed blocks");
        assert!(
            chainstate.has_block_state(tip),
            "the tip is a block whose state is really there"
        );
        assert!(
            chainstate.recorded_header(tip).is_some(),
            "a sealed root must not outlive the header a later block reads it through"
        );
        assert_eq!(
            chainstate.executed_blocks().last(),
            Some(&tip),
            "the ledger recovered is the one committed with the tip: neither behind it \
             nor for a block that never sealed"
        );
        let (_, expected, expected_root) = reference
            .iter()
            .find(|(block, _, _)| *block == tip)
            .expect("the killed run's tip is one the uninterrupted run also reached");
        assert_eq!(
            ledger_of(&mut chainstate),
            *expected,
            "and everything it holds outside the MARF matches the uninterrupted run's, \
             at the same block"
        );
        assert_eq!(
            chainstate.state_content_root(tip),
            *expected_root,
            "as does the state it sealed"
        );
        sealed_at_each_kill.push(chainstate.executed_blocks().len());
    }

    // Worth printing under `--nocapture`: how far each run got is the only way to
    // see that the kills landed spread through the replay rather than all at the
    // same moment in it.
    println!("blocks sealed after each kill: {sealed_at_each_kill:?}");
    // A kill that always landed before the first block would prove nothing, so
    // say that the runs really made progress across the kills.
    assert!(
        sealed_at_each_kill.last() > sealed_at_each_kill.first(),
        "the killed runs made progress: {sealed_at_each_kill:?}"
    );

    // And the state that survived all of it still replays forward to exactly what
    // the uninterrupted run reached, roots included — the child fails on the first
    // block whose root does not match the captured header.
    let (chainstate, _) =
        durable_replay_chainstate(&fixtures(), killed.path()).expect("reopen at the end");
    let sealed = captured_blocks_sealed(&fixtures(), &chainstate);
    drop(chainstate);
    let remaining = u64::try_from(
        REFERENCE_BLOCKS
            .checked_sub(sealed)
            .expect("the killed runs stayed inside the reference"),
    )
    .expect("the remainder fits");
    assert!(
        replay(killed.path(), remaining),
        "the state that survived {KILLS} kills replays the remaining {remaining} blocks"
    );
    let (mut chainstate, _) =
        durable_replay_chainstate(&fixtures(), killed.path()).expect("reopen at the end");
    let (expected_tip, expected_ledger, expected_root) =
        reference.last().expect("the reference reached its last block");
    assert_eq!(chainstate.tip(), Some(*expected_tip));
    assert_eq!(chainstate.state_content_root(*expected_tip), *expected_root);
    assert_eq!(ledger_of(&mut chainstate), *expected_ledger);
}
