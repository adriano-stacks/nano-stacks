//! A bounded set of outbound peers, kept alive and kept honest.
//!
//! One session is a conversation; a swarm is the reason to have several. It holds
//! at most [`SwarmLimits::outbound`] of them, replaces the ones that die from the
//! peer table, walks neighbours so the table outgrows its seed list, and sets a
//! peer aside when it breaks the protocol rather than when it merely stops
//! answering.
//!
//! ## What discovery is actually for
//!
//! Not block download. In Nakamoto, stacks-core fetches blocks and tenures over
//! **HTTP**, to each peer's own RPC endpoint — `tenure_downloader.rs` builds
//! `StacksHttpRequest::new_get_nakamoto_tenure` against a `PeerHost`, and there is
//! no p2p message for requesting a block at all. p2p carries the handshake,
//! neighbours, inventories, and pushed blocks and transactions.
//!
//! So what this yields is [`Discovered::endpoints`]: the `data_url` of every peer
//! that handshook and advertises the RPC service. That is what replaces a hosted
//! API. `https://api.mainnet.hiro.so` and `http://34.150.184.50:20443` are the
//! same protocol; the difference that matters is that the second one was found by
//! asking the network, is one of dozens, and is not a service whose rate limit is
//! nano's liveness. The endpoints go to `nano-sync`'s `PeerPool`, whose
//! `choose_canonical_tip` is the locally-authenticated boundary — nothing here
//! decides which chain is canonical.
//!
//! ## Scoring
//!
//! Two kinds of failure, and the difference is the whole of the policy. A peer that
//! times out or drops the connection has probably restarted, so its session is
//! dropped and the peer table gives it a growing backoff. A peer that sends a
//! malformed message, signs with the wrong key, contradicts itself about its own
//! Bitcoin view or floods instead of answering is *isolated*: the table gives it
//! the longest penalty it can express. Neither is fatal to anything, which is why
//! `SessionError` has no variant that stops the node.
//!
//! ## Why everything takes `&mut self`
//!
//! `PeerDb` holds a `rusqlite::Connection`, which is `Send` but not `Sync`. A
//! `&Swarm` held across an await would therefore make the future non-`Send` and
//! unspawnable, while a `&mut Swarm` is `Send` because `Swarm` is. So the peer
//! table is reached through a unique borrow throughout, which also happens to be
//! the honest signature: every one of these calls changes what this node knows.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nano_primitives::{BitVec, ConsensusHash, Hash160};

use crate::peers::{PeerDb, PeerDbError};
use crate::session::{LocalPeer, Protocol, Session, SessionError};
use crate::wire::{ChainView, PeerAddress};

/// How many peers to hold sessions with, and how patiently.
#[derive(Clone, Copy, Debug)]
pub struct SwarmLimits {
    /// How many outbound sessions to hold at once.
    ///
    /// stacks-core's default is 16. Eight is enough that no single peer is load
    /// bearing and few enough that a round of pings is quick; the number that
    /// matters is that it is greater than one.
    pub outbound: usize,
    /// How many new peers to try in one round. Bounded so that a round ends: a
    /// table with four thousand addresses and no reachable peer would otherwise
    /// spend minutes in one call.
    pub dials_per_round: usize,
    /// Per-request deadline for every session.
    pub timeout: Duration,
}

impl Default for SwarmLimits {
    fn default() -> Self {
        Self {
            outbound: 8,
            dials_per_round: 4,
            timeout: Duration::from_secs(15),
        }
    }
}

/// What one maintenance round did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Round {
    pub connected: usize,
    pub dialled: usize,
    /// Sessions closed because the peer stopped answering.
    pub dropped: usize,
    /// Sessions closed because the peer broke the protocol.
    pub isolated: usize,
    /// Addresses learned from a neighbour walk.
    pub learned: usize,
    /// How many peers answered an inventory request claiming at least one tenure of
    /// the cycle asked about.
    pub claiming: usize,
    /// Messages peers sent unprompted and this round collected: pushed blocks,
    /// relayed transactions, signer chunks, and the requests that were answered.
    pub collected: usize,
}

/// One live outbound peer.
struct Connected {
    session: Session,
    address: PeerAddress,
    port: u16,
}

impl Connected {
    fn endpoint(&self) -> Option<String> {
        let handshake = self.session.handshake();
        // An empty `data_url` is what a node with no routable HTTP endpoint sends,
        // and the RPC service bit is what says it will answer if dialled. Trying one
        // without either is a guaranteed wasted connection.
        if handshake.data_url.is_empty()
            || handshake.services & crate::wire::services::RPC == 0
            || !endpoint_is_reachable(&handshake.data_url, self.address)
        {
            return None;
        }
        Some(handshake.data_url.clone())
    }
}

/// Whether an endpoint a peer advertised is one *this* node could plausibly reach.
///
/// Mainnet returns `http://10.0.1.37:20443` — a load-balanced node advertising the
/// address it sees itself at behind its own NAT. Dialling that from here reaches
/// whatever happens to be at 10.0.1.37 on this machine's network, which is a wasted
/// connection at best and somebody else's service at worst.
///
/// The rule is a comparison rather than a blanket ban, because a private address is
/// exactly right on a private network: an endpoint may be private if the peer that
/// advertised it is *also* private, since then this node is on the same network as
/// it. A public peer advertising a private endpoint is describing something only it
/// can reach. That keeps Hacknet working without a configuration switch —
/// stacks-core's equivalent, `connection_opts.private_neighbors`, is a switch, and a
/// switch is a thing to get wrong.
fn endpoint_is_reachable(endpoint: &str, peer: PeerAddress) -> bool {
    // Only an address literal can be judged; a DNS name resolves to whatever it
    // resolves to, and guessing would reject legitimate hosts.
    let Some(host) = endpoint
        .split("//")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .map(|authority| authority.rsplit_once(':').map_or(authority, |(host, _)| host))
    else {
        return true;
    };
    let Ok(address) = host.trim_matches(['[', ']']).parse::<std::net::IpAddr>() else {
        return true;
    };
    is_private(address) == is_private(peer.to_ip())
}

/// Whether an address is one only somebody on the same network can reach.
///
/// `IpAddr::is_global` is still unstable, so this is the same set stacks-core's
/// `PeerAddress::is_in_private_range` checks, plus the unspecified and link-local
/// addresses that are never a peer's real endpoint either.
const fn is_private(address: std::net::IpAddr) -> bool {
    match address {
        std::net::IpAddr::V4(v4) => {
            v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified()
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback() || v6.is_unspecified() || v6.octets()[0] & 0xfe == 0xfc
        }
    }
}

/// What the rest of the node reads from a running swarm.
///
/// Cloneable and cheap on purpose: the follow loop reads it every round while the
/// swarm task writes it, and neither should wait for the other. A `std` mutex
/// rather than a `tokio` one because nothing holds it across an await.
#[derive(Clone, Debug, Default)]
pub struct Discovered {
    inner: Arc<Mutex<Snapshot>>,
}

#[derive(Clone, Debug, Default)]
struct Snapshot {
    endpoints: Vec<String>,
    claiming: Vec<String>,
    connected: usize,
    known: usize,
}

impl Discovered {
    /// The HTTP endpoints of peers that have handshook and serve RPC.
    #[must_use]
    pub fn endpoints(&self) -> Vec<String> {
        self.read().endpoints
    }

    /// The endpoints of peers whose inventory says they hold tenures of the cycle
    /// this node last asked about.
    ///
    /// This is what an inventory *buys*, on a downloader that walks parent links
    /// backwards: not a schedule but a shortlist. A peer that claims none of the
    /// cycle being walked is a wasted round trip, and finding that out by asking it
    /// for a tenure is exactly the round trip an inventory exists to avoid.
    #[must_use]
    pub fn claiming(&self) -> Vec<String> {
        self.read().claiming
    }

    /// How many outbound sessions are live.
    #[must_use]
    pub fn connected(&self) -> usize {
        self.read().connected
    }

    /// How many peers the table knows of, connected or not.
    #[must_use]
    pub fn known(&self) -> usize {
        self.read().known
    }

    fn read(&self) -> Snapshot {
        // A poisoned lock here would mean a panic while publishing a peer list,
        // which is not a reason to bring the node down: the last good snapshot is
        // still a usable answer, and an empty one is a correct answer too.
        self.inner
            .lock()
            .map(|snapshot| snapshot.clone())
            .unwrap_or_default()
    }

    fn publish(&self, snapshot: Snapshot) {
        if let Ok(mut held) = self.inner.lock() {
            *held = snapshot;
        }
    }
}

/// A peer's claim about which tenures of a reward cycle it has processed.
///
/// A claim, and named as one: the bit vector is what the peer says, the endpoint is
/// where to go and check.
#[derive(Clone, Debug)]
pub struct TenureClaim {
    pub peer: Hash160,
    /// Where to fetch from, if this peer serves HTTP at all.
    pub endpoint: Option<String>,
    pub tenures: BitVec<2100>,
}

/// A bounded set of outbound peers.
pub struct Swarm {
    peers: PeerDb,
    local: LocalPeer,
    protocol: Protocol,
    limits: SwarmLimits,
    connected: Vec<Connected>,
    discovered: Discovered,
    /// What to answer a peer's own requests with, when this node serves anything.
    service: Option<Arc<dyn crate::inbound::Service>>,
    /// Which session to ask for neighbours next, so the walk moves around rather
    /// than learning one peer's whole view of the network and stopping.
    walk_cursor: usize,
    /// The endpoints of peers that claimed tenures the last time inventories were
    /// exchanged, kept so that a round which asks about no cycle does not erase what
    /// the previous one learned.
    claiming: Vec<String>,
}

impl Swarm {
    /// Build a swarm over an existing peer table.
    #[must_use]
    pub fn new(
        peers: PeerDb,
        local: LocalPeer,
        protocol: Protocol,
        limits: SwarmLimits,
    ) -> Self {
        Self {
            peers,
            local,
            protocol,
            limits,
            connected: Vec::new(),
            discovered: Discovered::default(),
            service: None,
            walk_cursor: 0,
            claiming: Vec::new(),
        }
    }

    /// Answer the requests peers send on the connections *this node opened*.
    ///
    /// The same `Service` the listener answers with, because the protocol is
    /// symmetric once handshook: a stock node that nano dialled will ask nano for
    /// neighbours and inventories on that same socket, and a node that only answered
    /// the connections dialled *to* it is invisible to every peer behind a NAT.
    #[must_use]
    pub fn serving(mut self, service: Arc<dyn crate::inbound::Service>) -> Self {
        self.service = Some(service);
        self
    }

    /// The handle the rest of the node reads while this runs.
    #[must_use]
    pub fn discovered(&self) -> Discovered {
        self.discovered.clone()
    }

    #[must_use]
    pub const fn peer_table(&self) -> &PeerDb {
        &self.peers
    }

    /// Record a configured bootstrap peer, resolving its host.
    ///
    /// The key in a seed specification is ignored beyond being parsed: a session
    /// learns the peer's key from its handshake and authenticates against that, so
    /// a configured key that turns out to be wrong changes nothing about what nano
    /// accepts, and treating it as authority would be trusting a config file over
    /// evidence.
    pub async fn seed(&mut self, spec: &str) -> Result<usize, PeerDbError> {
        let Some((_, host, port)) = crate::parse_seed(spec) else {
            return Ok(0);
        };
        let Ok(addresses) = tokio::net::lookup_host((host.as_str(), port)).await else {
            return Ok(0);
        };
        let mut recorded = 0;
        for address in addresses {
            self.peers.seed(PeerAddress::from_ip(address.ip()), port)?;
            recorded += 1;
        }
        Ok(recorded)
    }

    /// Bring the session set back up to strength, and learn some addresses.
    ///
    /// Every failure in here is per-peer and swallowed into the returned counts:
    /// the one thing a maintenance round must not do is fail, because the node's
    /// only alternative to a thin peer set is no peer set.
    pub async fn maintain(&mut self, view: ChainView, cycle_start: Option<ConsensusHash>) -> Round {
        let mut round = Round::default();
        self.check_liveness(view, &mut round).await;
        self.dial_up_to_strength(view, &mut round).await;
        round.learned = self.walk_neighbors().await;
        if let Some(cycle_start) = cycle_start {
            round.claiming = self.exchange_inventories(cycle_start, &mut round).await;
        }
        round.collected = self.collect(&mut round).await;
        round.connected = self.connected.len();
        self.publish();
        round
    }

    /// Read what every peer said while we were not listening.
    ///
    /// The round ends here rather than beginning here, so a peer dialled *this* round
    /// is read too. It matters that this happens at all: a session nobody reads
    /// accumulates whatever the peer relays, and mainnet relays between 0.2 and 0.8
    /// messages a second per peer — enough that fifty seconds of not listening looks
    /// like a flood, which is what nano used to mistake it for.
    async fn collect(&mut self, round: &mut Round) -> usize {
        let mut faults = Vec::new();
        let mut collected = 0;
        for (index, peer) in self.connected.iter_mut().enumerate() {
            if let Err(error) = peer.session.collect().await {
                faults.push((index, error));
            }
            collected += peer.session.take_unsolicited_count();
        }
        self.retire(&faults, round);
        collected
    }

    /// Ping every session, and act on whichever ones do not answer.
    async fn check_liveness(&mut self, view: ChainView, round: &mut Round) {
        let mut survivors = Vec::with_capacity(self.connected.len());
        for mut peer in std::mem::take(&mut self.connected) {
            peer.session.advertise(view);
            match peer.session.ping().await {
                Ok(()) => survivors.push(peer),
                Err(error) => self.penalize(&peer, &error, round),
            }
        }
        self.connected = survivors;
    }

    /// Open sessions until the limit is reached or the candidates run out.
    async fn dial_up_to_strength(&mut self, view: ChainView, round: &mut Round) {
        if self.connected.len() >= self.limits.outbound {
            return;
        }
        let held: HashSet<(PeerAddress, u16)> = self
            .connected
            .iter()
            .map(|peer| (peer.address, peer.port))
            .collect();
        // Ask for more candidates than there are slots: most of them are addresses
        // learned from gossip that have never answered, and a batch that is exactly
        // slot-sized would spend every round on the same dead ones.
        let wanted = self.limits.outbound.saturating_sub(self.connected.len());
        let Ok(candidates) = self.peers.candidates(wanted + self.limits.dials_per_round * 4) else {
            return;
        };
        let mut attempts = 0;
        for candidate in candidates {
            if self.connected.len() >= self.limits.outbound
                || attempts >= self.limits.dials_per_round
            {
                break;
            }
            if held.contains(&(candidate.address, candidate.port)) {
                continue;
            }
            attempts += 1;
            let dialled = Session::open(
                candidate.socket_addr(),
                &self.local,
                self.protocol,
                view,
                self.limits.timeout,
            )
            .await;
            match dialled {
                Ok(mut session) => {
                    if let Some(service) = &self.service {
                        session.serving(service.clone());
                    }
                    // A completed handshake is the only thing that proves a key, so
                    // it is the only thing allowed to write one down.
                    let _ = self.peers.record_handshake(
                        candidate.address,
                        candidate.port,
                        session.handshake(),
                        self.protocol.peer_version,
                        self.protocol.network_id,
                    );
                    round.dialled += 1;
                    self.connected.push(Connected {
                        session,
                        address: candidate.address,
                        port: candidate.port,
                    });
                }
                Err(error) => {
                    // A peer that refuses a connection is not a peer that lied, so
                    // it earns a backoff and stays known. One that breaks the
                    // protocol during the handshake is isolated on the spot.
                    let isolate = error.is_protocol_fault();
                    let _ = if isolate {
                        round.isolated += 1;
                        self.peers.isolate(candidate.address, candidate.port)
                    } else {
                        self.peers.record_failure(candidate.address, candidate.port)
                    };
                }
            }
        }
    }

    /// Ask one peer who else it knows.
    ///
    /// One per round, and a different one each round. Asking all of them would
    /// learn the same addresses several times over and spend a round doing it,
    /// while the table only has to grow faster than peers go away.
    async fn walk_neighbors(&mut self) -> usize {
        if self.connected.is_empty() {
            return 0;
        }
        self.walk_cursor = self.walk_cursor.wrapping_add(1) % self.connected.len();
        let Some(peer) = self.connected.get_mut(self.walk_cursor) else {
            return 0;
        };
        let Ok(neighbors) = peer.session.neighbors().await else {
            return 0;
        };
        self.peers.learn(&neighbors).unwrap_or(0)
    }

    /// Ask every peer which tenures of a reward cycle it has processed.
    ///
    /// Several peers on purpose. One peer's inventory is one peer's claim, and a
    /// tenure that only one peer admits to having is exactly the case where a
    /// second opinion is worth the request.
    pub async fn tenure_claims(
        &mut self,
        cycle_start: ConsensusHash,
        round: &mut Round,
    ) -> Vec<TenureClaim> {
        let mut claims = Vec::new();
        let mut faults = Vec::new();
        for (index, peer) in self.connected.iter_mut().enumerate() {
            let endpoint = peer.endpoint();
            let hash = peer.session.public_key_hash();
            match peer.session.nakamoto_inventory(cycle_start).await {
                Ok(tenures) => claims.push(TenureClaim {
                    peer: hash,
                    endpoint,
                    tenures,
                }),
                // A peer that does not know the cycle nacks, which is an answer and
                // not a fault: this node may be asking about a fork that peer never
                // saw, or about a cycle it has pruned.
                Err(SessionError::Nack(_)) => {}
                Err(error) => faults.push((index, error)),
            }
        }
        self.retire(&faults, round);
        claims
    }

    /// Ask every peer about a reward cycle, and remember who has any of it.
    ///
    /// The result is a shortlist of endpoints, not a schedule, because nano's
    /// downloader walks parent links backwards and so always knows the one tenure it
    /// wants next — there is no set of wanted tenures to spread. What an inventory is
    /// worth on that downloader is still real: a peer that claims none of the cycle
    /// being walked has nothing to serve, and asking it for a tenure to find out is
    /// exactly the round trip the inventory exists to avoid.
    ///
    /// [`assign_tenures`] is the scheduler for a forward download driven by bit
    /// indices, which is a different downloader; it stays where it is until there is
    /// one.
    pub async fn exchange_inventories(
        &mut self,
        cycle_start: ConsensusHash,
        round: &mut Round,
    ) -> usize {
        let claims = self.tenure_claims(cycle_start, round).await;
        // A peer that answered with an all-zero vector is answering honestly and has
        // nothing for this cycle; it stays a peer and stops being a place to fetch
        // from until the cycle moves.
        let claiming: Vec<String> = claims
            .iter()
            .filter(|claim| (0..claim.tenures.len()).any(|bit| claim.tenures.get(bit) == Some(true)))
            .filter_map(|claim| claim.endpoint.clone())
            .collect();
        let answered = claiming.len();
        self.claiming = claiming;
        self.publish();
        answered
    }

    /// Close the sessions that failed during a request, penalising each.
    ///
    /// The round is the caller's, not a local one. It used to be local and thrown
    /// away, so a peer lost during an inventory exchange left a round reporting three
    /// peers connected and one dropped when it had in fact lost all three — the count
    /// that made a real failure look like a passing test.
    fn retire(&mut self, faults: &[(usize, SessionError)], round: &mut Round) {
        if faults.is_empty() {
            return;
        }
        let doomed: HashSet<usize> = faults.iter().map(|(index, _)| *index).collect();
        for (index, error) in faults {
            if let Some(peer) = self.connected.get(*index) {
                eprintln!(
                    "p2p: dropping peer {} after {error}",
                    peer.address.to_socket_addr(peer.port)
                );
                self.penalize(peer, error, round);
            }
        }
        let mut kept = Vec::with_capacity(self.connected.len());
        for (index, peer) in std::mem::take(&mut self.connected).into_iter().enumerate() {
            if !doomed.contains(&index) {
                kept.push(peer);
            }
        }
        self.connected = kept;
        self.publish();
    }

    /// Record what a peer did wrong, in the peer table that outlives the session.
    fn penalize(&self, peer: &Connected, error: &SessionError, round: &mut Round) {
        let _ = if error.is_protocol_fault() {
            round.isolated += 1;
            self.peers.isolate(peer.address, peer.port)
        } else {
            round.dropped += 1;
            self.peers.record_failure(peer.address, peer.port)
        };
    }

    fn publish(&self) {
        // De-duplicated, because several peers behind one load balancer advertise
        // the same endpoint: mainnet returned two copies of one address out of eight
        // peers. Left in, a pool of "eight" would have had six distinct places to
        // fetch from, and "no single peer is load bearing" would have been counting
        // the same peer twice.
        let mut endpoints: Vec<String> = self
            .connected
            .iter()
            .filter_map(Connected::endpoint)
            .collect();
        endpoints.sort_unstable();
        endpoints.dedup();
        self.discovered.publish(Snapshot {
            endpoints,
            claiming: self.claiming.clone(),
            connected: self.connected.len(),
            known: self.peers.count().unwrap_or(0),
        });
    }

    /// Push what this node has accepted to the peers that did not send it, and read
    /// whatever they have said since.
    ///
    /// Cheap enough to run on the node's own poll interval rather than the discovery
    /// interval: one write per peer per item and no round trip, because a push is an
    /// announcement and the peer is not expected to answer it. Running it often is
    /// also what keeps a peer's own pushes out of the socket, which is why it collects
    /// on the way out.
    ///
    /// A peer that fails a write is retired exactly as it would be for a failed
    /// request — a broken pipe is a peer that has gone, not a peer that lied.
    pub async fn relay(&mut self, offers: &[crate::relay::Offer], round: &mut Round) -> usize {
        let mut sent = 0;
        let mut faults: Vec<(usize, SessionError)> = Vec::new();
        for offer in offers {
            for (index, peer) in self.connected.iter_mut().enumerate() {
                // One fault per peer per round: a peer whose socket has closed would
                // otherwise be penalised once for every item in the batch.
                if faults.iter().any(|(failed, _)| *failed == index) {
                    continue;
                }
                // Never back where it came from. The peer already has it, and a node
                // that echoes is a node its neighbours stop listening to.
                if offer.from == Some(peer.session.public_key_hash()) {
                    continue;
                }
                // Cloned per peer because each message carries its own sequence
                // number and so its own signature. Encoding the frame once and
                // signing it per peer would save the copy, at the cost of a
                // second signing path that nothing else in the crate needs.
                let payload = match &offer.data {
                    crate::relay::Pushed::Block(block) => {
                        crate::wire::Payload::NakamotoBlocks(vec![(**block).clone()])
                    }
                    crate::relay::Pushed::Transaction(transaction) => {
                        crate::wire::Payload::Transaction(transaction.clone())
                    }
                };
                match peer.session.push(payload).await {
                    Ok(()) => sent += 1,
                    Err(error) => faults.push((index, error)),
                }
            }
        }
        self.retire(&faults, round);
        round.collected += self.collect(round).await;
        round.connected = self.connected.len();
        sent
    }

    /// Take the blocks and transactions peers pushed while we were asking them
    /// things, so a caller can put them through its own checks.
    ///
    /// Nothing here looks at them. A pushed block is the least-verified thing a
    /// node ever sees, and the whole design of this crate is that authentication
    /// says *who* sent something and never that it is true.
    pub fn take_pushed(&mut self) -> Vec<(Hash160, crate::wire::Message)> {
        let mut pushed = Vec::new();
        for peer in &mut self.connected {
            let hash = peer.session.public_key_hash();
            pushed.extend(
                peer.session
                    .take_pushed()
                    .into_iter()
                    .map(|message| (hash, message)),
            );
        }
        pushed
    }
}

/// Spread a cycle's wanted tenures over the peers that claim to have them.
///
/// This is the scheduling half of inventory sync, kept as a pure function because
/// it is the part with a policy in it and no I/O: given what each peer says it has
/// and which tenures this node is missing, decide who to ask for what.
///
/// Three properties, in the order they matter:
///
/// * **Only peers that claim a tenure are asked for it.** That is the entire point
///   of an inventory — without it, every fetch is a guess and a miss costs a round
///   trip.
/// * **The work is spread.** Each tenure goes to the next claiming peer in turn
///   rather than to the first, so a cycle of two thousand tenures does not become
///   two thousand requests to whichever peer sorted first. That is the difference
///   between many peers and one peer with witnesses.
/// * **The order is this node's, not a peer's.** Claims are sorted by peer key hash
///   before anything is assigned, so the answer does not depend on which peer
///   replied first — the same reasoning as `choose_canonical_tip`'s tie-break.
///
/// A tenure nobody claims is absent from the result rather than assigned to
/// somebody at random: the honest response to "no peer has this" is to ask again
/// later, once the peer set has changed.
#[must_use]
pub fn assign_tenures(claims: &[TenureClaim], wanted: &[u16]) -> Vec<(u16, String)> {
    let mut servers: Vec<(&Hash160, &String, &BitVec<2100>)> = claims
        .iter()
        .filter_map(|claim| {
            claim
                .endpoint
                .as_ref()
                .map(|endpoint| (&claim.peer, endpoint, &claim.tenures))
        })
        .collect();
    servers.sort_by(|left, right| left.0.cmp(right.0));
    if servers.is_empty() {
        return Vec::new();
    }
    let mut next = 0;
    let mut assignments = Vec::new();
    for &tenure in wanted {
        // Start from wherever the last assignment left off, so consecutive tenures
        // land on different peers, and walk at most once round.
        for offset in 0..servers.len() {
            let index = (next + offset) % servers.len();
            let Some((_, endpoint, tenures)) = servers.get(index) else {
                continue;
            };
            if tenures.get(tenure) == Some(true) {
                assignments.push((tenure, (*endpoint).clone()));
                next = index + 1;
                break;
            }
        }
    }
    assignments
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(peer: u8, endpoint: Option<&str>, has: &[u16]) -> TenureClaim {
        let mut tenures = BitVec::<2100>::zeros(2100).expect("a cycle-length bit vector");
        for &index in has {
            tenures.set(index, true).expect("in bounds");
        }
        TenureClaim {
            peer: Hash160::from_bytes([peer; 20]),
            endpoint: endpoint.map(str::to_owned),
            tenures,
        }
    }

    /// A private endpoint is reachable from a private peer and not from a public one.
    ///
    /// Mainnet really does advertise `http://10.0.1.37:20443` — a load-balanced node
    /// naming the address it sees itself at — and a node that fetched from it would
    /// be dialling its own network.
    #[test]
    fn only_a_private_peer_may_advertise_a_private_endpoint() {
        for (peer, endpoint, reachable) in [
            ("34.150.184.50", "http://34.150.184.50:20443", true),
            ("34.150.184.50", "http://10.0.1.37:20443", false),
            ("34.150.184.50", "http://127.0.0.1:20443", false),
            ("34.150.184.50", "http://0.0.0.0:20443", false),
            // On a private network, a private endpoint is the only kind there is.
            ("10.0.1.5", "http://10.0.1.37:20443", true),
            ("10.0.1.5", "http://34.150.184.50:20443", false),
            // A name resolves to whatever it resolves to; guessing would reject
            // legitimate hosts.
            ("34.150.184.50", "https://node.example.com/", true),
            ("34.150.184.50", "http://node.example.com:20443/v2/info", true),
            ("34.150.184.50", "http://[2001:db8::1]:20443", true),
            ("34.150.184.50", "http://[::1]:20443", false),
            ("34.150.184.50", "http://[fc00::1]:20443", false),
        ] {
            let address = PeerAddress::from_ip(peer.parse().expect("an address"));
            assert_eq!(
                endpoint_is_reachable(endpoint, address),
                reachable,
                "{peer} advertising {endpoint}"
            );
        }
    }

    /// Work goes only to peers that claim it, and is spread across them.
    #[test]
    fn wanted_tenures_are_spread_over_the_peers_that_claim_them() {
        let claims = [
            claim(1, Some("http://a"), &[0, 1, 2, 3]),
            claim(2, Some("http://b"), &[0, 1, 2, 3]),
        ];
        let assignments = assign_tenures(&claims, &[0, 1, 2, 3]);
        assert_eq!(
            assignments,
            vec![
                (0, "http://a".to_owned()),
                (1, "http://b".to_owned()),
                (2, "http://a".to_owned()),
                (3, "http://b".to_owned()),
            ]
        );
    }

    /// A tenure only one peer has goes to that peer, however the round-robin fell.
    #[test]
    fn a_tenure_only_one_peer_claims_goes_to_that_peer() {
        let claims = [
            claim(1, Some("http://a"), &[0, 1]),
            claim(2, Some("http://b"), &[1]),
        ];
        // Tenure 0 can only come from a, and asking b for it would be a wasted
        // round trip that an inventory exists precisely to avoid.
        assert_eq!(
            assign_tenures(&claims, &[0, 0, 0]),
            vec![
                (0, "http://a".to_owned()),
                (0, "http://a".to_owned()),
                (0, "http://a".to_owned()),
            ]
        );
    }

    /// A tenure nobody claims is left unassigned rather than guessed at.
    #[test]
    fn a_tenure_nobody_claims_is_not_assigned() {
        let claims = [claim(1, Some("http://a"), &[0])];
        assert_eq!(
            assign_tenures(&claims, &[0, 7]),
            vec![(0, "http://a".to_owned())]
        );
        assert!(assign_tenures(&[], &[0]).is_empty());
    }

    /// A peer with no HTTP endpoint is not somewhere to fetch from.
    ///
    /// Its inventory is still worth having as a second opinion, but there is nothing
    /// at the other end to ask, so it cannot be assigned work.
    #[test]
    fn a_peer_without_an_endpoint_is_not_assigned_work() {
        let claims = [claim(1, None, &[0, 1]), claim(2, Some("http://b"), &[0, 1])];
        assert_eq!(
            assign_tenures(&claims, &[0, 1]),
            vec![(0, "http://b".to_owned()), (1, "http://b".to_owned())]
        );
    }

    /// The assignment does not depend on which peer answered first.
    ///
    /// Claims arrive in whatever order the sessions happened to reply in, and an
    /// assignment that varied with it would make two nodes with the same peers and
    /// the same inventories fetch differently — the same failure `choose_canonical_tip`
    /// avoids by tie-breaking on the block identifier.
    #[test]
    fn the_assignment_does_not_depend_on_reply_order() {
        let first = [
            claim(9, Some("http://i"), &[0, 1]),
            claim(3, Some("http://c"), &[0, 1]),
        ];
        let reversed = [
            claim(3, Some("http://c"), &[0, 1]),
            claim(9, Some("http://i"), &[0, 1]),
        ];
        assert_eq!(
            assign_tenures(&first, &[0, 1]),
            assign_tenures(&reversed, &[0, 1])
        );
        assert_eq!(
            assign_tenures(&first, &[0, 1]),
            vec![(0, "http://c".to_owned()), (1, "http://i".to_owned())]
        );
    }
}
