//! Durable replay under real POSIX storage failures.
//!
//! `libfiu` interposes the process's actual libc calls, so this exercises `SQLite`
//! and the MARF without adding a production failpoint. The fault is enabled only
//! after one block seals; import and startup therefore stay clean, and the next
//! block is interrupted inside ordinary execution or commit.

use std::{
    env,
    fmt::Write as _,
    fs,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use nano_conformance::{captured_blocks_sealed, durable_replay_chainstate};

const TARGET_BLOCKS: u64 = 32;
const PATIENCE: Duration = Duration::from_secs(30);
const MAX_DURABILITY_CALLS: usize = 64;
// Linux errno values, used only by the Linux libfiu preload module.
const EIO: i32 = 5;
const ENOSPC: i32 = 28;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn command_exists(name: &str) -> bool {
    Command::new(name)
        .arg("-h")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn replay(directory: &Path, blocks: u64) -> ExitStatus {
    Command::new(env!("CARGO_BIN_EXE_replay-blocks"))
        .arg(fixtures())
        .arg(directory)
        .arg(blocks.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .expect("run the replay")
}

#[derive(Debug, Eq, PartialEq)]
struct Fingerprint {
    tip: [u8; 32],
    root: Option<nano_primitives::TrieHash>,
    accounting: Vec<u8>,
    executed: Vec<[u8; 32]>,
    parent_tenure_proof: Option<[u8; 80]>,
    tenure_start: Option<u32>,
    /// The content hash of the decision record the surviving tip committed
    /// under. A block is meant to be absent or fully visible *with its record*,
    /// so every scenario in this file compares this too: a tip that reopens with
    /// its root and accounting intact but no record is a partially visible
    /// block, which is the shape this file exists to refuse.
    decision: Option<[u8; 32]>,
}

/// The clean reference, which every injected run is compared against.
///
/// Asserting the record is present here is what keeps the comparison from being
/// vacuous: without it, a decision record that never landed on either side
/// would compare equal as `None` and the criterion would look satisfied.
fn reference_fingerprint(directory: &Path) -> Fingerprint {
    let clean = fingerprint(directory);
    assert!(
        clean.decision.is_some(),
        "the clean replay's tip must carry a decision record, or comparing it proves nothing"
    );
    clean
}

fn fingerprint(directory: &Path) -> Fingerprint {
    let (mut chainstate, _) = durable_replay_chainstate(&fixtures(), directory)
        .expect("the state left by the storage failure reopens");
    let tip = chainstate
        .tip()
        .expect("read the surviving tip")
        .expect("the replay sealed a block");
    let tenure_start = chainstate
        .recorded_header(tip)
        .and_then(|header| chainstate.tenure_start_height(header.tenure_height));
    Fingerprint {
        tip,
        root: chainstate
            .state_content_root(tip)
            .expect("read the surviving root"),
        accounting: chainstate
            .accounting_mut()
            .to_json()
            .expect("encode the surviving accounting"),
        executed: chainstate.executed_blocks(),
        parent_tenure_proof: chainstate.parent_tenure_proof(),
        tenure_start,
        decision: chainstate
            .decision_record(tip)
            .expect("read the surviving decision record")
            .map(|sealed| sealed.content_hash),
    }
}

fn finish(directory: &Path) -> Fingerprint {
    let (chainstate, _) = durable_replay_chainstate(&fixtures(), directory)
        .expect("the interrupted state reopens before replaying onward");
    let sealed = captured_blocks_sealed(&fixtures(), &chainstate)
        .expect("count the captured blocks that survived");
    drop(chainstate);
    let remaining = TARGET_BLOCKS
        .checked_sub(u64::try_from(sealed).expect("the sealed count fits"))
        .expect("the injected replay stayed within the reference");
    assert!(
        replay(directory, remaining).success(),
        "the recovered state replays the remaining {remaining} blocks"
    );
    fingerprint(directory)
}

fn wait(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(status) = child.try_wait().expect("poll the faulted replay") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            child.wait().expect("reap the timed-out replay");
            panic!("the injected replay did not stop after {PATIENCE:?}");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

struct Fault {
    name: &'static str,
    errno: Option<i32>,
    label: &'static str,
    must_stop: bool,
}

fn inject(directory: &Path, fault: &Fault) -> String {
    assert!(replay(directory, 2).success(), "prepare the faulted state");
    let control = tempfile::tempdir().expect("a control directory");
    let control_base = control.path().join("fiu");
    let mut child = Command::new("fiu-run")
        .arg("-x")
        .arg("-f")
        .arg(&control_base)
        .arg(env!("CARGO_BIN_EXE_replay-blocks"))
        .arg(fixtures())
        .arg(directory)
        .arg((TARGET_BLOCKS - 2).to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the replay under libfiu");
    let stdout = child.stdout.take().expect("the replay stdout is piped");
    let mut lines = BufReader::new(stdout);
    let mut first = String::new();
    assert!(
        lines.read_line(&mut first).expect("read replay progress") > 0,
        "the replay reached a clean commit before injecting {}",
        fault.label
    );
    assert!(
        first.starts_with("sealed "),
        "the first progress line names a sealed block: {first:?}"
    );

    let mut enable = format!("enable name={}", fault.name);
    if let Some(errno) = fault.errno {
        let _ = write!(enable, ",failinfo={errno}");
    }
    let control_status = Command::new("fiu-ctrl")
        .arg("-f")
        .arg(&control_base)
        .arg("-c")
        .arg(&enable)
        .arg(child.id().to_string())
        .status()
        .expect("control the faulted replay");
    assert!(control_status.success(), "enable {}", fault.label);

    let status = wait(&mut child);
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("the replay stderr is piped")
        .read_to_string(&mut stderr)
        .expect("read the replay error");
    drop(lines);
    if fault.must_stop {
        assert!(!status.success(), "{} was exercised", fault.label);
    }
    assert!(
        !stderr.contains("invalid transaction"),
        "{} was not misreported as a consensus-invalid transaction: {stderr}",
        fault.label
    );
    assert!(
        !stderr.contains("panicked at"),
        "{} returns an error instead of panicking: {stderr}",
        fault.label
    );
    stderr
}

fn set_read_only(path: &Path, read_only: bool) {
    let metadata = fs::metadata(path).expect("read state permissions");
    let mode = if metadata.is_dir() {
        if read_only { 0o555 } else { 0o755 }
    } else if read_only {
        0o444
    } else {
        0o644
    };
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set state permissions");
    if metadata.is_dir() {
        for entry in fs::read_dir(path).expect("read the state directory") {
            set_read_only(&entry.expect("read a state entry").path(), read_only);
        }
    }
}

fn prepare_state(blocks: u64) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("a damaged state directory");
    assert!(
        replay(directory.path(), blocks).success(),
        "prepare the state to damage"
    );
    directory
}

fn replay_with_nth_fault(
    directory: &Path,
    point: &str,
    at: usize,
    report: &Path,
) -> (ExitStatus, String) {
    let preload = env::var_os("NANO_FIU_NTH_PRELOAD").expect("the nth-failure helper is set");
    let preload_dir =
        PathBuf::from(env::var_os("NANO_FIU_PRELOAD_DIR").expect("the libfiu path is set"));
    // Preload constructors run in reverse order; libfiu must initialize first.
    let libraries = format!(
        "{}:{}",
        Path::new(&preload).display(),
        preload_dir.join("fiu_posix_preload.so").display()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_replay-blocks"))
        .arg(fixtures())
        .arg(directory)
        .arg("1")
        .env("LD_PRELOAD", libraries)
        .env("NANO_FIU_POINT", point)
        .env("NANO_FIU_AT", at.to_string())
        .env("NANO_FIU_REPORT", report)
        .output()
        .expect("run one block with an indexed fault");
    (
        output.status,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn sweep_durability_point(expected: &Fingerprint, point: &str) -> usize {
    for at in 1..=MAX_DURABILITY_CALLS {
        let directory = prepare_state(2);
        let report = directory.path().join("fiu-hit");
        let (status, stderr) = replay_with_nth_fault(directory.path(), point, at, &report);
        if !report.exists() {
            assert!(
                status.success(),
                "the replay failed before reaching {point} call {at}: {stderr}"
            );
            assert_eq!(
                finish(directory.path()),
                *expected,
                "a run past the final {point} call reaches the clean state"
            );
            return at - 1;
        }
        assert!(
            !stderr.contains("invalid transaction"),
            "{point} call {at} was misreported as consensus-invalid: {stderr}"
        );
        assert!(
            !stderr.contains("panicked at"),
            "{point} call {at} returned an error instead of panicking: {stderr}"
        );
        assert_eq!(
            finish(directory.path()),
            *expected,
            "{point} call {at} leaves only a complete replay prefix"
        );
    }
    panic!("{point} has more than {MAX_DURABILITY_CALLS} calls in one replayed block");
}

fn corrupt_first_page(path: &Path) {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open the database to corrupt");
    let mut byte = [0; 1];
    file.read_exact(&mut byte)
        .expect("read the database header");
    byte[0] ^= 0xff;
    file.seek(SeekFrom::Start(0)).expect("rewind the database");
    file.write_all(&byte).expect("corrupt the first page");
    file.sync_all().expect("persist the corrupt page");
}

fn truncate_half(path: &Path) {
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open the database to truncate");
    let length = file.metadata().expect("read database length").len();
    file.set_len(length / 2).expect("truncate the database");
    file.sync_all().expect("persist the truncation");
}

fn assert_damage_is_refused(directory: &Path, label: &str) {
    match durable_replay_chainstate(&fixtures(), directory) {
        Ok(_) => panic!("{label} was accepted as a complete state"),
        Err(error) => assert!(
            !error.contains("invalid transaction"),
            "{label} was misreported as a consensus-invalid transaction: {error}"
        ),
    }
}

#[test]
fn storage_failures_leave_only_a_complete_replay_prefix() {
    let unavailable = if !cfg!(target_os = "linux") {
        Some("libfiu storage injection requires Linux")
    } else if !fixtures().join("nakamoto/blocks").is_dir() {
        Some("the captured blocks are unavailable")
    } else if !command_exists("fiu-run") || !command_exists("fiu-ctrl") {
        Some("fiu-run and fiu-ctrl are unavailable")
    } else {
        None
    };
    if let Some(reason) = unavailable {
        nano_conformance::skip_gate(reason);
        return;
    }

    let reference = tempfile::tempdir().expect("a reference directory");
    assert!(
        replay(reference.path(), TARGET_BLOCKS).success(),
        "the uninterrupted reference reaches {TARGET_BLOCKS} blocks"
    );
    let expected = reference_fingerprint(reference.path());

    let faults = [
        Fault {
            name: "posix/io/rw/pread",
            errno: Some(EIO),
            label: "EIO while reading",
            must_stop: true,
        },
        Fault {
            name: "posix/io/rw/read/reduce",
            errno: None,
            label: "a short read",
            must_stop: false,
        },
        Fault {
            name: "posix/io/rw/pwrite",
            errno: Some(EIO),
            label: "EIO while writing",
            must_stop: true,
        },
        Fault {
            name: "posix/io/rw/pwrite",
            errno: Some(ENOSPC),
            label: "ENOSPC while writing",
            must_stop: true,
        },
        Fault {
            name: "posix/io/rw/pwrite/reduce",
            errno: None,
            label: "a short write",
            must_stop: false,
        },
        Fault {
            name: "posix/io/sync/fsync",
            errno: Some(EIO),
            label: "fsync failure",
            must_stop: true,
        },
    ];
    for fault in faults {
        let directory = tempfile::tempdir().expect("a faulted state directory");
        let error = inject(directory.path(), &fault);
        assert_eq!(
            finish(directory.path()),
            expected,
            "{} leaves a complete replay prefix; child error: {error}",
            fault.label
        );
    }

    let directory = tempfile::tempdir().expect("a read-only state directory");
    assert!(
        replay(directory.path(), 2).success(),
        "prepare the read-only state"
    );
    set_read_only(directory.path(), true);
    let status = replay(directory.path(), 1);
    set_read_only(directory.path(), false);
    assert!(!status.success(), "a read-only state refuses another block");
    assert_eq!(
        finish(directory.path()),
        expected,
        "a read-only attempt leaves the complete parent"
    );

    for database in ["marf.sqlite", "clarity.sqlite"] {
        let directory = prepare_state(2);
        corrupt_first_page(&directory.path().join(database));
        assert_damage_is_refused(directory.path(), &format!("a corrupt {database} page"));

        let directory = prepare_state(2);
        truncate_half(&directory.path().join(database));
        assert_damage_is_refused(directory.path(), &format!("a torn {database}"));
    }
}

#[test]
fn mismatched_store_generations_expose_only_a_complete_state() {
    if !fixtures().join("nakamoto/blocks").is_dir() {
        nano_conformance::skip_gate("the captured blocks are unavailable");
        return;
    }

    let reference = prepare_state(TARGET_BLOCKS);
    let expected = reference_fingerprint(reference.path());

    let parent = prepare_state(2);
    let parent_before = fingerprint(parent.path());
    let child = prepare_state(3);
    fs::copy(
        child.path().join("marf.sqlite"),
        parent.path().join("marf.sqlite"),
    )
    .expect("copy the child MARF beside the parent side store");
    assert_eq!(
        fingerprint(parent.path()),
        parent_before,
        "a MARF ahead of its side store exposes exactly the complete parent"
    );
    assert_eq!(
        finish(parent.path()),
        expected,
        "the recovered parent replays to the clean state"
    );

    // The side store is committed first. If a file-by-file snapshot sees it one
    // block ahead of the MARF, those rows are unreachable and the complete
    // parent remains the only visible state.
    let parent = prepare_state(2);
    let parent_before = fingerprint(parent.path());
    let child = prepare_state(3);
    fs::copy(
        child.path().join("clarity.sqlite"),
        parent.path().join("clarity.sqlite"),
    )
    .expect("copy the child side store beside the parent MARF");
    assert_eq!(
        fingerprint(parent.path()),
        parent_before,
        "an ahead side store exposes exactly the complete parent"
    );
    assert_eq!(
        finish(parent.path()),
        expected,
        "the safe half of a file-generation tear replays to the clean state"
    );
}

#[test]
fn every_durable_commit_boundary_exposes_only_a_complete_state() {
    let unavailable = if !cfg!(target_os = "linux") {
        Some("indexed libfiu injection requires Linux")
    } else if !fixtures().join("nakamoto/blocks").is_dir() {
        Some("the captured blocks are unavailable")
    } else if env::var_os("NANO_FIU_NTH_PRELOAD").is_none()
        || env::var_os("NANO_FIU_PRELOAD_DIR").is_none()
    {
        Some("the indexed libfiu helper is unavailable")
    } else {
        None
    };
    if let Some(reason) = unavailable {
        nano_conformance::skip_gate(reason);
        return;
    }

    let reference = prepare_state(TARGET_BLOCKS);
    let expected = reference_fingerprint(reference.path());
    let fsyncs = sweep_durability_point(&expected, "posix/io/sync/fsync");
    let fdatasyncs = sweep_durability_point(&expected, "posix/io/sync/fdatasync");
    let renames = sweep_durability_point(&expected, "posix/io/dir/rename");
    assert!(
        fsyncs + fdatasyncs >= 2,
        "both SQLite commit boundaries issue durable syncs"
    );
    println!("exercised {fsyncs} fsync, {fdatasyncs} fdatasync and {renames} rename boundaries");
}
