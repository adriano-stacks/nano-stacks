//! A node that stops mid-catch-up has to resume owing exactly what it owed.
//!
//! Executing is not the only thing a block does: it seals a state root, it
//! moves the tenure accounting, and both have to survive the process. A run
//! that stops after every block and starts again has to reach the same root and
//! owe the same as one that never stopped, or a restart quietly forks the node
//! from the chain it was following.

use std::{fs, path::Path};

use nano_chainstate::{ChainState, TenureAccounting};
use nano_conformance::{FixtureManifest, FixtureMode, replay_into};

/// How many captured blocks each run replays.
const BLOCKS: u64 = 40;

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
///
/// A reopened directory recovers the ledger committed with its tip, exactly as
/// `nano-node` does; only a directory with nothing sealed takes the checkpoint's
/// accounting. Nothing is carried across by hand, which is the point: it used to
/// be, and the three fields nobody thought to carry were lost on every restart.
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
        <[u8; 32]>::try_from(hex::decode(value).expect("hexadecimal").as_slice())
            .expect("32 bytes")
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
    let recovered = chainstate
        .tip()
        .filter(|tip| *tip != source)
        .is_some_and(|tip| {
            chainstate
                .recover_ledger_at(tip)
                .expect("the ledger reads back")
        });
    if !recovered {
        let accounting = fs::read(checkpoint.join("native-effects.json"))
            .ok()
            .and_then(|contents| TenureAccounting::from_json(&contents).ok())
            .expect("the checkpoint carries accounting");
        *chainstate.accounting_mut() = accounting;
    }
    (chainstate, source)
}

/// What a run holds outside the MARF when it stops.
struct BesideTheMarf {
    owed: Vec<u8>,
    /// The tenure the tip belongs to, and the height that tenure started at.
    tenure: Option<(u32, u32)>,
    parent_tenure_proof: Option<[u8; 80]>,
    executed: Vec<[u8; 32]>,
}

fn beside_the_marf(chainstate: &mut ChainState) -> BesideTheMarf {
    BesideTheMarf {
        owed: chainstate
            .accounting_mut()
            .to_json()
            .expect("encode the accounting"),
        tenure: chainstate.tip().and_then(|tip| {
            chainstate
                .recorded_header(tip)
                .map(|header| (header.tenure_height, header.tenure_start_height))
        }),
        parent_tenure_proof: chainstate.parent_tenure_proof(),
        executed: chainstate.executed_blocks(),
    }
}

/// Field by field, because which one is missing names what the node cannot do.
fn resumed_with(chainstate: &mut ChainState, before: &BesideTheMarf) {
    assert_eq!(
        chainstate
            .accounting_mut()
            .to_json()
            .expect("encode the accounting"),
        before.owed,
        "a restart resumes owing exactly what it owed"
    );
    let (tenure_height, tenure_start) = before.tenure.expect("the break left a header behind");
    assert_eq!(
        chainstate.tenure_start_height(tenure_height),
        Some(tenure_start),
        "and knows where the tenure in flight started, which `get-tenure-info?` answers"
    );
    assert_eq!(
        chainstate.parent_tenure_proof(),
        before.parent_tenure_proof,
        "and holds the proof the next tenure's committed seed has to hash to"
    );
    assert_eq!(
        chainstate.executed_blocks(),
        before.executed,
        "and can walk a reorganization back as far as the run before it could"
    );
}

#[test]
fn a_replay_stopped_halfway_resumes_to_the_same_state() {
    let uninterrupted = tempfile::tempdir().expect("a directory");
    let restarted = tempfile::tempdir().expect("a directory");

    let (mut chainstate, source) = open(uninterrupted.path());
    let whole = replay_into(
        &mut chainstate,
        source,
        &fixtures(),
        manifest(BLOCKS),
        0,
        &mut |_, _| {},
    );
    assert_eq!(
        whole.completed, BLOCKS,
        "the uninterrupted run replays every block: {:?}",
        whole.first_divergence
    );
    let expected_tip = chainstate.tip();
    assert!(
        expected_tip.is_some_and(|tip| tip != source),
        "the run sealed a tip of its own, so the comparison below means something"
    );
    let expected_owed = chainstate
        .accounting_mut()
        .to_json()
        .expect("encode the accounting");
    drop(chainstate);

    // The same blocks, in two runs, with the state closed between them.
    let half = usize::try_from(BLOCKS / 2).expect("half fits");
    let (mut chainstate, source) = open(restarted.path());
    let first = replay_into(
        &mut chainstate,
        source,
        &fixtures(),
        manifest(BLOCKS / 2),
        0,
        &mut |_, _| {},
    );
    assert_eq!(
        first.completed,
        BLOCKS / 2,
        "the first run replays its half: {:?}",
        first.first_divergence
    );
    let at_the_break = beside_the_marf(&mut chainstate);
    drop(chainstate);

    let (mut chainstate, _) = open(restarted.path());
    // Nothing is handed over: the ledger came back with the tip.
    resumed_with(&mut chainstate, &at_the_break);
    let second = replay_into(
        &mut chainstate,
        source,
        &fixtures(),
        manifest(BLOCKS / 2),
        half,
        &mut |_, _| {},
    );
    assert_eq!(
        second.completed,
        BLOCKS / 2,
        "the resumed run replays the rest: {:?}",
        second.first_divergence
    );

    assert_eq!(
        chainstate.tip(),
        expected_tip,
        "a restart reaches the same sealed tip"
    );
    assert_eq!(
        chainstate
            .accounting_mut()
            .to_json()
            .expect("encode the accounting"),
        expected_owed,
        "a restart owes the same"
    );
}
