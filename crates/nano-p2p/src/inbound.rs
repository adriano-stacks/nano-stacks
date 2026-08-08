//! Answering a peer that dialled us.
//!
//! Until this existed nano could join the network but not be part of it: a stock
//! `stacks-node` walking its neighbours would reach nano, get nothing, and set it
//! aside. Serving the four control messages is what makes nano a peer other nodes
//! keep rather than a client that happens to speak the protocol.
//!
//! What is served is deliberately small. `Handshake`, `Ping` and `GetNeighbors`
//! are answered from memory and cost nothing; `GetNakamotoInv` is answered only if
//! the node has actually processed the cycle, and `Nack`ed rather than guessed at
//! otherwise. Everything a peer pushes — blocks, transactions — is *offered* to
//! the caller and never acted on here, because an inbound conversation is the
//! least trustworthy thing in the system and this module has no way to check
//! anything.
//!
//! The authentication rule is stacks-core's: only a handshake, a nat punch and the
//! replies to our own requests are answered before a key is known. Anything else
//! gets a `HandshakeRequired` nack, so an unauthenticated peer cannot make this
//! node do work or reveal what it knows.

use std::net::SocketAddr;
use std::time::Duration;

use nano_chainstate::NakamotoBlock;
use nano_codec::Transaction;
use nano_crypto::StacksPublicKey;
use nano_primitives::{BitVec, ConsensusHash, Hash160, hash160};
use tokio::net::{TcpListener, TcpStream};

use crate::session::{Framed, LocalPeer, Protocol, SessionError};
use crate::wire::{
    ChainView, HandshakeAccept, NatPunch, NeighborAddress, Payload, PeerAddress, nack,
};

/// How often this node tells a peer it expects to hear from it, matching what
/// stacks-core advertises. Purely a hint: nothing here enforces it.
pub const HEARTBEAT_INTERVAL_SECS: u32 = 3600;

/// What a peer asking this node gets told.
///
/// Every method answers from state the node already holds, so none of them is
/// async: an inbound request that could block on a database is an inbound request
/// that can stall the listener, and there is nothing here worth that risk.
pub trait Service: Send + Sync {
    /// This node's Bitcoin view, read afresh for every reply so that a long
    /// conversation does not keep advertising the view it opened with.
    fn chain_view(&self) -> ChainView;

    /// The peers to name in a `Neighbors` reply.
    fn neighbors(&self) -> Vec<NeighborAddress>;

    /// Which tenures of the reward cycle beginning at `cycle_start` this node has
    /// fully processed, or `None` if it does not know that cycle.
    ///
    /// `None` becomes a `Nack`, which is the honest answer: a bit vector this node
    /// guessed at would make it look like it was withholding tenures it has, or
    /// offering ones it does not have.
    fn tenure_inventory(&self, cycle_start: ConsensusHash) -> Option<BitVec<2100>> {
        let _ = cycle_start;
        None
    }

    /// Blocks a peer pushed. Authenticated as coming from `from`, and nothing
    /// more: the caller still has to put them through every local check.
    fn offer_blocks(&self, from: Hash160, blocks: Vec<NakamotoBlock>) {
        let _ = (from, blocks);
    }

    /// A transaction a peer relayed, on the same terms.
    fn offer_transaction(&self, from: Hash160, transaction: Box<Transaction>) {
        let _ = (from, transaction);
    }
}

/// The reply a peer's request calls for, answered from what the node already holds.
///
/// Shared by both directions on purpose. Once a handshake has happened this protocol
/// is symmetric: a peer nano dialled sends the same `Ping` and `GetNeighbors` as one
/// that dialled nano, and stacks-core sends them on its heartbeat regardless of who
/// opened the connection. Answering in only one direction is what left the running
/// mainnet node counting a stock peer's requests as "unsolicited" and never replying
/// to them.
///
/// Returns `None` for anything that is not a request — a reply to one of our own, an
/// announcement, or a payload this node does not model — because a message the peer
/// is not waiting on must not be answered. Nacking a `Pong` is a conversation that
/// never ends.
pub(crate) fn answer_request<S: Service + ?Sized>(
    payload: &Payload,
    service: &S,
) -> Option<Payload> {
    match payload {
        Payload::Ping(nonce) => Some(Payload::Pong(*nonce)),
        Payload::GetNeighbors => Some(Payload::Neighbors(service.neighbors())),
        // `None` from the service becomes a `Nack`, which is the honest answer: a bit
        // vector this node guessed at would make it look like it was withholding
        // tenures it has, or offering ones it does not. A stock node reads the nack as
        // "ask somebody else" rather than "it has nothing".
        Payload::GetNakamotoInventory(cycle_start) => {
            Some(service.tenure_inventory(*cycle_start).map_or(
                Payload::Nack(nack::NO_SUCH_BITCOIN_BLOCK),
                Payload::NakamotoInventory,
            ))
        }
        _ => None,
    }
}

/// Bounds on one inbound conversation.
///
/// Both matter for the same reason: an inbound peer costs this node a task and a
/// socket, and neither may be something the peer decides how long to hold.
#[derive(Clone, Copy, Debug)]
pub struct InboundLimits {
    /// How long to wait on any single read or write.
    pub timeout: Duration,
    /// How long a conversation may be silent before it is closed.
    ///
    /// Distinct from `timeout`, and the distinction was a real bug: closing at the
    /// read deadline meant nano hung up on any stock node that had nothing to say for
    /// thirty seconds, which is *most of the time* — stacks-core advertises a 3600
    /// second heartbeat and pings on it. A node that drops its inbound peers twice a
    /// minute is not a peer anyone keeps.
    ///
    /// Bounded all the same, because a silently dead socket would otherwise hold one
    /// of `MAX_INBOUND_PEERS` slots forever.
    pub idle: Duration,
    /// How many messages one conversation may carry before it is closed. A peer
    /// that wants to keep talking reconnects, which costs it a handshake.
    pub max_messages: u64,
}

impl Default for InboundLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            idle: Duration::from_mins(15),
            max_messages: 4096,
        }
    }
}

/// What one inbound conversation amounted to.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Served {
    /// The peer's key hash, once its handshake proved one.
    pub peer: Option<Hash160>,
    pub answered: u64,
    pub nacked: u64,
    /// Messages read and deliberately not answered: replies to nothing, and
    /// payloads this node does not model.
    pub ignored: u64,
    pub blocks_offered: u64,
    pub transactions_offered: u64,
}

/// A socket accepting inbound peers.
pub struct Listener {
    listener: TcpListener,
}

impl Listener {
    pub async fn bind(address: SocketAddr) -> Result<Self, std::io::Error> {
        Ok(Self {
            listener: TcpListener::bind(address).await?,
        })
    }

    /// The address actually bound, which is what to advertise. Asking the socket
    /// rather than trusting the configuration is what makes port 0 usable, and
    /// port 0 is what a test binds.
    pub fn local_addr(&self) -> Result<SocketAddr, std::io::Error> {
        self.listener.local_addr()
    }

    pub async fn accept(&self) -> Result<(TcpStream, SocketAddr), std::io::Error> {
        let (stream, address) = self.listener.accept().await?;
        // Same reasoning as the outbound side: every reply here is written in full
        // and then waited on, so there is no second write for Nagle to wait for.
        stream.set_nodelay(true)?;
        Ok((stream, address))
    }
}

/// Serve one inbound peer until it closes, misbehaves or runs out of budget.
pub async fn serve_peer<S: Service + ?Sized>(
    stream: TcpStream,
    from: SocketAddr,
    local: &LocalPeer,
    protocol: Protocol,
    service: &S,
    limits: InboundLimits,
) -> Result<Served, SessionError> {
    let mut framed = Framed::new(
        stream,
        local,
        protocol,
        service.chain_view(),
        limits.timeout,
    );
    let mut served = Served::default();
    let ours = local.private_key.public_key();
    for _ in 0..limits.max_messages {
        let message = match framed.read_idle(limits.idle).await {
            Ok(message) => message,
            // A peer that closes cleanly is not a peer that did anything wrong,
            // and treating a hang-up as a fault is how an honest neighbour ends up
            // in the backoff table.
            Err(SessionError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(served);
            }
            Err(error) => return Err(error),
        };
        // Re-read the view for every reply: a conversation can outlive several
        // Bitcoin blocks, and a peer checks the view on each message rather than
        // once, so a stale one would eventually be refused.
        framed.view = service.chain_view();
        let seq = message.preamble.seq;
        let ours_already = crate::relay::relayed_by(&message, local.public_key_hash());
        let reply = match message.payload {
            Payload::Handshake(ref handshake) => {
                // The key a handshake announces has to be the key that signed it.
                // Without this check a peer could name somebody else's key and
                // have every later message judged against a key that never signed
                // anything.
                let Ok(key) = StacksPublicKey::from_bytes(&handshake.public_key) else {
                    return Err(SessionError::Wire(crate::wire::WireError::Signature(
                        nano_crypto::CryptoError::InvalidPublicKey,
                    )));
                };
                if message.verify(&key).is_err() {
                    return Err(SessionError::Unauthenticated);
                }
                // A key already revoked at this node's own tip, or this node's own
                // key coming back at it through a loop, is refused rather than
                // dropped: the peer is not lying, it is confused, and a rejection
                // says so.
                if handshake.expire_bitcoin_height <= framed.view.height || key == ours {
                    Some(Payload::HandshakeReject)
                } else {
                    served.peer = Some(hash160(&handshake.public_key));
                    framed.peer_key = Some(key);
                    Some(Payload::HandshakeAccept(HandshakeAccept {
                        handshake: local.announce(),
                        heartbeat_interval: HEARTBEAT_INTERVAL_SECS,
                    }))
                }
            }
            // A nat punch needs no authentication, because all it does is tell the
            // peer the address this node sees it at — which the peer could learn
            // by other means, and which is the whole point of asking.
            Payload::NatPunchRequest(nonce) => Some(Payload::NatPunchReply(NatPunch {
                address: PeerAddress::from_ip(from.ip()),
                port: from.port(),
                nonce,
            })),
            // Anything that is a reply rather than a request, or that this node
            // does not model, is read and dropped. Nacking a `Pong` would be a
            // conversation that never ends.
            Payload::HandshakeAccept(_)
            | Payload::StackerDbHandshakeAccept(..)
            | Payload::HandshakeReject
            | Payload::Neighbors(_)
            | Payload::Pong(_)
            | Payload::Nack(_)
            | Payload::NatPunchReply(_)
            | Payload::NakamotoInventory(_)
            | Payload::Unhandled(_) => {
                served.ignored = served.ignored.saturating_add(1);
                None
            }
            // Everything past here needs a handshake first.
            _ if served.peer.is_none() => Some(Payload::Nack(nack::HANDSHAKE_REQUIRED)),
            Payload::Ping(_) | Payload::GetNeighbors | Payload::GetNakamotoInventory(_) => {
                answer_request(&message.payload, service)
            }
            // Pushed data is handed on and never answered. The peer is not waiting
            // for a reply, and this node has no opinion until its own checks have
            // run — which happen somewhere with a chainstate, not here.
            //
            // Except for what this node itself relayed: nano names only itself in a
            // relayer list, so its own hash appearing there means the item has come
            // back round, and re-checking it would be work a loop chose for us.
            Payload::NakamotoBlocks(blocks) => {
                served.blocks_offered = served
                    .blocks_offered
                    .saturating_add(blocks.len().try_into().unwrap_or(u64::MAX));
                if let Some(peer) = served.peer.filter(|_| !ours_already) {
                    service.offer_blocks(peer, blocks);
                }
                None
            }
            Payload::Transaction(transaction) => {
                served.transactions_offered = served.transactions_offered.saturating_add(1);
                if let Some(peer) = served.peer.filter(|_| !ours_already) {
                    service.offer_transaction(peer, transaction);
                }
                None
            }
        };
        if let Some(reply) = reply {
            if matches!(reply, Payload::Nack(_)) {
                served.nacked = served.nacked.saturating_add(1);
            } else {
                served.answered = served.answered.saturating_add(1);
            }
            // The reply carries the request's sequence number, which is how the
            // peer pairs it: answering with a fresh one would look unsolicited and
            // be queued rather than returned.
            framed.send(seq, reply).await?;
        }
    }
    Ok(served)
}
