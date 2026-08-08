//! One recorded write journal, two MARFs.
//!
//! `marf_lockstep` drives both implementations with *synthetic* scripts, and that
//! cannot decide the question this exists for. A mainnet block whose receipts,
//! balances, values and event counts all agree with the chain while its
//! `state_index_root` does not is either an execution difference the receipts
//! cannot express — a key written that the network does not write, or written in
//! another order — or a trie difference. Random keys cannot tell those apart,
//! because they exercise neither the keys a real block writes nor the ancestry a
//! real block stands on.
//!
//! So this takes the journal a *real* block execution made — every transaction's
//! Clarity writes and every native effect, in the order they were made, with the
//! writes of a rolled-back Clarity transaction removed exactly as the MARF
//! removes them — and replays that one journal through `nano-marf` and through
//! the pinned stacks-core MARF, comparing the root after **every** block.
//!
//! What that separates: if the two roots agree and neither matches the chain,
//! the journal is wrong and execution is at fault. If they disagree, the trie is.
//!
//! Four shapes, because a MARF's root depends on its history three ways:
//!
//! - plain inserts and **rewrites**, from a sentinel parent, where the keys are
//!   real Clarity keys and a block rewrites what earlier blocks wrote;
//! - a **fork**, two children of one parent and each extended, where a
//!   back-pointer must resolve to the branch it stands on;
//! - the **imported checkpoint**, where the ancestry arrives as back-pointers
//!   with `back_block` annotations rather than as blocks this process wrote.
//!
//! The last one is the shape a mainnet replay actually has, and the one no
//! synthetic script reaches. It is also the strongest assertion here: stacks-core
//! opens the captured checkpoint's own MARF, is handed nano's journal, and is
//! asked whether it seals the root the block header committed to.

use std::{
    fs,
    path::{Path, PathBuf},
};

use nano_conformance::{FixtureManifest, FixtureMode};
use nano_marf::{MarfValue, VersionedMarf};
use nano_vm::{BlockJournal, JournalWrite};

use blockstack_lib::chainstate::stacks::index::ClarityMarfTrieId;
use blockstack_lib::chainstate::stacks::index::MARFValue as CoreMarfValue;
use blockstack_lib::chainstate::stacks::index::marf::{MARF, MARFOpenOpts};
use blockstack_lib::chainstate::stacks::index::storage::TrieHashCalculationMode;
use blockstack_lib::chainstate::stacks::{MINER_BLOCK_CONSENSUS_HASH, MINER_BLOCK_HEADER_HASH};
use stacks_common::types::chainstate::StacksBlockId;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// How many captured blocks the journals are recorded from.
///
/// Bounded so this stays a per-commit gate rather than a full replay: the
/// scoreboard already replays all 340. What matters here is that the window
/// holds tenure starts, so the native effects — the coinbase, the matured miner
/// rewards, the unlocks, the SIP-031 mint — are in the journal beside the
/// transactions, and `journals_hold_every_kind_of_write` asserts they are.
const RECORDED_BLOCKS: u64 = 48;

/// Record the journal of each of the first `RECORDED_BLOCKS` captured blocks.
///
/// Every journal returned belongs to a block whose sealed root matched its
/// header's `state_index_root` — a block whose root does not match is a replay
/// divergence and never completes — so `journal.root` *is* the chain's root, and
/// an implementation handed this journal can be asked for it directly.
fn recorded_journals() -> Vec<BlockJournal> {
    let root = fixtures();
    let (mut chainstate, source) =
        nano_conformance::replay_chainstate(&root).expect("the captured checkpoint opens");
    chainstate.vm_mut().record_writes();
    let depth = nano_conformance::replay_into(
        &mut chainstate,
        source,
        &root,
        FixtureManifest {
            mode: FixtureMode::Captured,
            replay_blocks: RECORDED_BLOCKS,
            // The captured Hacknet blocks carry event-observer receipts, and the
            // replay reads the PoX unlock heights out of them. Recording a
            // journal from a run that got those wrong would record the wrong
            // journal.
            receipts: true,
        },
        0,
        &mut |_, _| {},
    );
    assert_eq!(
        depth.completed, RECORDED_BLOCKS,
        "the captured replay has to reach {RECORDED_BLOCKS} blocks for a journal to mean \
         anything: {:?}",
        depth.first_divergence
    );
    let journals = chainstate.vm_mut().take_journal();
    assert_eq!(
        u64::try_from(journals.len()).expect("a small count"),
        RECORDED_BLOCKS,
        "one journal per executed block"
    );
    journals
}

/// Open stacks-core's MARF over `path`, with external blobs so it can be handed
/// a captured checkpoint's own blob file.
fn core_marf(path: &Path) -> MARF<StacksBlockId> {
    MARF::from_path(
        path.to_str().expect("a UTF-8 path"),
        MARFOpenOpts::new(TrieHashCalculationMode::Deferred, true),
    )
    .expect("open the stacks-core MARF")
}

/// A reflink copy of a stacks-core MARF, and its blob file with it.
///
/// stacks-core opens a MARF **read-write**, whatever it is asked for, so a gate
/// pointed at the artifact it is meant to be checking will write to it. Both
/// tests below said "point it at a copy" in their own documentation and nothing
/// enforced it; on 2026-08-07 a run pointed `NANO_MAINNET_MARF` at
/// `mainnet-capture/chainstate/checkpoint-H/marf.sqlite` and stacks-core appended
/// 56,579 bytes to the 229 GB `marf.sqlite.blobs` beside it before its
/// transaction rolled back on a `UNIQUE constraint`. The index was untouched and
/// nothing referenced was lost, but nothing about that was by design.
///
/// So the copy is made here rather than asked for. `--reflink=always` because
/// these files are hundreds of gigabytes: on a filesystem that can share extents
/// it is instant and free, and on one that cannot the honest answer is to refuse
/// rather than silently duplicate a quarter of a terabyte. The returned directory
/// owns the copy and deletes it.
fn reflinked(path: &Path) -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().expect("a directory for the copy");
    let name = path.file_name().expect("the MARF has a file name");
    let copy = directory.path().join(name);
    for (from, to) in [
        (path.to_path_buf(), copy.clone()),
        (
            path.with_extension("sqlite.blobs"),
            copy.with_extension("sqlite.blobs"),
        ),
    ] {
        if !from.exists() {
            continue;
        }
        let status = std::process::Command::new("cp")
            .arg("--reflink=always")
            .arg(&from)
            .arg(&to)
            .status()
            .expect("run cp");
        assert!(
            status.success(),
            "cannot reflink {} -- this filesystem would need a real copy of it, and a gate \
             may not write to the artifact it was given",
            from.display()
        );
    }
    (directory, copy)
}

/// The state a followed Nakamoto block executes under, before it is sealed under
/// its real identifier.
///
/// stacks-core runs `append_block` against `MINER_BLOCK_CONSENSUS_HASH` /
/// `MINER_BLOCK_HEADER_HASH` and commits to the real block afterwards, because
/// the real identifier contains the state root being computed. nano does the
/// same, and the two constants have to agree: the MARF's height keys name this
/// identifier, so they are consensus.
fn miner_state_id() -> StacksBlockId {
    StacksBlockId::new(&MINER_BLOCK_CONSENSUS_HASH, &MINER_BLOCK_HEADER_HASH)
}

/// Feed one journal block to stacks-core, exactly as it executes one.
fn core_seal(
    core: &mut MARF<StacksBlockId>,
    parent: Option<[u8; 32]>,
    journal: &BlockJournal,
    sealed_as: [u8; 32],
) -> String {
    let keys: Vec<String> = journal
        .writes
        .iter()
        .map(|write| write.key.clone())
        .collect();
    let values: Vec<CoreMarfValue> = journal
        .writes
        .iter()
        .map(|write| CoreMarfValue(write.marf_value))
        .collect();
    let mut transaction = core.begin_tx().expect("stacks-core begins");
    transaction
        .begin(
            &parent.map_or_else(StacksBlockId::sentinel, StacksBlockId),
            &StacksBlockId(journal.executed_as),
        )
        .expect("stacks-core begins the block");
    transaction
        .insert_batch(&keys, values)
        .expect("stacks-core inserts");
    let root = transaction.seal().expect("stacks-core seals");
    transaction
        .commit_to(&StacksBlockId(sealed_as))
        .expect("stacks-core commits");
    root.to_hex()
}

/// Feed one journal block to nano, exactly as it executes one.
fn nano_seal(
    nano: &mut VersionedMarf,
    parent: Option<[u8; 32]>,
    journal: &BlockJournal,
    sealed_as: [u8; 32],
) -> String {
    nano.begin(parent, journal.executed_as)
        .expect("nano begins the block");
    for write in &journal.writes {
        nano.insert(
            write.key.as_bytes(),
            MarfValue::from_bytes(write.marf_value),
        )
        .expect("nano inserts");
    }
    hex::encode(nano.seal_to(sealed_as).expect("nano seals").as_bytes())
}

/// The identifier a journal block sealed under.
const fn sealed_as(journal: &BlockJournal) -> [u8; 32] {
    journal.sealed_as.expect("a recorded block sealed")
}

/// Two MARFs holding the same history, ready to be handed the same journal.
///
/// Declared in drop order: both MARFs hold connections into the directories
/// beneath them.
struct Pair {
    nano: VersionedMarf,
    core: MARF<StacksBlockId>,
    /// Where the journal's first block hangs, which is the checkpoint for an
    /// imported pair and nothing at all for a fresh one.
    source: Option<[u8; 32]>,
    _nano_dir: Option<tempfile::TempDir>,
    _core_dir: tempfile::TempDir,
}

/// Two empty MARFs.
///
/// The keys and values are a real chain's; the ancestry is this run's own, so
/// the two implementations are compared on nothing but the journal.
fn fresh_pair() -> Pair {
    let nano_dir = tempfile::tempdir().expect("a directory");
    let core_dir = tempfile::tempdir().expect("a directory");
    Pair {
        nano: VersionedMarf::open(nano_dir.path().join("marf.sqlite")).expect("open nano's MARF"),
        core: core_marf(&core_dir.path().join("marf.sqlite")),
        source: None,
        _nano_dir: Some(nano_dir),
        _core_dir: core_dir,
    }
}

/// Two MARFs standing on the captured checkpoint.
///
/// nano *imports* it: the ancestry arrives as a trie node graph whose formerly
/// inline children are back-pointers annotated with the block they resolve to,
/// rather than as blocks this process wrote. `import_checkpoint` refuses a graph
/// that does not hash to the published root, so the two start equal by
/// construction — and this is the only shape a mainnet replay ever has.
///
/// stacks-core gets the same checkpoint in its own storage, with the blocks the
/// capture kept *after* it removed: those are the very blocks the journal
/// replays, and stacks-core cannot seal a block its database already holds. nano
/// imports the untouched fixture, which it opens read-only.
fn checkpoint_pair() -> Pair {
    let fixtures = fixtures();
    let (source, published) =
        nano_conformance::checkpoint_state(&fixtures).expect("the checkpoint declares its state");
    let checkpoint = fixtures.join("chainstate/checkpoint-H");
    let nano = nano_marf::import_checkpoint(checkpoint.join("marf.sqlite"), source, published)
        .expect("the checkpoint imports");

    let core_dir = tempfile::tempdir().expect("a directory");
    let core_path = core_dir.path().join("marf.sqlite");
    trim_to_source(&checkpoint, &core_path, source);
    let mut core = core_marf(&core_path);
    assert_eq!(
        core.get_root_hash_at(&StacksBlockId(source))
            .expect("stacks-core holds the checkpoint's root")
            .to_hex(),
        hex::encode(published.as_bytes()),
        "both implementations start from the same root at the checkpoint"
    );
    Pair {
        nano,
        core,
        source: Some(source),
        _nano_dir: None,
        _core_dir: core_dir,
    }
}

/// Replay a whole journal into both implementations, root by root, returning the
/// roots the pair sealed.
fn lockstep(pair: &mut Pair, journals: &[BlockJournal]) -> Vec<String> {
    let mut parent = pair.source;
    let mut roots = Vec::with_capacity(journals.len());
    for (index, journal) in journals.iter().enumerate() {
        let sealed = sealed_as(journal);
        let core_root = core_seal(&mut pair.core, parent, journal, sealed);
        let nano_root = nano_seal(&mut pair.nano, parent, journal, sealed);
        assert_eq!(
            nano_root,
            core_root,
            "journal block {index} ({} writes) seals the same root in both MARFs",
            journal.writes.len()
        );
        roots.push(nano_root);
        parent = Some(sealed);
    }
    roots
}

#[test]
fn nanos_execution_state_is_the_one_stacks_core_appends_under() {
    assert_eq!(
        nano_chainstate::temporary_state_id(),
        *miner_state_id().as_bytes(),
        "a followed block's height keys name this identifier in both implementations"
    );
}

#[test]
fn journals_hold_every_kind_of_write() {
    let journals = recorded_journals();

    // The MARF's own height keys, which `begin` writes before anything else.
    for journal in &journals {
        assert_eq!(
            journal.height_keys.len(),
            5,
            "a block above height zero writes five height keys"
        );
        assert!(
            journal
                .height_keys
                .iter()
                .all(|write| write.value.is_none()),
            "a height key holds an encoded height or a block hash, not a hashed string"
        );
        assert_eq!(
            journal.executed_as,
            nano_chainstate::temporary_state_id(),
            "every followed block executes under the same temporary state"
        );
        assert!(journal.root.is_some(), "a recorded block sealed");
    }

    // Every write carries the value whose hash the trie holds, and the two agree:
    // a journal whose value did not hash to the leaf would replay a different
    // trie without saying so.
    for journal in &journals {
        for write in &journal.writes {
            let value = write.value.as_deref().expect("a Clarity write has a value");
            assert_eq!(
                MarfValue::from_value(value.as_bytes()).as_bytes(),
                &write.marf_value,
                "{} holds the hash of the value recorded beside it",
                write.key
            );
        }
    }

    // Native effects, which no transaction performs and which a journal taken
    // from the VM alone would miss: the block's own Clarity metadata, an STX
    // balance moved outside any contract call, and the liquid supply a coinbase
    // and a SIP-031 mint raise.
    let keys: Vec<&str> = journals
        .iter()
        .flat_map(|journal| journal.writes.iter().map(|write| write.key.as_str()))
        .collect();
    for expected in [
        // `setup_block_metadata`, which every block writes first.
        "_stx-data::clarity_storage::block_time",
        // `setup_block`'s tenure height, which only a tenure start moves.
        "_stx-data::tenure_height",
        // A balance and a nonce, which the fee debit and the coinbase credit
        // move outside any contract call.
        "vm-account::",
        // The coinbase and the SIP-031 mint, which raise the liquid supply.
        "_stx-data::ustx_liquid_supply",
    ] {
        assert!(
            keys.iter().any(|key| key.contains(expected)),
            "the journal holds a write to {expected}"
        );
    }

    // A rewrite of a key an ancestor block already holds is the copy-on-write
    // path, and it is what a real block mostly does. Asserted rather than
    // assumed, because a window without one would leave that path unexercised.
    let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut rewritten_across_blocks = 0usize;
    let mut rewritten_within_a_block = 0usize;
    for (index, journal) in journals.iter().enumerate() {
        let mut here: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for write in &journal.writes {
            if !here.insert(write.key.as_str()) {
                rewritten_within_a_block += 1;
            }
            if seen.insert(write.key.as_str(), index).is_some() {
                rewritten_across_blocks += 1;
            }
        }
    }
    assert!(
        rewritten_across_blocks > 0,
        "the recorded window rewrites keys an ancestor block holds"
    );
    assert!(
        rewritten_within_a_block > 0,
        "the recorded window writes some key twice inside one block"
    );

    println!(
        "{} journals, {} writes, {rewritten_across_blocks} rewrites across blocks, \
         {rewritten_within_a_block} within one",
        journals.len(),
        journals
            .iter()
            .map(|journal| journal.writes.len())
            .sum::<usize>()
    );
}

#[test]
fn one_journal_seals_the_same_root_in_nano_and_stacks_core() {
    let journals = recorded_journals();
    lockstep(&mut fresh_pair(), &journals);
}

#[test]
fn a_journal_forked_and_extended_seals_the_same_root() {
    let journals = recorded_journals();
    // Over the imported checkpoint, because that is where a fork actually
    // happens: the branches share an ancestry that arrived as back-pointers.
    let mut pair = checkpoint_pair();

    // A shared parent, built from the first few journals.
    let (base, rest) = journals.split_at(3);
    lockstep(&mut pair, base);
    let shared = sealed_as(&base[base.len() - 1]);
    let Pair { nano, core, .. } = &mut pair;

    // Two children of it, each a different real block's writes, then each
    // extended by the block after. A back-pointer in either branch has to
    // resolve to the ancestor of *that* branch, which is the only thing a fork
    // adds and the thing an imported checkpoint's `back_block` annotations
    // encode.
    for (branch, pair) in rest.chunks_exact(2).take(2).enumerate() {
        let mut parent = shared;
        for (depth, journal) in pair.iter().enumerate() {
            // A distinct identifier per branch: the same block cannot seal
            // twice, and the identifier is hashed into every back-pointer.
            let mut sealed = sealed_as(journal);
            sealed[0] ^= u8::try_from(branch + 1).expect("a small branch");
            let core_root = core_seal(core, Some(parent), journal, sealed);
            let nano_root = nano_seal(nano, Some(parent), journal, sealed);
            assert_eq!(
                nano_root, core_root,
                "fork branch {branch} at depth {depth} seals the same root"
            );
            parent = sealed;
        }
    }
}

#[test]
fn a_journal_over_an_imported_checkpoint_seals_the_chains_root() {
    let journals = recorded_journals();
    let roots = lockstep(&mut checkpoint_pair(), &journals);

    // Every root the pair sealed is the root the block header committed to.
    // Replay only completes a block whose root matched its header, so this is
    // the chain's own answer, and stacks-core's MARF — handed nano's journal and
    // nothing else, over the same imported ancestry — reproduces it. That is
    // what makes the journal *complete* rather than merely self-consistent.
    for (index, (journal, root)) in journals.iter().zip(&roots).enumerate() {
        assert_eq!(
            *root,
            hex::encode(journal.root.expect("a recorded block sealed")),
            "journal block {index} seals the root its header committed to"
        );
    }
    println!(
        "{} journal blocks replayed over the imported checkpoint, both MARFs sealing the chain's \
         own roots",
        journals.len()
    );
}

/// Copy a captured checkpoint into `destination` and drop everything the capture
/// kept after `source`, so stacks-core can extend it.
fn trim_to_source(checkpoint: &Path, destination: &Path, source: [u8; 32]) {
    for suffix in ["", ".blobs"] {
        let name = format!("marf.sqlite{suffix}");
        fs::copy(checkpoint.join(&name), destination.with_file_name(&name))
            .unwrap_or_else(|error| panic!("copy {name}: {error}"));
    }
    let connection = rusqlite::Connection::open(destination).expect("open the copy");
    // Written back as a rollback journal: nano's importer opens the fixture with
    // `immutable=1`, which ignores a write-ahead log, and a trimmed database
    // whose deletions lived only in a WAL would be read as untrimmed.
    connection
        .pragma_update(None, "journal_mode", "DELETE")
        .expect("no write-ahead log");
    connection
        .execute(
            "DELETE FROM marf_data WHERE block_id > \
             (SELECT block_id FROM marf_data WHERE block_hash = ?1)",
            rusqlite::params![hex::encode(source)],
        )
        .expect("trim the later blocks");
    connection
        .execute_batch("DELETE FROM block_extension_locks")
        .expect("release any extension lock");
    drop(connection);
}

/// One key per write, keeping the value the block ended up with.
///
/// In the order each key was *first* written, which is the order the trie packs
/// pointers in — so this has to seal the block's own root: writing a key twice
/// leaves the same leaf as writing it once with the final value.
fn deduplicated(writes: &[JournalWrite]) -> Vec<JournalWrite> {
    let mut order: Vec<&str> = Vec::new();
    let mut last: std::collections::HashMap<&str, &JournalWrite> = std::collections::HashMap::new();
    for write in writes {
        if last.insert(write.key.as_str(), write).is_none() {
            order.push(write.key.as_str());
        }
    }
    order.into_iter().map(|key| last[key].clone()).collect()
}

/// Replay `journals[..index]` over the imported checkpoint, then seal `index`
/// with `writes` instead of its own, and return the two roots.
fn seal_variant(
    journals: &[BlockJournal],
    index: usize,
    writes: Vec<JournalWrite>,
) -> (String, String) {
    let mut pair = checkpoint_pair();
    lockstep(&mut pair, &journals[..index]);

    let mut variant = journals[index].clone();
    variant.writes = writes;
    let parent = if index > 0 {
        Some(sealed_as(&journals[index - 1]))
    } else {
        pair.source
    };
    let sealed = sealed_as(&variant);
    (
        nano_seal(&mut pair.nano, parent, &variant, sealed),
        core_seal(&mut pair.core, parent, &variant, sealed),
    )
}

/// The busiest recorded block, where a perturbation has the most room to hide.
fn busiest(journals: &[BlockJournal]) -> usize {
    (0..journals.len())
        .max_by_key(|index| deduplicated(&journals[*index].writes).len())
        .expect("a recorded block")
}

/// Whether the oracle can see a write the chain does not make, or one it does.
///
/// A comparison between two implementations proves nothing unless it would fail
/// when they disagreed. Two perturbations that a receipt cannot express and that
/// the whole task turns on: a write dropped, and a value changed.
#[test]
fn the_oracle_sees_a_dropped_write_and_a_changed_value() {
    let journals = recorded_journals();
    let index = busiest(&journals);
    let distinct = deduplicated(&journals[index].writes);
    assert!(distinct.len() > 1, "the busiest block writes several keys");

    // Every key once, with the value the block ended with, in the order each was
    // first written. The same trie: writing a key twice leaves the same leaf as
    // writing it once with the final value, and the pointer slot it occupies was
    // taken by the first write either way.
    let (faithful, core_faithful) = seal_variant(&journals, index, distinct.clone());
    assert_eq!(
        faithful, core_faithful,
        "deduplicating a block's rewrites agrees in both implementations"
    );
    assert_eq!(
        faithful,
        hex::encode(journals[index].root.expect("a recorded block sealed")),
        "and seals the root the chain sealed"
    );

    // One write dropped. "A write the network did not make is a leaf in the trie
    // it does not have" — and so is one it made that this node did not.
    let mut short = distinct.clone();
    short.pop();
    let (nano_short, core_short) = seal_variant(&journals, index, short);
    assert_eq!(nano_short, core_short, "a short journal agrees on its root");
    assert_ne!(
        nano_short, faithful,
        "dropping one write seals a different root, in both"
    );

    // One value changed, nothing else. The key set, the ordering and the count
    // are all untouched, which is what makes this the perturbation a write count
    // or an event count cannot see.
    let mut altered = distinct;
    let last = altered.len() - 1;
    altered[last].marf_value[0] ^= 1;
    let (nano_altered, core_altered) = seal_variant(&journals, index, altered);
    assert_eq!(nano_altered, core_altered, "an altered value agrees");
    assert_ne!(
        nano_altered, faithful,
        "changing one value seals a different root, in both"
    );
}

/// Two keys whose MARF paths share their first byte, so both land under one
/// child of the chr-indexed root and the node beneath it packs them in
/// insertion order.
fn colliding_keys() -> (String, String) {
    let key = |n: u32| format!("vm::SP000000000000000000002Q6VF78.c::19::{n}");
    let mut first_byte: std::collections::HashMap<u8, u32> = std::collections::HashMap::new();
    for n in 0..4096 {
        let byte = nano_marf::key_path(key(n).as_bytes()).as_bytes()[0];
        if let Some(earlier) = first_byte.insert(byte, n) {
            return (key(earlier), key(n));
        }
    }
    panic!("no two of 4096 keys share a first path byte");
}

/// Whether ordering is visible at all, and when it is not.
///
/// The answer turned out to be a finding rather than a formality. Reversing the
/// writes of a *real* block leaves its root unchanged — in both implementations,
/// identically — and the reason is structural: the root of a MARF is a `Node256`,
/// which is indexed by path byte rather than packed in insertion order, so two
/// writes can only be ordered with respect to each other if they descend into the
/// same node. Every write in the window's busiest block starts at a distinct path
/// byte, so none of them do.
///
/// Ordering is consensus for writes that *share a path prefix*, and for those it
/// is consensus absolutely, which the constructed pair below asserts. It is also
/// consensus only for a slot a block *creates*: a rewrite lands in a pointer slot
/// whichever block first wrote the key already packed, and 302 of this window's
/// 324 writes are rewrites.
///
/// So for a root that differs while every receipt matches, this narrows the
/// suspects rather than confirming one: unless the block introduces keys that
/// collide on a path prefix, write order is not the explanation, and a missing or
/// extra key is.
#[test]
fn ordering_is_consensus_for_writes_that_share_a_path_prefix() {
    let journals = recorded_journals();
    let index = busiest(&journals);
    let distinct = deduplicated(&journals[index].writes);

    let (faithful, _) = seal_variant(&journals, index, distinct.clone());
    let mut reversed = distinct.clone();
    reversed.reverse();
    let (nano_reversed, core_reversed) = seal_variant(&journals, index, reversed);
    assert_eq!(
        nano_reversed, core_reversed,
        "a reordering agrees in both implementations, whatever it does to the root"
    );

    // Whether any two of the block's writes descend into the same node at all.
    // Where none do, the reordering cannot change the root, and asserting that it
    // does not is asserting the mechanism rather than an accident.
    let mut bytes: std::collections::HashSet<u8> = std::collections::HashSet::new();
    let shares_a_prefix = distinct
        .iter()
        .any(|write| !bytes.insert(nano_marf::key_path(write.key.as_bytes()).as_bytes()[0]));
    assert!(
        !shares_a_prefix,
        "block {index}'s writes all start at distinct path bytes"
    );
    assert_eq!(
        nano_reversed, faithful,
        "and so reordering them cannot reach a different node than the chr-indexed root"
    );

    // Where a block does write keys that collide, order is consensus.
    let (first, second) = colliding_keys();
    assert_eq!(
        nano_marf::key_path(first.as_bytes()).as_bytes()[0],
        nano_marf::key_path(second.as_bytes()).as_bytes()[0],
        "the pair shares a first path byte"
    );
    let pair = |keys: [&str; 2]| {
        let block = BlockJournal {
            executed_as: nano_chainstate::temporary_state_id(),
            parent: None,
            height: 0,
            height_keys: Vec::new(),
            writes: keys
                .iter()
                .enumerate()
                .map(|(index, key)| JournalWrite {
                    key: (*key).to_owned(),
                    value: None,
                    marf_value: *MarfValue::from_u32(u32::try_from(index).expect("a small index"))
                        .as_bytes(),
                })
                .collect(),
            sealed_as: Some([9; 32]),
            root: None,
        };
        let mut pair = fresh_pair();
        (
            nano_seal(&mut pair.nano, None, &block, [9; 32]),
            core_seal(&mut pair.core, None, &block, [9; 32]),
        )
    };
    let (nano_forward, core_forward) = pair([&first, &second]);
    let (nano_backward, core_backward) = pair([&second, &first]);
    assert_eq!(nano_forward, core_forward, "one order agrees in both");
    assert_eq!(nano_backward, core_backward, "the other order agrees too");
    assert_ne!(
        nano_forward, nano_backward,
        "two keys sharing a path prefix seal a different root in each order, so the oracle can \
         see an ordering fault where an ordering fault is possible"
    );
}

/// Whether stacks-core can open a *mainnet* checkpoint's MARF at all.
///
/// [[037]] records this as the one piece of tooling that would unlock a general
/// mainnet oracle, and records it as closed: "an open path that seeks in a
/// `SQLite` blob where the trie is in the flat file beside it, read-only and
/// `external_blobs` alike". The offline pair above says otherwise for a captured
/// Hacknet trie, and the difference is one flag. `MARFOpenOpts::default()` leaves
/// `external_blobs` off, so stacks-core reads `marf_data.data` — which a
/// `stacks-core-marf-sqlite-v2` capture leaves empty, because the trie is in
/// `marf.sqlite.blobs` beside it. Turning it on is what makes the same file open.
///
/// Point it at a **copy**: stacks-core opens a MARF read-write, blob file and all.
///
/// ```text
/// NANO_MAINNET_MARF=/copy/checkpoint-H/marf.sqlite \
/// NANO_MAINNET_BLOCK=<64 hex chars> NANO_MAINNET_ROOT=<64 hex chars> \
///   cargo test -p nano-conformance write_journal -- --nocapture
/// ```
#[test]
fn stacks_core_opens_a_mainnet_checkpoint_with_external_blobs() {
    let (Ok(path), Ok(block), Ok(root)) = (
        std::env::var("NANO_MAINNET_MARF"),
        std::env::var("NANO_MAINNET_BLOCK"),
        std::env::var("NANO_MAINNET_ROOT"),
    ) else {
        nano_conformance::skip_gate(
            "NANO_MAINNET_MARF, NANO_MAINNET_BLOCK and NANO_MAINNET_ROOT are needed",
        );
        return;
    };
    let block = StacksBlockId::from_hex(&block).expect("the block identifier is hexadecimal");
    // Never the path handed in: see `reflinked`.
    let (_copy, path) = reflinked(Path::new(&path));
    let mut core = core_marf(&path);
    let opened = core
        .get_root_hash_at(&block)
        .expect("stacks-core reads the checkpoint's root")
        .to_hex();
    println!("stacks-core reads {block} as {opened}");
    assert_eq!(
        opened, root,
        "stacks-core opens the mainnet checkpoint and agrees with its published root"
    );
}

/// Drop every block a MARF holds above `parent`, on a copy.
///
/// The journal replays the blocks that come after its parent, and a MARF that
/// already holds them refuses the insert. Doing this by hand against a
/// hand-made copy is what the test used to ask for; both halves are automatic
/// now, which is one fewer way to point a writing gate at a real archive.
fn truncate_above(marf: &Path, parent: [u8; 32]) {
    let connection = rusqlite::Connection::open(marf).expect("open the copy");
    let removed = connection
        .execute(
            "DELETE FROM marf_data WHERE block_id > (SELECT block_id FROM marf_data \
             WHERE block_hash = ?1)",
            rusqlite::params![StacksBlockId(parent).to_hex()],
        )
        .expect("the copy is writable");
    println!("dropped {removed} block(s) above the journal's parent from the copy");
}

/// Read back the journal `replay-blocks` writes.
///
/// Only the `write` lines and the 40 bytes on them are needed to drive a MARF;
/// the value beside them is for reading. `marf` lines are skipped, because every
/// implementation writes its own height keys and feeding them back would write
/// them twice.
fn read_journal(text: &str) -> Vec<BlockJournal> {
    let mut blocks: Vec<BlockJournal> = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields.as_slice() {
            [
                "block",
                sealed,
                "height",
                height,
                "parent",
                parent,
                "root",
                root,
            ] => {
                let decode =
                    |value: &str| <[u8; 32]>::try_from(hex::decode(value).ok()?.as_slice()).ok();
                blocks.push(BlockJournal {
                    executed_as: nano_chainstate::temporary_state_id(),
                    parent: decode(parent),
                    height: height.parse().expect("a height"),
                    height_keys: Vec::new(),
                    writes: Vec::new(),
                    sealed_as: decode(sealed),
                    root: decode(root),
                });
            }
            ["write", key, "=", value, ..] => {
                let marf_value =
                    <[u8; 40]>::try_from(hex::decode(value).expect("hexadecimal").as_slice())
                        .expect("40 bytes");
                blocks
                    .last_mut()
                    .expect("a write belongs to a block")
                    .writes
                    .push(JournalWrite {
                        key: (*key).to_owned(),
                        value: None,
                        marf_value,
                    });
            }
            _ => {}
        }
    }
    blocks
}

/// A recorded **mainnet** journal, through the pinned stacks-core MARF, over the
/// mainnet checkpoint it was recorded against.
///
/// This is the offline pair above with Hacknet's 4 MB checkpoint swapped for
/// mainnet's 153 GB one, and it is the whole point of the exercise: a mainnet
/// root divergence with matching receipts stands on an *imported* ancestry, and
/// only stacks-core reading that same ancestry can say whether the journal or the
/// trie is at fault.
///
/// Take the journal with `replay-blocks <capture> <state-dir> <n> <journal>` from
/// a pristine parent, then point this at a writable copy of the checkpoint. The
/// copy has to be trimmed to the journal's own first parent: stacks-core cannot
/// seal a block its database already holds, and an archive's checkpoint carries
/// the blocks that came after it — including the ones the journal replays. Which
/// block that is comes from the journal rather than from an argument, so the two
/// cannot be pointed at different heights.
///
/// ```text
/// sqlite3 copy/marf.sqlite "DELETE FROM marf_data WHERE block_id >
///   (SELECT block_id FROM marf_data WHERE block_hash = '<the journal's parent>')"
///
/// NANO_MAINNET_MARF=/copy/marf.sqlite NANO_MAINNET_JOURNAL=/tmp/journal.txt \
///   cargo test -p nano-conformance write_journal -- --nocapture
/// ```
#[test]
fn a_recorded_mainnet_journal_seals_the_chains_root() {
    let (Ok(path), Ok(journal)) = (
        std::env::var("NANO_MAINNET_MARF"),
        std::env::var("NANO_MAINNET_JOURNAL"),
    ) else {
        nano_conformance::skip_gate("NANO_MAINNET_MARF and NANO_MAINNET_JOURNAL are needed");
        return;
    };
    let journals = read_journal(&fs::read_to_string(&journal).expect("read the journal"));
    assert!(!journals.is_empty(), "the journal holds at least one block");

    // Never the path handed in: this test *writes* the journal's blocks into the
    // MARF it is given. See `reflinked`.
    let (_copy, path) = reflinked(Path::new(&path));
    let mut parent = journals[0]
        .parent
        .expect("the journal names its own parent");
    // And the archive already holds the blocks about to be replayed, so inserting
    // them fails on `UNIQUE constraint failed: marf_data.block_hash`. This test
    // used to tell an operator to run the delete by hand against a copy they had
    // also made by hand; the copy is private now, so it does it. Everything above
    // the journal's parent is exactly what the journal is about to write.
    truncate_above(&path, parent);
    let mut core = core_marf(&path);
    println!(
        "stacks-core reads the journal's parent as {}",
        core.get_root_hash_at(&StacksBlockId(parent))
            .expect("stacks-core holds the block the journal stands on")
            .to_hex()
    );
    for (index, journal) in journals.iter().enumerate() {
        assert_eq!(
            journal.parent,
            Some(parent),
            "journal block {index} stands on the block before it"
        );
        let sealed = sealed_as(journal);
        let core_root = core_seal(&mut core, Some(parent), journal, sealed);
        let chain_root = hex::encode(journal.root.expect("a recorded block sealed"));
        println!(
            "mainnet {} at height {}: stacks-core {core_root}, the chain {chain_root}",
            hex::encode(sealed),
            journal.height
        );
        assert_eq!(
            core_root, chain_root,
            "stacks-core, handed nano's journal for mainnet block {} and nothing else, seals the \
             root its header committed to",
            journal.height
        );
        parent = sealed;
    }
}

/// Every journal write, keyed and ordered as recorded, for one block.
///
/// Kept because a divergence is read rather than guessed: printing this beside
/// the network's own writes is what named the wrong value at 8,665,699.
#[allow(dead_code)]
fn render(journal: &BlockJournal) -> String {
    use std::fmt::Write as _;
    let mut rendered = String::new();
    for write in journal.height_keys.iter().chain(&journal.writes) {
        let JournalWrite {
            key,
            value,
            marf_value,
        } = write;
        let _ = writeln!(
            rendered,
            "{key} = {} {}",
            hex::encode(marf_value),
            value.as_deref().unwrap_or("<encoded>")
        );
    }
    rendered
}
