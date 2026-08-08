//! What a block from before the checkpoint answers Clarity.
//!
//! A checkpoint carries the trie for all of history. Until the header export it
//! carried no headers, so every `get-stacks-block-info?`, `get-tenure-info?` and
//! epoch lookup below the anchor was answered either not at all — which stops
//! the node — or by a peer fetch that filled five fields of thirteen and left
//! zeros in the rest. A zero is a Clarity answer, so a contract reading the miner
//! address, the block reward or the burn spend of a pre-checkpoint block got a
//! confident wrong number with no error attached.
//!
//! This checks the export against the only oracle that settles it:
//! stacks-core's own `HeadersDB`, reading the same archive the export was taken
//! from, field by field and block by block. Where stacks-core answers a value,
//! nano must answer the same value; where stacks-core answers nothing, nano must
//! answer nothing too.
//!
//! The oracle needs a 56 GB archive, so it is gated:
//!
//! ```text
//! NANO_MAINNET_ARCHIVE=/path/mainnet/chainstate/vm/index.sqlite \
//! NANO_MAINNET_CAPTURE=/path/mainnet-capture \
//!   cargo test -p nano-conformance --test conformance pre_checkpoint_headers -- --nocapture
//! ```
//!
//! What it leaves behind is replayable with neither: the answers stacks-core
//! gave, and the slice of the export they were checked against, are written into
//! `fixtures/mainnet/headers/` and checked offline by the second test here.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use blockstack_lib::chainstate::stacks::index::marf::{MARF, MARFOpenOpts};
use blockstack_lib::chainstate::stacks::index::storage::{
    TrieFileStorage, TrieHashCalculationMode,
};
use clarity::vm::database::HeadersDB;
use nano_primitives::Network;
use nano_vm::{HeaderFields, HeaderKnowledge};
use stacks_common::types::StacksEpochId;
use stacks_common::types::chainstate::StacksBlockId;

/// The heights sampled, spread over the whole ancestry the checkpoint carries.
///
/// Deliberately not a window: the fields that were being zero-filled are
/// resolved at a block's *tenure start*, and a window inside one tenure would
/// check one tenure's worth of them. These cross the 2.x/Nakamoto boundary, the
/// first blocks of the chain, and the anchor itself.
const SAMPLED_HEIGHTS: [u64; 16] = [
    1, 2, 100, 1_000, 10_000, 100_000, 150_000, 153_000, 200_000, 1_000_000, 4_000_000, 8_000_000,
    8_600_000, 8_660_000, 8_665_000, 8_665_600,
];

/// One block's thirteen answers, as a chain gave them.
///
/// `None` is a real answer here — "the chain has nothing for this field either" —
/// which is why every one is optional rather than defaulted. An epoch 2.x block
/// genuinely has no Nakamoto timestamp, and a tenure whose reward has not matured
/// genuinely has no reward.
#[derive(Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
struct Answers {
    block_id: String,
    stacks_height: u64,
    burn_header_hash: Option<String>,
    burn_block_height: Option<u32>,
    burn_block_time: Option<u64>,
    stacks_block_time: Option<u64>,
    block_header_hash: Option<String>,
    consensus_hash: Option<String>,
    vrf_seed: Option<String>,
    miner_address: Option<String>,
    burn_spend_total: Option<String>,
    burn_spend_winner: Option<String>,
    block_reward: Option<String>,
    /// The tenure height the header claims, and the Stacks height a chain answers
    /// for it. Clarity reaches every tenure field through that mapping, so the
    /// mapping is what has to match rather than the number in isolation.
    tenure_height: Option<u32>,
    stacks_height_for_tenure_height: Option<u32>,
}

/// Ask a `HeadersDB` every question Clarity can ask about a block.
fn answers(
    headers: &dyn HeadersDB,
    block: &StacksBlockId,
    tip: &StacksBlockId,
    stacks_height: u64,
    tenure_height: Option<u32>,
) -> Answers {
    // The epoch each read is made under is the epoch of the block being read, as
    // `ClarityDatabase::get_stacks_epoch_for_block` supplies it: it decides which
    // header table stacks-core looks in, so passing the wrong one asks about a
    // different chain.
    let epoch = if stacks_height >= NAKAMOTO_FIRST_HEIGHT {
        StacksEpochId::Epoch30
    } else {
        StacksEpochId::Epoch21
    };
    Answers {
        block_id: hex::encode(block.as_bytes()),
        stacks_height,
        burn_header_hash: headers
            .get_burn_header_hash_for_block(block)
            .map(|hash| hex::encode(hash.as_bytes())),
        burn_block_height: headers.get_burn_block_height_for_block(block),
        burn_block_time: headers.get_burn_block_time_for_block(block, Some(&epoch)),
        stacks_block_time: headers.get_stacks_block_time_for_block(block),
        block_header_hash: headers
            .get_stacks_block_header_hash_for_block(block, &epoch)
            .map(|hash| hex::encode(hash.as_bytes())),
        consensus_hash: headers
            .get_consensus_hash_for_block(block, &epoch)
            .map(|hash| hex::encode(hash.as_bytes())),
        vrf_seed: headers
            .get_vrf_seed_for_block(block, tip, &epoch)
            .map(|seed| hex::encode(seed.as_bytes())),
        miner_address: headers
            .get_miner_address(block, tip, &epoch)
            .map(|address| address.to_string()),
        burn_spend_total: headers
            .get_burnchain_tokens_spent_for_block(block, tip, &epoch)
            .map(|spent| spent.to_string()),
        burn_spend_winner: headers
            .get_burnchain_tokens_spent_for_winning_block(block, tip, &epoch)
            .map(|spent| spent.to_string()),
        block_reward: headers
            .get_tokens_earned_for_block(block, tip, &epoch)
            .map(|earned| earned.to_string()),
        tenure_height,
        stacks_height_for_tenure_height: tenure_height
            .and_then(|height| headers.get_stacks_height_for_tenure_height(tip, height)),
    }
}

/// Where mainnet's first Nakamoto block sits, which is where the header table a
/// read lands in changes.
const NAKAMOTO_FIRST_HEIGHT: u64 = 153_811;

/// The archive's `index.sqlite`, when this run has one.
fn archive() -> Option<PathBuf> {
    env::var("NANO_MAINNET_ARCHIVE").ok().map(PathBuf::from)
}

/// The captured mainnet checkpoint, which is where the export lives.
fn capture() -> Option<PathBuf> {
    env::var("NANO_MAINNET_CAPTURE").ok().map(PathBuf::from)
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/mainnet/headers")
}

/// A sampled block's export row, imported into a state of its own.
///
/// Rows rather than the file, because the export of a mainnet ancestry is two
/// gigabytes and a fixture is a few kilobytes: the slice is what makes the
/// offline half of this possible.
fn slice_export(export: &Path, into: &Path, blocks: &[Vec<u8>]) -> Result<(), rusqlite::Error> {
    let connection = rusqlite::Connection::open(into)?;
    connection.execute_batch(nano_vm::HEADER_EXPORT_SCHEMA)?;
    connection.execute(
        "ATTACH DATABASE ?1 AS full",
        rusqlite::params![format!("file:{}?mode=ro", export.display())],
    )?;
    for block in blocks {
        connection.execute(
            "INSERT OR REPLACE INTO exported_header \
             SELECT * FROM full.exported_header WHERE block_id = ?1",
            rusqlite::params![block.as_slice()],
        )?;
    }
    connection.execute_batch("DETACH DATABASE full")?;
    Ok(())
}

/// The block identifiers at the sampled heights, from the export itself.
fn sampled_blocks(export: &Path) -> Vec<(Vec<u8>, u64, Option<u32>)> {
    let connection = rusqlite::Connection::open_with_flags(
        format!("file:{}?mode=ro", export.display()),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("open the header export");
    let heights = SAMPLED_HEIGHTS
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut statement = connection
        .prepare(&format!(
            "SELECT block_id, stacks_height, tenure_height, known FROM exported_header \
             WHERE stacks_height IN ({heights})"
        ))
        .expect("query the header export");
    let mut found: Vec<(Vec<u8>, u64, Option<u32>)> = statement
        .query_map([], |row| {
            let known = HeaderFields::from_bits(row.get::<_, u16>(3)?);
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, u64>(1)?,
                known
                    .contains(HeaderFields::TENURE_HEIGHT)
                    .then(|| row.get::<_, u32>(2))
                    .transpose()?,
            ))
        })
        .expect("read the header export")
        .collect::<Result<_, _>>()
        .expect("read the header export");
    found.sort_by_key(|(_, height, _)| *height);
    found
}

/// Import an export slice into a fresh state and ask it what Clarity would ask.
fn nano_answers(
    export: &Path,
    blocks: &[(Vec<u8>, u64, Option<u32>)],
    tip: &StacksBlockId,
) -> Vec<Answers> {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let mut vm = nano_vm::Vm::open(Network::MAINNET, directory.path().join("chainstate"))
        .expect("open a state");
    let imported = vm.import_block_headers(export).expect("import the headers");
    assert_eq!(imported, blocks.len(), "every sampled header is imported");
    blocks
        .iter()
        .map(|(block, height, tenure_height)| {
            let id = StacksBlockId(<[u8; 32]>::try_from(block.as_slice()).expect("32 bytes"));
            answers(vm.chain_context(), &id, tip, *height, *tenure_height)
        })
        .collect()
}

/// nano answers a pre-checkpoint block exactly what stacks-core answers.
///
/// The whole point of the export in one assertion, and it is field by field
/// rather than header by header: a header compared as a value would compare the
/// zero sitting where an unknown field is against a real answer and pass.
#[test]
fn every_exported_field_matches_stacks_cores_own_headers_db() {
    let (Some(archive), Some(capture)) = (archive(), capture()) else {
        nano_conformance::skip_gate(
            "NANO_MAINNET_ARCHIVE and NANO_MAINNET_CAPTURE must name an archive and a capture",
        );
        return;
    };
    let export = capture
        .join("chainstate/checkpoint-H")
        .join(nano_vm::HEADER_EXPORT_FILE);
    if !export.exists() {
        nano_conformance::skip_gate("the capture carries no header export yet");
        return;
    }
    let blocks = sampled_blocks(&export);
    assert!(
        blocks.len() >= SAMPLED_HEIGHTS.len(),
        "every sampled height is in the export: {} of {}",
        blocks.len(),
        SAMPLED_HEIGHTS.len()
    );

    // Read-only, because this archive is the oracle and a MARF that opens for
    // writing rewrites what it was asked to check.
    // `external_blobs`, because a mainnet chainstate keeps its tries in
    // `index.sqlite.blobs` beside the database: opened without it, the fork index
    // reads the blob column instead and reports the archive corrupt.
    let storage = TrieFileStorage::<StacksBlockId>::open_readonly(
        archive.to_str().expect("a path"),
        MARFOpenOpts::new(TrieHashCalculationMode::Deferred, true),
    )
    .expect("open the archive");
    let reference = MARF::from_storage(storage);
    let tip = tip_of(&capture);

    let expected: Vec<Answers> = blocks
        .iter()
        .map(|(block, height, tenure_height)| {
            let id = StacksBlockId(<[u8; 32]>::try_from(block.as_slice()).expect("32 bytes"));
            answers(&reference, &id, &tip, *height, *tenure_height)
        })
        .collect();

    let slice = fixture_dir().join("pre-checkpoint-export.sqlite");
    fs::create_dir_all(fixture_dir()).expect("a fixture directory");
    let staged = tempfile::tempdir().expect("a temporary directory");
    let staged_slice = staged.path().join("slice.sqlite");
    let ids: Vec<Vec<u8>> = blocks.iter().map(|(block, _, _)| block.clone()).collect();
    slice_export(&export, &staged_slice, &ids).expect("slice the export");
    let found = nano_answers(&staged_slice, &blocks, &tip);

    for (nano, chain) in found.iter().zip(expected.iter()) {
        assert_eq!(
            nano, chain,
            "block {} at height {} answers differently than the chain",
            chain.block_id, chain.stacks_height
        );
    }

    // Written only once the comparison passed, so the offline fixture can never
    // record an answer this run disagreed with.
    fs::copy(&staged_slice, &slice).expect("keep the export slice");
    fs::write(
        fixture_dir().join("stacks-core-answers.json"),
        serde_json::to_vec_pretty(&expected).expect("serialize the answers"),
    )
    .expect("keep the answers");
}

/// The same answers, replayed with no archive and no capture.
#[test]
fn the_export_answers_offline_what_stacks_core_answered() {
    let answers_path = fixture_dir().join("stacks-core-answers.json");
    let export = fixture_dir().join("pre-checkpoint-export.sqlite");
    if !answers_path.exists() || !export.exists() {
        nano_conformance::skip_gate(
            "the header fixtures are not captured yet; run the gated test above",
        );
        return;
    }
    let expected: Vec<Answers> =
        serde_json::from_slice(&fs::read(&answers_path).expect("read the answers"))
            .expect("parse the answers");
    let blocks: Vec<(Vec<u8>, u64, Option<u32>)> = expected
        .iter()
        .map(|answer| {
            (
                hex::decode(&answer.block_id).expect("hexadecimal"),
                answer.stacks_height,
                answer.tenure_height,
            )
        })
        .collect();
    // The tip only reaches stacks-core's fork index, which is not consulted here:
    // nano resolves a tenure field from the header it holds, so any identifier
    // does. Using the deepest sampled block says so.
    let tip = StacksBlockId(
        <[u8; 32]>::try_from(blocks.last().expect("a sample").0.as_slice()).expect("32 bytes"),
    );
    let found = nano_answers(&export, &blocks, &tip);
    assert_eq!(
        found, expected,
        "the export answers what the chain answered"
    );
}

/// A block the export never carried is distinguishable from one off this fork.
///
/// Both answered `none` before, and they are opposite faults: one is a header to
/// fetch, the other is a bug in whatever resolved a height to that identifier.
#[test]
fn a_header_never_carried_is_not_a_block_that_does_not_exist() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let mut vm = nano_vm::Vm::open(Network::MAINNET, directory.path().join("chainstate"))
        .expect("open a state");
    assert_eq!(
        vm.header_knowledge([9; 32]),
        HeaderKnowledge::Absent,
        "a block with no state and no header is not on this fork at all"
    );
    vm.begin_block(None, [1; 32]).expect("begin a block");
    vm.commit_block(
        [1; 32],
        &nano_vm::BlockCommit {
            header: nano_vm::BlockHeader {
                tenure_height: 7,
                tenure_start_height: 3,
                ..nano_vm::BlockHeader::default()
            },
            ledger: Vec::new(),
        },
    )
    .expect("seal it");
    let HeaderKnowledge::Held(known) = vm.header_knowledge([1; 32]) else {
        panic!("a sealed block holds a header");
    };
    assert!(
        known.is_complete(),
        "a block this node executed answers every field"
    );
    // A block in the index whose header was never written: what a checkpoint
    // without an export leaves behind for all of history.
    vm.begin_block(Some([1; 32]), [2; 32])
        .expect("begin a block");
    vm.seal_block().expect("seal it");
    assert_eq!(
        vm.header_knowledge([2; 32]),
        HeaderKnowledge::NeverCarried,
        "a block this node holds state for but no header for is a header to fetch"
    );
}

/// The tip a capture's checkpoint was taken at, which is the fork every sampled
/// block is an ancestor of.
fn tip_of(capture: &Path) -> StacksBlockId {
    let manifest = fs::read_to_string(capture.join("provenance.toml")).expect("a provenance file");
    let id = manifest
        .lines()
        .find_map(|line| line.strip_prefix("checkpoint_state_id = "))
        .map(|value| value.trim().trim_matches('"'))
        .expect("a checkpoint state identifier");
    StacksBlockId(
        <[u8; 32]>::try_from(hex::decode(id).expect("hexadecimal").as_slice()).expect("32 bytes"),
    )
}
