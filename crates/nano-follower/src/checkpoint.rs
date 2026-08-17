use std::{fmt, path::Path};

use nano_chainstate::{NakamotoBlockHeader, SignerSet, SignerSetError, TenureAccounting};
pub use nano_marf::{
    CheckpointAttestation, CheckpointBundleReceipt, CheckpointManifest, CheckpointProvenance,
};
use nano_primitives::{Network, TrieHash};

/// A checkpointed Clarity state, and what is needed to execute from it.
#[derive(Clone, Debug)]
pub struct Checkpoint<P> {
    /// The chain this state belongs to, which fixes how it is executed.
    pub network: Network,
    /// Path to the checkpoint's MARF.
    pub path: P,
    /// The MARF state the checkpoint was taken at.
    pub source: [u8; 32],
    /// The state root that state is published under.
    pub state_root: TrieHash,
    /// The matured native rewards the checkpoint still owes, if it records them.
    pub accounting: Option<TenureAccounting>,
}

impl<P> Checkpoint<P> {
    /// Take the state and root to import from what the checkpoint publishes.
    ///
    /// Attest the manifest first: `Checkpoint` carries no evidence, so whatever
    /// this is built from is what the node ends up believing.
    #[must_use]
    pub const fn from_manifest(network: Network, path: P, manifest: &CheckpointManifest) -> Self {
        Self {
            network,
            path,
            source: manifest.source_state_id,
            state_root: manifest.state_index_root,
            accounting: None,
        }
    }
}

/// Why a checkpoint is not the state the signed header at its height published.
#[derive(Debug)]
pub enum CheckpointTrustError {
    Height { claimed: u64, header: u64 },
    StateId { claimed: [u8; 32], header: [u8; 32] },
    StateRoot { claimed: TrieHash, header: TrieHash },
    Signers(SignerSetError),
    Provenance(nano_marf::CheckpointError),
}

impl fmt::Display for CheckpointTrustError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Height { claimed, header } => write!(
                formatter,
                "checkpoint claims Stacks height {claimed}, attesting header is at {header}"
            ),
            Self::StateId { claimed, header } => write!(
                formatter,
                "checkpoint claims state {claimed:02x?}, attesting header sealed {header:02x?}"
            ),
            Self::StateRoot { claimed, header } => write!(
                formatter,
                "checkpoint claims state root {claimed}, attesting header published {header}"
            ),
            Self::Signers(error) => {
                write!(formatter, "no reward set attested the checkpoint: {error}")
            }
            Self::Provenance(error) => {
                write!(formatter, "checkpoint provenance was refused: {error}")
            }
        }
    }
}

impl std::error::Error for CheckpointTrustError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Signers(error) => Some(error),
            Self::Provenance(error) => Some(error),
            Self::Height { .. } | Self::StateId { .. } | Self::StateRoot { .. } => None,
        }
    }
}

impl From<SignerSetError> for CheckpointTrustError {
    fn from(error: SignerSetError) -> Self {
        Self::Signers(error)
    }
}

impl From<nano_marf::CheckpointError> for CheckpointTrustError {
    fn from(error: nano_marf::CheckpointError) -> Self {
        Self::Provenance(error)
    }
}

/// Check a checkpoint against the signed Nakamoto header that published its state.
///
/// `signer_signature_hash` is taken over a preimage that contains
/// `state_index_root`, so a reward set that signed the header signed the root
/// too, and `block_id` binds both to one state. That moves the checkpoint's
/// root from something the operator asserts to something the chain's own
/// signers put threshold weight behind — as long as `signers` is the reward set
/// of that cycle obtained independently of the checkpoint. See
/// `docs/checkpoint-trust.md`.
pub fn attest_checkpoint(
    manifest: &CheckpointManifest,
    header: &NakamotoBlockHeader,
    signers: &SignerSet,
) -> Result<CheckpointAttestation, CheckpointTrustError> {
    if header.chain_length != manifest.stacks_height {
        return Err(CheckpointTrustError::Height {
            claimed: manifest.stacks_height,
            header: header.chain_length,
        });
    }
    if *header.block_id().as_bytes() != manifest.source_state_id {
        return Err(CheckpointTrustError::StateId {
            claimed: manifest.source_state_id,
            header: *header.block_id().as_bytes(),
        });
    }
    if header.state_index_root != manifest.state_index_root {
        return Err(CheckpointTrustError::StateRoot {
            claimed: manifest.state_index_root,
            header: header.state_index_root,
        });
    }
    Ok(CheckpointAttestation {
        attesting_block_id: *header.block_id().as_bytes(),
        signer_weight: signers.verify(header)?,
        approval_threshold: signers.approval_threshold()?,
    })
}

/// Attest a checkpoint and record it as the origin of a state directory.
pub fn adopt_checkpoint(
    state_directory: impl AsRef<Path>,
    manifest: &CheckpointManifest,
    header: &NakamotoBlockHeader,
    signers: &SignerSet,
) -> Result<CheckpointAttestation, CheckpointTrustError> {
    record_checkpoint(state_directory, manifest, header, signers, None)
}

/// Attest a signed bundle and persist the content-root receipt for bounded restarts.
pub fn adopt_checkpoint_bundle(
    state_directory: impl AsRef<Path>,
    manifest: &CheckpointManifest,
    header: &NakamotoBlockHeader,
    signers: &SignerSet,
    bundle: CheckpointBundleReceipt,
) -> Result<CheckpointAttestation, CheckpointTrustError> {
    record_checkpoint(state_directory, manifest, header, signers, Some(bundle))
}

fn record_checkpoint(
    state_directory: impl AsRef<Path>,
    manifest: &CheckpointManifest,
    header: &NakamotoBlockHeader,
    signers: &SignerSet,
    bundle: Option<CheckpointBundleReceipt>,
) -> Result<CheckpointAttestation, CheckpointTrustError> {
    let attestation = attest_checkpoint(manifest, header, signers)?;
    CheckpointProvenance {
        checkpoint: manifest.clone(),
        attestation: Some(attestation),
        bundle,
    }
    .record(state_directory)?;
    Ok(attestation)
}
