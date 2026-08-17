//! Bounded entry points shared by deterministic corpus replay and fuzz engines.

use std::{collections::BTreeMap, fs, io::Cursor};

use blockstack_lib::chainstate::{
    nakamoto::NakamotoBlock as ReferenceNakamotoBlock,
    stacks::{
        StacksTransaction as ReferenceTransaction,
        index::{
            ClarityMarfTrieId as _, MARFValue as ReferenceMarfValue,
            marf::{MARF as ReferenceMarf, MARFOpenOpts as ReferenceMarfOpenOpts},
        },
    },
};
use clarity::vm::{ClarityVersion, types::QualifiedContractIdentifier};
use nano_chainstate::NakamotoBlock;
use nano_codec::Transaction;
use nano_crypto::StacksPrivateKey;
use nano_marf::{
    BUNDLE_MANIFEST_FILE, CheckpointBundleManifest, CheckpointManifest, MarfTrie, MarfValue,
    import_checkpoint,
};
use nano_p2p::{
    InboundLimits, Listener, LocalPeer, Protocol, Service, Session, SessionError, serve_peer,
    wire::{ChainView, Message, NeighborAddress, PREAMBLE_LEN, PeerAddress, Preamble, nack},
};
use nano_primitives::{BitVec, BitcoinHeaderHash, ConsensusHash, Hash160, Network, TrieHash};
use nano_stackerdb::{Chunk, SignerMessage};
use nano_vm::{SemanticEpochInspection, Vm, semantic_epoch_at_burn_height};
use stacks_common::codec::StacksMessageCodec;
use stacks_common::types::chainstate::StacksBlockId as ReferenceStacksBlockId;

const MAX_WIRE_BYTES: usize = 1024 * 1024;
const MAX_CODEC_BYTES: usize = 2 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_CHECKPOINT_IMPORT_BYTES: usize = 1 + 4 * (1 + 16 * 2);
const MAX_CLARITY_BYTES: usize = 128 * 1024;
const MAX_MARF_OPERATIONS: usize = 256;
const MAX_SESSION_OPERATIONS: usize = 32;
const CHECKPOINT_SEPARATOR: &[u8] = b"\n--- checkpoint.toml ---\n";
const SESSION_CYCLE: ConsensusHash = ConsensusHash::from_bytes([0x61; 20]);

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

#[derive(Default)]
struct SessionService;

impl Service for SessionService {
    fn chain_view(&self) -> ChainView {
        session_view(100)
    }

    fn neighbors(&self) -> Vec<NeighborAddress> {
        vec![NeighborAddress {
            address: PeerAddress::from_bytes([0x12; 16]),
            port: 20444,
            public_key_hash: Hash160::from_bytes([0x34; 20]),
        }]
    }

    fn tenure_inventory(&self, cycle_start: ConsensusHash) -> Option<BitVec<2100>> {
        (cycle_start == SESSION_CYCLE).then(|| {
            let mut inventory = BitVec::zeros(2100).expect("fixed inventory width");
            inventory.set(0, true).expect("first inventory bit");
            inventory.set(2099, true).expect("last inventory bit");
            inventory
        })
    }
}

fn session_view(height: u64) -> ChainView {
    ChainView::with_stable_confirmations(
        height,
        BitcoinHeaderHash::from_bytes([0x45; 32]),
        BitcoinHeaderHash::from_bytes([0x67; 32]),
        1,
    )
    .expect("height exceeds one confirmation")
}

/// Exercise a bounded authenticated P2P conversation over a real loopback socket.
#[must_use]
pub fn p2p_session_state(input: &[u8]) -> u8 {
    let Some(input) = within(input, MAX_SESSION_OPERATIONS) else {
        return 0;
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build bounded session runtime")
        .block_on(session_state(input))
}

async fn session_state(operations: &[u8]) -> u8 {
    let protocol = Protocol::testnet()
        .with_stable_confirmations(1)
        .expect("one confirmation is valid");
    let listener = Listener::bind("127.0.0.1:0".parse().expect("loopback address"))
        .await
        .expect("bind loopback session");
    let address = listener.local_addr().expect("bound loopback address");
    let server_peer = LocalPeer::quiet(StacksPrivateKey::from_seed(b"fuzz server"), address.port());
    let client_peer = LocalPeer::quiet(StacksPrivateKey::from_seed(b"fuzz client"), 20444);
    let service = SessionService;
    let timeout = std::time::Duration::from_secs(1);

    let serve_session = async {
        let (stream, from) = listener.accept().await.expect("accept loopback session");
        serve_peer(
            stream,
            from,
            &server_peer,
            protocol,
            &service,
            InboundLimits {
                timeout,
                idle: timeout,
                max_messages: u64::try_from(operations.len() + 1).expect("bounded operations"),
            },
        )
        .await
        .expect("serve bounded session")
    };
    let client = async {
        let mut session =
            Session::open(address, &client_peer, protocol, session_view(101), timeout)
                .await
                .expect("complete loopback handshake");
        let mut coverage = 0;
        let mut answered = 1_u64;
        let mut nacked = 0_u64;
        for operation in operations {
            match operation % 5 {
                0 => {
                    session.ping().await.expect("ping reply");
                    coverage |= 1;
                    answered += 1;
                }
                1 => {
                    assert_eq!(session.neighbors().await.expect("neighbor reply").len(), 1);
                    coverage |= 2;
                    answered += 1;
                }
                2 => {
                    let inventory = session
                        .nakamoto_inventory(SESSION_CYCLE)
                        .await
                        .expect("known inventory reply");
                    assert!(inventory.get(0).expect("first inventory bit"));
                    assert!(inventory.get(2099).expect("last inventory bit"));
                    coverage |= 4;
                    answered += 1;
                }
                3 => {
                    assert!(matches!(
                        session
                            .nakamoto_inventory(ConsensusHash::from_bytes([0xff; 20]))
                            .await,
                        Err(SessionError::Nack(code)) if code == nack::NO_SUCH_BITCOIN_BLOCK
                    ));
                    coverage |= 8;
                    nacked += 1;
                }
                _ => {
                    session.advertise(session_view(102));
                    session.ping().await.expect("ping after view update");
                    coverage |= 16;
                    answered += 1;
                }
            }
        }
        assert_eq!(session.take_unsolicited_count(), 0);
        drop(session);
        (coverage, answered, nacked)
    };

    let (report, (coverage, answered, nacked)) = tokio::join!(serve_session, client);
    assert_eq!(report.peer, Some(client_peer.public_key_hash()));
    assert_eq!(report.answered, answered);
    assert_eq!(report.nacked, nacked);
    assert_eq!(report.ignored, 0);
    coverage
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

/// Build a bounded stacks-core checkpoint and import its complete graph through nano.
#[must_use]
pub fn checkpoint_import(input: &[u8]) -> u8 {
    let Some(input) = within(input, MAX_CHECKPOINT_IMPORT_BYTES) else {
        return 0;
    };
    let Some((&block_count, mut input)) = input.split_first() else {
        return 0;
    };
    let block_count = usize::from(block_count).min(4);
    if block_count == 0 {
        return 0;
    }

    let mut blocks = Vec::with_capacity(block_count);
    let mut expected = BTreeMap::new();
    let mut overwrote = false;
    for _ in 0..block_count {
        let Some((&entry_count, rest)) = input.split_first() else {
            return 0;
        };
        let entry_count = usize::from(entry_count).min(16);
        let Some((entries, rest)) = rest.split_at_checked(entry_count * 2) else {
            return 0;
        };
        input = rest;

        let mut writes = BTreeMap::new();
        for entry in entries.chunks_exact(2) {
            let key = format!("fuzz-key-{:02x}", entry[0]);
            let value = [entry[1]; 40];
            writes.insert(key.clone(), value);
            overwrote |= expected.insert(key, value).is_some();
        }
        if writes.is_empty() {
            return 0;
        }
        blocks.push(writes);
    }

    let directory = tempfile::tempdir().expect("temporary checkpoint directory");
    let checkpoint = directory.path().join("marf.sqlite");
    fs::write(format!("{}.blobs", checkpoint.display()), []).expect("create external blob file");
    let mut options = ReferenceMarfOpenOpts::default();
    options.external_blobs = true;
    let mut reference = ReferenceMarf::<ReferenceStacksBlockId>::from_path(
        checkpoint.to_str().expect("temporary path is UTF-8"),
        options,
    )
    .expect("create reference checkpoint");
    let mut parent = ReferenceStacksBlockId::sentinel();
    for (index, writes) in blocks.iter().enumerate() {
        let block =
            ReferenceStacksBlockId([u8::try_from(index + 1).expect("four bounded blocks"); 32]);
        let keys = writes.keys().cloned().collect::<Vec<_>>();
        let values = writes.values().copied().map(ReferenceMarfValue).collect();
        let mut transaction = reference.begin_tx().expect("begin reference transaction");
        transaction
            .begin(&parent, &block)
            .expect("begin reference block");
        transaction
            .insert_batch(&keys, values)
            .expect("write reference block");
        transaction.seal().expect("seal reference block");
        transaction.commit().expect("commit reference block");
        parent = block;
    }
    let root = reference
        .get_root_hash_at(&parent)
        .expect("read reference root");
    drop(reference);

    let source = parent.0;
    let root = TrieHash::from_bytes(root.0);
    let imported =
        import_checkpoint(&checkpoint, source, root).expect("import generated checkpoint");
    assert_eq!(
        imported.root(source).expect("read imported root"),
        Some(root)
    );
    for (key, value) in expected {
        assert_eq!(
            imported
                .get(source, key.as_bytes())
                .expect("read imported key"),
            Some(MarfValue::from_bytes(value))
        );
    }

    1 | (u8::from(block_count > 1) << 1) | (u8::from(overwrote) << 2)
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
