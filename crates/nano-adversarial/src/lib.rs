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
use clarity::{
    types::StacksEpochId,
    vm::{
        ClarityVersion, Value,
        costs::{ExecutionCost, LimitedCostTracker},
        errors::VmExecutionError,
        types::QualifiedContractIdentifier,
    },
};
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
use nano_vm::{
    ContractCallOutcome, EPOCH_4_BLOCK_LIMIT, MarfStore, SemanticEpochInspection,
    TransactionResult, Vm, semantic_epoch_at_burn_height,
};
use stacks_common::codec::StacksMessageCodec;
use stacks_common::types::chainstate::StacksBlockId as ReferenceStacksBlockId;

const MAX_WIRE_BYTES: usize = 1024 * 1024;
const MAX_CODEC_BYTES: usize = 2 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_CHECKPOINT_IMPORT_BYTES: usize = 1 + 4 * (1 + 16 * 2);
const MAX_CLARITY_BYTES: usize = 128 * 1024;
const MAX_CLARITY_DIFFERENTIAL_BYTES: usize = 50;
const MAX_CLARITY_STATEFUL_BYTES: usize = 50;
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
    let mut roots = Vec::with_capacity(blocks.len());
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
        let root = reference
            .get_root_hash_at(&block)
            .expect("read reference block root");
        roots.push((block.clone(), root));
        parent = block;
    }
    let root = roots.last().expect("nonempty generated checkpoint").1;
    drop(reference);

    let source = parent.0;
    let root = TrieHash::from_bytes(root.0);
    let imported =
        import_checkpoint(&checkpoint, source, root).expect("import generated checkpoint");
    for (block, expected_root) in roots {
        assert_eq!(
            imported.root(block.0).expect("read imported root"),
            Some(TrieHash::from_bytes(expected_root.0))
        );
    }
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

/// Compare compiled and interpreted results and costs for a structured program.
#[must_use]
pub fn clarity_result_and_cost_differential(input: &[u8]) -> u8 {
    let Some(input) = within(input, MAX_CLARITY_DIFFERENTIAL_BYTES) else {
        return 0;
    };
    let Some((&template, input)) = input.split_first() else {
        return 0;
    };
    let Some((&width, input)) = input.split_first() else {
        return 0;
    };
    let Some((left, input)) = input.split_at_checked(8) else {
        return 0;
    };
    let Some((right, bytes)) = input.split_at_checked(8) else {
        return 0;
    };
    let left = u64::from_le_bytes(left.try_into().expect("fixed integer width"));
    let right = u64::from_le_bytes(right.try_into().expect("fixed integer width"));
    let width = usize::from(width % 8) + 1;
    let template = template % 6;
    let body = match template {
        0 => format!("(define-read-only (answer) (ok (+ u{left} u{right})))"),
        1 => format!(
            "(define-read-only (answer) (get kept (default-to {{kept: u{left}}} \
             (some {{extra: u{right}, kept: u{left}}}))))"
        ),
        2 => {
            let values = (0..width)
                .map(|index| if index % 2 == 0 { left } else { right })
                .map(|value| format!("u{value}"))
                .collect::<Vec<_>>()
                .join(" ");
            format!("(define-read-only (answer) (index-of? (list {values}) u{right}))")
        }
        3 => format!(
            "(define-read-only (answer) (match (some {{left: u{left}, right: u{right}}}) \
             value (+ (get left value) (get right value)) u0))"
        ),
        4 => format!(
            "(define-read-only (answer) (len 0x{}))",
            stacks_common::util::hash::to_hex(bytes)
        ),
        _ => {
            let values = (0..width)
                .map(|index| if index % 2 == 0 { left } else { right })
                .map(|value| format!("u{value}"))
                .collect::<Vec<_>>()
                .join(" ");
            format!(
                "(define-private (sum (value uint) (total uint)) (+ value total)) \
                 (define-read-only (answer) (fold sum (list {values}) u0))"
            )
        }
    };

    let source =
        format!("(print {{template: u{template}, left: u{left}, right: u{right}}}) {body}");
    clar2wasm::tools::crosscheck_compare_only(&source);
    clar2wasm::tools::crosscheck_cost(&source, "answer", &[]);
    1 << template
}

/// Compare the compiler and interpreter's typed refusal for a structured failure.
#[must_use]
pub fn clarity_refusal_differential(input: &[u8]) -> u8 {
    let Some((&template, _)) = input.split_first() else {
        return 0;
    };
    let template = template % 6;
    let source = match template {
        0 => "(/ u1 u0)",
        1 => "(- u0 u1)",
        2 => "(+ u340282366920938463463374607431768211455 u1)",
        3 => "(unwrap-panic (if true none (some u1)))",
        4 => "(unwrap-err-panic (if true (ok u1) (err u2)))",
        _ => "(asserts! false (err u1))",
    };
    let compiled = clar2wasm::tools::evaluate(source);
    let interpreted = clar2wasm::tools::interpret(source);
    assert!(compiled.is_err(), "failure template compiled successfully");
    assert_eq!(
        compiled, interpreted,
        "compiled and interpreted refusal diverged"
    );
    1 << template
}

const STATEFUL_VARIABLE: &str = r#"
(define-data-var stored { amount: uint, flag: bool } { amount: u0, flag: false })
(define-public (mutate (amount uint) (flag bool))
  (let ((next { amount: amount, flag: flag }))
    (begin
      (var-set stored next)
      (print { kind: "variable", value: next })
      (ok next))))
(define-read-only (snapshot) (var-get stored))
"#;

const STATEFUL_MAP: &str = r#"
(define-map entries uint { amount: uint, note: (buff 32) })
(define-public (mutate (key uint) (amount uint) (note (buff 32)) (replace bool))
  (let ((next { amount: amount, note: note }))
    (begin
      (if replace (map-set entries key next) (map-insert entries key next))
      (print { kind: "map", key: key, value: next })
      (ok (map-get? entries key)))))
(define-read-only (snapshot (key uint)) (map-get? entries key))
"#;

const STATEFUL_FT: &str = r#"
(define-fungible-token token)
(define-public (mutate (amount uint) (recipient principal))
  (begin
    (try! (ft-mint? token amount tx-sender))
    (try! (ft-transfer? token amount tx-sender recipient))
    (print { kind: "ft", amount: amount, recipient: recipient })
    (ok (ft-get-balance token recipient))))
(define-read-only (snapshot (owner principal)) (ft-get-balance token owner))
"#;

const STATEFUL_NFT: &str = r#"
(define-non-fungible-token collectible uint)
(define-public (mutate (identifier uint) (recipient principal))
  (begin
    (try! (nft-mint? collectible identifier tx-sender))
    (try! (nft-transfer? collectible identifier tx-sender recipient))
    (print { kind: "nft", identifier: identifier, recipient: recipient })
    (ok (nft-get-owner? collectible identifier))))
(define-read-only (snapshot (identifier uint))
  (nft-get-owner? collectible identifier))
"#;

const STATEFUL_ROLLBACK: &str = r#"
(define-data-var stored uint u0)
(define-public (mutate (amount uint))
  (begin
    (var-set stored amount)
    (print { kind: "rollback", amount: amount })
    (err amount)))
(define-read-only (snapshot) (var-get stored))
"#;

const STATEFUL_MULTI_WRITE: &str = r#"
(define-data-var total uint u0)
(define-map records uint (buff 32))
(define-public (mutate (key uint) (amount uint) (note (buff 32)))
  (begin
    (var-set total (+ (var-get total) amount))
    (map-set records key note)
    (print { kind: "multi", key: key, total: (var-get total), note: note })
    (ok { total: (var-get total), record: (map-get? records key) })))
(define-read-only (snapshot (key uint))
  { total: (var-get total), record: (map-get? records key) })
"#;

struct StatefulCase {
    source: &'static str,
    arguments: Vec<Value>,
    snapshot_arguments: Vec<Value>,
    expects_assets: bool,
}

#[derive(Debug, PartialEq)]
enum ReceiptOutcome {
    Success(TransactionResult),
    Aborted(TransactionResult),
    RuntimeFailure {
        cost: ExecutionCost,
        error: VmExecutionError,
    },
}

#[derive(Debug, PartialEq)]
struct StatefulObservation {
    deployment: TransactionResult,
    call: ReceiptOutcome,
    snapshot: ReceiptOutcome,
    root: [u8; 32],
}

fn stateful_contract() -> QualifiedContractIdentifier {
    QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.stateful")
        .expect("fixed contract identifier")
}

fn stateful_case(template: u8, amount: u128, key: u128, flag: bool, note: &[u8]) -> StatefulCase {
    let amount = Value::UInt(amount);
    let key = Value::UInt(key);
    let flag = Value::Bool(flag);
    let note = Value::buff_from(note.to_vec()).expect("bounded buffer");
    let recipient = Value::Principal(stateful_contract().into());
    match template {
        0 => StatefulCase {
            source: STATEFUL_VARIABLE,
            arguments: vec![amount, flag],
            snapshot_arguments: vec![],
            expects_assets: false,
        },
        1 => StatefulCase {
            source: STATEFUL_MAP,
            arguments: vec![key.clone(), amount, note, flag],
            snapshot_arguments: vec![key],
            expects_assets: false,
        },
        2 => StatefulCase {
            source: STATEFUL_FT,
            arguments: vec![amount, recipient.clone()],
            snapshot_arguments: vec![recipient],
            expects_assets: true,
        },
        3 => StatefulCase {
            source: STATEFUL_NFT,
            arguments: vec![key.clone(), recipient],
            snapshot_arguments: vec![key],
            expects_assets: true,
        },
        4 => StatefulCase {
            source: STATEFUL_ROLLBACK,
            arguments: vec![amount],
            snapshot_arguments: vec![],
            expects_assets: false,
        },
        _ => StatefulCase {
            source: STATEFUL_MULTI_WRITE,
            arguments: vec![key.clone(), amount, note],
            snapshot_arguments: vec![key],
            expects_assets: false,
        },
    }
}

fn receipt(outcome: ContractCallOutcome) -> ReceiptOutcome {
    match outcome {
        ContractCallOutcome::Success(result) => ReceiptOutcome::Success(*result),
        ContractCallOutcome::AbortedByResponse(result) => ReceiptOutcome::Aborted(*result),
        ContractCallOutcome::RuntimeFailure { cost, error } => {
            ReceiptOutcome::RuntimeFailure { cost, error }
        }
    }
}

const fn receipt_result(outcome: &ReceiptOutcome) -> Option<&TransactionResult> {
    match outcome {
        ReceiptOutcome::Success(result) | ReceiptOutcome::Aborted(result) => Some(result),
        ReceiptOutcome::RuntimeFailure { .. } => None,
    }
}

fn consensus_arguments(arguments: &[Value]) -> Vec<Vec<u8>> {
    arguments
        .iter()
        .map(|argument| {
            argument
                .serialize_to_vec()
                .expect("generated argument is serializable")
        })
        .collect()
}

fn stateful_tracker(store: &mut MarfStore) -> LimitedCostTracker {
    let mut database = store.as_clarity_db();
    database.begin();
    database
        .set_clarity_epoch_version(StacksEpochId::Epoch40)
        .expect("declare execution epoch");
    let tracker = LimitedCostTracker::new_mid_block(
        Network::TESTNET.is_mainnet(),
        Network::TESTNET.chain_id(),
        EPOCH_4_BLOCK_LIMIT,
        &mut database,
        StacksEpochId::Epoch40,
    )
    .expect("load epoch 4 costs");
    database
        .roll_back()
        .expect("reading the cost schedule writes nothing");
    tracker
}

fn compiled_stateful(case: &StatefulCase) -> StatefulObservation {
    let directory = tempfile::tempdir().expect("compiled state directory");
    let mut vm = Vm::open(Network::TESTNET, directory.path()).expect("open compiled VM");
    vm.begin_block(None, [0x37; 32])
        .expect("begin compiled block");
    let contract = stateful_contract();
    let tracker = vm
        .transaction_cost_tracker()
        .expect("compiled cost tracker");
    let deployment = vm
        .deploy_contract(
            contract.clone(),
            ClarityVersion::Clarity6,
            case.source,
            tracker,
        )
        .expect("deploy compiled contract");
    let tracker = vm
        .transaction_cost_tracker()
        .expect("compiled cost tracker");
    let call = receipt(
        vm.execute_contract_call_outcome(
            contract.issuer.clone().into(),
            None,
            contract.clone(),
            "mutate",
            &consensus_arguments(&case.arguments),
            &tracker,
        )
        .expect("execute compiled call"),
    );
    let tracker = vm
        .transaction_cost_tracker()
        .expect("compiled cost tracker");
    let snapshot = receipt(
        vm.execute_contract_call_outcome(
            contract.issuer.clone().into(),
            None,
            contract,
            "snapshot",
            &consensus_arguments(&case.snapshot_arguments),
            &tracker,
        )
        .expect("read compiled state"),
    );
    let root = vm.seal_block().expect("seal compiled state").0;
    StatefulObservation {
        deployment,
        call,
        snapshot,
        root,
    }
}

fn interpreted_stateful(case: &StatefulCase) -> StatefulObservation {
    let directory = tempfile::tempdir().expect("interpreted state directory");
    let mut store =
        MarfStore::open(Network::TESTNET, directory.path()).expect("open interpreter state");
    store
        .begin(None, [0x37; 32])
        .expect("begin interpreted block");
    let contract = stateful_contract();
    let tracker = stateful_tracker(&mut store);
    let deployment = nano_oracle::deploy_contract(
        &mut store,
        contract.clone(),
        ClarityVersion::Clarity6,
        case.source,
        tracker,
    )
    .expect("deploy interpreted contract");
    let tracker = stateful_tracker(&mut store);
    let call = receipt(
        nano_oracle::execute_contract_call_outcome(
            &mut store,
            contract.issuer.clone().into(),
            None,
            contract.clone(),
            "mutate",
            &consensus_arguments(&case.arguments),
            tracker,
        )
        .expect("execute interpreted call"),
    );
    let tracker = stateful_tracker(&mut store);
    let snapshot = receipt(
        nano_oracle::execute_contract_call_outcome(
            &mut store,
            contract.issuer.clone().into(),
            None,
            contract,
            "snapshot",
            &consensus_arguments(&case.snapshot_arguments),
            tracker,
        )
        .expect("read interpreted state"),
    );
    let root = store.seal().expect("seal interpreted state").0;
    StatefulObservation {
        deployment,
        call,
        snapshot,
        root,
    }
}

/// Compare state, transaction receipts and roots for one generated public call.
#[must_use]
pub fn clarity_stateful_receipt_differential(input: &[u8]) -> u8 {
    let Some(input) = within(input, MAX_CLARITY_STATEFUL_BYTES) else {
        return 0;
    };
    let Some((&template, input)) = input.split_first() else {
        return 0;
    };
    let Some((&flags, input)) = input.split_first() else {
        return 0;
    };
    let Some((amount, input)) = input.split_at_checked(8) else {
        return 0;
    };
    let Some((key, note)) = input.split_at_checked(8) else {
        return 0;
    };
    let amount = u128::from(u64::from_le_bytes(
        amount.try_into().expect("fixed integer width"),
    )) + 1;
    let key = u128::from(u64::from_le_bytes(
        key.try_into().expect("fixed integer width"),
    ));
    let template = template % 6;
    let case = stateful_case(template, amount, key, flags & 1 != 0, note);
    let compiled = compiled_stateful(&case);
    let interpreted = interpreted_stateful(&case);

    let compiled_call = receipt_result(&compiled.call).expect("generated call has a receipt");
    assert_ne!(
        compiled_call.cost,
        ExecutionCost::ZERO,
        "receipt comparison must use metered execution"
    );
    assert!(
        !compiled_call.events.is_empty() || matches!(compiled.call, ReceiptOutcome::Aborted(_)),
        "successful generated call must emit an event"
    );
    if case.expects_assets {
        assert_ne!(
            compiled_call.assets,
            clarity::vm::contexts::AssetMap::default(),
            "token case must exercise AssetMap"
        );
    }
    assert_eq!(
        compiled, interpreted,
        "compiled and interpreted stateful observations diverged"
    );
    1 << template
}
