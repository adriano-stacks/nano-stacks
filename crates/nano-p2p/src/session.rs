//! One authenticated conversation with one peer.
//!
//! A session is deliberately a single TCP connection with a synchronous
//! request/reply discipline rather than a share of a poll loop. stacks-core's
//! `ConversationP2P` multiplexes every request over one socket with a reply
//! handle per sequence number, which it needs because it drives every peer from
//! one thread; nano runs each peer as its own task, so the sequence number only
//! has to distinguish a reply from an unsolicited message arriving in the middle
//! of one, and that fits in twenty lines instead of `connection.rs`'s eight
//! hundred.
//!
//! Nothing here is a consensus input. A session yields authenticated *claims* —
//! this peer says its tip is here, has these tenures, holds these blocks — and
//! it is the caller's fork choice that decides what any of it means.

use std::net::SocketAddr;
use std::time::Duration;

use nano_crypto::{StacksPrivateKey, StacksPublicKey};
use nano_primitives::{BitVec, ConsensusHash, Hash160, Network, hash160};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::wire::{
    ChainView, Handshake, Message, PREAMBLE_LEN, Payload, PeerAddress, Preamble, WireError, nack,
};

/// The major protocol version byte, which two peers must share exactly.
pub const PEER_VERSION_MAINNET_MAJOR: u32 = 0x1800_0000;
pub const PEER_VERSION_TESTNET_MAJOR: u32 = 0xfaca_de00;

/// The epoch a node advertises in the low byte of its peer version. A peer
/// rejects anyone whose epoch byte is *below* its own, so a 4.0-only node
/// advertises 4.0 and nothing else.
pub const PEER_VERSION_EPOCH_4_0: u32 = 0x10;

/// The peer version and network identifier a node presents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Protocol {
    pub peer_version: u32,
    /// The identifier a peer checks first, which is the **chain id**.
    ///
    /// `stacks-common` also exports a `NETWORK_ID_MAINNET = 0x17000000`, and it is
    /// a trap: nothing in the p2p path uses it. `stacks-node` passes
    /// `config.burnchain.chain_id` as `PeerDB::connect`'s `network_id`
    /// (`neon_node.rs:4713`), which is 1 on mainnet, and it is that value the
    /// preamble carries and `is_preamble_valid` compares. Sending `0x17000000`
    /// gets every message dropped without a reply, which is how this was found —
    /// a differential codec test cannot see it, because the constant is policy
    /// rather than encoding.
    pub network_id: u32,
}

impl Protocol {
    /// What a 4.0 node on `network` presents.
    #[must_use]
    pub const fn for_network(network: Network) -> Self {
        let major = if network.is_mainnet() {
            PEER_VERSION_MAINNET_MAJOR
        } else {
            PEER_VERSION_TESTNET_MAJOR
        };
        Self {
            peer_version: major | PEER_VERSION_EPOCH_4_0,
            network_id: network.chain_id(),
        }
    }

    #[must_use]
    pub const fn mainnet() -> Self {
        Self::for_network(Network::MAINNET)
    }

    #[must_use]
    pub const fn testnet() -> Self {
        Self::for_network(Network::TESTNET)
    }

    /// Whether a peer's preamble belongs to this network at all.
    ///
    /// Only the *major* version byte has to match: the low byte is the peer's
    /// epoch, and rejecting on it would refuse every node that supports a newer
    /// epoch than we do.
    #[must_use]
    pub const fn accepts(&self, preamble: &Preamble) -> bool {
        preamble.network_id == self.network_id
            && preamble.peer_version & 0xff00_0000 == self.peer_version & 0xff00_0000
    }

    /// Whether a peer supports at least the epoch we do.
    ///
    /// This mirrors stacks-core's `has_acceptable_epoch`, and it is policy rather
    /// than codec: a 3.x node's messages parse fine, they just cannot tell us
    /// anything about a 4.0 chain.
    #[must_use]
    pub const fn epoch_is_current(&self, preamble: &Preamble) -> bool {
        preamble.peer_version & 0xff >= self.peer_version & 0xff
    }
}

/// What this node says about itself.
#[derive(Clone)]
pub struct LocalPeer {
    pub private_key: StacksPrivateKey,
    /// The address peers should dial back on, which may be the any-net address
    /// for a node that does not know its own; a peer accepts that from an inbound
    /// connection, which is what ours is from its side.
    pub address: PeerAddress,
    pub port: u16,
    pub services: u16,
    /// The Bitcoin height past which this node's key is revoked. A peer rejects a
    /// handshake whose key has already expired against its own tip, so this has
    /// to lead the network's tip, not ours.
    pub key_expire_height: u64,
    /// This node's HTTP endpoint, or empty when it serves none. A peer validates
    /// it as an HTTP(S) URL with no query, fragment or credentials, so an
    /// approximation here costs a rejected handshake.
    pub data_url: String,
}

/// Printed without the private key, so that logging a configuration cannot leak
/// the identity a node signs its messages with.
impl std::fmt::Debug for LocalPeer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalPeer")
            .field("public_key", &hex(&self.private_key.public_key().to_bytes_compressed()))
            .field("address", &self.address)
            .field("port", &self.port)
            .field("services", &self.services)
            .field("key_expire_height", &self.key_expire_height)
            .field("data_url", &self.data_url)
            .finish()
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

impl LocalPeer {
    /// A peer that offers no services and advertises no HTTP endpoint.
    ///
    /// This is what a node syncing from the network looks like before it serves
    /// anything: it can handshake, walk neighbors and fetch, and no peer will ask
    /// it for data it does not have.
    #[must_use]
    pub const fn quiet(private_key: StacksPrivateKey, port: u16) -> Self {
        Self {
            private_key,
            address: PeerAddress::from_bytes([0; 16]),
            port,
            services: 0,
            // Far enough ahead that no reachable tip has passed it. A revocation
            // height is a promise about a key, and nano re-keys by restarting.
            key_expire_height: u64::MAX,
            data_url: String::new(),
        }
    }

    pub(crate) fn announce(&self) -> Handshake {
        Handshake {
            address: self.address,
            port: self.port,
            services: self.services,
            public_key: self.private_key.public_key().to_bytes_compressed(),
            expire_bitcoin_height: self.key_expire_height,
            data_url: self.data_url.clone(),
        }
    }
}

/// Errors a session raises. All of them are per-peer: none is a reason to stop
/// syncing, only a reason to stop talking to this peer.
#[derive(Debug)]
pub enum SessionError {
    Io(std::io::Error),
    Wire(WireError),
    /// The peer did not answer within the deadline.
    Timeout,
    /// The peer is on another network, or speaks another major version.
    WrongNetwork { peer_version: u32, network_id: u32 },
    /// The peer supports an older epoch than nano does, so it cannot serve a 4.0
    /// chain even if it is honest.
    StaleEpoch(u32),
    /// The peer's messages are not signed by the key it handshook with.
    Unauthenticated,
    /// The peer refused the handshake outright.
    HandshakeRejected,
    /// The peer answered a request with a `Nack`.
    Nack(u32),
    /// The peer answered with something other than the reply to the request.
    UnexpectedReply(&'static str),
    /// The peer sent a self-inconsistent Bitcoin view: the stable height must be
    /// exactly [`crate::wire::STABLE_CONFIRMATIONS`] below the tip.
    InconsistentView,
    /// The peer sent more unsolicited messages than a reply is worth waiting
    /// through.
    TooChatty,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "peer connection failed: {error}"),
            Self::Wire(error) => write!(formatter, "peer sent an invalid message: {error}"),
            Self::Timeout => formatter.write_str("peer did not answer in time"),
            Self::WrongNetwork {
                peer_version,
                network_id,
            } => write!(
                formatter,
                "peer is on network {network_id:#010x} version {peer_version:#010x}"
            ),
            Self::StaleEpoch(peer_version) => write!(
                formatter,
                "peer version {peer_version:#010x} predates epoch 4.0"
            ),
            Self::Unauthenticated => {
                formatter.write_str("peer message is not signed by its handshake key")
            }
            Self::HandshakeRejected => formatter.write_str("peer rejected the handshake"),
            Self::Nack(code) => write!(formatter, "peer refused the request with code {code}"),
            Self::UnexpectedReply(name) => write!(formatter, "peer answered with a {name}"),
            Self::InconsistentView => {
                formatter.write_str("peer's stable Bitcoin height does not follow its tip")
            }
            Self::TooChatty => formatter.write_str("peer flooded rather than answering"),
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Wire(error) => Some(error),
            _ => None,
        }
    }
}

impl SessionError {
    /// Whether the peer broke the protocol, as opposed to merely going away.
    ///
    /// The distinction is the whole of the swarm's scoring policy. Not answering is
    /// nearly always a restart, and a node that punished it would drop honest
    /// neighbours every time the network deployed. Sending a malformed message,
    /// signing with a key other than the one announced, contradicting itself about
    /// its own Bitcoin view, or flooding instead of answering are all things a
    /// working node does not do — so they are worth remembering across the session,
    /// and across a restart.
    ///
    /// A `Nack` is neither: it is an answer, and refusing a request is something an
    /// honest peer does constantly.
    #[must_use]
    pub const fn is_protocol_fault(&self) -> bool {
        match self {
            Self::Wire(_)
            | Self::Unauthenticated
            | Self::InconsistentView
            | Self::WrongNetwork { .. }
            | Self::StaleEpoch(_)
            | Self::TooChatty
            | Self::UnexpectedReply(_) => true,
            Self::Io(_) | Self::Timeout | Self::HandshakeRejected | Self::Nack(_) => false,
        }
    }
}

impl From<std::io::Error> for SessionError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<WireError> for SessionError {
    fn from(error: WireError) -> Self {
        Self::Wire(error)
    }
}

/// How many unsolicited messages a peer may interleave before a reply.
///
/// A peer legitimately pushes blocks and relays transactions while we are
/// waiting, so this cannot be zero; a peer that never gets to the reply is
/// either broken or stalling us, and either way it is not worth reading forever.
const MAX_UNSOLICITED_PER_REQUEST: usize = 32;

/// A framed connection, which knows how to speak the protocol but not yet to
/// whom.
///
/// This exists separately from [`Session`] because the handshake is the one
/// exchange that happens before the peer's key is known, and a `Session` that
/// could not name its peer would put an `Option` on every accessor for the sake
/// of one message.
pub(crate) struct Framed {
    stream: TcpStream,
    protocol: Protocol,
    private_key: StacksPrivateKey,
    pub(crate) view: ChainView,
    timeout: Duration,
    seq: u32,
    /// The peer's key, once its handshake has announced it. Until then messages
    /// are read but not authenticated, which is why the handshake reply is
    /// re-verified against the key it carries.
    pub(crate) peer_key: Option<StacksPublicKey>,
    remote_view: Option<ChainView>,
    pushed: Vec<Message>,
    unhandled: u64,
}

impl Framed {
    pub(crate) fn new(
        stream: TcpStream,
        local: &LocalPeer,
        protocol: Protocol,
        view: ChainView,
        timeout: Duration,
    ) -> Self {
        Self {
            stream,
            protocol,
            private_key: local.private_key.clone(),
            view,
            timeout,
            seq: initial_seq(),
            peer_key: None,
            remote_view: None,
            pushed: Vec::new(),
            unhandled: 0,
        }
    }

    /// Sign one message and put it on the wire, without waiting for anything.
    ///
    /// The sequence number is the caller's, because a reply has to carry the
    /// sequence number of the request it answers rather than one of its own.
    pub(crate) async fn send(&mut self, seq: u32, payload: Payload) -> Result<(), SessionError> {
        let message = Message::sign(
            self.protocol.peer_version,
            self.protocol.network_id,
            &self.view,
            seq,
            payload,
            &self.private_key,
        )?;
        deadline(self.timeout, self.stream.write_all(&message.encode())).await??;
        Ok(())
    }

    /// Send a message and read until the reply to it arrives.
    async fn request(&mut self, payload: Payload) -> Result<Message, SessionError> {
        let seq = self.next_seq();
        self.send(seq, payload).await?;
        for _ in 0..=MAX_UNSOLICITED_PER_REQUEST {
            let message = self.read().await?;
            if message.preamble.seq == seq {
                if let Payload::Nack(code) = message.payload {
                    return Err(SessionError::Nack(code));
                }
                return Ok(message);
            }
            self.pushed.push(message);
        }
        Err(SessionError::TooChatty)
    }

    /// Read one message: a fixed preamble, then the frame it announces.
    pub(crate) async fn read(&mut self) -> Result<Message, SessionError> {
        let mut header = [0; PREAMBLE_LEN];
        deadline(self.timeout, self.stream.read_exact(&mut header)).await??;
        let preamble = Preamble::decode(&header)?;
        if !self.protocol.accepts(&preamble) {
            return Err(SessionError::WrongNetwork {
                peer_version: preamble.peer_version,
                network_id: preamble.network_id,
            });
        }
        if !self.protocol.epoch_is_current(&preamble) {
            return Err(SessionError::StaleEpoch(preamble.peer_version));
        }
        // `Preamble::decode` has already bounded `payload_len`, so this
        // allocation is bounded by the protocol rather than by the peer.
        let mut frame = vec![0; preamble.payload_len as usize];
        deadline(self.timeout, self.stream.read_exact(&mut frame)).await??;
        let message = Message::decode(preamble, frame)?;
        self.observe(&message)?;
        Ok(message)
    }

    /// Apply the per-message checks that are about the peer rather than the bytes.
    fn observe(&mut self, message: &Message) -> Result<(), SessionError> {
        let preamble = &message.preamble;
        // stacks-core treats a stable height that is not exactly the tip less the
        // confirmation window as a protocol violation, because it means the
        // sender is not deriving one from the other.
        let view = ChainView::new(
            preamble.bitcoin_height,
            preamble.bitcoin_hash,
            preamble.stable_bitcoin_hash,
        )
        .filter(|view| view.stable_height == preamble.stable_bitcoin_height)
        .ok_or(SessionError::InconsistentView)?;
        if let Some(key) = &self.peer_key {
            message
                .verify(key)
                .map_err(|_| SessionError::Unauthenticated)?;
        }
        // A peer that reports a lower tip than it did before is reorganizing or
        // restarting; keeping the higher claim would be believing a stale one, so
        // the latest claim wins outright.
        self.remote_view = Some(view);
        if matches!(message.payload, Payload::Unhandled(_)) {
            self.unhandled = self.unhandled.saturating_add(1);
        }
        Ok(())
    }

    pub(crate) const fn next_seq(&mut self) -> u32 {
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }
}

/// An authenticated conversation with one peer.
pub struct Session {
    framed: Framed,
    remote: Handshake,
    heartbeat_interval: u32,
}

impl Session {
    /// Dial `address` and complete a handshake.
    ///
    /// `view` is *this node's* Bitcoin view. It is not cosmetic: a peer refuses a
    /// message whose stable header hash contradicts its own at that height, so a
    /// fabricated view only passes while it is old enough that no peer still
    /// remembers it (stacks-core keeps roughly 288 blocks back from its stable
    /// height). The honest source is the caller's own sortition database.
    pub async fn open(
        address: SocketAddr,
        local: &LocalPeer,
        protocol: Protocol,
        view: ChainView,
        timeout: Duration,
    ) -> Result<Self, SessionError> {
        let stream = deadline(timeout, TcpStream::connect(address)).await??;
        // Nagle's algorithm would hold a request back waiting for a second one
        // that is not coming: every message here is written in full and then
        // waited on.
        stream.set_nodelay(true)?;
        Self::negotiate(stream, local, protocol, view, timeout).await
    }

    /// Complete a handshake over an already-connected stream.
    pub async fn negotiate(
        stream: TcpStream,
        local: &LocalPeer,
        protocol: Protocol,
        view: ChainView,
        timeout: Duration,
    ) -> Result<Self, SessionError> {
        let mut framed = Framed::new(stream, local, protocol, view, timeout);
        let reply = framed.request(Payload::Handshake(local.announce())).await?;
        // Cloned rather than moved out, because the reply itself is still needed
        // below to check that the key it announces is the key that signed it.
        let accept = match &reply.payload {
            // A peer that shares our StackerDB service answers with the combined
            // form. The contract list it carries is about *its* StackerDB
            // replication, which nano does over HTTP, so the handshake half is
            // all that is taken from it.
            Payload::HandshakeAccept(accept) | Payload::StackerDbHandshakeAccept(accept, _) => {
                accept.clone()
            }
            Payload::HandshakeReject => return Err(SessionError::HandshakeRejected),
            other => return Err(SessionError::UnexpectedReply(other.name())),
        };
        // The reply was read before the peer's key was known, so it is verified
        // here against the key it itself announced. Without this a peer could
        // name one key and sign with another, and every later message would be
        // checked against a key that never signed anything.
        let peer_key = StacksPublicKey::from_bytes(&accept.handshake.public_key)
            .map_err(|error| SessionError::Wire(WireError::Signature(error)))?;
        reply
            .verify(&peer_key)
            .map_err(|_| SessionError::Unauthenticated)?;
        framed.peer_key = Some(peer_key);
        Ok(Self {
            framed,
            remote: accept.handshake,
            heartbeat_interval: accept.heartbeat_interval,
        })
    }

    /// The peer's node public key, learned from its handshake.
    #[must_use]
    pub const fn public_key(&self) -> &StacksPublicKey {
        self.framed
            .peer_key
            .as_ref()
            .expect("a session exists only after a handshake announced the peer's key")
    }

    /// The `Hash160` of the peer's node key, which is how neighbor gossip and
    /// `StackerDB` slot ownership name it.
    #[must_use]
    pub fn public_key_hash(&self) -> Hash160 {
        hash160(&self.public_key().to_bytes_compressed())
    }

    #[must_use]
    pub const fn handshake(&self) -> &Handshake {
        &self.remote
    }

    /// The peer's last-advertised Bitcoin view, which is a claim and not a fact.
    #[must_use]
    pub const fn remote_view(&self) -> ChainView {
        self.framed
            .remote_view
            .expect("the handshake reply carried the peer's view")
    }

    #[must_use]
    pub const fn heartbeat_interval(&self) -> u32 {
        self.heartbeat_interval
    }

    /// How many defined-but-unmodelled messages this peer has sent. A peer that
    /// only ever sends epoch-2.x messages is not a useful 4.0 peer.
    #[must_use]
    pub const fn unhandled_messages(&self) -> u64 {
        self.framed.unhandled
    }

    /// Replace the Bitcoin view advertised on outgoing messages.
    pub const fn advertise(&mut self, view: ChainView) {
        self.framed.view = view;
    }

    /// Take the messages that arrived unsolicited, so a caller can feed pushed
    /// blocks and relayed transactions through its own validation.
    #[must_use]
    pub fn take_pushed(&mut self) -> Vec<Message> {
        std::mem::take(&mut self.framed.pushed)
    }

    /// Prove the peer is still there, and that it is still the same peer.
    pub async fn ping(&mut self) -> Result<(), SessionError> {
        let nonce = self.framed.next_seq();
        match self.framed.request(Payload::Ping(nonce)).await?.payload {
            // The sequence number already paired this reply to its request; the
            // nonce is a second, independent pairing, and a peer that echoes the
            // wrong one is not tracking the conversation.
            Payload::Pong(echo) if echo == nonce => Ok(()),
            Payload::Pong(_) => Err(SessionError::UnexpectedReply("Pong with another nonce")),
            other => Err(SessionError::UnexpectedReply(other.name())),
        }
    }

    /// Ask the peer who else it knows.
    ///
    /// The answer is a set of hints: the key hashes are the peer's claims about
    /// third parties, and become facts only when a handshake with that third
    /// party produces the key itself.
    pub async fn neighbors(&mut self) -> Result<Vec<crate::wire::NeighborAddress>, SessionError> {
        match self.framed.request(Payload::GetNeighbors).await?.payload {
            Payload::Neighbors(neighbors) => Ok(neighbors),
            other => Err(SessionError::UnexpectedReply(other.name())),
        }
    }

    /// Ask which tenures of a reward cycle the peer has fully processed.
    ///
    /// The cycle is named by the consensus hash of its first sortition, which is
    /// also what makes the answer checkable: a peer that does not share our view
    /// of that sortition cannot answer at all, and says so with a `Nack`.
    pub async fn nakamoto_inventory(
        &mut self,
        cycle_start: ConsensusHash,
    ) -> Result<BitVec<2100>, SessionError> {
        match self
            .framed
            .request(Payload::GetNakamotoInventory(cycle_start))
            .await?
            .payload
        {
            Payload::NakamotoInventory(tenures) => Ok(tenures),
            other => Err(SessionError::UnexpectedReply(other.name())),
        }
    }
}

/// The first sequence number of a session.
///
/// stacks-core draws these at random; here they only have to distinguish a reply
/// from an unsolicited message *within one authenticated stream*, because every
/// message is checked against the peer's key before its sequence number is
/// looked at. Starting from the clock rather than a constant just keeps two
/// sessions with the same peer from reusing numbers.
fn initial_seq() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos())
}

pub(crate) async fn deadline<F: Future>(timeout: Duration, future: F) -> Result<F::Output, SessionError> {
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| SessionError::Timeout)
}

/// Whether a `Nack` says "not yet" rather than "no".
///
/// A throttled or stale-view peer is one to come back to; a peer that says the
/// request itself was invalid is one whose answers to that request are not worth
/// asking for again.
#[must_use]
pub const fn nack_is_transient(code: u32) -> bool {
    matches!(
        code,
        nack::THROTTLED
            | nack::STALE_VIEW
            | nack::FUTURE_VIEW
            | nack::STALE_VERSION
            | nack::FUTURE_VERSION
    )
}
