//! What a checkpoint says about itself, and what a node records about the
//! checkpoint its state descends from.
//!
//! nano cannot rebuild a mainnet state from genesis, so the state it starts
//! from arrives as bytes somebody else produced. `docs/checkpoint-trust.md`
//! sets out what trusting those bytes means; these types carry the claims that
//! document reasons about.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use nano_primitives::TrieHash;
use serde::{Deserialize, Serialize};

use crate::{CheckpointError, MarfBlockId, checkpoint::parse_hex};

/// The manifest a checkpoint directory publishes.
const MANIFEST_FILE: &str = "checkpoint.toml";
/// The consensus-serialized block that sealed the checkpoint's state, when the
/// checkpoint carries it.
///
/// A manifest states a root; this block is what a reward set signed it in. The
/// node fetches it from a peer, so a checkpoint without it is still adoptable —
/// keeping it is what lets the attestation be checked with no network at all.
pub const CHECKPOINT_BLOCK_FILE: &str = "block.bin";
/// The record a node keeps in its own state directory.
const PROVENANCE_FILE: &str = "checkpoint-provenance.toml";
/// The record that an import into a state directory is under way.
const UNFINISHED_FILE: &str = "checkpoint-import-unfinished.toml";

/// What a checkpoint publishes about itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointManifest {
    /// The layout of the checkpoint's MARF, which fixes how it is imported.
    pub format: String,
    /// The Stacks height the state was taken at.
    pub stacks_height: u64,
    /// The state's identifier, which is the `block_id` of the block that sealed it.
    pub source_state_id: MarfBlockId,
    /// The state root the checkpoint claims, which the signed Nakamoto header
    /// at `stacks_height` also carries.
    pub state_index_root: TrieHash,
    /// The Bitcoin height the state was taken at.
    pub first_bitcoin_height: u64,
}

impl CheckpointManifest {
    /// Read the manifest a checkpoint directory publishes.
    pub fn load(directory: impl AsRef<Path>) -> Result<Self, CheckpointError> {
        let contents = fs::read_to_string(directory.as_ref().join(MANIFEST_FILE))?;
        toml::from_str::<ManifestWire>(&contents)
            .map_err(|error| CheckpointError::InvalidManifest(error.to_string()))?
            .decode()
    }

    /// Read the manifest beside a checkpoint's MARF, when it publishes one.
    pub fn beside(marf_path: &Path) -> Result<Option<Self>, CheckpointError> {
        let directory = marf_path.parent().unwrap_or_else(|| Path::new("."));
        if directory.join(MANIFEST_FILE).exists() {
            return Self::load(directory).map(Some);
        }
        Ok(None)
    }

    /// Refuse a caller that declares a different root for the state this
    /// checkpoint publishes.
    ///
    /// A checkpoint's MARF carries states either side of the one it publishes,
    /// so importing one of those is not an error. Declaring a root for *this*
    /// state that the checkpoint does not publish is: the two are separate
    /// claims, and a state that hashes to a root nobody published proves
    /// nothing.
    pub fn check_declared_root(
        &self,
        source: MarfBlockId,
        root: TrieHash,
    ) -> Result<(), CheckpointError> {
        if source == self.source_state_id && root != self.state_index_root {
            return Err(CheckpointError::DeclaredRootMismatch {
                declared: root,
                published: self.state_index_root,
            });
        }
        Ok(())
    }
}

/// A signed Nakamoto header's endorsement of a checkpoint's state root.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CheckpointAttestation {
    /// The block whose signed header carried the root.
    pub attesting_block_id: MarfBlockId,
    /// The reward-set weight that signed it.
    pub signer_weight: u32,
    /// The weight that header needed to be accepted.
    pub approval_threshold: u32,
}

/// Where a node's state came from, recorded beside the state itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointProvenance {
    pub checkpoint: CheckpointManifest,
    /// The signed header that endorsed the root, when one was checked.
    pub attestation: Option<CheckpointAttestation>,
}

impl CheckpointProvenance {
    /// Read the provenance a state directory carries, if it has any.
    pub fn load(directory: impl AsRef<Path>) -> Result<Option<Self>, CheckpointError> {
        let path = directory.as_ref().join(PROVENANCE_FILE);
        if !path.exists() {
            return Ok(None);
        }
        toml::from_str::<ProvenanceWire>(&fs::read_to_string(path)?)
            .map_err(|error| CheckpointError::InvalidManifest(error.to_string()))?
            .decode()
            .map(Some)
    }

    /// Record this provenance in a state directory.
    ///
    /// State on disk descends from the checkpoint it was imported from, so a
    /// directory that already names a different one is refused rather than
    /// resumed: continuing would extend one chain's state under another
    /// chain's blocks, and nothing downstream could tell.
    pub fn record(&self, directory: impl AsRef<Path>) -> Result<(), CheckpointError> {
        let directory = directory.as_ref();
        if let Some(recorded) = Self::load(directory)?
            && recorded.checkpoint != self.checkpoint
        {
            return Err(CheckpointError::ProvenanceMismatch {
                recorded: Box::new(recorded.checkpoint),
                configured: Box::new(self.checkpoint.clone()),
            });
        }
        fs::create_dir_all(directory)?;
        let contents = toml::to_string(&ProvenanceWire::encode(self))
            .map_err(|error| CheckpointError::InvalidManifest(error.to_string()))?;
        fs::write(directory.join(PROVENANCE_FILE), contents)?;
        Ok(())
    }
}

/// The mark an import leaves in a state directory while it runs.
///
/// Journalling is off while a checkpoint is imported (see
/// `TrieStorage::open_for_import`), so an import that is killed cannot roll
/// back: pages its transaction had already written stay in the file, and the
/// value side store is copied afterwards out of the trie that was imported. So
/// neither "the MARF has a tip" nor "the value store has rows" tells a finished
/// import apart from one that stopped in the middle — and a node that resumes on
/// that difference executes blocks against a trie missing nodes, computing a
/// wrong state root for every one of them and reporting nothing, because the
/// blocks it is given are the ones the network accepted and their roots are the
/// ones it cannot reproduce.
///
/// This is what tells the two apart: written before the first row, removed after
/// the last. Absence therefore means finished, which is also what lets a state
/// directory imported before this existed open unchanged.
#[derive(Debug)]
pub struct UnfinishedImport {
    directory: PathBuf,
}

impl UnfinishedImport {
    /// Where the mark lives, for an operator to look at and a test to watch.
    #[must_use]
    pub fn marker(directory: impl AsRef<Path>) -> PathBuf {
        directory.as_ref().join(UNFINISHED_FILE)
    }

    /// Refuse a state directory whose last import did not finish.
    pub fn refuse(directory: impl AsRef<Path>) -> Result<(), CheckpointError> {
        let marker = Self::marker(directory.as_ref());
        if !marker.exists() {
            return Ok(());
        }
        let state = fs::read_to_string(&marker)
            .ok()
            .and_then(|contents| toml::from_str::<UnfinishedWire>(&contents).ok())
            .map_or_else(|| "an unnamed state".to_owned(), |wire| wire.source_state_id);
        Err(CheckpointError::UnfinishedImport {
            directory: directory.as_ref().to_path_buf(),
            marker,
            state,
        })
    }

    /// Mark an import of `source` into `directory` as under way.
    pub fn begin(
        directory: impl AsRef<Path>,
        source: MarfBlockId,
    ) -> Result<Self, CheckpointError> {
        let directory = directory.as_ref();
        Self::refuse(directory)?;
        let contents = toml::to_string(&UnfinishedWire {
            source_state_id: hex::encode(source),
            started_unix_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_secs()),
        })
        .map_err(|error| CheckpointError::InvalidManifest(error.to_string()))?;
        let marker = Self::marker(directory);
        fs::write(&marker, contents)?;
        // Durable before the import writes anything, so a machine that loses
        // power mid-import comes back with the mark rather than with the state
        // the import had reached and no way to know it is short.
        sync(&marker)?;
        sync(directory)?;
        Ok(Self {
            directory: directory.to_path_buf(),
        })
    }

    /// Flush what was imported and clear the mark.
    ///
    /// The files are synced first: with `synchronous = OFF` a committed import
    /// is a promise the operating system has not kept yet, and clearing the mark
    /// before it does would leave a directory that says it is complete and is
    /// not.
    pub fn finish(self, files: &[PathBuf]) -> Result<(), CheckpointError> {
        for file in files {
            if file.exists() {
                sync(file)?;
            }
        }
        fs::remove_file(Self::marker(&self.directory))?;
        sync(&self.directory)?;
        Ok(())
    }
}

/// Force a file — or a directory's entries — out to the disk.
fn sync(path: &Path) -> Result<(), CheckpointError> {
    Ok(fs::File::open(path)?.sync_all()?)
}

#[derive(Deserialize, Serialize)]
struct UnfinishedWire {
    source_state_id: String,
    started_unix_seconds: u64,
}

#[derive(Deserialize, Serialize)]
struct ManifestWire {
    format: String,
    checkpoint_stacks_height: u64,
    source_state_id: String,
    published_state_index_root: String,
    first_bitcoin_height: u64,
}

impl ManifestWire {
    fn decode(self) -> Result<CheckpointManifest, CheckpointError> {
        Ok(CheckpointManifest {
            format: self.format,
            stacks_height: self.checkpoint_stacks_height,
            source_state_id: parse_hex(&self.source_state_id)?,
            state_index_root: TrieHash::from_bytes(parse_hex(&self.published_state_index_root)?),
            first_bitcoin_height: self.first_bitcoin_height,
        })
    }

    fn encode(manifest: &CheckpointManifest) -> Self {
        Self {
            format: manifest.format.clone(),
            checkpoint_stacks_height: manifest.stacks_height,
            source_state_id: hex::encode(manifest.source_state_id),
            published_state_index_root: manifest.state_index_root.to_string(),
            first_bitcoin_height: manifest.first_bitcoin_height,
        }
    }
}

#[derive(Deserialize, Serialize)]
struct AttestationWire {
    attesting_block_id: String,
    signer_weight: u32,
    approval_threshold: u32,
}

#[derive(Deserialize, Serialize)]
struct ProvenanceWire {
    checkpoint: ManifestWire,
    attestation: Option<AttestationWire>,
}

impl ProvenanceWire {
    fn decode(self) -> Result<CheckpointProvenance, CheckpointError> {
        let attestation = self
            .attestation
            .map(|wire| {
                Ok::<_, CheckpointError>(CheckpointAttestation {
                    attesting_block_id: parse_hex(&wire.attesting_block_id)?,
                    signer_weight: wire.signer_weight,
                    approval_threshold: wire.approval_threshold,
                })
            })
            .transpose()?;
        Ok(CheckpointProvenance {
            checkpoint: self.checkpoint.decode()?,
            attestation,
        })
    }

    fn encode(provenance: &CheckpointProvenance) -> Self {
        Self {
            checkpoint: ManifestWire::encode(&provenance.checkpoint),
            attestation: provenance
                .attestation
                .map(|attestation| AttestationWire {
                    attesting_block_id: hex::encode(attestation.attesting_block_id),
                    signer_weight: attestation.signer_weight,
                    approval_threshold: attestation.approval_threshold,
                }),
        }
    }
}
