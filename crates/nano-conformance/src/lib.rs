use std::{
    collections::BTreeMap,
    fmt::Write,
    fs,
    path::{Path, PathBuf},
};

use nano_bitcoin::{BitcoinOperation, BitcoinOperationKind, PreStxCache, decode_block_with_pre_stx};
use nano_chainstate::{BitcoinBlockContext, ChainState, NakamotoBlock, TenureAccounting};
use nano_primitives::{Network, TrieHash};
use serde::Deserialize;

/// The minimum metadata needed to make replay depth visible before fixture
/// capture is available.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FixtureManifest {
    pub mode: FixtureMode,
    pub replay_blocks: u64,
}

/// Whether the fixture directory holds the empty baseline the scoreboard starts
/// from, or a real capture the conformance tests can consume.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FixtureMode {
    Baseline,
    Captured,
}

#[derive(Deserialize)]
struct CapturedBitcoinSnapshot {
    block_height: u64,
    burn_header_hash: String,
    burn_header_timestamp: u64,
    consensus_hash: String,
    winning_block_txid: String,
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

    for relative_path in [
        "sortition/snapshots.json",
        "provenance.toml",
        "chainstate/checkpoint-H/native-effects.json",
    ] {
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
    let accounting_path = checkpoint.join("native-effects.json");
    TenureAccounting::from_json(
        &fs::read(&accounting_path)
            .map_err(|_| FixtureValidationError::MissingOrEmptyFile(accounting_path.clone()))?,
    )
    .map_err(|_| FixtureValidationError::InvalidNativeAccounting(accounting_path))?;

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
    InvalidNativeAccounting(PathBuf),
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
            Self::InvalidNativeAccounting(path) => write!(
                formatter,
                "invalid native accounting fixture: {}",
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
    /// Costs are reported on their own, because they only change consensus when
    /// a block nears a limit; the state root and receipts do not depend on them.
    pub first_cost_divergence: Option<(u64, String)>,
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
    ReplayDepth {
        completed: 0,
        expected: manifest.replay_blocks,
        first_failure: (manifest.replay_blocks > 0).then_some(1),
        first_divergence: None,
        first_cost_divergence: None,
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
    let first_failure = replay.first_failure.map_or_else(
        || "—".to_owned(),
        |height| {
            replay.first_divergence.as_ref().map_or_else(
                || format!("block {height}"),
                |divergence| format!("block {height}: {divergence}"),
            )
        },
    );
    let _ = writeln!(
        output,
        "fixtures              offline integrity          {fixture_status}  —"
    );
    let _ = writeln!(
        output,
        "replay: state root   fixture block headers       {}/{}          {}",
        replay.completed, replay.expected, first_failure
    );
    let _ = writeln!(
        output,
        "replay: receipts     event observer receipts     {}/{}          {}",
        replay.completed, replay.expected, first_failure
    );
    let costs = replay.first_cost_divergence.as_ref().map_or_else(
        || format!("{}/{}          —", replay.completed, replay.expected),
        |(height, reason)| {
            format!(
                "{}/{}          block {height}: {reason}",
                height.saturating_sub(1),
                replay.expected
            )
        },
    );
    let _ = writeln!(
        output,
        "replay: costs        receipt cost dimensions     {costs}"
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

/// Replay the captured block stream, handing every executed block and its
/// receipts to the caller.
///
/// The scoreboard only needs the depth; anything that has to see what
/// execution produced — an event payload, a receipt, a cost — replays through
/// here rather than rebuilding the checkpoint plumbing.
pub fn replay_captured_blocks(
    root: &Path,
    blocks: u64,
    visit: &mut dyn FnMut(&NakamotoBlock, &nano_chainstate::AppliedBlock),
) -> ReplayDepth {
    captured_replay_visiting(
        root,
        FixtureManifest {
            mode: FixtureMode::Captured,
            replay_blocks: blocks,
        },
        visit,
    )
}

fn captured_replay(root: &Path, manifest: FixtureManifest) -> ReplayDepth {
    captured_replay_visiting(root, manifest, &mut |_, _| {})
}

fn captured_replay_visiting(
    root: &Path,
    manifest: FixtureManifest,
    visit: &mut dyn FnMut(&NakamotoBlock, &nano_chainstate::AppliedBlock),
) -> ReplayDepth {
    let (mut chainstate, source) = match replay_chainstate(root) {
        Ok(chainstate) => chainstate,
        Err(message) => return replay_fixture_failure(manifest, message),
    };
    let Some(snapshots) = captured_bitcoin_snapshots(root) else {
        return replay_fixture_failure(manifest, "captured Bitcoin snapshots are unavailable");
    };
    let Some(bitcoin_operations) = captured_bitcoin_operations(root) else {
        return replay_fixture_failure(manifest, "captured Bitcoin operations are unavailable");
    };
    let Ok(mut entries) = fs::read_dir(root.join("nakamoto/blocks")) else {
        return replay_fixture_failure(manifest, "captured blocks are unavailable");
    };
    let mut paths = entries
        .by_ref()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    paths.sort();

    let mut completed = 0;
    let mut parent = Some(source);
    let mut bitcoin_view = String::new();
    let mut first_cost_divergence = None;
    for (offset, path) in paths.into_iter().enumerate() {
        if completed >= manifest.replay_blocks {
            break;
        }
        let block_number = u64::try_from(offset).unwrap_or(u64::MAX).saturating_add(1);
        let (block, applied, cost_divergence) = match apply_captured_block(
            root,
            &mut chainstate,
            &snapshots,
            &bitcoin_operations,
            parent,
            &mut bitcoin_view,
            &path,
        ) {
            Ok(block) => block,
            Err(divergence) => {
                return ReplayDepth {
                    completed,
                    expected: manifest.replay_blocks,
                    first_failure: Some(block_number),
                    first_divergence: Some(divergence),
                    first_cost_divergence,
                };
            }
        };
        if first_cost_divergence.is_none() {
            first_cost_divergence = cost_divergence.map(|reason| (block_number, reason));
        }
        visit(&block, &applied);
        parent = Some(*block.block_id().as_bytes());
        completed += 1;
    }
    ReplayDepth {
        completed,
        expected: manifest.replay_blocks,
        first_failure: (completed < manifest.replay_blocks).then_some(completed + 1),
        first_divergence: None,
        first_cost_divergence,
    }
}

fn replay_chainstate(root: &Path) -> Result<(ChainState, [u8; 32]), &'static str> {
    let (source, state_root) =
        checkpoint_state(root).ok_or("checkpoint metadata is unavailable")?;
    let checkpoint = root.join("chainstate/checkpoint-H/marf.sqlite");
    let mut chainstate =
        ChainState::from_checkpoint(captured_network(root), checkpoint, source, state_root)
            .map_err(|_| "checkpoint cannot be opened")?;
    let accounting = fs::read(root.join("chainstate/checkpoint-H/native-effects.json"))
        .ok()
        .and_then(|contents| TenureAccounting::from_json(&contents).ok())
        .ok_or("native accounting fixture cannot be loaded")?;
    *chainstate.accounting_mut() = accounting;
    Ok((chainstate, source))
}

fn replay_fixture_failure(manifest: FixtureManifest, message: &str) -> ReplayDepth {
    ReplayDepth {
        completed: 0,
        expected: manifest.replay_blocks,
        first_failure: Some(1),
        first_divergence: Some(ReplayDivergence::Fixture(message.to_owned())),
        first_cost_divergence: None,
    }
}

fn apply_captured_block(
    root: &Path,
    chainstate: &mut ChainState,
    snapshots: &BTreeMap<String, BitcoinBlockContext>,
    bitcoin_operations: &BTreeMap<String, Vec<BitcoinOperation>>,
    parent: Option<[u8; 32]>,
    bitcoin_view: &mut String,
    path: &Path,
) -> Result<(NakamotoBlock, nano_chainstate::AppliedBlock, Option<String>), ReplayDivergence> {
    let bytes =
        fs::read(path).map_err(|_| ReplayDivergence::Fixture("block cannot be read".to_owned()))?;
    let block = NakamotoBlock::decode(&bytes)
        .map_err(|_| ReplayDivergence::Fixture("block cannot be decoded".to_owned()))?;
    // A tenure extend moves the Clarity burn view without starting a tenure, so
    // the view carries forward until the next tenure change moves it again.
    if let Some(view) = block.bitcoin_view_consensus_hash() {
        *bitcoin_view = view.to_string();
    } else if bitcoin_view.is_empty() {
        // Replay can start mid-tenure, where the view is the tenure's own sortition.
        *bitcoin_view = block.header.consensus_hash.to_string();
    }
    let mut bitcoin_context = *snapshots.get(bitcoin_view.as_str()).ok_or_else(|| {
        ReplayDivergence::Fixture(
            "block Bitcoin view is absent from captured Bitcoin snapshots".to_owned(),
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
    bitcoin_context.pox_5_activation_height = event.v4;
    let operations = bitcoin_operations
        .get(&block.header.consensus_hash.to_string())
        .ok_or_else(|| {
            ReplayDivergence::Fixture(
                "block consensus hash is absent from captured Bitcoin operations".to_owned(),
            )
        })?;
    let applied = chainstate
        .execute_nakamoto_block_with_bitcoin_operations(bitcoin_context, operations, parent, &block)
        .map_err(|error| ReplayDivergence::Application(error.to_string()))?;
    let cost_divergence = compare_receipts(&event, &applied.receipts)?;
    let actual = TrieHash::from_bytes(applied.execution.state_root.0);
    if actual != block.header.state_index_root {
        return Err(ReplayDivergence::StateRoot {
            expected: block.header.state_index_root,
            actual,
        });
    }
    Ok((block, applied, cost_divergence))
}

/// Compare a block's receipts, returning any cost difference separately.
fn compare_receipts(
    event: &CapturedBlockEvent,
    receipts: &[nano_chainstate::TransactionReceipt],
) -> Result<Option<String>, ReplayDivergence> {
    if event.transactions.len() != receipts.len() {
        return Err(ReplayDivergence::Receipt(
            "transaction count differs".to_owned(),
        ));
    }
    for (index, (expected, actual)) in event.transactions.iter().zip(receipts).enumerate() {
        let status = match &actual.status {
            nano_chainstate::TransactionStatus::Success => "success",
            nano_chainstate::TransactionStatus::PostConditionAborted(_) => {
                "abort_by_post_condition"
            }
            nano_chainstate::TransactionStatus::AbortedByResponse
            | nano_chainstate::TransactionStatus::RuntimeFailure(_) => "abort_by_response",
        };
        if expected.status != status {
            return Err(ReplayDivergence::Receipt(format!(
                "transaction {index} ({:?}) status differs: expected {}, got {status} ({:?})",
                actual.txid, expected.status, actual.status
            )));
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
            return Ok(Some(format!(
                "transaction {index} ({:?}) cost differs: expected ({}, {}, {}, {}, {}), got ({}, {}, {}, {}, {})",
                actual.txid,
                expected.execution_cost.read_count,
                expected.execution_cost.read_length,
                expected.execution_cost.runtime,
                expected.execution_cost.write_count,
                expected.execution_cost.write_length,
                cost.read_count,
                cost.read_length,
                cost.runtime,
                cost.write_count,
                cost.write_length,
            )));
        }
    }
    let actual_events = receipts
        .iter()
        .flat_map(|receipt| {
            receipt
                .result
                .events
                .iter()
                .map(move |entry| (entry, receipt.txid, receipt.committed))
        })
        .enumerate()
        .map(|(index, (entry, txid, committed))| entry.json_serialize(index, &txid, committed))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ReplayDivergence::Receipt("event cannot serialize".to_owned()))?;
    let mut expected_events = event.events.clone();
    expected_events
        .sort_by_key(|entry| entry.get("event_index").and_then(serde_json::Value::as_u64));
    if expected_events != actual_events {
        return Err(ReplayDivergence::Receipt("events differ".to_owned()));
    }
    Ok(None)
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

/// Read a scalar field from the capture's provenance record.
fn provenance_field(root: &Path, name: &str) -> Option<String> {
    let provenance = fs::read_to_string(root.join("provenance.toml")).ok()?;
    provenance
        .lines()
        .find_map(|line| line.trim().strip_prefix(&format!("{name} = ")))
        .map(|value| value.trim().trim_matches('"').to_owned())
}

/// The chain the fixtures were captured from.
///
/// Replay has to execute a capture as the network that produced it: the flag
/// and the identifier both reach the state root.
pub fn captured_network(root: &Path) -> Network {
    provenance_field(root, "chain_id")
        .and_then(|value| {
            value
                .strip_prefix("0x")
                .map_or_else(|| value.parse().ok(), |hex| u32::from_str_radix(hex, 16).ok())
        })
        .map_or(Network::TESTNET, Network::from_chain_id)
}

/// The matured rewards a capture owes for tenures earned before its window.
#[cfg(test)]
fn captured_accounting(root: &Path) -> Option<TenureAccounting> {
    let contents = fs::read(root.join("chainstate/checkpoint-H/native-effects.json")).ok()?;
    TenureAccounting::from_json(&contents).ok()
}

/// Open the captured checkpoint the way replay does, accounting included.
///
/// A window that opens part way through the chain owes rewards earned before
/// it, so a chainstate without the capture's accounting fails the moment one
/// of those tenures matures.
#[cfg(test)]
fn captured_chainstate(root: &Path) -> ChainState {
    let (source, state_root) = checkpoint_state(root).expect("checkpoint metadata");
    let mut chainstate = ChainState::from_checkpoint(
        captured_network(root),
        root.join("chainstate/checkpoint-H/marf.sqlite"),
        source,
        state_root,
    )
    .expect("open checkpoint");
    let accounting = fs::read(root.join("chainstate/checkpoint-H/native-effects.json"))
        .ok()
        .and_then(|contents| TenureAccounting::from_json(&contents).ok())
        .expect("captured native accounting");
    *chainstate.accounting_mut() = accounting;
    chainstate
}

/// Every reward set the capture recorded, by the cycle it pays.
///
/// A block is signed by the set of its own cycle, and a window long enough to
/// cross a rollover spans more than one.
#[cfg(test)]
fn captured_signer_sets(root: &Path) -> BTreeMap<u64, nano_chainstate::SignerSet> {
    #[derive(Deserialize)]
    struct SignerWire {
        signing_key: String,
        stacked_amt: u64,
    }
    #[derive(Deserialize)]
    struct RewardSetWire {
        signers: Vec<SignerWire>,
    }
    #[derive(Deserialize)]
    struct StackerSetWire {
        stacker_set: RewardSetWire,
    }

    let mut sets = BTreeMap::new();
    for entry in fs::read_dir(root.join("stacker_set")).expect("read reward sets") {
        let path = entry.expect("reward set entry").path();
        let Some(cycle) = path
            .file_stem()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("cycle-"))
            .and_then(|cycle| cycle.parse::<u64>().ok())
        else {
            continue;
        };
        let Ok(reward_set) =
            serde_json::from_slice::<StackerSetWire>(&fs::read(&path).expect("read reward set"))
        else {
            continue;
        };
        let signers = reward_set
            .stacker_set
            .signers
            .into_iter()
            .map(|signer| {
                (
                    nano_crypto::StacksPublicKey::from_bytes(
                        &hex::decode(signer.signing_key).expect("decode signing key"),
                    )
                    .expect("valid signer key"),
                    u128::from(signer.stacked_amt),
                )
            })
            .collect();
        if let Ok((set, _)) = nano_chainstate::SignerSet::from_reward_slots(signers, 30) {
            sets.insert(cycle, set);
        }
    }
    sets
}

/// The reward set the capture's cycle stacked, as a signer set.
#[cfg(test)]
fn captured_signer_set(root: &Path) -> nano_chainstate::SignerSet {
    #[derive(Deserialize)]
    struct SignerWire {
        signing_key: String,
        stacked_amt: u64,
    }
    #[derive(Deserialize)]
    struct RewardSetWire {
        signers: Vec<SignerWire>,
    }
    #[derive(Deserialize)]
    struct StackerSetWire {
        stacker_set: RewardSetWire,
    }

    let mut paths = fs::read_dir(root.join("stacker_set"))
        .expect("read reward sets")
        .map(|entry| entry.expect("reward set entry").path())
        .collect::<Vec<_>>();
    paths.sort();
    let path = paths.first().expect("a captured reward set");
    let reward_set: StackerSetWire =
        serde_json::from_slice(&fs::read(path).expect("read reward set")).expect("decode reward set");
    let signers = reward_set
        .stacker_set
        .signers
        .into_iter()
        .map(|signer| {
            (
                nano_crypto::StacksPublicKey::from_bytes(
                    &hex::decode(signer.signing_key).expect("decode signing key"),
                )
                .expect("valid signer key"),
                u128::from(signer.stacked_amt),
            )
        })
        .collect();
    nano_chainstate::SignerSet::from_reward_slots(signers, 30)
        .expect("build the captured signer set")
        .0
}

/// The burnchain magic the captured chain prefixes its `OP_RETURN`s with.
fn captured_magic(root: &Path) -> [u8; 2] {
    provenance_field(root, "bitcoin_magic")
        .and_then(|value| value.as_bytes().try_into().ok())
        .unwrap_or(*b"T3")
}

fn captured_bitcoin_snapshots(root: &Path) -> Option<BTreeMap<String, BitcoinBlockContext>> {
    let snapshots: Vec<CapturedBitcoinSnapshot> =
        serde_json::from_slice(&fs::read(root.join("sortition/snapshots.json")).ok()?).ok()?;
    // A block's reward cycle position decides whether it sets up a signer set,
    // so replay needs the captured network's stacking calendar.
    let field = |name: &str| -> Option<u64> { provenance_field(root, name)?.parse().ok() };
    let first_height = field("pox_first_bitcoin_height")?;
    let prepare_phase_length = u32::try_from(field("pox_prepare_phase_length")?).ok()?;
    let reward_phase_length = u32::try_from(field("pox_reward_phase_length")?).ok()?;
    let operations = captured_bitcoin_operations(root)?;
    snapshots
        .into_iter()
        .map(|snapshot| {
            // What Clarity reads back about the tenure's burn block, which the
            // capture holds either in the snapshot or in the Bitcoin block.
            let commits = operations.get(&snapshot.consensus_hash);
            let winner = decode_hash(&snapshot.winning_block_txid);
            let burn = |operation: &BitcoinOperation| -> u128 {
                operation
                    .outputs
                    .iter()
                    .map(|output| u128::from(output.amount_sats))
                    .sum()
            };
            let (vrf_seed, burn_spend_winner) = commits
                .and_then(|commits| {
                    commits.iter().find_map(|operation| match operation.kind {
                        BitcoinOperationKind::LeaderBlockCommit { new_seed, .. }
                            if Some(operation.txid) == winner =>
                        {
                            Some((new_seed, burn(operation)))
                        }
                        _ => None,
                    })
                })
                .unwrap_or(([0; 32], 0));
            let burn_spend_total = commits.map_or(0, |commits| {
                commits
                    .iter()
                    .filter(|operation| {
                        matches!(
                            operation.kind,
                            BitcoinOperationKind::LeaderBlockCommit { .. }
                        )
                    })
                    .map(burn)
                    .sum()
            });
            Some((
                snapshot.consensus_hash.clone(),
                BitcoinBlockContext {
                    first_height,
                    prepare_phase_length,
                    reward_phase_length,
                    burn_header_hash: decode_hash(&snapshot.burn_header_hash)?,
                    burn_block_time: snapshot.burn_header_timestamp,
                    vrf_seed,
                    burn_spend_total,
                    burn_spend_winner,
                    ..BitcoinBlockContext::at_height(snapshot.block_height)
                },
            ))
        })
        .collect()
}

fn captured_bitcoin_operations(root: &Path) -> Option<BTreeMap<String, Vec<BitcoinOperation>>> {
    let mut snapshots: Vec<CapturedBitcoinSnapshot> =
        serde_json::from_slice(&fs::read(root.join("sortition/snapshots.json")).ok()?).ok()?;
    snapshots.sort_by_key(|snapshot| snapshot.block_height);
    let magic = captured_magic(root);
    let mut cache = PreStxCache::new();
    snapshots
        .into_iter()
        .map(|snapshot| {
            let encoded = fs::read_to_string(
                root.join("bitcoin/blocks")
                    .join(format!("{}.hex", snapshot.burn_header_hash)),
            )
            .ok()?;
            let raw = hex::decode(encoded.trim()).ok()?;
            let block =
                decode_block_with_pre_stx(snapshot.block_height, &raw, magic, &mut cache).ok()?;
            Some((snapshot.consensus_hash, block.operations))
        })
        .collect()
}

fn checkpoint_manifest(root: &Path) -> Option<nano_marf::CheckpointManifest> {
    nano_marf::CheckpointManifest::load(root.join("chainstate/checkpoint-H")).ok()
}

fn checkpoint_state(root: &Path) -> Option<([u8; 32], TrieHash)> {
    let manifest = checkpoint_manifest(root)?;
    Some((manifest.source_state_id, manifest.state_index_root))
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
    use std::collections::BTreeMap;

    use super::{
        ChainState, FixtureManifest, FixtureMode, FixtureStatus, apply_captured_block,
        baseline_replay, captured_bitcoin_operations, captured_bitcoin_snapshots, captured_network,
        captured_accounting, captured_chainstate, captured_signer_set, captured_signer_sets,
        checkpoint_manifest,
        checkpoint_state,
        decode_hash, scoreboard, validate_fixture_tree,
    };
    use nano_sortition::BURN_BLOCK_MINED_AT_MODULUS;
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
            TransactionAuth as ReferenceTransactionAuth, TransactionAuthVerificationMode,
            TransactionPayload as ReferenceTransactionPayload,
            TransactionVersion as ReferenceTransactionVersion,
        },
    };
    use blockstack_lib::core::StacksEpochId;
    use clarity::vm::ClarityVersion as ReferenceClarityVersion;
    use clarity::vm::costs::LimitedCostTracker;
    use clarity::vm::types::{PrincipalData, StandardPrincipalData, Value};
    use nano_address::{PoxAddress, PoxAddressType20, PoxAddressType32, StacksAddress};
    use nano_bitcoin::{
        BitcoinBlock, BitcoinSource, PreStxCache, decode_block as decode_bitcoin_block,
        decode_block_with_pre_stx,
    };
    use nano_chainstate::{BitcoinBlockContext, NakamotoBlock as NanoNakamotoBlock, SignerSet};
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
    use nano_node::{
        Checkpoint, CheckpointExecutor, CheckpointTrustError, adopt_checkpoint, attest_checkpoint,
    };
    use nano_primitives::{BitVec, Network, TrieHash, hash160, sha256, sha512, sha512_256};
    use nano_signer::{ChainstateProposalValidator, ProposalValidator};
    use nano_stackerdb::{BlockAcceptance, BlockProposal, BlockResponse, Chunk, SignerMessage};
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

    #[derive(Debug)]
    struct FixtureBitcoinSource {
        blocks: BTreeMap<u64, BitcoinBlock>,
    }

    impl BitcoinSource for FixtureBitcoinSource {
        type Error = String;

        fn block_at(&mut self, height: u64) -> Result<BitcoinBlock, Self::Error> {
            self.blocks
                .get(&height)
                .cloned()
                .ok_or_else(|| format!("missing captured Bitcoin block at height {height}"))
        }
    }
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

    #[derive(Clone, Deserialize)]
    struct CapturedSortitionSnapshot {
        block_height: u64,
        burn_header_hash: String,
        parent_burn_header_hash: String,
        sortition_id: String,
        parent_sortition_id: String,
        consensus_hash: String,
        ops_hash: String,
        total_burn: String,
        pox_valid: u8,
        sortition: u8,
        sortition_hash: String,
        winning_block_txid: String,
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
        let manifest = FixtureManifest::load(&root.join("manifest.toml")).expect("manifest");
        assert_eq!(
            validate_fixture_tree(&root).expect("captured fixture directory is valid"),
            FixtureStatus::Captured {
                replay_blocks: manifest.replay_blocks
            }
        );
        assert!(fs::metadata(root.join("README.md")).is_ok());
    }

    /// The captured corpus is recaptured wholesale, so tests address its blocks
    /// by position rather than by the name of any one capture.
    fn captured_block_paths(fixture: &Path) -> Vec<PathBuf> {
        let mut paths = fs::read_dir(fixture.join("nakamoto/blocks"))
            .expect("read captured blocks")
            .map(|entry| entry.expect("captured block entry").path())
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    #[test]
    fn checkpoint_graph_import_matches_the_published_root() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let (source, root) = checkpoint_state(&fixture).expect("checkpoint metadata");
        let checkpoint = fixture.join("chainstate/checkpoint-H/marf.sqlite");
        let imported = import_checkpoint(checkpoint, source, root).expect("imports checkpoint");
        assert_eq!(imported.root(source), Some(root));
    }

    /// A checkpoint is trusted because signers signed its root, not because it
    /// says so.
    ///
    /// `signer_signature_hash` covers `state_index_root`, so a header the
    /// reward set put threshold weight behind states what the state root at
    /// that height was. The capture starts one block after the checkpoint, so
    /// the attestation runs against a captured block treated as a checkpoint —
    /// the mechanism is the same one a mainnet checkpoint goes through.
    #[test]
    fn a_signed_header_attests_the_checkpoint_it_sealed() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let signers = captured_signer_set(&fixture);
        let published = checkpoint_manifest(&fixture).expect("checkpoint manifest");
        let block = NanoNakamotoBlock::decode(
            &fs::read(captured_block_paths(&fixture).first().expect("captured block"))
                .expect("read block"),
        )
        .expect("decode block");
        let manifest = nano_marf::CheckpointManifest {
            stacks_height: block.header.chain_length,
            source_state_id: *block.block_id().as_bytes(),
            state_index_root: block.header.state_index_root,
            ..published
        };

        let directory = tempfile::tempdir().expect("state directory");
        let attestation = adopt_checkpoint(&directory, &manifest, &block.header, &signers)
            .expect("attested checkpoint");
        assert!(
            attestation.signer_weight >= attestation.approval_threshold,
            "attestation accepted a header the reward set did not approve"
        );
        assert_eq!(
            nano_marf::CheckpointProvenance::load(&directory)
                .expect("read provenance")
                .and_then(|provenance| provenance.attestation),
            Some(attestation),
            "adopting a checkpoint left no record of what attested it"
        );

        let mut tampered_root = *manifest.state_index_root.as_bytes();
        tampered_root[0] ^= 0x01;
        let tampered = nano_marf::CheckpointManifest {
            state_index_root: TrieHash::from_bytes(tampered_root),
            ..manifest.clone()
        };
        assert!(
            matches!(
                attest_checkpoint(&tampered, &block.header, &signers),
                Err(CheckpointTrustError::StateRoot { .. })
            ),
            "a checkpoint root the signed header does not carry was accepted"
        );

        let mut header = block.header;
        header.signer_signatures.clear();
        assert!(
            matches!(
                attest_checkpoint(&manifest, &header, &signers),
                Err(CheckpointTrustError::Signers(_))
            ),
            "an unsigned header attested a checkpoint"
        );
    }

    /// An import is refused when the caller's root is not the published one.
    ///
    /// The two roots are separate claims — one from the operator's
    /// configuration, one from the checkpoint itself — and a checkpoint being
    /// self-consistent with a root nobody published proves nothing.
    #[test]
    fn a_declared_root_that_is_not_the_published_one_is_refused() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let (source, root) = checkpoint_state(&fixture).expect("checkpoint metadata");
        let checkpoint = fixture.join("chainstate/checkpoint-H/marf.sqlite");
        let mut tampered = *root.as_bytes();
        tampered[0] ^= 0x01;

        assert!(
            matches!(
                import_checkpoint(&checkpoint, source, TrieHash::from_bytes(tampered)),
                Err(nano_marf::CheckpointError::DeclaredRootMismatch { .. })
            ),
            "a root the checkpoint does not publish was imported"
        );
    }

    /// A node remembers which checkpoint its state descends from.
    ///
    /// Without the record a restart re-reads the configuration and believes
    /// whatever it now says, so swapping the configured checkpoint would graft
    /// one chain's blocks onto another chain's state.
    #[test]
    fn checkpoint_provenance_survives_a_restart() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let manifest = checkpoint_manifest(&fixture).expect("checkpoint manifest");
        let directory = tempfile::tempdir().expect("state directory");
        let provenance = nano_marf::CheckpointProvenance {
            checkpoint: manifest.clone(),
            attestation: Some(nano_marf::CheckpointAttestation {
                attesting_block_id: manifest.source_state_id,
                signer_weight: 7,
                approval_threshold: 7,
            }),
        };
        provenance.record(&directory).expect("record provenance");

        assert_eq!(
            nano_marf::CheckpointProvenance::load(&directory).expect("read provenance"),
            Some(provenance),
            "a restart lost where the state came from"
        );

        let mut other = manifest;
        other.stacks_height += 1;
        assert!(
            matches!(
                nano_marf::CheckpointProvenance {
                    checkpoint: other,
                    attestation: None,
                }
                .record(&directory),
                Err(nano_marf::CheckpointError::ProvenanceMismatch { .. })
            ),
            "state imported from one checkpoint was resumed under another"
        );
    }

    #[test]
    fn captured_first_block_state_matches_reference() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let (source, _) = checkpoint_state(&fixture).expect("checkpoint metadata");
        let path = captured_block_paths(&fixture)
            .into_iter()
            .next()
            .expect("captured block");
        let block =
            NanoNakamotoBlock::decode(&fs::read(&path).expect("read block")).expect("decode block");
        let expected_state = import_checkpoint(
            fixture.join("chainstate/checkpoint-H/marf.sqlite"),
            *block.block_id().as_bytes(),
            block.header.state_index_root,
        )
        .expect("import expected state");
        let snapshots = captured_bitcoin_snapshots(&fixture).expect("snapshots");
        let bitcoin_operations = captured_bitcoin_operations(&fixture).expect("Bitcoin operations");
        let mut chainstate = captured_chainstate(&fixture);
        let bitcoin_context = *snapshots
            .get(&block.header.consensus_hash.to_string())
            .expect("Bitcoin context");
        let applied = chainstate
            .execute_nakamoto_block_with_bitcoin_operations(
                bitcoin_context,
                bitcoin_operations
                    .get(&block.header.consensus_hash.to_string())
                    .expect("Bitcoin operations"),
                Some(source),
                &block,
            )
            .expect("execute block");
        assert_eq!(
            TrieHash::from_bytes(applied.execution.state_root.0),
            block.header.state_index_root
        );
        assert_eq!(
            chainstate
                .state_content_root(*block.block_id().as_bytes())
                .expect("actual content root"),
            expected_state
                .content_root(*block.block_id().as_bytes())
                .expect("expected content root")
        );
        let expected_pointers = expected_state
            .root_pointers(*block.block_id().as_bytes())
            .expect("expected root pointers");
        let actual_pointers = chainstate
            .state_root_pointers(*block.block_id().as_bytes())
            .expect("actual root pointers");
        assert_eq!(actual_pointers, expected_pointers);
        assert_eq!(
            chainstate
                .state_leaves(*block.block_id().as_bytes())
                .expect("actual leaves"),
            expected_state
                .leaves(*block.block_id().as_bytes())
                .expect("expected leaves")
        );
    }

    #[test]
    fn candidate_block_derives_the_captured_state_root_before_finalizing_its_id() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let (source, _) = checkpoint_state(&fixture).expect("checkpoint metadata");
        let path = captured_block_paths(&fixture)
            .into_iter()
            .next()
            .expect("captured block");
        let block =
            NanoNakamotoBlock::decode(&fs::read(path).expect("read block")).expect("decode block");
        let snapshots = captured_bitcoin_snapshots(&fixture).expect("snapshots");
        let bitcoin_context = *snapshots
            .get(&block.header.consensus_hash.to_string())
            .expect("Bitcoin context");
        let mut chainstate = captured_chainstate(&fixture);
        let miner = StacksPrivateKey::from_seed(b"candidate miner");
        let expected_root = block.header.state_index_root;
        let (candidate, applied) = chainstate
            .assemble_nakamoto_block_with_bitcoin_context(
                bitcoin_context,
                Some(source),
                block,
                &miner,
            )
            .expect("assemble candidate");

        assert_eq!(candidate.header.state_index_root, expected_root);
        assert_eq!(
            TrieHash::from_bytes(applied.execution.state_root.0),
            expected_root
        );
        assert_eq!(
            candidate
                .header
                .miner_signature
                .recover(candidate.header.miner_signature_hash().as_bytes())
                .expect("recover candidate miner"),
            miner.public_key()
        );
    }

    #[test]
    fn signer_validator_executes_a_captured_proposal() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let (source, _) = checkpoint_state(&fixture).expect("checkpoint metadata");
        let snapshots = captured_bitcoin_snapshots(&fixture).expect("snapshots");
        let bitcoin_operations = captured_bitcoin_operations(&fixture).expect("Bitcoin operations");
        let mut paths = fs::read_dir(fixture.join("nakamoto/blocks"))
            .expect("read blocks")
            .map(|entry| entry.expect("block entry").path())
            .collect::<Vec<_>>();
        paths.sort();
        let first = NanoNakamotoBlock::decode(&fs::read(&paths[0]).expect("read first block"))
            .expect("decode first block");
        let second = NanoNakamotoBlock::decode(&fs::read(&paths[1]).expect("read second block"))
            .expect("decode second block");
        let first_context = *snapshots
            .get(&first.header.consensus_hash.to_string())
            .expect("first Bitcoin context");
        let second_context = *snapshots
            .get(&second.header.consensus_hash.to_string())
            .expect("second Bitcoin context");
        let mut chainstate = captured_chainstate(&fixture);
        let mut bitcoin = FixtureBitcoinSource {
            blocks: [
                (first_context.height, &first.header.consensus_hash),
                (second_context.height, &second.header.consensus_hash),
            ]
            .into_iter()
            .map(|(height, consensus_hash)| {
                (
                    height,
                    BitcoinBlock {
                        height,
                        hash: [0; 32],
                        operations: bitcoin_operations
                            .get(&consensus_hash.to_string())
                            .expect("Bitcoin operations")
                            .clone(),
                    },
                )
            })
            .collect(),
        };
        let first_operations = bitcoin
            .block_at(first_context.height)
            .expect("first Bitcoin operations");
        chainstate
            .append_nakamoto_block_with_bitcoin_operations(
                first_context,
                &first_operations.operations,
                Some(source),
                &first,
            )
            .expect("apply anchor block");
        let mut validator =
            ChainstateProposalValidator::new(chainstate, &first, first_context, bitcoin);
        let proposal = BlockProposal {
            block: second.clone(),
            bitcoin_height: second_context.height,
            reward_cycle: 0,
            data: BlockProposal::empty_data(),
        };

        validator.validate(&proposal).expect("proposal state root");
        validator
            .observe(&second, second_context.height)
            .expect("observe candidate");
    }

    #[test]
    fn checkpoint_executor_executes_captured_descendants() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let (source, root) = checkpoint_state(&fixture).expect("checkpoint metadata");
        let snapshots = captured_bitcoin_snapshots(&fixture).expect("snapshots");
        let mut paths = fs::read_dir(fixture.join("nakamoto/blocks"))
            .expect("read blocks")
            .map(|entry| entry.expect("block entry").path())
            .collect::<Vec<_>>();
        paths.sort();
        let first = NanoNakamotoBlock::decode(&fs::read(&paths[0]).expect("read first block"))
            .expect("decode first block");
        let second = NanoNakamotoBlock::decode(&fs::read(&paths[1]).expect("read second block"))
            .expect("decode second block");
        let first_context = *snapshots
            .get(&first.header.consensus_hash.to_string())
            .expect("first Bitcoin context");
        let second_context = *snapshots
            .get(&second.header.consensus_hash.to_string())
            .expect("second Bitcoin context");
        let bitcoin_operations = captured_bitcoin_operations(&fixture).expect("Bitcoin operations");
        let bitcoin = FixtureBitcoinSource {
            blocks: [
                (first_context.height, &first.header.consensus_hash),
                (second_context.height, &second.header.consensus_hash),
            ]
            .into_iter()
            .map(|(height, consensus_hash)| {
                (
                    height,
                    BitcoinBlock {
                        height,
                        hash: [0; 32],
                        operations: bitcoin_operations
                            .get(&consensus_hash.to_string())
                            .expect("Bitcoin operations")
                            .clone(),
                    },
                )
            })
            .collect(),
        };
        let mut executor = CheckpointExecutor::from_checkpoint(
            Checkpoint {
                network: captured_network(&fixture),
                path: fixture.join("chainstate/checkpoint-H/marf.sqlite"),
                source,
                state_root: root,
                accounting: captured_accounting(&fixture),
            },
            first,
            first_context,
            bitcoin,
        )
        .expect("open checkpoint executor");

        executor
            .apply(&second, second_context)
            .expect("execute descendant");
        assert_eq!(executor.tip().block_id(), second.block_id());
    }

    #[test]
    fn captured_fourth_block_state_matches_reference() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let (source, _) = checkpoint_state(&fixture).expect("checkpoint metadata");
        let snapshots = captured_bitcoin_snapshots(&fixture).expect("snapshots");
        let bitcoin_operations = captured_bitcoin_operations(&fixture).expect("Bitcoin operations");
        let mut chainstate = captured_chainstate(&fixture);
        let mut parent = Some(source);
        let mut bitcoin_view = String::new();
        let mut fourth = None;
        let mut paths = fs::read_dir(fixture.join("nakamoto/blocks"))
            .expect("read blocks")
            .map(|entry| entry.expect("block entry").path())
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths.into_iter().take(4) {
            let block = NanoNakamotoBlock::decode(&fs::read(&path).expect("read block"))
                .expect("decode block");
            apply_captured_block(
                &fixture,
                &mut chainstate,
                &snapshots,
                &bitcoin_operations,
                parent,
                &mut bitcoin_view,
                &path,
            )
            .expect("apply captured block");
            parent = Some(*block.block_id().as_bytes());
            fourth = Some(block);
        }
        let block = fourth.expect("fourth block");
        let block_id = *block.block_id().as_bytes();
        let expected = import_checkpoint(
            fixture.join("chainstate/checkpoint-H/marf.sqlite"),
            block_id,
            block.header.state_index_root,
        )
        .expect("import expected state");
        let expected_leaves = expected.leaves(block_id).expect("expected leaves");
        let actual_leaves = chainstate.state_leaves(block_id).expect("actual leaves");
        let expected_only = expected_leaves
            .iter()
            .filter(|leaf| !actual_leaves.contains(leaf))
            .collect::<Vec<_>>();
        let actual_only = actual_leaves
            .iter()
            .filter(|leaf| !expected_leaves.contains(leaf))
            .collect::<Vec<_>>();
        assert!(
            expected_only.is_empty() && actual_only.is_empty(),
            "expected-only: {expected_only:#?}\nactual-only: {actual_only:#?}"
        );
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
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let (source, root) = checkpoint_state(&fixture).expect("checkpoint metadata");
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
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let (source, root) = checkpoint_state(&fixture).expect("checkpoint metadata");
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
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let (source, root) = checkpoint_state(&fixture).expect("checkpoint metadata");
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
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let (source, published_root) = checkpoint_state(&fixtures).expect("checkpoint metadata");
        let fixture = fixtures.join("chainstate/checkpoint-H");
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
            &format!(
                "[snapshot]\nblock_hash = \"0x{}\"\n\n[roots]\nclarity_archival_marf_root_hash = \"0x{}\"\n",
                hex::encode(source),
                hex::encode(published_root.as_bytes())
            ),
        )?;
        let imported = import_pcs(&root)?;
        assert!(imported.root(source).is_some());
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn a_chainstate_reopened_from_disk_matches_one_that_never_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let (source, root) = checkpoint_state(&fixtures).expect("checkpoint metadata");
        let checkpoint = fixtures.join("chainstate/checkpoint-H/marf.sqlite");
        let network = captured_network(&fixtures);
        let temporary = temporary_fixture_root()?;

        // One variable per block, so every block moves the state root.
        let programs = [
            "(define-data-var first uint u1)",
            "(define-data-var second uint u2)",
            "(define-data-var third uint u3)",
        ];
        let blocks = [[0xa1; 32], [0xa2; 32], [0xa3; 32]];

        let mut open = nano_vm::Vm::open_from_checkpoint(
            network,
            temporary.join("open"),
            &checkpoint,
            source,
            root,
        )?;
        let mut parent = source;
        let mut expected = Vec::new();
        for (block, program) in blocks.iter().zip(programs) {
            open.begin_block(Some(parent), *block)?;
            open.execute(program, LimitedCostTracker::new_free())
                .expect("execute block");
            expected.push(open.seal_block()?);
            parent = *block;
        }
        drop(open);

        let directory = temporary.join("reopened");
        let mut parent = source;
        let mut reopened_roots = Vec::new();
        for (block, program) in blocks.iter().zip(programs) {
            let mut reopened = nano_vm::Vm::open_from_checkpoint(
                network,
                &directory,
                &checkpoint,
                source,
                root,
            )?;
            assert_eq!(reopened.tip(), Some(parent), "resumes from the tip on disk");
            reopened.begin_block(Some(parent), *block)?;
            reopened.execute(program, LimitedCostTracker::new_free())
                .expect("execute block");
            reopened_roots.push(reopened.seal_block()?);
            parent = *block;
        }

        assert_eq!(reopened_roots, expected);
        fs::remove_dir_all(temporary)?;
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

    /// What a block may spend in epoch 4, which the node enforces and a miner
    /// has to stop filling a block at.
    #[test]
    fn epoch_4_block_limit_matches_stacks_core() {
        let ours = nano_vm::EPOCH_4_BLOCK_LIMIT;
        let reference = blockstack_lib::core::BLOCK_LIMIT_MAINNET_40;

        assert_eq!(ours.write_length, reference.write_length);
        assert_eq!(ours.write_count, reference.write_count);
        assert_eq!(ours.read_length, reference.read_length);
        assert_eq!(ours.read_count, reference.read_count);
        assert_eq!(ours.runtime, reference.runtime);
    }

    /// The two consensus-visible halves of a network's identity.
    ///
    /// Both reach the state root: the chain identifier through every signing
    /// preimage and `(chain-id)`, and the boot address through the principal of
    /// every boot contract a block touches.
    #[test]
    fn network_identity_matches_stacks_core() {
        for (network, mainnet) in [(Network::MAINNET, true), (Network::TESTNET, false)] {
            assert_eq!(network.is_mainnet(), mainnet);
            assert_eq!(
                network.chain_id(),
                if mainnet {
                    stacks_common::consts::CHAIN_ID_MAINNET
                } else {
                    stacks_common::consts::CHAIN_ID_TESTNET
                }
            );
            assert_eq!(
                network.boot_address(),
                clarity::boot_util::boot_code_addr(mainnet).to_string()
            );
            assert_eq!(
                network.boot_contract_id("pox-5"),
                clarity::boot_util::boot_code_id("pox-5", mainnet).to_string()
            );
        }
    }

    /// A chainstate reopened from disk is the same chainstate.
    ///
    /// This is the route a node takes across a restart, so it has to reach the
    /// same state roots as one that never closed — otherwise a restart is a
    /// silent fork.
    #[test]
    fn a_chainstate_reopened_between_blocks_matches_one_that_never_closed() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let network = captured_network(&fixture);
        let (source, root) = checkpoint_state(&fixture).expect("checkpoint metadata");
        let checkpoint = fixture.join("chainstate/checkpoint-H/marf.sqlite");
        let snapshots = captured_bitcoin_snapshots(&fixture).expect("snapshots");
        let operations = captured_bitcoin_operations(&fixture).expect("Bitcoin operations");
        let blocks = captured_block_paths(&fixture)
            .into_iter()
            .take(3)
            .map(|path| {
                NanoNakamotoBlock::decode(&fs::read(&path).expect("read block"))
                    .expect("decode block")
            })
            .collect::<Vec<_>>();

        let apply = |chainstate: &mut ChainState, block: &NanoNakamotoBlock, parent| {
            let view = block.header.consensus_hash.to_string();
            chainstate
                .append_nakamoto_block_with_bitcoin_operations(
                    *snapshots.get(&view).expect("Bitcoin context"),
                    operations.get(&view).expect("Bitcoin operations"),
                    parent,
                    block,
                )
                .expect("execute block")
        };

        // One chainstate that stays open for all three blocks.
        let mut open = captured_chainstate(&fixture);
        let mut parent = Some(source);
        let continuous = blocks
            .iter()
            .map(|block| {
                let applied = apply(&mut open, block, parent);
                parent = Some(*block.block_id().as_bytes());
                applied.execution.state_root
            })
            .collect::<Vec<_>>();

        // The same three blocks, closing and reopening the directory each time.
        let directory = tempfile::tempdir().expect("chainstate directory");
        let mut parent = Some(source);
        let reopened = blocks
            .iter()
            .map(|block| {
                let mut chainstate = ChainState::open_from_checkpoint(
                    network,
                    directory.path(),
                    &checkpoint,
                    source,
                    root,
                )
                .expect("reopen chainstate");
                if let Some(accounting) = captured_accounting(&fixture) {
                    *chainstate.accounting_mut() = accounting;
                }
                let applied = apply(&mut chainstate, block, parent);
                parent = Some(*block.block_id().as_bytes());
                applied.execution.state_root
            })
            .collect::<Vec<_>>();

        assert_eq!(
            reopened, continuous,
            "reopening the chainstate between blocks changed the state it produced"
        );
    }

    /// A commitment that missed its Bitcoin block is not one of the block's
    /// operations.
    ///
    /// stacks-core parses such a commitment and keeps it only so its UTXO can
    /// chain through the mining window; it wins no sortition and it is not
    /// covered by the block's `ops_hash`. nano hashed every commitment it could
    /// parse, so a single late miner would have moved its consensus hash.
    #[test]
    fn a_missed_commitment_leaves_the_operation_hash() {
        // The rule, against stacks-core's own arithmetic, over every modulus a
        // commitment can carry and a run of heights.
        for parent_modulus in 0..=u8::try_from(BURN_BLOCK_MINED_AT_MODULUS).expect("small") {
            for height in 0..32_u64 {
                let intended = (u64::from(parent_modulus) % BURN_BLOCK_MINED_AT_MODULUS + 1)
                    % BURN_BLOCK_MINED_AT_MODULUS;
                assert_eq!(
                    nano_sortition::commitment_is_on_time(parent_modulus, height),
                    height % BURN_BLOCK_MINED_AT_MODULUS == intended,
                    "modulus {parent_modulus} at height {height}"
                );
            }
        }

        // A block holding one commitment that arrived on time and one that did
        // not: only the first is one of the block's operations.
        let height = 305;
        let on_time = u8::try_from((height + BURN_BLOCK_MINED_AT_MODULUS - 1) % 5).expect("small");
        let commit = |txid: u8, parent_modulus: u8| nano_bitcoin::BitcoinOperation {
            txid: [txid; 32],
            transaction_index: u32::from(txid),
            inputs: Vec::new(),
            outputs: Vec::new(),
            kind: nano_bitcoin::BitcoinOperationKind::LeaderBlockCommit {
                block_header_hash: [0; 32],
                new_seed: [0; 32],
                parent_block_height: 0,
                parent_transaction_index: 0,
                key_block_height: 0,
                key_transaction_index: 0,
                memo: 0,
                parent_modulus,
            },
        };
        let block = nano_bitcoin::BitcoinBlock {
            height,
            hash: [9; 32],
            operations: vec![commit(1, on_time), commit(2, (on_time + 1) % 5)],
        };

        assert_eq!(
            nano_sortition::accepted_operation_txids(&block),
            vec![[1; 32]],
            "the late commitment must not be one of the block's operations"
        );
    }

    /// A state on disk names the ancestors a resume can fall back to.
    ///
    /// When the block a node sealed at leaves the chain, walking back needs
    /// the parents the store recorded — without them the only answer is to
    /// stop, which is what turned a one-block reorganization into a dead node.
    #[test]
    fn a_stored_state_remembers_what_it_was_built_on() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let (source, _) = checkpoint_state(&fixture).expect("checkpoint metadata");
        let snapshots = captured_bitcoin_snapshots(&fixture).expect("snapshots");
        let operations = captured_bitcoin_operations(&fixture).expect("Bitcoin operations");
        let mut chainstate = captured_chainstate(&fixture);

        let mut parent = Some(source);
        let mut executed = Vec::new();
        for path in captured_block_paths(&fixture).into_iter().take(4) {
            let block = NanoNakamotoBlock::decode(&fs::read(&path).expect("read block"))
                .expect("decode block");
            let view = block.header.consensus_hash.to_string();
            chainstate
                .append_nakamoto_block_with_bitcoin_operations(
                    *snapshots.get(&view).expect("Bitcoin context"),
                    operations.get(&view).expect("Bitcoin operations"),
                    parent,
                    &block,
                )
                .expect("execute block");
            parent = Some(*block.block_id().as_bytes());
            executed.push(*block.block_id().as_bytes());
        }

        // From the tip, every ancestor back to the checkpoint, in order.
        let mut walked = Vec::new();
        let mut block = *executed.last().expect("executed blocks");
        while let Some(parent) = chainstate.parent_of(block) {
            walked.push(parent);
            block = parent;
        }

        // The blocks just executed, newest first, then the checkpoint — and
        // past it, the ancestors the import kept, which a resume can also fall
        // back to.
        let mut expected: Vec<_> = executed[..executed.len() - 1].to_vec();
        expected.reverse();
        expected.push(source);
        assert_eq!(
            walked.get(..expected.len()),
            Some(expected.as_slice()),
            "a resume must be able to walk back to the checkpoint one block at a time"
        );
        assert!(
            walked.len() > expected.len(),
            "the checkpoint's own ancestors are reachable too"
        );
    }

    /// Fork choice weighs signatures before it compares lengths.
    ///
    /// Following one peer means following whatever it says. A pool has to pick,
    /// and a longer chain nobody signed must never win over a shorter one the
    /// reward set put its name to.
    #[test]
    fn a_longer_unsigned_chain_never_wins_the_fork_choice() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let signers = captured_signer_set(&fixture);
        let blocks = captured_block_paths(&fixture)
            .into_iter()
            .map(|path| {
                NanoNakamotoBlock::decode(&fs::read(&path).expect("read block"))
                    .expect("decode block")
            })
            .collect::<Vec<_>>();

        // A real, signed tip from the captured chain.
        let signed = blocks
            .iter()
            .find(|block| nano_sync::weigh_tip(&block.header, &signers).is_ok())
            .expect("a captured block the reward set signed");
        // The same chain, longer, with its signatures stripped.
        let mut forged = signed.header.clone();
        forged.chain_length = signed.header.chain_length + 1_000;
        forged.signer_signatures.clear();

        assert_eq!(
            nano_sync::weigh_tip(&forged, &signers),
            Err(nano_sync::TipRejection::InsufficientWeight),
            "an unsigned header carries no weight however long it claims to be"
        );

        let tip = |peer: usize, header: nano_chainstate::NakamotoBlockHeader| {
            nano_sync::CandidateTip {
                peer,
                info: nano_sync::TenureInfo {
                    consensus_hash: header.consensus_hash,
                    tenure_start_block_id: header.block_id(),
                    parent_consensus_hash: header.consensus_hash,
                    parent_tenure_start_block_id: header.parent_block_id,
                    tip_block_id: header.block_id(),
                    tip_height: header.chain_length,
                    reward_cycle: 0,
                },
                header,
            }
        };
        let candidates = vec![tip(0, forged), tip(1, signed.header.clone())];

        let chosen = nano_sync::choose_canonical_tip(&candidates, &signers)
            .expect("a signed candidate is available");
        assert_eq!(chosen.peer, 1, "the signed tip wins despite being shorter");

        // With nothing signed, there is no canonical tip to follow at all.
        assert!(
            nano_sync::choose_canonical_tip(&candidates[..1], &signers).is_none(),
            "an unsigned chain is not a chain to follow"
        );
    }

    /// Two peers offering equally long signed tips resolve the same way
    /// everywhere, or nodes following the same peers would split.
    #[test]
    fn an_exact_tie_in_fork_choice_resolves_deterministically() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let signers = captured_signer_set(&fixture);
        let signed = captured_block_paths(&fixture)
            .into_iter()
            .map(|path| {
                NanoNakamotoBlock::decode(&fs::read(&path).expect("read block"))
                    .expect("decode block")
            })
            .find(|block| nano_sync::weigh_tip(&block.header, &signers).is_ok())
            .expect("a captured block the reward set signed");

        let tip = |peer: usize| nano_sync::CandidateTip {
            peer,
            info: nano_sync::TenureInfo {
                consensus_hash: signed.header.consensus_hash,
                tenure_start_block_id: signed.block_id(),
                parent_consensus_hash: signed.header.consensus_hash,
                parent_tenure_start_block_id: signed.header.parent_block_id,
                tip_block_id: signed.block_id(),
                tip_height: signed.header.chain_length,
                reward_cycle: 0,
            },
            header: signed.header.clone(),
        };
        let forwards = vec![tip(0), tip(1)];
        let backwards = vec![tip(1), tip(0)];

        assert_eq!(
            nano_sync::choose_canonical_tip(&forwards, &signers).map(|tip| tip.header.block_id()),
            nano_sync::choose_canonical_tip(&backwards, &signers).map(|tip| tip.header.block_id()),
            "the order the peers answered in must not decide the tip"
        );
    }

    /// A reorganization takes the tenures it invalidated off the executed chain.
    ///
    /// The burnchain side retracts snapshots; this is the other half, which
    /// stops nano from building on a tenure Bitcoin no longer awarded.
    #[test]
    fn a_reorganization_retracts_the_tenures_it_invalidated() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let (source, _) = checkpoint_state(&fixture).expect("checkpoint metadata");
        let snapshots = captured_bitcoin_snapshots(&fixture).expect("snapshots");
        let bitcoin_operations = captured_bitcoin_operations(&fixture).expect("Bitcoin operations");
        let mut chainstate = captured_chainstate(&fixture);

        let mut parent = Some(source);
        let mut blocks = Vec::new();
        for path in captured_block_paths(&fixture).into_iter().take(8) {
            let block = NanoNakamotoBlock::decode(&fs::read(&path).expect("read block"))
                .expect("decode block");
            let view = block.header.consensus_hash.to_string();
            chainstate
                .append_nakamoto_block_with_bitcoin_operations(
                    *snapshots.get(&view).expect("Bitcoin context"),
                    bitcoin_operations.get(&view).expect("Bitcoin operations"),
                    parent,
                    &block,
                )
                .expect("execute block");
            parent = Some(*block.block_id().as_bytes());
            blocks.push(block);
        }
        let executed = chainstate.executed_blocks();
        assert_eq!(executed.len(), blocks.len());

        // Retract from the last tenure the capture starts, so the tenure's own
        // blocks and everything after it come back.
        let last_tenure = blocks
            .iter()
            .rev()
            .find(|block| nano_chainstate::starts_new_tenure(block))
            .expect("a captured tenure-start block");
        let retracted_from = blocks
            .iter()
            .position(|block| block.header.consensus_hash == last_tenure.header.consensus_hash)
            .expect("the tenure is in the executed chain");

        let reorg = nano_sortition::SortitionReorg {
            valid_ancestor: nano_sortition::SortitionSnapshot::genesis(
                0,
                nano_primitives::BitcoinHeaderHash::from_bytes([0; 32]),
            ),
            retracted: vec![nano_sortition::SortitionSnapshot {
                consensus_hash: last_tenure.header.consensus_hash,
                ..nano_sortition::SortitionSnapshot::genesis(
                    1,
                    nano_primitives::BitcoinHeaderHash::from_bytes([1; 32]),
                )
            }],
        };
        let retraction = chainstate.retract(&reorg);

        assert_eq!(
            retraction.discarded,
            executed[retracted_from..].to_vec(),
            "every block of the invalidated tenure and its successors is discarded"
        );
        assert_eq!(
            retraction.resume_from,
            retracted_from
                .checked_sub(1)
                .map(|index| executed[index]),
            "execution resumes from the last surviving block"
        );
        assert_eq!(chainstate.executed_blocks(), executed[..retracted_from]);

        // A reorganization that invalidates nothing nano executed leaves the
        // chain where it was.
        let untouched = chainstate.retract(&nano_sortition::SortitionReorg {
            valid_ancestor: nano_sortition::SortitionSnapshot::genesis(
                0,
                nano_primitives::BitcoinHeaderHash::from_bytes([0; 32]),
            ),
            retracted: Vec::new(),
        });
        assert!(untouched.discarded.is_empty());
        assert_eq!(untouched.resume_from, retraction.resume_from);
    }

    /// Clarity reads back the header of a block nano executed.
    ///
    /// `get-stacks-block-info?` and `get-tenure-info?` are answered from nano's
    /// own index; before it existed every one of these returned `none`, which is
    /// a divergence for any contract that consults chain history.
    #[test]
    fn clarity_reads_the_headers_of_executed_blocks() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let (source, _) = checkpoint_state(&fixture).expect("checkpoint metadata");
        let snapshots = captured_bitcoin_snapshots(&fixture).expect("snapshots");
        let bitcoin_operations = captured_bitcoin_operations(&fixture).expect("Bitcoin operations");
        let mut chainstate = captured_chainstate(&fixture);

        // Execute far enough in that the block being read is an ancestor of the
        // state the read runs against, which is what Clarity requires.
        let mut parent = Some(source);
        let mut executed = Vec::new();
        for path in captured_block_paths(&fixture).into_iter().take(4) {
            let block = NanoNakamotoBlock::decode(&fs::read(&path).expect("read block"))
                .expect("decode block");
            let view = block.header.consensus_hash.to_string();
            let context = *snapshots.get(&view).expect("Bitcoin context");
            chainstate
                .append_nakamoto_block_with_bitcoin_operations(
                    context,
                    bitcoin_operations.get(&view).expect("Bitcoin operations"),
                    parent,
                    &block,
                )
                .expect("execute block");
            parent = Some(*block.block_id().as_bytes());
            executed.push((block, context));
        }

        let (target, context) = &executed[1];
        let height = target.header.chain_length;
        let mut read = |source: &str| {
            chainstate
                .evaluate(source)
                .unwrap_or_else(|error| panic!("evaluate {source}: {error}"))
                .unwrap_or_else(|| panic!("{source} produced no value"))
        };

        assert_eq!(
            read(&format!("(get-stacks-block-info? header-hash u{height})")),
            Value::some(
                Value::buff_from(target.header.block_hash().as_bytes().to_vec())
                    .expect("32-byte buffer")
            )
            .expect("optional"),
        );
        assert_eq!(
            read(&format!("(get-stacks-block-info? time u{height})")),
            Value::some(Value::UInt(u128::from(target.header.timestamp))).expect("optional"),
        );
        assert_eq!(
            read(&format!("(get-tenure-info? burnchain-header-hash u{height})")),
            Value::some(
                Value::buff_from(context.burn_header_hash.to_vec()).expect("32-byte buffer")
            )
            .expect("optional"),
        );
        assert_eq!(
            read(&format!("(get-tenure-info? time u{height})")),
            Value::some(Value::UInt(u128::from(context.burn_block_time))).expect("optional"),
        );
        assert_eq!(
            read(&format!("(get-tenure-info? vrf-seed u{height})")),
            Value::some(Value::buff_from(context.vrf_seed.to_vec()).expect("32-byte buffer"))
                .expect("optional"),
        );
        assert_eq!(
            read(&format!("(get-tenure-info? miner-spend-winner u{height})")),
            Value::some(Value::UInt(context.burn_spend_winner)).expect("optional"),
        );
        assert_eq!(
            read(&format!("(get-tenure-info? miner-spend-total u{height})")),
            Value::some(Value::UInt(context.burn_spend_total)).expect("optional"),
        );
        assert_ne!(
            context.burn_spend_winner, 0,
            "the capture must record a real commitment for the read to mean anything"
        );
    }

    /// Every captured tenure-start block satisfies both VRF rules.
    ///
    /// The coinbase proof must come from the winning miner's registered key over
    /// the tenure's sortition hash, and the seed that miner committed on Bitcoin
    /// must be the hash of the parent tenure's proof. A follower that skips
    /// either will build on a chain the network rejects.
    #[test]
    fn captured_tenures_satisfy_the_vrf_rules() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let snapshots: Vec<serde_json::Value> = serde_json::from_slice(
            &fs::read(fixture.join("sortition/snapshots.json")).expect("read snapshots"),
        )
        .expect("decode snapshots");
        let operations = captured_bitcoin_operations(&fixture).expect("Bitcoin operations");
        let by_height = |height: u64| -> Vec<nano_bitcoin::BitcoinOperation> {
            snapshots
                .iter()
                .find(|snapshot| snapshot["block_height"].as_u64() == Some(height))
                .and_then(|snapshot| snapshot["consensus_hash"].as_str())
                .and_then(|consensus_hash| operations.get(consensus_hash))
                .cloned()
                .unwrap_or_default()
        };

        let mut previous_proof: Option<[u8; 80]> = None;
        let mut checked = 0_usize;
        for path in captured_block_paths(&fixture) {
            let block = NanoNakamotoBlock::decode(&fs::read(&path).expect("read block"))
                .expect("decode block");
            if !nano_chainstate::starts_new_tenure(&block) {
                continue;
            }
            let consensus_hash = block.header.consensus_hash.to_string();
            let snapshot = snapshots
                .iter()
                .find(|snapshot| snapshot["consensus_hash"].as_str() == Some(&consensus_hash))
                .expect("captured tenure has a sortition snapshot");
            let sortition_hash =
                decode_hash(snapshot["sortition_hash"].as_str().expect("sortition hash"))
                    .expect("32-byte sortition hash");
            // Several miners can commit to the same block hash in the same
            // Bitcoin block, so the winner is identified by its transaction, not
            // by what it committed to.
            let winning_txid = decode_hash(
                snapshot["winning_block_txid"]
                    .as_str()
                    .expect("winning commitment txid"),
            )
            .expect("32-byte winning txid");
            let height = snapshot["block_height"].as_u64().expect("burn height");

            // The winning commitment names the leader key it registered under,
            // which is what the proof has to have been produced with.
            let (key_height, key_index, new_seed) = by_height(height)
                .into_iter()
                .find_map(|operation| match operation.kind {
                    nano_bitcoin::BitcoinOperationKind::LeaderBlockCommit {
                        key_block_height,
                        key_transaction_index,
                        new_seed,
                        ..
                    } if operation.txid == winning_txid => {
                        Some((key_block_height, key_transaction_index, new_seed))
                    }
                    _ => None,
                })
                .expect("the winning commitment is in the captured Bitcoin block");
            let vrf_public_key = by_height(u64::from(key_height))
                .into_iter()
                .find_map(|operation| match operation.kind {
                    nano_bitcoin::BitcoinOperationKind::LeaderKeyRegistration {
                        vrf_public_key,
                        ..
                    } if operation.transaction_index == u32::from(key_index) => Some(vrf_public_key),
                    _ => None,
                })
                .expect("the leader key registration is in the captured Bitcoin block");

            nano_chainstate::verify_coinbase_vrf_proof(&block, &vrf_public_key, &sortition_hash)
                .unwrap_or_else(|error| {
                    panic!("tenure {consensus_hash} failed its coinbase proof: {error}")
                });

            // The seed is checked against the tenure before it, which the first
            // captured tenure does not have inside the window.
            if let Some(parent_proof) = previous_proof {
                nano_chainstate::verify_committed_vrf_seed(&new_seed, &parent_proof)
                    .unwrap_or_else(|error| {
                        panic!("tenure {consensus_hash} committed a bad seed: {error}")
                    });
            }
            previous_proof = nano_chainstate::coinbase_vrf_proof(&block);
            checked += 1;
        }
        assert!(
            checked > 1,
            "the capture must hold more than one tenure to check a seed against its parent"
        );
    }

    /// A tampered proof or seed is rejected, so the check is not vacuous.
    #[test]
    fn a_tampered_vrf_proof_or_seed_is_rejected() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let block = captured_block_paths(&fixture)
            .into_iter()
            .map(|path| {
                NanoNakamotoBlock::decode(&fs::read(&path).expect("read block"))
                    .expect("decode block")
            })
            .find(nano_chainstate::starts_new_tenure)
            .expect("a captured tenure-start block");
        let proof = nano_chainstate::coinbase_vrf_proof(&block).expect("coinbase proof");

        // A well-formed key that did not produce the proof, and a key that is
        // not a curve point at all, fail differently and both fail.
        let stranger = nano_crypto::VrfPrivateKey::from_bytes([3; 32])
            .public_key()
            .to_bytes();
        assert_eq!(
            nano_chainstate::verify_coinbase_vrf_proof(&block, &stranger, &[9; 32]),
            Err(nano_chainstate::TenureVrfError::ProofNotFromLeaderKey)
        );
        assert_eq!(
            nano_chainstate::verify_coinbase_vrf_proof(&block, &[7; 32], &[9; 32]),
            Err(nano_chainstate::TenureVrfError::MalformedProof)
        );

        assert_eq!(
            nano_chainstate::verify_committed_vrf_seed(
                &nano_chainstate::vrf_seed_from_proof(&proof),
                &proof
            ),
            Ok(())
        );
        assert_eq!(
            nano_chainstate::verify_committed_vrf_seed(&[0; 32], &proof),
            Err(nano_chainstate::TenureVrfError::SeedNotFromParentProof)
        );
    }

    /// A tenure-start block inside the emission schedule moves real STX.
    ///
    /// The capture's own burn heights sit far below the schedule, which is why
    /// replay stays green without the emission at all; executing one of its
    /// tenure-start blocks against a Bitcoin height inside the schedule is what
    /// shows the mint lands. The state root is not checked, because the height
    /// is not the one the block committed to.
    #[test]
    fn a_tenure_start_block_mints_the_sip_031_emission() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let network = captured_network(&fixture);
        let (source, _) = checkpoint_state(&fixture).expect("checkpoint metadata");
        let snapshots = captured_bitcoin_snapshots(&fixture).expect("snapshots");
        let bitcoin_operations = captured_bitcoin_operations(&fixture).expect("Bitcoin operations");
        let block = captured_block_paths(&fixture)
            .into_iter()
            .map(|path| {
                NanoNakamotoBlock::decode(&fs::read(&path).expect("read block"))
                    .expect("decode block")
            })
            .find(nano_chainstate::starts_new_tenure)
            .expect("a captured tenure-start block");

        // The second testnet interval, so the amount is neither the first
        // boundary nor zero.
        let bitcoin_height = 71_525 + 360 * 2;
        let expected = nano_chainstate::sip_031_emission(network, bitcoin_height);
        assert!(expected > 0, "the chosen height must be inside the schedule");

        let mut chainstate = captured_chainstate(&fixture);
        let recipient = clarity::vm::types::PrincipalData::Contract(
            clarity::vm::types::QualifiedContractIdentifier::parse(
                &network.boot_contract_id("sip-031"),
            )
            .expect("the SIP-031 contract is a valid identifier"),
        );
        let before = chainstate
            .account_balance(&recipient)
            .expect("read the recipient's balance");

        let mut context = *snapshots
            .get(&block.header.consensus_hash.to_string())
            .expect("Bitcoin context");
        context.height = bitcoin_height;
        let applied = chainstate
            .execute_nakamoto_block_with_bitcoin_operations(
                context,
                bitcoin_operations
                    .get(&block.header.consensus_hash.to_string())
                    .expect("Bitcoin operations"),
                Some(source),
                &block,
            )
            .expect("execute block");

        let after = chainstate
            .account_balance(&recipient)
            .expect("read the recipient's balance");
        assert_eq!(after - before, expected, "the emission did not land");

        // The mint is reported on the coinbase, which is where stacks-core
        // attaches it and so where a receipt comparison looks for it.
        let minted = applied.receipts.iter().any(|receipt| {
            receipt.result.events.iter().any(|event| {
                matches!(
                    event,
                    clarity::vm::events::StacksTransactionEvent::STXEvent(
                        clarity::vm::events::STXEventType::STXMintEvent(data),
                    ) if data.amount == expected && data.recipient == recipient
                )
            })
        });
        assert!(minted, "the coinbase receipt does not report the mint");
    }

    /// The emission a tenure-start block mints to `.sip-031`.
    ///
    /// stacks-core's release schedule is behind a `testing` feature that swaps
    /// in an overridable table, so the intervals are compared against the static
    /// ones the release build uses rather than through the accessor.
    #[test]
    fn sip_031_emission_matches_stacks_core() {
        let reference = |network: Network, height: u64| -> u128 {
            let intervals: &[stacks_common::types::SIP031EmissionInterval] = if network.is_mainnet()
            {
                &*stacks_common::types::SIP031_EMISSION_INTERVALS_MAINNET
            } else {
                &*stacks_common::types::SIP031_EMISSION_INTERVALS_TESTNET
            };
            intervals
                .iter()
                .find(|interval| height >= interval.start_height)
                .map_or(0, |interval| interval.amount)
        };

        for network in [Network::MAINNET, Network::TESTNET] {
            // Every interval boundary, the height either side of it, and the
            // range below the schedule where nothing is minted at all.
            let boundaries = if network.is_mainnet() {
                vec![907_740, 960_300, 1_012_860, 1_065_420, 1_117_980, 1_170_540]
            } else {
                (1..=6).map(|step| 71_525 + 360 * step).collect()
            };
            let mut heights = vec![0, 1, 100_000];
            for boundary in boundaries {
                heights.extend([boundary - 1, boundary, boundary + 1]);
            }
            heights.push(u64::from(u32::MAX));
            for height in heights {
                assert_eq!(
                    nano_chainstate::sip_031_emission(network, height),
                    reference(network, height),
                    "SIP-031 emission diverges at Bitcoin height {height}"
                );
            }
        }
    }

    /// A peer's reported chain identifier is what decides the network, and only
    /// the mainnet identifier means mainnet.
    #[test]
    fn only_the_mainnet_chain_identifier_is_mainnet() {
        assert!(Network::from_chain_id(stacks_common::consts::CHAIN_ID_MAINNET).is_mainnet());
        for chain_id in [
            stacks_common::consts::CHAIN_ID_TESTNET,
            0x8000_0005,
            0,
            u32::MAX,
        ] {
            assert!(!Network::from_chain_id(chain_id).is_mainnet());
            assert_eq!(Network::from_chain_id(chain_id).chain_id(), chain_id);
        }
    }

    /// Find keys whose hashed trie paths share a leading prefix.
    ///
    /// A trie only compresses a path when two keys collide over more than one
    /// byte, and hashed keys drawn at random never do, so the layouts that
    /// compression creates are unreachable without searching for the collision.
    fn keys_sharing_prefix(prefix: usize, count: usize) -> Vec<String> {
        let mut groups: BTreeMap<Vec<u8>, Vec<String>> = BTreeMap::new();
        for index in 0..2_000_000_u32 {
            let key = format!("collide-{index}");
            let path = nano_marf::key_path(key.as_bytes());
            let group = groups
                .entry(path.as_bytes()[..prefix].to_vec())
                .or_default();
            group.push(key);
            if group.len() == count {
                return group.clone();
            }
        }
        panic!("no {count} keys share a {prefix}-byte trie path prefix");
    }

    /// Find a key whose path shares `prefix` bytes with another but diverges at
    /// the next byte, which is what splits a compressed path.
    fn key_diverging_after(reference: &str, prefix: usize) -> String {
        let target = nano_marf::key_path(reference.as_bytes());
        for index in 0..2_000_000_u32 {
            let key = format!("diverge-{index}");
            let path = nano_marf::key_path(key.as_bytes());
            let shares = path.as_bytes()[..prefix] == target.as_bytes()[..prefix];
            if shares && path.as_bytes()[prefix] != target.as_bytes()[prefix] {
                return key;
            }
        }
        panic!("no key diverges from {reference} after {prefix} bytes");
    }

    /// Every way one write can reshape a trie that already holds another path.
    ///
    /// Two keys sharing a path prefix compress into a node whose own path a
    /// third key can split, and stacks-core packs that split's pointers in the
    /// opposite order to the one a split leaf produces.
    #[test]
    fn versioned_marf_compressed_path_layouts_match_stacks_core() {
        let compressed = keys_sharing_prefix(2, 2);
        let deeply_compressed = keys_sharing_prefix(3, 2);
        let scripts: Vec<(&str, Vec<String>)> = vec![
            ("two paths compressing into one node", compressed.clone()),
            (
                "a leaf splitting a compressed node path",
                vec![
                    compressed[0].clone(),
                    compressed[1].clone(),
                    key_diverging_after(&compressed[0], 1),
                ],
            ),
            (
                "a leaf splitting a compressed leaf path",
                vec![
                    compressed[0].clone(),
                    key_diverging_after(&compressed[0], 1),
                ],
            ),
            (
                "a split above a deeper split",
                vec![
                    deeply_compressed[0].clone(),
                    deeply_compressed[1].clone(),
                    key_diverging_after(&deeply_compressed[0], 2),
                    key_diverging_after(&deeply_compressed[0], 1),
                ],
            ),
            ("children promoting a node around a split", {
                let mut keys = keys_sharing_prefix(1, 6);
                keys.push(key_diverging_after(&keys[0], 1));
                keys
            }),
        ];

        for (index, (description, keys)) in scripts.into_iter().enumerate() {
            let mut reference = ReferenceMarf::<ReferenceStacksBlockId>::from_path(
                ":memory:",
                ReferenceMarfOpenOpts::default(),
            )
            .expect("open reference MARF");
            let mut ours = VersionedMarf::default();
            let mut parent: Option<[u8; 32]> = None;
            // One key per block also exercises the copy-on-write layouts.
            for (step, key) in keys.iter().enumerate() {
                let mut block = [0; 32];
                block[0] = u8::try_from(index).expect("script index");
                block[31] = u8::try_from(step + 1).expect("script step");
                let reference_root = append_reference_marf_state(
                    &mut reference,
                    parent.map(ReferenceStacksBlockId).as_ref(),
                    &ReferenceStacksBlockId(block),
                    key,
                    "value",
                );
                let root = append_nano_marf_state(&mut ours, parent, block, key, "value");
                assert_eq!(
                    root.as_bytes(),
                    &reference_root.0,
                    "{description}: state root diverged writing {key} at step {step}"
                );
                parent = Some(block);
            }
        }
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
            "[{{\"block_height\":1,\"burn_header_hash\":\"{bitcoin_hash}\",\"burn_header_timestamp\":0,\"consensus_hash\":\"0000000000000000000000000000000000000000\",\"winning_block_txid\":\"{bitcoin_hash}\"}}]"
        );
        write_file(&root.join("sortition/snapshots.json"), &snapshot)?;
        write_file(
            &root.join("chainstate/checkpoint-H/checkpoint.toml"),
            "format = \"stacks-core-marf-sqlite-v2\"\nsource_state_id = \"id\"\npublished_state_index_root = \"root\"\n",
        )?;
        write_file(
            &root.join("chainstate/checkpoint-H/native-effects.json"),
            "{\"matured_effects\":[]}",
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
                reference.tx_count(),
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
        const HACKNET_REWARD_SLOTS: u32 = 30;

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
        let mut reward_sets = fs::read_dir(fixture_root.join("stacker_set"))
            .expect("read reward sets")
            .map(|entry| entry.expect("reward set entry").path())
            .collect::<Vec<_>>();
        reward_sets.sort();
        for path in reward_sets {
            let reward_set: StackerSetWire =
                serde_json::from_slice(&fs::read(&path).expect("read reward set"))
                    .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
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
            let (signer_set, threshold) =
                SignerSet::from_reward_slots(signers, HACKNET_REWARD_SLOTS)
                    .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            assert_eq!(
                threshold,
                u128::from(reward_set.stacker_set.pox_ustx_threshold),
                "{} stacking threshold",
                path.display()
            );
            assert_eq!(
                signer_set.weights(),
                expected_weights,
                "{} signer weights",
                path.display()
            );
        }

        let sets = captured_signer_sets(&fixture_root);
        assert!(!sets.is_empty(), "the capture records no reward set");

        let snapshots = captured_bitcoin_snapshots(&fixture_root).expect("snapshots");
        let cycle_of = |context: &BitcoinBlockContext| -> u64 {
            let length = u64::from(context.prepare_phase_length + context.reward_phase_length);
            context.height.saturating_sub(context.first_height) / length.max(1)
        };

        for entry in fs::read_dir(fixture_root.join("nakamoto/blocks")).expect("read blocks") {
            let path = entry.expect("fixture entry").path();
            let block = NanoNakamotoBlock::decode(&fs::read(&path).expect("read block"))
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            let Some(context) = snapshots.get(&block.header.consensus_hash.to_string()) else {
                continue;
            };
            let Some(signer_set) = sets.get(&cycle_of(context)) else {
                continue;
            };
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
            let winner = winner_vrf_seed.map(|vrf_seed| nano_sortition::SortitionWinner {
                txid: hash,
                vrf_seed,
            });
            let snapshot = chain
                .append_with_winner(&block, 0, nano_sortition::PoxId::initial(), winner)
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
    fn captured_sortition_snapshots_match_the_reference_bitcoin_chain() {
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let snapshots = captured_sortition_snapshots(&fixture_root);
        let (genesis, rest) = snapshots.split_first().expect("captured genesis snapshot");
        let mut replay = CapturedReplay::new(&fixture_root, genesis);

        for snapshot in rest {
            replay.append(snapshot);
        }
    }

    #[test]
    fn a_reorganized_bitcoin_branch_converges_on_the_captured_snapshots() {
        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let snapshots = captured_sortition_snapshots(&fixture_root);
        let (genesis, rest) = snapshots.split_first().expect("captured genesis snapshot");
        let (replayed, discarded) = rest.split_at(200);
        let mut replay = CapturedReplay::new(&fixture_root, genesis);
        for snapshot in replayed {
            replay.append(snapshot);
        }

        // Bitcoin hands the node three blocks, then replaces them with the
        // branch the capture recorded.
        let fork_height = replay.chain.tip().bitcoin_height;
        let anchors = replay.pox.pox_id().bits().len();
        for snapshot in discarded.iter().rev().take(3) {
            replay.append_off_chain(snapshot);
        }
        assert!(
            replay.pox.pox_id().bits().len() > anchors,
            "the discarded branch has to start a reward cycle to exercise the rewind"
        );
        let canonical = snapshots
            .iter()
            .map(|snapshot| (snapshot.block_height, hex_array(&snapshot.burn_header_hash)))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            replay
                .chain
                .find_fork(|height| canonical.get(&height).copied().ok_or(height)),
            Ok(nano_sortition::Fork::Above(fork_height))
        );

        let reorg = replay
            .chain
            .retract_above(fork_height)
            .expect("retract the discarded branch");
        assert_eq!(reorg.depth(), 3);
        assert_eq!(reorg.resume_bitcoin_height(), fork_height + 1);
        assert_eq!(
            reorg.invalidated_consensus_hashes(),
            reorg
                .retracted
                .iter()
                .map(|snapshot| snapshot.consensus_hash)
                .collect::<Vec<_>>()
        );
        replay
            .pox
            .retract_to(fork_height)
            .expect("rewind the PoX history of the discarded branch");
        assert_eq!(replay.pox.pox_id().bits().len(), anchors);
        replay
            .pre_stx
            .invalidate_from(reorg.resume_bitcoin_height());

        for snapshot in discarded {
            replay.append(snapshot);
        }
        assert_eq!(
            replay.chain.tip().bitcoin_height,
            snapshots.last().expect("captured tip").block_height
        );
    }

    fn captured_sortition_snapshots(fixture_root: &Path) -> Vec<CapturedSortitionSnapshot> {
        let path = fixture_root.join("sortition/snapshots.json");
        let snapshots: Vec<CapturedSortitionSnapshot> =
            serde_json::from_slice(&fs::read(&path).expect("read captured sortition snapshots"))
                .expect("parse captured sortition snapshots");
        assert_eq!(
            snapshots.first().map(|snapshot| snapshot.block_height),
            Some(0)
        );
        snapshots
    }

    /// A replay of the captured Bitcoin chain into sortition snapshots.
    struct CapturedReplay {
        fixture_root: PathBuf,
        chain: nano_sortition::SnapshotChain,
        pox: nano_sortition::PoxIdTracker,
        pre_stx: PreStxCache,
    }

    impl CapturedReplay {
        fn new(fixture_root: &Path, genesis: &CapturedSortitionSnapshot) -> Self {
            let chain =
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
            assert_eq!(
                chain.tip().sortition_id.as_bytes(),
                &hex_array(&genesis.sortition_id)
            );
            let schedule = nano_sortition::RewardCycleSchedule::new(0, 20, Some(280))
                .expect("captured reward-cycle schedule is valid");
            Self {
                fixture_root: fixture_root.to_path_buf(),
                chain,
                pox: nano_sortition::PoxIdTracker::new(schedule),
                pre_stx: PreStxCache::new(),
            }
        }

        /// Replay a captured Bitcoin block, against the snapshot it produced.
        fn append(&mut self, snapshot: &CapturedSortitionSnapshot) {
            assert_eq!(
                self.chain.tip().bitcoin_header_hash.as_bytes(),
                &hex_array(&snapshot.parent_burn_header_hash)
            );
            let block = captured_bitcoin_block(&self.fixture_root, snapshot, &mut self.pre_stx);
            let winner = (snapshot.sortition != 0).then(|| {
                let winning_txid = hex_array(&snapshot.winning_block_txid);
                block
                    .operations
                    .iter()
                    .find(|operation| operation.txid == winning_txid)
                    .and_then(|operation| match operation.kind {
                        nano_bitcoin::BitcoinOperationKind::LeaderBlockCommit {
                            new_seed, ..
                        } => Some(nano_sortition::SortitionWinner {
                            txid: winning_txid,
                            vrf_seed: new_seed,
                        }),
                        _ => None,
                    })
                    .unwrap_or_else(|| {
                        panic!(
                            "missing winning commitment at Bitcoin height {}",
                            snapshot.block_height
                        )
                    })
            });
            let pox_id = self
                .pox
                .advance(snapshot.block_height, snapshot.pox_valid != 0)
                .expect("captured Bitcoin heights are contiguous")
                .clone();
            let total_burn = snapshot
                .total_burn
                .parse::<u64>()
                .expect("captured total burn is a u64");
            let derived = self
                .chain
                .append_with_winner(&block, total_burn, pox_id, winner)
                .expect("contiguous captured Bitcoin block");
            assert_captured_snapshot(derived, snapshot, total_burn);
        }

        /// Replay a captured Bitcoin block at the tip's next height, standing in
        /// for a branch Bitcoin later replaces.
        fn append_off_chain(&mut self, snapshot: &CapturedSortitionSnapshot) {
            let height = self.chain.tip().bitcoin_height + 1;
            let mut spliced = snapshot.clone();
            spliced.block_height = height;
            let block = captured_bitcoin_block(&self.fixture_root, &spliced, &mut self.pre_stx);
            let pox_id = self
                .pox
                .advance(height, true)
                .expect("spliced Bitcoin heights are contiguous")
                .clone();
            let total_burn = self.chain.tip().total_burn;
            self.chain
                .append(&block, total_burn, pox_id)
                .expect("contiguous spliced Bitcoin block");
        }
    }

    fn captured_bitcoin_block(
        fixture_root: &Path,
        snapshot: &CapturedSortitionSnapshot,
        pre_stx_cache: &mut PreStxCache,
    ) -> nano_bitcoin::BitcoinBlock {
        let path = fixture_root
            .join("bitcoin/blocks")
            .join(format!("{}.hex", snapshot.burn_header_hash));
        let raw = fs::read_to_string(path).unwrap_or_else(|error| {
            panic!(
                "missing Bitcoin block at height {}: {error}",
                snapshot.block_height
            )
        });
        decode_block_with_pre_stx(
            snapshot.block_height,
            &hex::decode(raw.trim()).expect("decode captured Bitcoin block"),
            *b"T3",
            pre_stx_cache,
        )
        .unwrap_or_else(|error| {
            panic!(
                "decode captured Bitcoin block at height {}: {error}",
                snapshot.block_height
            )
        })
    }

    fn assert_captured_snapshot(
        derived: &nano_sortition::SortitionSnapshot,
        snapshot: &CapturedSortitionSnapshot,
        total_burn: u64,
    ) {
        let height = snapshot.block_height;
        assert_eq!(derived.total_burn, total_burn, "{height}");
        assert_eq!(
            derived.operations_hash.as_bytes(),
            &hex_array(&snapshot.ops_hash),
            "{height}"
        );
        assert_eq!(
            derived.winner_txid,
            (snapshot.sortition != 0).then(|| hex_array(&snapshot.winning_block_txid)),
            "{height}"
        );
        assert_eq!(
            derived.consensus_hash.as_bytes(),
            &hex_array(&snapshot.consensus_hash),
            "{height}"
        );
        assert_eq!(
            derived.sortition_id.as_bytes(),
            &hex_array(&snapshot.sortition_id),
            "{height}"
        );
        assert_eq!(
            derived.parent_sortition_id.as_bytes(),
            &hex_array(&snapshot.parent_sortition_id),
            "{height}"
        );
        assert_eq!(
            derived.sortition_hash.as_bytes(),
            &hex_array(&snapshot.sortition_hash),
            "{height}"
        );
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
                    &nano_sortition::accepted_operation_txids(&block),
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
            for transaction in block.into_executed_and_skipped_txs() {
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
            let mut nano_transactions = Vec::with_capacity(block.tx_count());
            for transaction in block.executed_and_skipped_txs() {
                let mut reference = Vec::new();
                transaction
                    .consensus_serialize(&mut reference)
                    .expect("serialize reference transaction");
                let (nano, consumed) =
                    NanoTransaction::decode(&reference).expect("decode transaction");
                assert_eq!(consumed, reference.len());
                assert_eq!(nano.encode(), reference);
                nano.verify_authorization()
                    .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
                let authorization_end = 5 + nano.auth().encode().len();
                let rejects_tampering = (6..authorization_end).any(|index| {
                    let mut mutated = reference.clone();
                    mutated[index] ^= 1;
                    NanoTransaction::decode(&mutated).is_ok_and(|(transaction, consumed)| {
                        consumed == mutated.len() && transaction.verify_authorization().is_err()
                    })
                });
                assert!(
                    rejects_tampering,
                    "{} has no authorization byte protected by verification",
                    path.display()
                );
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
    fn nano_signed_transactions_verify_in_stacks_core() {
        let key = StacksPrivateKey::from_seed(b"nano-transaction-signer");
        let blocks = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/nakamoto/blocks");
        let mut payloads = Vec::new();
        for entry in fs::read_dir(blocks).expect("read fixture blocks") {
            let bytes = fs::read(entry.expect("fixture entry").path()).expect("read fixture block");
            let block = ReferenceNakamotoBlock::consensus_deserialize(&mut Cursor::new(&bytes))
                .expect("decode fixture block");
            for transaction in block.into_executed_and_skipped_txs() {
                let mut encoded = Vec::new();
                transaction
                    .consensus_serialize(&mut encoded)
                    .expect("serialize fixture transaction");
                let (nano, _) = NanoTransaction::decode(&encoded).expect("decode fixture");
                payloads.push(nano.payload().data().clone());
            }
        }
        assert!(!payloads.is_empty(), "fixture corpus contains transactions");

        for (nonce, payload) in payloads.into_iter().enumerate() {
            let signed = NanoTransaction::sign_standard(
                nano_codec::TransactionVersion::Testnet,
                0x8000_0000,
                nano_codec::AnchorMode::OnChainOnly,
                &key,
                nonce as u64,
                180,
                payload,
            )
            .expect("nano signs a standard transaction");
            let encoded = signed.encode();
            let reference =
                ReferenceStacksTransaction::consensus_deserialize(&mut Cursor::new(&encoded))
                    .expect("stacks-core decodes the nano-signed transaction");

            reference
                .verify(TransactionAuthVerificationMode::EnforceLowS)
                .expect("stacks-core accepts the nano-signed authorization");
            assert_eq!(reference.txid().as_bytes(), signed.txid().as_bytes());
            assert_eq!(
                reference.origin_address().to_string(),
                signed
                    .origin_address()
                    .expect("nano transaction has an origin")
                    .to_string()
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
            payloads.extend(
                block
                    .into_executed_and_skipped_txs()
                    .into_iter()
                    .map(|transaction| transaction.payload),
            );
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
            let transaction_index = transaction_index % block.tx_count();
            let mut transaction = block
                .executed_and_skipped_txs_mut()
                .remove(transaction_index);
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
            for (transaction_index, transaction) in block
                .into_executed_and_skipped_txs()
                .into_iter()
                .enumerate()
            {
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
    fn stackerdb_chunk_encoding_matches_stacks_core() {
        let key = StacksPrivateKey::from_seed(b"stackerdb conformance");
        let mut chunk = Chunk::new(7, 11, vec![1, 2, 3, 4, 5]);
        chunk.sign(&key).expect("sign chunk");
        let encoded = chunk.encode().expect("encode chunk");

        let mut cursor = Cursor::new(encoded.as_slice());
        let reference = libstackerdb::StackerDBChunkData::consensus_deserialize(&mut cursor)
            .expect("reference decodes chunk");
        let mut reference_encoded = Vec::new();
        reference
            .consensus_serialize(&mut reference_encoded)
            .expect("reference encodes chunk");
        assert_eq!(reference_encoded, encoded);
    }

    #[test]
    fn signer_acceptance_encoding_matches_stacks_core() {
        let key = StacksPrivateKey::from_seed(b"signer acceptance conformance");
        let digest = sha512_256(b"candidate block");
        let signature = key.sign(digest.as_bytes());
        let message = SignerMessage::BlockResponse(BlockResponse::Accepted(BlockAcceptance::new(
            digest, signature,
        )));
        let encoded = message.encode().expect("encode signer message");

        let reference =
            libsigner::v0::messages::SignerMessage::consensus_deserialize(&mut encoded.as_slice())
                .expect("reference decodes signer message");
        let libsigner::v0::messages::SignerMessage::BlockResponse(
            libsigner::v0::messages::BlockResponse::Accepted(accepted),
        ) = reference
        else {
            panic!("reference did not decode an accepted response");
        };
        assert_eq!(accepted.signer_signature_hash.0, *digest.as_bytes());
        assert_eq!(accepted.signature.as_bytes(), signature.as_bytes());
        assert_eq!(accepted.response_data.tenure_extend_timestamp, u64::MAX);
        assert_eq!(
            accepted.response_data.tenure_extend_read_count_timestamp,
            u64::MAX
        );
    }

    #[test]
    fn state_machine_update_round_trips_stacks_core() {
        let update = nano_stackerdb::StateMachineUpdate {
            active_protocol_version: 2,
            local_supported_protocol_version: 2,
            bitcoin_consensus_hash: nano_primitives::ConsensusHash::from_bytes([3; 20]),
            bitcoin_height: 4_242,
            current_miner: nano_stackerdb::CurrentMiner::Active {
                public_key_hash: nano_primitives::Hash160::from_bytes([5; 20]),
                tenure_consensus_hash: nano_primitives::ConsensusHash::from_bytes([6; 20]),
                parent_tenure_consensus_hash: nano_primitives::ConsensusHash::from_bytes([7; 20]),
                parent_tenure_last_block: nano_primitives::StacksBlockId::from_bytes([8; 32]),
                parent_tenure_last_block_height: 909,
            },
            replay_transactions: Vec::new(),
        };
        let encoded = SignerMessage::StateMachineUpdate(update.clone())
            .encode()
            .expect("encode state machine update");

        let reference =
            libsigner::v0::messages::SignerMessage::consensus_deserialize(&mut encoded.as_slice())
                .expect("reference decodes state machine update");
        let libsigner::v0::messages::SignerMessage::StateMachineUpdate(reference_update) =
            &reference
        else {
            panic!("reference did not decode a state machine update");
        };
        let libsigner::v0::messages::StateMachineUpdateContent::V2 {
            burn_block,
            burn_block_height,
            current_miner,
            replay_transactions,
        } = &reference_update.content
        else {
            panic!("reference did not decode version 2 content");
        };
        assert_eq!(burn_block.0, *update.bitcoin_consensus_hash.as_bytes());
        assert_eq!(*burn_block_height, update.bitcoin_height);
        assert!(replay_transactions.is_empty());
        assert!(matches!(
            current_miner,
            libsigner::v0::messages::StateMachineUpdateMinerState::ActiveMiner {
                parent_tenure_last_block_height,
                ..
            } if *parent_tenure_last_block_height == 909
        ));

        let mut reference_encoded = Vec::new();
        reference
            .consensus_serialize(&mut reference_encoded)
            .expect("reference re-encodes state machine update");
        assert_eq!(reference_encoded, encoded);
        assert_eq!(
            SignerMessage::decode(&encoded).expect("nano decodes its own update"),
            SignerMessage::StateMachineUpdate(update)
        );
    }

    #[test]
    fn signer_rejection_round_trips_stacks_core() {
        let key = ReferenceSecp256k1PrivateKey::from_seed(b"signer rejection conformance");
        let digest = Sha512Trunc256Sum(*sha512_256(b"candidate block").as_bytes());
        let response = libsigner::v0::messages::BlockResponse::rejected(
            digest.clone(),
            libsigner::v0::messages::RejectReason::InvalidMiner,
            &key,
            false,
            17,
            19,
        );
        let reference = libsigner::v0::messages::SignerMessage::BlockResponse(response);
        let mut encoded = Vec::new();
        reference
            .consensus_serialize(&mut encoded)
            .expect("encode stock rejection");

        let decoded = SignerMessage::decode(&encoded).expect("nano decodes stock rejection");
        let SignerMessage::BlockResponse(BlockResponse::Rejected(rejection)) = &decoded else {
            panic!("nano did not decode a rejected response");
        };
        assert_eq!(rejection.reason, "The miner has been marked as invalid.");
        assert_eq!(rejection.signer_signature_hash.as_bytes(), &digest.0);
        assert_eq!(decoded.encode().expect("nano reencodes rejection"), encoded);
    }

    #[test]
    fn signer_proposal_decoding_matches_stacks_core() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let path = captured_block_paths(&fixture)
            .into_iter()
            .next()
            .expect("captured block");
        let bytes = fs::read(path).expect("read captured block");
        let reference_block = ReferenceNakamotoBlock::consensus_deserialize(&mut bytes.as_slice())
            .expect("reference decodes captured block");
        let message =
            libsigner::v0::messages::SignerMessage::BlockProposal(libsigner::BlockProposal {
                block: reference_block,
                burn_height: 1_110,
                reward_cycle: 1,
                block_proposal_data: libsigner::BlockProposalData::empty(),
            });
        let mut encoded = Vec::new();
        message
            .consensus_serialize(&mut encoded)
            .expect("reference encodes proposal");

        let SignerMessage::BlockProposal(proposal) =
            SignerMessage::decode(&encoded).expect("nano decodes proposal")
        else {
            panic!("nano did not decode a proposal");
        };
        assert_eq!(proposal.block.encode(), bytes);
        assert_eq!(proposal.bitcoin_height, 1_110);
        assert_eq!(proposal.reward_cycle, 1);
        assert_eq!(
            SignerMessage::BlockProposal(proposal)
                .encode()
                .expect("nano encodes proposal"),
            encoded
        );
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

    fn hex_array<const N: usize>(value: &str) -> [u8; N] {
        let mut bytes = [0; N];
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
