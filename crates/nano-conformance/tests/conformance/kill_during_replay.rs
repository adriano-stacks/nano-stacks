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
    collections::BTreeSet,
    fs,
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

/// Blocks the reference run establishes fingerprints for.
///
/// It has to outlast every killed run put together, because each one resumes
/// where the last was cut off. That total is `KILLS` times the blocks a child
/// gets through in `scatter`'s window — about four milliseconds a block here, so
/// twenty kills of at most twenty milliseconds reach roughly a hundred and fifty.
/// This is generous rather than tight: running out shows up as the tip having no
/// fingerprint, which says nothing about the property under test.
const REFERENCE_BLOCKS: usize = 260;

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
        .expect("read the ledger tip")
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
            depth.completed,
            1,
            "the reference run replays block {}: {:?}",
            offset + 1,
            depth.first_divergence
        );
        let tip = chainstate
            .tip()
            .expect("read the reference tip")
            .expect("the reference run sealed a block");
        let root = chainstate
            .state_content_root(tip)
            .expect("read the reference content root");
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
    kill_when(directory, blocks, |_| true, after).is_some()
}

/// Replay in a child process and kill it at the first sealed block `trigger`
/// accepts, answering with the height that block was.
///
/// The reader is deliberately still alive when the signal is sent. Taking the
/// child's stdout, reading one line and dropping the reader closes the read end
/// of the pipe, and the child's next `println!` then fails — which *panics*, and
/// a panic unwinds, and unwinding drops the chainstate, and dropping it closes
/// every store cleanly. That is the one case this file exists to avoid, and it
/// was racing the signal for the whole window. Holding the reader open until
/// after `wait` returns means the only way this child ever stops is the kill.
fn kill_when(
    directory: &Path,
    blocks: u64,
    trigger: impl Fn(u64) -> bool,
    after: Duration,
) -> Option<u64> {
    let mut child = replay_process(directory, blocks, Stdio::piped());
    let stdout = child.stdout.take().expect("the replay's stdout is a pipe");
    let mut lines = BufReader::new(stdout).lines();
    let mut sealed = None;
    while let Some(Ok(line)) = lines.next() {
        let Some(height) = line
            .strip_prefix("sealed ")
            .and_then(|height| height.trim().parse::<u64>().ok())
        else {
            continue;
        };
        if trigger(height) {
            sealed = Some(height);
            break;
        }
    }
    std::thread::sleep(after);
    // SIGKILL: no destructor runs, no store is closed, nothing is flushed that
    // was not already committed. Which is the whole point — SIGTERM would
    // exercise the orderly path the restart test already covers.
    drop(child.kill());
    child.wait().expect("reap the replay");
    drop(lines);
    sealed
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

/// Reopen a killed state directory and hold what survived to the reference's.
///
/// Answers how many blocks the survivor holds, which is how a caller sees that
/// its kills landed at different depths rather than all at one.
fn survived(
    killed: &Path,
    reference: &[([u8; 32], Ledger, Option<nano_primitives::TrieHash>)],
) -> usize {
    let (mut chainstate, source) =
        durable_replay_chainstate(&fixtures(), killed).expect("reopen after the kill");
    let tip = chainstate
        .tip()
        .expect("read the surviving tip")
        .expect("the state has a tip");
    assert_ne!(tip, source, "the run before the kills sealed blocks");
    assert!(
        chainstate
            .has_block_state(tip)
            .expect("read the tip's state"),
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
        chainstate
            .state_content_root(tip)
            .expect("read the tip's content root"),
        *expected_root,
        "as does the state it sealed"
    );
    // The decision record travels in the transaction before the MARF commit,
    // so a block that survived a kill has one, whole, content-addressed and
    // naming the accepted verdict with the sealed root. A record for a block
    // the trie never sealed was discarded at reopen; a sealed block without
    // its record would be the impossible order.
    let sealed = chainstate
        .decision_record(tip)
        .expect("read the tip's decision record")
        .expect("a block sealed under this revision carries its decision record");
    let record: nano_chainstate::DecisionRecord =
        serde_json::from_slice(&sealed.record).expect("the sealed record parses");
    assert_eq!(
        record.content_hash().expect("hash the parsed record"),
        sealed.content_hash,
        "the record is the bytes its content hash names"
    );
    assert_eq!(
        record.verdict,
        nano_chainstate::Verdict::Accepted,
        "a sealed block's record says accepted"
    );
    assert_eq!(
        record.block_id,
        hex::encode(tip),
        "the record names the block it was sealed under"
    );
    chainstate.executed_blocks().len()
}

/// The Stacks heights of the captured blocks that start a tenure.
fn captured_tenure_starts() -> BTreeSet<u64> {
    nano_conformance::captured_block_paths(&fixtures())
        .iter()
        .filter_map(|path| {
            let block = nano_chainstate::NakamotoBlock::decode(&fs::read(path).ok()?).ok()?;
            nano_chainstate::starts_new_tenure(&block).then_some(block.header.chain_length)
        })
        .collect()
}

/// A delay that lands somewhere different every time, so the kills spread through
/// a block rather than always at the same instant in one.
///
/// Twenty milliseconds rather than the eighty this started with. The child used
/// to die of a broken pipe long before the signal reached it, so a wide window
/// cost nothing; now that it survives until it is killed, the window is how much
/// of the reference each kill consumes.
fn scatter(iteration: usize) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| u64::from(since.subsec_nanos()));
    let iteration = u64::try_from(iteration).unwrap_or(0);
    Duration::from_micros((nanos / 251 + iteration * 3_571) % 20_000)
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

        sealed_at_each_kill.push(survived(killed.path(), &reference));
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
    let sealed =
        captured_blocks_sealed(&fixtures(), &chainstate).expect("read the number of sealed blocks");
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
    let (expected_tip, expected_ledger, expected_root) = reference
        .last()
        .expect("the reference reached its last block");
    assert_eq!(
        chainstate.tip().expect("read the final tip"),
        Some(*expected_tip)
    );
    assert_eq!(
        chainstate
            .state_content_root(*expected_tip)
            .expect("read the final content root"),
        *expected_root
    );
    assert_eq!(ledger_of(&mut chainstate), *expected_ledger);
}

/// How many tenure transitions to interrupt. Each one costs a process start and
/// a state open, and the tenure starts in the capture are three or four blocks
/// apart, so this stays well inside the reference.
const TENURE_KILLS: usize = 8;

/// A tenure transition is the widest thing a block commits, so interrupt one on
/// purpose rather than by luck.
///
/// A tenure-start block does everything an ordinary block does and then some: it
/// matures a miner's rewards a hundred tenures back, mints against the liquid
/// supply, opens the new tenure's earnings, and keeps the coinbase proof the
/// *next* tenure's committed seed is checked against. All of that is in the same
/// ledger blob as the state root and all of it has to arrive or not arrive
/// together.
///
/// Scattering kills through a replay lands in one of these sometimes, which is
/// not the same as testing it. `replay-blocks` prints a line per sealed block, so
/// the parent watches for the block *before* a tenure start and kills as the child
/// begins the tenure — the same lesson as the wall-clock delay that put sixteen of
/// twenty kills before their run had begun, applied to a narrower target.
#[test]
fn a_kill_inside_a_tenure_transition_leaves_the_complete_parent() {
    if !fixtures().join("nakamoto/blocks").is_dir() {
        nano_conformance::skip_gate("the captured blocks are unavailable");
        return;
    }
    let tenure_starts = captured_tenure_starts();
    assert!(
        tenure_starts.len() > TENURE_KILLS,
        "the capture starts more tenures than this test interrupts: {} of them",
        tenure_starts.len()
    );

    let reference_directory = tempfile::tempdir().expect("a directory");
    let reference = reference(reference_directory.path());

    let killed = tempfile::tempdir().expect("a directory");
    // The import is not what is being tested and is far longer than a block.
    assert!(
        replay(killed.path(), 2),
        "the first run imports the checkpoint and replays two blocks"
    );

    let mut interrupted = Vec::new();
    for iteration in 0..TENURE_KILLS {
        // Killed as the child starts the block after this one, which is a tenure
        // start; the scatter spreads the signal through that block's execution
        // rather than always hitting the same instant of it.
        let sealed = kill_when(
            killed.path(),
            BLOCKS_PER_RUN,
            |height| tenure_starts.contains(&(height + 1)),
            Duration::from_micros(u64::try_from(iteration).unwrap_or(0) * 700),
        )
        .expect("the replay reached a block whose successor starts a tenure");
        interrupted.push(sealed + 1);
        survived(killed.path(), &reference);
    }

    // Under `--nocapture`: the tenures actually interrupted, which is the only
    // way to see that this hit eight different ones rather than one eight times.
    println!("tenure-start blocks interrupted: {interrupted:?}");
    assert_eq!(
        interrupted.iter().collect::<BTreeSet<_>>().len(),
        TENURE_KILLS,
        "each kill interrupted a different tenure transition: {interrupted:?}"
    );

    // And the survivor of all of them replays forward to what the uninterrupted
    // run reached, which is what says the interrupted tenures were left whole.
    let (chainstate, _) =
        durable_replay_chainstate(&fixtures(), killed.path()).expect("reopen at the end");
    let sealed =
        captured_blocks_sealed(&fixtures(), &chainstate).expect("read the number of sealed blocks");
    drop(chainstate);
    let remaining = u64::try_from(
        REFERENCE_BLOCKS
            .checked_sub(sealed)
            .expect("the killed runs stayed inside the reference"),
    )
    .expect("the remainder fits");
    assert!(
        replay(killed.path(), remaining),
        "the state that survived {TENURE_KILLS} interrupted tenures replays the rest"
    );
    let (mut chainstate, _) =
        durable_replay_chainstate(&fixtures(), killed.path()).expect("reopen at the end");
    let (expected_tip, expected_ledger, expected_root) = reference
        .last()
        .expect("the reference reached its last block");
    assert_eq!(
        chainstate.tip().expect("read the final tip"),
        Some(*expected_tip)
    );
    assert_eq!(
        chainstate
            .state_content_root(*expected_tip)
            .expect("read the final content root"),
        *expected_root
    );
    assert_eq!(ledger_of(&mut chainstate), *expected_ledger);
}
