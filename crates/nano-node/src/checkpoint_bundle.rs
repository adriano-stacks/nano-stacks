//! Build and verify the checkpoint trust root without opening production state.

use std::{fs, path::Path};

use nano_chainstate::{NakamotoBlock, NakamotoCodecError, Signer, SignerSet, SignerSetError};
use nano_crypto::StacksPublicKey;
use nano_marf::{
    BundleClaims, BundleError, CHECKPOINT_BLOCK_FILE, CheckpointBundleManifest, CheckpointManifest,
};

use crate::{CheckpointTrustError, attest_checkpoint};

/// The independently sourced reward set that attests `block.bin`.
pub const CHECKPOINT_REWARD_SET_FILE: &str = "reward-set.json";

/// Build a content manifest after locally checking the checkpoint's signer proof.
///
/// # Errors
///
/// Returns an error for an unreadable or malformed bundle, a signer proof below
/// threshold, claims inconsistent with `checkpoint.toml`, or an existing
/// `checkpoint-bundle.toml`.
pub fn build_checkpoint_bundle_manifest(
    directory: impl AsRef<Path>,
    bitcoin_block_hash: &str,
) -> Result<CheckpointBundleManifest, CheckpointBundleError> {
    let directory = directory.as_ref();
    let claims = observed_claims(directory, bitcoin_block_hash)?;
    Ok(CheckpointBundleManifest::write_new(directory, claims)?)
}

/// Verify every bundle byte and independently recompute its signer/profile claims.
///
/// # Errors
///
/// Returns an error for every content mismatch, malformed signer set, insufficient
/// signature weight, or mismatch with this artifact's compiler/profile identity.
pub fn verify_checkpoint_bundle(
    directory: impl AsRef<Path>,
) -> Result<CheckpointBundleManifest, CheckpointBundleError> {
    let directory = directory.as_ref();
    let manifest = CheckpointBundleManifest::verify(directory)?;
    let observed = observed_claims(directory, &manifest.checkpoint.bitcoin_block_hash)?;
    if observed != manifest.checkpoint {
        return Err(CheckpointBundleError::Invalid(
            "bundle claims differ from their locally verified values".to_owned(),
        ));
    }
    Ok(manifest)
}

fn observed_claims(
    directory: &Path,
    bitcoin_block_hash: &str,
) -> Result<BundleClaims, CheckpointBundleError> {
    let checkpoint = CheckpointManifest::load(directory)?;
    let block = NakamotoBlock::decode(&fs::read(directory.join(CHECKPOINT_BLOCK_FILE))?)?;
    let signers = attesting_reward_set(&fs::read(directory.join(CHECKPOINT_REWARD_SET_FILE))?)?;
    let attestation = attest_checkpoint(&checkpoint, &block.header, &signers)?;
    Ok(BundleClaims {
        state_format: checkpoint.format,
        checkpoint_stacks_height: checkpoint.stacks_height,
        source_state_id: hex::encode(checkpoint.source_state_id),
        published_state_index_root: checkpoint.state_index_root.to_string(),
        bitcoin_height: checkpoint.first_bitcoin_height,
        bitcoin_block_hash: bitcoin_block_hash.to_owned(),
        attesting_block_id: hex::encode(attestation.attesting_block_id),
        signer_weight: attestation.signer_weight,
        approval_threshold: attestation.approval_threshold,
        semantic_epoch: "Epoch40".to_owned(),
        compiler_identity: nano_vm::COMPILER_IDENTITY.to_owned(),
        profile_fingerprint: nano_vm::compatibility_profile_fingerprint().to_string(),
    })
}

/// The reward set a `/v3/stacker_set/:cycle` document names.
pub(crate) fn attesting_reward_set(bytes: &[u8]) -> Result<SignerSet, CheckpointBundleError> {
    let document: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| CheckpointBundleError::Invalid(error.to_string()))?;
    let entries = document["stacker_set"]["signers"]
        .as_array()
        .ok_or_else(|| {
            CheckpointBundleError::Invalid("the reward set names no signers".to_owned())
        })?;
    let signers = entries
        .iter()
        .map(|entry| {
            let key = entry["signing_key"].as_str().ok_or_else(|| {
                CheckpointBundleError::Invalid("a signer has no signing key".to_owned())
            })?;
            let key = hex::decode(key.trim_start_matches("0x"))
                .map_err(|error| CheckpointBundleError::Invalid(error.to_string()))?;
            let public_key = StacksPublicKey::from_bytes(&key).map_err(|error| {
                CheckpointBundleError::Invalid(format!(
                    "a signing key is not a public key: {error}"
                ))
            })?;
            let weight = entry["weight"].as_u64().ok_or_else(|| {
                CheckpointBundleError::Invalid("a signer has no weight".to_owned())
            })?;
            Ok(Signer {
                public_key,
                weight: u32::try_from(weight).map_err(|_| {
                    CheckpointBundleError::Invalid("a signer weight exceeds u32".to_owned())
                })?,
            })
        })
        .collect::<Result<Vec<_>, CheckpointBundleError>>()?;
    Ok(SignerSet::new(signers)?)
}

/// Why an offline checkpoint bundle check failed.
#[derive(Debug)]
pub enum CheckpointBundleError {
    Io(std::io::Error),
    Bundle(BundleError),
    Checkpoint(nano_marf::CheckpointError),
    Codec(NakamotoCodecError),
    Signers(SignerSetError),
    Trust(CheckpointTrustError),
    Invalid(String),
}

impl std::fmt::Display for CheckpointBundleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "checkpoint bundle I/O failed: {error}"),
            Self::Bundle(error) => write!(formatter, "{error}"),
            Self::Checkpoint(error) => write!(formatter, "{error}"),
            Self::Codec(error) => write!(formatter, "checkpoint block is invalid: {error}"),
            Self::Signers(error) => write!(formatter, "checkpoint reward set is invalid: {error}"),
            Self::Trust(error) => write!(formatter, "{error}"),
            Self::Invalid(reason) => write!(formatter, "invalid checkpoint bundle: {reason}"),
        }
    }
}

impl std::error::Error for CheckpointBundleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Bundle(error) => Some(error),
            Self::Checkpoint(error) => Some(error),
            Self::Codec(error) => Some(error),
            Self::Signers(error) => Some(error),
            Self::Trust(error) => Some(error),
            Self::Invalid(_) => None,
        }
    }
}

impl From<std::io::Error> for CheckpointBundleError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<BundleError> for CheckpointBundleError {
    fn from(error: BundleError) -> Self {
        Self::Bundle(error)
    }
}

impl From<nano_marf::CheckpointError> for CheckpointBundleError {
    fn from(error: nano_marf::CheckpointError) -> Self {
        Self::Checkpoint(error)
    }
}

impl From<NakamotoCodecError> for CheckpointBundleError {
    fn from(error: NakamotoCodecError) -> Self {
        Self::Codec(error)
    }
}

impl From<SignerSetError> for CheckpointBundleError {
    fn from(error: SignerSetError) -> Self {
        Self::Signers(error)
    }
}

impl From<CheckpointTrustError> for CheckpointBundleError {
    fn from(error: CheckpointTrustError) -> Self {
        Self::Trust(error)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;

    use nano_chainstate::{NakamotoBlock, NakamotoBlockHeader};
    use nano_crypto::StacksPrivateKey;
    use nano_marf::{CHECKPOINT_BLOCK_FILE, CheckpointBundleManifest};
    use nano_primitives::{BitVec, ConsensusHash, Sha256Sum, StacksBlockId, TrieHash};

    use super::{
        CHECKPOINT_REWARD_SET_FILE, CheckpointBundleError, build_checkpoint_bundle_manifest,
        observed_claims, verify_checkpoint_bundle,
    };

    pub fn fixture(all_signers: bool) -> (tempfile::TempDir, [StacksPrivateKey; 2]) {
        let root = tempfile::tempdir().expect("checkpoint bundle");
        let miner = StacksPrivateKey::from_seed(b"checkpoint miner");
        let first = StacksPrivateKey::from_seed(b"checkpoint builder signer one");
        let second = StacksPrivateKey::from_seed(b"checkpoint builder signer two");
        let mut block = NakamotoBlock {
            header: NakamotoBlockHeader {
                version: 1,
                chain_length: 7,
                bitcoin_spent: 11,
                consensus_hash: ConsensusHash::from_bytes([1; 20]),
                parent_block_id: StacksBlockId::from_bytes([2; 32]),
                transaction_merkle_root: Sha256Sum::default(),
                state_index_root: TrieHash::from_bytes([3; 32]),
                timestamp: 4,
                miner_signature: miner.sign(&[5; 32]),
                signer_signatures: Vec::new(),
                pox_treatment: BitVec::zeros(1).expect("PoX treatment"),
                problematic_transactions: Vec::new(),
            },
            transactions: Vec::new(),
        };
        let digest = block.header.signer_signature_hash();
        block.header.signer_signatures = vec![first.sign(digest.as_bytes())];
        if all_signers {
            block
                .header
                .signer_signatures
                .push(second.sign(digest.as_bytes()));
        }
        fs::write(root.path().join(CHECKPOINT_BLOCK_FILE), block.encode()).expect("block");
        fs::write(
            root.path().join(CHECKPOINT_REWARD_SET_FILE),
            serde_json::to_vec(&serde_json::json!({
                "stacker_set": {
                    "signers": [
                        {
                            "signing_key": hex::encode(first.public_key().to_bytes_compressed()),
                            "weight": 1,
                        },
                        {
                            "signing_key": hex::encode(second.public_key().to_bytes_compressed()),
                            "weight": 1,
                        }
                    ]
                }
            }))
            .expect("reward set"),
        )
        .expect("reward set file");
        fs::write(
            root.path().join("checkpoint.toml"),
            format!(
                "format = \"stacks-core-marf-sqlite-v2\"\n\
                 checkpoint_stacks_height = 7\n\
                 source_state_id = \"{}\"\n\
                 published_state_index_root = \"{}\"\n\
                 first_bitcoin_height = 11\n\
                 profile_fingerprint = \"{}\"\n",
                block.header.block_id(),
                block.header.state_index_root,
                nano_vm::compatibility_profile_fingerprint()
            ),
        )
        .expect("checkpoint manifest");
        fs::write(root.path().join("marf.sqlite"), b"state bytes").expect("state");
        (root, [first, second])
    }

    #[test]
    fn offline_bundle_build_and_verification_recompute_the_signer_proof() {
        let (root, _) = fixture(true);
        let manifest = build_checkpoint_bundle_manifest(root.path(), &"06".repeat(32))
            .expect("build manifest");
        let verified = verify_checkpoint_bundle(root.path()).expect("verify manifest");
        assert_eq!(verified.content_root(), manifest.content_root());
        assert_eq!(verified.checkpoint.signer_weight, 2);
        assert_eq!(verified.checkpoint.approval_threshold, 2);
    }

    #[test]
    fn a_self_consistent_but_false_signer_claim_is_refused() {
        let (root, _) = fixture(true);
        let mut claims = observed_claims(root.path(), &"06".repeat(32)).expect("observed claims");
        claims.signer_weight += 1;
        CheckpointBundleManifest::write_new(root.path(), claims).expect("false manifest");
        assert!(matches!(
            verify_checkpoint_bundle(root.path()),
            Err(CheckpointBundleError::Invalid(reason))
                if reason.contains("locally verified")
        ));
    }

    #[test]
    fn a_checkpoint_below_its_signer_threshold_gets_no_manifest() {
        let (root, _) = fixture(false);
        assert!(matches!(
            build_checkpoint_bundle_manifest(root.path(), &"06".repeat(32)),
            Err(CheckpointBundleError::Trust(_))
        ));
        assert!(!root.path().join(nano_marf::BUNDLE_MANIFEST_FILE).exists());
    }
}
