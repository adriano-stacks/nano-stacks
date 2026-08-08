//! What a hard kill during a checkpoint import leaves, and that it is refused.
//!
//! An import writes with journalling off — a mainnet import fell from 60 MB/s to
//! under 3 with a WAL that could never checkpoint — so it cannot roll back. The
//! pages it wrote stay in the file and read as state: `marf_block` has rows, the
//! MARF has a tip, the value store has values, and none of that says whether the
//! import ran to the end. The rule this replaces treated any `marf_block` row as
//! "already imported", so a killed import was resumed and every state root
//! computed afterwards was wrong for a reason nothing reported.
//!
//! Which needs a real process and SIGKILL: an in-process test drops the store,
//! and a clean close is the case that already worked. The kill is triggered by
//! *watching the directory* rather than by a wall-clock delay, because the whole
//! import of the captured checkpoint takes a few tens of milliseconds and process
//! start-up is longer than that — `kill_during_replay` learned this the
//! expensive way, where a fixed delay meant sixteen of twenty kills landed before
//! the run had begun and the test measured almost nothing. Here the parent polls
//! for the file that marks each phase and kills once it sees it, so every kill
//! lands where it is claimed to.
//!
//! Two phases, because they leave different wreckage:
//!
//! - the **trie** phase, killed as soon as the import mark appears, leaves a
//!   `marf.sqlite` holding schema pages and no committed states;
//! - the **side store** phase, killed as soon as `clarity.sqlite` appears, leaves
//!   a *committed* MARF — tip included — beside a value store missing values its
//!   own leaves name. This is the shape the old rule got wrong without ambiguity,
//!   and the test asserts the sealed states that would have fooled it are really
//!   there. Resumed without the mark, this state opens, executes and fails with
//!   `origin nonce does not match account state`: a consensus divergence
//!   reported as a bad transaction.
//!
//! The captured checkpoint is 5.9 MB, which is small enough that the import's one
//! transaction never spills `SQLite`'s page cache — so the third shape, a *partial*
//! trie with committed states in it, is not reachable offline. That is the shape
//! the 33 GB mainnet import hit. The mark covers it by construction, since it
//! spans from before the first write to after the last, and there is nothing
//! about it for a bigger checkpoint to reach that this does not.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use nano_conformance::durable_replay_chainstate;
use nano_marf::UnfinishedImport;

/// Kills per phase. One proves nothing about a window a few milliseconds wide.
const KILLS: usize = 12;

/// How long the parent waits for a phase to start before giving up on it.
const PATIENCE: Duration = Duration::from_secs(30);

/// How often the parent looks for the file that says a phase has started.
const POLL: Duration = Duration::from_micros(100);

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// Import the captured checkpoint in a child process, replaying no blocks.
///
/// Nothing but the import, so a kill cannot land in block execution instead and
/// be mistaken for one that landed in the import.
fn import_process(directory: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_replay-blocks"))
        .arg(fixtures())
        .arg(directory)
        .arg("0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the import")
}

/// Wait for `path` to appear, or for the child to exit without it appearing.
fn wait_for(path: &Path, child: &mut Child) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        if child.try_wait().expect("poll the import").is_some() {
            return false;
        }
        std::thread::sleep(POLL);
    }
    false
}

/// Run an import to completion, timing the two phases a kill can land in.
///
/// Reported because the windows are what the kills have to hit: a phase whose
/// window is zero would mean the test below is killing somewhere else.
fn phase_windows(directory: &Path) -> (Duration, Duration) {
    let marker = UnfinishedImport::marker(directory);
    let clarity = directory.join("clarity.sqlite");
    let mut child = import_process(directory);
    let start = Instant::now();
    assert!(
        wait_for(&marker, &mut child),
        "the import marks the directory before it writes to it"
    );
    let trie_started = start.elapsed();
    assert!(
        wait_for(&clarity, &mut child),
        "the import reaches the value store"
    );
    let side_store = start.elapsed();
    assert!(
        child.wait().expect("reap the import").success(),
        "the uninterrupted import finishes"
    );
    let finished = start.elapsed();
    assert!(
        !marker.exists(),
        "a finished import leaves no mark: absence is what lets a directory \
         imported before this existed still open"
    );
    (
        side_store.saturating_sub(trie_started),
        finished.saturating_sub(side_store),
    )
}

/// What one killed import left behind.
struct Wreckage {
    /// Whether the directory carries the import mark.
    marked: bool,
    /// Sealed states in the partial MARF — what the old rule read as
    /// "already imported".
    blocks: u32,
}

fn wreckage(directory: &Path) -> Wreckage {
    let marf = directory.join("marf.sqlite");
    let blocks =
        rusqlite::Connection::open_with_flags(&marf, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()
            .and_then(|connection| {
                connection
                    .query_row("SELECT count(*) FROM marf_block", [], |row| row.get(0))
                    .ok()
            })
            .unwrap_or(0);
    Wreckage {
        marked: UnfinishedImport::marker(directory).exists(),
        blocks,
    }
}

/// Reopen a state directory the way the node does, and require a refusal.
fn refusal(directory: &Path) -> String {
    match durable_replay_chainstate(&fixtures(), directory) {
        Ok(_) => panic!(
            "an import that was killed must not open: {}",
            directory.display()
        ),
        Err(error) => error,
    }
}

/// A delay that spreads the kills through a phase instead of stacking them at
/// its first instant.
///
/// Over the first half of the window, not all of it: the window is measured on a
/// cold import and the runs that are killed are warm, so a delay near the
/// measured end lands past the real one. Scattering over the whole window put
/// seventeen of twenty-four kills after the import had already finished — the same
/// mistake `kill_during_replay` made with a fixed delay, in a different disguise.
fn scatter(iteration: usize, window: Duration) -> Duration {
    let steps = u32::try_from(KILLS).expect("the kill count fits");
    let step = u32::try_from(iteration).expect("the iteration fits") % steps;
    window * step / (2 * steps)
}

/// Kill one import in `phase`, and say what it left.
fn kill_during(directory: &Path, trigger: &Path, delay: Duration) -> Wreckage {
    let mut child = import_process(directory);
    let started = wait_for(trigger, &mut child);
    std::thread::sleep(delay);
    // SIGKILL: no destructor runs, no connection is closed, nothing is flushed
    // that was not already written. SIGTERM would exercise the orderly path.
    drop(child.kill());
    child.wait().expect("reap the import");
    assert!(
        started,
        "the kill landed inside the import, not before it started: {}",
        trigger.display()
    );
    wreckage(directory)
}

#[test]
fn an_import_a_kill_interrupted_is_refused_and_not_resumed() {
    if !fixtures().join("nakamoto/blocks").is_dir() {
        nano_conformance::skip_gate("the captured blocks are unavailable");
        return;
    }
    // Measured twice and taken at the shorter: the first import of a process is
    // cold, and a window measured cold is longer than the ones being killed.
    let cold = tempfile::tempdir().expect("a directory");
    let (cold_trie, cold_side_store) = phase_windows(cold.path());
    let reference_directory = tempfile::tempdir().expect("a directory");
    let (warm_trie, warm_side_store) = phase_windows(reference_directory.path());
    let (trie_window, side_store_window) = (
        cold_trie.min(warm_trie),
        cold_side_store.min(warm_side_store),
    );
    println!("import phases: trie {trie_window:?}, side store {side_store_window:?}");

    let mut refused = 0_usize;
    let mut resumable = 0_usize;
    let mut bare = 0_usize;
    let mut raced = 0_usize;
    let mut kept: Option<tempfile::TempDir> = None;
    for iteration in 0..KILLS * 2 {
        let directory = tempfile::tempdir().expect("a directory");
        let trie_phase = iteration % 2 == 0;
        let (trigger, window) = if trie_phase {
            (UnfinishedImport::marker(directory.path()), trie_window)
        } else {
            (directory.path().join("clarity.sqlite"), side_store_window)
        };
        let left = kill_during(directory.path(), &trigger, scatter(iteration / 2, window));
        if !left.marked {
            // The import finished between the trigger and the signal landing.
            // Then the directory is complete and must open — which is the other
            // half of the claim, so it is checked rather than skipped.
            raced += 1;
            durable_replay_chainstate(&fixtures(), directory.path())
                .expect("a directory whose import did finish opens");
            continue;
        }

        refused += 1;
        let message = refusal(directory.path());
        assert!(
            message.contains("did not finish"),
            "the refusal says the import did not finish: {message}"
        );
        assert!(
            message.contains(
                &UnfinishedImport::marker(directory.path())
                    .display()
                    .to_string()
            ),
            "and names the file that says so: {message}"
        );
        assert!(
            message.contains("Remove"),
            "and what the operator has to do: {message}"
        );
        // Started again without the operator doing anything, it refuses again:
        // recovery is not a silent redo on top of the wreckage.
        assert_eq!(refusal(directory.path()), message);

        if left.blocks > 0 {
            // The rule this replaces asked exactly this question and would have
            // called the directory imported.
            resumable += 1;
        } else {
            bare += 1;
        }
        kept = Some(directory);
    }

    println!(
        "of {} kills: {refused} refused ({resumable} already holding sealed states, \
         {bare} holding none), {raced} finished before the signal landed",
        KILLS * 2
    );
    assert!(
        refused * 4 >= KILLS * 2 * 3,
        "at least three kills in four landed inside the import: {refused} of {}",
        KILLS * 2
    );
    assert!(
        resumable > 0,
        "at least one killed import left the sealed states the old rule read as \
         'already imported'"
    );
    assert!(
        bare > 0,
        "and at least one was killed before any state was sealed, so both shapes \
         of wreckage are covered"
    );

    // Recovery is what the message says: remove the directory and import again.
    // The result has to be the state a clean import produces, not something that
    // merely opens.
    let wrecked = kept.expect("a killed import was kept");
    fs::remove_dir_all(wrecked.path()).expect("remove the wrecked state");
    let recovered = state_after_two_blocks(wrecked.path());
    assert_eq!(
        recovered,
        state_after_two_blocks(reference_directory.path()),
        "the re-imported state is the one a clean import reaches, roots included"
    );
}

/// The tip and sealed root a directory holds after replaying two blocks.
fn state_after_two_blocks(
    directory: &Path,
) -> (Option<[u8; 32]>, Option<nano_primitives::TrieHash>) {
    assert!(
        Command::new(env!("CARGO_BIN_EXE_replay-blocks"))
            .arg(fixtures())
            .arg(directory)
            .arg("2")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()
            .expect("run the replay")
            .success(),
        "{} replays two blocks",
        directory.display()
    );
    let (chainstate, _) =
        durable_replay_chainstate(&fixtures(), directory).expect("reopen the replayed state");
    let tip = chainstate.tip().expect("read the imported tip");
    let root = tip.and_then(|tip| {
        chainstate
            .state_content_root(tip)
            .expect("read the imported content root")
    });
    (tip, root)
}
