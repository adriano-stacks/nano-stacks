#![forbid(unsafe_code)]

use std::{
    fmt::Write,
    fs,
    path::{Path, PathBuf},
};

use nano_node::{BaselineSource, ReplayFailure, replay_one};

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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplayDepth {
    pub completed: u64,
    pub expected: u64,
    pub first_failure: Option<u64>,
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
    }
}

/// Render a stable, human-readable progress report for local development and CI.
#[must_use]
pub fn scoreboard(manifest: FixtureManifest) -> String {
    let replay = baseline_replay(manifest);
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
        replay
            .first_failure
            .map_or_else(|| "—".to_owned(), |height| format!("block {height}"))
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

#[cfg(test)]
mod tests {
    use super::{
        FixtureManifest, FixtureMode, FixtureStatus, baseline_replay, scoreboard,
        validate_fixture_tree,
    };
    use blockstack_lib::chainstate::nakamoto::NakamotoBlock as ReferenceNakamotoBlock;
    use blockstack_lib::chainstate::stacks::address::{
        PoxAddress as ReferencePoxAddress, PoxAddressType20 as ReferencePoxAddressType20,
        PoxAddressType32 as ReferencePoxAddressType32,
    };
    use blockstack_lib::chainstate::stacks::index::MARFValue as ReferenceMarfValue;
    use nano_address::{PoxAddress, PoxAddressType20, PoxAddressType32, StacksAddress};
    use nano_codec::{
        Transaction as NanoTransaction, TransactionAuth as NanoTransactionAuth,
        transaction_merkle_root,
    };
    use nano_crypto::{
        CryptoError, MessageSignature, StacksPrivateKey, Vrf, VrfPrivateKey, VrfProof,
    };
    use nano_marf::{MarfValue, key_path};
    use nano_primitives::{BitVec, TrieHash, hash160, sha256, sha512, sha512_256};
    use proptest::prelude::*;
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
            chainstate::{StacksAddress as ReferenceStacksAddress, TrieHash as ReferenceTrieHash},
        },
        util::hash::{
            Hash160 as ReferenceHash160, Sha256Sum as ReferenceSha256Sum,
            Sha512Sum as ReferenceSha512Sum, Sha512Trunc256Sum,
        },
        util::uint::Uint256 as ReferenceUint256,
    };
    use std::{
        fs,
        io::Cursor,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

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
    fn captured_fixture_requires_every_oracle_input() -> Result<(), Box<dyn std::error::Error>> {
        let root = temporary_fixture_root()?;
        write_file(
            &root.join("manifest.toml"),
            "mode = \"captured\"\nreplay_blocks = 1\n",
        )?;
        write_file(&root.join("bitcoin/blocks/00000001.hex"), "00")?;
        write_file(&root.join("nakamoto/blocks/00000001.bin"), "block")?;
        write_file(&root.join("events/new_block/00000001.json"), "{}")?;
        write_file(&root.join("stacker_set/cycle-0.json"), "{}")?;
        write_file(&root.join("sortition/snapshots.json"), "[]")?;
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
                nano_transactions.push(nano);
            }
            assert_eq!(
                transaction_merkle_root(&nano_transactions).as_bytes(),
                &block.header.tx_merkle_root.0
            );
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
        assert_eq!(
            ours.public_key()
                .verify_signer(&digest, &high_s)
                .expect_err("signer signatures reject high-S"),
            CryptoError::HighS
        );
        let reference_high_s = ReferenceMessageSignature(high_s.as_bytes().to_owned());
        assert!(Secp256k1PublicKey::recover_to_pubkey(&digest, &reference_high_s).is_err());
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

    fn temporary_fixture_root() -> Result<PathBuf, std::io::Error> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("nano-stacks-fixtures-{unique}"));
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
