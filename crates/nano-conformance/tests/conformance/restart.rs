//! A node that stops mid-catch-up has to resume owing exactly what it owed.
//!
//! Executing is not the only thing a block does: it seals a state root, it
//! moves the tenure accounting, and both have to survive the process. A run
//! that stops after every block and starts again has to reach the same root and
//! owe the same as one that never stopped, or a restart quietly forks the node
//! from the chain it was following.
//!
//! Going *forward* over a restart is the easy half. The hard half is going
//! backwards over one: a Bitcoin reorganization or a heavier Stacks fork asks a
//! node to give up blocks it executed, and everything that says what to give up
//! — the executed suffix, the tenure heights it started, the coinbase proof the
//! next tenure's committed seed has to hash to — was memory until it was written
//! down with each block. So the retraction tests below run one scenario twice,
//! once in a single process and once across a restart, and demand the same
//! canonical chain out of both.

use std::{fs, path::Path};

use nano_chainstate::{ChainState, NakamotoBlock, TenureAccounting};
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

/// The canonical chain a retraction leaves, as one comparable value.
///
/// Field by field, because which field differs names what a node cannot do: an
/// executed suffix that came back short cannot be walked back over again, an
/// absent parent tenure proof cannot check the seed the replacement tenure
/// commits, and a tenure-start height that survived its tenure is a wrong answer
/// to `get-tenure-info?` for every block of the branch that follows.
#[derive(Debug, Eq, PartialEq)]
struct Canonical {
    executed: Vec<[u8; 32]>,
    accounting: Vec<u8>,
    parent_tenure_proof: Option<[u8; 80]>,
    /// Each tenure the replay saw: what the durable map answers for it, and
    /// what Clarity answers. Both, so that the two disagreeing is itself an
    /// inequality — they are two copies of one fact, and every bug in this area
    /// has been one of them moving without the other.
    tenure_starts: Vec<(u32, Option<u32>, Option<u32>)>,
}

fn canonical(chainstate: &mut ChainState, tenures: &[u32]) -> Canonical {
    Canonical {
        executed: chainstate.executed_blocks(),
        accounting: chainstate
            .accounting_mut()
            .to_json()
            .expect("encode the accounting"),
        parent_tenure_proof: chainstate.parent_tenure_proof(),
        tenure_starts: tenures
            .iter()
            .map(|tenure| {
                (
                    *tenure,
                    chainstate.tenure_start_height(*tenure),
                    chainstate.clarity_tenure_start_height(*tenure),
                )
            })
            .collect(),
    }
}

/// Replay the captured chain into `directory`, closing the process in between
/// when asked.
///
/// Every retraction test below runs this both ways. A chain built in one process
/// holds its executed suffix, tenure heights and parent tenure proof in memory;
/// a chain reopened from disk has to have read them back. The retraction that
/// follows cannot tell the two apart, and that is the property.
fn replayed(directory: &Path, across_a_restart: bool) -> (ChainState, Vec<NakamotoBlock>) {
    let (mut chainstate, source) = open(directory);
    let mut blocks = Vec::new();
    let progress = replay_into(
        &mut chainstate,
        source,
        &fixtures(),
        manifest(BLOCKS),
        0,
        &mut |block, _| blocks.push(block.clone()),
    );
    assert_eq!(
        progress.completed, BLOCKS,
        "the run reaches the fork point: {:?}",
        progress.first_divergence
    );
    if across_a_restart {
        drop(chainstate);
        let (reopened, _) = open(directory);
        return (reopened, blocks);
    }
    (chainstate, blocks)
}

/// Where the capture's last tenure begins, and the tenures it crosses.
///
/// The *last* tenure start rather than any: retracting it gives back a whole
/// tenure with blocks under it, and the tenure before it — which there has to be,
/// or there is no earlier coinbase proof for the retraction to fall back to and
/// the test would pass on a chain that never had one.
fn last_tenure_start(chainstate: &ChainState, blocks: &[NakamotoBlock]) -> (usize, Vec<u32>) {
    let start = blocks
        .iter()
        .rposition(nano_chainstate::starts_new_tenure)
        .expect("the capture starts a tenure");
    assert!(
        start > 0
            && blocks[..start]
                .iter()
                .any(nano_chainstate::starts_new_tenure),
        "the capture starts a second tenure, so a retraction has an earlier proof to fall back to"
    );
    let mut tenures: Vec<u32> = blocks
        .iter()
        .filter_map(|block| {
            chainstate
                .recorded_header(*block.block_id().as_bytes())
                .map(|header| header.tenure_height)
        })
        .collect();
    tenures.sort_unstable();
    tenures.dedup();
    assert!(
        tenures.len() > 1,
        "the replay crossed more than one tenure, so a retraction has one to give back"
    );
    (start, tenures)
}

/// Retract the last tenure the capture starts, as Bitcoin taking its sortition
/// away would, and answer with the canonical chain that is left.
fn bitcoin_reorganization(directory: &Path, across_a_restart: bool) -> Canonical {
    let (mut chainstate, blocks) = replayed(directory, across_a_restart);
    let (start, tenures) = last_tenure_start(&chainstate, &blocks);
    let held_before = chainstate
        .parent_tenure_proof()
        .expect("the replay accepted a tenure, so it holds that tenure's proof");
    let retracted_tenure = chainstate
        .recorded_header(*blocks[start].block_id().as_bytes())
        .expect("the tenure start was sealed with its header")
        .tenure_height;

    // A retracted sortition takes its whole tenure with it, which is the signal
    // `nano-sortition` hands over: the consensus hashes whose blocks are no
    // longer on the chain.
    let retraction = chainstate.retract(&nano_sortition::SortitionReorg {
        valid_ancestor: nano_sortition::SortitionSnapshot::genesis(
            0,
            nano_primitives::BitcoinHeaderHash::from_bytes([0; 32]),
        ),
        retracted: vec![nano_sortition::SortitionSnapshot {
            consensus_hash: blocks[start].header.consensus_hash,
            ..nano_sortition::SortitionSnapshot::genesis(
                1,
                nano_primitives::BitcoinHeaderHash::from_bytes([1; 32]),
            )
        }],
    });
    assert_eq!(
        retraction.discarded,
        blocks[start..]
            .iter()
            .map(|block| *block.block_id().as_bytes())
            .collect::<Vec<_>>(),
        "the invalidated tenure and everything after it is given back"
    );
    assert_eq!(
        retraction.resume_from,
        Some(*blocks[start - 1].block_id().as_bytes()),
        "and the chain stands on the last block that survived"
    );

    // The three things a retraction has to move, asserted directly rather than
    // only compared between the two runs: a bug that moves neither of them moves
    // neither of them in both runs, and the comparison would be satisfied.
    assert_eq!(
        chainstate.tenure_start_height(retracted_tenure),
        None,
        "the retracted tenure is not a tenure this chain started any more"
    );
    assert_eq!(
        chainstate.clarity_tenure_start_height(retracted_tenure),
        None,
        "and Clarity stops answering for it too — that map is not keyed by branch, \
         so an answer left in it belongs to a chain this node has left"
    );
    let held_after = chainstate
        .parent_tenure_proof()
        .expect("the tenure before the retracted one is still this chain's");
    assert_ne!(
        held_after, held_before,
        "and the proof the next tenure's seed is checked against went back with it: \
         left at the retracted tenure's, the honest tenure that replaces it commits a \
         seed that hashes to something else and is refused"
    );
    canonical(&mut chainstate, &tenures)
}

/// A restart must not change what a Bitcoin reorganization gives back.
#[test]
fn a_restart_before_a_bitcoin_reorganization_retracts_the_same_suffix() {
    let straight = tempfile::tempdir().expect("a directory");
    let restarted = tempfile::tempdir().expect("a directory");
    assert_eq!(
        bitcoin_reorganization(restarted.path(), true),
        bitcoin_reorganization(straight.path(), false),
        "a chain read back from disk retracts exactly what the process that built it would"
    );
}

/// A captured tenure-start block as a *different* miner would have produced it.
///
/// One second later, its tenure change naming this miner, and no signatures on
/// it: the state root and the miner signature are what assembling the block
/// produces, and the signers have not seen it. Everything else — the tenure it
/// claims, the parent it ends, the coinbase and its VRF proof — is the captured
/// block's, because those are what the retraction test is about.
fn competing_tenure(captured: &NakamotoBlock) -> (nano_crypto::StacksPrivateKey, NakamotoBlock) {
    let miner = nano_crypto::StacksPrivateKey::from_seed(b"a competing miner");
    let mut forked = captured.clone();
    forked.header.timestamp += 1;
    forked.header.signer_signatures.clear();
    let payload = forked
        .transactions
        .iter()
        .find_map(|transaction| match transaction.payload().data() {
            nano_codec::TransactionPayloadData::TenureChange(payload) => Some(payload.clone()),
            _ => None,
        })
        .expect("a tenure-start block carries a tenure change");
    forked.transactions[0] = nano_codec::Transaction::sign_standard(
        nano_codec::TransactionVersion::Testnet,
        forked.transactions[0].chain_id(),
        nano_codec::AnchorMode::OnChainOnly,
        &miner,
        0,
        0,
        nano_codec::TransactionPayloadData::TenureChange(nano_codec::TenureChangePayload {
            public_key_hash: nano_primitives::hash160(&miner.public_key().to_bytes_compressed()),
            ..payload
        }),
    )
    .expect("the tenure change signs");
    forked.header.transaction_merkle_root =
        nano_codec::transaction_merkle_root(&forked.transactions);
    (miner, forked)
}

/// Stand on the block before the last tenure and execute a competing tenure over
/// it, as a heavier Stacks fork would, answering with what the chain computed.
fn stacks_fork(
    directory: &Path,
    across_a_restart: bool,
) -> (Canonical, nano_marf::StateRoot, [u8; 32]) {
    let (mut chainstate, blocks) = replayed(directory, across_a_restart);
    let (start, tenures) = last_tenure_start(&chainstate, &blocks);
    let abandoned_tip = chainstate.tip().expect("the replay sealed a tip");
    let ancestor = *blocks[start - 1].block_id().as_bytes();

    // A Stacks fork names the ancestor directly: the sortitions still stand and
    // what changed is which chain of blocks is heaviest.
    let retraction = chainstate.retract_to(ancestor);
    assert_eq!(retraction.resume_from, Some(ancestor));
    assert_eq!(retraction.discarded.len(), blocks.len() - start);
    assert_eq!(
        chainstate.tip(),
        Some(abandoned_tip),
        "the MARF tip is still the block being abandoned: a state is addressed by the \
         block that sealed it, so nothing is deleted and a crash here resumes on the \
         abandoned branch and re-derives the switch from its peers"
    );

    // The competing tenure: the captured tenure-start block with one second added
    // to its timestamp, so it is a different block sealing a different state over
    // the same parent — a fork, rather than the same block replayed, which the
    // MARF refuses because that version already exists. Its committed seed is the
    // captured one, so it is accepted only if the retraction put back the proof of
    // the tenure *before* the one it gave up.
    //
    // *Mined* rather than edited, because the follow path now authenticates the
    // signatures a block carries ([[050]]): the timestamp is in both signature
    // preimages, so the captured miner signature over a changed header recovers
    // to some other key and the captured signer signatures belong to a block
    // hash that no longer exists. A candidate has neither yet — this node is
    // building it — so it goes in the way a competing miner's would, with a
    // tenure change naming the miner that signs the header.
    let (miner, forked) = competing_tenure(&blocks[start]);
    let view = forked.header.consensus_hash.to_string();
    let contexts =
        nano_conformance::captured_bitcoin_snapshots(&fixtures()).expect("captured contexts");
    let operations =
        nano_conformance::captured_bitcoin_operations(&fixtures()).expect("captured operations");
    let (forked, applied) = chainstate
        .assemble_nakamoto_block_with_bitcoin_operations(
            *contexts.get(&view).expect("the tenure's Bitcoin context"),
            operations
                .get(&view)
                .expect("the tenure's Bitcoin operations"),
            Some(ancestor),
            forked,
            &miner,
        )
        .expect("the competing tenure is accepted over the ancestor it stands on");
    let forked_id = *forked.block_id().as_bytes();
    assert_eq!(
        chainstate.executed_blocks().last(),
        Some(&forked_id),
        "and the chain now stands on it"
    );
    (
        canonical(&mut chainstate, &tenures),
        applied.execution.state_root,
        forked_id,
    )
}

/// A restart must not change what a chain computes on the other side of a fork.
///
/// The root is the strong part. A fork switch is followed by execution, and the
/// state that execution seals reads the tenure heights and the accounting the
/// retraction left — so a restart that recovered any of them differently seals a
/// different root here, with every receipt matching.
#[test]
fn a_restart_before_a_stacks_fork_reaches_the_same_canonical_state() {
    let straight = tempfile::tempdir().expect("a directory");
    let restarted = tempfile::tempdir().expect("a directory");
    let (whole, whole_root, whole_id) = stacks_fork(straight.path(), false);
    let (across, across_root, across_id) = stacks_fork(restarted.path(), true);
    assert_eq!(
        across_id, whole_id,
        "the competing block is the same block in both runs"
    );
    assert_eq!(
        across_root, whole_root,
        "and the state it seals over the retracted chain is the same state"
    );
    assert_eq!(
        across, whole,
        "as is everything the chain holds beside the MARF"
    );
}

/// A retraction writes nothing, and that is the design rather than a gap.
///
/// The window a crash could fall into is between a fork switch and the next
/// sealed block. There is nothing incoherent in it: a retraction only *reads* —
/// it stands on the ledger the surviving block already committed — so the disk
/// after one is byte-identical to the disk before it, and reopening gives back the
/// chain that was abandoned. `nano-node` then walks back for a block the network
/// still has and stands on that block's ledger, which is how the switch is
/// re-derived rather than remembered.
///
/// Making it durable would mean a second durable answer to "which chain am I on",
/// beside the sortitions and the peers that decided it. That is the thing this
/// group of tasks exists to remove, not to add — so this pins the absence.
#[test]
fn a_retraction_leaves_the_disk_where_it_found_it() {
    let directory = tempfile::tempdir().expect("a directory");
    let (mut chainstate, blocks) = replayed(directory.path(), false);
    let (start, _) = last_tenure_start(&chainstate, &blocks);
    let abandoned_tip = chainstate.tip().expect("the replay sealed a tip");
    let abandoned_chain = chainstate.executed_blocks();

    let retraction = chainstate.retract_to(*blocks[start - 1].block_id().as_bytes());
    assert!(!retraction.discarded.is_empty(), "something was given back");
    drop(chainstate);

    let (mut reopened, _) = open(directory.path());
    assert_eq!(
        reopened.tip(),
        Some(abandoned_tip),
        "the deepest sealed block is still the abandoned one: a retraction deletes no state"
    );
    assert!(
        reopened
            .recover_ledger_at(abandoned_tip)
            .expect("read back"),
        "and the ledger it committed is still there to stand on"
    );
    assert_eq!(
        reopened.executed_blocks(),
        abandoned_chain,
        "so a restart resumes on the abandoned chain, with the whole suffix a second \
         retraction needs to walk back over"
    );
}
