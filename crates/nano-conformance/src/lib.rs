use std::{
    collections::BTreeMap,
    fmt::Write,
    fs,
    path::{Path, PathBuf},
};

use nano_bitcoin::{
    BitcoinOperation, BitcoinOperationKind, PreStxCache, decode_block, decode_block_with_pre_stx,
};
use nano_chainstate::{
    BitcoinBlockContext, CHECKPOINT_HISTORY_LIMIT, ChainState, NakamotoBlock, TenureAccounting,
};
use nano_codec::{TenureChangeCause, TransactionPayloadData};
use nano_primitives::{Network, TrieHash};
use serde::Deserialize;

/// The minimum metadata needed to make replay depth visible before fixture
/// capture is available.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FixtureManifest {
    pub mode: FixtureMode,
    pub replay_blocks: u64,
    /// Whether the capture carries the event-observer receipts.
    ///
    /// They come from an observer attached to a running node, so a capture
    /// taken from an archived chainstate has none. The state root is in the
    /// block header either way, which is why the two are checked separately.
    pub receipts: bool,
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
    /// stacks-core's own `pox_payouts` column, verbatim: a JSON pair of the payout
    /// addresses and the amount *per address*.
    ///
    /// The address list is padded to the number of payout outputs a commitment in
    /// that block carries (`SortitionHandleTx::get_num_pox_payouts`), so its length
    /// states that count and `amount × length` is the block's whole payout burn —
    /// which is what Clarity reads back as `miner-spend-total`. Taking it from here
    /// rather than re-deriving it means the replay's oracle for that field is the
    /// archive rather than nano's own arithmetic.
    pox_payouts: String,
    /// The tenure's sortition hash, which a coinbase VRF proof is over.
    ///
    /// Read from the archive rather than re-derived, like `pox_payouts` above and
    /// for the same reason: it is the oracle. Leaving it out left every captured
    /// context's hash at zero, which no VRF proof can verify against -- so the five
    /// `tenure_vrf_enforcement` gates skipped themselves on *every* capture and
    /// reported it as a missing leader key.
    #[serde(default)]
    sortition_hash: String,
}

/// The payout outputs and the total burn a snapshot's `pox_payouts` states.
///
/// The count comes back as well as the total because the winner's own share has to
/// be summed over exactly that many of its outputs: everything after them is the
/// miner's change, which on mainnet is three orders of magnitude larger than the
/// commitment.
fn captured_pox_payouts(encoded: &str) -> Option<(usize, u128)> {
    let (addresses, per_output): (Vec<serde_json::Value>, u128) =
        serde_json::from_str(encoded).ok()?;
    let outputs = addresses.len();
    Some((outputs, per_output.checked_mul(outputs as u128)?))
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointBoundaryRecord {
    parent_tenure_consensus_hash: String,
    coinbase_vrf_proof: String,
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
        // Absent means present: every capture written before receipts could be
        // left out carries them.
        let receipts = contents
            .lines()
            .map(str::trim)
            .find_map(|line| line.strip_prefix("receipts ="))
            .is_none_or(|value| value.trim() != "false");
        Ok(Self {
            mode,
            replay_blocks: value,
            receipts,
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

    validate_capture_layout(root, &manifest)?;
    validate_sortition_seed(root)?;
    validate_checkpoint(root)?;

    Ok(FixtureStatus::Captured {
        replay_blocks: manifest.replay_blocks,
    })
}

fn validate_capture_layout(
    root: &Path,
    manifest: &FixtureManifest,
) -> Result<(), FixtureValidationError> {
    let mut requirements = vec![
        // Several Nakamoto blocks can share one burn block in the same tenure.
        ("bitcoin/blocks", 1),
        ("nakamoto/blocks", manifest.replay_blocks),
        ("stacker_set", 1),
    ];
    if manifest.receipts {
        requirements.push(("events/new_block", manifest.replay_blocks));
    }
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
    Ok(())
}

fn validate_sortition_seed(root: &Path) -> Result<(), FixtureValidationError> {
    let snapshots_path = root.join("sortition/snapshots.json");
    let snapshots: Vec<CapturedBitcoinSnapshot> = serde_json::from_slice(
        &fs::read(&snapshots_path)
            .map_err(|_| FixtureValidationError::InvalidSnapshotFile(snapshots_path.clone()))?,
    )
    .map_err(|_| FixtureValidationError::InvalidSnapshotFile(snapshots_path.clone()))?;
    if snapshots.is_empty() {
        return Err(FixtureValidationError::InvalidSnapshotFile(snapshots_path));
    }
    for snapshot in &snapshots {
        let block = root
            .join("bitcoin/blocks")
            .join(format!("{}.hex", snapshot.burn_header_hash));
        if !is_nonempty_file(&block)? {
            return Err(FixtureValidationError::MissingOrEmptyFile(block));
        }
    }

    let sortition = root.join("sortition");
    let mut tracker =
        nano_node::sortition::SortitionTracker::from_capture(&sortition).map_err(|error| {
            FixtureValidationError::InvalidSortitionSeed {
                path: sortition.clone(),
                reason: error.to_string(),
            }
        })?;
    tracker
        .recover_seed(|height| {
            let snapshot = snapshots
                .iter()
                .find(|snapshot| snapshot.block_height == height)
                .ok_or_else(|| format!("no captured Bitcoin snapshot at burn {height}"))?;
            let path = root
                .join("bitcoin/blocks")
                .join(format!("{}.hex", snapshot.burn_header_hash));
            let encoded = fs::read_to_string(&path)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            let raw = hex::decode(encoded.trim())
                .map_err(|error| format!("{}: {error}", path.display()))?;
            decode_block(height, &raw, captured_magic(root))
                .map_err(|error| format!("{}: {error}", path.display()))
        })
        .map_err(|error| FixtureValidationError::InvalidSortitionSeed {
            path: sortition,
            reason: error.to_string(),
        })?;
    Ok(())
}

fn validate_checkpoint(root: &Path) -> Result<(), FixtureValidationError> {
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
    let published = nano_marf::CheckpointManifest::load(&checkpoint).map_err(|_| {
        FixtureValidationError::InvalidCheckpointManifest(checkpoint_manifest.clone())
    })?;
    validate_checkpoint_authentication_history(
        &checkpoint.join("authentication-history"),
        published.source_state_id,
        published.state_index_root,
    )?;
    let accounting_path = checkpoint.join("native-effects.json");
    TenureAccounting::from_json(
        &fs::read(&accounting_path)
            .map_err(|_| FixtureValidationError::MissingOrEmptyFile(accounting_path.clone()))?,
    )
    .map_err(|_| FixtureValidationError::InvalidNativeAccounting(accounting_path))?;
    Ok(())
}

fn invalid_checkpoint_history(path: &Path, reason: impl Into<String>) -> FixtureValidationError {
    FixtureValidationError::InvalidCheckpointHistory {
        path: path.to_owned(),
        reason: reason.into(),
    }
}

fn history_hex<const LENGTH: usize>(
    root: &Path,
    field: &str,
    value: &str,
) -> Result<[u8; LENGTH], FixtureValidationError> {
    hex::decode(value)
        .ok()
        .and_then(|bytes| <[u8; LENGTH]>::try_from(bytes.as_slice()).ok())
        .ok_or_else(|| {
            invalid_checkpoint_history(
                root,
                format!("{field} is not exactly {LENGTH} hexadecimal bytes"),
            )
        })
}

fn checkpoint_history_boundary(root: &Path) -> Result<[u8; 20], FixtureValidationError> {
    let boundary_path = root.join("boundary.json");
    let boundary: CheckpointBoundaryRecord = fs::read(&boundary_path)
        .map_err(|error| invalid_checkpoint_history(root, error.to_string()))
        .and_then(|bytes| {
            serde_json::from_slice(&bytes)
                .map_err(|error| invalid_checkpoint_history(root, error.to_string()))
        })?;
    let consensus = history_hex::<20>(
        root,
        "parent_tenure_consensus_hash",
        &boundary.parent_tenure_consensus_hash,
    )?;
    let proof = history_hex::<80>(root, "coinbase_vrf_proof", &boundary.coinbase_vrf_proof)?;
    nano_crypto::VrfProof::from_bytes(&proof)
        .map_err(|error| invalid_checkpoint_history(root, error.to_string()))?;
    Ok(consensus)
}

fn checkpoint_history_paths(root: &Path) -> Result<Vec<PathBuf>, FixtureValidationError> {
    let blocks_directory = root.join("blocks");
    let entries = fs::read_dir(&blocks_directory)
        .map_err(|error| invalid_checkpoint_history(root, error.to_string()))?;
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|error| invalid_checkpoint_history(root, error.to_string()))?
            .path();
        if path.extension().is_some_and(|extension| extension == "bin") {
            paths.push(path);
        }
    }
    if paths.is_empty() {
        return Err(invalid_checkpoint_history(
            root,
            "blocks directory contains no Nakamoto block files",
        ));
    }
    if paths.len() > CHECKPOINT_HISTORY_LIMIT {
        return Err(invalid_checkpoint_history(
            root,
            format!(
                "{} blocks exceed the bounded limit of {CHECKPOINT_HISTORY_LIMIT}",
                paths.len()
            ),
        ));
    }
    Ok(paths)
}

fn decode_checkpoint_history_blocks(
    root: &Path,
    paths: Vec<PathBuf>,
) -> Result<BTreeMap<[u8; 32], NakamotoBlock>, FixtureValidationError> {
    let mut by_id = BTreeMap::new();
    for path in paths {
        let block = fs::read(&path)
            .map_err(|error| invalid_checkpoint_history(root, error.to_string()))
            .and_then(|bytes| {
                NakamotoBlock::decode(&bytes)
                    .map_err(|error| invalid_checkpoint_history(root, error.to_string()))
            })?;
        let id = *block.block_id().as_bytes();
        if by_id.insert(id, block).is_some() {
            return Err(invalid_checkpoint_history(
                root,
                format!("block {} occurs more than once", hex::encode(id)),
            ));
        }
    }
    Ok(by_id)
}

fn ordered_checkpoint_history(
    root: &Path,
    mut by_id: BTreeMap<[u8; 32], NakamotoBlock>,
    source: [u8; 32],
) -> Result<Vec<NakamotoBlock>, FixtureValidationError> {
    let mut cursor = source;
    let mut reversed = Vec::with_capacity(by_id.len());
    while let Some(block) = by_id.remove(&cursor) {
        cursor = *block.header.parent_block_id.as_bytes();
        reversed.push(block);
    }
    if reversed.is_empty() {
        return Err(invalid_checkpoint_history(
            root,
            format!(
                "history contains no checkpoint source block {}",
                hex::encode(source)
            ),
        ));
    }
    if !by_id.is_empty() {
        return Err(invalid_checkpoint_history(
            root,
            format!(
                "{} block(s) are disconnected from checkpoint source {}",
                by_id.len(),
                hex::encode(source)
            ),
        ));
    }
    reversed.reverse();
    Ok(reversed)
}

fn validate_checkpoint_history_blocks(
    root: &Path,
    history: &[NakamotoBlock],
    state_root: TrieHash,
    boundary_consensus: [u8; 20],
) -> Result<(), FixtureValidationError> {
    let first = &history[0];
    let last = &history[history.len() - 1];
    if !nano_chainstate::starts_new_tenure(first) {
        return Err(invalid_checkpoint_history(
            root,
            "history does not begin at a tenure-start block",
        ));
    }
    if last.header.state_index_root != state_root {
        return Err(invalid_checkpoint_history(
            root,
            format!(
                "source block publishes state root {}, not {}",
                last.header.state_index_root, state_root
            ),
        ));
    }
    for pair in history.windows(2) {
        let parent = &pair[0];
        let child = &pair[1];
        if child.header.parent_block_id != parent.block_id()
            || child.header.chain_length != parent.header.chain_length.saturating_add(1)
        {
            return Err(invalid_checkpoint_history(
                root,
                format!(
                    "history is not contiguous at Stacks height {}",
                    child.header.chain_length
                ),
            ));
        }
    }
    let previous_tenure =
        first
            .transactions
            .iter()
            .find_map(|transaction| match transaction.payload().data() {
                TransactionPayloadData::TenureChange(payload)
                    if payload.cause == TenureChangeCause::BlockFound =>
                {
                    Some(payload.previous_tenure_consensus_hash)
                }
                _ => None,
            });
    if previous_tenure.map(|hash| *hash.as_bytes()) != Some(boundary_consensus) {
        return Err(invalid_checkpoint_history(
            root,
            "boundary proof names a different parent tenure than the first history block",
        ));
    }
    Ok(())
}

fn validate_checkpoint_authentication_history(
    root: &Path,
    source: [u8; 32],
    state_root: TrieHash,
) -> Result<(), FixtureValidationError> {
    let boundary_consensus = checkpoint_history_boundary(root)?;
    let paths = checkpoint_history_paths(root)?;
    let by_id = decode_checkpoint_history_blocks(root, paths)?;
    let history = ordered_checkpoint_history(root, by_id, source)?;
    validate_checkpoint_history_blocks(root, &history, state_root, boundary_consensus)
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
    InvalidSortitionSeed {
        path: PathBuf,
        reason: String,
    },
    InvalidCheckpointHistory {
        path: PathBuf,
        reason: String,
    },
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
            Self::InvalidSortitionSeed { path, reason } => write!(
                formatter,
                "invalid checkpoint sortition seed under {}: {reason}",
                path.display()
            ),
            Self::InvalidCheckpointHistory { path, reason } => write!(
                formatter,
                "invalid checkpoint authentication history under {}: {reason}",
                path.display()
            ),
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
    /// The two engines answer a call in this block differently, checked before
    /// anything was sealed.
    Engine(String),
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
    scoreboard_result(root, manifest).0
}

/// The board, and whether every required surface on it passed.
///
/// The second half is what makes it a gate. `scoreboard` printed a table naming a
/// consensus divergence and exited zero, because loading the manifest had
/// succeeded -- so the command that exists to say whether nano computes the same
/// chain as stacks-core reported failure and success at the same time, and only the
/// half nobody parses said failure.
///
/// A required surface is the captured replay: roots, receipts and costs over a
/// bounded fixture whose oracle is stacks-core's own output. A cost divergence
/// counts, because a cost decides block admission even where the root matches.
#[must_use]
pub fn scoreboard_result(root: &Path, manifest: FixtureManifest) -> (String, bool) {
    let replay = match manifest.mode {
        FixtureMode::Baseline => baseline_replay(manifest),
        FixtureMode::Captured => captured_replay(root, manifest),
    };
    // A baseline tree has nothing to replay and is not a failing replay: it is the
    // state the scoreboard starts in, before any fixture is captured.
    let required = match manifest.mode {
        FixtureMode::Baseline => true,
        FixtureMode::Captured => {
            replay.completed == replay.expected
                && replay.first_failure.is_none()
                && replay.first_divergence.is_none()
                && replay.first_cost_divergence.is_none()
        }
    };
    (render_scoreboard(manifest, &replay), required)
}

/// What the board can say about mainnet, which is a different question from what
/// it says about the captured fixture.
///
/// The captured rows are a *replay against an oracle*: 340 blocks whose roots and
/// receipts stacks-core produced. Mainnet has no such oracle for receipts — no
/// public API serves a historical `new_block` — so the two things it can report are
/// the depth a durable state has actually executed to, and whether the frozen
/// regression slice is intact. Both are read from disk and neither runs anything,
/// so the board stays a command that answers in milliseconds.
///
/// `NANO_MAINNET_STATE` names a node's state directory. Without it the row says so
/// rather than saying zero, because zero is what a divergence at the first block
/// looks like.
fn mainnet_rows() -> String {
    let mut output = String::new();
    let depth = std::env::var_os("NANO_MAINNET_STATE")
        .map(PathBuf::from)
        .and_then(|state| mainnet_executed_height(&state));
    match depth {
        Some((anchor, tip)) => {
            let _ = writeln!(
                output,
                "replay: mainnet root durable executed tip   {:>9}  from {anchor}",
                tip.saturating_sub(anchor)
            );
        }
        None => {
            let _ = writeln!(
                output,
                "replay: mainnet root durable executed tip   no state  NANO_MAINNET_STATE"
            );
        }
    }
    match frozen_receipt_slice() {
        Some((blocks, first, last)) => {
            let _ = writeln!(
                output,
                "regression: mainnet  frozen receipt digests   {blocks:>4}/{blocks}      {first}-{last}"
            );
        }
        None => {
            let _ = writeln!(
                output,
                "regression: mainnet  frozen receipt digests    absent  fixtures/mainnet/receipts.json"
            );
        }
    }
    output
}

/// The anchor a mainnet state was imported at, and the height it has executed to.
///
/// Read out of the state on disk, because that is the only height that means
/// anything: a fetched, staged or peer-reported one is not a block this node has
/// executed.
///
/// The *deepest seal* is not that height either, and the difference is not
/// theoretical. A block is committed by writing its ledger and then sealing the
/// MARF, so a sealed state no ledger names is a block this node abandoned rather
/// than executed. A live mainnet state held seals to 8,713,522 against a ledger
/// naming 8,713,221 — this row would have reported 301 blocks of replay depth that
/// no restart could stand on, which is precisely the overstatement the north-star
/// metric exists to rule out. So the walk goes down to the deepest block a ledger
/// names, and reports that.
fn mainnet_executed_height(state: &Path) -> Option<(u64, u64)> {
    let marf = nano_marf::VersionedMarf::open(state.join("chainstate/marf.sqlite")).ok()?;
    let tip = executed_tip(state, &marf, marf.tip().ok()??)?;
    let height = marf.height(tip).ok()??;
    // A checkpointed state's ancestry arrives with the import, so the anchor is the
    // first height this node sealed itself: the checkpoint's own height plus one.
    // `marf.first_sealed_height` is not a thing the MARF records, and the capture
    // manifest is the wrong place to ask because a state can outlive it -- so the
    // anchor is taken from the environment where it is known and the row reports the
    // tip alone where it is not.
    let anchor = std::env::var("NANO_MAINNET_ANCHOR")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| u64::from(height));
    Some((anchor, u64::from(height)))
}

/// Give an executor the sortition chain the capture carries.
///
/// A rig that skips this executes nothing now, and that is the point:
/// [[077-remove-peer-derived-consensus-execution-fallbacks]] removed the path where
/// a peer's `/v3/sortitions` answer became the burn view a block ran under, so a
/// node -- or a harness -- without a locally derived chain has no burn view at all.
/// These rigs were exercising exactly that removed path, which is why they are the
/// ones that had to change.
///
/// Seeded from the capture's own `sortition/` directory, which is what a configured
/// node's `checkpoint.sortition` points at, so a harness and a node derive from the
/// same bytes by the same route.
///
/// # Panics
///
/// If the capture cannot seed a chain. A rig that silently ran without one is what
/// this is here to prevent.
pub fn derive_sortitions<S>(
    executor: &mut nano_node::CheckpointExecutor<S>,
    fixtures: &Path,
    state: &Path,
) where
    S: nano_bitcoin::BitcoinSource,
    S::Error: std::fmt::Display,
{
    let tracker = nano_node::sortition::SortitionTracker::resume_or_capture(
        state,
        &fixtures.join("sortition"),
    )
    .expect("the capture carries a sortition history a chain can be seeded from");
    executor.track_sortitions(tracker, state.to_path_buf());
}

/// Walk down from a seal to the deepest block the side store holds a ledger for.
///
/// The same rule the node resumes by, and bounded the same way: a run seals at most
/// a catch-up round's worth of blocks before it commits one, so a walk longer than
/// that has nothing to find. A state whose side store or MARF cannot be read is
/// omitted rather than reported at a plausible but unverified seal.
fn executed_tip(
    state: &Path,
    marf: &nano_marf::VersionedMarf,
    tip: nano_marf::MarfBlockId,
) -> Option<nano_marf::MarfBlockId> {
    const REACH: usize = 1000;
    let Ok(side_store) = rusqlite::Connection::open_with_flags(
        state.join("chainstate/clarity.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return None;
    };
    let mut walk = tip;
    for _ in 0..REACH {
        let named = side_store
            .prepare_cached("SELECT COUNT(*) FROM chain_ledger WHERE block_id = ?1")
            .and_then(|mut statement| {
                statement.query_row(rusqlite::params![&walk[..]], |row| row.get::<_, u32>(0))
            });
        match named {
            Ok(1..) => return Some(walk),
            Ok(0) => {}
            // No table, no column, no readable database: the walk cannot tell a seal
            // from a commit here, so it does not pretend to.
            Err(_) => return None,
        }
        match marf.parent(walk) {
            Ok(Some(Some(parent))) => walk = parent,
            Ok(_) => return Some(tip),
            Err(_) => return None,
        }
    }
    Some(tip)
}

/// How many blocks the frozen mainnet regression slice pins, and which.
fn frozen_receipt_slice() -> Option<(usize, u64, u64)> {
    #[derive(Deserialize)]
    struct Slice {
        first_height: u64,
        last_height: u64,
        blocks: Vec<ReceiptDigest>,
    }
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/mainnet/receipts.json");
    let slice: Slice = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    Some((slice.blocks.len(), slice.first_height, slice.last_height))
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
    // A capture without receipts must not read as a passing row: the state
    // root is checked, the receipts simply were not there to check.
    if manifest.receipts {
        let _ = writeln!(
            output,
            "replay: receipts     event observer receipts     {}/{}          {}",
            replay.completed, replay.expected, first_failure
        );
    } else {
        let _ = writeln!(
            output,
            "replay: receipts     event observer receipts   not captured  needs an observer"
        );
    }
    if !manifest.receipts {
        let _ = writeln!(
            output,
            "replay: costs        receipt cost dimensions   not captured  needs an observer"
        );
        let _ = write!(output, "{}", mainnet_rows());
        let _ = writeln!(
            output,
            "\nREPLAY DEPTH: {} / {} ({})",
            replay.completed, replay.expected, replay_mode
        );
        return output;
    }
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
    let _ = write!(output, "{}", mainnet_rows());
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
            receipts: true,
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
    replay_into(&mut chainstate, source, root, manifest, 0, visit)
}

/// Replay captured blocks into a chainstate the caller owns, from an offset.
///
/// A chainstate a caller can hold across calls is what makes a restart
/// testable: the same blocks, executed in one run or in two with the state
/// closed and reopened between, have to reach the same root and owe the same.
pub fn replay_into(
    chainstate: &mut ChainState,
    source: [u8; 32],
    root: &Path,
    manifest: FixtureManifest,
    skip: usize,
    visit: &mut dyn FnMut(&NakamotoBlock, &nano_chainstate::AppliedBlock),
) -> ReplayDepth {
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
    // A resumed run stands on what the previous one sealed.
    let mut parent = if skip == 0 {
        Some(source)
    } else {
        match chainstate.tip() {
            Ok(tip) => tip,
            Err(error) => {
                return replay_fixture_failure(
                    manifest,
                    &format!("the resumed state tip cannot be read: {error}"),
                );
            }
        }
    };
    let mut bitcoin_view = String::new();
    let mut first_cost_divergence = None;
    for (offset, path) in paths.into_iter().enumerate() {
        if offset < skip {
            continue;
        }
        if completed >= manifest.replay_blocks {
            break;
        }
        let block_number = u64::try_from(offset).unwrap_or(u64::MAX).saturating_add(1);
        let capture = ReplayInputs {
            root,
            snapshots: &snapshots,
            bitcoin_operations: &bitcoin_operations,
            receipts: manifest.receipts,
        };
        let (block, applied, cost_divergence) = match apply_captured_block(
            &capture,
            chainstate,
            parent,
            &mut bitcoin_view,
            &path,
            &mut |_| {},
            ChainState::execute_unauthenticated_fixture_block_with_bitcoin_operations,
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

/// The capture a test should read, which is not always the one in the tree.
///
/// `NANO_FIXTURES` points the suite at a capture outside the repository — the way
/// a mainnet or a live-hacknet one is read without installing it first, and the
/// same variable the scoreboard already honoured. A test that hardcodes
/// `CARGO_MANIFEST_DIR/fixtures` cannot be handed a chain the in-tree one does not
/// contain, which is how five VRF gates came to be unrunnable: the tree's capture
/// carries no leader-key registry, and the chain it came from is gone.
#[must_use]
pub fn capture_root(default: &Path) -> std::path::PathBuf {
    std::env::var_os("NANO_FIXTURES")
        .map_or_else(|| default.to_path_buf(), std::path::PathBuf::from)
}

/// Every captured block, in height order.
#[must_use]
pub fn captured_block_paths(fixture: &Path) -> Vec<std::path::PathBuf> {
    let mut paths = fs::read_dir(fixture.join("nakamoto/blocks"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

/// A chainstate opened on the captured checkpoint, and the state it starts from.
///
/// Public because a test that asks what the follow path *does* has to go through
/// the same door the replay does.
pub fn replay_chainstate(root: &Path) -> Result<(ChainState, [u8; 32]), &'static str> {
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

/// A durable chainstate over the captured checkpoint, resuming what `directory`
/// already holds.
///
/// The same door `nano-node` opens: a directory with a tip of its own recovers
/// the ledger committed with that tip, and only one with nothing sealed takes the
/// checkpoint's accounting. Tests that carried the accounting across a restart by
/// hand could not have caught the three fields nobody carried.
pub fn durable_replay_chainstate(
    root: &Path,
    directory: &Path,
) -> Result<(ChainState, [u8; 32]), String> {
    let (source, state_root) =
        checkpoint_state(root).ok_or("checkpoint metadata is unavailable")?;
    let mut chainstate = ChainState::open_from_checkpoint(
        captured_network(root),
        directory,
        root.join("chainstate/checkpoint-H/marf.sqlite"),
        source,
        state_root,
    )
    .map_err(|error| format!("the checkpoint cannot be opened: {error}"))?;
    let recovered = match chainstate
        .tip()
        .map_err(|error| format!("the state tip cannot be read: {error}"))?
        .filter(|tip| *tip != source)
    {
        Some(tip) => chainstate
            .recover_ledger_at(tip)
            .map_err(|error| format!("the ledger cannot be read back: {error}"))?,
        None => false,
    };
    if !recovered {
        let accounting = fs::read(root.join("chainstate/checkpoint-H/native-effects.json"))
            .ok()
            .and_then(|contents| TenureAccounting::from_json(&contents).ok())
            .ok_or("native accounting fixture cannot be loaded")?;
        *chainstate.accounting_mut() = accounting;
    }
    Ok((chainstate, source))
}

/// How many captured blocks a state directory has already sealed.
///
/// Counted from the fixtures rather than passed in, because a process that is
/// killed cannot report where it got to.
pub fn captured_blocks_sealed(root: &Path, chainstate: &ChainState) -> Result<usize, String> {
    let mut sealed = 0;
    for path in captured_block_paths(root) {
        let Some(block) = fs::read(path)
            .ok()
            .and_then(|bytes| NakamotoBlock::decode(&bytes).ok())
        else {
            break;
        };
        if !chainstate
            .has_block_state(*block.block_id().as_bytes())
            .map_err(|error| format!("the block state cannot be read: {error}"))?
        {
            break;
        }
        sealed += 1;
    }
    Ok(sealed)
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

/// What a replay carries from block to block, as against per block.
struct ReplayInputs<'a> {
    root: &'a Path,
    snapshots: &'a BTreeMap<String, BitcoinBlockContext>,
    bitcoin_operations: &'a BTreeMap<String, Vec<BitcoinOperation>>,
    receipts: bool,
}

/// One block's receipts, reduced to what a regression has to notice.
///
/// Kept as a digest rather than as the payload because 500 mainnet blocks of
/// receipts are 250 MB and this lives in CI. Every field a compiler change could
/// move is inside the digest: each transaction's identity, its status, all five
/// cost dimensions, the value it returned, and the ordered events it emitted. The
/// block's own identity is outside it, so a mismatch says *which* block and then
/// what about it.
///
/// `block` is the Nakamoto block hash, which is the signer signature hash, which
/// commits to `state_index_root` -- so freezing it pins the root without the
/// payload carrying one.
#[derive(Clone, Debug, Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct ReceiptDigest {
    pub height: u64,
    pub block: String,
    pub transactions: usize,
    pub events: usize,
    /// `Sha512_256` over the ordered receipts, hexadecimal.
    pub digest: String,
}

/// Reduce an event observer's `new_block` payload to a [`ReceiptDigest`].
///
/// The ordering is the payload's own, which is consensus: a receipt list is the
/// order the block executed in, and an event list is the order the block emitted.
#[must_use]
pub fn receipt_digest(payload: &serde_json::Value) -> ReceiptDigest {
    let strip = |value: &serde_json::Value| {
        value
            .as_str()
            .unwrap_or_default()
            .trim_start_matches("0x")
            .to_owned()
    };
    let mut preimage = Vec::new();
    let transactions = payload["transactions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for transaction in &transactions {
        preimage.extend_from_slice(strip(&transaction["txid"]).as_bytes());
        preimage.extend_from_slice(strip(&transaction["status"]).as_bytes());
        preimage.extend_from_slice(strip(&transaction["raw_result"]).as_bytes());
        let cost = &transaction["execution_cost"];
        for dimension in [
            "runtime",
            "read_count",
            "read_length",
            "write_count",
            "write_length",
        ] {
            preimage.extend_from_slice(cost[dimension].to_string().as_bytes());
        }
    }
    let events = payload["events"].as_array().cloned().unwrap_or_default();
    for event in &events {
        preimage.extend_from_slice(strip(&event["txid"]).as_bytes());
        preimage.extend_from_slice(strip(&event["type"]).as_bytes());
        preimage.extend_from_slice(event["committed"].to_string().as_bytes());
        // Whatever payload the event carries, canonically: an event's shape is
        // per type and enumerating them here would be a second definition of it.
        preimage.extend_from_slice(serde_json::to_vec(event).unwrap_or_default().as_slice());
    }
    ReceiptDigest {
        height: payload["block_height"].as_u64().unwrap_or_default(),
        block: strip(&payload["block_hash"]),
        transactions: transactions.len(),
        events: events.len(),
        digest: hex::encode(nano_primitives::sha512_256(&preimage).as_bytes()),
    }
}

/// Whether a gate that cannot run may quietly report nothing.
///
/// Most of these tests need a capture or a node's state directory, and skipping
/// when it is absent is right for a working tree. It is exactly wrong for a
/// release gate: a suite where every mainnet test skipped looks identical to one
/// where every mainnet test passed, and the difference is the whole question.
///
/// Setting `NANO_REQUIRE_MAINNET` turns every such skip into a failure, so a run
/// that claims the mainnet gates are green had to actually run them.
///
/// # Panics
///
/// When `NANO_REQUIRE_MAINNET` is set and the inputs a gate needs are not.
pub fn skip_gate(reason: &str) {
    assert!(
        std::env::var_os("NANO_REQUIRE_MAINNET").is_none(),
        "this gate cannot run and NANO_REQUIRE_MAINNET is set: {reason}"
    );
    eprintln!("skipped: {reason}");
}

/// Name an unavailable parameterized investigation without treating it as a
/// release gate. Diagnostic call sites are separately inventoried as optional.
pub fn skip_diagnostic(reason: &str) {
    eprintln!("diagnostic unavailable: {reason}");
}

/// Execute the next captured block with a state root its header does not commit
/// to, so that it is rejected exactly where a real divergence rejects it.
///
/// A node retries a block it cannot execute for as long as it runs, so what a
/// rejection leaves behind is not a detail — and reaching the rejection needs a
/// block whose transactions all succeed, which only a captured one does.
/// Returns whether the block was rejected.
pub fn reject_captured_block(
    chainstate: &mut ChainState,
    root: &Path,
    manifest: FixtureManifest,
    skip: usize,
) -> Result<bool, String> {
    let Some(snapshots) = captured_bitcoin_snapshots(root) else {
        return Ok(false);
    };
    let Some(bitcoin_operations) = captured_bitcoin_operations(root) else {
        return Ok(false);
    };
    let Ok(entries) = fs::read_dir(root.join("nakamoto/blocks")) else {
        return Ok(false);
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    paths.sort();
    let Some(path) = paths.into_iter().nth(skip) else {
        return Ok(false);
    };

    let capture = ReplayInputs {
        root,
        snapshots: &snapshots,
        bitcoin_operations: &bitcoin_operations,
        receipts: manifest.receipts,
    };
    let mut bitcoin_view = String::new();
    let parent = chainstate
        .tip()
        .map_err(|error| format!("the state tip cannot be read: {error}"))?;
    // The replay path accepts whatever root execution produces and compares
    // afterwards, which seals the block. A node following a chain verifies
    // before sealing, and that is the path whose rollback is in question.
    Ok(apply_captured_block(
        &capture,
        chainstate,
        parent,
        &mut bitcoin_view,
        &path,
        &mut |block| {
            // Move the block time rather than the committed root: every
            // transaction still executes and its fees still land, and only the
            // state the block produces differs from what its header commits to.
            // Corrupting the root instead gets the block turned away earlier,
            // before it has touched anything worth rolling back.
            block.header.timestamp = block.header.timestamp.wrapping_add(1);
        },
        ChainState::append_unauthenticated_fixture_block_with_bitcoin_operations,
    )
    .is_err())
}

/// How a captured block is executed: the replay accepts whatever root it
/// produces and compares afterwards, a following node verifies before sealing.
type ExecuteBlock = fn(
    &mut ChainState,
    nano_chainstate::BitcoinBlockContext,
    &[nano_bitcoin::BitcoinOperation],
    Option<[u8; 32]>,
    &NakamotoBlock,
) -> Result<nano_chainstate::AppliedBlock, nano_chainstate::ChainStateError>;

/// Whether to put every contract call through both engines before sealing.
///
/// Deliberately not named after any of the four interpreter switches task 060
/// retired from the production call path. `wasm_is_the_engine` forbids their
/// names anywhere in the tree and caught the first spelling of this one, which is
/// the check working: a name that reads like a retired production switch is a
/// hazard even when the thing behind it is a test.
///
/// Off by default and read only here, in the conformance harness. The
/// interpreter is a differential oracle and nothing else — task 060's boundary is
/// that the shipped node cannot reach it, and it cannot: it lives in
/// `nano-oracle`, which `nano-node` does not depend on. This is the *test* side of
/// that boundary, which the same task asks for in as many words.
fn crosschecking_engines() -> bool {
    std::env::var_os("NANO_REPLAY_BOTH_ENGINES").is_some()
}

/// Where the engine bench appends its samples, when `NANO_BENCH_ENGINES` names it.
fn benching_engines() -> Option<PathBuf> {
    std::env::var_os("NANO_BENCH_ENGINES").map(PathBuf::from)
}

/// A contract call as a captured transaction carries it, decoded once so the
/// crosscheck and the bench ask both engines exactly the same question.
struct CapturedCall<'a> {
    txid: String,
    sender: clarity::vm::types::PrincipalData,
    contract: clarity::vm::types::QualifiedContractIdentifier,
    function: &'a str,
    arguments: Vec<Vec<u8>>,
}

/// Every contract call in a block that both engines can be asked.
fn contract_calls(block: &NakamotoBlock) -> Vec<CapturedCall<'_>> {
    let mut calls = Vec::new();
    for transaction in &block.transactions {
        let TransactionPayloadData::ContractCall {
            address,
            contract_name,
            function_name,
            arguments,
        } = transaction.payload().data()
        else {
            continue;
        };
        let Some(origin) = transaction.origin_address() else {
            continue;
        };
        let Ok(sender) = clarity::vm::types::PrincipalData::parse(&origin.to_string()) else {
            continue;
        };
        let name = format!("{address}.{contract_name}");
        let Ok(contract) = clarity::vm::types::QualifiedContractIdentifier::parse(&name) else {
            continue;
        };
        calls.push(CapturedCall {
            txid: transaction.txid().to_string(),
            sender,
            contract,
            function: function_name,
            arguments: arguments
                .iter()
                .map(|argument| argument.as_bytes().to_vec())
                .collect(),
        });
    }
    calls
}

/// Ask one engine one call against the parent state, opened and aborted.
///
/// Answers the formatted result, the cost the call was charged, and how long
/// the engine took — the execution alone, not the open, the tracker build or
/// the abort, which cost the same whichever engine runs. `None` when the
/// parent cannot be opened, which ends the caller's whole comparison the way
/// it always has.
fn ask_engine_before_sealing(
    chainstate: &mut ChainState,
    parent: [u8; 32],
    call: &CapturedCall<'_>,
    interpreted: bool,
    free: bool,
) -> Option<(
    String,
    clarity::vm::costs::ExecutionCost,
    std::time::Duration,
)> {
    use clarity::vm::costs::{ExecutionCost, LimitedCostTracker};
    let vm = chainstate.vm_mut();
    vm.begin_block(Some(parent), [0xcc; 32]).ok()?;
    let tracker = if free {
        Some(LimitedCostTracker::new_free())
    } else {
        vm.transaction_cost_tracker().ok()
    };
    let started = std::time::Instant::now();
    let mut measured = None;
    let outcome = tracker.map(|tracker| {
        if interpreted {
            // The oracle reports the interpreter's own time, without the
            // healing scaffolding it wraps compiler-deployed contracts in.
            nano_oracle::interpret_contract_call_measured(
                vm,
                nano_oracle::ContractCall {
                    sender: call.sender.clone(),
                    sponsor: None,
                    contract: call.contract.clone(),
                    function: call.function,
                    arguments: &call.arguments,
                },
                tracker,
            )
            .map(|(outcome, took)| {
                measured = Some(took);
                outcome
            })
        } else {
            vm.execute_contract_call_outcome(
                call.sender.clone(),
                None,
                call.contract.clone(),
                call.function,
                &call.arguments,
                &tracker,
            )
        }
    });
    let took = measured.unwrap_or_else(|| started.elapsed());
    let (answer, cost) = match &outcome {
        Some(Ok(
            nano_vm::ContractCallOutcome::Success(result)
            | nano_vm::ContractCallOutcome::AbortedByResponse(result),
        )) => (format!("{:?}", result.value), result.cost.clone()),
        Some(Ok(nano_vm::ContractCallOutcome::RuntimeFailure { error, cost })) => {
            (format!("failed: {error:?}"), cost.clone())
        }
        Some(Err(error)) => (format!("error: {error:?}"), ExecutionCost::ZERO),
        None => (
            "error: the consensus cost tracker cannot be built".to_owned(),
            ExecutionCost::ZERO,
        ),
    };
    drop(vm.abort_block());
    Some((answer, cost, took))
}

/// Ask both engines every contract call in a block, before the block is applied.
///
/// Before, not after, and that is the whole point: at this moment the chainstate
/// stands on the parent, which is the state the call actually ran against. Each
/// call opens a block on the parent and aborts it, so nothing is sealed and no
/// root moves.
///
/// Answers a description of the first disagreement. Seven of the eight mainnet
/// divergences found so far were a compiler bug that showed up first as a
/// different answer to one call, and every one of them was localised with this
/// comparison run by hand (`xtask call-both-tx`); running it in the harness is
/// the same question asked without being asked to.
fn engines_disagree_before_sealing(
    chainstate: &mut ChainState,
    parent: Option<[u8; 32]>,
    block: &NakamotoBlock,
) -> Option<String> {
    let parent = parent?;
    for call in contract_calls(block) {
        let (compiled, ..) = ask_engine_before_sealing(chainstate, parent, &call, false, true)?;
        let (interpreted, ..) = ask_engine_before_sealing(chainstate, parent, &call, true, true)?;
        if compiled != interpreted {
            return Some(format!(
                "the engines answer {}::{} in transaction {} differently: \
                 clarity-wasm says {compiled} and the interpreter says {interpreted}. \
                 clarity-wasm is the engine that has to run mainnet, so this is a compiler \
                 bug to fix rather than a reason to prefer the other answer",
                call.contract, call.function, call.txid
            ));
        }
    }
    None
}

/// Time both engines on every contract call in a block, against the parent.
///
/// The same seam and the same open-and-abort discipline as
/// [`engines_disagree_before_sealing`], measured instead of compared: each call
/// runs once unmeasured — compiling the wasm module and walking the trie pages
/// a following node would already have warm — and then `NANO_BENCH_REPEATS`
/// times per engine, alternating so neither engine systematically inherits the
/// other's cache warmth. Consensus cost trackers rather than free ones, because
/// cost tracking is part of what an engine costs in production.
///
/// One tab-separated line per call: height, txid, contract, function, the
/// charged `runtime` cost dimension, the compiled and interpreted wall times in
/// nanoseconds (comma-joined repeats), and whether the answers agreed.
fn bench_engines_before_sealing(
    chainstate: &mut ChainState,
    parent: Option<[u8; 32]>,
    block: &NakamotoBlock,
    samples: &Path,
) -> std::io::Result<()> {
    use std::io::Write as _;
    let Some(parent) = parent else {
        return Ok(());
    };
    let calls = contract_calls(block);
    if calls.is_empty() {
        return Ok(());
    }
    let repeats: usize = std::env::var("NANO_BENCH_REPEATS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3);
    let mut sink = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(samples)?;
    for call in calls {
        if ask_engine_before_sealing(chainstate, parent, &call, false, false).is_none() {
            return Ok(());
        }
        let mut agree = true;
        let mut runtime = 0;
        let (mut compiled, mut interpreted) = (Vec::new(), Vec::new());
        for _ in 0..repeats {
            let Some((wasm_answer, cost, took)) =
                ask_engine_before_sealing(chainstate, parent, &call, false, false)
            else {
                return Ok(());
            };
            runtime = cost.runtime;
            compiled.push(took);
            let Some((interpreter_answer, _, took)) =
                ask_engine_before_sealing(chainstate, parent, &call, true, false)
            else {
                return Ok(());
            };
            interpreted.push(took);
            agree &= wasm_answer == interpreter_answer;
        }
        let nanos = |timings: &[std::time::Duration]| {
            timings
                .iter()
                .map(|duration| duration.as_nanos().to_string())
                .collect::<Vec<_>>()
                .join(",")
        };
        writeln!(
            sink,
            "{}\t{}\t{}\t{}\t{runtime}\t{}\t{}\t{agree}",
            block.header.chain_length,
            call.txid,
            call.contract,
            call.function,
            nanos(&compiled),
            nanos(&interpreted),
        )?;
    }
    Ok(())
}

fn apply_captured_block(
    capture: &ReplayInputs<'_>,
    chainstate: &mut ChainState,
    parent: Option<[u8; 32]>,
    bitcoin_view: &mut String,
    path: &Path,
    prepare: &mut dyn FnMut(&mut NakamotoBlock),
    execute: ExecuteBlock,
) -> Result<(NakamotoBlock, nano_chainstate::AppliedBlock, Option<String>), ReplayDivergence> {
    let bytes =
        fs::read(path).map_err(|_| ReplayDivergence::Fixture("block cannot be read".to_owned()))?;
    let mut block = NakamotoBlock::decode(&bytes)
        .map_err(|_| ReplayDivergence::Fixture("block cannot be decoded".to_owned()))?;
    prepare(&mut block);
    // A tenure extend moves the Clarity burn view without starting a tenure, so
    // the view carries forward until the next tenure change moves it again.
    if let Some(view) = block.bitcoin_view_consensus_hash() {
        *bitcoin_view = view.to_string();
    } else if bitcoin_view.is_empty() {
        // Replay can start mid-tenure, where the view is the tenure's own sortition.
        *bitcoin_view = block.header.consensus_hash.to_string();
    }
    // The view's snapshot carries everything Clarity reads. The tenure's carries the
    // one thing the prepare-phase rule reads, and they are the same snapshot unless
    // this block extends its tenure past the burn block that elected it.
    let mut bitcoin_context = *capture
        .snapshots
        .get(bitcoin_view.as_str())
        .ok_or_else(|| {
            ReplayDivergence::Fixture(
                "block Bitcoin view is absent from captured Bitcoin snapshots".to_owned(),
            )
        })?;
    if let Some(tenure) = capture
        .snapshots
        .get(block.header.consensus_hash.to_string().as_str())
    {
        let view = bitcoin_context.height;
        bitcoin_context.move_to_burn_block(tenure.height);
        bitcoin_context.extend_view_to(view);
    }
    let event_path = capture.root.join("events/new_block").join(
        path.file_stem()
            .map(|name| format!("{}.json", name.to_string_lossy()))
            .ok_or_else(|| ReplayDivergence::Fixture("block has no file name".to_owned()))?,
    );
    // A capture without receipts still needs the unlock heights the events
    // otherwise carry. They are constants of the chain rather than of a block,
    // so the provenance record holds them.
    let event = if capture.receipts {
        let event: CapturedBlockEvent = serde_json::from_slice(
            &fs::read(event_path)
                .map_err(|_| ReplayDivergence::Fixture("block event cannot be read".to_owned()))?,
        )
        .map_err(|_| ReplayDivergence::Fixture("block event cannot be decoded".to_owned()))?;
        bitcoin_context.v1_unlock_height = event.v1;
        bitcoin_context.v2_unlock_height = event.v2;
        bitcoin_context.v3_unlock_height = event.v3;
        bitcoin_context.pox_5_activation_height = event.v4;
        Some(event)
    } else {
        let height = |name: &str| {
            provenance_field(capture.root, name)
                .and_then(|value| value.trim().parse::<u32>().ok())
                .ok_or_else(|| {
                    ReplayDivergence::Fixture(format!(
                        "a capture without receipts needs {name} in its provenance"
                    ))
                })
        };
        bitcoin_context.v1_unlock_height = height("pox_v1_unlock_height")?;
        bitcoin_context.v2_unlock_height = height("pox_v2_unlock_height")?;
        bitcoin_context.v3_unlock_height = height("pox_v3_unlock_height")?;
        bitcoin_context.pox_5_activation_height = height("pox_v4_unlock_height")?;
        None
    };
    let operations = capture
        .bitcoin_operations
        .get(&block.header.consensus_hash.to_string())
        .ok_or_else(|| {
            ReplayDivergence::Fixture(
                "block consensus hash is absent from captured Bitcoin operations".to_owned(),
            )
        })?;
    if crosschecking_engines()
        && let Some(disagreement) = engines_disagree_before_sealing(chainstate, parent, &block)
    {
        return Err(ReplayDivergence::Engine(disagreement));
    }
    if let Some(samples) = benching_engines()
        && let Err(error) = bench_engines_before_sealing(chainstate, parent, &block, &samples)
    {
        return Err(ReplayDivergence::Fixture(format!(
            "the engine bench cannot write its samples: {error}"
        )));
    }
    let applied = execute(chainstate, bitcoin_context, operations, parent, &block)
        .map_err(|error| ReplayDivergence::Application(error.to_string()))?;
    let cost_divergence = match &event {
        Some(event) => compare_receipts(event, &applied.receipts)?,
        None => None,
    };
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
            Self::Application(message)
            | Self::Fixture(message)
            | Self::Receipt(message)
            | Self::Engine(message) => formatter.write_str(message),
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
            value.strip_prefix("0x").map_or_else(
                || value.parse().ok(),
                |hex| u32::from_str_radix(hex, 16).ok(),
            )
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
        serde_json::from_slice(&fs::read(path).expect("read reward set"))
            .expect("decode reward set");
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

/// Where the captured chain deployed the sBTC registry `.pox-5` reads.
///
/// A deployment rather than a boot contract, so it is a fact about the capture in
/// the same way its chain identifier and burnchain magic are, and a node is told
/// it by configuration. The captured chain's is not where stacks-core defaults a
/// testnet to.
#[must_use]
pub fn captured_sbtc_registry(root: &Path) -> Option<String> {
    provenance_field(root, "sbtc_registry_contract")
}

/// The burnchain magic the captured chain prefixes its `OP_RETURN`s with.
fn captured_magic(root: &Path) -> [u8; 2] {
    provenance_field(root, "bitcoin_magic")
        .and_then(|value| value.as_bytes().try_into().ok())
        .unwrap_or(*b"T3")
}

#[must_use]
pub fn captured_bitcoin_snapshots(root: &Path) -> Option<BTreeMap<String, BitcoinBlockContext>> {
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
            // How many of a commitment's outputs are payouts, and the block's whole
            // payout burn, both stated by the archive. This used to sum *every*
            // output of every commitment, change included — which on mainnet makes
            // a commitment of 30,000 sats read as the 16-23 million behind it, the
            // same trap `nano_sortition::PayoutSchedule` exists to avoid one layer
            // down.
            let (payout_outputs, burn_spend_total) = captured_pox_payouts(&snapshot.pox_payouts)?;
            let burn = |operation: &BitcoinOperation| -> u128 {
                operation
                    .outputs
                    .iter()
                    .take(payout_outputs)
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
            Some((snapshot.consensus_hash.clone(), {
                // Through `at_height`, so this snapshot's burn block is both the
                // tenure and the view. A replayed block carrying an extend moves
                // the view on afterwards, in `execute_captured_block`.
                let mut context = BitcoinBlockContext::at_height(snapshot.block_height);
                context.first_height = first_height;
                context.prepare_phase_length = prepare_phase_length;
                context.reward_phase_length = reward_phase_length;
                context.burn_header_hash = decode_hash(&snapshot.burn_header_hash)?;
                context.burn_block_time = snapshot.burn_header_timestamp;
                context.vrf_seed = vrf_seed;
                context.burn_spend_total = burn_spend_total;
                context.burn_spend_winner = burn_spend_winner;
                // Validation only, and absent from a capture written before it
                // was recorded: zero then, which reads as "cannot check" at the
                // rule rather than as a wrong answer.
                context.sortition_hash = decode_hash(&snapshot.sortition_hash).unwrap_or_default();
                context
            }))
        })
        .collect()
}

#[must_use]
pub fn captured_bitcoin_operations(root: &Path) -> Option<BTreeMap<String, Vec<BitcoinOperation>>> {
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

/// The block that sealed the checkpoint, when the capture kept it.
///
/// Absent from every capture taken before `capture-fixtures` started keeping it,
/// and a capture is not invalid for that — a node fetches the block from a peer.
/// What it costs is that the attestation cannot be checked offline against the
/// state a node would actually import.
#[must_use]
pub fn captured_checkpoint_block(root: &Path) -> Option<NakamotoBlock> {
    let bytes = fs::read(
        root.join("chainstate/checkpoint-H")
            .join(nano_marf::CHECKPOINT_BLOCK_FILE),
    )
    .ok()?;
    NakamotoBlock::decode(&bytes).ok()
}

#[must_use]
pub fn checkpoint_state(root: &Path) -> Option<([u8; 32], TrieHash)> {
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
        CHECKPOINT_HISTORY_LIMIT, ChainState, FixtureManifest, FixtureMode, FixtureValidationError,
        apply_captured_block, baseline_replay, captured_accounting, captured_bitcoin_operations,
        captured_bitcoin_snapshots, captured_chainstate, captured_checkpoint_block, captured_magic,
        captured_network, captured_signer_set, captured_signer_sets, checkpoint_manifest,
        checkpoint_state, decode_hash, scoreboard, validate_checkpoint_authentication_history,
        validate_fixture_tree, validate_sortition_seed,
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
    use nano_sortition::BURN_BLOCK_MINED_AT_MODULUS;
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

        fn block_hash_at(&self, _height: u64) -> Result<[u8; 32], Self::Error> {
            unimplemented!("this source is only asked for blocks")
        }

        fn tip_height(&self) -> Result<u64, Self::Error> {
            self.blocks
                .keys()
                .next_back()
                .copied()
                .ok_or_else(|| "this source holds no Bitcoin blocks".to_owned())
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
                receipts: true,
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
            receipts: true,
        });
        assert!(report.contains("0/1"));
        assert!(report.contains("block 1"));
    }

    /// Replay depth is the deepest block a ledger names, not the deepest seal.
    ///
    /// The two part on a real state: a live mainnet node held seals to 8,713,522
    /// against a ledger naming 8,713,221, and reporting the seal would have claimed
    /// 301 blocks of depth that no restart could stand on. The north-star metric is
    /// the one number the whole plan is read through, so overstating it is worse
    /// than most divergences it is supposed to find.
    #[test]
    fn replay_depth_is_the_deepest_committed_block_not_the_deepest_seal() {
        let directory = tempfile::tempdir().expect("a directory");
        let state = directory.path();
        fs::create_dir_all(state.join("chainstate")).expect("a chainstate directory");
        {
            let mut vm =
                nano_vm::Vm::open(nano_primitives::Network::MAINNET, state.join("chainstate"))
                    .expect("open");
            let mut parent = None;
            for height in 1..=3u8 {
                vm.begin_block(parent, [height; 32]).expect("begin");
                vm.commit_block(
                    [height; 32],
                    &nano_vm::BlockCommit {
                        header: nano_vm::BlockHeader::default(),
                        ledger: b"committed".to_vec(),
                    },
                )
                .expect("commit");
                parent = Some([height; 32]);
            }
            // Two seals above it that no ledger names.
            for height in 4..=5u8 {
                vm.begin_block(parent, [height; 32]).expect("begin");
                vm.seal_block_to([height; 32]).expect("seal");
                parent = Some([height; 32]);
            }
        }

        let (anchor, tip) = super::mainnet_executed_height(state).expect("a height");
        assert_eq!(
            anchor, tip,
            "no anchor is set, so the row reports the tip alone"
        );
        let marf = nano_marf::VersionedMarf::open(state.join("chainstate/marf.sqlite"))
            .expect("open the marf");
        let sealed = marf
            .height(marf.tip().expect("read the tip").expect("a tip"))
            .expect("read the height")
            .expect("a height");
        assert_eq!(
            u64::from(sealed) - tip,
            2,
            "the deepest seal is two blocks above the deepest ledger, and depth is the ledger's"
        );
    }

    /// Corrupt one expected receipt and the board must go red *and* say so.
    ///
    /// The gate this pins is not the table -- it is the second half of
    /// `scoreboard_result`. `cargo xtask scoreboard` printed a divergence at block 76
    /// and exited **zero**, because loading the manifest had succeeded, so every
    /// caller that reads an exit status was told the replay passed. A gate that
    /// reports failure only in prose is not a gate.
    ///
    /// Done by tampering rather than by constructing a `ReplayDepth`: a test that
    /// builds the failing value itself proves the boolean and not the replay.
    #[test]
    fn a_tampered_expectation_makes_the_board_red_and_the_command_fail() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let manifest = FixtureManifest::load(&root.join("manifest.toml")).expect("manifest");
        let (_, passed) = super::scoreboard_result(&root, manifest);
        assert!(
            passed,
            "the checked-in capture has to pass before tampering means anything"
        );

        let directory = tempfile::tempdir().expect("a directory");
        let copy = directory.path().join("fixtures");
        copy_tree(&root, &copy).expect("copy the fixture tree");

        // One receipt, one field: the status of the first transaction of one block.
        // Whichever event file holds it, the replay compares against it.
        let events = copy.join("events/new_block");
        let mut tampered = false;
        let mut entries: Vec<_> = fs::read_dir(&events)
            .expect("the capture has new_block events")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        entries.sort();
        for path in entries {
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            // A receipt's `status`, which is the field the replay compares first and
            // the one a compiler regression moves: `success` became
            // `abort_by_response` at block 76 on 2026-08-07.
            let marker = "\"status\":\"success\"";
            if let Some(at) = text.find(marker) {
                let mut changed = text.clone();
                changed.replace_range(at..at + marker.len(), "\"status\":\"abort_by_response\"");
                fs::write(&path, changed).expect("write the tampered receipt");
                tampered = true;
                break;
            }
        }
        assert!(
            tampered,
            "no receipt in the capture states a successful status to tamper with"
        );

        let manifest = FixtureManifest::load(&copy.join("manifest.toml")).expect("manifest");
        let (board, passed) = super::scoreboard_result(&copy, manifest);
        assert!(
            !passed,
            "a tampered receipt has to fail the command, not only appear in the table:\n{board}"
        );
    }

    /// A plain recursive copy, so the tamper happens somewhere the tree is not.
    fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            let target = to.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                copy_tree(&entry.path(), &target)?;
            } else {
                fs::copy(entry.path(), target)?;
            }
        }
        Ok(())
    }

    /// The one measured point behind 082's anchor rule, from mainnet's own data.
    ///
    /// A locally derived sortition chain cannot cross a reward cycle boundary without
    /// knowing whether the opening cycle selected a `PoX` anchor block: the consensus
    /// hash mixes the `PoX` history, so a guessed bit derives a wrong hash for every
    /// block after it. The rule the node uses is that a cycle it recorded a *signer
    /// set* for selected an anchor — the anchor block is what carries the set.
    ///
    /// That rule was reasoned rather than measured, and every live source that could
    /// have settled it was unavailable: the captured fixture's state cannot answer
    /// `get-signers` for its opening cycle, the hacknet rig's chain is down, and the
    /// mainnet state that holds both facts together was corrupted. What survives is
    /// this, on disk and offline: mainnet's cycle 140 has a signer set, and mainnet's
    /// `PoX` history is 142 bits with **no** zero in it — so the bit for a cycle with
    /// a recorded set is 1, which is the direction the rule claims.
    ///
    /// Mainnet's `PoX` history at the checkpoint, as every run of the follower printed
    /// it: 142 cycles, none of which failed to select an anchor.
    ///
    /// One confirming point and not a proof: nothing here exhibits a cycle whose bit
    /// is 0, because mainnet has never had one. The converse stays unmeasured, and
    /// the node's decider is built so that only a *positively* recorded set decides
    /// anything — an unmeasured converse can therefore leave a boundary uncrossed,
    /// but cannot produce a wrong hash.
    const MAINNET_POX_HISTORY_AT_THE_CHECKPOINT: &str = "1111111111111111111111111111111111111111111111111111111111111111111111\
         111111111111111111111111111111111111111111111111111111111111111111111111";

    #[test]
    fn a_mainnet_cycle_with_a_signer_set_has_its_pox_anchor_bit_set() {
        #[derive(Deserialize)]
        struct Document {
            stacker_set: Set,
        }
        #[derive(Deserialize)]
        struct Set {
            signers: Vec<serde_json::Value>,
        }
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/mainnet/stacker_set-140.json");
        let document: Document =
            serde_json::from_slice(&fs::read(&path).expect("the cycle 140 set"))
                .expect("a reward set document");
        assert!(
            !document.stacker_set.signers.is_empty(),
            "mainnet cycle 140 has a recorded signer set, which is the half of the rule \
             that says an anchor was selected"
        );

        // The other half, from the checkpoint this capture was taken at. Its
        // sortition identifier is the burn header hash and the `PoX` history hashed
        // together, so the history is recoverable from it and is not a claim the
        // capture makes about itself.
        let bits: String = MAINNET_POX_HISTORY_AT_THE_CHECKPOINT
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        assert_eq!(
            bits.len(),
            142,
            "mainnet had 142 reward cycles at the checkpoint"
        );
        assert!(
            !bits.contains('0'),
            "a cycle that selected no anchor would be the case this rule cannot yet decide"
        );
    }

    struct AuthenticationFixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        source: [u8; 32],
        state_root: TrieHash,
        disconnected: Vec<u8>,
    }

    fn authentication_fixture() -> AuthenticationFixture {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let blocks = captured_block_paths(&fixture)
            .into_iter()
            .map(|path| {
                let raw = fs::read(path).expect("read captured block");
                let block = NanoNakamotoBlock::decode(&raw).expect("decode captured block");
                (raw, block)
            })
            .collect::<Vec<_>>();
        let starts = blocks
            .iter()
            .enumerate()
            .filter(|(_, (_, block))| nano_chainstate::starts_new_tenure(block))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert!(starts.len() >= 2, "capture contains two tenure starts");
        let boundary_index = starts[0];
        let first_index = starts[1];
        let source_index = starts.get(2).map_or(blocks.len() - 1, |index| *index - 1);
        let directory = tempfile::tempdir().expect("temporary authentication history");
        let root = directory.path().join("authentication-history");
        fs::create_dir_all(root.join("blocks")).expect("history blocks directory");
        let boundary = &blocks[boundary_index].1;
        fs::write(
            root.join("boundary.json"),
            serde_json::to_vec(&serde_json::json!({
                "parent_tenure_consensus_hash": hex::encode(boundary.header.consensus_hash.as_bytes()),
                "coinbase_vrf_proof": hex::encode(
                    nano_chainstate::coinbase_vrf_proof(boundary).expect("boundary proof")
                ),
            }))
            .expect("boundary JSON"),
        )
        .expect("write boundary");
        for (raw, block) in &blocks[first_index..=source_index] {
            fs::write(
                root.join("blocks").join(format!(
                    "{:08}-{}.bin",
                    block.header.chain_length,
                    hex::encode(block.block_id().as_bytes())
                )),
                raw,
            )
            .expect("write history block");
        }
        let source = &blocks[source_index].1;
        AuthenticationFixture {
            _directory: directory,
            root,
            source: *source.block_id().as_bytes(),
            state_root: source.header.state_index_root,
            disconnected: blocks[boundary_index].0.clone(),
        }
    }

    #[test]
    fn a_complete_checkpoint_authentication_history_is_valid() {
        let fixture = authentication_fixture();
        validate_checkpoint_authentication_history(
            &fixture.root,
            fixture.source,
            fixture.state_root,
        )
        .expect("valid authentication history");
    }

    #[test]
    fn a_missing_checkpoint_authentication_history_is_rejected() {
        let directory = tempfile::tempdir().expect("temporary fixture");
        let error = validate_checkpoint_authentication_history(
            &directory.path().join("missing"),
            [0; 32],
            TrieHash::from_bytes([0; 32]),
        )
        .expect_err("missing authentication history");
        assert!(
            matches!(
                error,
                FixtureValidationError::InvalidCheckpointHistory { .. }
            ),
            "{error}"
        );
    }

    #[test]
    fn a_disconnected_checkpoint_authentication_block_is_rejected() {
        let fixture = authentication_fixture();
        fs::write(
            fixture.root.join("blocks/disconnected.bin"),
            fixture.disconnected,
        )
        .expect("write disconnected block");
        let error = validate_checkpoint_authentication_history(
            &fixture.root,
            fixture.source,
            fixture.state_root,
        )
        .expect_err("disconnected authentication block");
        assert!(error.to_string().contains("disconnected"), "{error}");
    }

    #[test]
    fn an_oversized_checkpoint_authentication_history_is_rejected() {
        let fixture = authentication_fixture();
        let blocks = fixture.root.join("blocks");
        let present = fs::read_dir(&blocks).expect("history blocks").count();
        for index in present..=CHECKPOINT_HISTORY_LIMIT {
            fs::write(blocks.join(format!("padding-{index:04}.bin")), []).expect("write padding");
        }
        let error = validate_checkpoint_authentication_history(
            &fixture.root,
            fixture.source,
            fixture.state_root,
        )
        .expect_err("oversized authentication history");
        assert!(error.to_string().contains("bounded limit"), "{error}");
    }

    #[test]
    fn checkpoint_authentication_history_must_end_at_the_published_source_and_root() {
        let fixture = authentication_fixture();
        let source_error = validate_checkpoint_authentication_history(
            &fixture.root,
            [0xff; 32],
            fixture.state_root,
        )
        .expect_err("wrong source");
        assert!(
            source_error.to_string().contains("no checkpoint source"),
            "{source_error}"
        );
        let root_error = validate_checkpoint_authentication_history(
            &fixture.root,
            fixture.source,
            TrieHash::from_bytes([0xff; 32]),
        )
        .expect_err("wrong root");
        assert!(
            root_error.to_string().contains("publishes state root"),
            "{root_error}"
        );
    }

    #[test]
    fn checked_in_capture_is_explicitly_execution_only_until_recaptured() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let error = validate_fixture_tree(&root).expect_err(
            "a capture without the checkpoint authentication suffix cannot qualify a release",
        );
        assert!(
            matches!(
                error,
                FixtureValidationError::InvalidCheckpointHistory { .. }
            ),
            "{error}"
        );
        assert!(fs::metadata(root.join("README.md")).is_ok());
    }

    /// The captured corpus is recaptured wholesale, so tests address its blocks
    /// by position rather than by the name of any one capture.
    use super::captured_block_paths;

    /// Every contract a checkpoint carries has to be findable in the trie it
    /// imported, by the key Clarity looks it up with.
    ///
    /// A matching root proves the trie has the right shape, not that every key
    /// written before the checkpoint can still be walked to. Those are
    /// different claims, and the second is the one execution depends on:
    /// Clarity reads a contract through `clarity-contract::<id>`, its analysis
    /// loader swallows a failed read with `.ok()`, and the caller is told the
    /// contract is unresolved. Against mainnet that stopped execution at a
    /// deployment referencing a contract from long before the checkpoint.
    #[test]
    fn every_checkpointed_contract_is_reachable_in_the_imported_trie() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let (source, root) = checkpoint_state(&fixture).expect("checkpoint metadata");
        let checkpoint = fixture.join("chainstate/checkpoint-H/marf.sqlite");
        let contracts = checkpointed_contracts(&checkpoint);
        assert!(
            !contracts.is_empty(),
            "the checkpoint carries contracts to look for"
        );

        let imported = import_checkpoint(&checkpoint, source, root).expect("imports checkpoint");
        let missing: Vec<&String> = contracts
            .iter()
            .filter(|contract| {
                imported
                    .get(source, format!("clarity-contract::{contract}").as_bytes())
                    .expect("the imported trie reads")
                    .is_none()
            })
            .collect();

        assert!(
            missing.is_empty(),
            "{} of {} checkpointed contracts cannot be reached: {:?}",
            missing.len(),
            contracts.len(),
            missing.iter().take(5).collect::<Vec<_>>()
        );
    }

    /// The contracts a checkpoint's side store says it holds an analysis for.
    fn checkpointed_contracts(checkpoint: &Path) -> Vec<String> {
        let connection = rusqlite::Connection::open_with_flags(
            checkpoint,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open the checkpoint");
        let mut statement = connection
            .prepare(
                "SELECT DISTINCT key FROM metadata_table WHERE key LIKE 'clr-meta::%::analysis'",
            )
            .expect("query the checkpoint");
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("read the checkpoint");
        rows.filter_map(|key| {
            let key = key.ok()?;
            Some(
                key.strip_prefix("clr-meta::")?
                    .strip_suffix("::analysis")?
                    .to_owned(),
            )
        })
        .collect()
    }

    #[test]
    fn checkpoint_graph_import_matches_the_published_root() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let (source, root) = checkpoint_state(&fixture).expect("checkpoint metadata");
        let checkpoint = fixture.join("chainstate/checkpoint-H/marf.sqlite");
        let imported = import_checkpoint(checkpoint, source, root).expect("imports checkpoint");
        assert_eq!(
            imported.root(source).expect("read imported root"),
            Some(root)
        );
    }

    /// A checkpoint is trusted because signers signed its root, not because it
    /// says so.
    ///
    /// `signer_signature_hash` covers `state_index_root`, so a header the
    /// reward set put threshold weight behind states what the state root at
    /// that height was.
    ///
    /// A capture that carries the checkpoint's own block is attested against it,
    /// which is what a node does. A capture taken before the tool kept that
    /// block has only the ones after it, so the first of those stands in as a
    /// checkpoint of its own: the mechanism is identical, the block is not the
    /// one a node would adopt. `mainnet_envelope` attests a real published
    /// checkpoint against the block that sealed it.
    #[test]
    fn a_signed_header_attests_the_checkpoint_it_sealed() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let signers = captured_signer_set(&fixture);
        let published = checkpoint_manifest(&fixture).expect("checkpoint manifest");
        let (block, manifest) = if let Some(block) = captured_checkpoint_block(&fixture) {
            (block, published)
        } else {
            let block = NanoNakamotoBlock::decode(
                &fs::read(
                    captured_block_paths(&fixture)
                        .first()
                        .expect("captured block"),
                )
                .expect("read block"),
            )
            .expect("decode block");
            let manifest = nano_marf::CheckpointManifest {
                stacks_height: block.header.chain_length,
                source_state_id: *block.block_id().as_bytes(),
                state_index_root: block.header.state_index_root,
                ..published
            };
            (block, manifest)
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

        // A checkpoint claiming a height the signed header is not at. The root
        // and the state id would still agree, so nothing but this check stands
        // between an operator and a state imported at the wrong height.
        let wrong_height = nano_marf::CheckpointManifest {
            stacks_height: manifest.stacks_height + 1,
            ..manifest.clone()
        };
        assert!(
            matches!(
                attest_checkpoint(&wrong_height, &block.header, &signers),
                Err(CheckpointTrustError::Height { .. })
            ),
            "a checkpoint at a height the signed header is not at was accepted"
        );

        // A checkpoint naming a state the signed header does not identify. This
        // is the one an operator is most likely to get wrong by copying a
        // configuration, and the one that silently builds on someone else's
        // chain: the trie would import, the root would match, and every block
        // after it would be computed against the wrong ancestry.
        let mut wrong_state = manifest.source_state_id;
        wrong_state[0] ^= 0x01;
        let wrong_state = nano_marf::CheckpointManifest {
            source_state_id: wrong_state,
            ..manifest.clone()
        };
        assert!(
            matches!(
                attest_checkpoint(&wrong_state, &block.header, &signers),
                Err(CheckpointTrustError::StateId { .. })
            ),
            "a checkpoint naming a state the signed header does not identify was accepted"
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
            .execute_unauthenticated_fixture_block_with_bitcoin_operations(
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
                (&first_context, &first.header.consensus_hash),
                (&second_context, &second.header.consensus_hash),
            ]
            .into_iter()
            .map(|(context, consensus_hash)| {
                (
                    context.height,
                    BitcoinBlock {
                        height: context.height,
                        hash: [0; 32],
                        timestamp: context.burn_block_time,
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
            .append_unauthenticated_fixture_block_with_bitcoin_operations(
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

    /// The burn block a proposal is validated under has to be the proposal's own.
    ///
    /// `ChainstateProposalValidator` refreshed only `height` and the accumulated
    /// coinbase per proposal, so `sortition_hash`, `vrf_seed` and the winning
    /// miner's registered keys stayed at whatever the validator was *constructed*
    /// with -- the checkpoint anchor's, for the life of the process. A tenure-start
    /// proposal was therefore checked against the anchor's committed seed and
    /// rejected as `committed seed is not the hash of the parent tenure's VRF
    /// proof`: the node telling a stock signer that a perfectly good block was
    /// invalid, which is what the hosted-signer run measured.
    ///
    /// Two halves, and the first is what makes the second mean anything: the
    /// captured burn blocks have to disagree about the seed at all, or a validator
    /// carrying the wrong one would look correct.
    #[test]
    fn a_proposal_is_validated_under_its_own_burn_block() {
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
        assert_ne!(
            (first_context.vrf_seed, first_context.sortition_hash),
            (second_context.vrf_seed, second_context.sortition_hash),
            "two burn blocks that agree about the seed cannot show which one was used"
        );

        let mut chainstate = captured_chainstate(&fixture);
        let mut bitcoin = FixtureBitcoinSource {
            blocks: [
                (&first_context, &first.header.consensus_hash),
                (&second_context, &second.header.consensus_hash),
            ]
            .into_iter()
            .map(|(context, consensus_hash)| {
                (
                    context.height,
                    BitcoinBlock {
                        height: context.height,
                        hash: [0; 32],
                        timestamp: context.burn_block_time,
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
            .append_unauthenticated_fixture_block_with_bitcoin_operations(
                first_context,
                &first_operations.operations,
                Some(source),
                &first,
            )
            .expect("apply anchor block");
        let mut validator =
            ChainstateProposalValidator::new(chainstate, &first, first_context, bitcoin);

        // As built: the anchor's burn block, whatever height a proposal names.
        assert_eq!(
            validator.bitcoin_context().vrf_seed,
            first_context.vrf_seed,
            "the standing context is the one the validator was constructed with"
        );

        // What the hosted validator does before every proposal, out of its own
        // derived sortition chain rather than out of the peer that served it.
        validator.set_bitcoin_context(second_context);
        assert_eq!(
            validator.bitcoin_context().vrf_seed,
            second_context.vrf_seed,
            "and the proposal's own burn block is what replaces it"
        );
        assert_eq!(
            validator.bitcoin_context().sortition_hash,
            second_context.sortition_hash,
            "including the sortition hash the coinbase proof is over"
        );

        validator
            .validate(&BlockProposal {
                block: second,
                bitcoin_height: second_context.height,
                reward_cycle: 0,
                data: BlockProposal::empty_data(),
            })
            .expect("the proposal still executes to the state root it commits to");
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
                (&first_context, &first.header.consensus_hash),
                (&second_context, &second.header.consensus_hash),
            ]
            .into_iter()
            .map(|(context, consensus_hash)| {
                (
                    context.height,
                    BitcoinBlock {
                        height: context.height,
                        hash: [0; 32],
                        timestamp: context.burn_block_time,
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
                &super::ReplayInputs {
                    root: &fixture,
                    snapshots: &snapshots,
                    bitcoin_operations: &bitcoin_operations,
                    receipts: true,
                },
                &mut chainstate,
                parent,
                &mut bitcoin_view,
                &path,
                &mut |_| {},
                ChainState::execute_unauthenticated_fixture_block_with_bitcoin_operations,
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
        let expected_leaves = expected
            .leaves(block_id)
            .expect("read expected leaves")
            .expect("expected leaves");
        let actual_leaves = chainstate
            .state_leaves(block_id)
            .expect("read actual leaves")
            .expect("actual leaves");
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
        assert!(imported.root(source).expect("read imported root").is_some());
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

        // One contract per block, so every block moves the state root, and
        // through the engine the node actually runs.
        let programs = [
            ("first", "(define-data-var counter uint u1)"),
            ("second", "(define-data-var counter uint u2)"),
            ("third", "(define-data-var counter uint u3)"),
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
        for (block, (name, source)) in blocks.iter().zip(programs) {
            open.begin_block(Some(parent), *block)?;
            open.deploy_contract(
                deployed_contract(name),
                clarity::vm::ClarityVersion::Clarity6,
                source,
                LimitedCostTracker::new_free(),
            )
            .expect("deploy in block");
            expected.push(open.seal_block()?);
            parent = *block;
        }
        drop(open);

        let directory = temporary.join("reopened");
        let mut parent = source;
        let mut reopened_roots = Vec::new();
        for (block, (name, contract_source)) in blocks.iter().zip(programs) {
            let mut reopened =
                nano_vm::Vm::open_from_checkpoint(network, &directory, &checkpoint, source, root)?;
            assert_eq!(
                reopened.tip().expect("read reopened tip"),
                Some(parent),
                "resumes from the tip on disk"
            );
            reopened.begin_block(Some(parent), *block)?;
            reopened
                .deploy_contract(
                    deployed_contract(name),
                    clarity::vm::ClarityVersion::Clarity6,
                    contract_source,
                    LimitedCostTracker::new_free(),
                )
                .expect("deploy in block");
            reopened_roots.push(reopened.seal_block()?);
            parent = *block;
        }

        assert_eq!(reopened_roots, expected);
        fs::remove_dir_all(temporary)?;
        Ok(())
    }

    /// The block and tenure reads a contract on the chain would make, as a
    /// contract, because that is the only way the node answers them.
    const READER: &str = "
(define-read-only (block-header-hash (h uint)) (get-stacks-block-info? header-hash h))
(define-read-only (block-time (h uint)) (get-stacks-block-info? time h))
(define-read-only (tenure-burn-hash (h uint)) (get-tenure-info? burnchain-header-hash h))
(define-read-only (tenure-time (h uint)) (get-tenure-info? time h))
(define-read-only (tenure-vrf-seed (h uint)) (get-tenure-info? vrf-seed h))
(define-read-only (tenure-spend-winner (h uint)) (get-tenure-info? miner-spend-winner h))
(define-read-only (tenure-spend-total (h uint)) (get-tenure-info? miner-spend-total h))
";

    /// A contract identifier under a fixed test principal.
    fn deployed_contract(name: &str) -> clarity::vm::types::QualifiedContractIdentifier {
        clarity::vm::types::QualifiedContractIdentifier::parse(&format!(
            "ST000000000000000000002AMW42H.{name}"
        ))
        .expect("a contract identifier")
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
                .append_unauthenticated_fixture_block_with_bitcoin_operations(
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
            timestamp: 0,
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
                .append_unauthenticated_fixture_block_with_bitcoin_operations(
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
        while let Some(parent) = chainstate.parent_of(block).expect("read block parent") {
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
        // By signing-key hash, which is the shape `.signers` records and the one
        // both selection and execution weigh against.
        let signers = captured_signer_set(&fixture)
            .signing_weights()
            .expect("the captured reward set is well formed");
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

        let tip =
            |peer: usize, header: nano_chainstate::NakamotoBlockHeader| nano_sync::CandidateTip {
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
            };
        let candidates = vec![tip(0, forged), tip(1, signed.header.clone())];

        let chosen = nano_sync::choose_canonical_tip(&candidates, Some(&signers), None)
            .expect("a signed candidate is available");
        assert_eq!(chosen.peer, 1, "the signed tip wins despite being shorter");

        // With nothing signed, there is no canonical tip to follow at all.
        assert!(
            nano_sync::choose_canonical_tip(&candidates[..1], Some(&signers), None).is_none(),
            "an unsigned chain is not a chain to follow"
        );
    }

    /// Two peers offering equally long signed tips resolve the same way
    /// everywhere, or nodes following the same peers would split.
    #[test]
    fn an_exact_tie_in_fork_choice_resolves_deterministically() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let signers = captured_signer_set(&fixture)
            .signing_weights()
            .expect("the captured reward set is well formed");
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
            nano_sync::choose_canonical_tip(&forwards, Some(&signers), None)
                .map(|tip| tip.header.block_id()),
            nano_sync::choose_canonical_tip(&backwards, Some(&signers), None)
                .map(|tip| tip.header.block_id()),
            "the order the peers answered in must not decide the tip"
        );
    }

    /// A burn view whose height this test states, so a tie can be placed.
    struct StatedBurnView(std::collections::BTreeMap<String, u64>);

    impl nano_sync::BurnView for StatedBurnView {
        fn derived(&self, _: nano_primitives::ConsensusHash, _: u64) -> Option<bool> {
            // Nothing about *this* test: the tie-break is what is under test, and a
            // rejection would decide the question before it was asked.
            None
        }

        fn height_of(&self, consensus_hash: nano_primitives::ConsensusHash) -> Option<u64> {
            self.0.get(&consensus_hash.to_string()).copied()
        }
    }

    /// The tie between two equally high tips goes the way stacks-core's goes.
    ///
    /// Transcribed from `SortitionDB::set_stacks_block_accepted_at_tip`, which is
    /// the only place in stacks-core that decides it:
    ///
    /// ```text
    /// if cur_height < stacks_block_height  -> replace
    /// else if cur_height > stacks_block_height -> keep
    /// else if cur_ch == consensus_hash     -> keep      // same tenure
    /// else  // "break ties by going with the latter-signed block"
    ///   replace iff sn_current.block_height < sn_accepted.block_height
    /// ```
    ///
    /// So the tie-break is the burn height of each tip's **own sortition**, later
    /// wins. Nano compared block identifiers, which is deterministic and *not this
    /// rule* — two nodes could stand on different tips of the same length, each
    /// behaving as designed. That is what this task suspected and what this pins.
    ///
    /// It is transcribed rather than called, and the reason is worth being exact
    /// about: the rule lives inside a `&mut SortitionHandleTx` method that reads two
    /// snapshots out of a sortition database and writes the winner back. There is no
    /// pure function to hand two headers to. What *is* checked against stacks-core
    /// here is the input the rule consumes — the burn height of a consensus hash,
    /// which `mainnet_sortition` already asserts nano derives identically for every
    /// block of the captured mainnet window.
    #[test]
    fn the_fork_choice_tie_break_is_the_later_sortition() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let signers = captured_signer_set(&fixture)
            .signing_weights()
            .expect("the captured reward set is well formed");
        let blocks: Vec<NanoNakamotoBlock> = captured_block_paths(&fixture)
            .into_iter()
            .map(|path| {
                NanoNakamotoBlock::decode(&fs::read(&path).expect("read block"))
                    .expect("decode block")
            })
            .filter(|block| nano_sync::weigh_tip(&block.header, &signers).is_ok())
            .collect();
        // Two tips of equal length in *different* tenures, which is the only branch
        // of the rule that compares anything.
        let mut tenures: Vec<&NanoNakamotoBlock> = Vec::new();
        for block in &blocks {
            if !tenures
                .iter()
                .any(|kept| kept.header.consensus_hash == block.header.consensus_hash)
            {
                tenures.push(block);
            }
        }
        assert!(
            tenures.len() >= 2,
            "the capture holds only {} signed tenures, so no tie between different \
             sortitions can be built",
            tenures.len()
        );
        let (earlier, later) = (tenures[0], tenures[1]);
        let candidate = |block: &NanoNakamotoBlock, peer: usize, height: u64| {
            let mut header = block.header.clone();
            // Equal length is the premise; the tie-break is what decides.
            header.chain_length = height;
            nano_sync::CandidateTip {
                peer,
                info: nano_sync::TenureInfo {
                    consensus_hash: header.consensus_hash,
                    tenure_start_block_id: block.block_id(),
                    parent_consensus_hash: header.consensus_hash,
                    parent_tenure_start_block_id: header.parent_block_id,
                    tip_block_id: block.block_id(),
                    tip_height: height,
                    reward_cycle: 0,
                },
                header,
            }
        };
        let view = StatedBurnView(
            [
                (earlier.header.consensus_hash.to_string(), 100),
                (later.header.consensus_hash.to_string(), 101),
            ]
            .into_iter()
            .collect(),
        );
        let candidates = vec![candidate(earlier, 0, 500), candidate(later, 1, 500)];
        // The weight rule is deliberately out of this test, and it has to be: a
        // header's chain length is inside its signature preimage, so *making* two
        // real captured tips equally long invalidates both signatures. Two equally
        // long, equally weighted, genuinely signed tips cannot be built from a
        // capture at all -- only mined -- so what is pinned here is the comparator
        // that runs after refusal, and refusal has its own tests either side of
        // this one.
        let chosen = nano_sync::choose_canonical_tip(&candidates, None, Some(&view))
            .expect("one of two tips is canonical");
        assert_eq!(
            chosen.header.consensus_hash, later.header.consensus_hash,
            "the tip whose sortition is at the higher burn height wins the tie, as \
             stacks-core's `sn_current.block_height < sn_accepted.block_height` does"
        );
        // And the other way round, so the answer is the rule rather than the order.
        let swapped = vec![candidate(later, 0, 500), candidate(earlier, 1, 500)];
        assert_eq!(
            nano_sync::choose_canonical_tip(&swapped, None, Some(&view))
                .map(|tip| tip.header.consensus_hash),
            Some(later.header.consensus_hash),
        );
        // A longer chain still wins outright: the tie-break is a tie-break.
        let longer = vec![candidate(earlier, 0, 501), candidate(later, 1, 500)];
        assert_eq!(
            nano_sync::choose_canonical_tip(&longer, None, Some(&view))
                .map(|tip| tip.header.consensus_hash),
            Some(earlier.header.consensus_hash),
        );
        // With no burn view to place them, the deterministic identifier decides --
        // which is where stacks-core keeps whichever block it happened to see first,
        // so there is no answer of its own to agree with.
        assert!(
            nano_sync::choose_canonical_tip(&candidates, None, None).is_some(),
            "a tie this node cannot place still resolves"
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
                .append_unauthenticated_fixture_block_with_bitcoin_operations(
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
            retracted_from.checked_sub(1).map(|index| executed[index]),
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
                .append_unauthenticated_fixture_block_with_bitcoin_operations(
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
        let height = Value::UInt(u128::from(target.header.chain_length));

        // Read through a deployed contract rather than a bare program: the node
        // has one execution engine, and a read it answers by another route is
        // not the read a contract on the chain would get.
        let reader = deployed_contract("reader");
        let sender: clarity::vm::types::PrincipalData = reader.issuer.clone().into();
        let (_, last_context) = executed.last().expect("a block was executed");
        let vm = chainstate.vm_mut();
        vm.begin_block_with_bitcoin_context(parent, [0xbb; 32], *last_context)
            .expect("begin a block to read from");
        vm.deploy_contract(
            reader.clone(),
            clarity::vm::ClarityVersion::Clarity6,
            READER,
            LimitedCostTracker::new_free(),
        )
        .expect("deploy the reader");
        let mut read = |function: &str| {
            vm.call_contract_values(&sender, &reader, function, std::slice::from_ref(&height))
                .unwrap_or_else(|error| panic!("{function}: {error}"))
        };

        assert_eq!(
            read("block-header-hash"),
            Value::some(
                Value::buff_from(target.header.block_hash().as_bytes().to_vec())
                    .expect("32-byte buffer")
            )
            .expect("optional"),
        );
        assert_eq!(
            read("block-time"),
            Value::some(Value::UInt(u128::from(target.header.timestamp))).expect("optional"),
        );
        assert_eq!(
            read("tenure-burn-hash"),
            Value::some(
                Value::buff_from(context.burn_header_hash.to_vec()).expect("32-byte buffer")
            )
            .expect("optional"),
        );
        assert_eq!(
            read("tenure-time"),
            Value::some(Value::UInt(u128::from(context.burn_block_time))).expect("optional"),
        );
        assert_eq!(
            read("tenure-vrf-seed"),
            Value::some(Value::buff_from(context.vrf_seed.to_vec()).expect("32-byte buffer"))
                .expect("optional"),
        );
        assert_eq!(
            read("tenure-spend-winner"),
            Value::some(Value::UInt(context.burn_spend_winner)).expect("optional"),
        );
        assert_eq!(
            read("tenure-spend-total"),
            Value::some(Value::UInt(context.burn_spend_total)).expect("optional"),
        );
        // Nothing is sealed: this block only existed to read from.
        drop(chainstate.vm_mut().abort_block());
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
                    } if operation.transaction_index == u32::from(key_index) => {
                        Some(vrf_public_key)
                    }
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

    /// A tenure-start block inside the emission schedule moves real STX, and
    /// moves the liquid supply with it.
    ///
    /// The capture's own burn heights sit far below the schedule, which is why
    /// replay stays green without the emission at all; executing one of its
    /// tenure-start blocks against a Bitcoin height inside the schedule is what
    /// shows the mint lands. The state root is not checked, because the height
    /// is not the one the block committed to.
    ///
    /// The same block is executed twice, once at each height, because the supply
    /// is not readable as a number: the MARF holds the hash of the value, so what
    /// can be compared is the leaf itself, and the emission is the only term of
    /// that write which the Bitcoin height moves. A mint that credited the
    /// recipient without raising the supply passes every other assertion here
    /// and seals a root the network does not have.
    #[test]
    fn a_tenure_start_block_mints_the_sip_031_emission() {
        /// Where Clarity keeps the liquid supply, which every mint has to raise.
        const LIQUID_SUPPLY: &str = "_stx-data::ustx_liquid_supply";

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
        assert!(
            expected > 0,
            "the chosen height must be inside the schedule"
        );

        let recipient = clarity::vm::types::PrincipalData::Contract(
            clarity::vm::types::QualifiedContractIdentifier::parse(
                &network.boot_contract_id("sip-031"),
            )
            .expect("the SIP-031 contract is a valid identifier"),
        );
        let captured_context = *snapshots
            .get(&block.header.consensus_hash.to_string())
            .expect("Bitcoin context");
        let operations = bitcoin_operations
            .get(&block.header.consensus_hash.to_string())
            .expect("Bitcoin operations");

        // Each run starts from the checkpoint, which imports into memory, so the
        // two are independent states rather than one state executed twice.
        let executed = |height: u64| {
            let mut chainstate = captured_chainstate(&fixture);
            let before = chainstate
                .account_balance(&recipient)
                .expect("read the recipient's balance");
            let mut context = captured_context;
            context.move_to_burn_block(height);
            let applied = chainstate
                .execute_unauthenticated_fixture_block_with_bitcoin_operations(
                    context,
                    operations,
                    Some(source),
                    &block,
                )
                .expect("execute block");
            let after = chainstate
                .account_balance(&recipient)
                .expect("read the recipient's balance");
            let supply = chainstate
                .state_leaves(*block.block_id().as_bytes())
                .expect("read the block's leaves")
                .expect("the block sealed its leaves")
                .into_iter()
                .find(|(path, _)| *path == nano_marf::key_path(LIQUID_SUPPLY.as_bytes()))
                .map(|(_, value)| value)
                .expect("every block writes the liquid supply");
            (after - before, applied.receipts, supply)
        };

        let (credited, receipts, supply) = executed(bitcoin_height);
        assert_eq!(credited, expected, "the emission did not land");

        // The mint is reported on the coinbase, which is where stacks-core
        // attaches it and so where a receipt comparison looks for it.
        let minted = receipts.iter().any(|receipt| {
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

        // The same block at the height it was actually mined at, which the
        // schedule does not reach.
        assert_eq!(
            nano_chainstate::sip_031_emission(network, captured_context.height),
            0,
            "the capture's own heights are below the schedule"
        );
        let (uncredited, quiet, unraised) = executed(captured_context.height);
        assert_eq!(uncredited, 0, "an emission landed outside the schedule");
        assert!(
            !quiet.iter().any(|receipt| {
                receipt.result.events.iter().any(|event| {
                    matches!(
                        event,
                        clarity::vm::events::StacksTransactionEvent::STXEvent(
                            clarity::vm::events::STXEventType::STXMintEvent(data),
                        ) if data.recipient == recipient
                    )
                })
            }),
            "a mint was reported outside the schedule"
        );
        assert_ne!(
            supply, unraised,
            "the emission credited the recipient without raising the liquid supply"
        );
    }

    /// The emission a tenure-start block mints to `.sip-031`.
    ///
    /// stacks-core's own lookup is the oracle, not a copy of it: a `testing`
    /// build reads the schedule out of an overridable table and nothing else, so
    /// the release table is installed into that override and
    /// `get_sip_031_emission_at_height` is then asked. That covers the scan rule
    /// as well as the amounts — a table read with `>` instead of `>=` mints a
    /// tenure late at every boundary. Every probe height comes from the table
    /// rather than from a list restated here, so an interval that moves is
    /// caught rather than probed around.
    ///
    /// This also stands in for stacks-core's `includes_sip_031()` gate, which
    /// nano has no counterpart to. Two things make it unreachable: mainnet's
    /// first interval *is* the epoch 3.2 boundary, asserted below against
    /// stacks-core's own constant, and nano executes epoch 4.0 blocks only, so
    /// the gate is open in every block it will ever finalize.
    #[test]
    fn sip_031_emission_matches_stacks_core() {
        use stacks_common::types::{
            SIP031_EMISSION_INTERVALS_MAINNET, SIP031_EMISSION_INTERVALS_TESTNET,
            SIP031EmissionInterval, set_test_sip_031_emission_schedule,
        };

        for network in [Network::MAINNET, Network::TESTNET] {
            let schedule: Vec<SIP031EmissionInterval> = if network.is_mainnet() {
                SIP031_EMISSION_INTERVALS_MAINNET.to_vec()
            } else {
                SIP031_EMISSION_INTERVALS_TESTNET.to_vec()
            };
            // Every interval boundary, the height either side of it, and the
            // range below the schedule where nothing is minted at all.
            let mut heights = vec![0, 1, 100_000, u64::from(u32::MAX)];
            for interval in &schedule {
                let start = interval.start_height;
                heights.extend([start - 1, start, start + 1]);
            }
            // The override ignores its `mainnet` argument, so one network's
            // table is installed at a time. Nothing else in this binary reads
            // it.
            set_test_sip_031_emission_schedule(Some(schedule));
            for height in heights {
                let minted = nano_chainstate::sip_031_emission(network, height);
                assert_eq!(
                    minted,
                    SIP031EmissionInterval::get_sip_031_emission_at_height(
                        height,
                        network.is_mainnet()
                    ),
                    "SIP-031 emission diverges at Bitcoin height {height}"
                );
                // The epoch gate, where it can be checked at all: nano reads a
                // non-mainnet chain's epochs from its own configuration and
                // executes it at 4.0, so only mainnet has a boundary to compare
                // against.
                assert!(
                    minted == 0
                        || !network.is_mainnet()
                        || height >= blockstack_lib::core::BITCOIN_MAINNET_STACKS_32_BURN_HEIGHT,
                    "mainnet mints {minted} at Bitcoin height {height}, below the epoch 3.2 \
                     boundary where stacks-core's `includes_sip_031()` first opens"
                );
            }
            set_test_sip_031_emission_schedule(None);
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

    fn install_execution_only_capture(
        root: &Path,
        fixture: &Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        write_file(
            &root.join("manifest.toml"),
            "mode = \"captured\"\nreplay_blocks = 1\nreceipts = false\n",
        )?;
        write_file(&root.join("nakamoto/blocks/00000001.bin"), "block")?;
        write_file(&root.join("stacker_set/cycle-0.json"), "{}")?;
        write_file(
            &root.join("chainstate/checkpoint-H/checkpoint.toml"),
            "format = \"stacks-core-marf-sqlite-v2\"\nsource_state_id = \"id\"\npublished_state_index_root = \"root\"\n",
        )?;
        write_file(
            &root.join("chainstate/checkpoint-H/native-effects.json"),
            "{\"matured_effects\":[]}",
        )?;
        fs::copy(
            fixture.join("provenance.toml"),
            root.join("provenance.toml"),
        )?;
        Ok(())
    }

    fn install_captured_seed(
        root: &Path,
        fixture: &Path,
    ) -> Result<(serde_json::Value, BitcoinBlock), Box<dyn std::error::Error>> {
        let history = fs::read(fixture.join("sortition/consensus-hashes.json"))?;
        let history: serde_json::Value = serde_json::from_slice(&history)?;
        let seed_consensus_hash = history["hashes"]
            .as_array()
            .and_then(|hashes| hashes.last())
            .and_then(serde_json::Value::as_str)
            .expect("the captured history ends at a consensus hash");
        let snapshots: Vec<serde_json::Value> =
            serde_json::from_slice(&fs::read(fixture.join("sortition/snapshots.json"))?)?;
        let seed = snapshots
            .into_iter()
            .find(|snapshot| snapshot["consensus_hash"] == seed_consensus_hash)
            .expect("the capture carries its seed snapshot");
        let bitcoin_hash = seed["burn_header_hash"]
            .as_str()
            .expect("the seed names its Bitcoin block");
        let bitcoin_height = seed["block_height"]
            .as_u64()
            .expect("the seed names its Bitcoin height");
        fs::create_dir_all(root.join("sortition"))?;
        fs::copy(
            fixture.join("sortition/consensus-hashes.json"),
            root.join("sortition/consensus-hashes.json"),
        )?;
        fs::copy(
            fixture.join("sortition/leader-keys.json"),
            root.join("sortition/leader-keys.json"),
        )?;
        fs::create_dir_all(root.join("bitcoin/blocks"))?;
        fs::copy(
            fixture
                .join("bitcoin/blocks")
                .join(format!("{bitcoin_hash}.hex")),
            root.join("bitcoin/blocks")
                .join(format!("{bitcoin_hash}.hex")),
        )?;

        write_file(
            &root.join("sortition/snapshots.json"),
            &serde_json::to_string(&[&seed])?,
        )?;
        validate_sortition_seed(root).expect("the unmodified captured seed is recoverable");

        let raw = hex::decode(
            fs::read_to_string(
                root.join("bitcoin/blocks")
                    .join(format!("{bitcoin_hash}.hex")),
            )?
            .trim(),
        )?;
        let block = decode_bitcoin_block(bitcoin_height, &raw, captured_magic(root))?;
        Ok((seed, block))
    }

    #[test]
    fn a_capture_with_an_absent_winner_and_disagreeing_candidates_is_rejected()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = temporary_fixture_root()?;
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        install_execution_only_capture(&root, &fixture)?;
        let (mut seed, block) = install_captured_seed(&root, &fixture)?;
        let bitcoin_height = seed["block_height"]
            .as_u64()
            .expect("the seed names its Bitcoin height");
        let eligible_seeds = block
            .operations
            .iter()
            .filter_map(|operation| match operation.kind {
                nano_bitcoin::BitcoinOperationKind::LeaderBlockCommit {
                    new_seed,
                    parent_modulus,
                    ..
                } if nano_sortition::commitment_is_on_time(parent_modulus, bitcoin_height) => {
                    Some(new_seed)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            eligible_seeds
                .iter()
                .skip(1)
                .any(|candidate| candidate != &eligible_seeds[0]),
            "the real seed block must carry disagreeing eligible commitments"
        );

        let absent_winner = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        assert!(
            block
                .operations
                .iter()
                .all(|operation| hex::encode(operation.txid) != absent_winner),
            "the adversarial snapshot must name a commitment absent from the decoded block"
        );
        seed["winning_block_txid"] = serde_json::Value::String(absent_winner.to_owned());
        write_file(
            &root.join("sortition/snapshots.json"),
            &serde_json::to_string(&[seed])?,
        )?;

        let error = validate_fixture_tree(&root)
            .expect_err("release validation must reject an unrecoverable capture seed");
        let FixtureValidationError::InvalidSortitionSeed { reason, .. } = error else {
            panic!("the fixture must reach seed recovery, not fail earlier: {error}");
        };
        assert!(reason.contains(absent_winner), "{reason}");
        assert!(
            reason.contains("eligible commitment nor an agreement"),
            "{reason}"
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
            // Subtraction and the big-endian conversion, which the two
            // implementations disagree about in opposite directions if either is
            // wrong: `primitive_types` refuses an underflow where stacks-core's own
            // `Uint256` wraps, and a byte order is only ever visible against
            // somebody else's.
            if ours_left >= ours_right {
                let (ours_difference, _) = ours_left.overflowing_sub(ours_right);
                prop_assert_eq!(
                    uint_to_words(ours_difference),
                    (reference_left - reference_right).0
                );
            }
            // `to_u8_slice` is stacks-core's *little*-endian conversion and
            // `to_u8_slice_be` the big-endian one; asking the wrong one of the pair
            // is how this assertion first failed, on nano's correct answer.
            let ours_big_endian = ours_left.to_big_endian();
            prop_assert_eq!(ours_big_endian, reference_left.to_u8_slice_be());
            prop_assert_eq!(ours_left.to_little_endian(), reference_left.to_u8_slice());
            prop_assert_eq!(
                uint_to_words(nano_primitives::Uint256::from_big_endian(&ours_big_endian)),
                left
            );
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
            ours.insert_path(path, MarfValue::from_value(text.as_bytes()))
                .expect("the in-memory trie stores");
            let root = ours.root_hash().expect("the in-memory trie hashes");
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
            ours.insert_path(path, MarfValue::from_value(first_text.as_bytes()))
                .expect("the in-memory trie stores");
            ours.insert_path(alternate, MarfValue::from_value(second_text.as_bytes()))
                .expect("the in-memory trie stores");
            let root = ours.root_hash().expect("the in-memory trie hashes");
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
                timestamp: 0,
                operations: Vec::new(),
            };
            let winner_vrf_seed = (height % 3 == 0).then_some(hash);
            let winner = winner_vrf_seed.map(|vrf_seed| nano_sortition::SortitionWinner {
                vrf_public_key: None,
                signing_key_hash: None,
                txid: hash,
                vrf_seed,
                committed_block_hash: hash,
                parent_bitcoin_height: height.saturating_sub(1),
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
                            block_header_hash,
                            new_seed,
                            parent_block_height,
                            ..
                        } => Some(nano_sortition::SortitionWinner {
                            vrf_public_key: None,
                            signing_key_hash: None,
                            txid: winning_txid,
                            vrf_seed: new_seed,
                            committed_block_hash: block_header_hash,
                            parent_bitcoin_height: u64::from(parent_block_height),
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
                nano_sortition::OpsHash::from_txids(&nano_sortition::accepted_operation_txids(
                    &block
                ),)
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
