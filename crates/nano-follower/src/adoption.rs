//! Authenticate and adopt the immutable checkpoint before chainstate opens.

use std::{error::Error, fs, path::Path};

use nano_bitcoin::BitcoinRpcSource;
use nano_chainstate::NakamotoBlock;
use nano_primitives::Network;

use crate::{
    CheckpointBundleReceipt, CheckpointManifest, CheckpointProvenance, adopt_checkpoint,
    adopt_checkpoint_bundle,
    checkpoint_bundle::{CHECKPOINT_REWARD_SET_FILE, attesting_reward_set},
    checkpoint_signatures::{
        BuilderPolicy, verify_builder_signatures, verify_signed_checkpoint_bundle,
    },
    config::Config,
};

/// Authenticate the configured checkpoint and bind it to this state directory.
pub fn adopt(config: &Config, directory: &Path) -> Result<(), Box<dyn Error>> {
    let source = config.checkpoint.source_state_id()?;
    if let Some((bundle, policy, signatures)) = config.checkpoint.signed_bundle()? {
        if config.burnchain.rest_url.is_some() {
            return Err(
                "a signed checkpoint must be authenticated against the operator's Bitcoin Core, not a REST source"
                    .into(),
            );
        }
        let bitcoin = BitcoinRpcSource::new(
            &config.burnchain.rpc_url,
            config.burnchain.rpc_user.clone(),
            config.burnchain.rpc_password.clone(),
            config.burnchain.magic()?,
        )?;
        return adopt_signed(
            config, directory, source, &bitcoin, bundle, policy, signatures,
        );
    }
    if config.network().is_some_and(Network::is_mainnet) {
        return Err(
            "a mainnet checkpoint needs a signed bundle before production state is opened".into(),
        );
    }
    adopt_attested(config, directory, source, None)
}

fn adopt_signed<S: nano_bitcoin::BitcoinSource>(
    config: &Config,
    directory: &Path,
    source: [u8; 32],
    bitcoin: &S,
    bundle: &Path,
    policy: &Path,
    signatures: &Path,
) -> Result<(), Box<dyn Error>>
where
    S::Error: std::fmt::Display,
{
    if let Some(recorded) = CheckpointProvenance::load(directory)? {
        already_adopted(recorded.checkpoint.source_state_id, source)?;
        if config.network().is_some_and(Network::is_mainnet) {
            recorded
                .checkpoint
                .check_profile(nano_vm::compatibility_profile_fingerprint())?;
        }
        let receipt = recorded.bundle.as_ref().ok_or(
            "the existing state predates signed checkpoint provenance and must be re-imported",
        )?;
        let canonical = bitcoin
            .block_hash_at(receipt.bitcoin_height)
            .map_err(|error| format!("local Bitcoin header lookup failed: {error}"))?;
        if canonical != receipt.bitcoin_block_hash {
            return Err(format!(
                "checkpoint Bitcoin block {} at height {} is no longer locally canonical ({})",
                hex::encode(receipt.bitcoin_block_hash),
                receipt.bitcoin_height,
                hex::encode(canonical)
            )
            .into());
        }
        verify_external_signing_evidence(policy, signatures)?;
        let policy = BuilderPolicy::load(policy)?;
        let builders = verify_builder_signatures(
            &hex::encode(receipt.content_root),
            recorded.checkpoint.stacks_height,
            &policy,
            signatures,
        )?;
        println!(
            "checkpoint bundle {} reauthenticated by {} from persisted provenance",
            builders.content_root,
            builders.names.join(", ")
        );
        return Ok(());
    }
    verify_external_checkpoint_evidence(bundle, policy, signatures)?;
    let policy = BuilderPolicy::load(policy)?;
    let builders = verify_signed_checkpoint_bundle(bundle, bitcoin, &policy, signatures)?;
    verify_checkpoint_paths(config, bundle)?;
    println!(
        "checkpoint bundle {} authenticated by {}",
        builders.content_root,
        builders.names.join(", ")
    );
    let manifest = nano_marf::CheckpointBundleManifest::load(bundle)?;
    let receipt = CheckpointBundleReceipt {
        content_root: decode_checkpoint_digest(builders.content_root.as_str(), "content root")?,
        bitcoin_height: manifest.checkpoint.bitcoin_height,
        bitcoin_block_hash: decode_checkpoint_digest(
            &manifest.checkpoint.bitcoin_block_hash,
            "Bitcoin block hash",
        )?,
        builders: builders.names,
    };
    adopt_attested(config, directory, source, Some(receipt))
}

fn decode_checkpoint_digest(value: &str, name: &str) -> Result<[u8; 32], Box<dyn Error>> {
    let bytes = hex::decode(value)?;
    bytes
        .try_into()
        .map_err(|_| format!("checkpoint {name} is not 32 bytes").into())
}

fn verify_external_checkpoint_evidence(
    bundle: &Path,
    policy: &Path,
    signatures: &Path,
) -> Result<(), Box<dyn Error>> {
    let bundle_metadata = fs::symlink_metadata(bundle)?;
    if !bundle_metadata.file_type().is_dir() || bundle_metadata.file_type().is_symlink() {
        return Err(format!("{} is not a checkpoint bundle directory", bundle.display()).into());
    }
    verify_external_signing_evidence(policy, signatures)?;
    let bundle = fs::canonicalize(bundle)?;
    for (name, path) in [
        ("builder policy", policy),
        ("builder signatures", signatures),
    ] {
        if fs::canonicalize(path)?.starts_with(&bundle) {
            return Err(format!(
                "the {name} at {} is inside the bundle and therefore self-declared",
                path.display()
            )
            .into());
        }
    }
    Ok(())
}

fn verify_external_signing_evidence(
    policy: &Path,
    signatures: &Path,
) -> Result<(), Box<dyn Error>> {
    let policy_metadata = fs::symlink_metadata(policy)?;
    if !policy_metadata.file_type().is_file() || policy_metadata.file_type().is_symlink() {
        return Err(format!("{} is not a regular builder policy", policy.display()).into());
    }
    let signatures_metadata = fs::symlink_metadata(signatures)?;
    if !signatures_metadata.file_type().is_dir() || signatures_metadata.file_type().is_symlink() {
        return Err(format!(
            "{} is not a builder signature directory",
            signatures.display()
        )
        .into());
    }
    Ok(())
}

fn verify_checkpoint_paths(config: &Config, bundle: &Path) -> Result<(), Box<dyn Error>> {
    require_checkpoint_file(
        bundle,
        &config.checkpoint.marf,
        "marf.sqlite",
        "checkpoint.marf",
    )?;
    require_checkpoint_file(
        bundle,
        config
            .checkpoint
            .attesting_block
            .as_deref()
            .ok_or("a signed checkpoint has no configured attesting block")?,
        nano_marf::CHECKPOINT_BLOCK_FILE,
        "checkpoint.attesting_block",
    )?;
    require_checkpoint_file(
        bundle,
        config
            .checkpoint
            .attesting_reward_set
            .as_deref()
            .ok_or("a signed checkpoint has no configured attesting reward set")?,
        CHECKPOINT_REWARD_SET_FILE,
        "checkpoint.attesting_reward_set",
    )?;
    for (name, path) in [
        (
            "checkpoint.anchor_block",
            Some(config.checkpoint.anchor_block.as_path()),
        ),
        (
            "checkpoint.tenure_accounting",
            config.checkpoint.tenure_accounting.as_deref(),
        ),
        (
            "checkpoint.sortition",
            config.checkpoint.sortition.as_deref(),
        ),
        (
            "checkpoint.authentication_history",
            config.checkpoint.authentication_history.as_deref(),
        ),
    ] {
        if let Some(path) = path {
            require_path_in_bundle(bundle, path, name)?;
        }
    }
    Ok(())
}

fn require_checkpoint_file(
    bundle: &Path,
    configured: &Path,
    relative: &str,
    name: &str,
) -> Result<(), Box<dyn Error>> {
    let expected = bundle.join(relative);
    if fs::canonicalize(configured)? != fs::canonicalize(&expected)? {
        return Err(format!(
            "{name} names {}, not the verified bundle file {}",
            configured.display(),
            expected.display()
        )
        .into());
    }
    Ok(())
}

fn require_path_in_bundle(
    bundle: &Path,
    configured: &Path,
    name: &str,
) -> Result<(), Box<dyn Error>> {
    let bundle = fs::canonicalize(bundle)?;
    let configured = fs::canonicalize(configured)?;
    if configured == bundle || !configured.starts_with(&bundle) {
        return Err(format!(
            "{name} at {} is outside the verified checkpoint bundle",
            configured.display()
        )
        .into());
    }
    Ok(())
}

fn adopt_attested(
    config: &Config,
    directory: &Path,
    source: [u8; 32],
    bundle: Option<CheckpointBundleReceipt>,
) -> Result<(), Box<dyn Error>> {
    let manifest = CheckpointManifest::load(
        config
            .checkpoint
            .marf
            .parent()
            .ok_or("the checkpoint has no directory")?,
    )?;
    if config.network().is_some_and(Network::is_mainnet) {
        manifest.check_profile(nano_vm::compatibility_profile_fingerprint())?;
    }
    if manifest.source_state_id != source {
        return Err(format!(
            "the checkpoint names state {} where this follower is configured for {}",
            hex::encode(manifest.source_state_id),
            hex::encode(source)
        )
        .into());
    }
    if let Some(recorded) = CheckpointProvenance::load(directory)? {
        already_adopted(
            recorded.checkpoint.source_state_id,
            manifest.source_state_id,
        )?;
        return Ok(());
    }

    let (Some(block), Some(reward_set)) = (
        config.checkpoint.attesting_block.as_ref(),
        config.checkpoint.attesting_reward_set.as_ref(),
    ) else {
        return Err(
            "a checkpoint needs an attesting block and the independently sourced reward set that signed it"
                .into(),
        );
    };
    let block = NakamotoBlock::decode(&fs::read(block)?)?;
    let signers = attesting_reward_set(&fs::read(reward_set)?)?;
    let attestation = if let Some(bundle) = bundle {
        adopt_checkpoint_bundle(directory, &manifest, &block.header, &signers, bundle)?
    } else {
        adopt_checkpoint(directory, &manifest, &block.header, &signers)?
    };
    println!(
        "checkpoint {} attested by {} of {} signer weight",
        hex::encode(manifest.source_state_id),
        attestation.signer_weight,
        attestation.approval_threshold
    );
    Ok(())
}

fn already_adopted(recorded: [u8; 32], configured: [u8; 32]) -> Result<(), String> {
    if recorded == configured {
        Ok(())
    } else {
        Err(format!(
            "this state descends from checkpoint {} and cannot be reused for {}",
            hex::encode(recorded),
            hex::encode(configured)
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use nano_crypto::StacksPrivateKey;

    use super::{adopt_signed, already_adopted};
    use crate::{
        CheckpointManifest, CheckpointProvenance,
        checkpoint_bundle::{
            CHECKPOINT_REWARD_SET_FILE, build_checkpoint_bundle_manifest,
            test_support::{TestBitcoin, fixture},
        },
        checkpoint_signatures::{BuilderPolicy, sign_checkpoint_bundle},
        config::Config,
    };

    fn builder_evidence(
        bundle: &Path,
        evidence: &Path,
        bitcoin: &TestBitcoin,
        builders: &[StacksPrivateKey; 2],
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let policy_path = evidence.join("builders.toml");
        fs::write(
            &policy_path,
            format!(
                "schema = \"nano-stacks/checkpoint-builder-policy/v1\"\n\
                 required_signatures = 2\n\
                 [[builders]]\n\
                 name = \"archive-east\"\n\
                 public_key = \"{}\"\n\
                 valid_from_height = 0\n\
                 [[builders]]\n\
                 name = \"archive-west\"\n\
                 public_key = \"{}\"\n\
                 valid_from_height = 0\n",
                hex::encode(builders[0].public_key().to_bytes_compressed()),
                hex::encode(builders[1].public_key().to_bytes_compressed())
            ),
        )
        .expect("builder policy");
        let policy = BuilderPolicy::load(&policy_path).expect("load builder policy");
        let signatures = evidence.join("signatures");
        for (name, builder) in [
            ("archive-east", &builders[0]),
            ("archive-west", &builders[1]),
        ] {
            sign_checkpoint_bundle(bundle, bitcoin, &policy, &signatures, name, builder)
                .expect("builder signature");
        }
        (policy_path, signatures)
    }

    fn config(
        bundle: &Path,
        state: &Path,
        policy: &Path,
        signatures: &Path,
        manifest: &CheckpointManifest,
    ) -> Config {
        Config::parse(&format!(
            r#"
[follower]
working_dir = "{}"
network = "mainnet"
peers = []

[burnchain]
rpc_url = "http://127.0.0.1:18443"
rpc_user = "bitcoin"
rpc_password = "bitcoin"

[checkpoint]
bundle = "{}"
builder_policy = "{}"
builder_signatures = "{}"
marf = "{}"
source_state_id = "{}"
state_root = "{}"
anchor_block = "{}"
anchor_bitcoin_height = 11
attesting_block = "{}"
attesting_reward_set = "{}"
"#,
            state.display(),
            bundle.display(),
            policy.display(),
            signatures.display(),
            bundle.join("marf.sqlite").display(),
            hex::encode(manifest.source_state_id),
            manifest.state_index_root,
            bundle.join("anchor.bin").display(),
            bundle.join(nano_marf::CHECKPOINT_BLOCK_FILE).display(),
            bundle.join(CHECKPOINT_REWARD_SET_FILE).display(),
        ))
        .expect("signed checkpoint config")
    }

    #[test]
    fn a_state_directory_belongs_to_one_checkpoint() {
        already_adopted([1; 32], [1; 32]).expect("same checkpoint");
        let error = already_adopted([1; 32], [2; 32]).expect_err("different checkpoint");
        assert!(error.contains("descends from checkpoint"));
    }

    #[test]
    fn signed_adoption_is_checked_before_state_is_touched_and_on_every_restart() {
        let (bundle, builders) = fixture(true);
        let bitcoin = TestBitcoin(11, [6; 32]);
        build_checkpoint_bundle_manifest(bundle.path(), &bitcoin).expect("content manifest");
        let evidence = tempfile::tempdir().expect("external evidence");
        let (policy, signatures) =
            builder_evidence(bundle.path(), evidence.path(), &bitcoin, &builders);
        let manifest = CheckpointManifest::load(bundle.path()).expect("checkpoint manifest");
        let state = evidence.path().join("state");
        let config = config(bundle.path(), &state, &policy, &signatures, &manifest);

        let wrong = evidence.path().join("wrong-view");
        let error = adopt_signed(
            &config,
            &wrong,
            manifest.source_state_id,
            &TestBitcoin(11, [7; 32]),
            bundle.path(),
            &policy,
            &signatures,
        )
        .expect_err("wrong Bitcoin view");
        assert!(error.to_string().contains("not locally canonical"));
        assert!(!wrong.exists());

        let mut escaped = config.clone();
        escaped.checkpoint.anchor_block = policy.clone();
        let error = adopt_signed(
            &escaped,
            &state,
            manifest.source_state_id,
            &bitcoin,
            bundle.path(),
            &policy,
            &signatures,
        )
        .expect_err("path outside bundle");
        assert!(
            error
                .to_string()
                .contains("outside the verified checkpoint bundle")
        );
        assert!(!state.exists());

        adopt_signed(
            &config,
            &state,
            manifest.source_state_id,
            &bitcoin,
            bundle.path(),
            &policy,
            &signatures,
        )
        .expect("adopt signed checkpoint");
        let provenance = CheckpointProvenance::load(&state)
            .expect("provenance")
            .expect("recorded provenance");
        let receipt = provenance.bundle.expect("bundle receipt");
        assert_eq!(receipt.bitcoin_block_hash, [6; 32]);
        assert_eq!(receipt.builders, ["archive-east", "archive-west"]);

        fs::remove_file(bundle.path().join("marf.sqlite")).expect("discard bundle payload");
        adopt_signed(
            &config,
            &state,
            manifest.source_state_id,
            &bitcoin,
            bundle.path(),
            &policy,
            &signatures,
        )
        .expect("bounded restart from receipt");
        let error = adopt_signed(
            &config,
            &state,
            manifest.source_state_id,
            &TestBitcoin(11, [7; 32]),
            bundle.path(),
            &policy,
            &signatures,
        )
        .expect_err("changed Bitcoin view");
        assert!(error.to_string().contains("no longer locally canonical"));
    }
}
