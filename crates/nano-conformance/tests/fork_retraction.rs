//! Standing on an ancestor again, when a heavier fork appears.
//!
//! A Stacks fork is not a Bitcoin reorganization: the sortitions still stand and
//! what changes is which chain of blocks is heaviest. Following one peer's tip
//! and refusing anything that does not extend it is obedience rather than fork
//! choice, and a peer that reorganizes past nano strands it
//! ([[027-choose-a-fork-instead-of-following-a-peer]]).
//!
//! Retracting is cheap because nothing is deleted: the MARF addresses a state by
//! the block that sealed it, so an abandoned branch merely stops being reachable
//! and is still there if the fork changes its mind. What has to be rewound is
//! everything kept *beside* the MARF — the executed chain and the accounting —
//! which is what this checks.

use std::{fs, path::Path};

use nano_chainstate::{ChainState, TenureAccounting};
use nano_conformance::{FixtureManifest, FixtureMode, replay_into};

/// How many captured blocks to execute before retracting.
const BLOCKS: u64 = 12;

fn fixtures() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

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
    let mut chainstate = ChainState::open_from_checkpoint(
        nano_primitives::Network::TESTNET,
        directory,
        checkpoint.join("marf.sqlite"),
        source,
        nano_primitives::TrieHash::from_bytes(decode(&field("published_state_index_root"))),
    )
    .expect("open the checkpoint");
    if let Some(accounting) = fs::read(checkpoint.join("native-effects.json"))
        .ok()
        .and_then(|contents| TenureAccounting::from_json(&contents).ok())
    {
        *chainstate.accounting_mut() = accounting;
    }
    (chainstate, source)
}

#[test]
fn retracting_to_an_ancestor_gives_back_everything_after_it() {
    let directory = tempfile::tempdir().expect("a directory");
    let (mut chainstate, source) = open(directory.path());
    let progress = replay_into(
        &mut chainstate,
        source,
        &fixtures(),
        FixtureManifest {
            mode: FixtureMode::Captured,
            replay_blocks: BLOCKS,
            receipts: true,
        },
        0,
        &mut |_, _| {},
    );
    assert_eq!(progress.completed, BLOCKS, "the run reaches the fork point");

    let executed = chainstate.executed_blocks();
    assert_eq!(executed.len(), usize::try_from(BLOCKS).expect("small"));
    let ancestor = executed[3];

    let retraction = chainstate.retract_to(ancestor);
    assert_eq!(
        retraction.resume_from,
        Some(ancestor),
        "the node stands on the ancestor it named"
    );
    assert_eq!(
        retraction.discarded.len(),
        executed.len() - 4,
        "everything after the ancestor is given back"
    );
    assert_eq!(
        chainstate.executed_blocks(),
        executed[..4].to_vec(),
        "and the executed chain ends there"
    );
}

#[test]
fn retracting_to_a_block_this_node_never_executed_does_nothing() {
    let directory = tempfile::tempdir().expect("a directory");
    let (mut chainstate, source) = open(directory.path());
    replay_into(
        &mut chainstate,
        source,
        &fixtures(),
        FixtureManifest {
            mode: FixtureMode::Captured,
            replay_blocks: 4,
            receipts: true,
        },
        0,
        &mut |_, _| {},
    );
    let executed = chainstate.executed_blocks();

    // A node cannot stand on a state it did not compute, so a peer naming an
    // ancestor from another chain must not be able to empty this one.
    let retraction = chainstate.retract_to([0x5c; 32]);
    assert!(
        retraction.discarded.is_empty(),
        "an unknown ancestor discards nothing"
    );
    assert_eq!(
        chainstate.executed_blocks(),
        executed,
        "and leaves the executed chain alone"
    );
}

#[test]
fn a_fork_point_names_the_last_block_of_its_tenure() {
    let directory = tempfile::tempdir().expect("a directory");
    let (mut chainstate, source) = open(directory.path());
    let mut tenures = Vec::new();
    replay_into(
        &mut chainstate,
        source,
        &fixtures(),
        FixtureManifest {
            mode: FixtureMode::Captured,
            replay_blocks: BLOCKS,
            receipts: true,
        },
        0,
        &mut |block, _| tenures.push((block.header.consensus_hash, *block.block_id().as_bytes())),
    );

    // A fork point is agreed in burn blocks and answers with a consensus hash;
    // a retraction has to name a Stacks block. Everything up to the last block
    // of that tenure is on both chains, and everything after it is disputed.
    let (consensus_hash, _) = tenures.first().copied().expect("a tenure");
    let last = tenures
        .iter()
        .rev()
        .find(|(hash, _)| *hash == consensus_hash)
        .map(|(_, id)| *id)
        .expect("a block in that tenure");
    assert_eq!(
        chainstate.last_block_of_tenure(consensus_hash),
        Some(last),
        "the bridge names the last block executed under the tenure"
    );

    let retraction = chainstate.retract_to(last);
    assert_eq!(retraction.resume_from, Some(last));
    assert!(
        chainstate
            .executed_blocks()
            .last()
            .is_some_and(|tip| *tip == last),
        "and the node stands there"
    );
}

#[test]
fn a_tenure_this_node_never_executed_names_nothing() {
    let directory = tempfile::tempdir().expect("a directory");
    let (chainstate, _) = open(directory.path());
    assert_eq!(
        chainstate.last_block_of_tenure(nano_primitives::ConsensusHash::from_bytes([0x7f; 20])),
        None,
        "a tenure from another chain names no block of this one"
    );
}

