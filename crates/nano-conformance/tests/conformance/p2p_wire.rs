//! Every p2p message nano encodes, checked against stacks-core's own codec.
//!
//! `nano-p2p` has to reimplement the wire format because the release node may not
//! link `stackslib` (`release_dependencies.rs`), which leaves no shared code and
//! therefore no shared bugs — a fabricated preamble field or a byte of padding in
//! the wrong place would show up as a peer silently dropping the connection, at
//! which point the answer costs a packet capture to find.
//!
//! `stackslib` is a dev-dependency here, so it is the cheapest possible oracle.
//! Every message goes round the loop four ways:
//!
//! 1. stacks-core encodes it; nano decodes it.
//! 2. nano re-encodes the frame *from its decoded structure* and matches theirs
//!    byte for byte. This is the half that actually tests nano's encoder — a
//!    decoded message keeps the bytes it arrived as, so comparing those would
//!    prove nothing.
//! 3. nano verifies the signature stacks-core made.
//! 4. nano signs the same payload and stacks-core decodes it and verifies *that*
//!    signature.
//!
//! Anything short of all four leaves a direction untested, and both directions
//! matter: nano has to accept what the network sends and send what it accepts.

use std::fs;
use std::path::Path;

use nano_p2p::wire::{self, PREAMBLE_LEN, Payload};
use proptest::prelude::*;

use blockstack_lib::chainstate::nakamoto::NakamotoBlock as CoreBlock;
use blockstack_lib::chainstate::stacks::StacksTransaction as CoreTransaction;
use blockstack_lib::net::{
    BlocksAvailableData, GetNakamotoInvData, GetPoxInv, HandshakeAcceptData, HandshakeData,
    NackData, NakamotoBlocksData, NakamotoInvData, NatPunchData,
    NeighborAddress as CoreNeighborAddress, NeighborsData, PingData, PongData,
    Preamble as CorePreamble, StackerDBHandshakeData, StacksMessage, StacksMessageType,
};
use blockstack_lib::util_lib::strings::UrlString;
use clarity::vm::types::QualifiedContractIdentifier;
use stacks_common::bitvec::BitVec as CoreBitVec;
use stacks_common::codec::StacksMessageCodec;
use stacks_common::types::StacksPublicKeyBuffer;
use stacks_common::types::chainstate::{BurnchainHeaderHash, ConsensusHash as CoreConsensusHash};
use stacks_common::types::net::PeerAddress as CorePeerAddress;
use stacks_common::util::hash::Hash160 as CoreHash160;
use stacks_common::util::secp256k1::{
    MessageSignature as CoreSignature, Secp256k1PrivateKey, Secp256k1PublicKey,
};

/// One key, used by both implementations, so that a signature either side makes
/// is one the other has to accept.
const KEY: [u8; 32] = [
    0x9f, 0x1b, 0x51, 0x92, 0x37, 0xa5, 0x35, 0x3e, 0x03, 0x37, 0xbf, 0x3d, 0x1e, 0x4b, 0x40, 0x60,
    0xc4, 0xd7, 0x51, 0xea, 0x60, 0x2f, 0x9e, 0x0d, 0x11, 0xbd, 0xc0, 0x0c, 0xd2, 0x2b, 0x1e, 0x53,
];

fn core_key() -> Secp256k1PrivateKey {
    Secp256k1PrivateKey::from_slice(&KEY).expect("a valid key")
}

fn nano_key() -> nano_crypto::StacksPrivateKey {
    nano_crypto::StacksPrivateKey::from_bytes(KEY).expect("a valid key")
}

/// A Bitcoin view for the messages under test. Any self-consistent one will do:
/// the codec does not interpret it, and the peer checks it against a chain that
/// is not part of this test.
const TIP: u64 = 961_200;
const TIP_HASH: [u8; 32] = [0x5a; 32];
const STABLE_HASH: [u8; 32] = [0xc3; 32];

fn core_message(payload: StacksMessageType, relayers: usize) -> StacksMessage {
    let key = core_key();
    let mut message = StacksMessage::new(
        nano_p2p::session::PEER_VERSION_MAINNET_MAJOR | nano_p2p::session::PEER_VERSION_EPOCH_4_0,
        nano_p2p::Protocol::mainnet().network_id,
        TIP,
        &BurnchainHeaderHash(TIP_HASH),
        TIP - u64::from(u8::try_from(nano_p2p::STABLE_CONFIRMATIONS).unwrap()),
        &BurnchainHeaderHash(STABLE_HASH),
        payload,
    );
    message.sign(0x1234_5678, &key).expect("sign");
    for index in 0..relayers {
        let hop = CoreNeighborAddress {
            addrbytes: CorePeerAddress([u8::try_from(index).unwrap_or(0xff); 16]),
            port: 20444,
            public_key_hash: CoreHash160([u8::try_from(index).unwrap_or(0xff); 20]),
        };
        message
            .sign_relay(&key, 0x9000_0000 + u32::try_from(index).unwrap_or(0), &hop)
            .expect("relay");
    }
    message
}

/// Decode a message with nano, which is also where a decode failure is reported
/// against the bytes stacks-core produced.
fn nano_decode(bytes: &[u8]) -> nano_p2p::Message {
    let preamble = nano_p2p::Preamble::decode(&bytes[..PREAMBLE_LEN])
        .unwrap_or_else(|error| panic!("nano cannot decode the preamble: {error}"));
    nano_p2p::Message::decode(preamble, bytes[PREAMBLE_LEN..].to_vec())
        .unwrap_or_else(|error| panic!("nano cannot decode the payload: {error}"))
}

/// Put one payload through all four directions.
fn exchange(payload: StacksMessageType, relayers: usize) {
    let theirs = core_message(payload, relayers);
    let bytes = theirs.serialize_to_vec();

    // 1 and 2: nano decodes their bytes, and reproduces them from what it decoded.
    let ours = nano_decode(&bytes);
    assert_eq!(
        ours.relayers.len(),
        relayers,
        "nano lost a relayer from a {}",
        ours.payload.name()
    );
    let reframed = wire::encode_frame(&ours.relayers, &ours.payload)
        .unwrap_or_else(|error| panic!("nano cannot re-encode a {}: {error}", ours.payload.name()));
    assert_eq!(
        reframed,
        ours.frame(),
        "nano re-encodes a {} differently from stacks-core",
        ours.payload.name()
    );
    assert_eq!(ours.encode(), bytes);

    // 3: nano authenticates a signature stacks-core made.
    let public_key = nano_crypto::StacksPublicKey::from_bytes(
        &Secp256k1PublicKey::from_private(&core_key()).to_bytes_compressed(),
    )
    .expect("a valid key");
    ours.verify(&public_key)
        .unwrap_or_else(|error| panic!("nano rejects stacks-core's signature: {error}"));
    assert_eq!(
        ours.signer().expect("recover").to_bytes_compressed(),
        public_key.to_bytes_compressed(),
    );

    // 4: stacks-core decodes and authenticates a message nano signed. Nano only
    // originates messages, so this direction carries no relayers.
    let view = nano_p2p::ChainView::new(
        TIP,
        nano_primitives::BitcoinHeaderHash::from_bytes(TIP_HASH),
        nano_primitives::BitcoinHeaderHash::from_bytes(STABLE_HASH),
    )
    .expect("a tip above the confirmation window");
    let signed = nano_p2p::Message::sign(
        theirs.preamble.peer_version,
        theirs.preamble.network_id,
        &view,
        theirs.preamble.seq,
        nano_decode(&bytes).payload,
        &nano_key(),
    )
    .expect("nano signs its own message");
    let encoded = signed.encode();
    let mine = StacksMessage::consensus_deserialize(&mut &encoded[..])
        .unwrap_or_else(|error| panic!("stacks-core cannot decode nano's message: {error}"));
    assert_eq!(mine.payload, theirs.payload);
    assert!(mine.relayers.is_empty());
    mine.verify_secp256k1(&StacksPublicKeyBuffer::from_public_key(
        &Secp256k1PublicKey::from_private(&core_key()),
    ))
    .unwrap_or_else(|error| panic!("stacks-core rejects nano's signature: {error}"));
}

fn handshake() -> impl Strategy<Value = HandshakeData> {
    // A URL a peer will accept: HTTP(S), a host, no query, no fragment, no
    // credentials. The empty string is the form a node with no routable data URL
    // sends, and is the one nano itself sends, so it has to round-trip.
    let urls = prop::sample::select(vec![
        "",
        "http://1.2.3.4:20443",
        "https://api.example.com",
        "http://[2001:db8::1]:20443/",
    ]);
    (
        any::<[u8; 16]>(),
        1_u16..=u16::MAX,
        any::<u16>(),
        any::<u64>(),
        urls,
    )
        .prop_map(|(address, port, services, expire, url)| HandshakeData {
            addrbytes: CorePeerAddress(address),
            port,
            services,
            node_public_key: StacksPublicKeyBuffer::from_public_key(
                &Secp256k1PublicKey::from_private(&core_key()),
            ),
            expire_block_height: expire,
            data_url: UrlString::try_from(url.to_string()).expect("a valid URL"),
        })
}

fn accept() -> impl Strategy<Value = HandshakeAcceptData> {
    (handshake(), any::<u32>()).prop_map(|(handshake, heartbeat_interval)| HandshakeAcceptData {
        handshake,
        heartbeat_interval,
    })
}

fn neighbors() -> impl Strategy<Value = NeighborsData> {
    prop::collection::vec(
        (any::<[u8; 16]>(), any::<u16>(), any::<[u8; 20]>()).prop_map(|(address, port, hash)| {
            CoreNeighborAddress {
                addrbytes: CorePeerAddress(address),
                port,
                public_key_hash: CoreHash160(hash),
            }
        }),
        // 128 is `MAX_NEIGHBORS_DATA_LEN`, and the empty reply is what a peer
        // that knows nobody sends.
        0..=128_usize,
    )
    .prop_map(|neighbors| NeighborsData { neighbors })
}

fn stackerdb_handshake() -> impl Strategy<Value = StackerDBHandshakeData> {
    // Real contract identifiers rather than random bytes: stacks-core validates
    // the address version byte and the contract name on decode, so a generator
    // that ignored either would be testing its own rejections.
    let contracts = prop::sample::subsequence(
        vec![
            "SP000000000000000000002Q6VF78.signers-0-0",
            "SP000000000000000000002Q6VF78.signers-1-1",
            "SP000000000000000000002Q6VF78.miners",
            "ST000000000000000000002AMW42H.signers-0-4",
        ],
        0..=4,
    );
    (any::<[u8; 20]>(), contracts).prop_map(|(consensus_hash, contracts)| StackerDBHandshakeData {
        rc_consensus_hash: CoreConsensusHash(consensus_hash),
        smart_contracts: contracts
            .into_iter()
            .map(|id| QualifiedContractIdentifier::parse(id).expect("a valid contract identifier"))
            .collect(),
    })
}

fn tenure_inventory() -> impl Strategy<Value = NakamotoInvData> {
    // 2100 is the mainnet reward cycle length, which is also the bit vector's
    // maximum; 1 is its minimum, because a zero-length bit vector is refused.
    prop::collection::vec(any::<bool>(), 1..=2100_usize).prop_map(|bits| NakamotoInvData {
        tenures: CoreBitVec::<2100>::try_from(bits.as_slice()).expect("a bounded bit vector"),
    })
}

fn payload() -> impl Strategy<Value = StacksMessageType> {
    prop_oneof![
        handshake().prop_map(StacksMessageType::Handshake),
        accept().prop_map(StacksMessageType::HandshakeAccept),
        Just(StacksMessageType::HandshakeReject),
        Just(StacksMessageType::GetNeighbors),
        neighbors().prop_map(StacksMessageType::Neighbors),
        any::<u32>().prop_map(|code| StacksMessageType::Nack(NackData { error_code: code })),
        any::<u32>().prop_map(|nonce| StacksMessageType::Ping(PingData { nonce })),
        any::<u32>().prop_map(|nonce| StacksMessageType::Pong(PongData { nonce })),
        any::<u32>().prop_map(StacksMessageType::NatPunchRequest),
        (any::<[u8; 16]>(), any::<u16>(), any::<u32>()).prop_map(|(address, port, nonce)| {
            StacksMessageType::NatPunchReply(NatPunchData {
                addrbytes: CorePeerAddress(address),
                port,
                nonce,
            })
        }),
        (accept(), stackerdb_handshake())
            .prop_map(|(accept, db)| StacksMessageType::StackerDBHandshakeAccept(accept, db)),
        any::<[u8; 20]>().prop_map(
            |hash| StacksMessageType::GetNakamotoInv(GetNakamotoInvData {
                consensus_hash: CoreConsensusHash(hash),
            })
        ),
        tenure_inventory().prop_map(StacksMessageType::NakamotoInv),
    ]
}

proptest! {
    /// The preamble, on its own, in both directions.
    ///
    /// It gets its own case because a signed message's preamble cannot carry
    /// arbitrary values — `payload_len` and `signature` are derived, and nano
    /// always writes a zero `additional_data` because the protocol reserves it —
    /// so the field would otherwise never be exercised at all.
    #[test]
    fn the_preamble_round_trips_with_stacks_core(
        peer_version in any::<u32>(),
        network_id in any::<u32>(),
        seq in any::<u32>(),
        stable_height in 0_u64..u64::MAX / 2,
        gap in 1_u64..1000,
        tip_hash in any::<[u8; 32]>(),
        stable_hash in any::<[u8; 32]>(),
        additional_data in any::<u32>(),
        signature in any::<[u8; 32]>(),
        // 5 is the smallest payload the protocol allows (an empty relayer vector
        // and a type byte); the ceiling is `MAX_MESSAGE_LEN - PREAMBLE_ENCODED_SIZE`.
        payload_len in 5_u32..=16_777_889,
    ) {
        let mut bytes = [0_u8; 65];
        bytes[1..33].copy_from_slice(&signature);
        let theirs = CorePreamble {
            peer_version,
            network_id,
            seq,
            burn_block_height: stable_height + gap,
            burn_block_hash: BurnchainHeaderHash(tip_hash),
            burn_stable_block_height: stable_height,
            burn_stable_block_hash: BurnchainHeaderHash(stable_hash),
            additional_data,
            signature: CoreSignature(bytes),
            payload_len,
        };
        let encoded = theirs.serialize_to_vec();
        prop_assert_eq!(encoded.len(), PREAMBLE_LEN);
        let ours = nano_p2p::Preamble::decode(&encoded).expect("nano decodes the preamble");
        prop_assert_eq!(ours.peer_version, theirs.peer_version);
        prop_assert_eq!(ours.network_id, theirs.network_id);
        prop_assert_eq!(ours.seq, theirs.seq);
        prop_assert_eq!(ours.bitcoin_height, theirs.burn_block_height);
        prop_assert_eq!(ours.bitcoin_hash.as_bytes(), &theirs.burn_block_hash.0);
        prop_assert_eq!(ours.stable_bitcoin_height, theirs.burn_stable_block_height);
        prop_assert_eq!(ours.stable_bitcoin_hash.as_bytes(), &theirs.burn_stable_block_hash.0);
        prop_assert_eq!(ours.additional_data, theirs.additional_data);
        prop_assert_eq!(ours.signature.as_bytes(), &theirs.signature.0);
        prop_assert_eq!(ours.payload_len, theirs.payload_len);
        prop_assert_eq!(ours.encode(), encoded);
    }

    /// Every modelled message, with and without a relayer chain.
    #[test]
    fn every_message_round_trips_with_stacks_core(
        payload in payload(),
        // 16 is `MAX_RELAYERS_LEN`; a message nano originates has none, and one
        // that reached us through the gossip mesh has up to that many.
        relayers in 0..=16_usize,
    ) {
        exchange(payload, relayers);
    }
}

/// A preamble whose stable height does not sit below its tip is refused.
///
/// stacks-core refuses it too, on the grounds that a node deriving one from the
/// other cannot produce it; accepting it would let a peer skip the `-7` and claim
/// a stable view of an unconfirmed block.
#[test]
fn a_preamble_with_no_stable_height_is_refused() {
    let build = |tip: u64, stable: u64| {
        CorePreamble {
            peer_version: 0x1800_0010,
            network_id: nano_p2p::Protocol::mainnet().network_id,
            seq: 1,
            burn_block_height: tip,
            burn_block_hash: BurnchainHeaderHash([0x11; 32]),
            burn_stable_block_height: stable,
            burn_stable_block_hash: BurnchainHeaderHash([0x22; 32]),
            additional_data: 0,
            signature: CoreSignature([0; 65]),
            payload_len: 5,
        }
        .serialize_to_vec()
    };
    for (tip, stable) in [(100, 100), (100, 101)] {
        let bytes = build(tip, stable);
        assert!(nano_p2p::Preamble::decode(&bytes).is_err());
        // ... and stacks-core agrees, which is what makes this a shared rule
        // rather than nano being stricter than the network.
        assert!(CorePreamble::consensus_deserialize(&mut &bytes[..]).is_err());
    }
    assert!(nano_p2p::Preamble::decode(&build(101, 100)).is_ok());
}

/// A frame length outside the protocol's bounds is refused before anything is
/// allocated for it.
#[test]
fn an_impossible_frame_length_is_refused() {
    let mut bytes = CorePreamble {
        peer_version: 0x1800_0010,
        network_id: nano_p2p::Protocol::mainnet().network_id,
        seq: 1,
        burn_block_height: 100,
        burn_block_hash: BurnchainHeaderHash([0x11; 32]),
        burn_stable_block_height: 93,
        burn_stable_block_hash: BurnchainHeaderHash([0x22; 32]),
        additional_data: 0,
        signature: CoreSignature([0; 65]),
        payload_len: 5,
    }
    .serialize_to_vec();
    for length in [0_u32, 4, u32::MAX, 16_777_890] {
        bytes[PREAMBLE_LEN - 4..].copy_from_slice(&length.to_be_bytes());
        assert!(
            nano_p2p::Preamble::decode(&bytes).is_err(),
            "nano accepted a frame length of {length}"
        );
    }
}

/// The epoch-2.x messages are recognised and discarded, not treated as garbage.
///
/// A mainnet peer sends these unsolicited. Failing to parse one would drop the
/// connection, and modelling one would be code for a chain nano does not follow,
/// so the correct behaviour is to name the identifier and skip the body.
#[test]
fn epoch_two_messages_are_recognised_and_discarded() {
    for (payload, expected) in [
        (
            StacksMessageType::GetPoxInv(GetPoxInv {
                consensus_hash: CoreConsensusHash([0x33; 20]),
                num_cycles: 12,
            }),
            7_u8,
        ),
        (
            StacksMessageType::BlocksAvailable(BlocksAvailableData {
                available: vec![(
                    CoreConsensusHash([0x44; 20]),
                    BurnchainHeaderHash([0x55; 32]),
                )],
            }),
            9,
        ),
    ] {
        let bytes = core_message(payload, 0).serialize_to_vec();
        let ours = nano_decode(&bytes);
        match ours.payload {
            Payload::Unhandled(id) => assert_eq!(id, expected),
            other => panic!("nano modelled an epoch-2.x message as a {}", other.name()),
        }
        // Discarded, and unforgeable: a message nano did not model is one it
        // cannot re-encode, so it can never be relayed as something it is not.
        assert!(wire::encode_frame(&ours.relayers, &ours.payload).is_err());
        // Still authenticated, because verification hashes the bytes that
        // arrived rather than a re-encoding of them.
        let public_key = nano_crypto::StacksPublicKey::from_bytes(
            &Secp256k1PublicKey::from_private(&core_key()).to_bytes_compressed(),
        )
        .expect("a valid key");
        ours.verify(&public_key).expect("still authenticated");
    }
}

/// An identifier the protocol never assigned is an error, not something to skip.
#[test]
fn an_undefined_message_identifier_is_refused() {
    let mut bytes = core_message(StacksMessageType::GetNeighbors, 0).serialize_to_vec();
    // The frame is a zero-length relayer vector then the type byte.
    let type_byte = PREAMBLE_LEN + 4;
    for id in [20_u8, 29, 100, 254, 255] {
        bytes[type_byte] = id;
        let preamble = nano_p2p::Preamble::decode(&bytes[..PREAMBLE_LEN]).expect("preamble");
        assert!(
            nano_p2p::Message::decode(preamble, bytes[PREAMBLE_LEN..].to_vec()).is_err(),
            "nano accepted the undefined message identifier {id}"
        );
    }
}

fn fixture_blocks() -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/nakamoto/blocks");
    let mut blocks: Vec<_> = fs::read_dir(directory)
        .expect("read fixture blocks")
        .map(|entry| {
            let path = entry.expect("fixture entry").path();
            let bytes = fs::read(&path).expect("read fixture block");
            (path, bytes)
        })
        .collect();
    blocks.sort_by(|left, right| left.0.cmp(&right.0));
    blocks
}

/// Pushed blocks and relayed transactions, using real mainnet ones.
///
/// These two payloads are generated from fixtures rather than proptest because a
/// randomly assembled block or transaction is one no encoder on either side would
/// ever be asked to produce, while a captured one is exactly what the network
/// pushes.
#[test]
fn pushed_blocks_and_transactions_round_trip_with_stacks_core() {
    let fixtures = fixture_blocks();
    assert!(!fixtures.is_empty(), "the fixture tree holds blocks");
    let mut blocks = Vec::new();
    let mut transactions: Vec<CoreTransaction> = Vec::new();
    for (path, bytes) in &fixtures {
        let block = CoreBlock::consensus_deserialize(&mut &bytes[..])
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        transactions.extend(block.txs().map(|transaction| match transaction {
            blockstack_lib::chainstate::nakamoto::TxToProcess::Execute(transaction)
            | blockstack_lib::chainstate::nakamoto::TxToProcess::Skip {
                tx: transaction, ..
            } => transaction.clone(),
        }));
        blocks.push(block);
    }

    for transaction in &transactions {
        exchange(StacksMessageType::Transaction(transaction.clone()), 0);
    }

    // One block, and then as many as the protocol allows in one message: the
    // count is a length prefix, so a single block would not exercise it.
    for count in [1, blocks.len().min(32)] {
        exchange(
            StacksMessageType::NakamotoBlocks(NakamotoBlocksData {
                blocks: blocks[..count].to_vec(),
            }),
            0,
        );
    }
}

/// A peer cannot make us validate the same block twice in one message.
///
/// Without the check the bound on pushed blocks is a bound on *distinct* work
/// only in the honest case: thirty-two copies of one block costs one message and
/// thirty-two validations.
#[test]
fn a_duplicated_pushed_block_is_refused() {
    let (path, bytes) = fixture_blocks()
        .into_iter()
        .next()
        .expect("a fixture block");
    let block = CoreBlock::consensus_deserialize(&mut &bytes[..])
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    // stacks-core's own deserializer refuses this, so the message has to be built
    // by hand rather than round-tripped: `NakamotoBlocksData`'s encoder is happy
    // to write it.
    let message = core_message(
        StacksMessageType::NakamotoBlocks(NakamotoBlocksData {
            blocks: vec![block.clone(), block],
        }),
        0,
    );
    let bytes = message.serialize_to_vec();
    let preamble = nano_p2p::Preamble::decode(&bytes[..PREAMBLE_LEN]).expect("preamble");
    assert!(nano_p2p::Message::decode(preamble, bytes[PREAMBLE_LEN..].to_vec()).is_err());
    assert!(StacksMessage::consensus_deserialize(&mut &bytes[..]).is_err());
}
