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

use std::{collections::BTreeSet, time::Duration};

use nano_bitcoin::BitcoinSource as _;
use nano_p2p::Discovered;
use nano_primitives::Network;
use nano_rpc::{ProposalRejectCode, ProposalRequest, RpcState};
use nano_signer::{AccumulatedCoinbase as _, ProposalValidator as _};
use nano_stackerdb::{BlockProposal, Chunk, StackerDbClient, StackerDbContract};
use nano_sync::{PoxInfo, SyncClient, TenureSource};
use tokio::time::sleep;

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
    state: RpcState,
    mut requests: nano_queue::Receiver<ProposalRequest>,
) -> Role {
    let interval = Duration::from_secs(config.node.poll_interval_secs);
    let mut burn = match LocalBurnView::open(&config, validator.validator_mut().bitcoin_context()) {
        Ok(burn) => burn,
        Err(error) => return Err(error),
    };
    let mut endpoints = peers.endpoints();
    println!(
        "validating block proposals for the signers this node hosts, over {} peers",
        endpoints.len().max(1)
    );
    loop {
        let queue = requests.status();
        state
            .metrics()
            .publish_ingress_queue(nano_rpc::IngressQueue::Proposals, queue.into());
        state
            .publish_queues(nano_rpc::QueueReport {
                queued_proposals: Some(queue.items),
                ..nano_rpc::QueueReport::default()
            })
            .await;
        // A throttle and a failure are set aside for a round, and a validator's
        // round is one proposal: forgiving them here is what stops a peer that was
        // unreachable once from being written off for the life of the node.
        peers.forgive_throttles();
        refresh_pool(
            &config,
            discovered.as_ref(),
            &mut peers,
            &mut endpoints,
            "validating proposals",
        );
        state.metrics().publish_proposal_peers(endpoints.len());
        let request = tokio::select! {
            request = requests.recv() => request,
            // A validator that only catches up when asked would execute the whole
            // canonical chain inside the first proposal it is given.
            () = sleep(interval) => {
                if let Err(error) =
                    signer::catch_up(
                        &mut peers,
                        &mut burn,
                        &pox,
                        &mut validator,
                        config.node.max_sync_blocks,
                    ).await
                {
                    eprintln!("the proposal validator could not follow the chain: {error}");
                }
                continue;
            }
        };
        let Some(request) = request else {
            return Err("the proposal route closed".to_owned());
        };
        let verdict = judge(
            &config,
            &pox,
            &mut peers,
            &mut burn,
            &mut validator,
            &request,
        )
        .await;
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
    println!(
        "{what} over {} peers: {}",
        rebuilt.len(),
        rebuilt.endpoints().join(", ")
    );
    *peers = TenureSource::new(rebuilt.into_clients());
    *endpoints = found;
}

/// The burn block a proposal is validated under, derived from this node's own
/// burnchain.
///
/// A proposal names a burn view, and everything execution reads from that view --
/// the sortition hash the coinbase proof is over, the seed the winning commitment
/// carried, the winning miner's registered keys -- has to come from somewhere. The
/// validator was refreshing only the *height*, so every proposal was checked
/// against the checkpoint anchor's seed and every tenure-start one was rejected as
/// `committed seed is not the hash of the parent tenure's VRF proof`. The peer that
/// served the proposal is the wrong place to get it from: two of those fields
/// decide whether the tenure is the one the network elected, and the rest land in
/// the state root the header commits to.
///
/// So the same chain the canonical path derives is derived here, off the same
/// Bitcoin blocks, and written down as it advances so a restart resumes it.
pub(crate) struct LocalBurnView {
    tracker: crate::sortition::SortitionTracker,
    bitcoin: crate::runtime::BurnchainSource,
    state: std::path::PathBuf,
    canonical_view: nano_primitives::ConsensusHash,
}

fn requested_burn_height(
    tenure_change: Option<nano_primitives::ConsensusHash>,
    proposal_height: Option<u64>,
    canonical: nano_primitives::ConsensusHash,
    locate: impl FnOnce(nano_primitives::ConsensusHash) -> Result<u64, String>,
) -> Result<u64, String> {
    if let Some(view) = tenure_change {
        let height = locate(view)?;
        if let Some(proposal_height) = proposal_height
            && proposal_height != height
        {
            return Err(format!(
                "proposal says burn {proposal_height}, but its tenure change names burn {height}"
            ));
        }
        return Ok(height);
    }
    proposal_height.map_or_else(|| locate(canonical), Ok)
}

/// The capture a locally derived burn view is seeded from.
///
/// `checkpoint.sortition`, which is the same directory the canonical follower
/// seeds from and the only one that holds a sortition history: `snapshots.json`
/// and the consensus hashes behind it, because a consensus hash mixes the ones at
/// power-of-two offsets back.
///
/// This took `checkpoint.marf`'s *parent directory* instead. That is
/// `chainstate/checkpoint-H` — the trie, the block headers and the native effects,
/// and never a snapshot — so the seed failed on every configuration where the two
/// are not the same directory, which is every mainnet one. With it went the
/// leader-key registry the same loader carries, so the validator could check no
/// tenure's VRF proof and no miner's signature: the two things it exists to check.
fn capture_directory(config: &Config) -> Result<&std::path::Path, String> {
    config.checkpoint.sortition.as_deref().ok_or_else(|| {
        "no checkpoint sortition history is configured, so the proposal validator cannot derive \
         burn views locally and can check no tenure's VRF"
            .to_owned()
    })
}

pub(crate) fn validator_sortition_state(config: &Config) -> std::path::PathBuf {
    config
        .chainstate_dir(crate::runtime::SIGNER_CHAINSTATE)
        .join("sortitions")
}

fn recover_validator_seed<E: std::fmt::Display>(
    tracker: &mut crate::sortition::SortitionTracker,
    block_at: impl FnMut(u64) -> Result<nano_bitcoin::BitcoinBlock, E>,
) -> Result<(), String> {
    tracker.recover_seed(block_at).map_err(|error| {
        format!("the proposal validator cannot authenticate its saved winner: {error}")
    })
}

struct LocalBlockContext {
    bitcoin: nano_chainstate::BitcoinBlockContext,
    sortition: nano_sync::SortitionInfo,
    reward_cycle: u64,
}

impl LocalBurnView {
    /// The height whose reward-cycle signer channel is currently active.
    pub(crate) fn bitcoin_tip_height(&self) -> Result<u64, String> {
        self.bitcoin
            .tip_height()
            .map_err(|error| format!("this node's burnchain cannot be read: {error}"))
    }

    /// Resume the derived chain, or seed it from the checkpoint that carries one.
    ///
    /// From `checkpoint.sortition`, which is the same directory the canonical
    /// follower seeds from and the only one that holds a sortition history. This
    /// took the MARF's *parent directory* instead — `chainstate/checkpoint-H`,
    /// which holds the trie, the headers and the native effects and has never held
    /// `snapshots.json`. So the seed failed on every mainnet configuration, and
    /// with it went the leader-key registry that the same loader carries: a
    /// validator that can derive no burn view can check no tenure's VRF and no
    /// miner's signature, which is the whole reason this exists.
    pub(crate) fn open(
        config: &Config,
        standing: nano_chainstate::BitcoinBlockContext,
    ) -> Result<Self, String> {
        let capture = capture_directory(config)?;
        let state = validator_sortition_state(config);
        let mut tracker = crate::sortition::SortitionTracker::resume_or_capture_below(
            &state,
            capture,
            standing.height,
        )
        .map_err(|error| {
            format!(
                "the proposal validator cannot derive burn views locally, so it can check no \
                 tenure's VRF: {error}"
            )
        })?;
        if tracker.leader_keys() == 0 {
            return Err(format!(
                "checkpoint sortition history {} carries no authenticated leader-key registry",
                capture.display()
            ));
        }
        let mut bitcoin = crate::runtime::bitcoin_source(config)
            .map_err(|error| format!("the proposal validator has no burnchain: {error}"))?;
        recover_validator_seed(&mut tracker, |height| bitcoin.block_at(height))?;
        let canonical_view = tracker.consensus_hash_at(standing.height).ok_or_else(|| {
            format!(
                "the proposal validator's standing burn {} is absent from the local sortition \
                 history",
                standing.height
            )
        })?;
        println!(
            "the proposal validator derives burn views locally from burn {} on PoX history {}",
            tracker.tip().bitcoin_height,
            tracker.tip().pox_id
        );
        Ok(Self {
            tracker,
            bitcoin,
            state,
            canonical_view,
        })
    }

    /// Derive every proposal-validation input from this node's Bitcoin chain.
    fn context_for(
        &mut self,
        block: &nano_chainstate::NakamotoBlock,
        proposal_height: Option<u64>,
        pox: &PoxInfo,
        schedule: Option<nano_chainstate::CoinbaseSchedule>,
    ) -> Result<LocalBlockContext, String> {
        let height = requested_burn_height(
            block.bitcoin_view_consensus_hash(),
            proposal_height,
            self.canonical_view,
            |view| self.locate(view, pox),
        )?;
        let view_snapshot = self
            .tracker
            .snapshot_at(height)
            .ok_or_else(|| format!("no derived snapshot for burn {height}"))?;
        if view_snapshot.total_burn != block.header.bitcoin_spent {
            return Err(format!(
                "block {} says {} burn has been spent, while the local sortition at burn \
                 {height} derives {}",
                block.header.chain_length, block.header.bitcoin_spent, view_snapshot.total_burn
            ));
        }
        let view_local = crate::LocalSortition::from_snapshot(view_snapshot);
        let tenure_height = self
            .tracker
            .height_of_consensus_hash(block.header.consensus_hash)
            .ok_or_else(|| {
                format!(
                    "tenure {} is absent from the local sortition history",
                    block.header.consensus_hash
                )
            })?;
        let tenure_snapshot = self
            .tracker
            .snapshot_at(tenure_height)
            .ok_or_else(|| format!("no derived snapshot for tenure burn {tenure_height}"))?;
        let tenure_local = crate::LocalSortition::from_snapshot(tenure_snapshot);
        let sortition = self
            .tracker
            .sortition_info_at(tenure_height)
            .ok_or_else(|| format!("no local sortition for tenure burn {tenure_height}"))?;

        let mut bitcoin = pox.bitcoin_context();
        view_local.record(&mut bitcoin);
        tenure_local.record_authentication(&mut bitcoin);
        if tenure_height != height {
            bitcoin.move_to_burn_block(tenure_height);
            bitcoin.extend_view_to(height);
        }
        self.record_coinbase(block, schedule, tenure_height, &mut bitcoin)?;
        Ok(LocalBlockContext {
            bitcoin,
            sortition,
            reward_cycle: pox.reward_cycle(tenure_height),
        })
    }

    fn locate(
        &mut self,
        view: nano_primitives::ConsensusHash,
        pox: &PoxInfo,
    ) -> Result<u64, String> {
        let payouts = crate::payout_schedule(pox)
            .ok_or_else(|| "no payout schedule, so no sortition can be derived".to_owned())?;
        let standing_on = self
            .tracker
            .height_of_consensus_hash(self.canonical_view)
            .ok_or_else(|| {
                format!(
                    "the canonical burn view {} is absent from the local sortition history",
                    self.canonical_view
                )
            })?;
        self.tracker.keep_from(standing_on);
        let burnchain_tip = self
            .bitcoin
            .tip_height()
            .map_err(|error| format!("this node's burnchain cannot be read: {error}"))?;
        let (found, walk) = {
            let Self {
                tracker, bitcoin, ..
            } = self;
            tracker
                .locate_view(
                    view,
                    |height| bitcoin.block_at(height),
                    burnchain_tip,
                    payouts,
                    crate::sortition::CATCH_UP_LIMIT,
                )
                .map_err(|error| format!("deriving the burn view locally failed: {error}"))?
        };
        // Written down only as it advances: many proposals stand on one burn block,
        // and rewriting the whole derived history for each of them is a third of a
        // second on mainnet for a history that has not changed.
        if walk.advanced > 0
            && let Err(error) = self.tracker.save(&self.state)
        {
            eprintln!("the derived sortition chain could not be written down: {error}");
        }
        let height =
            found.ok_or_else(|| format!("burn view {view} is not on this node's burnchain yet"))?;
        Ok(height)
    }

    fn record_coinbase(
        &self,
        block: &nano_chainstate::NakamotoBlock,
        schedule: Option<nano_chainstate::CoinbaseSchedule>,
        tenure_height: u64,
        bitcoin: &mut nano_chainstate::BitcoinBlockContext,
    ) -> Result<(), String> {
        if nano_chainstate::starts_new_tenure(block) {
            let schedule = schedule.ok_or_else(|| {
                format!(
                    "no authenticated coinbase schedule is available for tenure burn {tenure_height}"
                )
            })?;
            let previous = self
                .tracker
                .previous_sortition_height(tenure_height)
                .ok_or_else(|| {
                    format!(
                        "the local sortition history cannot derive accumulated coinbase for \
                         tenure burn {tenure_height}"
                    )
                })?;
            bitcoin.accumulated_coinbase = schedule.accumulated_at(tenure_height, Some(previous));
        }
        Ok(())
    }

    pub(crate) fn prepare(
        &mut self,
        block: &nano_chainstate::NakamotoBlock,
        pox: &PoxInfo,
        validator: &mut Validator,
    ) -> Result<(u64, u64), String> {
        self.prepare_at(block, None, pox, validator)
    }

    pub(crate) fn prepare_proposal(
        &mut self,
        proposal: &BlockProposal,
        pox: &PoxInfo,
        validator: &mut Validator,
    ) -> Result<(u64, u64), String> {
        self.prepare_at(
            &proposal.block,
            Some(proposal.bitcoin_height),
            pox,
            validator,
        )
    }

    fn prepare_at(
        &mut self,
        block: &nano_chainstate::NakamotoBlock,
        proposal_height: Option<u64>,
        pox: &PoxInfo,
        validator: &mut Validator,
    ) -> Result<(u64, u64), String> {
        validator.clear_context();
        let schedule = validator.coinbase_schedule();
        let context = self.context_for(block, proposal_height, pox, schedule)?;
        if nano_chainstate::starts_new_tenure(block) {
            validator.set_accumulated_coinbase(
                context.sortition.bitcoin_height,
                context.bitcoin.accumulated_coinbase,
            );
        }
        validator
            .validator_mut()
            .set_bitcoin_context(context.bitcoin);
        let bitcoin_height = context.sortition.bitcoin_height;
        let reward_cycle = context.reward_cycle;
        validator.set_context(context.sortition, reward_cycle);
        Ok((bitcoin_height, reward_cycle))
    }

    pub(crate) fn observe(
        &mut self,
        block: &nano_chainstate::NakamotoBlock,
        pox: &PoxInfo,
        validator: &mut Validator,
    ) -> Result<(), String> {
        let (bitcoin_height, _) = self.prepare(block, pox, validator)?;
        let waterfall_payout = validator
            .validator_mut()
            .observe(block, bitcoin_height)
            .map_err(|error| {
                format!(
                    "canonical block {} at height {} failed to validate: {error}",
                    block.block_id(),
                    block.header.chain_length
                )
            })?;
        if let Some((cycle, recipient)) = waterfall_payout {
            let cycle_length = u64::from(pox.prepare_phase_length)
                .checked_add(u64::from(pox.reward_phase_length))
                .ok_or_else(|| "reward-cycle length overflow".to_owned())?;
            let offset = cycle
                .checked_mul(cycle_length)
                .ok_or_else(|| format!("reward cycle {cycle} starts past u64::MAX"))?;
            let starts_at = pox
                .first_bitcoin_height
                .checked_add(offset)
                .ok_or_else(|| format!("reward cycle {cycle} starts past u64::MAX"))?;
            self.tracker
                .record_waterfall_payout(starts_at, bitcoin_height, recipient);
            self.tracker
                .save_standing_on(&self.state, bitcoin_height)
                .map_err(|error| {
                    format!("the locally derived waterfall payout could not be saved: {error}")
                })?;
        }
        self.canonical_view = block
            .bitcoin_view_consensus_hash()
            .unwrap_or(self.canonical_view);
        Ok(())
    }
}

/// Execute one proposal and say what happened to it.
async fn judge(
    config: &Config,
    pox: &PoxInfo,
    peers: &mut TenureSource,
    burn: &mut LocalBurnView,
    validator: &mut Validator,
    request: &ProposalRequest,
) -> Result<(), (String, ProposalRejectCode)> {
    let block = &request.block;
    signer::catch_up(peers, burn, pox, validator, config.node.max_sync_blocks)
        .await
        .map_err(|error| {
            (
                format!("this node could not follow the chain the proposal builds on: {error}"),
                ProposalRejectCode::ChainstateError,
            )
        })?;
    let (bitcoin_height, cycle) = burn.prepare(block, pox, validator).map_err(|error| {
        (
            format!(
                "this node cannot derive the local validation context for proposal {}: {error}",
                block.block_id()
            ),
            ProposalRejectCode::NoSuchTenure,
        )
    })?;
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
    /// The endpoint list exactly as the caller gave it, so a round can tell whether
    /// discovery has actually moved.
    ///
    /// Compared against the *given* strings and not against `endpoints`, which hold
    /// `Url`-normalised ones: a peer's handshake advertises
    /// `http://34.150.184.50:20443` and the client built from it holds
    /// `http://34.150.184.50:20443/`, so comparing the two forms made every round
    /// look like a change. The pool was then rebuilt every poll, which reset the
    /// cursor to the front and meant the rotation this type exists for never
    /// happened -- measured on a live mainnet follower, sixteen rebuilds of the same
    /// two peers.
    requested: Vec<String>,
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
            requested: endpoints.to_vec(),
            endpoints: peers
                .iter()
                .map(|peer| peer.base_url().to_string())
                .collect(),
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
    pub fn retargeted(
        &mut self,
        serving: &mut Option<String>,
    ) -> Option<(SyncClient, StackerDbClient)> {
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
        if endpoints == self.requested.as_slice() || endpoints.is_empty() {
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
    mut written: nano_queue::Receiver<(String, Chunk)>,
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
    let mut reported = (0, 0);
    loop {
        let queue = written.status();
        state
            .metrics()
            .publish_ingress_queue(nano_rpc::IngressQueue::StackerDbChunks, queue.into());
        state
            .publish_queues(nano_rpc::QueueReport {
                queued_stackerdb_chunks: Some(queue.items),
                ..nano_rpc::QueueReport::default()
            })
            .await;
        replicas.refresh(&crate::runtime::follow_endpoints(
            &config,
            discovered.as_ref(),
        ));
        state
            .metrics()
            .publish_stackerdb_peers(replicas.endpoints.len());
        // Drained before the round rather than inside it, and kept when the round
        // fails: a chunk the hosted signer wrote is the whole reason this loop
        // exists, and dropping it because the peer whose turn it was went away
        // would lose a signature the network is counting.
        retain_written_chunks(&mut written, &mut outbound);
        match round(&mut replicas, network, &state, &outbound).await {
            Ok(()) => outbound.clear(),
            Err(refusal) => {
                state.metrics().record_stackerdb_round_unanswered();
                eprintln!("{refusal}");
            }
        }
        // What the run can be held to afterwards. `71` asks a no-hosted-API run to
        // *prove* distribution rather than assert it, and a pool that was never
        // asked twice looks identical to one peer doing all the work -- so the two
        // numbers are said out loud whenever either moves: how many distinct peers
        // have served a round, and how many rounds went unanswered.
        let distribution = replicas.distribution();
        if distribution != reported {
            reported = distribution;
            let (served, failures) = distribution;
            println!(
                "StackerDB replication has been served by {served} of {} peers, {failures} rounds \
                 unanswered",
                replicas.endpoints.len()
            );
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
    outbound: &[nano_queue::Lease<(String, Chunk)>],
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
    for pending in outbound {
        let (contract_id, chunk) = pending.value();
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

/// Retain the queue reservations while chunks wait for a peer to accept them.
fn retain_written_chunks(
    written: &mut nano_queue::Receiver<(String, Chunk)>,
    outbound: &mut Vec<nano_queue::Lease<(String, Chunk)>>,
) {
    while let Ok(pending) = written.try_recv_lease() {
        outbound.push(pending);
    }
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
        let taken = state.accept_stackerdb_chunk(&contract_id, chunk).await;
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

#[cfg(test)]
mod tests {
    use super::{
        Chunk, capture_directory, recover_validator_seed, requested_burn_height,
        retain_written_chunks, validator_sortition_state,
    };
    use crate::config::Config;

    /// A mainnet checkpoint whose sortition history is not beside its trie, which
    /// is where every real one is.
    const MAINNET: &str = r#"
        [node]
        working_dir = "/tmp/nano"
        network = "mainnet"
        peers = []
        rpc_bind = "127.0.0.1:20443"

        [burnchain]
        rpc_url = "http://127.0.0.1:8332"
        rpc_user = "local"
        rpc_password = "local"
        magic = "X2"
        pox_5_activation_height = 960230

        [checkpoint]
        bundle = "/capture/checkpoint-bundle"
        builder_policy = "/capture/checkpoint-builders.toml"
        builder_signatures = "/capture/checkpoint-signatures"
        marf = "/capture/chainstate/checkpoint-H/marf.sqlite"
        source_state_id = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
        state_root = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100"
        anchor_block = "/capture/nakamoto/anchor.bin"
        anchor_bitcoin_height = 960231
        sortition = "/capture/sortition"
    "#;

    #[test]
    fn failed_stackerdb_delivery_keeps_its_queue_bytes_reserved() {
        let limits = nano_queue::Limits::new(2, 8);
        let (sender, mut receiver) = nano_queue::channel(limits);
        sender
            .try_send(
                (
                    "ST000000000000000000002AMW42H.signers".to_owned(),
                    Chunk::new(0, 1, vec![1]),
                ),
                6,
            )
            .expect("the first chunk fits");

        let mut outbound = Vec::new();
        retain_written_chunks(&mut receiver, &mut outbound);
        assert_eq!(receiver.status().items, 1, "a retry remains accounted");
        assert_eq!(receiver.status().bytes, 6);
        assert!(
            sender
                .try_send(
                    (
                        "ST000000000000000000002AMW42H.signers".to_owned(),
                        Chunk::new(1, 1, vec![2]),
                    ),
                    3,
                )
                .is_err(),
            "a failed peer cannot move retained bytes outside the queue budget"
        );

        outbound.clear();
        assert_eq!(receiver.status().items, 0);
        assert_eq!(receiver.status().bytes, 0);
        assert_eq!(receiver.status().saturations, 1);
        assert!(
            sender
                .try_send(
                    (
                        "ST000000000000000000002AMW42H.signers".to_owned(),
                        Chunk::new(1, 1, vec![2]),
                    ),
                    3,
                )
                .is_ok(),
            "the budget recovers when delivery succeeds"
        );
    }

    /// The validator seeds from the sortition capture, not from beside the trie.
    ///
    /// It took `checkpoint.marf`'s parent directory, which on every mainnet
    /// configuration is `chainstate/checkpoint-H` — the trie and the headers, never
    /// a `snapshots.json`. So it seeded nothing, carried no leader-key registry, and
    /// could check neither a tenure's VRF proof nor a miner's signature.
    #[test]
    fn the_proposal_validator_seeds_from_the_sortition_capture() {
        let config = Config::parse(MAINNET).expect("a valid mainnet configuration");
        assert_eq!(
            capture_directory(&config),
            Ok(std::path::Path::new("/capture/sortition"))
        );
        assert_ne!(
            capture_directory(&config).ok(),
            config.checkpoint.marf.parent(),
            "the trie's directory holds no sortition history, and this is the bug"
        );
    }

    /// And a node configured without one is told, rather than seeding from a guess.
    #[test]
    fn a_checkpoint_with_no_sortition_history_derives_no_burn_view() {
        let without = MAINNET.replace(r#"sortition = "/capture/sortition""#, "");
        let config = Config::parse(&without).expect("a valid mainnet configuration");
        assert!(capture_directory(&config).is_err());
    }

    #[test]
    fn a_proposal_without_a_tenure_change_uses_its_recorded_burn_height() {
        let canonical = nano_primitives::ConsensusHash::from_bytes([3; 20]);
        assert_eq!(
            requested_burn_height(None, Some(281), canonical, |_| panic!("no lookup")),
            Ok(281)
        );
        let tenure_change = nano_primitives::ConsensusHash::from_bytes([4; 20]);
        assert_eq!(
            requested_burn_height(Some(tenure_change), Some(280), canonical, |view| {
                assert_eq!(view, tenure_change);
                Ok(280)
            }),
            Ok(280)
        );
        assert_eq!(
            requested_burn_height(None, None, canonical, |view| {
                assert_eq!(view, canonical);
                Ok(282)
            }),
            Ok(282)
        );
    }

    fn tracker(height: u64, identity: u8) -> crate::sortition::SortitionTracker {
        let mut snapshot = nano_sortition::SortitionSnapshot::genesis(
            height,
            nano_primitives::BitcoinHeaderHash::from_bytes([identity; 32]),
        );
        snapshot.consensus_hash = nano_primitives::ConsensusHash::from_bytes([identity; 20]);
        crate::sortition::SortitionTracker::new(snapshot.clone(), vec![snapshot.consensus_hash])
            .expect("the history ends at its snapshot")
    }

    fn saved_identity(directory: &std::path::Path) -> (u64, String) {
        let rows: Vec<serde_json::Value> = serde_json::from_slice(
            &std::fs::read(directory.join("snapshots.json")).expect("read saved snapshots"),
        )
        .expect("saved snapshots decode");
        let row = rows.first().expect("one saved snapshot");
        (
            row["block_height"].as_u64().expect("a burn height"),
            row["consensus_hash"]
                .as_str()
                .expect("a consensus hash")
                .to_owned(),
        )
    }

    #[test]
    fn a_stale_validator_cannot_overwrite_the_followers_sortition_state() {
        let directory = tempfile::tempdir().expect("a working directory");
        let configured = MAINNET.replace("/tmp/nano", &directory.path().display().to_string());
        let config = Config::parse(&configured).expect("a valid mainnet configuration");
        let follower_state = config.node.working_dir.clone();
        let validator_state = validator_sortition_state(&config);
        assert_eq!(
            validator_state,
            follower_state
                .join(crate::runtime::SIGNER_CHAINSTATE)
                .join("sortitions")
        );

        let follower = tracker(400, 0xaa);
        let stale_validator = tracker(300, 0xbb);
        let (follower_saved, wait_for_follower) = std::sync::mpsc::sync_channel(0);
        let follower_write = follower_state.clone();
        let validator_write = validator_state.clone();
        std::thread::scope(|scope| {
            let follower_writer = scope.spawn(move || {
                follower
                    .save(&follower_write)
                    .expect("the follower state saves");
                follower_saved.send(()).expect("signal the follower save");
            });
            let validator_writer = scope.spawn(move || {
                wait_for_follower.recv().expect("wait for the newer state");
                stale_validator
                    .save(&validator_write)
                    .expect("the stale validator state saves afterwards");
            });
            follower_writer
                .join()
                .expect("the follower writer finishes");
            validator_writer
                .join()
                .expect("the validator writer finishes");
        });

        assert_eq!(
            saved_identity(&follower_state),
            (400, "aa".repeat(20)),
            "a validator that finished later replaced the follower's newer identity"
        );
        assert_eq!(
            saved_identity(&validator_state),
            (300, "bb".repeat(20)),
            "the validator did not persist under its role-specific state"
        );
    }

    #[test]
    fn a_reopened_validator_recovers_its_saved_winner_key() {
        use nano_bitcoin::{BitcoinBlock, BitcoinOperation, BitcoinOperationKind};
        use nano_primitives::{BitcoinHeaderHash, ConsensusHash, SortitionId, sha512_256};
        use nano_sortition::{PoxId, SortitionSnapshot};

        let height = 100;
        let winner = [0x11; 32];
        let seed = [0xaa; 32];
        let public_key = [0x22; 32];
        let signing_key = [0x33; 20];
        let header = BitcoinHeaderHash::from_bytes([1; 32]);
        let mut snapshot = SortitionSnapshot::genesis(height, header);
        let mut identifier = Vec::from(header.as_bytes());
        identifier.extend_from_slice(&snapshot.pox_id.as_consensus_bytes());
        snapshot.sortition_id = SortitionId::from_bytes(*sha512_256(&identifier).as_bytes());
        snapshot.consensus_hash = ConsensusHash::from_bytes([0x7f; 20]);
        snapshot.winner_txid = Some(winner);
        snapshot.winner_vrf_seed = Some(seed);
        snapshot.winner_vrf_public_key = Some(public_key);
        snapshot.winner_signing_key_hash = Some(signing_key);
        snapshot.committed_block_hash = Some([0x44; 32]);
        snapshot.pox_id = PoxId::initial();
        let history = vec![
            ConsensusHash::from_bytes([0xbe; 20]),
            snapshot.consensus_hash,
        ];
        let mut tracker = crate::sortition::SortitionTracker::new(snapshot, history)
            .expect("the history ends at its snapshot");

        let state = tempfile::tempdir().expect("a validator state directory");
        std::fs::write(
            state.path().join(crate::sortition::LEADER_KEY_FILE),
            format!(
                r#"[{{"block_height":0,"vtxindex":0,"public_key":"{}","memo":"{}"}}]"#,
                hex::encode(public_key),
                hex::encode(signing_key)
            ),
        )
        .expect("write the carried leader key");
        tracker
            .load_leader_keys(state.path())
            .expect("load the carried leader key");
        tracker.save(state.path()).expect("save the validator view");

        let mut reopened = crate::sortition::SortitionTracker::resume_or_capture_below(
            state.path(),
            state.path(),
            height,
        )
        .expect("reopen the saved validator view");
        assert_eq!(reopened.tip().winner_vrf_public_key, None);
        let block = BitcoinBlock {
            height,
            hash: [1; 32],
            timestamp: 0,
            operations: vec![BitcoinOperation {
                txid: winner,
                transaction_index: 0,
                inputs: Vec::new(),
                outputs: Vec::new(),
                kind: BitcoinOperationKind::LeaderBlockCommit {
                    block_header_hash: [0x44; 32],
                    new_seed: seed,
                    parent_block_height: 0,
                    parent_transaction_index: 0,
                    key_block_height: 0,
                    key_transaction_index: 0,
                    memo: 0,
                    parent_modulus: u8::try_from((height + 4) % 5).expect("modulo five fits u8"),
                },
            }],
        };
        recover_validator_seed(&mut reopened, |_| Ok::<_, String>(block.clone()))
            .expect("recover the winner from Bitcoin and the carried registry");
        assert_eq!(reopened.tip().winner_vrf_public_key, Some(public_key));
    }
}
