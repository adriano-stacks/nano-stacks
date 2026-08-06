//! Hosting somebody else's signer: the two things a stock `stacks-signer`
//! needs from the node it is pointed at and cannot get anywhere else.
//!
//! A signer has no chain state and no peers. It reads the miner's proposal from
//! its node's `.miners` replica, asks its node whether the block executes, and
//! writes its answer back into its node's own replica. Everything after that is
//! the node's problem: nothing else carries the chunk to the miner counting it.
//!
//! So this module is two loops.
//!
//! **Validating proposals.** nano vouches for a state root by executing the
//! block, and a candidate off the tip must not be executed into the state the
//! node serves. The validator here keeps a chain state of its own — the same one
//! nano's embedded signer uses — and the route reaches it through a channel
//! rather than a lock, because deciding needs the peer as well as the state and
//! an HTTP handler is the wrong place to wait for either.
//!
//! **Replicating chunks.** A node that only serves its own replica hosts a
//! signer that can see no proposals and whose answers reach nobody. Chunks are
//! pulled from a peer and pushed back to it over the same `/v2/stackerdb` routes
//! the signer uses, and every pulled chunk is verified against the writer this
//! node assigned the slot — replication, not trust.
//!
//! Neither loop is bound to one peer. Both used to clone the single `SyncClient`
//! the runtime picked at startup and loop on it forever, which on mainnet was the
//! hosted API: chain synchronization could survive losing it, and the signer this
//! node hosts silently could not. They walk the same pool the chain is followed
//! over now, rebuilt as peer discovery finds more, and each says which peer served
//! it so a run can show that no single one was load bearing.

use std::{
    collections::BTreeSet,
    time::Duration,
};

use nano_p2p::Discovered;
use nano_primitives::Network;
use nano_rpc::{ProposalRejectCode, ProposalRequest, RpcState};
use nano_signer::{AccumulatedCoinbase as _, ProposalValidator as _};
use nano_stackerdb::{BlockProposal, Chunk, StackerDbClient, StackerDbContract};
use nano_sync::{PoxInfo, SyncClient, TenureSource};
use tokio::{sync::mpsc::UnboundedReceiver, time::sleep};

use crate::{
    config::{Config, cycle_contract, miner_contract},
    runtime::Role,
    signer::{self, Validator},
};

/// Answer for proposals until the node stops.
///
/// Every proposal is answered, including the ones this node cannot judge: a
/// signer waiting on a verdict that never comes treats the wait as a rejection
/// after a timeout, which is the same outcome reached slower and with less said.
pub async fn validate_proposals(
    config: Config,
    pox: PoxInfo,
    discovered: Option<Discovered>,
    mut peers: TenureSource,
    mut validator: Validator,
    mut requests: UnboundedReceiver<ProposalRequest>,
) -> Role {
    let interval = Duration::from_secs(config.node.poll_interval_secs);
    let mut endpoints = peers.endpoints();
    println!(
        "validating block proposals for the signers this node hosts, over {} peers",
        endpoints.len().max(1)
    );
    loop {
        // A throttle and a failure are set aside for a round, and a validator's
        // round is one proposal: forgiving them here is what stops a peer that was
        // unreachable once from being written off for the life of the node.
        peers.forgive_throttles();
        refresh_pool(&config, discovered.as_ref(), &mut peers, &mut endpoints, "validating proposals");
        let request = tokio::select! {
            request = requests.recv() => request,
            // A validator that only catches up when asked would execute the whole
            // canonical chain inside the first proposal it is given.
            () = sleep(interval) => {
                if let Err(error) =
                    signer::catch_up(&mut peers, &mut validator, config.node.max_sync_blocks).await
                {
                    eprintln!("the proposal validator could not follow the chain: {error}");
                }
                continue;
            }
        };
        let Some(request) = request else {
            return Err("the proposal route closed".to_owned());
        };
        let verdict = judge(&config, &pox, &mut peers, &mut validator, &request).await;
        // Nobody is left to tell if the request was abandoned, which is not an
        // error: the block was still executed and the state was still checked.
        drop(request.verdict.send(verdict));
    }
}

/// Take account of what peer discovery has learned since the last round.
///
/// Guarded on the endpoint list having *changed*, because rebuilding the pool
/// discards the position its rotation had walked to and the set-asides it had
/// learned — so doing it every round would leave every role asking the same first
/// peer forever.
fn refresh_pool(
    config: &Config,
    discovered: Option<&Discovered>,
    peers: &mut TenureSource,
    endpoints: &mut Vec<String>,
    what: &str,
) {
    let found = crate::runtime::follow_endpoints(config, discovered);
    if found == *endpoints || found.is_empty() {
        return;
    }
    let rebuilt = nano_sync::PeerPool::from_endpoints(&found);
    if rebuilt.is_empty() {
        return;
    }
    println!("{what} over {} peers: {}", rebuilt.len(), rebuilt.endpoints().join(", "));
    *peers = TenureSource::new(rebuilt.into_clients());
    *endpoints = found;
}

/// Execute one proposal and say what happened to it.
async fn judge(
    config: &Config,
    pox: &PoxInfo,
    peers: &mut TenureSource,
    validator: &mut Validator,
    request: &ProposalRequest,
) -> Result<(), (String, ProposalRejectCode)> {
    let block = &request.block;
    signer::catch_up(peers, validator, config.node.max_sync_blocks)
        .await
        .map_err(|error| {
            (
                format!("this node could not follow the chain the proposal builds on: {error}"),
                ProposalRejectCode::ChainstateError,
            )
        })?;
    let sortition = peers
        .sortition(block.header.consensus_hash)
        .await
        .map_err(|error| {
            (
                format!(
                    "this node has no sortition for the tenure {} the proposal names: {error}",
                    block.header.consensus_hash
                ),
                ProposalRejectCode::NoSuchTenure,
            )
        })?;
    let cycle = pox.reward_cycle(sortition.bitcoin_height);
    // A tenure's coinbase depends on the burn blocks since the last sortition, so
    // a proposal validated without it would seal a root that differs from the
    // network's, and the validator refuses to guess.
    let schedule = validator.coinbase_schedule();
    match peers
        .accumulated_coinbase(block, schedule, sortition.bitcoin_height)
        .await
    {
        Ok(Some(accumulated)) => {
            validator.set_accumulated_coinbase(sortition.bitcoin_height, accumulated);
        }
        Ok(None) => {}
        Err(error) => {
            return Err((
                format!("this node could not read the tenure's accumulated coinbase: {error}"),
                ProposalRejectCode::ChainstateError,
            ));
        }
    }
    let bitcoin_height = sortition.bitcoin_height;
    validator.set_context(sortition, cycle);
    validator
        .validate(&BlockProposal {
            block: block.clone(),
            bitcoin_height,
            reward_cycle: cycle,
            data: BlockProposal::empty_data(),
        })
        .map_err(classify)
}

/// Say whether a refusal is about the block or about this node.
///
/// The validator answers in prose, and the distinction matters to a signer: a
/// block that does not execute is `InvalidBlock` and must never be signed, while
/// a node that was missing a parent or a burn block is saying nothing about the
/// block at all. Reporting the second as the first would have this node telling a
/// signer that a perfectly good block is invalid.
fn classify(error: String) -> (String, ProposalRejectCode) {
    let code = if error.contains("trusted chain view") {
        ProposalRejectCode::UnknownParent
    } else if error.contains("accumulated coinbase") || error.contains("Bitcoin operations") {
        ProposalRejectCode::ChainstateError
    } else {
        ProposalRejectCode::InvalidBlock
    };
    (error, code)
}

/// Every contract a reward cycle's signers and its miners exchange chunks on.
#[must_use]
pub fn replicated(network: Network, cycle: u64) -> Vec<StackerDbContract> {
    let mut contracts = vec![miner_contract(network)];
    contracts.extend(
        crate::runtime::SIGNER_MESSAGE_IDS
            .into_iter()
            .map(|message| cycle_contract(network, cycle, message)),
    );
    contracts
}

/// The peers this node replicates `StackerDB` chunks with, and which of them have.
///
/// A replication round is a conversation — slot metadata, then the chunks it says
/// are newer — so the peer is chosen per round rather than per request, and the
/// round that fails anywhere moves the choice on. What that buys is the whole
/// point of the pool: losing the peer this node happened to start with costs one
/// round instead of the hosted signer's liveness.
///
/// Rotation is not trust. Every chunk taken is still verified against the writer
/// this node assigned the slot, inside `put`, so a peer reached by failover can no
/// more forge a chunk than the first one could.
pub struct Replicas {
    peers: Vec<SyncClient>,
    /// One client per peer, so a round does not build a connection pool per request.
    clients: Vec<StackerDbClient>,
    endpoints: Vec<String>,
    /// Whose turn it is, which only moves on a failure.
    cursor: usize,
    /// The peers that have actually served a round, which is what makes "not bound
    /// to one peer" a measurement rather than an intention.
    served: BTreeSet<String>,
    /// How many rounds have failed on the peer they were asked of. Bounded by
    /// being a count: a run that reports zero and one endpoint has proved nothing.
    failures: u64,
}

impl Replicas {
    /// Build clients for the endpoints that parse, keeping their order.
    #[must_use]
    pub fn from_endpoints(endpoints: &[String]) -> Self {
        let peers = nano_sync::PeerPool::from_endpoints(endpoints).into_clients();
        let clients = peers
            .iter()
            .filter_map(|peer| StackerDbClient::new(peer.base_url().clone()).ok())
            .collect::<Vec<_>>();
        // An endpoint whose StackerDB client will not build is dropped along with
        // its `SyncClient`, so the two stay index-aligned.
        let peers = peers
            .into_iter()
            .filter(|peer| StackerDbClient::new(peer.base_url().clone()).is_ok())
            .collect::<Vec<_>>();
        Self {
            endpoints: peers.iter().map(|peer| peer.base_url().to_string()).collect(),
            peers,
            clients,
            cursor: 0,
            served: BTreeSet::new(),
            failures: 0,
        }
    }

    /// Whose turn it is this round, if there is anybody to ask.
    fn current(&self) -> Option<(&SyncClient, &StackerDbClient)> {
        if self.peers.is_empty() {
            return None;
        }
        let index = self.cursor % self.peers.len();
        Some((self.peers.get(index)?, self.clients.get(index)?))
    }

    /// The endpoint this round is talking to.
    #[must_use]
    pub fn serving(&self) -> Option<&str> {
        self.peers
            .get(self.cursor.checked_rem(self.peers.len())?)
            .map(|peer| peer.base_url().as_str())
    }

    /// Whether there is anybody to ask at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// Whose turn it is, ready to be handed to whatever holds a client.
    #[must_use]
    pub fn current_pair(&self) -> Option<(SyncClient, StackerDbClient)> {
        self.current()
            .map(|(peer, client)| (peer.clone(), client.clone()))
    }

    /// The peer to talk to now, when it is not the one already named.
    ///
    /// Answers nothing while the turn has not moved, so an ordinary round keeps the
    /// connections it had rather than rebuilding them every poll; `serving` is
    /// updated to whatever it hands back.
    pub fn retargeted(&mut self, serving: &mut Option<String>) -> Option<(SyncClient, StackerDbClient)> {
        let (peer, client) = self.current()?;
        let endpoint = peer.base_url().to_string();
        if serving.as_deref() == Some(endpoint.as_str()) {
            return None;
        }
        let pair = (peer.clone(), client.clone());
        *serving = Some(endpoint);
        Some(pair)
    }

    /// Move on, because the peer this round asked did not answer.
    pub const fn rotate(&mut self) {
        self.failures = self.failures.saturating_add(1);
        if !self.peers.is_empty() {
            self.cursor = (self.cursor + 1) % self.peers.len();
        }
    }

    /// Note that a round completed against whoever it was talking to.
    pub fn credit(&mut self) {
        if let Some(endpoint) = self.serving().map(ToOwned::to_owned) {
            self.served.insert(endpoint);
        }
    }

    /// How many distinct peers have served a round, and how many rounds failed.
    #[must_use]
    pub fn distribution(&self) -> (usize, u64) {
        (self.served.len(), self.failures)
    }

    /// Take on the endpoints discovery has found, keeping whose turn it was.
    ///
    /// The cursor follows the endpoint it was pointing at rather than the index, so
    /// a pool that grew does not silently send the next round back to the front.
    pub fn refresh(&mut self, endpoints: &[String]) {
        if endpoints == self.endpoints.as_slice() || endpoints.is_empty() {
            return;
        }
        let serving = self.serving().map(ToOwned::to_owned);
        let rebuilt = Self::from_endpoints(endpoints);
        if rebuilt.peers.is_empty() {
            return;
        }
        println!(
            "replicating StackerDB chunks over {} peers: {}",
            rebuilt.endpoints.len(),
            rebuilt.endpoints.join(", ")
        );
        let cursor = serving
            .and_then(|endpoint| rebuilt.endpoints.iter().position(|held| *held == endpoint))
            .unwrap_or(0);
        let (served, failures) = (std::mem::take(&mut self.served), self.failures);
        *self = rebuilt;
        self.cursor = cursor;
        self.served = served;
        self.failures = failures;
    }
}

/// Keep this node's replicas and its peers' in step, both ways.
pub async fn replicate(
    config: Config,
    network: Network,
    discovered: Option<Discovered>,
    mut replicas: Replicas,
    state: RpcState,
    mut written: UnboundedReceiver<(String, Chunk)>,
) -> Role {
    if replicas.is_empty() {
        return Err("no peer to replicate StackerDB chunks with".to_owned());
    }
    let interval = Duration::from_secs(config.node.poll_interval_secs);
    println!(
        "replicating StackerDB chunks over {} peers: {}",
        replicas.endpoints.len(),
        replicas.endpoints.join(", ")
    );
    let mut outbound = Vec::new();
    loop {
        replicas.refresh(&crate::runtime::follow_endpoints(
            &config,
            discovered.as_ref(),
        ));
        // Drained before the round rather than inside it, and kept when the round
        // fails: a chunk the hosted signer wrote is the whole reason this loop
        // exists, and dropping it because the peer whose turn it was went away
        // would lose a signature the network is counting.
        while let Ok(written) = written.try_recv() {
            outbound.push(written);
        }
        match round(&mut replicas, network, &state, &outbound).await {
            Ok(()) => outbound.clear(),
            Err(refusal) => eprintln!("{refusal}"),
        }
        if replicas.is_empty() {
            return Err("no peer left to replicate StackerDB chunks with".to_owned());
        }
        sleep(interval).await;
    }
}

/// One round with whichever peer's turn it is: what it holds, and what this node
/// has to hand it.
///
/// The seam the loop is built on, so the rotation can be measured without a clock.
/// A round that the peer did not answer moves the turn on and says why; a round it
/// answered credits it, whatever this node then decided about the chunks
/// themselves.
pub async fn round(
    replicas: &mut Replicas,
    network: Network,
    state: &RpcState,
    outbound: &[(String, Chunk)],
) -> Result<(), String> {
    let Some((peer, client)) = replicas.current_pair() else {
        return Err("no peer left to replicate StackerDB chunks with".to_owned());
    };
    let cycle = match peer.tenure_info().await {
        Ok(tenure) => tenure.reward_cycle,
        Err(error) => {
            replicas.rotate();
            return Err(format!(
                "{} cannot tell StackerDB replication the active cycle, trying another peer: \
                 {error}",
                peer.base_url()
            ));
        }
    };
    let contracts = replicated(network, cycle);
    // Outbound first: a chunk a hosted signer wrote is what the network is
    // waiting for, and it must not wait behind a round of pulling.
    let mut answered = true;
    for (contract_id, chunk) in outbound {
        answered &= push(&client, &contracts, contract_id, chunk).await;
    }
    for contract in &contracts {
        answered &= pull(&client, state, contract).await;
    }
    if answered {
        replicas.credit();
        return Ok(());
    }
    replicas.rotate();
    Err(format!(
        "{} did not serve a round of StackerDB replication, trying another peer",
        peer.base_url()
    ))
}

/// Hand a chunk this node took to the peer that has to see it.
///
/// Answers whether the peer *responded*, not whether it took the chunk: a refusal
/// is the peer working and usually means it already has it, so it is no reason to
/// go looking for another one.
async fn push(
    client: &StackerDbClient,
    contracts: &[StackerDbContract],
    contract_id: &str,
    chunk: &Chunk,
) -> bool {
    let Some(contract) = contracts
        .iter()
        .find(|contract| identifier(contract) == contract_id)
    else {
        return true;
    };
    match client.put_chunk(contract, chunk).await {
        Ok(acknowledgement) if !acknowledgement.accepted => {
            eprintln!(
                "the peer refused the chunk this node took for {contract_id} slot {}: {}",
                chunk.slot_id,
                acknowledgement.reason.unwrap_or_default()
            );
            true
        }
        Ok(_) => true,
        Err(error) => {
            eprintln!("passing on a {contract_id} chunk failed: {error}");
            false
        }
    }
}

/// Take whatever the peer holds that this node does not.
///
/// Answers whether the peer served the round. A chunk it holds that this node
/// refuses is *this node's* verdict on a forgery and not a failure of the peer to
/// answer — so it does not send the next round elsewhere, and an equivocating peer
/// therefore cannot make a node walk its pool by serving rubbish.
async fn pull(client: &StackerDbClient, state: &RpcState, contract: &StackerDbContract) -> bool {
    let contract_id = identifier(contract);
    // An unconfigured contract has no writers, so nothing could be checked
    // against anything: there is nothing to replicate into.
    let Some(held) = state.stackerdb().read().await.metadata(&contract_id) else {
        return true;
    };
    let Ok(remote) = client.slot_metadata(contract).await else {
        return false;
    };
    let mut answered = true;
    for metadata in remote {
        let slot = usize::try_from(metadata.slot_id).unwrap_or(usize::MAX);
        let newer = held
            .get(slot)
            .is_some_and(|held| held.slot_version < metadata.slot_version);
        if !newer {
            continue;
        }
        // A slot the peer named and then could not serve is the peer failing to
        // answer, which is what makes a stale or half-serving replica cost one
        // round rather than every round.
        let served = client
            .chunk_at(contract, metadata.slot_id, metadata.slot_version)
            .await;
        let Ok(Some(data)) = served else {
            answered = false;
            continue;
        };
        let chunk = Chunk {
            slot_id: metadata.slot_id,
            slot_version: metadata.slot_version,
            signature: metadata.signature,
            data,
        };
        // Verified against the writer this node assigned the slot, inside `put`:
        // a peer serving a forged chunk gets it refused here.
        let taken = state.stackerdb().write().await.put(&contract_id, chunk);
        if let Err(refusal) = taken {
            eprintln!(
                "the chunk the peer holds for {contract_id} slot {} is not one this node \
                 will take: {}",
                metadata.slot_id,
                refusal.reason()
            );
        }
    }
    answered
}

/// The `address.name` a `/v2/stackerdb` route is keyed by.
#[must_use]
pub fn identifier(contract: &StackerDbContract) -> String {
    format!("{}.{}", contract.address, contract.name)
}
