//! The Stacks peer-to-peer wire format.
//!
//! Every message is a fixed-size signed preamble, a relayer list and a payload.
//! The preamble carries the sender's Bitcoin view, which is what lets a peer
//! decide whether the sender follows the same rules before it spends anything
//! parsing the payload, and it is signed over
//! `Sha512_256(preamble_with_a_blank_signature ‖ relayers ‖ payload)` with the
//! sender's node key.
//!
//! Nano cannot reuse `stackslib`'s codec: the release node is forbidden from
//! linking a reference-node crate at all (see
//! `nano-conformance/tests/conformance/release_dependencies.rs`), so this is
//! nano's own. `stackslib` is a dev-dependency of `nano-conformance`, though,
//! which makes it a free differential oracle — `p2p_wire.rs` encodes with theirs
//! and decodes with this, both directions, and asserts byte equality.
//!
//! Two deliberate departures from `stackslib/src/net/codec.rs`:
//!
//! * A decoded message keeps the raw payload frame. Signature verification
//!   hashes the bytes that arrived rather than a re-encoding of what we made of
//!   them, so a message whose payload nano declines to model can still be
//!   authenticated and discarded, and no encoder disagreement can turn a valid
//!   signature into an invalid one.
//! * The epoch-2.x message types (`Blocks`, `Microblocks`, the block and
//!   `PoX` inventories) are decoded as [`Payload::Unhandled`] rather than modelled. A
//!   4.0-only node will never send or act on one, and a mainnet peer does send
//!   them unsolicited, so recognising the identifier and discarding the body is
//!   the whole of the correct behaviour. The same variant holds the
//!   `StackerDB` replication messages, which nano carries over HTTP today.

use std::fmt;

use nano_chainstate::{NakamotoBlock, NakamotoCodecError};
use nano_codec::{CodecError, Transaction};
use nano_crypto::{CryptoError, MessageSignature, StacksPrivateKey, StacksPublicKey};
use nano_primitives::{BitVec, BitVecError, BitcoinHeaderHash, ConsensusHash, Hash160, sha512_256};

/// The encoded size of a preamble, which is fixed and therefore how much has to
/// be read before a frame length is known.
pub const PREAMBLE_LEN: usize = 4 + 4 + 4 + 8 + 32 + 8 + 32 + 4 + 65 + 4;

/// The largest payload frame a peer may announce.
///
/// This mirrors `MAX_MESSAGE_LEN - PREAMBLE_ENCODED_SIZE` in `stacks-common`.
/// Bounding it before allocating is the only thing standing between a hostile
/// preamble and 16 MB of our memory per connection.
pub const MAX_PAYLOAD_LEN: u32 = (1 + 16 * 1024 * 1024) + 16 * (38 + 4);

/// A payload holds at least a zero-length relayer vector and a type byte.
const MIN_PAYLOAD_LEN: u32 = 5;

/// `stacks-common`'s `MAX_RELAYERS_LEN`: a message that has been forwarded more
/// than sixteen times is a loop.
const MAX_RELAYERS: u32 = 16;

/// `stackslib`'s `MAX_NEIGHBORS_DATA_LEN`.
const MAX_NEIGHBORS: u32 = 128;

/// `stackslib`'s `NAKAMOTO_BLOCKS_PUSHED_MAX`. This bounds the validation work a
/// peer can ask of us with one message, which is why it is enforced at decode.
const MAX_PUSHED_BLOCKS: u32 = 32;

/// A `StackerDBHandshakeData` announces at most this many contracts, and the
/// count is a single byte on the wire.
const MAX_ANNOUNCED_CONTRACTS: usize = 256;

/// Clarity's `MAX_STRING_LEN`, which bounds a `UrlString`.
const MAX_URL_LEN: u8 = 128;

/// `CONTRACT_MIN_NAME_LENGTH` and `CONTRACT_MAX_NAME_LENGTH`.
const CONTRACT_NAME_LEN: std::ops::RangeInclusive<usize> = 1..=40;

/// Errors raised while decoding, encoding or authenticating a p2p message.
#[derive(Debug)]
pub enum WireError {
    EndOfInput,
    /// A length field describes more bytes than the frame holds, or more items
    /// than the protocol allows.
    Length,
    /// The frame length in the preamble is outside the protocol's bounds.
    FrameLength(u32),
    /// A preamble whose stable height is not below its tip height cannot come
    /// from a node with a Bitcoin view.
    UnstableView,
    /// A message identifier no version of the protocol has defined.
    UnknownMessage(u8),
    InvalidUtf8,
    InvalidContractName,
    /// The payload nano encoded is longer than the protocol permits.
    PayloadTooLong(usize),
    DuplicateBlock,
    Block(NakamotoCodecError),
    Transaction(CodecError),
    BitVec(BitVecError),
    Signature(CryptoError),
}

impl fmt::Display for WireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EndOfInput => formatter.write_str("unexpected end of message"),
            Self::Length => formatter.write_str("invalid length"),
            Self::FrameLength(length) => write!(formatter, "invalid payload frame length {length}"),
            Self::UnstableView => {
                formatter.write_str("preamble tip height is not above its stable height")
            }
            Self::UnknownMessage(id) => write!(formatter, "unknown message identifier {id}"),
            Self::InvalidUtf8 => formatter.write_str("string is not valid UTF-8"),
            Self::InvalidContractName => formatter.write_str("invalid contract name"),
            Self::PayloadTooLong(length) => {
                write!(formatter, "payload of {length} bytes is too long")
            }
            Self::DuplicateBlock => formatter.write_str("block pushed twice in one message"),
            Self::Block(error) => write!(formatter, "invalid pushed block: {error}"),
            Self::Transaction(error) => write!(formatter, "invalid transaction: {error}"),
            Self::BitVec(error) => write!(formatter, "invalid bit vector: {error}"),
            Self::Signature(error) => write!(formatter, "invalid message signature: {error}"),
        }
    }
}

impl std::error::Error for WireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Block(error) => Some(error),
            Self::Transaction(error) => Some(error),
            Self::BitVec(error) => Some(error),
            Self::Signature(error) => Some(error),
            _ => None,
        }
    }
}

/// A peer's address, always stored as the IPv6 form so that a v4 and a v6 peer
/// occupy the same sixteen bytes on the wire.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PeerAddress([u8; 16]);

impl PeerAddress {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    #[must_use]
    pub const fn from_ip(address: std::net::IpAddr) -> Self {
        Self(match address {
            std::net::IpAddr::V4(v4) => v4.to_ipv6_mapped().octets(),
            std::net::IpAddr::V6(v6) => v6.octets(),
        })
    }

    /// Recover the address, unmapping the IPv4 form so that a socket address
    /// built from it dials v4 rather than a v4-mapped v6 socket.
    #[must_use]
    pub fn to_ip(self) -> std::net::IpAddr {
        let v6 = std::net::Ipv6Addr::from(self.0);
        v6.to_ipv4_mapped()
            .map_or(std::net::IpAddr::V6(v6), std::net::IpAddr::V4)
    }

    #[must_use]
    pub fn to_socket_addr(self, port: u16) -> std::net::SocketAddr {
        std::net::SocketAddr::new(self.to_ip(), port)
    }
}

impl fmt::Display for PeerAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.to_ip(), formatter)
    }
}

/// A peer, as one peer describes another.
///
/// The key hash is a hint only — the peer serving it is asserting something
/// about a third party — so nothing may be trusted until a handshake with that
/// third party produces the key itself.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NeighborAddress {
    pub address: PeerAddress,
    pub port: u16,
    pub public_key_hash: Hash160,
}

/// What a node says about itself when it opens a conversation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Handshake {
    pub address: PeerAddress,
    pub port: u16,
    pub services: u16,
    pub public_key: [u8; 33],
    /// The Bitcoin height after which this node's key is revoked. A peer rejects
    /// a handshake whose key has already expired against *its* tip.
    pub expire_bitcoin_height: u64,
    /// Where this node serves HTTP, or empty if it serves none.
    pub data_url: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandshakeAccept {
    pub handshake: Handshake,
    /// How often the peer expects to hear from us before it forgets us.
    pub heartbeat_interval: u32,
}

/// The `StackerDB` half of a handshake reply, sent only when both sides
/// advertise the `StackerDB` service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackerDbHandshake {
    pub reward_cycle_consensus_hash: ConsensusHash,
    pub contracts: Vec<ContractId>,
}

/// A qualified contract identifier as the p2p codec spells it: an address
/// version byte, the address hash and the contract name. It is deliberately not
/// a Clarity type — nothing here evaluates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContractId {
    pub version: u8,
    pub hash: Hash160,
    pub name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NatPunch {
    pub address: PeerAddress,
    pub port: u16,
    pub nonce: u32,
}

/// A node that forwarded this message, and the sequence number it used.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RelayData {
    pub peer: NeighborAddress,
    pub seq: u32,
}

/// Why a peer refused a request. The numbers are `NackErrorCodes`.
pub mod nack {
    pub const HANDSHAKE_REQUIRED: u32 = 1;
    pub const NO_SUCH_BITCOIN_BLOCK: u32 = 2;
    pub const THROTTLED: u32 = 3;
    pub const INVALID_POX_FORK: u32 = 4;
    pub const INVALID_MESSAGE: u32 = 5;
    pub const NO_SUCH_DB: u32 = 6;
    pub const STALE_VERSION: u32 = 7;
    pub const STALE_VIEW: u32 = 8;
    pub const FUTURE_VERSION: u32 = 9;
    pub const FUTURE_VIEW: u32 = 10;
}

/// The services a node offers, as `ServiceFlags`.
pub mod services {
    pub const RELAY: u16 = 0x01;
    pub const RPC: u16 = 0x02;
    pub const STACKERDB: u16 = 0x04;
}

/// Message identifiers, from `StacksMessageID`.
mod id {
    pub const HANDSHAKE: u8 = 0;
    pub const HANDSHAKE_ACCEPT: u8 = 1;
    pub const HANDSHAKE_REJECT: u8 = 2;
    pub const GET_NEIGHBORS: u8 = 3;
    pub const NEIGHBORS: u8 = 4;
    pub const TRANSACTION: u8 = 13;
    pub const NACK: u8 = 14;
    pub const PING: u8 = 15;
    pub const PONG: u8 = 16;
    pub const NAT_PUNCH_REQUEST: u8 = 17;
    pub const NAT_PUNCH_REPLY: u8 = 18;
    pub const STACKERDB_HANDSHAKE_ACCEPT: u8 = 19;
    pub const GET_NAKAMOTO_INVENTORY: u8 = 26;
    pub const NAKAMOTO_INVENTORY: u8 = 27;
    pub const NAKAMOTO_BLOCKS: u8 = 28;

    /// Identifiers the protocol defines that nano decodes as
    /// [`super::Payload::Unhandled`]: 5..=12 are the epoch-2.x block and
    /// inventory messages, and 21..=25 are the `StackerDB` replication messages
    /// nano carries over HTTP. 20 is absent because the protocol never assigned
    /// it, so it stays an unknown identifier rather than a discardable one.
    pub const UNHANDLED: [u8; 13] = [5, 6, 7, 8, 9, 10, 11, 12, 21, 22, 23, 24, 25];
}

/// Every p2p message nano models.
#[derive(Debug)]
pub enum Payload {
    Handshake(Handshake),
    HandshakeAccept(HandshakeAccept),
    HandshakeReject,
    GetNeighbors,
    Neighbors(Vec<NeighborAddress>),
    /// Boxed because a transaction is by far the largest thing a p2p message
    /// carries, and every other variant would otherwise be padded to its size.
    Transaction(Box<Transaction>),
    Nack(u32),
    Ping(u32),
    Pong(u32),
    NatPunchRequest(u32),
    NatPunchReply(NatPunch),
    StackerDbHandshakeAccept(HandshakeAccept, StackerDbHandshake),
    /// Ask for a reward cycle's tenure availability bit vector, naming the cycle
    /// by the consensus hash of its first sortition.
    GetNakamotoInventory(ConsensusHash),
    /// Bit `i` is set when the peer has processed every block of the tenure that
    /// began at the cycle's `i`th sortition.
    NakamotoInventory(BitVec<2100>),
    NakamotoBlocks(Vec<NakamotoBlock>),
    /// A defined message this node does not model, carrying its identifier so a
    /// caller can count what it is dropping.
    Unhandled(u8),
}

impl Payload {
    #[must_use]
    pub const fn id(&self) -> u8 {
        match self {
            Self::Handshake(_) => id::HANDSHAKE,
            Self::HandshakeAccept(_) => id::HANDSHAKE_ACCEPT,
            Self::HandshakeReject => id::HANDSHAKE_REJECT,
            Self::GetNeighbors => id::GET_NEIGHBORS,
            Self::Neighbors(_) => id::NEIGHBORS,
            Self::Transaction(_) => id::TRANSACTION,
            Self::Nack(_) => id::NACK,
            Self::Ping(_) => id::PING,
            Self::Pong(_) => id::PONG,
            Self::NatPunchRequest(_) => id::NAT_PUNCH_REQUEST,
            Self::NatPunchReply(_) => id::NAT_PUNCH_REPLY,
            Self::StackerDbHandshakeAccept(..) => id::STACKERDB_HANDSHAKE_ACCEPT,
            Self::GetNakamotoInventory(_) => id::GET_NAKAMOTO_INVENTORY,
            Self::NakamotoInventory(_) => id::NAKAMOTO_INVENTORY,
            Self::NakamotoBlocks(_) => id::NAKAMOTO_BLOCKS,
            Self::Unhandled(id) => *id,
        }
    }

    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Handshake(_) => "Handshake",
            Self::HandshakeAccept(_) => "HandshakeAccept",
            Self::HandshakeReject => "HandshakeReject",
            Self::GetNeighbors => "GetNeighbors",
            Self::Neighbors(_) => "Neighbors",
            Self::Transaction(_) => "Transaction",
            Self::Nack(_) => "Nack",
            Self::Ping(_) => "Ping",
            Self::Pong(_) => "Pong",
            Self::NatPunchRequest(_) => "NatPunchRequest",
            Self::NatPunchReply(_) => "NatPunchReply",
            Self::StackerDbHandshakeAccept(..) => "StackerDBHandshakeAccept",
            Self::GetNakamotoInventory(_) => "GetNakamotoInv",
            Self::NakamotoInventory(_) => "NakamotoInv",
            Self::NakamotoBlocks(_) => "NakamotoBlocks",
            Self::Unhandled(_) => "Unhandled",
        }
    }
}

/// A node's Bitcoin view, which every message it sends carries.
///
/// The stable height is the tip less [`STABLE_CONFIRMATIONS`], and a peer treats
/// any other relationship between the two as a protocol violation, so the
/// invariant is established once here rather than at every send.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChainView {
    pub height: u64,
    pub hash: BitcoinHeaderHash,
    pub stable_height: u64,
    pub stable_hash: BitcoinHeaderHash,
}

/// How far back a peer considers the burnchain settled
/// (`Burnchain::stable_confirmations` for Bitcoin).
pub const STABLE_CONFIRMATIONS: u64 = 7;

impl ChainView {
    /// Build the view a node at `height` advertises.
    ///
    /// Returns `None` below [`STABLE_CONFIRMATIONS`], where no stable height
    /// exists; a node that far from genesis has nothing to gossip about.
    #[must_use]
    pub fn new(
        height: u64,
        hash: BitcoinHeaderHash,
        stable_hash: BitcoinHeaderHash,
    ) -> Option<Self> {
        Self::with_stable_confirmations(height, hash, stable_hash, STABLE_CONFIRMATIONS)
    }

    /// Build a view for a burnchain with the given settlement window.
    #[must_use]
    pub fn with_stable_confirmations(
        height: u64,
        hash: BitcoinHeaderHash,
        stable_hash: BitcoinHeaderHash,
        stable_confirmations: u64,
    ) -> Option<Self> {
        Some(Self {
            height,
            hash,
            stable_height: height.checked_sub(stable_confirmations)?,
            stable_hash,
        })
    }
}

/// The signed header every message carries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Preamble {
    pub peer_version: u32,
    pub network_id: u32,
    pub seq: u32,
    pub bitcoin_height: u64,
    pub bitcoin_hash: BitcoinHeaderHash,
    pub stable_bitcoin_height: u64,
    pub stable_bitcoin_hash: BitcoinHeaderHash,
    /// Reserved by the protocol and always zero.
    pub additional_data: u32,
    pub signature: MessageSignature,
    /// The length of the relayer list plus the payload that follow.
    pub payload_len: u32,
}

impl Preamble {
    fn encode_into(&self, writer: &mut Writer) {
        writer.u32(self.peer_version);
        writer.u32(self.network_id);
        writer.u32(self.seq);
        writer.u64(self.bitcoin_height);
        writer.raw(self.bitcoin_hash.as_bytes());
        writer.u64(self.stable_bitcoin_height);
        writer.raw(self.stable_bitcoin_hash.as_bytes());
        writer.u32(self.additional_data);
        writer.raw(self.signature.as_bytes());
        writer.u32(self.payload_len);
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::default();
        self.encode_into(&mut writer);
        writer.finish()
    }

    /// Decode a preamble from exactly [`PREAMBLE_LEN`] bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let mut reader = Reader::new(bytes);
        let preamble = Self {
            peer_version: reader.u32()?,
            network_id: reader.u32()?,
            seq: reader.u32()?,
            bitcoin_height: reader.u64()?,
            bitcoin_hash: BitcoinHeaderHash::from_bytes(reader.array()?),
            stable_bitcoin_height: reader.u64()?,
            stable_bitcoin_hash: BitcoinHeaderHash::from_bytes(reader.array()?),
            additional_data: reader.u32()?,
            signature: MessageSignature::from_bytes(reader.array()?),
            payload_len: reader.u32()?,
        };
        if preamble.payload_len < MIN_PAYLOAD_LEN || preamble.payload_len > MAX_PAYLOAD_LEN {
            return Err(WireError::FrameLength(preamble.payload_len));
        }
        if preamble.bitcoin_height <= preamble.stable_bitcoin_height {
            return Err(WireError::UnstableView);
        }
        Ok(preamble)
    }

    /// The digest a message's signature covers: this preamble with a blank
    /// signature, then the payload frame exactly as it appeared on the wire.
    fn digest(&self, frame: &[u8]) -> [u8; 32] {
        let mut blanked = *self;
        blanked.signature = MessageSignature::from_bytes([0; 65]);
        let mut preimage = blanked.encode();
        preimage.extend_from_slice(frame);
        *sha512_256(&preimage).as_bytes()
    }
}

/// A decoded message and the payload bytes it arrived as.
///
/// Keeping the frame is what makes authentication independent of nano's own
/// encoder, and it is what lets [`Payload::Unhandled`] exist at all.
#[derive(Debug)]
pub struct Message {
    pub preamble: Preamble,
    pub relayers: Vec<RelayData>,
    pub payload: Payload,
    frame: Vec<u8>,
}

impl Message {
    /// Build and sign a message this node originates.
    pub fn sign(
        peer_version: u32,
        network_id: u32,
        view: &ChainView,
        seq: u32,
        payload: Payload,
        private_key: &StacksPrivateKey,
    ) -> Result<Self, WireError> {
        // An originated message has no relayers, so the frame opens with a
        // zero-length vector rather than being able to skip the field.
        Self::relay(
            peer_version,
            network_id,
            view,
            seq,
            Vec::new(),
            payload,
            private_key,
        )
    }

    /// Build and sign a message this node is passing on.
    ///
    /// The relayer list is inside the frame the signature covers, which is why a
    /// relayed message has to be re-encoded and re-signed rather than forwarded
    /// verbatim: appending ourselves changes the bytes the previous sender signed.
    pub fn relay(
        peer_version: u32,
        network_id: u32,
        view: &ChainView,
        seq: u32,
        relayers: Vec<RelayData>,
        payload: Payload,
        private_key: &StacksPrivateKey,
    ) -> Result<Self, WireError> {
        let frame = encode_frame(&relayers, &payload)?;
        let payload_len =
            u32::try_from(frame.len()).map_err(|_| WireError::PayloadTooLong(frame.len()))?;
        if payload_len > MAX_PAYLOAD_LEN {
            return Err(WireError::PayloadTooLong(frame.len()));
        }
        let mut preamble = Preamble {
            peer_version,
            network_id,
            seq,
            bitcoin_height: view.height,
            bitcoin_hash: view.hash,
            stable_bitcoin_height: view.stable_height,
            stable_bitcoin_hash: view.stable_hash,
            additional_data: 0,
            signature: MessageSignature::from_bytes([0; 65]),
            payload_len,
        };
        preamble.signature = private_key.sign(&preamble.digest(&frame));
        Ok(Self {
            preamble,
            relayers,
            payload,
            frame,
        })
    }

    /// Decode a message from a preamble and the frame of `preamble.payload_len`
    /// bytes that follows it.
    pub fn decode(preamble: Preamble, frame: Vec<u8>) -> Result<Self, WireError> {
        let mut reader = Reader::new(&frame);
        let relayers = reader.vector(MAX_RELAYERS, |reader| {
            Ok(RelayData {
                peer: read_neighbor(reader)?,
                seq: reader.u32()?,
            })
        })?;
        let payload = decode_payload(&mut reader)?;
        Ok(Self {
            preamble,
            relayers,
            payload,
            frame,
        })
    }

    /// Check that this message was signed by `public_key`.
    ///
    /// The signature is a recoverable one and is checked by recovery, without
    /// rejecting a high `S` — p2p messages follow the signer rule rather than
    /// the transaction rule, and a node that insisted on low `S` here would
    /// refuse messages the network accepts.
    pub fn verify(&self, public_key: &StacksPublicKey) -> Result<(), WireError> {
        public_key
            .verify_signer(&self.preamble.digest(&self.frame), &self.preamble.signature)
            .map_err(WireError::Signature)
    }

    /// The public key that signed this message, recovered from the signature.
    pub fn signer(&self) -> Result<StacksPublicKey, WireError> {
        self.preamble
            .signature
            .recover(&self.preamble.digest(&self.frame))
            .map_err(WireError::Signature)
    }

    /// The bytes to put on the wire.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = self.preamble.encode();
        bytes.extend_from_slice(&self.frame);
        bytes
    }

    /// The payload frame as it arrived, for a caller that wants to relay a
    /// message on without re-encoding it.
    #[must_use]
    pub fn frame(&self) -> &[u8] {
        &self.frame
    }

    /// Bytes retained from the peer for this complete wire message.
    #[must_use]
    pub const fn wire_len(&self) -> usize {
        PREAMBLE_LEN + self.frame.len()
    }
}

/// Encode the frame a message carries: its relayer list, then its payload.
///
/// This is what a signature covers, so it is also what has to reproduce a peer's
/// bytes exactly. A relay path needs it too — appending ourselves to the relayer
/// list changes the frame, so the message has to be re-encoded and re-signed
/// rather than forwarded verbatim.
pub fn encode_frame(relayers: &[RelayData], payload: &Payload) -> Result<Vec<u8>, WireError> {
    // Encoding an `Unhandled` payload would emit its identifier and drop its
    // body, which is a truncated message rather than the one the caller named. A
    // message this node did not model is one it cannot originate or relay.
    if let Payload::Unhandled(id) = payload {
        return Err(WireError::UnknownMessage(*id));
    }
    let mut writer = Writer::default();
    writer.length(relayers.len());
    for relayer in relayers {
        write_neighbor(relayer.peer, &mut writer);
        writer.u32(relayer.seq);
    }
    encode_payload(payload, &mut writer);
    Ok(writer.finish())
}

fn encode_payload(payload: &Payload, writer: &mut Writer) {
    writer.byte(payload.id());
    match payload {
        Payload::HandshakeReject | Payload::GetNeighbors | Payload::Unhandled(_) => {}
        Payload::Handshake(handshake) => write_handshake(handshake, writer),
        Payload::HandshakeAccept(accept) => write_accept(accept, writer),
        Payload::Neighbors(neighbors) => {
            writer.length(neighbors.len());
            for neighbor in neighbors {
                write_neighbor(*neighbor, writer);
            }
        }
        Payload::Transaction(transaction) => writer.raw(&transaction.encode()),
        Payload::Nack(word)
        | Payload::Ping(word)
        | Payload::Pong(word)
        | Payload::NatPunchRequest(word) => writer.u32(*word),
        Payload::NatPunchReply(punch) => {
            writer.raw(punch.address.as_bytes());
            writer.u16(punch.port);
            writer.u32(punch.nonce);
        }
        Payload::StackerDbHandshakeAccept(accept, stackerdb) => {
            write_accept(accept, writer);
            writer.raw(stackerdb.reward_cycle_consensus_hash.as_bytes());
            writer.byte(u8::try_from(stackerdb.contracts.len()).unwrap_or(u8::MAX));
            for contract in &stackerdb.contracts {
                writer.byte(contract.version);
                writer.raw(contract.hash.as_bytes());
                writer.short_string(&contract.name);
            }
        }
        Payload::GetNakamotoInventory(consensus_hash) => writer.raw(consensus_hash.as_bytes()),
        Payload::NakamotoInventory(tenures) => writer.raw(&tenures.wire_bytes()),
        Payload::NakamotoBlocks(blocks) => {
            writer.length(blocks.len());
            for block in blocks {
                writer.raw(&block.encode());
            }
        }
    }
}

fn decode_payload(reader: &mut Reader<'_>) -> Result<Payload, WireError> {
    let id = reader.byte()?;
    match id {
        id::HANDSHAKE => Ok(Payload::Handshake(read_handshake(reader)?)),
        id::HANDSHAKE_ACCEPT => Ok(Payload::HandshakeAccept(read_accept(reader)?)),
        id::HANDSHAKE_REJECT => Ok(Payload::HandshakeReject),
        id::GET_NEIGHBORS => Ok(Payload::GetNeighbors),
        id::NEIGHBORS => Ok(Payload::Neighbors(
            reader.vector(MAX_NEIGHBORS, read_neighbor)?,
        )),
        id::TRANSACTION => {
            let (transaction, consumed) =
                Transaction::decode(reader.rest()).map_err(WireError::Transaction)?;
            reader.advance(consumed)?;
            Ok(Payload::Transaction(Box::new(transaction)))
        }
        id::NACK => Ok(Payload::Nack(reader.u32()?)),
        id::PING => Ok(Payload::Ping(reader.u32()?)),
        id::PONG => Ok(Payload::Pong(reader.u32()?)),
        id::NAT_PUNCH_REQUEST => Ok(Payload::NatPunchRequest(reader.u32()?)),
        id::NAT_PUNCH_REPLY => Ok(Payload::NatPunchReply(NatPunch {
            address: PeerAddress(reader.array()?),
            port: reader.u16()?,
            nonce: reader.u32()?,
        })),
        id::STACKERDB_HANDSHAKE_ACCEPT => {
            let accept = read_accept(reader)?;
            let reward_cycle_consensus_hash = ConsensusHash::from_bytes(reader.array()?);
            let count = usize::from(reader.byte()?);
            let mut contracts = Vec::with_capacity(count.min(MAX_ANNOUNCED_CONTRACTS));
            for _ in 0..count {
                contracts.push(ContractId {
                    version: reader.byte()?,
                    hash: Hash160::from_bytes(reader.array()?),
                    name: reader.contract_name()?,
                });
            }
            Ok(Payload::StackerDbHandshakeAccept(
                accept,
                StackerDbHandshake {
                    reward_cycle_consensus_hash,
                    contracts,
                },
            ))
        }
        id::GET_NAKAMOTO_INVENTORY => Ok(Payload::GetNakamotoInventory(ConsensusHash::from_bytes(
            reader.array()?,
        ))),
        id::NAKAMOTO_INVENTORY => Ok(Payload::NakamotoInventory(reader.bit_vec()?)),
        id::NAKAMOTO_BLOCKS => Ok(Payload::NakamotoBlocks(read_pushed_blocks(reader)?)),
        _ if id::UNHANDLED.contains(&id) => Ok(Payload::Unhandled(id)),
        _ => Err(WireError::UnknownMessage(id)),
    }
}

/// Decode a pushed block list, rejecting duplicates.
///
/// The duplicate check is not politeness: without it a peer can make us validate
/// the same block thirty-two times for the price of one message.
fn read_pushed_blocks(reader: &mut Reader<'_>) -> Result<Vec<NakamotoBlock>, WireError> {
    let count = reader.count(MAX_PUSHED_BLOCKS)?;
    let mut blocks: Vec<NakamotoBlock> = Vec::with_capacity(count);
    for _ in 0..count {
        let (block, consumed) =
            NakamotoBlock::decode_prefix(reader.rest()).map_err(WireError::Block)?;
        reader.advance(consumed)?;
        if blocks
            .iter()
            .any(|seen| seen.block_id() == block.block_id())
        {
            return Err(WireError::DuplicateBlock);
        }
        blocks.push(block);
    }
    Ok(blocks)
}

fn write_handshake(handshake: &Handshake, writer: &mut Writer) {
    writer.raw(handshake.address.as_bytes());
    writer.u16(handshake.port);
    writer.u16(handshake.services);
    writer.raw(&handshake.public_key);
    writer.u64(handshake.expire_bitcoin_height);
    writer.short_string(&handshake.data_url);
}

fn read_handshake(reader: &mut Reader<'_>) -> Result<Handshake, WireError> {
    Ok(Handshake {
        address: PeerAddress(reader.array()?),
        port: reader.u16()?,
        services: reader.u16()?,
        public_key: reader.array()?,
        expire_bitcoin_height: reader.u64()?,
        data_url: reader.short_string(MAX_URL_LEN)?,
    })
}

fn write_accept(accept: &HandshakeAccept, writer: &mut Writer) {
    write_handshake(&accept.handshake, writer);
    writer.u32(accept.heartbeat_interval);
}

fn read_accept(reader: &mut Reader<'_>) -> Result<HandshakeAccept, WireError> {
    Ok(HandshakeAccept {
        handshake: read_handshake(reader)?,
        heartbeat_interval: reader.u32()?,
    })
}

fn write_neighbor(neighbor: NeighborAddress, writer: &mut Writer) {
    writer.raw(neighbor.address.as_bytes());
    writer.u16(neighbor.port);
    writer.raw(neighbor.public_key_hash.as_bytes());
}

fn read_neighbor(reader: &mut Reader<'_>) -> Result<NeighborAddress, WireError> {
    Ok(NeighborAddress {
        address: PeerAddress(reader.array()?),
        port: reader.u16()?,
        public_key_hash: Hash160::from_bytes(reader.array()?),
    })
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], WireError> {
        let end = self.offset.checked_add(length).ok_or(WireError::Length)?;
        let taken = self
            .bytes
            .get(self.offset..end)
            .ok_or(WireError::EndOfInput)?;
        self.offset = end;
        Ok(taken)
    }

    fn advance(&mut self, length: usize) -> Result<(), WireError> {
        self.take(length).map(|_| ())
    }

    fn rest(&self) -> &'a [u8] {
        self.bytes.get(self.offset..).unwrap_or_default()
    }

    fn byte(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        Ok(self.take(N)?.try_into().expect("fixed slice"))
    }

    fn u16(&mut self) -> Result<u16, WireError> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    /// A four-byte item count, refused above `maximum`.
    ///
    /// The bound is checked before anything is allocated, so a length field
    /// claiming four billion items costs nothing.
    fn count(&mut self, maximum: u32) -> Result<usize, WireError> {
        let count = self.u32()?;
        if count > maximum {
            return Err(WireError::Length);
        }
        usize::try_from(count).map_err(|_| WireError::Length)
    }

    fn vector<T>(
        &mut self,
        maximum: u32,
        mut item: impl FnMut(&mut Self) -> Result<T, WireError>,
    ) -> Result<Vec<T>, WireError> {
        let count = self.count(maximum)?;
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            items.push(item(self)?);
        }
        Ok(items)
    }

    /// A string behind a one-byte length, as `UrlString` and `ContractName` are
    /// both encoded.
    fn short_string(&mut self, maximum: u8) -> Result<String, WireError> {
        let length = self.byte()?;
        if length > maximum {
            return Err(WireError::Length);
        }
        String::from_utf8(self.take(usize::from(length))?.to_vec())
            .map_err(|_| WireError::InvalidUtf8)
    }

    fn contract_name(&mut self) -> Result<String, WireError> {
        let name = self.short_string(u8::try_from(*CONTRACT_NAME_LEN.end()).unwrap_or(u8::MAX))?;
        if !CONTRACT_NAME_LEN.contains(&name.len()) {
            return Err(WireError::InvalidContractName);
        }
        Ok(name)
    }

    fn bit_vec<const MAX: u16>(&mut self) -> Result<BitVec<MAX>, WireError> {
        // The header is a two-byte bit count and a four-byte byte count; the
        // byte count is what says how much to consume, and `from_wire_bytes`
        // then rejects the two disagreeing.
        let header = self.bytes.get(self.offset..).ok_or(WireError::EndOfInput)?;
        let data_len = header
            .get(2..6)
            .ok_or(WireError::EndOfInput)
            .map(|bytes| u32::from_be_bytes(bytes.try_into().expect("fixed slice")))?;
        let total = usize::try_from(data_len)
            .map_err(|_| WireError::Length)?
            .checked_add(6)
            .ok_or(WireError::Length)?;
        BitVec::from_wire_bytes(self.take(total)?).map_err(WireError::BitVec)
    }
}

#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn byte(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn raw(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    fn u16(&mut self, value: u16) {
        self.raw(&value.to_be_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.raw(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.raw(&value.to_be_bytes());
    }

    fn length(&mut self, value: usize) {
        self.u32(u32::try_from(value).expect("item counts are bounded well below u32::MAX"));
    }

    fn short_string(&mut self, value: &str) {
        self.byte(u8::try_from(value.len()).unwrap_or(u8::MAX));
        self.raw(value.as_bytes());
    }
}
