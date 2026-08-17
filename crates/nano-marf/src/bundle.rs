//! A deterministic, content-addressed inventory of one checkpoint bundle.

use std::{
    fs::{self, File},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
};

use nano_primitives::sha256;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::CheckpointManifest;

/// The unsigned manifest every checkpoint payload file is bound into.
pub const BUNDLE_MANIFEST_FILE: &str = "checkpoint-bundle.toml";
const BUNDLE_SCHEMA: &str = "nano-stacks/checkpoint-bundle/v1";
const CHUNK_BYTES: usize = 4 * 1024 * 1024;
const CONTENT_DOMAIN: &[u8] = b"nano-stacks/checkpoint-content/v1\0";

/// Consensus and attestation claims bound into a checkpoint's content root.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BundleClaims {
    pub state_format: String,
    pub checkpoint_stacks_height: u64,
    pub source_state_id: String,
    pub published_state_index_root: String,
    pub bitcoin_height: u64,
    pub bitcoin_block_hash: String,
    pub attesting_block_id: String,
    pub signer_weight: u32,
    pub approval_threshold: u32,
    pub semantic_epoch: String,
    pub compiler_identity: String,
    pub profile_fingerprint: String,
}

impl BundleClaims {
    fn validate(&self) -> Result<(), BundleError> {
        if self.state_format.is_empty() {
            return Err(BundleError::Invalid("state format is empty".to_owned()));
        }
        for (name, value) in [
            ("source_state_id", self.source_state_id.as_str()),
            (
                "published_state_index_root",
                self.published_state_index_root.as_str(),
            ),
            ("bitcoin_block_hash", self.bitcoin_block_hash.as_str()),
            ("attesting_block_id", self.attesting_block_id.as_str()),
            ("profile_fingerprint", self.profile_fingerprint.as_str()),
        ] {
            fixed_hex(name, value, 32)?;
        }
        if self.attesting_block_id != self.source_state_id {
            return Err(BundleError::Invalid(
                "the attesting block is not the checkpoint state ID".to_owned(),
            ));
        }
        if self.approval_threshold == 0 || self.signer_weight < self.approval_threshold {
            return Err(BundleError::Invalid(
                "the signer proof does not reach a nonzero approval threshold".to_owned(),
            ));
        }
        if self.semantic_epoch != "Epoch40" {
            return Err(BundleError::Invalid(format!(
                "unsupported semantic epoch {}",
                self.semantic_epoch
            )));
        }
        let Some(compiler) = self.compiler_identity.strip_prefix("sha256:") else {
            return Err(BundleError::Invalid(
                "compiler identity is not a SHA-256 digest".to_owned(),
            ));
        };
        fixed_hex("compiler_identity", compiler, 32)
    }

    fn check_checkpoint(&self, checkpoint: &CheckpointManifest) -> Result<(), BundleError> {
        if self.state_format != checkpoint.format
            || self.checkpoint_stacks_height != checkpoint.stacks_height
            || self.source_state_id != hex::encode(checkpoint.source_state_id)
            || self.published_state_index_root != checkpoint.state_index_root.to_string()
            || self.bitcoin_height != checkpoint.first_bitcoin_height
            || checkpoint
                .profile_fingerprint
                .map(|value| value.to_string())
                .as_deref()
                != Some(self.profile_fingerprint.as_str())
        {
            return Err(BundleError::Invalid(
                "bundle claims differ from checkpoint.toml".to_owned(),
            ));
        }
        Ok(())
    }
}

/// One regular payload file and every fixed-size piece of it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct BundleFile {
    path: String,
    size: u64,
    sha256: String,
    chunks: Vec<String>,
}

/// The complete unsigned checkpoint inventory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointBundleManifest {
    schema: String,
    chunk_bytes: u64,
    content_root: String,
    pub checkpoint: BundleClaims,
    files: Vec<BundleFile>,
}

impl CheckpointBundleManifest {
    /// Build a deterministic manifest from every regular file under `directory`.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid claim, non-portable path, symlink,
    /// special file, changing file, or unreadable checkpoint.
    pub fn build(
        directory: impl AsRef<Path>,
        checkpoint: BundleClaims,
    ) -> Result<Self, BundleError> {
        let directory = directory.as_ref();
        checkpoint.validate()?;
        checkpoint.check_checkpoint(&CheckpointManifest::load(directory)?)?;
        let files = digest_files(directory)?;
        Ok(Self {
            schema: BUNDLE_SCHEMA.to_owned(),
            chunk_bytes: CHUNK_BYTES as u64,
            content_root: content_root(&checkpoint, &files),
            checkpoint,
            files,
        })
    }

    /// Write one new manifest without replacing an existing trust root.
    ///
    /// # Errors
    ///
    /// Returns an error when the bundle is invalid, the manifest already
    /// exists, or the file cannot be made durable.
    pub fn write_new(
        directory: impl AsRef<Path>,
        checkpoint: BundleClaims,
    ) -> Result<Self, BundleError> {
        let directory = directory.as_ref();
        let path = directory.join(BUNDLE_MANIFEST_FILE);
        let manifest = Self::build(directory, checkpoint)?;
        let contents =
            toml::to_string(&manifest).map_err(|error| BundleError::Invalid(error.to_string()))?;
        let mut output = File::options().write(true).create_new(true).open(&path)?;
        output.write_all(contents.as_bytes())?;
        output.sync_all()?;
        File::open(directory)?.sync_all()?;
        Ok(manifest)
    }

    /// Load a manifest without trusting any payload bytes yet.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing, non-regular, symlinked, or malformed
    /// manifest.
    pub fn load(directory: impl AsRef<Path>) -> Result<Self, BundleError> {
        let path = directory.as_ref().join(BUNDLE_MANIFEST_FILE);
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(BundleError::Invalid(format!(
                "{} is not a regular manifest file",
                path.display()
            )));
        }
        toml::from_str(&fs::read_to_string(path)?)
            .map_err(|error| BundleError::Invalid(error.to_string()))
    }

    /// Verify every payload byte and reject any unlisted payload file.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed claims, a mismatch with
    /// `checkpoint.toml`, missing/extra files, or any changed byte or size.
    pub fn verify(directory: impl AsRef<Path>) -> Result<Self, BundleError> {
        let directory = directory.as_ref();
        let manifest = Self::load(directory)?;
        manifest.validate()?;
        manifest
            .checkpoint
            .check_checkpoint(&CheckpointManifest::load(directory)?)?;
        let actual = digest_files(directory)?;
        if actual != manifest.files {
            return Err(describe_difference(&manifest.files, &actual));
        }
        Ok(manifest)
    }

    /// The content address builder signatures bind to.
    #[must_use]
    pub fn content_root(&self) -> &str {
        &self.content_root
    }

    fn validate(&self) -> Result<(), BundleError> {
        if self.schema != BUNDLE_SCHEMA {
            return Err(BundleError::Invalid(format!(
                "unsupported bundle schema {}",
                self.schema
            )));
        }
        if self.chunk_bytes != CHUNK_BYTES as u64 {
            return Err(BundleError::Invalid(format!(
                "bundle uses {}-byte chunks instead of {CHUNK_BYTES}",
                self.chunk_bytes
            )));
        }
        self.checkpoint.validate()?;
        let mut previous = None;
        for file in &self.files {
            validate_path(&file.path)?;
            if previous.is_some_and(|prior| prior >= file.path.as_str()) {
                return Err(BundleError::Invalid(
                    "bundle file paths are duplicated or not sorted".to_owned(),
                ));
            }
            previous = Some(file.path.as_str());
            fixed_hex("file sha256", &file.sha256, 32)?;
            for chunk in &file.chunks {
                fixed_hex("chunk sha256", chunk, 32)?;
            }
            let expected_chunks = file.size.div_ceil(CHUNK_BYTES as u64);
            if file.chunks.len() as u64 != expected_chunks {
                return Err(BundleError::Invalid(format!(
                    "{} has {} chunk hashes for {} bytes",
                    file.path,
                    file.chunks.len(),
                    file.size
                )));
            }
        }
        let actual = content_root(&self.checkpoint, &self.files);
        if actual != self.content_root {
            return Err(BundleError::Invalid(format!(
                "content root {} does not match {actual}",
                self.content_root
            )));
        }
        Ok(())
    }
}

/// Why a checkpoint bundle cannot be authenticated as the bytes its manifest names.
#[derive(Debug)]
pub enum BundleError {
    Io(std::io::Error),
    Checkpoint(crate::CheckpointError),
    Invalid(String),
    Missing(String),
    Extra(String),
    Changed(String),
}

impl std::fmt::Display for BundleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "checkpoint bundle I/O error: {error}"),
            Self::Checkpoint(error) => write!(formatter, "checkpoint bundle claim: {error}"),
            Self::Invalid(reason) => write!(formatter, "invalid checkpoint bundle: {reason}"),
            Self::Missing(path) => write!(formatter, "checkpoint bundle is missing {path}"),
            Self::Extra(path) => write!(formatter, "checkpoint bundle has extra file {path}"),
            Self::Changed(path) => write!(formatter, "checkpoint bundle file changed: {path}"),
        }
    }
}

impl std::error::Error for BundleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Checkpoint(error) => Some(error),
            Self::Invalid(_) | Self::Missing(_) | Self::Extra(_) | Self::Changed(_) => None,
        }
    }
}

impl From<std::io::Error> for BundleError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<crate::CheckpointError> for BundleError {
    fn from(error: crate::CheckpointError) -> Self {
        Self::Checkpoint(error)
    }
}

fn digest_files(directory: &Path) -> Result<Vec<BundleFile>, BundleError> {
    let mut paths = Vec::new();
    collect_files(directory, directory, &mut paths)?;
    paths.sort_by(|left, right| left.0.cmp(&right.0));
    paths
        .into_iter()
        .map(|(relative, absolute)| digest_file(relative, &absolute))
        .collect()
}

fn collect_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<(String, PathBuf)>,
) -> Result<(), BundleError> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let path = entry.path();
        let relative = portable_path(path.strip_prefix(root).map_err(|_| {
            BundleError::Invalid(format!("{} escaped the bundle", path.display()))
        })?)?;
        if kind.is_symlink() {
            return Err(BundleError::Invalid(format!(
                "bundle contains symlink {relative}"
            )));
        }
        if kind.is_dir() {
            collect_files(root, &path, files)?;
        } else if kind.is_file() {
            if relative != BUNDLE_MANIFEST_FILE {
                files.push((relative, path));
            }
        } else {
            return Err(BundleError::Invalid(format!(
                "bundle contains special file {relative}"
            )));
        }
    }
    Ok(())
}

fn portable_path(path: &Path) -> Result<String, BundleError> {
    let mut parts = Vec::new();
    for component in path.components() {
        let part = component.as_os_str().to_str().ok_or_else(|| {
            BundleError::Invalid(format!("bundle path {} is not UTF-8", path.display()))
        })?;
        if part.is_empty() || part.contains(['/', '\\']) || matches!(part, "." | "..") {
            return Err(BundleError::Invalid(format!(
                "bundle path {} is not portable",
                path.display()
            )));
        }
        parts.push(part);
    }
    let path = parts.join("/");
    if path.is_empty() {
        return Err(BundleError::Invalid("bundle path is empty".to_owned()));
    }
    Ok(path)
}

fn validate_path(path: &str) -> Result<(), BundleError> {
    if path.is_empty()
        || path.starts_with('/')
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | "..") || part.contains('\\'))
        || path == BUNDLE_MANIFEST_FILE
    {
        return Err(BundleError::Invalid(format!(
            "bundle path {path:?} is not a canonical payload path"
        )));
    }
    Ok(())
}

fn digest_file(path: String, absolute: &Path) -> Result<BundleFile, BundleError> {
    validate_path(&path)?;
    let expected_size = absolute.metadata()?.len();
    let mut input = File::open(absolute)?;
    let mut file_hasher = Sha256::new();
    let mut chunks = Vec::new();
    let mut size = 0_u64;
    let mut buffer = vec![0; CHUNK_BYTES];
    loop {
        let mut filled = 0;
        while filled < buffer.len() {
            let read = input.read(&mut buffer[filled..])?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        if filled == 0 {
            break;
        }
        let bytes = &buffer[..filled];
        file_hasher.update(bytes);
        chunks.push(sha256(bytes).to_string());
        size = size
            .checked_add(filled as u64)
            .ok_or_else(|| BundleError::Invalid(format!("{path} is too large")))?;
    }
    if size != expected_size || absolute.metadata()?.len() != expected_size {
        return Err(BundleError::Changed(path));
    }
    Ok(BundleFile {
        path,
        size,
        sha256: hex::encode(file_hasher.finalize()),
        chunks,
    })
}

fn content_root(checkpoint: &BundleClaims, files: &[BundleFile]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(CONTENT_DOMAIN);
    update_string(&mut hasher, &checkpoint.state_format);
    hasher.update(checkpoint.checkpoint_stacks_height.to_be_bytes());
    update_string(&mut hasher, &checkpoint.source_state_id);
    update_string(&mut hasher, &checkpoint.published_state_index_root);
    hasher.update(checkpoint.bitcoin_height.to_be_bytes());
    update_string(&mut hasher, &checkpoint.bitcoin_block_hash);
    update_string(&mut hasher, &checkpoint.attesting_block_id);
    hasher.update(checkpoint.signer_weight.to_be_bytes());
    hasher.update(checkpoint.approval_threshold.to_be_bytes());
    update_string(&mut hasher, &checkpoint.semantic_epoch);
    update_string(&mut hasher, &checkpoint.compiler_identity);
    update_string(&mut hasher, &checkpoint.profile_fingerprint);
    for file in files {
        update_string(&mut hasher, &file.path);
        hasher.update(file.size.to_be_bytes());
        hasher.update(hex::decode(&file.sha256).expect("validated file digest"));
        hasher.update((file.chunks.len() as u64).to_be_bytes());
        for chunk in &file.chunks {
            hasher.update(hex::decode(chunk).expect("validated chunk digest"));
        }
    }
    hex::encode(hasher.finalize())
}

fn update_string(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value.as_bytes());
}

fn describe_difference(expected: &[BundleFile], actual: &[BundleFile]) -> BundleError {
    let expected_paths = expected.iter().map(|file| file.path.as_str());
    let actual_paths = actual.iter().map(|file| file.path.as_str());
    for (expected, actual) in expected_paths.clone().zip(actual_paths.clone()) {
        if expected != actual {
            return if expected < actual {
                BundleError::Missing(expected.to_owned())
            } else {
                BundleError::Extra(actual.to_owned())
            };
        }
    }
    if expected.len() > actual.len() {
        return BundleError::Missing(expected[actual.len()].path.clone());
    }
    if actual.len() > expected.len() {
        return BundleError::Extra(actual[expected.len()].path.clone());
    }
    let changed = expected
        .iter()
        .zip(actual)
        .find(|(expected, actual)| expected != actual)
        .map_or_else(|| "content root".to_owned(), |(file, _)| file.path.clone());
    BundleError::Changed(changed)
}

fn fixed_hex(name: &str, value: &str, bytes: usize) -> Result<(), BundleError> {
    if value.len() != bytes * 2 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(BundleError::Invalid(format!(
            "{name} is not a {bytes}-byte hexadecimal value"
        )));
    }
    if value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err(BundleError::Invalid(format!("{name} is not lowercase")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{BundleClaims, BundleError, CheckpointBundleManifest};

    const PROFILE: &str = "0909090909090909090909090909090909090909090909090909090909090909";

    fn claims() -> BundleClaims {
        BundleClaims {
            state_format: "stacks-core-marf-sqlite-v2".to_owned(),
            checkpoint_stacks_height: 7,
            source_state_id: "01".repeat(32),
            published_state_index_root: "02".repeat(32),
            bitcoin_height: 11,
            bitcoin_block_hash: "03".repeat(32),
            attesting_block_id: "01".repeat(32),
            signer_weight: 7,
            approval_threshold: 6,
            semantic_epoch: "Epoch40".to_owned(),
            compiler_identity: format!("sha256:{}", "04".repeat(32)),
            profile_fingerprint: PROFILE.to_owned(),
        }
    }

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("bundle");
        fs::write(
            root.path().join("checkpoint.toml"),
            format!(
                "format = \"stacks-core-marf-sqlite-v2\"\n\
                 checkpoint_stacks_height = 7\n\
                 source_state_id = \"{}\"\n\
                 published_state_index_root = \"{}\"\n\
                 first_bitcoin_height = 11\n\
                 profile_fingerprint = \"{PROFILE}\"\n",
                "01".repeat(32),
                "02".repeat(32)
            ),
        )
        .expect("checkpoint manifest");
        fs::create_dir(root.path().join("sortition")).expect("sortition directory");
        fs::write(root.path().join("sortition/history.bin"), b"history").expect("history");
        fs::write(root.path().join("marf.sqlite"), b"state").expect("state");
        root
    }

    #[test]
    fn independent_builders_produce_the_same_byte_manifest() {
        let first = fixture();
        let second = fixture();
        let first_manifest =
            CheckpointBundleManifest::write_new(first.path(), claims()).expect("first manifest");
        let second_manifest =
            CheckpointBundleManifest::write_new(second.path(), claims()).expect("second manifest");
        assert_eq!(first_manifest, second_manifest);
        assert_eq!(
            fs::read(first.path().join(super::BUNDLE_MANIFEST_FILE)).expect("first bytes"),
            fs::read(second.path().join(super::BUNDLE_MANIFEST_FILE)).expect("second bytes")
        );
        let mut different_claim = claims();
        different_claim.bitcoin_block_hash = "05".repeat(32);
        let different = CheckpointBundleManifest::build(second.path(), different_claim)
            .expect("different claim manifest");
        assert_ne!(first_manifest.content_root(), different.content_root());
        CheckpointBundleManifest::verify(first.path()).expect("first verifies");
        CheckpointBundleManifest::verify(second.path()).expect("second verifies");
    }

    #[test]
    fn changed_missing_extra_and_truncated_payloads_are_refused() {
        let root = fixture();
        CheckpointBundleManifest::write_new(root.path(), claims()).expect("manifest");

        fs::write(root.path().join("marf.sqlite"), b"other").expect("change state");
        assert!(matches!(
            CheckpointBundleManifest::verify(root.path()),
            Err(BundleError::Changed(path)) if path == "marf.sqlite"
        ));
        fs::write(root.path().join("marf.sqlite"), b"state").expect("restore state");

        fs::remove_file(root.path().join("sortition/history.bin")).expect("remove history");
        assert!(matches!(
            CheckpointBundleManifest::verify(root.path()),
            Err(BundleError::Missing(path)) if path == "sortition/history.bin"
        ));
        fs::write(root.path().join("sortition/history.bin"), b"history").expect("restore history");

        fs::write(root.path().join("extra"), b"extra").expect("extra file");
        assert!(matches!(
            CheckpointBundleManifest::verify(root.path()),
            Err(BundleError::Extra(path)) if path == "extra"
        ));
        fs::remove_file(root.path().join("extra")).expect("remove extra");

        fs::write(root.path().join("sortition/history.bin"), b"his").expect("truncate history");
        assert!(matches!(
            CheckpointBundleManifest::verify(root.path()),
            Err(BundleError::Changed(path)) if path == "sortition/history.bin"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_payloads_are_refused() {
        let root = fixture();
        std::os::unix::fs::symlink("marf.sqlite", root.path().join("alias"))
            .expect("payload symlink");
        assert!(matches!(
            CheckpointBundleManifest::build(root.path(), claims()),
            Err(BundleError::Invalid(reason)) if reason.contains("symlink")
        ));
    }

    #[test]
    fn claims_must_match_the_checkpoint_and_attestation() {
        let root = fixture();
        let mut wrong = claims();
        wrong.checkpoint_stacks_height += 1;
        assert!(matches!(
            CheckpointBundleManifest::build(root.path(), wrong),
            Err(BundleError::Invalid(reason)) if reason.contains("checkpoint.toml")
        ));

        let mut weak = claims();
        weak.signer_weight = weak.approval_threshold - 1;
        assert!(matches!(
            CheckpointBundleManifest::build(root.path(), weak),
            Err(BundleError::Invalid(reason)) if reason.contains("threshold")
        ));
    }
}
