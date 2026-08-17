//! Bounded entry points shared by deterministic corpus replay and fuzz engines.

use std::{collections::BTreeMap, fs, io::Cursor};

use blockstack_lib::chainstate::{
    nakamoto::NakamotoBlock as ReferenceNakamotoBlock,
    stacks::StacksTransaction as ReferenceTransaction,
};
use clarity::vm::{ClarityVersion, types::QualifiedContractIdentifier};
use nano_chainstate::NakamotoBlock;
use nano_codec::Transaction;
use nano_marf::{
    BUNDLE_MANIFEST_FILE, CheckpointBundleManifest, CheckpointManifest, MarfTrie, MarfValue,
};
use nano_p2p::{
    Protocol,
    wire::{Message, PREAMBLE_LEN, Preamble},
};
use nano_primitives::Network;
use nano_stackerdb::{Chunk, SignerMessage};
use nano_vm::{SemanticEpochInspection, Vm, semantic_epoch_at_burn_height};
use stacks_common::codec::StacksMessageCodec;

const MAX_WIRE_BYTES: usize = 1024 * 1024;
const MAX_CODEC_BYTES: usize = 2 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_CLARITY_BYTES: usize = 128 * 1024;
const MAX_MARF_OPERATIONS: usize = 256;
const CHECKPOINT_SEPARATOR: &[u8] = b"\n--- checkpoint.toml ---\n";

fn within(input: &[u8], maximum: usize) -> Option<&[u8]> {
    (input.len() <= maximum).then_some(input)
}

/// Exercise P2P preamble framing, message decoding and network admission policy.
#[must_use]
pub fn p2p_frame_and_protocol(input: &[u8]) -> u8 {
    let Some(input) = within(input, MAX_WIRE_BYTES) else {
        return 0;
    };
    let Some((header, frame)) = input.split_at_checked(PREAMBLE_LEN) else {
        return 0;
    };
    let Ok(preamble) = Preamble::decode(header) else {
        return 0;
    };

    let mut coverage = 0;
    for protocol in [Protocol::mainnet(), Protocol::testnet()] {
        let accepted = protocol.accepts(&preamble);
        assert_eq!(
            accepted,
            preamble.network_id == protocol.network_id
                && preamble.peer_version & 0xff00_0000 == protocol.peer_version & 0xff00_0000
        );
        coverage |= u8::from(accepted) << 1;
    }

    let Ok(payload_len) = usize::try_from(preamble.payload_len) else {
        return coverage;
    };
    if frame.len() != payload_len {
        return coverage;
    }
    let Ok(message) = Message::decode(preamble, frame.to_vec()) else {
        return coverage;
    };
    assert_eq!(message.encode(), input);
    assert_eq!(message.wire_len(), input.len());
    coverage | 1
}

/// Exercise transaction and Nakamoto block codecs and their canonical encoders.
#[must_use]
pub fn transaction_and_block_codecs(input: &[u8]) -> u8 {
    let Some(input) = within(input, MAX_CODEC_BYTES) else {
        return 0;
    };

    let mut coverage = 0;
    if let Ok((transaction, consumed)) = Transaction::decode(input) {
        assert!(consumed <= input.len());
        let encoded = transaction.encode();
        assert_eq!(encoded, input[..consumed]);
        let (decoded, decoded_len) = Transaction::decode(&encoded).expect("encoded transaction");
        assert_eq!(decoded_len, encoded.len());
        assert_eq!(decoded, transaction);
        coverage |= 1;
    }

    if let Ok(block) = NakamotoBlock::decode(input) {
        let encoded = block.encode();
        assert_eq!(encoded, input);
        assert_eq!(
            NakamotoBlock::decode(&encoded).expect("encoded block"),
            block
        );
        coverage |= 2;
    }
    coverage
}

/// Compare canonical transaction and Nakamoto block codecs with pinned stacks-core.
#[must_use]
pub fn transaction_and_block_differential(input: &[u8]) -> u8 {
    let Some(input) = within(input, MAX_CODEC_BYTES) else {
        return 0;
    };

    let nano_transaction = Transaction::decode(input).map(|(transaction, consumed)| {
        (
            transaction.encode(),
            u64::try_from(consumed).expect("bounded input"),
        )
    });
    let mut reference_input = Cursor::new(input);
    let reference_transaction = ReferenceTransaction::consensus_deserialize(&mut reference_input)
        .map(|transaction| {
            let mut encoded = Vec::new();
            transaction
                .consensus_serialize(&mut encoded)
                .expect("decoded reference transaction re-encodes");
            (encoded, reference_input.position())
        });
    let transaction_coverage = match (nano_transaction, reference_transaction) {
        (Ok(nano), Ok(reference)) => {
            assert_eq!(nano, reference, "transaction codec divergence");
            1
        }
        (Err(_), Err(_)) => 0,
        (nano, reference) => panic!(
            "transaction acceptance divergence: nano={}, reference={}",
            nano.is_ok(),
            reference.is_ok()
        ),
    };

    let nano_block = NakamotoBlock::decode(input).map(|block| block.encode());
    let mut reference_input = Cursor::new(input);
    let reference_block = ReferenceNakamotoBlock::consensus_deserialize(&mut reference_input)
        .and_then(|block| {
            if reference_input.position()
                != u64::try_from(input.len()).expect("bounded input length")
            {
                return Err(stacks_common::codec::Error::DeserializeError(
                    "trailing Nakamoto block bytes".to_owned(),
                ));
            }
            let mut encoded = Vec::new();
            block.consensus_serialize(&mut encoded)?;
            Ok(encoded)
        });
    let block_coverage = match (nano_block, reference_block) {
        (Ok(nano), Ok(reference)) => {
            assert_eq!(nano, reference, "Nakamoto block codec divergence");
            2
        }
        (Err(_), Err(_)) => 0,
        (nano, reference) => panic!(
            "Nakamoto block acceptance divergence: nano={}, reference={}",
            nano.is_ok(),
            reference.is_ok()
        ),
    };

    transaction_coverage | block_coverage
}

/// Exercise signer messages and their enclosing `StackerDB` chunks.
#[must_use]
pub fn signer_and_stackerdb_codecs(input: &[u8]) -> u8 {
    let Some(input) = within(input, MAX_CODEC_BYTES) else {
        return 0;
    };

    let mut coverage = 0;
    if let Ok(message) = SignerMessage::decode(input) {
        let encoded = message.encode().expect("decoded signer message re-encodes");
        assert_eq!(encoded, input);
        assert_eq!(
            SignerMessage::decode(&encoded).expect("encoded signer message"),
            message
        );
        coverage |= 1;
    }

    if let Ok(chunk) = Chunk::decode(input) {
        let encoded = chunk.encode().expect("decoded chunk re-encodes");
        assert_eq!(encoded, input);
        assert_eq!(Chunk::decode(&encoded).expect("encoded chunk"), chunk);
        coverage |= 2;
    }
    coverage
}

/// Exercise checkpoint and bundle manifest loading plus their joint validation.
#[must_use]
pub fn checkpoint_manifests(input: &[u8]) -> u8 {
    let Some(input) = within(input, MAX_MANIFEST_BYTES) else {
        return 0;
    };
    let (bundle, checkpoint) = input
        .windows(CHECKPOINT_SEPARATOR.len())
        .position(|window| window == CHECKPOINT_SEPARATOR)
        .map_or_else(
            || input.split_at(input.len() / 2),
            |offset| {
                (
                    &input[..offset],
                    &input[offset + CHECKPOINT_SEPARATOR.len()..],
                )
            },
        );
    let directory = tempfile::tempdir().expect("temporary manifest directory");
    fs::write(directory.path().join(BUNDLE_MANIFEST_FILE), bundle)
        .expect("write temporary bundle manifest");
    fs::write(directory.path().join("checkpoint.toml"), checkpoint)
        .expect("write temporary checkpoint manifest");

    let loaded_bundle = CheckpointBundleManifest::load(directory.path());
    let loaded_checkpoint = CheckpointManifest::load(directory.path());
    let coverage = if let (Ok(bundle), Ok(checkpoint)) = (loaded_bundle, loaded_checkpoint) {
        let first = bundle.validate_against(&checkpoint);
        let second = bundle.validate_against(&checkpoint);
        assert_eq!(first.is_ok(), second.is_ok());
        assert_eq!(
            first.as_ref().map_err(ToString::to_string),
            second.as_ref().map_err(ToString::to_string)
        );

        let encoded = toml::to_string(&bundle).expect("serialize loaded bundle manifest");
        let decoded: CheckpointBundleManifest =
            toml::from_str(&encoded).expect("deserialize serialized bundle manifest");
        assert_eq!(decoded, bundle);
        u8::from(first.is_ok())
    } else {
        0
    };

    let _ = CheckpointBundleManifest::verify(directory.path());
    coverage
}

/// Exercise a bounded stream of in-memory MARF inserts and reads against an oracle.
#[must_use]
pub fn marf_operations(input: &[u8]) -> u8 {
    const RECORD_BYTES: usize = 1 + 32 + 40;

    let mut trie = MarfTrie::default();
    let mut expected = BTreeMap::new();
    let mut coverage = 0;
    for record in input.chunks_exact(RECORD_BYTES).take(MAX_MARF_OPERATIONS) {
        let path: [u8; 32] = record[1..33].try_into().expect("fixed path width");
        let value = MarfValue::from_bytes(record[33..].try_into().expect("fixed value width"));
        if record[0] & 1 == 0 {
            trie.insert_path(path, value).expect("in-memory insert");
            expected.insert(path, value);
            coverage |= 1;
            assert_eq!(
                trie.get_path(path).expect("read inserted path"),
                Some(value)
            );
        } else {
            coverage |= 2;
            assert_eq!(
                trie.get_path(path).expect("read path"),
                expected.get(&path).copied()
            );
        }
    }

    for (path, value) in &expected {
        assert_eq!(trie.get_path(*path).expect("read final path"), Some(*value));
    }
    assert_eq!(
        trie.leaves().expect("enumerate leaves").len(),
        expected.len()
    );
    assert_eq!(
        trie.root_hash().expect("hash trie"),
        trie.root_hash().expect("rehash trie")
    );
    coverage
}

/// Compile accepted Clarity source through the production compiler and Wasm loader.
#[must_use]
pub fn clarity_wasm_abi(input: &[u8]) -> u8 {
    let Some(input) = within(input, MAX_CLARITY_BYTES) else {
        return 0;
    };
    let Ok(source) = std::str::from_utf8(input) else {
        return 0;
    };
    let contract =
        QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.adversarial-corpus")
            .expect("fixed contract identifier");
    let mut vm = Vm::new(Network::TESTNET).expect("empty VM");
    vm.begin_block(None, [0x37; 32])
        .expect("begin temporary block");
    let epoch = semantic_epoch_at_burn_height(Network::TESTNET, 0);

    let Ok(inspection) =
        vm.inspect_module_semantic_epoch(&contract, ClarityVersion::Clarity4, source, epoch)
    else {
        return 0;
    };
    if let SemanticEpochInspection::Inspected(inspection) = inspection {
        assert!(
            inspection.refusal.is_none(),
            "accepted Clarity emitted a module the production loader refused: {:?}",
            inspection.refusal
        );
        return 1;
    }
    0
}
