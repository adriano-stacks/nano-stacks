//! nano's MARF against stacks-core's, root by root.
//!
//! This is the oracle the plan calls M7a, and it was missing. A state root is
//! consensus, and it is history-dependent three ways — back-pointer children
//! hash to the ancestor block hash, the root is a skip-list over ancestor roots,
//! and a node's pointers pack in the order its children were first written — so
//! a MARF can agree on every value and still seal a different root.
//!
//! Nothing else in the tree can catch that. Receipts match, values match, write
//! counts match, and the root is wrong.
//!
//! So this drives both implementations with the same scripts and compares the
//! root after **every** block: enough keys in one block to promote a node from
//! Node4 to Node16 to Node48 to Node256, where insertion-order packing starts to
//! matter; rewriting keys an ancestor block already holds, which is the
//! copy-on-write path and what a real block mostly does; and a fork, where two
//! blocks share a parent and the back-pointers have to resolve to the right
//! ancestor.

use nano_marf::{MarfValue, VersionedMarf};

use blockstack_lib::chainstate::stacks::index::MARFValue as CoreMarfValue;
use blockstack_lib::chainstate::stacks::index::marf::{MARF, MARFOpenOpts};
use stacks_common::types::chainstate::{StacksBlockId, TrieHash as CoreTrieHash};
use blockstack_lib::chainstate::stacks::index::ClarityMarfTrieId;

/// A deterministic pseudo-random source: tests must not vary run to run.
struct Rng(u64);

impl Rng {
    const fn next(&mut self) -> u64 {
        // xorshift64*, chosen for being short rather than for its statistics.
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

const fn block_id(n: u8) -> [u8; 32] {
    let mut bytes = [0; 32];
    bytes[0] = n;
    bytes
}

/// Open stacks-core's MARF over a temporary directory.
fn core_marf(directory: &std::path::Path) -> MARF<StacksBlockId> {
    MARF::from_path(
        directory.join("marf.sqlite").to_str().expect("a path"),
        MARFOpenOpts::default(),
    )
    .expect("open the stacks-core MARF")
}

/// Write one block of key/value pairs into both and return the two roots.
fn seal_both(
    nano: &mut VersionedMarf,
    core: &mut MARF<StacksBlockId>,
    parent: Option<[u8; 32]>,
    block: [u8; 32],
    pairs: &[(String, [u8; 40])],
) -> (String, String) {
    let core_parent = parent.map_or_else(StacksBlockId::sentinel, StacksBlockId);
    let keys: Vec<String> = pairs.iter().map(|(key, _)| key.clone()).collect();
    let values: Vec<CoreMarfValue> = pairs
        .iter()
        .map(|(_, value)| CoreMarfValue(*value))
        .collect();

    let core_root: CoreTrieHash = {
        let mut transaction = core.begin_tx().expect("stacks-core begins");
        transaction
            .begin(&core_parent, &StacksBlockId(block))
            .expect("stacks-core begins the block");
        transaction
            .insert_batch(&keys, values)
            .expect("stacks-core inserts");
        let root = transaction.seal().expect("stacks-core seals");
        transaction.commit().expect("stacks-core commits");
        root
    };

    nano.begin(parent, block).expect("nano begins the block");
    for (key, value) in pairs {
        nano.insert(key.as_bytes(), MarfValue::from_bytes(*value))
            .expect("nano inserts");
    }
    let nano_root = nano.seal().expect("nano seals");

    (hex::encode(nano_root.as_bytes()), core_root.to_hex())
}

/// `count` keys in one block, which is what promotes a trie node.
fn pairs(rng: &mut Rng, count: usize) -> Vec<(String, [u8; 40])> {
    (0..count)
        .map(|_| {
            let key = format!("vm::SP000000000000000000002Q6VF78.c::19::{:x}", rng.next());
            let mut value = [0u8; 40];
            value[..8].copy_from_slice(&rng.next().to_be_bytes());
            (key, value)
        })
        .collect()
}

/// Open both implementations over fresh directories.
fn both(
    nano_dir: &std::path::Path,
    core_dir: &std::path::Path,
) -> (VersionedMarf, MARF<StacksBlockId>) {
    (
        VersionedMarf::open(nano_dir.join("marf.sqlite")).expect("open nano's MARF"),
        core_marf(core_dir),
    )
}

#[test]
fn nano_and_stacks_core_seal_the_same_root_block_by_block() {
    let nano_dir = tempfile::tempdir().expect("a directory");
    let core_dir = tempfile::tempdir().expect("a directory");
    let (mut nano, mut core) = both(nano_dir.path(), core_dir.path());
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);

    let mut parent: Option<[u8; 32]> = None;
    // Block sizes chosen to cross every node promotion: 4, 16, 48, 256.
    for (index, count) in [1usize, 3, 5, 17, 49, 260, 2, 7].into_iter().enumerate() {
        let block = block_id(u8::try_from(index + 1).expect("a small index"));
        let written = pairs(&mut rng, count);
        let (nano_root, core_root) = seal_both(&mut nano, &mut core, parent, block, &written);
        assert_eq!(
            nano_root, core_root,
            "block {index} with {count} keys seals the same root"
        );
        parent = Some(block);
    }
}

#[test]
fn rewriting_an_ancestors_keys_seals_the_same_root() {
    let nano_dir = tempfile::tempdir().expect("a directory");
    let core_dir = tempfile::tempdir().expect("a directory");
    let (mut nano, mut core) = both(nano_dir.path(), core_dir.path());
    let mut rng = Rng(0x1234_5678_9abc_def0);

    // A block of keys, then blocks that overwrite them: every rewrite copies a
    // path out of an ancestor, which is where back-pointers are made.
    let original = pairs(&mut rng, 40);
    let (nano_root, core_root) = seal_both(&mut nano, &mut core, None, block_id(1), &original);
    assert_eq!(nano_root, core_root, "the first block agrees");

    let mut parent = block_id(1);
    for round in 2..=5u8 {
        let take = usize::from(round) * 7 % original.len() + 1;
        let mut rewritten: Vec<(String, [u8; 40])> = original
            .iter()
            .take(take)
            .map(|(key, _)| {
                let mut value = [0u8; 40];
                value[..8].copy_from_slice(&rng.next().to_be_bytes());
                (key.clone(), value)
            })
            .collect();
        rewritten.extend(pairs(&mut rng, usize::from(round)));
        let block = block_id(round);
        let (nano_root, core_root) =
            seal_both(&mut nano, &mut core, Some(parent), block, &rewritten);
        assert_eq!(
            nano_root, core_root,
            "block {round}, rewriting an ancestor's keys, agrees"
        );
        parent = block;
    }
}

#[test]
fn a_fork_seals_the_same_root_on_both_branches() {
    let nano_dir = tempfile::tempdir().expect("a directory");
    let core_dir = tempfile::tempdir().expect("a directory");
    let (mut nano, mut core) = both(nano_dir.path(), core_dir.path());
    let mut rng = Rng(0xfeed_face_dead_beef);

    let base = pairs(&mut rng, 24);
    let (nano_root, core_root) = seal_both(&mut nano, &mut core, None, block_id(1), &base);
    assert_eq!(nano_root, core_root, "the shared parent agrees");

    // Two children of the same parent, each rewriting some of its keys, then one
    // extended: the ancestor a back-pointer resolves to is the fork it stands on.
    for (index, child) in [block_id(2), block_id(3)].into_iter().enumerate() {
        let written: Vec<(String, [u8; 40])> = base
            .iter()
            .skip(index)
            .step_by(2)
            .map(|(key, _)| {
                let mut value = [0u8; 40];
                value[..8].copy_from_slice(&rng.next().to_be_bytes());
                (key.clone(), value)
            })
            .collect();
        let (nano_root, core_root) =
            seal_both(&mut nano, &mut core, Some(block_id(1)), child, &written);
        assert_eq!(nano_root, core_root, "fork branch {index} agrees");
    }

    let extension = pairs(&mut rng, 9);
    let (nano_root, core_root) = seal_both(
        &mut nano,
        &mut core,
        Some(block_id(2)),
        block_id(4),
        &extension,
    );
    assert_eq!(nano_root, core_root, "extending one branch agrees");
}

