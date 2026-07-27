#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    fmt::Write,
    fs,
    path::{Path, PathBuf},
};

use nano_chainstate::{BitcoinBlockContext, ChainState, NakamotoBlock};
use nano_node::{BaselineSource, ReplayFailure, replay_one};
use nano_primitives::TrieHash;
use serde::Deserialize;

/// The minimum metadata needed to make replay depth visible before fixture
/// capture is available.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FixtureManifest {
    pub mode: FixtureMode,
    pub replay_blocks: u64,
}

/// Whether the fixture directory contains an explicit M0 baseline or a real
/// capture that downstream conformance tests may consume.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FixtureMode {
    Baseline,
    Captured,
}

#[derive(Deserialize)]
struct CapturedBitcoinSnapshot {
    block_height: u64,
    burn_header_hash: String,
    consensus_hash: String,
}

#[derive(Deserialize)]
struct CapturedBlockEvent {
    #[serde(rename = "pox_v1_unlock_height")]
    v1: u32,
    #[serde(rename = "pox_v2_unlock_height")]
    v2: u32,
    #[serde(rename = "pox_v3_unlock_height")]
    v3: u32,
    #[serde(rename = "pox_v4_unlock_height")]
    v4: u32,
    transactions: Vec<CapturedTransaction>,
    events: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct CapturedTransaction {
    txid: String,
    status: String,
    raw_result: String,
    execution_cost: CapturedExecutionCost,
}

#[derive(Deserialize)]
struct CapturedExecutionCost {
    read_count: u64,
    read_length: u64,
    runtime: u64,
    write_count: u64,
    write_length: u64,
}

impl FixtureManifest {
    /// Read the intentionally tiny, dependency-free fixture manifest.
    pub fn load(path: &Path) -> Result<Self, ManifestError> {
        let contents = fs::read_to_string(path).map_err(ManifestError::Read)?;
        let value = contents
            .lines()
            .map(str::trim)
            .find_map(|line| line.strip_prefix("replay_blocks ="))
            .ok_or(ManifestError::MissingReplayBlocks)?
            .trim()
            .parse()
            .map_err(ManifestError::InvalidReplayBlocks)?;
        let mode = contents
            .lines()
            .map(str::trim)
            .find_map(|line| line.strip_prefix("mode ="))
            .map(str::trim)
            .ok_or(ManifestError::MissingMode)?;
        let mode = match mode.trim_matches('"') {
            "baseline" => FixtureMode::Baseline,
            "captured" => FixtureMode::Captured,
            other => return Err(ManifestError::InvalidMode(other.to_owned())),
        };
        Ok(Self {
            mode,
            replay_blocks: value,
        })
    }
}

#[derive(Debug)]
pub enum ManifestError {
    Read(std::io::Error),
    MissingReplayBlocks,
    InvalidReplayBlocks(std::num::ParseIntError),
    MissingMode,
    InvalidMode(String),
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(error) => write!(formatter, "could not read fixture manifest: {error}"),
            Self::MissingReplayBlocks => {
                formatter.write_str("fixture manifest is missing replay_blocks")
            }
            Self::InvalidReplayBlocks(error) => write!(formatter, "invalid replay_blocks: {error}"),
            Self::MissingMode => formatter.write_str("fixture manifest is missing mode"),
            Self::InvalidMode(mode) => write!(formatter, "invalid fixture mode: {mode}"),
        }
    }
}

/// The concrete evidence needed before a fixture tree may be used as a
/// conformance oracle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FixtureStatus {
    Baseline { replay_blocks: u64 },
    Captured { replay_blocks: u64 },
}

/// Validate the fixture layout without requiring a running node.
pub fn validate_fixture_tree(root: &Path) -> Result<FixtureStatus, FixtureValidationError> {
    let manifest = FixtureManifest::load(&root.join("manifest.toml"))
        .map_err(FixtureValidationError::Manifest)?;
    if manifest.mode == FixtureMode::Baseline {
        return Ok(FixtureStatus::Baseline {
            replay_blocks: manifest.replay_blocks,
        });
    }
    if manifest.replay_blocks == 0 {
        return Err(FixtureValidationError::EmptyCapture);
    }

    let requirements = [
        // Several Nakamoto blocks can share one burn block in the same tenure.
        ("bitcoin/blocks", 1),
        ("nakamoto/blocks", manifest.replay_blocks),
        ("events/new_block", manifest.replay_blocks),
        ("stacker_set", 1),
    ];
    for (relative_path, minimum_files) in requirements {
        let path = root.join(relative_path);
        let found = count_files(&path)?;
        if found < minimum_files {
            return Err(FixtureValidationError::InsufficientFiles {
                path,
                found,
                minimum_files,
            });
        }
    }

    for relative_path in ["sortition/snapshots.json", "provenance.toml"] {
        let path = root.join(relative_path);
        if !is_nonempty_file(&path)? {
            return Err(FixtureValidationError::MissingOrEmptyFile(path));
        }
    }
    let snapshots: Vec<CapturedBitcoinSnapshot> = serde_json::from_slice(
        &fs::read(root.join("sortition/snapshots.json")).map_err(|_| {
            FixtureValidationError::InvalidSnapshotFile(root.join("sortition/snapshots.json"))
        })?,
    )
    .map_err(|_| {
        FixtureValidationError::InvalidSnapshotFile(root.join("sortition/snapshots.json"))
    })?;
    if snapshots.is_empty() {
        return Err(FixtureValidationError::InvalidSnapshotFile(
            root.join("sortition/snapshots.json"),
        ));
    }
    for snapshot in snapshots {
        let block = root
            .join("bitcoin/blocks")
            .join(format!("{}.hex", snapshot.burn_header_hash));
        if !is_nonempty_file(&block)? {
            return Err(FixtureValidationError::MissingOrEmptyFile(block));
        }
    }

    let checkpoint = root.join("chainstate/checkpoint-H");
    if count_files_recursively(&checkpoint)? == 0 {
        return Err(FixtureValidationError::EmptyCheckpoint(checkpoint));
    }
    let checkpoint_manifest = checkpoint.join("checkpoint.toml");
    let checkpoint_contents = fs::read_to_string(&checkpoint_manifest)
        .map_err(|_| FixtureValidationError::MissingOrEmptyFile(checkpoint_manifest.clone()))?;
    for required_field in [
        "format = \"stacks-core-marf-sqlite-v2\"",
        "source_state_id = ",
        "published_state_index_root = ",
    ] {
        if !checkpoint_contents.contains(required_field) {
            return Err(FixtureValidationError::InvalidCheckpointManifest(
                checkpoint_manifest,
            ));
        }
    }

    Ok(FixtureStatus::Captured {
        replay_blocks: manifest.replay_blocks,
    })
}

fn is_nonempty_file(path: &Path) -> Result<bool, FixtureValidationError> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file() && metadata.len() > 0),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(FixtureValidationError::Metadata {
            path: path.to_owned(),
            error,
        }),
    }
}

fn count_files(path: &Path) -> Result<u64, FixtureValidationError> {
    let mut entries = fs::read_dir(path).map_err(|error| FixtureValidationError::ReadDir {
        path: path.to_owned(),
        error,
    })?;
    entries.try_fold(0_u64, |count, entry| {
        let entry = entry.map_err(FixtureValidationError::ReadDirEntry)?;
        let file_type = entry
            .file_type()
            .map_err(FixtureValidationError::ReadDirEntry)?;
        if file_type.is_file() {
            Ok(count + 1)
        } else {
            Ok(count)
        }
    })
}

fn count_files_recursively(path: &Path) -> Result<u64, FixtureValidationError> {
    let mut count = 0;
    let entries = fs::read_dir(path).map_err(|error| FixtureValidationError::ReadDir {
        path: path.to_owned(),
        error,
    })?;
    for entry in entries {
        let entry = entry.map_err(FixtureValidationError::ReadDirEntry)?;
        let entry_path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(FixtureValidationError::ReadDirEntry)?;
        if file_type.is_file() {
            count += 1;
        } else if file_type.is_dir() {
            count += count_files_recursively(&entry_path)?;
        }
    }
    Ok(count)
}

#[derive(Debug)]
pub enum FixtureValidationError {
    Manifest(ManifestError),
    MissingOrEmptyFile(PathBuf),
    EmptyCheckpoint(PathBuf),
    InvalidCheckpointManifest(PathBuf),
    InvalidSnapshotFile(PathBuf),
    EmptyCapture,
    Metadata {
        path: PathBuf,
        error: std::io::Error,
    },
    ReadDir {
        path: PathBuf,
        error: std::io::Error,
    },
    ReadDirEntry(std::io::Error),
    InsufficientFiles {
        path: PathBuf,
        found: u64,
        minimum_files: u64,
    },
}

impl std::fmt::Display for FixtureValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Manifest(error) => write!(formatter, "{error}"),
            Self::MissingOrEmptyFile(path) => write!(
                formatter,
                "missing or empty required fixture file: {}",
                path.display()
            ),
            Self::EmptyCheckpoint(path) => {
                write!(
                    formatter,
                    "checkpoint contains no files: {}",
                    path.display()
                )
            }
            Self::InvalidCheckpointManifest(path) => write!(
                formatter,
                "invalid portable MARF checkpoint manifest: {}",
                path.display()
            ),
            Self::InvalidSnapshotFile(path) => {
                write!(
                    formatter,
                    "invalid captured Bitcoin snapshots: {}",
                    path.display()
                )
            }
            Self::EmptyCapture => {
                formatter.write_str("captured fixtures must contain at least one replay block")
            }
            Self::Metadata { path, error } => {
                write!(
                    formatter,
                    "could not inspect fixture path {}: {error}",
                    path.display()
                )
            }
            Self::ReadDir { path, error } => {
                write!(
                    formatter,
                    "could not read fixture directory {}: {error}",
                    path.display()
                )
            }
            Self::ReadDirEntry(error) => write!(formatter, "could not read fixture entry: {error}"),
            Self::InsufficientFiles {
                path,
                found,
                minimum_files,
            } => write!(
                formatter,
                "fixture directory {} contains {found} files, expected at least {minimum_files}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for FixtureValidationError {}

impl std::error::Error for ManifestError {}

/// Replay status, deliberately small until the real block decoder arrives.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayDepth {
    pub completed: u64,
    pub expected: u64,
    pub first_failure: Option<u64>,
    pub first_divergence: Option<ReplayDivergence>,
}

/// The precise reason captured replay first diverged from its fixture oracle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplayDivergence {
    StateRoot {
        expected: TrieHash,
        actual: TrieHash,
    },
    Receipt(String),
    Application(String),
    Fixture(String),
}

#[must_use]
pub fn baseline_replay(manifest: FixtureManifest) -> ReplayDepth {
    let first_failure = match replay_one(&BaselineSource, 1) {
        Err(ReplayFailure::StateRoot) if manifest.replay_blocks > 0 => Some(1),
        _ => None,
    };
    ReplayDepth {
        completed: 0,
        expected: manifest.replay_blocks,
        first_failure,
        first_divergence: None,
    }
}

/// Render a stable, human-readable progress report for local development and CI.
#[must_use]
pub fn scoreboard(manifest: FixtureManifest) -> String {
    render_scoreboard(manifest, &baseline_replay(manifest))
}

/// Render the fixture replay score using the checkpoint and captured block stream.
#[must_use]
pub fn scoreboard_at(root: &Path, manifest: FixtureManifest) -> String {
    let replay = match manifest.mode {
        FixtureMode::Baseline => baseline_replay(manifest),
        FixtureMode::Captured => captured_replay(root, manifest),
    };
    render_scoreboard(manifest, &replay)
}

fn render_scoreboard(manifest: FixtureManifest, replay: &ReplayDepth) -> String {
    let mut output = String::from(
        "surface              oracle                     passing        first failure\n\
         ─────────────────────────────────────────────────────────────────────────────\n",
    );
    let fixture_status = match manifest.mode {
        FixtureMode::Baseline => "baseline only",
        FixtureMode::Captured => "captured",
    };
    let replay_mode = match manifest.mode {
        FixtureMode::Baseline => "baseline",
        FixtureMode::Captured => "captured fixture",
    };
    let _ = writeln!(
        output,
        "fixtures              offline integrity          {fixture_status}  —"
    );
    let _ = writeln!(
        output,
        "replay: state root   fixture block headers       {}/{}          {}",
        replay.completed,
        replay.expected,
        replay.first_failure.map_or_else(
            || "—".to_owned(),
            |height| replay.first_divergence.as_ref().map_or_else(
                || format!("block {height}"),
                |divergence| format!("block {height}: {divergence}"),
            ),
        )
    );
    let _ = writeln!(
        output,
        "\nREPLAY DEPTH: {} / {} ({})",
        replay.completed,
        replay.expected,
        if replay.expected == 0 {
            "n/a"
        } else {
            replay_mode
        }
    );
    output
}

fn captured_replay(root: &Path, manifest: FixtureManifest) -> ReplayDepth {
    let Some((source, state_root)) = checkpoint_state(root) else {
        return ReplayDepth {
            completed: 0,
            expected: manifest.replay_blocks,
            first_failure: Some(1),
            first_divergence: Some(ReplayDivergence::Fixture(
                "checkpoint metadata is unavailable".to_owned(),
            )),
        };
    };
    let checkpoint = root.join("chainstate/checkpoint-H/marf.sqlite");
    let Ok(mut chainstate) = ChainState::from_checkpoint(checkpoint, source, state_root) else {
        return ReplayDepth {
            completed: 0,
            expected: manifest.replay_blocks,
            first_failure: Some(1),
            first_divergence: Some(ReplayDivergence::Fixture(
                "checkpoint cannot be opened".to_owned(),
            )),
        };
    };
    let Some(snapshots) = captured_bitcoin_snapshots(root) else {
        return ReplayDepth {
            completed: 0,
            expected: manifest.replay_blocks,
            first_failure: Some(1),
            first_divergence: Some(ReplayDivergence::Fixture(
                "captured Bitcoin snapshots are unavailable".to_owned(),
            )),
        };
    };
    let Ok(mut entries) = fs::read_dir(root.join("nakamoto/blocks")) else {
        return ReplayDepth {
            completed: 0,
            expected: manifest.replay_blocks,
            first_failure: Some(1),
            first_divergence: Some(ReplayDivergence::Fixture(
                "captured blocks are unavailable".to_owned(),
            )),
        };
    };
    let mut paths = entries
        .by_ref()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    paths.sort();

    let mut completed = 0;
    let mut parent = Some(source);
    for (offset, path) in paths.into_iter().enumerate() {
        if completed >= manifest.replay_blocks {
            break;
        }
        let block_number = u64::try_from(offset).unwrap_or(u64::MAX).saturating_add(1);
        let block = match apply_captured_block(root, &mut chainstate, &snapshots, parent, &path) {
            Ok(block) => block,
            Err(divergence) => {
                return ReplayDepth {
                    completed,
                    expected: manifest.replay_blocks,
                    first_failure: Some(block_number),
                    first_divergence: Some(divergence),
                };
            }
        };
        parent = Some(*block.block_id().as_bytes());
        completed += 1;
    }
    ReplayDepth {
        completed,
        expected: manifest.replay_blocks,
        first_failure: (completed < manifest.replay_blocks).then_some(completed + 1),
        first_divergence: None,
    }
}

fn apply_captured_block(
    root: &Path,
    chainstate: &mut ChainState,
    snapshots: &BTreeMap<String, BitcoinBlockContext>,
    parent: Option<[u8; 32]>,
    path: &Path,
) -> Result<NakamotoBlock, ReplayDivergence> {
    let bytes =
        fs::read(path).map_err(|_| ReplayDivergence::Fixture("block cannot be read".to_owned()))?;
    let block = NakamotoBlock::decode(&bytes)
        .map_err(|_| ReplayDivergence::Fixture("block cannot be decoded".to_owned()))?;
    let mut bitcoin_context = *snapshots
        .get(&block.header.consensus_hash.to_string())
        .ok_or_else(|| {
            ReplayDivergence::Fixture(
                "block consensus hash is absent from captured Bitcoin snapshots".to_owned(),
            )
        })?;
    let event_path = root.join("events/new_block").join(
        path.file_stem()
            .map(|name| format!("{}.json", name.to_string_lossy()))
            .ok_or_else(|| ReplayDivergence::Fixture("block has no file name".to_owned()))?,
    );
    let event: CapturedBlockEvent = serde_json::from_slice(
        &fs::read(event_path)
            .map_err(|_| ReplayDivergence::Fixture("block event cannot be read".to_owned()))?,
    )
    .map_err(|_| ReplayDivergence::Fixture("block event cannot be decoded".to_owned()))?;
    bitcoin_context.v1_unlock_height = event.v1;
    bitcoin_context.v2_unlock_height = event.v2;
    bitcoin_context.v3_unlock_height = event.v3;
    bitcoin_context.v4_unlock_height = event.v4;
    let applied = chainstate
        .execute_nakamoto_block_with_bitcoin_context(bitcoin_context, parent, &block)
        .map_err(|error| ReplayDivergence::Application(error.to_string()))?;
    compare_receipts(&event, &applied.receipts)?;
    let actual = TrieHash::from_bytes(applied.execution.state_root.0);
    if actual != block.header.state_index_root {
        return Err(ReplayDivergence::StateRoot {
            expected: block.header.state_index_root,
            actual,
        });
    }
    Ok(block)
}

fn compare_receipts(
    event: &CapturedBlockEvent,
    receipts: &[nano_chainstate::TransactionReceipt],
) -> Result<(), ReplayDivergence> {
    if event.transactions.len() != receipts.len() {
        return Err(ReplayDivergence::Receipt(
            "transaction count differs".to_owned(),
        ));
    }
    for (expected, actual) in event.transactions.iter().zip(receipts) {
        if expected.status != "success" {
            return Err(ReplayDivergence::Receipt(
                "non-success receipt is not implemented".to_owned(),
            ));
        }
        if expected.txid != format!("0x{:?}", actual.txid) {
            return Err(ReplayDivergence::Receipt(
                "transaction ID differs".to_owned(),
            ));
        }
        let raw_result = actual
            .result
            .value
            .as_ref()
            .ok_or_else(|| ReplayDivergence::Receipt("transaction result is absent".to_owned()))?
            .serialize_to_hex()
            .map_err(|_| {
                ReplayDivergence::Receipt("transaction result cannot serialize".to_owned())
            })?;
        if expected.raw_result != format!("0x{raw_result}") {
            return Err(ReplayDivergence::Receipt(
                "transaction result differs".to_owned(),
            ));
        }
        let cost = &actual.result.cost;
        if (
            cost.read_count,
            cost.read_length,
            cost.runtime,
            cost.write_count,
            cost.write_length,
        ) != (
            expected.execution_cost.read_count,
            expected.execution_cost.read_length,
            expected.execution_cost.runtime,
            expected.execution_cost.write_count,
            expected.execution_cost.write_length,
        ) {
            return Err(ReplayDivergence::Receipt(
                "execution cost differs".to_owned(),
            ));
        }
    }
    let actual_events = receipts
        .iter()
        .flat_map(|receipt| {
            receipt
                .result
                .events
                .iter()
                .enumerate()
                .map(move |(index, entry)| entry.json_serialize(index, &receipt.txid, true))
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ReplayDivergence::Receipt("event cannot serialize".to_owned()))?;
    if event.events != actual_events {
        return Err(ReplayDivergence::Receipt("events differ".to_owned()));
    }
    Ok(())
}

impl std::fmt::Display for ReplayDivergence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StateRoot { expected, actual } => {
                write!(formatter, "state root {expected} != {actual}")
            }
            Self::Application(message) | Self::Fixture(message) | Self::Receipt(message) => {
                formatter.write_str(message)
            }
        }
    }
}

fn captured_bitcoin_snapshots(root: &Path) -> Option<BTreeMap<String, BitcoinBlockContext>> {
    let snapshots: Vec<CapturedBitcoinSnapshot> =
        serde_json::from_slice(&fs::read(root.join("sortition/snapshots.json")).ok()?).ok()?;
    snapshots
        .into_iter()
        .map(|snapshot| {
            Some((
                snapshot.consensus_hash,
                BitcoinBlockContext::at_height(snapshot.block_height),
            ))
        })
        .collect()
}

fn checkpoint_state(root: &Path) -> Option<([u8; 32], TrieHash)> {
    let contents = fs::read_to_string(root.join("chainstate/checkpoint-H/checkpoint.toml")).ok()?;
    let source = checkpoint_field(&contents, "source_state_id")?;
    let state_root = checkpoint_field(&contents, "published_state_index_root")?;
    Some((
        decode_hash(source)?,
        TrieHash::from_bytes(decode_hash(state_root)?),
    ))
}

fn checkpoint_field<'a>(contents: &'a str, field: &str) -> Option<&'a str> {
    contents
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(&format!("{field} = ")))
        .map(|value| value.trim_matches('"'))
}

fn decode_hash(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::{
        FixtureManifest, FixtureMode, FixtureStatus, baseline_replay, scoreboard,
        validate_fixture_tree,
    };
    use blockstack_lib::burnchains::{
        MagicBytes,
        bitcoin::{BitcoinNetworkType, BitcoinTxInput, blocks::BitcoinBlockParser},
    };
    use blockstack_lib::chainstate::burn::{
        ConsensusHashExtensions, OpsHash as ReferenceOpsHash,
        SortitionHash as ReferenceSortitionHash,
    };
    use blockstack_lib::chainstate::stacks::address::{
        PoxAddress as ReferencePoxAddress, PoxAddressType20 as ReferencePoxAddressType20,
        PoxAddressType32 as ReferencePoxAddressType32,
    };
    use blockstack_lib::chainstate::stacks::index::{
        BlockMap as ReferenceBlockMap, ClarityMarfTrieId, Error as ReferenceMarfError,
        MARFValue as ReferenceMarfValue, TrieLeaf as ReferenceTrieLeaf,
        bits::{get_leaf_hash, get_node_hash},
        marf::{MARF as ReferenceMarf, MARFOpenOpts as ReferenceMarfOpenOpts},
        node::{
            TrieNode4 as ReferenceTrieNode4, TrieNode256 as ReferenceTrieNode256,
            TriePtr as ReferenceTriePointer,
        },
    };
    use blockstack_lib::chainstate::{
        nakamoto::NakamotoBlock as ReferenceNakamotoBlock,
        stacks::{
            CoinbasePayload, StacksMicroblockHeader,
            StacksTransaction as ReferenceStacksTransaction,
            StacksTransactionSigner as ReferenceStacksTransactionSigner, TokenTransferMemo,
            TransactionAuth as ReferenceTransactionAuth,
            TransactionPayload as ReferenceTransactionPayload,
            TransactionVersion as ReferenceTransactionVersion,
        },
    };
    use blockstack_lib::core::StacksEpochId;
    use clarity::vm::ClarityVersion as ReferenceClarityVersion;
    use clarity::vm::types::{PrincipalData, StandardPrincipalData, Value};
    use nano_address::{PoxAddress, PoxAddressType20, PoxAddressType32, StacksAddress};
    use nano_bitcoin::{
        PreStxCache, decode_block as decode_bitcoin_block, decode_block_with_pre_stx,
    };
    use nano_chainstate::{NakamotoBlock as NanoNakamotoBlock, SignerSet};
    use nano_codec::{
        Transaction as NanoTransaction, TransactionAuth as NanoTransactionAuth,
        transaction_merkle_root,
    };
    use nano_crypto::{
        CryptoError, MessageSignature, StacksPrivateKey, StacksPublicKey, Vrf, VrfPrivateKey,
        VrfProof,
    };
    use nano_marf::{
        MarfTrie, MarfValue, TrieNodeId, TriePointer, VersionedMarf, import_checkpoint, import_pcs,
        internal_node_hash, key_path, leaf_hash,
    };
    use nano_primitives::{BitVec, TrieHash, hash160, sha256, sha512, sha512_256};
    use proptest::prelude::*;
    use serde::Deserialize;
    use stacks_common::util::{
        secp256k1::{
            MessageSignature as ReferenceMessageSignature,
            Secp256k1PrivateKey as ReferenceSecp256k1PrivateKey, Secp256k1PublicKey,
        },
        vrf::{VRF as ReferenceVrf, VRFPrivateKey as ReferenceVrfPrivateKey},
    };
    use stacks_common::{
        bitvec::BitVec as ReferenceBitVec,
        codec::StacksMessageCodec,
        types::{
            PrivateKey, PublicKey,
            chainstate::{
                BlockHeaderHash, StacksAddress as ReferenceStacksAddress,
                StacksBlockId as ReferenceStacksBlockId, TrieHash as ReferenceTrieHash,
            },
        },
        util::hash::{
            Hash160 as ReferenceHash160, Sha256Sum as ReferenceSha256Sum,
            Sha512Sum as ReferenceSha512Sum, Sha512Trunc256Sum,
        },
        util::uint::Uint256 as ReferenceUint256,
        util::vrf::VRFProof as ReferenceVrfProof,
    };
    use stacks_common::{
        deps_common::bitcoin::network::serialize::deserialize as reference_bitcoin_deserialize,
        types::chainstate::{
            BurnchainHeaderHash as ReferenceBitcoinHeaderHash,
            ConsensusHash as ReferenceConsensusHash, PoxId as ReferencePoxId,
            VRFSeed as ReferenceVrfSeed,
        },
    };
    use std::{
        fs,
        io::Cursor,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    #[derive(Deserialize)]
    struct CapturedSortitionSnapshot {
        block_height: u64,
        burn_header_hash: String,
        parent_burn_header_hash: String,
        ops_hash: String,
        sortition: u8,
        sortition_hash: String,
    }

    struct EmptyReferenceBlockMap;

    struct SingleReferenceBlockMap {
        block: stacks_common::types::chainstate::StacksBlockId,
    }

    impl ReferenceBlockMap for EmptyReferenceBlockMap {
        type TrieId = stacks_common::types::chainstate::StacksBlockId;

        fn get_block_hash(&self, _id: u32) -> Result<Self::TrieId, ReferenceMarfError> {
            Err(ReferenceMarfError::NotFoundError)
        }

        fn get_block_hash_caching(
            &mut self,
            _id: u32,
        ) -> Result<&Self::TrieId, ReferenceMarfError> {
            Err(ReferenceMarfError::NotFoundError)
        }

        fn is_block_hash_cached(&self, _id: u32) -> bool {
            false
        }

        fn get_block_id(&self, _hash: &Self::TrieId) -> Result<u32, ReferenceMarfError> {
            Err(ReferenceMarfError::NotFoundError)
        }

        fn get_block_id_caching(
            &mut self,
            _hash: &Self::TrieId,
        ) -> Result<u32, ReferenceMarfError> {
            Err(ReferenceMarfError::NotFoundError)
        }
    }

    impl ReferenceBlockMap for SingleReferenceBlockMap {
        type TrieId = stacks_common::types::chainstate::StacksBlockId;

        fn get_block_hash(&self, id: u32) -> Result<Self::TrieId, ReferenceMarfError> {
            (id == 1)
                .then(|| self.block.clone())
                .ok_or(ReferenceMarfError::NotFoundError)
        }

        fn get_block_hash_caching(&mut self, id: u32) -> Result<&Self::TrieId, ReferenceMarfError> {
            (id == 1)
                .then_some(&self.block)
                .ok_or(ReferenceMarfError::NotFoundError)
        }

        fn is_block_hash_cached(&self, id: u32) -> bool {
            id == 1
        }

        fn get_block_id(&self, hash: &Self::TrieId) -> Result<u32, ReferenceMarfError> {
            (hash == &self.block)
                .then_some(1)
                .ok_or(ReferenceMarfError::NotFoundError)
        }

        fn get_block_id_caching(&mut self, hash: &Self::TrieId) -> Result<u32, ReferenceMarfError> {
            self.get_block_id(hash)
        }
    }

    #[test]
    fn baseline_is_block_one() {
        assert_eq!(
            baseline_replay(FixtureManifest {
                mode: FixtureMode::Baseline,
                replay_blocks: 1,
            })
            .first_failure,
            Some(1)
        );
    }

    #[test]
    fn scoreboard_reports_the_baseline() {
        let report = scoreboard(FixtureManifest {
            mode: FixtureMode::Baseline,
            replay_blocks: 1,
        });
        assert!(report.contains("0/1"));
        assert!(report.contains("block 1"));
    }

    #[test]
    fn checked_in_fixture_is_a_valid_capture() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        assert_eq!(
            validate_fixture_tree(&root).expect("captured fixture directory is valid"),
            FixtureStatus::Captured { replay_blocks: 10 }
        );
        assert!(fs::metadata(root.join("README.md")).is_ok());
    }

    #[test]
    fn checkpoint_graph_import_matches_the_published_root() {
        let source = [
            0x73, 0xd5, 0x36, 0xfd, 0x05, 0x5e, 0x08, 0x3f, 0x60, 0xbe, 0x70, 0x35, 0x0e, 0x72,
            0x9d, 0x99, 0xcc, 0xea, 0xc3, 0x47, 0xc5, 0xbf, 0xaa, 0xa7, 0x9f, 0xd4, 0x62, 0xd1,
            0xb8, 0x21, 0x53, 0xf3,
        ];
        let root = TrieHash::from_bytes([
            0x8f, 0xdf, 0xf0, 0x9f, 0xd8, 0x7a, 0xe7, 0x9f, 0x97, 0x0a, 0x23, 0x36, 0x27, 0x01,
            0x3f, 0x09, 0x47, 0x8e, 0xe1, 0x71, 0x53, 0x79, 0xa7, 0x34, 0x42, 0x58, 0x4b, 0xb4,
            0x3a, 0x64, 0xc0, 0x71,
        ]);
        let checkpoint = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/chainstate/checkpoint-H/marf.sqlite");
        let imported = import_checkpoint(checkpoint, source, root).expect("imports checkpoint");
        assert_eq!(imported.root(source), Some(root));
    }

    #[test]
    fn checkpoint_extension_matches_stacks_core() -> Result<(), Box<dyn std::error::Error>> {
        let temporary = temporary_fixture_root()?;
        let checkpoint = temporary.join("marf.sqlite");
        fs::write(format!("{}.blobs", checkpoint.display()), [])?;
        let source = [0x41; 32];
        let next = [0x42; 32];
        let mut options = ReferenceMarfOpenOpts::default();
        options.external_blobs = true;
        let root = {
            let mut reference = ReferenceMarf::<ReferenceStacksBlockId>::from_path(
                checkpoint.to_str().expect("temporary path is UTF-8"),
                options.clone(),
            )?;
            let mut transaction = reference.begin_tx()?;
            transaction.begin(
                &ReferenceStacksBlockId::sentinel(),
                &ReferenceStacksBlockId(source),
            )?;
            transaction.insert_batch(
                &["checkpoint-source".to_owned()],
                vec![ReferenceMarfValue::from_value("source")],
            )?;
            transaction.seal()?;
            transaction.commit()?;
            TrieHash::from_bytes(
                reference
                    .get_root_hash_at(&ReferenceStacksBlockId(source))?
                    .0,
            )
        };
        let mut imported = import_checkpoint(&checkpoint, source, root)?;
        imported.begin(Some(source), next)?;
        imported.insert(b"checkpoint-extension", MarfValue::from_value(b"value"))?;
        let imported_root = imported.seal()?;

        let mut reference = ReferenceMarf::<ReferenceStacksBlockId>::from_path(
            checkpoint.to_str().expect("temporary path is UTF-8"),
            options,
        )?;
        let mut transaction = reference.begin_tx()?;
        transaction.begin(
            &ReferenceStacksBlockId(source),
            &ReferenceStacksBlockId(next),
        )?;
        transaction.insert_batch(
            &["checkpoint-extension".to_owned()],
            vec![ReferenceMarfValue::from_value("value")],
        )?;
        transaction.seal()?;
        transaction.commit()?;
        let reference_root = reference.get_root_hash_at(&ReferenceStacksBlockId(next))?;
        assert_eq!(imported_root.as_bytes(), &reference_root.0);

        drop(reference);
        fs::remove_dir_all(temporary)?;
        Ok(())
    }

    #[test]
    fn captured_checkpoint_extension_matches_stacks_core() -> Result<(), Box<dyn std::error::Error>>
    {
        let source = [
            0x73, 0xd5, 0x36, 0xfd, 0x05, 0x5e, 0x08, 0x3f, 0x60, 0xbe, 0x70, 0x35, 0x0e, 0x72,
            0x9d, 0x99, 0xcc, 0xea, 0xc3, 0x47, 0xc5, 0xbf, 0xaa, 0xa7, 0x9f, 0xd4, 0x62, 0xd1,
            0xb8, 0x21, 0x53, 0xf3,
        ];
        let root = TrieHash::from_bytes([
            0x8f, 0xdf, 0xf0, 0x9f, 0xd8, 0x7a, 0xe7, 0x9f, 0x97, 0x0a, 0x23, 0x36, 0x27, 0x01,
            0x3f, 0x09, 0x47, 0x8e, 0xe1, 0x71, 0x53, 0x79, 0xa7, 0x34, 0x42, 0x58, 0x4b, 0xb4,
            0x3a, 0x64, 0xc0, 0x71,
        ]);
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/chainstate/checkpoint-H");
        let temporary = temporary_fixture_root()?;
        let checkpoint = temporary.join("marf.sqlite");
        fs::copy(fixture.join("marf.sqlite"), &checkpoint)?;
        fs::copy(
            fixture.join("marf.sqlite.blobs"),
            temporary.join("marf.sqlite.blobs"),
        )?;

        let next = [0x42; 32];
        let mut imported = import_checkpoint(&checkpoint, source, root)?;
        let mut options = ReferenceMarfOpenOpts::default();
        options.external_blobs = true;
        let mut reference = ReferenceMarf::<ReferenceStacksBlockId>::from_path(
            checkpoint.to_str().expect("temporary path is UTF-8"),
            options,
        )?;
        let mut transaction = reference.begin_tx()?;
        transaction.begin(
            &ReferenceStacksBlockId(source),
            &ReferenceStacksBlockId(next),
        )?;
        transaction.insert_batch(
            &["checkpoint-extension".to_owned()],
            vec![ReferenceMarfValue::from_value("value")],
        )?;
        transaction.seal()?;
        transaction.commit()?;
        let reference_root = reference.get_root_hash_at(&ReferenceStacksBlockId(next))?;

        imported.begin(Some(source), next)?;
        imported.insert(b"checkpoint-extension", MarfValue::from_value(b"value"))?;
        let imported_root = imported.seal()?;
        assert_eq!(imported_root.as_bytes(), &reference_root.0);

        drop(reference);
        fs::remove_dir_all(temporary)?;
        Ok(())
    }

    #[test]
    fn captured_checkpoint_temporary_execution_id_matches_stacks_core()
    -> Result<(), Box<dyn std::error::Error>> {
        let source = [
            0x73, 0xd5, 0x36, 0xfd, 0x05, 0x5e, 0x08, 0x3f, 0x60, 0xbe, 0x70, 0x35, 0x0e, 0x72,
            0x9d, 0x99, 0xcc, 0xea, 0xc3, 0x47, 0xc5, 0xbf, 0xaa, 0xa7, 0x9f, 0xd4, 0x62, 0xd1,
            0xb8, 0x21, 0x53, 0xf3,
        ];
        let root = TrieHash::from_bytes([
            0x8f, 0xdf, 0xf0, 0x9f, 0xd8, 0x7a, 0xe7, 0x9f, 0x97, 0x0a, 0x23, 0x36, 0x27, 0x01,
            0x3f, 0x09, 0x47, 0x8e, 0xe1, 0x71, 0x53, 0x79, 0xa7, 0x34, 0x42, 0x58, 0x4b, 0xb4,
            0x3a, 0x64, 0xc0, 0x71,
        ]);
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/chainstate/checkpoint-H");
        let temporary = temporary_fixture_root()?;
        let checkpoint = temporary.join("marf.sqlite");
        fs::copy(fixture.join("marf.sqlite"), &checkpoint)?;
        fs::copy(
            fixture.join("marf.sqlite.blobs"),
            temporary.join("marf.sqlite.blobs"),
        )?;

        let execution_id = *sha512_256(&[1; 52]).as_bytes();
        let key = "temporary-execution-id";
        let value = "value";
        let mut options = ReferenceMarfOpenOpts::default();
        options.external_blobs = true;
        let mut reference = ReferenceMarf::<ReferenceStacksBlockId>::from_path(
            checkpoint.to_str().expect("temporary path is UTF-8"),
            options,
        )?;
        let mut transaction = reference.begin_tx()?;
        transaction.begin(
            &ReferenceStacksBlockId(source),
            &ReferenceStacksBlockId(execution_id),
        )?;
        transaction.insert_batch(
            &[key.to_owned()],
            vec![ReferenceMarfValue::from_value(value)],
        )?;
        transaction.seal()?;
        transaction.commit()?;
        let reference_root = reference.get_root_hash_at(&ReferenceStacksBlockId(execution_id))?;

        let mut imported = import_checkpoint(&checkpoint, source, root)?;
        imported.begin(Some(source), execution_id)?;
        imported.insert(key.as_bytes(), MarfValue::from_value(value.as_bytes()))?;
        let imported_root = imported.seal()?;
        assert_eq!(imported_root.as_bytes(), &reference_root.0);

        drop(reference);
        fs::remove_dir_all(temporary)?;
        Ok(())
    }

    #[test]
    fn captured_checkpoint_overwrite_matches_stacks_core() -> Result<(), Box<dyn std::error::Error>>
    {
        let source = [
            0x73, 0xd5, 0x36, 0xfd, 0x05, 0x5e, 0x08, 0x3f, 0x60, 0xbe, 0x70, 0x35, 0x0e, 0x72,
            0x9d, 0x99, 0xcc, 0xea, 0xc3, 0x47, 0xc5, 0xbf, 0xaa, 0xa7, 0x9f, 0xd4, 0x62, 0xd1,
            0xb8, 0x21, 0x53, 0xf3,
        ];
        let root = TrieHash::from_bytes([
            0x8f, 0xdf, 0xf0, 0x9f, 0xd8, 0x7a, 0xe7, 0x9f, 0x97, 0x0a, 0x23, 0x36, 0x27, 0x01,
            0x3f, 0x09, 0x47, 0x8e, 0xe1, 0x71, 0x53, 0x79, 0xa7, 0x34, 0x42, 0x58, 0x4b, 0xb4,
            0x3a, 0x64, 0xc0, 0x71,
        ]);
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/chainstate/checkpoint-H");
        let temporary = temporary_fixture_root()?;
        let checkpoint = temporary.join("marf.sqlite");
        fs::copy(fixture.join("marf.sqlite"), &checkpoint)?;
        fs::copy(
            fixture.join("marf.sqlite.blobs"),
            temporary.join("marf.sqlite.blobs"),
        )?;

        let next = [0x43; 32];
        let key = "_stx-data::clarity_storage::block_time";
        let value = "1785170178";
        let mut imported = import_checkpoint(&checkpoint, source, root)?;
        let mut options = ReferenceMarfOpenOpts::default();
        options.external_blobs = true;
        let mut reference = ReferenceMarf::<ReferenceStacksBlockId>::from_path(
            checkpoint.to_str().expect("temporary path is UTF-8"),
            options,
        )?;
        let mut transaction = reference.begin_tx()?;
        transaction.begin(
            &ReferenceStacksBlockId(source),
            &ReferenceStacksBlockId(next),
        )?;
        transaction.insert_batch(
            &[key.to_owned()],
            vec![ReferenceMarfValue::from_value(value)],
        )?;
        transaction.seal()?;
        transaction.commit()?;
        let reference_root = reference.get_root_hash_at(&ReferenceStacksBlockId(next))?;

        imported.begin(Some(source), next)?;
        imported.insert(key.as_bytes(), MarfValue::from_value(value.as_bytes()))?;
        let imported_root = imported.seal()?;
        assert_eq!(imported_root.as_bytes(), &reference_root.0);

        drop(reference);
        fs::remove_dir_all(temporary)?;
        Ok(())
    }

    #[test]
    fn pcs_layout_import_uses_the_manifest_root() -> Result<(), Box<dyn std::error::Error>> {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/chainstate/checkpoint-H");
        let root = temporary_fixture_root()?;
        let clarity = root.join("chainstate/vm/clarity");
        fs::create_dir_all(&clarity)?;
        fs::copy(fixture.join("marf.sqlite"), clarity.join("marf.sqlite"))?;
        fs::copy(
            fixture.join("marf.sqlite.blobs"),
            clarity.join("marf.sqlite.blobs"),
        )?;
        write_file(
            &root.join("PCS_manifest.toml"),
            "[snapshot]\nblock_hash = \"0x73d536fd055e083f60be70350e729d99cceac347c5bfaaa79fd462d1b82153f3\"\n\n[roots]\nclarity_archival_marf_root_hash = \"0x8fdff09fd87ae79f970a233627013f09478ee1715379a73442584bb43a64c071\"\n",
        )?;
        let imported = import_pcs(&root)?;
        let source = [
            0x73, 0xd5, 0x36, 0xfd, 0x05, 0x5e, 0x08, 0x3f, 0x60, 0xbe, 0x70, 0x35, 0x0e, 0x72,
            0x9d, 0x99, 0xcc, 0xea, 0xc3, 0x47, 0xc5, 0xbf, 0xaa, 0xa7, 0x9f, 0xd4, 0x62, 0xd1,
            0xb8, 0x21, 0x53, 0xf3,
        ];
        assert!(imported.root(source).is_some());
        fs::remove_dir_all(root)?;
        Ok(())
    }

    fn append_reference_marf_state(
        reference: &mut ReferenceMarf<ReferenceStacksBlockId>,
        parent: Option<&ReferenceStacksBlockId>,
        block: &ReferenceStacksBlockId,
        key: &str,
        value: &str,
    ) -> ReferenceTrieHash {
        let parent = parent
            .cloned()
            .unwrap_or_else(ReferenceStacksBlockId::sentinel);
        let mut transaction = reference.begin_tx().expect("start reference transaction");
        transaction
            .begin(&parent, block)
            .expect("begin reference state");
        transaction
            .insert_batch(
                &[key.to_owned()],
                vec![ReferenceMarfValue::from_value(value)],
            )
            .expect("insert reference value");
        transaction.seal().expect("seal reference state");
        transaction.commit().expect("commit reference state");
        reference
            .get_root_hash_at(block)
            .expect("read reference state root")
    }

    fn append_nano_marf_state(
        ours: &mut VersionedMarf,
        parent: Option<[u8; 32]>,
        block: [u8; 32],
        key: &str,
        value: &str,
    ) -> TrieHash {
        ours.begin(parent, block).expect("begin nano state");
        ours.insert(key.as_bytes(), MarfValue::from_value(value.as_bytes()))
            .expect("insert nano value");
        ours.seal().expect("seal nano state")
    }

    #[test]
    fn versioned_marf_first_write_matches_stacks_core() {
        let mut reference = ReferenceMarf::<ReferenceStacksBlockId>::from_path(
            ":memory:",
            ReferenceMarfOpenOpts::default(),
        )
        .expect("open reference MARF");
        let first = ReferenceStacksBlockId([1; 32]);
        let reference_root =
            append_reference_marf_state(&mut reference, None, &first, "alpha", "first");

        let mut ours = VersionedMarf::default();
        let root = append_nano_marf_state(&mut ours, None, first.0, "alpha", "first");
        assert_eq!(root.as_bytes(), &reference_root.0);

        let second = ReferenceStacksBlockId([2; 32]);
        let reference_second_root =
            append_reference_marf_state(&mut reference, Some(&first), &second, "beta", "second");

        let second_root =
            append_nano_marf_state(&mut ours, Some(first.0), second.0, "beta", "second");
        assert_eq!(second_root.as_bytes(), &reference_second_root.0);

        let fork = ReferenceStacksBlockId([3; 32]);
        let reference_fork_root =
            append_reference_marf_state(&mut reference, Some(&first), &fork, "gamma", "fork");

        let fork_root = append_nano_marf_state(&mut ours, Some(first.0), fork.0, "gamma", "fork");
        assert_eq!(fork_root.as_bytes(), &reference_fork_root.0);

        let mut blocks = vec![first, second, fork];
        let mut seed = 0x9e37_79b9_u32;
        for index in 0_u16..10_000 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let parent = blocks[(seed as usize) % blocks.len()].clone();
            let mut block_id = [0; 32];
            block_id[30..].copy_from_slice(&(index + 4).to_be_bytes());
            let block = ReferenceStacksBlockId(block_id);
            let key = format!("key-{}", seed % 17);
            let value = format!("value-{index}-{seed}");

            let reference_root =
                append_reference_marf_state(&mut reference, Some(&parent), &block, &key, &value);
            let root = append_nano_marf_state(&mut ours, Some(parent.0), block.0, &key, &value);
            assert_eq!(
                root.as_bytes(),
                &reference_root.0,
                "randomized lockstep mismatch at state {index}"
            );
            blocks.push(block);
        }
    }

    #[test]
    fn captured_fixture_requires_every_oracle_input() -> Result<(), Box<dyn std::error::Error>> {
        let root = temporary_fixture_root()?;
        write_file(
            &root.join("manifest.toml"),
            "mode = \"captured\"\nreplay_blocks = 1\n",
        )?;
        let bitcoin_hash = "0000000000000000000000000000000000000000000000000000000000000000";
        write_file(
            &root
                .join("bitcoin/blocks")
                .join(format!("{bitcoin_hash}.hex")),
            "00",
        )?;
        write_file(&root.join("nakamoto/blocks/00000001.bin"), "block")?;
        write_file(&root.join("events/new_block/00000001.json"), "{}")?;
        write_file(&root.join("stacker_set/cycle-0.json"), "{}")?;
        let snapshot = format!(
            "[{{\"block_height\":1,\"burn_header_hash\":\"{bitcoin_hash}\",\"consensus_hash\":\"0000000000000000000000000000000000000000\"}}]"
        );
        write_file(&root.join("sortition/snapshots.json"), &snapshot)?;
        write_file(
            &root.join("chainstate/checkpoint-H/checkpoint.toml"),
            "format = \"stacks-core-marf-sqlite-v2\"\nsource_state_id = \"id\"\npublished_state_index_root = \"root\"\n",
        )?;
        write_file(&root.join("provenance.toml"), "hacknet_commit = \"test\"\n")?;

        assert_eq!(
            validate_fixture_tree(&root)?,
            FixtureStatus::Captured { replay_blocks: 1 }
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }

    proptest! {
        #[test]
        fn hash_primitives_match_stacks_core(data in proptest::collection::vec(any::<u8>(), 0..1024)) {
            let our_trie = TrieHash::from_data(&data);
            let reference_trie = ReferenceTrieHash::from_data(&data);
            let our_hash160 = hash160(&data);
            let reference_hash160 = ReferenceHash160::from_data(&data);
            let our_sha256 = sha256(&data);
            let reference_sha256 = ReferenceSha256Sum::from_data(&data);
            let our_sha512 = sha512(&data);
            let reference_sha512 = ReferenceSha512Sum::from_data(&data);
            let our_sha512_256 = sha512_256(&data);
            let reference_sha512_256 = Sha512Trunc256Sum::from_data(&data);
            prop_assert_eq!(our_trie.as_bytes(), &reference_trie.0);
            prop_assert_eq!(our_hash160.as_bytes(), &reference_hash160.0);
            prop_assert_eq!(our_sha256.as_bytes(), &reference_sha256.0);
            prop_assert_eq!(our_sha512.as_bytes(), &reference_sha512.0);
            prop_assert_eq!(our_sha512_256.as_bytes(), &reference_sha512_256.0);
        }

        #[test]
        fn uint256_matches_stacks_core(left in any::<[u64; 4]>(), right in any::<[u64; 4]>()) {
            let ours_left = uint_from_words(left);
            let ours_right = uint_from_words(right);
            let reference_left = ReferenceUint256(left);
            let reference_right = ReferenceUint256(right);
            let (ours_sum, _) = ours_left.overflowing_add(ours_right);
            prop_assert_eq!(uint_to_words(ours_sum), (reference_left + reference_right).0);
            let (ours_product, _) = ours_left.overflowing_mul(ours_right);
            prop_assert_eq!(uint_to_words(ours_product), (reference_left * reference_right).0);
            if right != [0; 4] {
                prop_assert_eq!(uint_to_words(ours_left / ours_right), (reference_left / reference_right).0);
            }
        }

        #[test]
        fn bitvec_matches_stacks_core(values in proptest::collection::vec(any::<bool>(), 1..=64)) {
            let length = u16::try_from(values.len()).expect("test bound fits u16");
            let mut ours = BitVec::<64>::zeros(length).expect("valid bit vector");
            let mut reference = ReferenceBitVec::<64>::zeros(length).expect("valid bit vector");
            for (index, value) in values.into_iter().enumerate() {
                let index = u16::try_from(index).expect("test bound fits u16");
                ours.set(index, value).expect("in bounds");
                reference.set(index, value).expect("in bounds");
            }
            let mut reference_bytes = Vec::new();
            reference.consensus_serialize(&mut reference_bytes).expect("serializes");
            prop_assert_eq!(ours.wire_bytes(), reference_bytes);
        }

        #[test]
        fn marf_values_match_stacks_core(value in proptest::collection::vec(any::<u8>(), 0..1024)) {
            let text = String::from_utf8_lossy(&value);
            let ours = MarfValue::from_value(text.as_bytes());
            let reference = ReferenceMarfValue::from_value(&text);
            let our_path = key_path(text.as_bytes());
            let reference_path = ReferenceTrieHash::from_key(&text);
            prop_assert_eq!(ours.as_bytes(), &reference.0);
            prop_assert_eq!(our_path.as_bytes(), reference_path.as_bytes());
        }

        #[test]
        fn marf_leaf_hashes_match_stacks_core(
            path in proptest::collection::vec(any::<u8>(), 0..=32),
            value in proptest::collection::vec(any::<u8>(), 0..1024),
        ) {
            let text = String::from_utf8_lossy(&value);
            let ours = leaf_hash(&path, MarfValue::from_value(text.as_bytes())).expect("bounded path");
            let reference = get_leaf_hash(&ReferenceTrieLeaf::from_value(
                &path,
                ReferenceMarfValue::from_value(&text),
            ));
            prop_assert_eq!(ours.as_bytes(), &reference.0);
        }

        #[test]
        fn marf_node4_hashes_match_stacks_core(
            path in proptest::collection::vec(any::<u8>(), 0..=32),
            characters in any::<[u8; 4]>(),
            child_bytes in any::<[[u8; 32]; 4]>(),
        ) {
            let mut reference_node = ReferenceTrieNode4::new(&path);
            let mut pointers = [TriePointer {
                id: 0,
                character: 0,
                referenced_block: None,
            }; 4];
            let mut reference_children = Vec::with_capacity(4);
            let mut children = Vec::with_capacity(4);
            for index in 0..4 {
                reference_node.ptrs[index] = ReferenceTriePointer::new(1, characters[index], 0);
                pointers[index] = TriePointer {
                    id: 1,
                    character: characters[index],
                    referenced_block: None,
                };
                reference_children.push(ReferenceTrieHash(child_bytes[index]));
                children.push(TrieHash::from_bytes(child_bytes[index]));
            }
            let mut map = EmptyReferenceBlockMap;
            let reference = get_node_hash(&reference_node, &reference_children, &mut map);
            let ours = internal_node_hash(TrieNodeId::Node4, &pointers, &path, &children)
                .expect("fixed pointer count");
            prop_assert_eq!(ours.as_bytes(), &reference.0);
        }

        #[test]
        fn marf_back_pointer_nodes_match_stacks_core(
            character in any::<u8>(),
            block in any::<[u8; 32]>(),
        ) {
            let mut reference_node = ReferenceTrieNode256::new(&[]);
            let mut reference_pointer = ReferenceTriePointer::new(0x81, character, 0);
            reference_pointer.back_block = 1;
            reference_node.ptrs[usize::from(character)] = reference_pointer;
            let mut reference_children = vec![ReferenceTrieHash::from_data(&[]); 256];
            reference_children[usize::from(character)] = ReferenceTrieHash(block);
            let mut map = SingleReferenceBlockMap {
                block: stacks_common::types::chainstate::StacksBlockId(block),
            };
            let reference = get_node_hash(&reference_node, &reference_children, &mut map);

            let mut pointers = vec![TriePointer {
                id: 0,
                character: 0,
                referenced_block: None,
            }; 256];
            pointers[usize::from(character)] = TriePointer {
                id: 0x81,
                character,
                referenced_block: Some(block),
            };
            let mut children = vec![TrieHash::EMPTY; 256];
            children[usize::from(character)] = TrieHash::from_bytes(block);
            let ours = internal_node_hash(TrieNodeId::Node256, &pointers, &[], &children)
                .expect("fixed pointer count");
            prop_assert_eq!(ours.as_bytes(), &reference.0);
        }

        #[test]
        fn marf_first_insert_matches_stacks_core(
            path in any::<[u8; 32]>(),
            value in proptest::collection::vec(any::<u8>(), 0..1024),
        ) {
            let text = String::from_utf8_lossy(&value);
            let mut reference_root = ReferenceTrieNode256::new(&[]);
            reference_root.ptrs[usize::from(path[0])] = ReferenceTriePointer::new(1, path[0], 0);
            let reference_leaf = ReferenceTrieLeaf::from_value(
                &path[1..],
                ReferenceMarfValue::from_value(&text),
            );
            let mut child_hashes = vec![ReferenceTrieHash::from_data(&[]); 256];
            child_hashes[usize::from(path[0])] = get_leaf_hash(&reference_leaf);
            let mut map = EmptyReferenceBlockMap;
            let reference = get_node_hash(&reference_root, &child_hashes, &mut map);

            let mut ours = MarfTrie::default();
            ours.insert_path(path, MarfValue::from_value(text.as_bytes()));
            let root = ours.root_hash();
            prop_assert_eq!(root.as_bytes(), &reference.0);
        }

        #[test]
        fn marf_first_path_split_matches_stacks_core(
            path in any::<[u8; 32]>(),
            first_value in proptest::collection::vec(any::<u8>(), 0..1024),
            second_value in proptest::collection::vec(any::<u8>(), 0..1024),
        ) {
            let first_text = String::from_utf8_lossy(&first_value);
            let second_text = String::from_utf8_lossy(&second_value);
            let mut alternate = path;
            alternate[4] = alternate[4].wrapping_add(1);

            let first_leaf = ReferenceTrieLeaf::from_value(
                &path[5..],
                ReferenceMarfValue::from_value(&first_text),
            );
            let second_leaf = ReferenceTrieLeaf::from_value(
                &alternate[5..],
                ReferenceMarfValue::from_value(&second_text),
            );
            let mut branch = ReferenceTrieNode4::new(&path[1..4]);
            branch.ptrs[0] = ReferenceTriePointer::new(1, path[4], 0);
            branch.ptrs[1] = ReferenceTriePointer::new(1, alternate[4], 0);
            let mut map = EmptyReferenceBlockMap;
            let mut branch_hashes = vec![ReferenceTrieHash::from_data(&[]); 4];
            branch_hashes[0] = get_leaf_hash(&first_leaf);
            branch_hashes[1] = get_leaf_hash(&second_leaf);
            let branch_hash = get_node_hash(&branch, &branch_hashes, &mut map);

            let mut reference_root = ReferenceTrieNode256::new(&[]);
            reference_root.ptrs[usize::from(path[0])] = ReferenceTriePointer::new(2, path[0], 0);
            let mut root_hashes = vec![ReferenceTrieHash::from_data(&[]); 256];
            root_hashes[usize::from(path[0])] = branch_hash;
            let reference = get_node_hash(&reference_root, &root_hashes, &mut map);

            let mut ours = MarfTrie::default();
            ours.insert_path(path, MarfValue::from_value(first_text.as_bytes()));
            ours.insert_path(alternate, MarfValue::from_value(second_text.as_bytes()));
            let root = ours.root_hash();
            prop_assert_eq!(root.as_bytes(), &reference.0);
        }

        #[test]
        fn stacks_addresses_match_stacks_core(version in 0_u8..=31, bytes in any::<[u8; 20]>()) {
            let ours = StacksAddress::new(version, nano_primitives::Hash160::from_bytes(bytes)).expect("valid version");
            let reference = ReferenceStacksAddress::new(version, ReferenceHash160(bytes)).expect("valid version");
            prop_assert_eq!(ours.to_string(), reference.to_string());
            prop_assert_eq!(ours.to_string().parse::<StacksAddress>().expect("decodes"), ours);
        }
    }

    #[test]
    fn bitvec_wire_format_matches_stacks_core() {
        let mut ours = BitVec::<16>::zeros(10).expect("valid bit vector");
        let mut reference = ReferenceBitVec::<16>::zeros(10).expect("valid bit vector");
        for index in [0, 3, 8] {
            ours.set(index, true).expect("in bounds");
            reference.set(index, true).expect("in bounds");
        }
        let mut reference_bytes = Vec::new();
        reference
            .consensus_serialize(&mut reference_bytes)
            .expect("serializes");
        assert_eq!(ours.wire_bytes(), reference_bytes);
    }

    #[test]
    fn pox_addresses_match_stacks_core() {
        let bytes20 = [0x42; 20];
        let bytes32 = [0x24; 32];
        let cases = [
            (
                PoxAddress::Standard {
                    address: StacksAddress::new(26, nano_primitives::Hash160::from_bytes(bytes20))
                        .expect("valid address"),
                    hash_mode: None,
                },
                ReferencePoxAddress::Standard(
                    ReferenceStacksAddress::new(26, ReferenceHash160(bytes20))
                        .expect("valid address"),
                    None,
                ),
            ),
            (
                PoxAddress::Addr20 {
                    mainnet: true,
                    address_type: PoxAddressType20::P2wpkh,
                    bytes: bytes20,
                },
                ReferencePoxAddress::Addr20(true, ReferencePoxAddressType20::P2WPKH, bytes20),
            ),
            (
                PoxAddress::Addr32 {
                    mainnet: false,
                    address_type: PoxAddressType32::P2wsh,
                    bytes: bytes32,
                },
                ReferencePoxAddress::Addr32(false, ReferencePoxAddressType32::P2WSH, bytes32),
            ),
            (
                PoxAddress::Addr32 {
                    mainnet: true,
                    address_type: PoxAddressType32::P2tr,
                    bytes: bytes32,
                },
                ReferencePoxAddress::Addr32(true, ReferencePoxAddressType32::P2TR, bytes32),
            ),
        ];
        for (ours, reference) in cases {
            assert_eq!(
                ours.bitcoin_address()
                    .expect("valid Bitcoin address")
                    .to_string(),
                reference.to_b58()
            );
        }
    }

    #[test]
    fn captured_blocks_round_trip_with_stacks_core() {
        let blocks = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/nakamoto/blocks");
        for entry in fs::read_dir(blocks).expect("read fixture blocks") {
            let path = entry.expect("fixture entry").path();
            let bytes = fs::read(&path).expect("read fixture block");
            let block = ReferenceNakamotoBlock::consensus_deserialize(&mut Cursor::new(&bytes))
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            let mut encoded = Vec::new();
            block
                .consensus_serialize(&mut encoded)
                .expect("serialize fixture block");
            assert_eq!(encoded, bytes, "{}", path.display());
        }
    }

    #[test]
    fn captured_nakamoto_envelopes_match_stacks_core() {
        let blocks = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/nakamoto/blocks");
        for entry in fs::read_dir(blocks).expect("read fixture blocks") {
            let path = entry.expect("fixture entry").path();
            let bytes = fs::read(&path).expect("read fixture block");
            let reference = ReferenceNakamotoBlock::consensus_deserialize(&mut Cursor::new(&bytes))
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            let ours = NanoNakamotoBlock::decode(&bytes)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));

            assert_eq!(ours.encode(), bytes, "{}", path.display());
            assert_eq!(
                ours.header.version,
                reference.header.version,
                "{}",
                path.display()
            );
            assert_eq!(
                ours.header.chain_length,
                reference.header.chain_length,
                "{}",
                path.display()
            );
            assert_eq!(
                ours.header.consensus_hash.as_bytes(),
                &reference.header.consensus_hash.0,
                "{}",
                path.display()
            );
            assert_eq!(
                ours.header.state_index_root.as_bytes(),
                &reference.header.state_index_root.0,
                "{}",
                path.display()
            );
            assert_eq!(
                ours.header.miner_signature_hash().as_bytes(),
                &reference.header.miner_signature_hash().0,
                "{}",
                path.display()
            );
            assert_eq!(
                ours.header.signer_signature_hash().as_bytes(),
                &reference.header.signer_signature_hash().0,
                "{}",
                path.display()
            );
            assert_eq!(
                ours.block_id().as_bytes(),
                &reference.block_id().0,
                "{}",
                path.display()
            );
            assert_eq!(
                ours.transactions.len(),
                reference.txs.len(),
                "{}",
                path.display()
            );
        }
    }

    #[test]
    fn captured_nakamoto_blocks_link_to_their_predecessors() {
        let blocks = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/nakamoto/blocks");
        let mut paths = fs::read_dir(blocks)
            .expect("read fixture blocks")
            .map(|entry| entry.expect("fixture entry").path())
            .collect::<Vec<_>>();
        paths.sort();
        let mut previous = None;
        for path in paths {
            let block = NanoNakamotoBlock::decode(&fs::read(&path).expect("read fixture block"))
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            if let Some(parent) = previous.as_ref() {
                block
                    .validate_successor(parent)
                    .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            }
            previous = Some(block.header);
        }
    }

    #[test]
    fn concatenated_nakamoto_blocks_decode_as_a_tenure_stream() {
        let blocks = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/nakamoto/blocks");
        let mut paths = fs::read_dir(blocks)
            .expect("read fixture blocks")
            .map(|entry| entry.expect("fixture entry").path())
            .collect::<Vec<_>>();
        paths.sort();
        let bytes = paths.iter().take(3).fold(Vec::new(), |mut bytes, path| {
            bytes.extend(fs::read(path).expect("read fixture block"));
            bytes
        });
        let mut offset = 0;
        let mut count = 0;
        while offset < bytes.len() {
            let (_, consumed) = NanoNakamotoBlock::decode_prefix(&bytes[offset..])
                .expect("decode block from tenure stream");
            offset += consumed;
            count += 1;
        }
        assert_eq!(count, 3);
        assert_eq!(offset, bytes.len());
    }

    #[test]
    fn captured_blocks_have_the_expected_signer_weight() {
        #[derive(Deserialize)]
        struct SignerWire {
            signing_key: String,
            stacked_amt: u64,
            weight: u32,
        }
        #[derive(Deserialize)]
        struct RewardSetWire {
            pox_ustx_threshold: u64,
            signers: Vec<SignerWire>,
        }
        #[derive(Deserialize)]
        struct StackerSetWire {
            stacker_set: RewardSetWire,
        }

        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let reward_set: StackerSetWire = serde_json::from_slice(
            &fs::read(fixture_root.join("stacker_set/cycle-18.json")).expect("read reward set"),
        )
        .expect("parse reward set");
        let expected_weights = reward_set
            .stacker_set
            .signers
            .iter()
            .map(|signer| signer.weight)
            .collect::<Vec<_>>();
        let signers = reward_set
            .stacker_set
            .signers
            .into_iter()
            .map(|signer| {
                (
                    StacksPublicKey::from_bytes(
                        &hex::decode(signer.signing_key).expect("decode signing key"),
                    )
                    .expect("valid signer key"),
                    u128::from(signer.stacked_amt),
                )
            })
            .collect();
        let signer_set = SignerSet::from_stacked_amounts(
            signers,
            u128::from(reward_set.stacker_set.pox_ustx_threshold),
        )
        .expect("valid signer set");
        assert_eq!(
            signer_set.weights(),
            expected_weights.as_slice(),
            "fixture signer weights"
        );

        for entry in fs::read_dir(fixture_root.join("nakamoto/blocks")).expect("read blocks") {
            let path = entry.expect("fixture entry").path();
            let block = NanoNakamotoBlock::decode(&fs::read(&path).expect("read block"))
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            assert!(
                signer_set.verify(&block.header).is_ok(),
                "{}",
                path.display()
            );
        }
    }

    #[test]
    fn captured_bitcoin_packets_match_stacks_core() {
        let blocks = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/bitcoin/blocks");
        let parser = BitcoinBlockParser::new(
            BitcoinNetworkType::Regtest,
            MagicBytes::from(b"T3".as_slice()),
        );
        for entry in fs::read_dir(blocks).expect("read fixture blocks") {
            let path = entry.expect("fixture entry").path();
            let hex = fs::read_to_string(&path).expect("read fixture block");
            let bytes = hex::decode(hex.trim()).expect("decode fixture hex");
            let reference_block = reference_bitcoin_deserialize(&bytes)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            let reference = parser.parse_block(&reference_block, 0, StacksEpochId::Epoch40);
            let ours = decode_bitcoin_block(0, &bytes, *b"T3")
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));

            assert_eq!(ours.hash, reference.block_hash.0, "{}", path.display());
            assert_eq!(
                ours.operations.len(),
                reference.txs.len(),
                "{}",
                path.display()
            );
            for (ours, reference) in ours.operations.iter().zip(&reference.txs) {
                assert_eq!(ours.txid, reference.txid.0, "{}", path.display());
                assert_eq!(
                    ours.transaction_index,
                    reference.vtxindex,
                    "{}",
                    path.display()
                );
                assert_eq!(
                    operation_opcode(ours),
                    reference.opcode,
                    "{}",
                    path.display()
                );
                assert_eq!(operation_data(ours), reference.data, "{}", path.display());
                assert_eq!(
                    ours.inputs.len(),
                    reference.inputs.len(),
                    "{}",
                    path.display()
                );
                assert_eq!(
                    ours.outputs.len(),
                    reference.outputs.len(),
                    "{}",
                    path.display()
                );
                for (ours, reference) in ours.inputs.iter().zip(&reference.inputs) {
                    let (txid, output_index) = match reference {
                        BitcoinTxInput::Raw(input) => (input.tx_ref.0.0, input.tx_ref.1),
                        BitcoinTxInput::Structured(input) => (input.tx_ref.0.0, input.tx_ref.1),
                    };
                    assert_eq!(ours.txid, txid, "{}", path.display());
                    assert_eq!(ours.output_index, output_index, "{}", path.display());
                }
                for (ours, reference) in ours.outputs.iter().zip(&reference.outputs) {
                    assert_eq!(ours.amount_sats, reference.units, "{}", path.display());
                    assert_eq!(
                        ours.recipient.script_pubkey().as_bytes(),
                        reference_bitcoin_script_pubkey(&reference.address),
                        "{}",
                        path.display()
                    );
                }
            }
        }
    }

    #[test]
    fn sortition_hash_chain_matches_stacks_core() {
        let mut chain =
            nano_sortition::SnapshotChain::new(nano_sortition::SortitionSnapshot::genesis(
                0,
                nano_primitives::BitcoinHeaderHash::from_bytes([0; 32]),
            ));
        let mut reference_sortition_hash = ReferenceSortitionHash::initial();
        let mut reference_consensus_hashes = vec![ReferenceConsensusHash::empty()];

        for height in 1_u64..=64 {
            let hash = [u8::try_from(height).expect("test height fits u8"); 32];
            let block = nano_bitcoin::BitcoinBlock {
                height,
                hash,
                operations: Vec::new(),
            };
            let winner_vrf_seed = (height % 3 == 0).then_some(hash);
            let snapshot = chain
                .append_with_winner(&block, 0, nano_sortition::PoxId::initial(), winner_vrf_seed)
                .expect("contiguous Bitcoin block");
            let reference_header_hash = ReferenceBitcoinHeaderHash(hash);
            let reference_ops_hash = ReferenceOpsHash::from_txids(&[]);
            let parent_index = reference_consensus_hashes.len() - 1;
            let mut previous_hashes = Vec::new();
            for exponent in 0..64 {
                let offset = (1_usize << exponent).saturating_sub(1);
                let Some(index) = parent_index.checked_sub(offset) else {
                    break;
                };
                previous_hashes.push(reference_consensus_hashes[index].clone());
            }
            let reference_consensus_hash = ReferenceConsensusHash::from_ops(
                &reference_header_hash,
                &reference_ops_hash,
                0,
                &previous_hashes,
                &ReferencePoxId::initial(),
            );
            reference_sortition_hash =
                reference_sortition_hash.mix_burn_header(&reference_header_hash);
            if let Some(seed) = winner_vrf_seed {
                reference_sortition_hash =
                    reference_sortition_hash.mix_VRF_seed(&ReferenceVrfSeed(seed));
            }

            assert_eq!(
                snapshot.operations_hash.as_bytes(),
                reference_ops_hash.as_bytes()
            );
            assert_eq!(
                snapshot.consensus_hash.as_bytes(),
                reference_consensus_hash.as_bytes()
            );
            assert_eq!(
                snapshot.sortition_hash.as_bytes(),
                reference_sortition_hash.as_bytes()
            );
            reference_consensus_hashes.push(reference_consensus_hash);
        }
    }

    #[test]
    fn captured_sortition_hashes_form_the_reference_bitcoin_chain() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/sortition/snapshots.json");
        let snapshots: Vec<CapturedSortitionSnapshot> =
            serde_json::from_slice(&fs::read(&path).expect("read captured sortition snapshots"))
                .expect("parse captured sortition snapshots");
        let genesis = snapshots.first().expect("captured genesis snapshot");
        assert_eq!(genesis.block_height, 0);
        let mut chain =
            nano_sortition::SnapshotChain::new(nano_sortition::SortitionSnapshot::genesis(
                genesis.block_height,
                nano_primitives::BitcoinHeaderHash::from_bytes(hex_array(
                    &genesis.burn_header_hash,
                )),
            ));
        assert_eq!(
            chain.tip().sortition_hash.as_bytes(),
            &hex_array(&genesis.sortition_hash)
        );

        for snapshot in snapshots.iter().skip(1) {
            if snapshot.sortition != 0 {
                break;
            }
            assert_eq!(
                chain.tip().bitcoin_header_hash.as_bytes(),
                &hex_array(&snapshot.parent_burn_header_hash)
            );
            let block = nano_bitcoin::BitcoinBlock {
                height: snapshot.block_height,
                hash: hex_array(&snapshot.burn_header_hash),
                operations: Vec::new(),
            };
            let derived = chain
                .append(&block, 0, nano_sortition::PoxId::initial())
                .expect("contiguous captured Bitcoin block");
            assert_eq!(
                derived.sortition_hash.as_bytes(),
                &hex_array(&snapshot.sortition_hash),
                "{}",
                snapshot.block_height
            );
        }
    }

    #[test]
    fn captured_bitcoin_blocks_match_the_recorded_operation_hashes() {
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let snapshots: Vec<CapturedSortitionSnapshot> = serde_json::from_slice(
            &fs::read(fixture_root.join("sortition/snapshots.json"))
                .expect("read captured sortition snapshots"),
        )
        .expect("parse captured sortition snapshots");
        let mut pre_stx_cache = PreStxCache::new();

        for snapshot in snapshots {
            if snapshot.block_height == 0 {
                continue;
            }
            let raw = fs::read_to_string(
                fixture_root
                    .join("bitcoin/blocks")
                    .join(format!("{}.hex", snapshot.burn_header_hash)),
            )
            .unwrap_or_else(|error| {
                panic!(
                    "missing Bitcoin block at height {}: {error}",
                    snapshot.block_height
                )
            });
            let block = decode_block_with_pre_stx(
                snapshot.block_height,
                &hex::decode(raw.trim()).expect("decode captured Bitcoin block"),
                *b"T3",
                &mut pre_stx_cache,
            )
            .unwrap_or_else(|error| {
                panic!(
                    "decode captured Bitcoin block at height {}: {error}",
                    snapshot.block_height
                )
            });
            assert_eq!(
                block.hash,
                hex_array(&snapshot.burn_header_hash),
                "Bitcoin block hash at height {}",
                snapshot.block_height
            );
            assert_eq!(
                nano_sortition::OpsHash::from_txids(
                    &block
                        .operations
                        .iter()
                        .map(|operation| operation.txid)
                        .collect::<Vec<_>>(),
                )
                .as_bytes(),
                &hex_array(&snapshot.ops_hash),
                "operation hash at Bitcoin height {}",
                snapshot.block_height
            );
        }
    }

    #[test]
    fn fixture_authorizations_round_trip_with_nano_codec() {
        let blocks = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/nakamoto/blocks");
        for entry in fs::read_dir(blocks).expect("read fixture blocks") {
            let path = entry.expect("fixture entry").path();
            let bytes = fs::read(&path).expect("read fixture block");
            let block = ReferenceNakamotoBlock::consensus_deserialize(&mut Cursor::new(&bytes))
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            for transaction in block.txs {
                let mut reference = Vec::new();
                transaction
                    .auth
                    .consensus_serialize(&mut reference)
                    .expect("serialize reference auth");
                let (nano, consumed) =
                    NanoTransactionAuth::decode(&reference).expect("decode auth");
                assert_eq!(consumed, reference.len());
                assert_eq!(nano.encode(), reference);
            }
        }
    }

    #[test]
    fn fixture_transactions_round_trip_with_nano_codec() {
        let blocks = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/nakamoto/blocks");
        for entry in fs::read_dir(blocks).expect("read fixture blocks") {
            let path = entry.expect("fixture entry").path();
            let bytes = fs::read(&path).expect("read fixture block");
            let block = ReferenceNakamotoBlock::consensus_deserialize(&mut Cursor::new(&bytes))
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            let mut nano_transactions = Vec::with_capacity(block.txs.len());
            for transaction in &block.txs {
                let mut reference = Vec::new();
                transaction
                    .consensus_serialize(&mut reference)
                    .expect("serialize reference transaction");
                let (nano, consumed) =
                    NanoTransaction::decode(&reference).expect("decode transaction");
                assert_eq!(consumed, reference.len());
                assert_eq!(nano.encode(), reference);
                assert_eq!(nano.txid().as_bytes(), transaction.txid().as_bytes());
                assert_eq!(
                    nano.origin_address().map(|address| address.to_string()),
                    Some(transaction.origin_address().to_string())
                );
                assert_eq!(
                    nano.sponsor_address().map(|address| address.to_string()),
                    transaction
                        .sponsor_address()
                        .map(|address| address.to_string())
                );
                nano_transactions.push(nano);
            }
            assert_eq!(
                transaction_merkle_root(&nano_transactions).as_bytes(),
                &block.header.tx_merkle_root.0
            );
        }
    }

    #[test]
    fn generated_reference_payloads_round_trip_with_nano_codec() {
        let mut payloads = reference_payloads();
        let blocks = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/nakamoto/blocks");
        for entry in fs::read_dir(blocks).expect("read fixture blocks") {
            let bytes = fs::read(entry.expect("fixture entry").path()).expect("read fixture block");
            let block = ReferenceNakamotoBlock::consensus_deserialize(&mut Cursor::new(&bytes))
                .expect("decode fixture block");
            payloads.extend(block.txs.into_iter().map(|transaction| transaction.payload));
        }
        assert!(
            payloads
                .iter()
                .any(|payload| matches!(payload, ReferenceTransactionPayload::TenureChange(_))),
            "fixture corpus contains a tenure-change payload"
        );

        for payload in payloads {
            let transaction = ReferenceStacksTransaction::new(
                ReferenceTransactionVersion::Testnet,
                ReferenceTransactionAuth::from_p2pkh(&ReferenceSecp256k1PrivateKey::from_seed(
                    b"nano-payload-generator",
                ))
                .expect("generated reference authorization is valid"),
                payload,
            );
            let transaction = sign_generated_reference_transaction(transaction, 0, [0x42; 32]);
            assert_reference_transaction_round_trip(&transaction);
        }
    }

    proptest! {
        #[test]
        fn reference_generated_transaction_round_trips_with_nano_codec(
            chain_id in any::<u32>(),
            block_index in any::<usize>(),
            transaction_index in any::<usize>(),
            auth_shape in 0_usize..7,
            key_material in any::<[u8; 32]>(),
        ) {
            let blocks = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/nakamoto/blocks");
            let paths = fs::read_dir(blocks)
                .expect("read fixture blocks")
                .map(|entry| entry.expect("fixture entry").path())
                .collect::<Vec<_>>();
            let path = &paths[block_index % paths.len()];
            let bytes = fs::read(path).expect("read fixture block");
            let mut block = ReferenceNakamotoBlock::consensus_deserialize(&mut Cursor::new(&bytes))
                .expect("decode fixture block");
            let transaction_index = transaction_index % block.txs.len();
            let mut transaction = block.txs.remove(transaction_index);
            transaction.chain_id = chain_id;
            let transaction = sign_generated_reference_transaction(transaction, auth_shape, key_material);

            let mut encoded = Vec::new();
            transaction
                .consensus_serialize(&mut encoded)
                .expect("serialize generated reference transaction");
            let (nano, consumed) = NanoTransaction::decode(&encoded).expect("decode generated transaction");
            prop_assert_eq!(consumed, encoded.len());
            prop_assert_eq!(nano.encode(), encoded);
            let nano_txid = nano.txid();
            let reference_txid = transaction.txid();
            prop_assert_eq!(nano_txid.as_bytes(), reference_txid.as_bytes());
        }
    }

    #[test]
    fn fixture_transaction_mutations_match_stacks_core_acceptance() {
        let blocks = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/nakamoto/blocks");
        for entry in fs::read_dir(blocks).expect("read fixture blocks") {
            let path = entry.expect("fixture entry").path();
            let bytes = fs::read(&path).expect("read fixture block");
            let block = ReferenceNakamotoBlock::consensus_deserialize(&mut Cursor::new(&bytes))
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            for (transaction_index, transaction) in block.txs.into_iter().enumerate() {
                let mut encoded = Vec::new();
                transaction
                    .consensus_serialize(&mut encoded)
                    .expect("serialize reference transaction");
                for index in 0..encoded.len() {
                    let mut mutated = encoded.clone();
                    mutated[index] ^= 0xff;
                    let reference = reference_transaction_decodes(&mutated);
                    let ours = NanoTransaction::decode(&mutated)
                        .is_ok_and(|(_, consumed)| consumed == mutated.len());
                    assert_eq!(
                        ours,
                        reference,
                        "{} transaction {transaction_index} byte {index}",
                        path.display(),
                    );
                }
                for length in 0..encoded.len() {
                    let truncated = &encoded[..length];
                    let reference = reference_transaction_decodes(truncated);
                    let ours = NanoTransaction::decode(truncated)
                        .is_ok_and(|(_, consumed)| consumed == truncated.len());
                    assert_eq!(
                        ours,
                        reference,
                        "{} transaction {transaction_index} truncated to {length} bytes",
                        path.display(),
                    );
                }
            }
        }
    }

    #[test]
    fn secp256k1_matches_stacks_core() {
        let seed = b"nano-stacks compatibility";
        let digest = *sha256(b"signed payload").as_bytes();
        let ours = StacksPrivateKey::from_seed(seed);
        let reference = ReferenceSecp256k1PrivateKey::from_seed(seed);
        let ours_signature = ours.sign(&digest);
        let reference_signature = reference.sign(&digest).expect("signs");

        assert_eq!(ours_signature.as_bytes(), reference_signature.as_bytes());
        assert_eq!(
            ours.public_key().to_bytes_compressed().as_slice(),
            Secp256k1PublicKey::from_private(&reference)
                .to_bytes_compressed()
                .as_slice()
        );

        let high_s = high_s_signature(*ours_signature.as_bytes());
        let high_s = MessageSignature::from_bytes(high_s);
        assert_eq!(
            ours.public_key()
                .verify_transaction(&digest, &high_s)
                .expect_err("transaction signatures reject high-S"),
            CryptoError::HighS
        );
        assert!(
            ours.public_key().verify_signer(&digest, &high_s).is_ok(),
            "signer signatures accept high-S"
        );
        let reference_high_s = ReferenceMessageSignature(high_s.as_bytes().to_owned());
        assert!(Secp256k1PublicKey::recover_to_pubkey(&digest, &reference_high_s).is_err());
        assert!(
            Secp256k1PublicKey::recover_to_pubkey_without_validating_low_s(
                &digest,
                &reference_high_s,
            )
            .is_ok(),
            "reference accepts high-S signer signatures"
        );
        assert!(
            Secp256k1PublicKey::from_private(&reference)
                .verify(&digest, &reference_high_s)
                .is_err(),
            "reference rejects high-S transaction signatures"
        );
    }

    #[test]
    fn vrf_matches_stacks_core() {
        let private_key = [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
            0x1c, 0xae, 0x7f, 0x60,
        ];
        let message = b"";
        let ours_private = VrfPrivateKey::from_bytes(private_key);
        let reference_private =
            ReferenceVrfPrivateKey::from_bytes(&private_key).expect("valid key");
        let ours_proof = Vrf::prove(&ours_private, message).expect("proves");
        let reference_proof = ReferenceVrf::prove(&reference_private, message).expect("proves");

        assert_eq!(ours_proof.to_bytes(), reference_proof.to_bytes());
        let decoded = VrfProof::from_bytes(&reference_proof.to_bytes()).expect("decodes");
        assert!(Vrf::verify(&ours_private.public_key(), &decoded, message).expect("verifies"));
    }

    fn reference_transaction_decodes(bytes: &[u8]) -> bool {
        let mut cursor = Cursor::new(bytes);
        ReferenceStacksTransaction::consensus_deserialize(&mut cursor).is_ok_and(|_| {
            usize::try_from(cursor.position()).expect("cursor fits usize") == bytes.len()
        })
    }

    fn operation_opcode(operation: &nano_bitcoin::BitcoinOperation) -> u8 {
        match &operation.kind {
            nano_bitcoin::BitcoinOperationKind::LeaderBlockCommit { .. } => b'[',
            nano_bitcoin::BitcoinOperationKind::LeaderKeyRegistration { .. } => b'^',
            nano_bitcoin::BitcoinOperationKind::PreStx { .. } => b'p',
            nano_bitcoin::BitcoinOperationKind::StackStx { .. } => b'x',
            nano_bitcoin::BitcoinOperationKind::TransferStx { .. } => b'$',
            nano_bitcoin::BitcoinOperationKind::DelegateStx { .. } => b'#',
            nano_bitcoin::BitcoinOperationKind::VoteForAggregateKey { .. } => b'v',
        }
    }

    fn hex_array(value: &str) -> [u8; 32] {
        let mut bytes = [0; 32];
        hex::decode_to_slice(value, &mut bytes).expect("fixed-size hex hash");
        bytes
    }

    fn reference_bitcoin_script_pubkey(
        address: &blockstack_lib::burnchains::bitcoin::address::BitcoinAddress,
    ) -> Vec<u8> {
        match address {
            blockstack_lib::burnchains::bitcoin::address::BitcoinAddress::Legacy(address) => {
                let mut script = match address.addrtype {
                    blockstack_lib::burnchains::bitcoin::address::LegacyBitcoinAddressType::PublicKeyHash => {
                        vec![0x76, 0xa9, 0x14]
                    }
                    blockstack_lib::burnchains::bitcoin::address::LegacyBitcoinAddressType::ScriptHash => {
                        vec![0xa9, 0x14]
                    }
                };
                script.extend_from_slice(&address.bytes.0);
                script.extend_from_slice(match address.addrtype {
                    blockstack_lib::burnchains::bitcoin::address::LegacyBitcoinAddressType::PublicKeyHash => {
                        &[0x88, 0xac]
                    }
                    blockstack_lib::burnchains::bitcoin::address::LegacyBitcoinAddressType::ScriptHash => {
                        &[0x87]
                    }
                });
                script
            }
            blockstack_lib::burnchains::bitcoin::address::BitcoinAddress::Segwit(address) => {
                let mut script = match address {
                    blockstack_lib::burnchains::bitcoin::address::SegwitBitcoinAddress::P2WPKH(
                        ..,
                    ) => {
                        vec![0x00, 0x14]
                    }
                    blockstack_lib::burnchains::bitcoin::address::SegwitBitcoinAddress::P2WSH(
                        ..,
                    ) => {
                        vec![0x00, 0x20]
                    }
                    blockstack_lib::burnchains::bitcoin::address::SegwitBitcoinAddress::P2TR(
                        ..,
                    ) => {
                        vec![0x51, 0x20]
                    }
                };
                script.extend_from_slice(address.bytes_ref());
                script
            }
        }
    }

    fn operation_data(operation: &nano_bitcoin::BitcoinOperation) -> Vec<u8> {
        let mut data = Vec::new();
        match &operation.kind {
            nano_bitcoin::BitcoinOperationKind::LeaderBlockCommit {
                block_header_hash,
                new_seed,
                parent_block_height,
                parent_transaction_index,
                key_block_height,
                key_transaction_index,
                memo,
                parent_modulus,
            } => {
                data.extend_from_slice(block_header_hash);
                data.extend_from_slice(new_seed);
                data.extend_from_slice(&parent_block_height.to_be_bytes());
                data.extend_from_slice(&parent_transaction_index.to_be_bytes());
                data.extend_from_slice(&key_block_height.to_be_bytes());
                data.extend_from_slice(&key_transaction_index.to_be_bytes());
                data.push((memo << 3) | parent_modulus);
            }
            nano_bitcoin::BitcoinOperationKind::LeaderKeyRegistration {
                consensus_hash,
                vrf_public_key,
                memo,
                ..
            } => {
                data.extend_from_slice(consensus_hash);
                data.extend_from_slice(vrf_public_key);
                data.extend_from_slice(memo);
            }
            nano_bitcoin::BitcoinOperationKind::PreStx { .. } => {}
            nano_bitcoin::BitcoinOperationKind::StackStx {
                amount,
                cycles,
                signer_key,
                max_amount,
                authorization_id,
                ..
            } => {
                data.extend_from_slice(&amount.to_be_bytes());
                data.push(*cycles);
                if let Some(signer_key) = signer_key {
                    data.extend_from_slice(signer_key);
                }
                if let Some(max_amount) = max_amount {
                    data.extend_from_slice(&max_amount.to_be_bytes());
                }
                if let Some(authorization_id) = authorization_id {
                    data.extend_from_slice(&authorization_id.to_be_bytes());
                }
            }
            nano_bitcoin::BitcoinOperationKind::TransferStx { amount, memo, .. } => {
                data.extend_from_slice(&amount.to_be_bytes());
                data.extend_from_slice(memo);
            }
            nano_bitcoin::BitcoinOperationKind::DelegateStx {
                amount,
                reward_address_output,
                until_bitcoin_height,
                ..
            } => {
                data.extend_from_slice(&amount.to_be_bytes());
                match reward_address_output {
                    Some(index) => {
                        data.push(1);
                        data.extend_from_slice(&index.to_be_bytes());
                    }
                    None => data.push(0),
                }
                match until_bitcoin_height {
                    Some(height) => {
                        data.push(1);
                        data.extend_from_slice(&height.to_be_bytes());
                    }
                    None => data.push(0),
                }
            }
            nano_bitcoin::BitcoinOperationKind::VoteForAggregateKey {
                signer_index,
                aggregate_key,
                round,
                reward_cycle,
                ..
            } => {
                data.extend_from_slice(&signer_index.to_be_bytes());
                data.extend_from_slice(aggregate_key);
                data.extend_from_slice(&round.to_be_bytes());
                data.extend_from_slice(&reward_cycle.to_be_bytes());
            }
        }
        data
    }

    fn reference_payloads() -> Vec<ReferenceTransactionPayload> {
        let principal: PrincipalData = StandardPrincipalData::transient().into();
        let address = ReferenceStacksAddress::new(26, ReferenceHash160([0x24; 20]))
            .expect("valid generated contract address");
        let key = ReferenceSecp256k1PrivateKey::from_seed(b"nano-poison-generator");
        let mut first = StacksMicroblockHeader {
            version: 0,
            sequence: 0,
            prev_block: BlockHeaderHash([0; 32]),
            tx_merkle_root: Sha512Trunc256Sum([0; 32]),
            signature: ReferenceMessageSignature::empty(),
        };
        first.sign(&key).expect("generated microblock signs");
        let mut second = first.clone();
        second.sequence = 1;
        second.sign(&key).expect("generated microblock signs");

        vec![
            ReferenceTransactionPayload::TokenTransfer(
                principal.clone(),
                42,
                TokenTransferMemo([0; 34]),
            ),
            ReferenceTransactionPayload::new_contract_call(
                address,
                "contract",
                "function",
                vec![Value::UInt(42)],
            )
            .expect("generated contract call is valid"),
            ReferenceTransactionPayload::new_smart_contract(
                "contract",
                "(define-public (function) (ok true))",
                None,
            )
            .expect("generated smart contract is valid"),
            ReferenceTransactionPayload::new_smart_contract(
                "contract",
                "(define-public (function) (ok true))",
                Some(ReferenceClarityVersion::Clarity6),
            )
            .expect("generated versioned smart contract is valid"),
            ReferenceTransactionPayload::PoisonMicroblock(first, second),
            ReferenceTransactionPayload::Coinbase(CoinbasePayload([0; 32]), None, None),
            ReferenceTransactionPayload::Coinbase(
                CoinbasePayload([0; 32]),
                Some(principal.clone()),
                None,
            ),
            ReferenceTransactionPayload::Coinbase(
                CoinbasePayload([0; 32]),
                Some(principal),
                Some(ReferenceVrfProof::empty()),
            ),
            ReferenceTransactionPayload::Coinbase(
                CoinbasePayload([0; 32]),
                None,
                Some(ReferenceVrfProof::empty()),
            ),
        ]
    }

    fn assert_reference_transaction_round_trip(transaction: &ReferenceStacksTransaction) {
        let mut encoded = Vec::new();
        transaction
            .consensus_serialize(&mut encoded)
            .expect("serialize generated reference transaction");
        let (nano, consumed) =
            NanoTransaction::decode(&encoded).expect("decode generated reference transaction");
        assert_eq!(consumed, encoded.len());
        assert_eq!(nano.encode(), encoded);
        assert_eq!(nano.txid().as_bytes(), transaction.txid().as_bytes());
    }

    fn sign_generated_reference_transaction(
        mut transaction: ReferenceStacksTransaction,
        shape: usize,
        key_material: [u8; 32],
    ) -> ReferenceStacksTransaction {
        let keys = [0_u8, 1, 2].map(|suffix| {
            let mut seed = key_material.to_vec();
            seed.push(suffix);
            ReferenceSecp256k1PrivateKey::from_seed(&seed)
        });
        let standard = match shape % 6 {
            0 => ReferenceTransactionAuth::from_p2pkh(&keys[0]),
            1 => ReferenceTransactionAuth::from_p2wpkh(&keys[0]),
            2 => ReferenceTransactionAuth::from_p2sh(&keys, 2),
            3 => ReferenceTransactionAuth::from_p2wsh(&keys, 2),
            4 => ReferenceTransactionAuth::from_order_independent_p2sh(&keys, 2),
            5 => ReferenceTransactionAuth::from_order_independent_p2wsh(&keys, 2),
            _ => unreachable!(),
        }
        .expect("generated reference authorization is valid");
        transaction.auth = if shape == 6 {
            standard
                .into_sponsored(
                    ReferenceTransactionAuth::from_p2pkh(&keys[1])
                        .expect("generated sponsor authorization is valid"),
                )
                .expect("generated sponsored authorization is valid")
        } else {
            standard
        };
        let mut signer = ReferenceStacksTransactionSigner::new(&transaction);
        signer
            .sign_origin(&keys[0])
            .expect("generated origin signature is valid");
        if (2..6).contains(&shape) {
            signer
                .sign_origin(&keys[1])
                .expect("generated origin signature is valid");
        }
        if shape == 6 {
            signer
                .sign_sponsor(&keys[1])
                .expect("generated sponsor signature is valid");
        }
        signer.get_tx().expect("generated transaction is complete")
    }

    fn temporary_fixture_root() -> Result<PathBuf, std::io::Error> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before the Unix epoch")
            .as_nanos();
        let path = PathBuf::from("/tmp").join(format!("nano-stacks-fixtures-{unique}"));
        fs::create_dir(&path)?;
        Ok(path)
    }

    fn write_file(path: &Path, contents: &str) -> Result<(), std::io::Error> {
        let parent = path.parent().expect("fixture path has a parent");
        fs::create_dir_all(parent)?;
        fs::write(path, contents)
    }

    fn uint_from_words(words: [u64; 4]) -> nano_primitives::Uint256 {
        let mut bytes = [0; 32];
        for (index, word) in words.iter().enumerate() {
            bytes[index * 8..(index + 1) * 8].copy_from_slice(&word.to_le_bytes());
        }
        nano_primitives::Uint256::from_little_endian(&bytes)
    }

    fn uint_to_words(value: nano_primitives::Uint256) -> [u64; 4] {
        let bytes = value.to_little_endian();
        std::array::from_fn(|index| {
            u64::from_le_bytes(
                bytes[index * 8..(index + 1) * 8]
                    .try_into()
                    .expect("word slice"),
            )
        })
    }

    fn high_s_signature(mut signature: [u8; 65]) -> [u8; 65] {
        const CURVE_ORDER: [u8; 32] = [
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xfe, 0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c,
            0xd0, 0x36, 0x41, 0x41,
        ];
        let mut borrow = 0_i16;
        for index in (0..32).rev() {
            let difference =
                i16::from(CURVE_ORDER[index]) - i16::from(signature[index + 33]) - borrow;
            signature[index + 33] =
                u8::try_from(difference.rem_euclid(256)).expect("remainder fits in a byte");
            borrow = i16::from(difference < 0);
        }
        signature[0] ^= 1;
        signature
    }
}
