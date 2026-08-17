//! Independent builder signatures over a checkpoint content root.

use std::{
    collections::BTreeSet,
    fmt::Display,
    fs::{self, File},
    io::Write as _,
    path::{Path, PathBuf},
};

use nano_bitcoin::BitcoinSource;
use nano_crypto::{CryptoError, MessageSignature, StacksPrivateKey, StacksPublicKey};
use nano_primitives::sha256;
use serde::{Deserialize, Serialize};

use crate::checkpoint_bundle::{CheckpointBundleError, verify_checkpoint_bundle};

const POLICY_SCHEMA: &str = "nano-stacks/checkpoint-builder-policy/v1";
const SIGNATURE_SCHEMA: &str = "nano-stacks/checkpoint-builder-signature/v1";
const SIGNATURE_DOMAIN: &[u8] = b"nano-stacks/checkpoint-builder-signature/v1\0";

/// The locally trusted builders and the number that must agree.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuilderPolicy {
    schema: String,
    required_signatures: usize,
    builders: Vec<BuilderKey>,
}

/// One builder key's explicit validity and revocation interval.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuilderKey {
    name: String,
    public_key: String,
    valid_from_height: u64,
    valid_through_height: Option<u64>,
    revoked_from_height: Option<u64>,
}

/// One immutable builder statement about one checkpoint content root.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct BuilderSignature {
    schema: String,
    builder: String,
    content_root: String,
    signature: String,
}

/// Builders whose locally pinned keys authenticated a checkpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedBuilders {
    pub content_root: String,
    pub names: Vec<String>,
}

impl BuilderPolicy {
    /// Read and validate a locally selected builder policy.
    ///
    /// # Errors
    ///
    /// Returns an error for unreadable TOML, an unsupported schema, malformed
    /// keys, overlapping rotations, duplicate keys, or an impossible threshold.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, BuilderSignatureError> {
        let policy: Self = toml::from_str(&fs::read_to_string(path)?)
            .map_err(|error| BuilderSignatureError::Invalid(error.to_string()))?;
        policy.validate()?;
        Ok(policy)
    }

    fn validate(&self) -> Result<(), BuilderSignatureError> {
        if self.schema != POLICY_SCHEMA {
            return Err(BuilderSignatureError::Invalid(format!(
                "unsupported builder policy schema {}",
                self.schema
            )));
        }
        if self.required_signatures == 0 {
            return Err(BuilderSignatureError::Invalid(
                "builder signature threshold is zero".to_owned(),
            ));
        }
        let mut prior: Option<(&str, u64, Option<u64>)> = None;
        let mut keys = BTreeSet::new();
        for builder in &self.builders {
            validate_builder_name(&builder.name)?;
            let key = parse_public_key(&builder.public_key)?;
            if key.to_bytes_compressed().as_slice()
                != hex::decode(&builder.public_key)
                    .map_err(|error| BuilderSignatureError::Invalid(error.to_string()))?
            {
                return Err(BuilderSignatureError::Invalid(format!(
                    "builder {} does not use a compressed public key",
                    builder.name
                )));
            }
            if !keys.insert(builder.public_key.as_str()) {
                return Err(BuilderSignatureError::Invalid(
                    "one builder key is assigned more than once".to_owned(),
                ));
            }
            let effective_end = builder.effective_end()?;
            if let Some((prior_name, prior_start, prior_end)) = prior {
                if (prior_name, prior_start) >= (builder.name.as_str(), builder.valid_from_height) {
                    return Err(BuilderSignatureError::Invalid(
                        "builder policy entries are duplicated or not sorted".to_owned(),
                    ));
                }
                if prior_name == builder.name
                    && prior_end.is_none_or(|end| end >= builder.valid_from_height)
                {
                    return Err(BuilderSignatureError::Invalid(format!(
                        "builder {} has overlapping key intervals",
                        builder.name
                    )));
                }
            }
            prior = Some((&builder.name, builder.valid_from_height, effective_end));
        }
        let distinct = self
            .builders
            .iter()
            .map(|builder| builder.name.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        if self.required_signatures > distinct {
            return Err(BuilderSignatureError::Invalid(format!(
                "builder threshold {} exceeds {distinct} distinct builders",
                self.required_signatures
            )));
        }
        Ok(())
    }

    fn active_key(
        &self,
        builder_name: &str,
        height: u64,
    ) -> Result<StacksPublicKey, BuilderSignatureError> {
        self.builders
            .iter()
            .find(|builder| builder.name == builder_name && builder.active_at(height))
            .map(|builder| parse_public_key(&builder.public_key))
            .transpose()?
            .ok_or_else(|| {
                BuilderSignatureError::Invalid(format!(
                    "builder {builder_name} has no active key at height {height}"
                ))
            })
    }

    fn active_builders(&self, height: u64) -> usize {
        self.builders
            .iter()
            .filter(|builder| builder.active_at(height))
            .map(|builder| builder.name.as_str())
            .collect::<BTreeSet<_>>()
            .len()
    }
}

impl BuilderKey {
    fn effective_end(&self) -> Result<Option<u64>, BuilderSignatureError> {
        if self
            .valid_through_height
            .is_some_and(|end| end < self.valid_from_height)
        {
            return Err(BuilderSignatureError::Invalid(format!(
                "builder {} ends before its key becomes valid",
                self.name
            )));
        }
        if self
            .revoked_from_height
            .is_some_and(|revoked| revoked <= self.valid_from_height)
        {
            return Err(BuilderSignatureError::Invalid(format!(
                "builder {} is revoked before its key becomes valid",
                self.name
            )));
        }
        let before_revocation = self
            .revoked_from_height
            .map(|revoked| revoked.saturating_sub(1));
        Ok(match (self.valid_through_height, before_revocation) {
            (Some(valid), Some(revoked)) => Some(valid.min(revoked)),
            (Some(valid), None) => Some(valid),
            (None, Some(revoked)) => Some(revoked),
            (None, None) => None,
        })
    }

    fn active_at(&self, height: u64) -> bool {
        height >= self.valid_from_height
            && self
                .valid_through_height
                .is_none_or(|through| height <= through)
            && self
                .revoked_from_height
                .is_none_or(|revoked| height < revoked)
    }
}

/// Sign a verified bundle with the key pinned for `builder` at its height.
///
/// The signature file is created once and never replaced.
///
/// # Errors
///
/// Returns an error if the bundle or policy is invalid, the private key is not
/// the pinned active key, or the signature path already exists.
pub fn sign_checkpoint_bundle<S: BitcoinSource>(
    bundle: impl AsRef<Path>,
    bitcoin: &S,
    policy: &BuilderPolicy,
    signatures: impl AsRef<Path>,
    builder: &str,
    private_key: &StacksPrivateKey,
) -> Result<PathBuf, BuilderSignatureError>
where
    S::Error: Display,
{
    let manifest = verify_checkpoint_bundle(bundle, bitcoin)?;
    let expected = policy.active_key(builder, manifest.checkpoint.checkpoint_stacks_height)?;
    if private_key.public_key() != expected {
        return Err(BuilderSignatureError::Invalid(format!(
            "the private key is not the active key for builder {builder}"
        )));
    }
    let signatures = signatures.as_ref();
    prepare_signature_directory(signatures)?;
    let statement = BuilderSignature {
        schema: SIGNATURE_SCHEMA.to_owned(),
        builder: builder.to_owned(),
        content_root: manifest.content_root().to_owned(),
        signature: hex::encode(
            private_key
                .sign(&signature_digest(manifest.content_root())?)
                .as_bytes(),
        ),
    };
    let path = signatures.join(format!("{builder}.toml"));
    let bytes = toml::to_string(&statement)
        .map_err(|error| BuilderSignatureError::Invalid(error.to_string()))?;
    let mut output = File::options().write(true).create_new(true).open(&path)?;
    output.write_all(bytes.as_bytes())?;
    output.sync_all()?;
    File::open(signatures)?.sync_all()?;
    Ok(path)
}

/// Verify a bundle and its independent builder signatures against local policy.
///
/// # Errors
///
/// Returns an error before import for any bundle mismatch, unknown or inactive
/// builder, malformed signature, changed root, or unmet threshold.
pub fn verify_signed_checkpoint_bundle<S: BitcoinSource>(
    bundle: impl AsRef<Path>,
    bitcoin: &S,
    policy: &BuilderPolicy,
    signatures: impl AsRef<Path>,
) -> Result<VerifiedBuilders, BuilderSignatureError>
where
    S::Error: Display,
{
    let manifest = verify_checkpoint_bundle(bundle, bitcoin)?;
    verify_builder_signatures(
        manifest.content_root(),
        manifest.checkpoint.checkpoint_stacks_height,
        policy,
        signatures,
    )
}

/// Verify signatures over an already authenticated content root.
///
/// This is the bounded restart path: first import persisted the root after
/// hashing every payload byte, so a restart only rechecks external policy and
/// signatures rather than reading the discarded import source again.
///
/// # Errors
///
/// Returns an error for an invalid policy, inactive or unknown builder,
/// malformed signature, changed content root, or unmet threshold.
pub fn verify_builder_signatures(
    content_root: &str,
    height: u64,
    policy: &BuilderPolicy,
    signatures: impl AsRef<Path>,
) -> Result<VerifiedBuilders, BuilderSignatureError> {
    policy.validate()?;
    let active = policy.active_builders(height);
    if active < policy.required_signatures {
        return Err(BuilderSignatureError::Invalid(format!(
            "only {active} builders are active at height {height}, below threshold {}",
            policy.required_signatures
        )));
    }
    let entries = signature_files(signatures.as_ref())?;
    let digest = signature_digest(content_root)?;
    let mut names = Vec::new();
    for path in entries {
        let statement: BuilderSignature = toml::from_str(&fs::read_to_string(&path)?)
            .map_err(|error| BuilderSignatureError::Invalid(error.to_string()))?;
        statement.validate(content_root, &path)?;
        let key = policy.active_key(&statement.builder, height)?;
        let signature = parse_signature(&statement.signature)?;
        key.verify_transaction(&digest, &signature)?;
        names.push(statement.builder);
    }
    names.sort();
    if names.len() < policy.required_signatures {
        return Err(BuilderSignatureError::Invalid(format!(
            "{} valid builder signatures do not reach threshold {}",
            names.len(),
            policy.required_signatures
        )));
    }
    Ok(VerifiedBuilders {
        content_root: content_root.to_owned(),
        names,
    })
}

impl BuilderSignature {
    fn validate(&self, root: &str, path: &Path) -> Result<(), BuilderSignatureError> {
        if self.schema != SIGNATURE_SCHEMA {
            return Err(BuilderSignatureError::Invalid(format!(
                "unsupported builder signature schema {}",
                self.schema
            )));
        }
        validate_builder_name(&self.builder)?;
        if self.content_root != root {
            return Err(BuilderSignatureError::Invalid(format!(
                "builder {} signed content root {} instead of {root}",
                self.builder, self.content_root
            )));
        }
        if path.file_name().and_then(|name| name.to_str())
            != Some(format!("{}.toml", self.builder).as_str())
        {
            return Err(BuilderSignatureError::Invalid(format!(
                "builder signature {} has a noncanonical file name",
                path.display()
            )));
        }
        Ok(())
    }
}

fn prepare_signature_directory(path: &Path) -> Result<(), BuilderSignatureError> {
    if !path.exists() {
        fs::create_dir(path)?;
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(BuilderSignatureError::Invalid(format!(
            "{} is not a signature directory",
            path.display()
        )));
    }
    Ok(())
}

fn signature_files(directory: &Path) -> Result<Vec<PathBuf>, BuilderSignatureError> {
    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(BuilderSignatureError::Invalid(format!(
            "{} is not a signature directory",
            directory.display()
        )));
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if !kind.is_file() || kind.is_symlink() {
            return Err(BuilderSignatureError::Invalid(format!(
                "signature entry {} is not a regular file",
                entry.path().display()
            )));
        }
        paths.push(entry.path());
    }
    paths.sort();
    Ok(paths)
}

fn validate_builder_name(name: &str) -> Result<(), BuilderSignatureError> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_".contains(&byte))
        || !name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
    {
        return Err(BuilderSignatureError::Invalid(format!(
            "builder name {name:?} is not canonical"
        )));
    }
    Ok(())
}

fn parse_public_key(value: &str) -> Result<StacksPublicKey, BuilderSignatureError> {
    if value.len() != 66
        || value
            .bytes()
            .any(|byte| !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase())
    {
        return Err(BuilderSignatureError::Invalid(
            "builder public key is not 33-byte lowercase hexadecimal".to_owned(),
        ));
    }
    StacksPublicKey::from_bytes(
        &hex::decode(value).map_err(|error| BuilderSignatureError::Invalid(error.to_string()))?,
    )
    .map_err(Into::into)
}

fn parse_signature(value: &str) -> Result<MessageSignature, BuilderSignatureError> {
    let bytes =
        hex::decode(value).map_err(|error| BuilderSignatureError::Invalid(error.to_string()))?;
    let bytes: [u8; 65] = bytes.try_into().map_err(|_| {
        BuilderSignatureError::Invalid("builder signature is not 65-byte hexadecimal".to_owned())
    })?;
    Ok(MessageSignature::from_bytes(bytes))
}

fn signature_digest(root: &str) -> Result<[u8; 32], BuilderSignatureError> {
    let root =
        hex::decode(root).map_err(|error| BuilderSignatureError::Invalid(error.to_string()))?;
    if root.len() != 32 {
        return Err(BuilderSignatureError::Invalid(
            "checkpoint content root is not 32 bytes".to_owned(),
        ));
    }
    Ok(*sha256(&[SIGNATURE_DOMAIN, root.as_slice()].concat()).as_bytes())
}

/// Why independent builder authentication failed.
#[derive(Debug)]
pub enum BuilderSignatureError {
    Io(std::io::Error),
    Bundle(CheckpointBundleError),
    Crypto(CryptoError),
    Invalid(String),
}

impl std::fmt::Display for BuilderSignatureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "builder signature I/O failed: {error}"),
            Self::Bundle(error) => write!(formatter, "{error}"),
            Self::Crypto(error) => write!(formatter, "invalid builder signature: {error}"),
            Self::Invalid(reason) => write!(formatter, "invalid builder policy: {reason}"),
        }
    }
}

impl std::error::Error for BuilderSignatureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Bundle(error) => Some(error),
            Self::Crypto(error) => Some(error),
            Self::Invalid(_) => None,
        }
    }
}

impl From<std::io::Error> for BuilderSignatureError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<CheckpointBundleError> for BuilderSignatureError {
    fn from(error: CheckpointBundleError) -> Self {
        Self::Bundle(error)
    }
}

impl From<CryptoError> for BuilderSignatureError {
    fn from(error: CryptoError) -> Self {
        Self::Crypto(error)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use nano_crypto::StacksPrivateKey;

    use super::{
        BuilderKey, BuilderPolicy, BuilderSignatureError, POLICY_SCHEMA, sign_checkpoint_bundle,
        verify_signed_checkpoint_bundle,
    };
    use crate::{
        checkpoint_bundle::{
            build_checkpoint_bundle_manifest,
            tests::{TestBitcoin, fixture},
        },
        checkpoint_signatures::BuilderSignature,
    };

    const BITCOIN: TestBitcoin = TestBitcoin(11, [6; 32]);

    fn key(
        name: &str,
        private_key: &StacksPrivateKey,
        valid_from_height: u64,
        valid_through_height: Option<u64>,
        revoked_from_height: Option<u64>,
    ) -> BuilderKey {
        BuilderKey {
            name: name.to_owned(),
            public_key: hex::encode(private_key.public_key().to_bytes_compressed()),
            valid_from_height,
            valid_through_height,
            revoked_from_height,
        }
    }

    fn policy(required_signatures: usize, builders: Vec<BuilderKey>) -> BuilderPolicy {
        BuilderPolicy {
            schema: POLICY_SCHEMA.to_owned(),
            required_signatures,
            builders,
        }
    }

    #[test]
    fn two_pinned_builders_sign_one_content_root_append_only() {
        let (bundle, [first, second]) = fixture(true);
        let manifest =
            build_checkpoint_bundle_manifest(bundle.path(), &BITCOIN).expect("content manifest");
        let signatures = tempfile::tempdir().expect("signature directory");
        let policy = policy(
            2,
            vec![
                key("archive-east", &first, 0, None, None),
                key("archive-west", &second, 0, None, None),
            ],
        );
        sign_checkpoint_bundle(
            bundle.path(),
            &BITCOIN,
            &policy,
            signatures.path(),
            "archive-east",
            &first,
        )
        .expect("first builder");
        sign_checkpoint_bundle(
            bundle.path(),
            &BITCOIN,
            &policy,
            signatures.path(),
            "archive-west",
            &second,
        )
        .expect("second builder");
        let verified =
            verify_signed_checkpoint_bundle(bundle.path(), &BITCOIN, &policy, signatures.path())
                .expect("builder threshold");
        assert_eq!(verified.content_root, manifest.content_root());
        assert_eq!(verified.names, ["archive-east", "archive-west"]);
        assert!(matches!(
            sign_checkpoint_bundle(
                bundle.path(),
                &BITCOIN,
                &policy,
                signatures.path(),
                "archive-east",
                &first,
            ),
            Err(BuilderSignatureError::Io(error))
                if error.kind() == std::io::ErrorKind::AlreadyExists
        ));
    }

    #[test]
    fn a_bundle_cannot_declare_its_own_builder_or_bypass_the_threshold() {
        let (bundle, [first, second]) = fixture(true);
        build_checkpoint_bundle_manifest(bundle.path(), &BITCOIN).expect("content manifest");
        let signatures = tempfile::tempdir().expect("signature directory");
        let trusted = policy(
            2,
            vec![
                key("archive-east", &first, 0, None, None),
                key("archive-west", &second, 0, None, None),
            ],
        );
        sign_checkpoint_bundle(
            bundle.path(),
            &BITCOIN,
            &trusted,
            signatures.path(),
            "archive-east",
            &first,
        )
        .expect("one trusted builder");
        assert!(matches!(
            verify_signed_checkpoint_bundle(
                bundle.path(),
                &BITCOIN,
                &trusted,
                signatures.path()
            ),
            Err(BuilderSignatureError::Invalid(reason)) if reason.contains("threshold 2")
        ));

        let attacker = StacksPrivateKey::from_seed(b"checkpoint attacker");
        let attacker_policy = policy(1, vec![key("attacker", &attacker, 0, None, None)]);
        let attacker_signatures = tempfile::tempdir().expect("attacker signatures");
        sign_checkpoint_bundle(
            bundle.path(),
            &BITCOIN,
            &attacker_policy,
            attacker_signatures.path(),
            "attacker",
            &attacker,
        )
        .expect("self-declared attacker signature");
        assert!(matches!(
            verify_signed_checkpoint_bundle(
                bundle.path(),
                &BITCOIN,
                &trusted,
                attacker_signatures.path()
            ),
            Err(BuilderSignatureError::Invalid(reason))
                if reason.contains("no active key")
        ));
    }

    #[test]
    fn rotation_and_revocation_are_decided_at_the_checkpoint_height() {
        let (bundle, [old, peer]) = fixture(true);
        build_checkpoint_bundle_manifest(bundle.path(), &BITCOIN).expect("content manifest");
        let replacement = StacksPrivateKey::from_seed(b"replacement checkpoint builder");
        let policy = policy(
            2,
            vec![
                key("archive", &old, 0, None, Some(7)),
                key("archive", &replacement, 7, None, None),
                key("peer", &peer, 0, None, Some(8)),
            ],
        );
        policy.validate().expect("rotation policy");
        let signatures = tempfile::tempdir().expect("signature directory");
        assert!(matches!(
            sign_checkpoint_bundle(
                bundle.path(),
                &BITCOIN,
                &policy,
                signatures.path(),
                "archive",
                &old,
            ),
            Err(BuilderSignatureError::Invalid(reason))
                if reason.contains("not the active key")
        ));
        sign_checkpoint_bundle(
            bundle.path(),
            &BITCOIN,
            &policy,
            signatures.path(),
            "archive",
            &replacement,
        )
        .expect("rotated builder");
        sign_checkpoint_bundle(
            bundle.path(),
            &BITCOIN,
            &policy,
            signatures.path(),
            "peer",
            &peer,
        )
        .expect("peer before revocation");
        verify_signed_checkpoint_bundle(bundle.path(), &BITCOIN, &policy, signatures.path())
            .expect("height seven policy");

        let mut revoked = policy;
        revoked.builders[2].revoked_from_height = Some(7);
        assert!(matches!(
            verify_signed_checkpoint_bundle(
                bundle.path(),
                &BITCOIN,
                &revoked,
                signatures.path()
            ),
            Err(BuilderSignatureError::Invalid(reason))
                if reason.contains("only 1 builders are active")
        ));
    }

    #[test]
    fn changed_signature_bytes_and_noncanonical_entries_are_refused() {
        let (bundle, [first, _]) = fixture(true);
        build_checkpoint_bundle_manifest(bundle.path(), &BITCOIN).expect("content manifest");
        let policy = policy(1, vec![key("archive", &first, 0, None, None)]);
        let signatures = tempfile::tempdir().expect("signature directory");
        let path = sign_checkpoint_bundle(
            bundle.path(),
            &BITCOIN,
            &policy,
            signatures.path(),
            "archive",
            &first,
        )
        .expect("builder signature");
        let mut statement: BuilderSignature =
            toml::from_str(&fs::read_to_string(&path).expect("signature bytes"))
                .expect("signature statement");
        statement.signature.replace_range(2..4, "ff");
        fs::write(
            &path,
            toml::to_string(&statement).expect("changed signature"),
        )
        .expect("replace test signature");
        assert!(matches!(
            verify_signed_checkpoint_bundle(bundle.path(), &BITCOIN, &policy, signatures.path()),
            Err(BuilderSignatureError::Crypto(_))
        ));
        fs::create_dir(signatures.path().join("nested")).expect("noncanonical entry");
        assert!(matches!(
            verify_signed_checkpoint_bundle(
                bundle.path(),
                &BITCOIN,
                &policy,
                signatures.path()
            ),
            Err(BuilderSignatureError::Invalid(reason))
                if reason.contains("not a regular file")
        ));
    }
}
