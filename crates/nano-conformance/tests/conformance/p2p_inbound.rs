//! stacks-core's own codec, dialling nano's listener.
//!
//! `p2p_wire.rs` proves the bytes agree. `nano-p2p/tests/loopback.rs` proves nano
//! answers nano. Neither shows what task 054 actually asks for — that *a stock
//! `stacks-node` can complete the handshake with nano and exchange inventory in
//! both directions* — because both of them have nano on at least one end of the
//! conversation.
//!
//! This puts the reference implementation on the dialling end. Every message sent
//! is built, signed and serialised by `stackslib`, and every reply is deserialised
//! and authenticated by `stackslib`, over a real socket. What is left between that
//! and a stock node is the stock node's own scheduler; the wire, the handshake, the
//! sequence pairing and the payloads are all the reference implementation's here.

use std::time::Duration;

use nano_crypto::StacksPrivateKey;
use nano_p2p::wire::{ChainView, services};
use nano_p2p::{InboundLimits, Listener, LocalPeer, Protocol, Service};
use nano_primitives::{BitVec, BitcoinHeaderHash, ConsensusHash, Hash160};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use blockstack_lib::net::{
    GetNakamotoInvData, HandshakeData, PingData, Preamble as CorePreamble, StacksMessage,
    StacksMessageType,
};
use blockstack_lib::util_lib::strings::UrlString;
use stacks_common::codec::{MAX_MESSAGE_LEN, PREAMBLE_ENCODED_SIZE, StacksMessageCodec};
use stacks_common::types::StacksPublicKeyBuffer;
use stacks_common::types::chainstate::BurnchainHeaderHash;
use stacks_common::types::net::PeerAddress as CorePeerAddress;
use stacks_common::util::secp256k1::{Secp256k1PrivateKey, Secp256k1PublicKey};

/// The cycle nano's listener will claim to know, so an inventory and a refusal can
/// be told apart.
const KNOWN_CYCLE: ConsensusHash = ConsensusHash::from_bytes([0x4b; 20]);

/// A Bitcoin view for both ends. Self-consistent is all the protocol asks; neither
/// side has a burnchain in this test.
const TIP: u64 = 900_000;

fn nano_view() -> ChainView {
    ChainView::new(
        TIP,
        BitcoinHeaderHash::from_bytes([0x5a; 32]),
        BitcoinHeaderHash::from_bytes([0xc3; 32]),
    )
    .expect("a tip above the confirmation window")
}

/// What nano tells a peer, when the peer is stacks-core.
struct NanoNode;

impl Service for NanoNode {
    fn chain_view(&self) -> ChainView {
        nano_view()
    }

    fn neighbors(&self) -> Vec<nano_p2p::NeighborAddress> {
        vec![nano_p2p::NeighborAddress {
            address: nano_p2p::PeerAddress::from_ip(std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                203, 0, 113, 7,
            ))),
            port: 20444,
            public_key_hash: Hash160::from_bytes([0x77; 20]),
        }]
    }

    fn tenure_inventory(&self, cycle_start: ConsensusHash) -> Option<BitVec<2100>> {
        (cycle_start == KNOWN_CYCLE).then(|| {
            let mut tenures = BitVec::<2100>::zeros(2100).expect("a cycle-length bit vector");
            for index in [0_u16, 5, 2099] {
                tenures.set(index, true).expect("in bounds");
            }
            tenures
        })
    }
}

/// One conversation, driven entirely by `stackslib`'s codec.
struct Reference {
    stream: tokio::net::TcpStream,
    key: Secp256k1PrivateKey,
    seq: u32,
}

impl Reference {
    /// Send a payload the way stacks-core sends it, and read back what nano says.
    ///
    /// The reply is deserialised with `StacksMessage::consensus_deserialize`, which
    /// applies every bound the reference implementation applies — the preamble
    /// checks, the relayer cap, the payload's own validation — so a reply nano gets
    /// wrong fails here rather than being quietly accepted.
    async fn exchange(&mut self, payload: StacksMessageType) -> StacksMessage {
        self.seq = self.seq.wrapping_add(1);
        let mut message = StacksMessage::new(
            Protocol::testnet().peer_version,
            Protocol::testnet().network_id,
            TIP,
            &BurnchainHeaderHash([0x5a; 32]),
            TIP - 7,
            &BurnchainHeaderHash([0xc3; 32]),
            payload,
        );
        message.sign(self.seq, &self.key).expect("sign");
        self.stream
            .write_all(&message.serialize_to_vec())
            .await
            .expect("send");

        let mut header = vec![0; PREAMBLE_ENCODED_SIZE as usize];
        self.stream
            .read_exact(&mut header)
            .await
            .expect("read a preamble");
        let preamble =
            CorePreamble::consensus_deserialize(&mut &header[..]).expect("stacks-core decodes it");
        assert!(
            preamble.payload_len < MAX_MESSAGE_LEN,
            "nano announced a frame stacks-core would refuse"
        );
        // The reply has to carry the request's sequence number: that is how
        // stacks-core pairs a reply to the handle waiting for it, and a reply with a
        // fresh one would be treated as unsolicited and dropped.
        assert_eq!(preamble.seq, self.seq, "nano's reply is not paired");
        let mut frame = vec![0; preamble.payload_len as usize];
        self.stream
            .read_exact(&mut frame)
            .await
            .expect("read the frame");
        let mut whole = header;
        whole.extend_from_slice(&frame);
        StacksMessage::consensus_deserialize(&mut &whole[..])
            .expect("stacks-core decodes nano's reply")
    }
}

/// Bring nano up as a listening peer and hand back a stacks-core client for it.
async fn dial_nano() -> (Reference, Secp256k1PublicKey) {
    let listener = Listener::bind("127.0.0.1:0".parse().expect("a loopback address"))
        .await
        .expect("bind");
    let address = listener.local_addr().expect("the bound address");
    let mut local = LocalPeer::quiet(
        StacksPrivateKey::from_seed(b"nano listener"),
        address.port(),
    );
    local.address = nano_p2p::PeerAddress::from_ip(address.ip());
    local.data_url = format!("http://{address}");
    local.services = services::RPC | services::RELAY;
    let nano_key =
        Secp256k1PublicKey::from_slice(&local.private_key.public_key().to_bytes_compressed())
            .expect("nano's key is a secp256k1 key");

    tokio::spawn(async move {
        let mut conversations = tokio::task::JoinSet::new();
        while let Ok((stream, from)) = listener.accept().await {
            let local = local.clone();
            conversations.spawn(async move {
                let _ = nano_p2p::serve_peer(
                    stream,
                    from,
                    &local,
                    Protocol::testnet(),
                    &NanoNode,
                    InboundLimits {
                        timeout: Duration::from_secs(10),
                        ..InboundLimits::default()
                    },
                )
                .await;
            });
        }
    });

    let stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect to nano");
    stream.set_nodelay(true).expect("nodelay");
    (
        Reference {
            stream,
            key: Secp256k1PrivateKey::from_slice(&[0x3c; 32]).expect("a valid key"),
            seq: 0x7000_0000,
        },
        nano_key,
    )
}

/// A stock handshake, a ping and an inventory exchange, all in stacks-core's codec.
#[tokio::test]
async fn stacks_core_handshakes_with_nano_and_exchanges_inventory() {
    let (mut peer, nano_key) = dial_nano().await;

    // ---- The handshake, built the way `HandshakeData::from_local_peer` builds it.
    let handshake = HandshakeData {
        addrbytes: CorePeerAddress::from_ipv4(127, 0, 0, 1),
        port: 20444,
        services: 0x03,
        node_public_key: StacksPublicKeyBuffer::from_public_key(&Secp256k1PublicKey::from_private(
            &peer.key,
        )),
        // Must lead nano's tip, or nano is right to reject the key as revoked.
        expire_block_height: TIP + 10_000,
        data_url: UrlString::try_from("http://127.0.0.1:20443".to_string()).expect("a valid URL"),
    };
    let reply = peer.exchange(StacksMessageType::Handshake(handshake)).await;

    let accept = match reply.payload {
        StacksMessageType::HandshakeAccept(ref accept) => accept.clone(),
        ref other => panic!(
            "nano answered a handshake with a {}",
            other.get_message_name()
        ),
    };
    // The key nano announced is the key nano signed with, checked by the reference
    // implementation's own verifier rather than by nano's.
    assert_eq!(
        accept.handshake.node_public_key,
        StacksPublicKeyBuffer::from_public_key(&nano_key)
    );
    reply
        .verify_secp256k1(&accept.handshake.node_public_key)
        .expect("stacks-core accepts nano's signature");
    // And the things a stock node then records about a peer: where to dial it back,
    // and where its HTTP is. An empty or unparseable `data_url` would have failed in
    // `consensus_deserialize` already, which is the point of decoding with theirs.
    assert!(accept.handshake.port != 0);
    assert!(
        accept
            .handshake
            .data_url
            .parse_to_block_url()
            .expect("nano's data url is a block url")
            .host_str()
            .is_some()
    );
    assert_eq!(
        accept.heartbeat_interval,
        nano_p2p::inbound::HEARTBEAT_INTERVAL_SECS
    );
    assert_eq!(
        accept.handshake.services & u16::from(blockstack_lib::net::ServiceFlags::RPC as u8),
        u16::from(blockstack_lib::net::ServiceFlags::RPC as u8)
    );

    // ---- Liveness.
    let reply = peer
        .exchange(StacksMessageType::Ping(PingData { nonce: 0xfeed_face }))
        .await;
    match reply.payload {
        StacksMessageType::Pong(ref pong) => assert_eq!(pong.nonce, 0xfeed_face),
        ref other => panic!("nano answered a ping with a {}", other.get_message_name()),
    }
    reply
        .verify_secp256k1(&accept.handshake.node_public_key)
        .expect("every reply stays signed by the handshake key");

    // ---- Neighbours: a stock node's walk is how nano gets into other peer tables.
    let reply = peer.exchange(StacksMessageType::GetNeighbors).await;
    match reply.payload {
        StacksMessageType::Neighbors(ref neighbors) => {
            assert_eq!(neighbors.neighbors.len(), 1);
            assert_eq!(neighbors.neighbors[0].port, 20444);
        }
        ref other => panic!(
            "nano answered GetNeighbors with a {}",
            other.get_message_name()
        ),
    }

    // ---- Inventory, in both directions: nano's answer decoded as a
    // `NakamotoInvData` by the implementation that defines what one is.
    let reply = peer
        .exchange(StacksMessageType::GetNakamotoInv(GetNakamotoInvData {
            consensus_hash: blockstack_lib::chainstate::burn::ConsensusHash(
                *KNOWN_CYCLE.as_bytes(),
            ),
        }))
        .await;
    match reply.payload {
        StacksMessageType::NakamotoInv(ref inventory) => {
            assert!(inventory.has_ith_tenure(0));
            assert!(inventory.has_ith_tenure(5));
            assert!(inventory.has_ith_tenure(2099));
            assert!(!inventory.has_ith_tenure(1));
            assert_eq!(inventory.tenures.len(), 2100);
        }
        ref other => panic!(
            "nano answered GetNakamotoInv with a {}",
            other.get_message_description()
        ),
    }

    // A cycle nano does not know is nacked rather than answered with zeroes, which
    // a stock node reads as "ask somebody else" instead of "it has nothing".
    let reply = peer
        .exchange(StacksMessageType::GetNakamotoInv(GetNakamotoInvData {
            consensus_hash: blockstack_lib::chainstate::burn::ConsensusHash([0xee; 20]),
        }))
        .await;
    match reply.payload {
        StacksMessageType::Nack(ref nack) => assert_eq!(
            nack.error_code,
            blockstack_lib::net::NackErrorCodes::NoSuchBurnchainBlock
        ),
        ref other => panic!(
            "nano answered an unknown cycle with a {}",
            other.get_message_name()
        ),
    }
}

/// A stock node asking before it handshakes is nacked, in its own vocabulary.
#[tokio::test]
async fn stacks_core_is_told_to_handshake_first() {
    let (mut peer, _) = dial_nano().await;
    let reply = peer.exchange(StacksMessageType::GetNeighbors).await;
    match reply.payload {
        StacksMessageType::Nack(ref nack) => assert_eq!(
            nack.error_code,
            blockstack_lib::net::NackErrorCodes::HandshakeRequired
        ),
        ref other => panic!(
            "nano served an unauthenticated request with a {}",
            other.get_message_name()
        ),
    }
}
