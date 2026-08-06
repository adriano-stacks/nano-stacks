mod chain;
mod events;
mod stackerdb;

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet},
    convert::Infallible,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, Sse},
    },
    routing::{get, post},
};
use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier};
use nano_address::StacksAddress;
use nano_chainstate::NakamotoBlock;
use nano_codec::Transaction;
use nano_crypto::MessageSignature;
use nano_mempool::{Account, ChainTip, Mempool};
use nano_primitives::{BlockHeaderHash, ConsensusHash, Network, StacksBlockId, TrieHash};
use nano_sync::{FollowedTenure, NodeView, PoxInfo};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};
use tokio_stream::{StreamExt, wrappers::BroadcastStream};

pub use chain::{AccountEntry, ChainAccess, ChainAccessError, ReadOnlyCall};
pub use events::{
    BlockEventContext, DEFAULT_DISPATCH_ATTEMPTS, EventDispatcher, EventKind, MaturedReward,
    ProposalOutcome, ProposalRejectCode, RewardSetEvent, RewardSetSigner, derived_signers,
    matured_rewards, mined_nakamoto_block_payload, new_block_payload, new_burn_block_payload,
    proposal_response_payload, stackerdb_chunks_payload, stacker_set_payload,
};
pub use stackerdb::{ChunkRefusal, StackerDbStore};

/// The one boundary an unsolicited block passes before a node will hold it.
///
/// A block uploaded or proposed over HTTP arrives from anyone, so it has to
/// satisfy what a followed block satisfies: a node that admits over its own API
/// what it would refuse from a peer is forkable through its own API. This is
/// deliberately not a second validator — the node's implementation routes
/// straight to `ChainState::authenticate_block`, the boundary
/// [[050-authenticate-every-followed-nakamoto-block]] put before execution.
pub trait BlockAdmission: Send {
    /// Why this block is not one this chain would accept, if it is not.
    fn authenticate(&mut self, block: &NakamotoBlock) -> Result<(), String>;
}

/// The blocks a node kept because it executed them.
///
/// `/v3/blocks/:id` and `/v3/tenures/:id` used to be answered out of the peer
/// view bounded at the executed tip, which meant a node could not serve a block
/// it had executed unless a peer had recently told it about the same block —
/// so 36,876 blocks behind mainnet, with a perfectly good executed tip, it
/// answered `404` for that very tip. Nothing kept the blocks: staging drops each
/// one the moment it seals.
///
/// A trait rather than a type because the store is the node's, and the node
/// depends on this crate. Answers bytes rather than blocks: both routes serve the
/// consensus serialization, and decoding a block to re-encode it is work with a
/// way to be wrong in it.
pub trait ExecutedBlocks: Send + Sync {
    /// The block this identifier names.
    fn block(&self, block_id: StacksBlockId) -> Option<Vec<u8>>;

    /// The blocks of the tenure that starts at this block, lowest first, and
    /// stopping before `stop` when it is named.
    fn tenure(&self, start_block_id: StacksBlockId, stop: Option<StacksBlockId>) -> Vec<Vec<u8>>;
}

/// A block waiting to be vouched for, and where the verdict goes.
///
/// Authentication does not look at a state root, so a node that answered `Ok` on
/// it alone would be telling a signer to sign whatever the proposer computed. The
/// only truthful answer comes from running the block, and running it needs a chain
/// state that is allowed to hold a candidate — which the node's executor is not.
///
/// So the route asks rather than decides: it hands the block to whichever part of
/// the node keeps such a state and waits for the answer, exactly as an uploaded
/// block is handed to the executor. The refusal carries the code as well as the
/// reason, because only the validator knows whether the block was wrong or this
/// node was not ready to say.
pub struct ProposalRequest {
    pub block: NakamotoBlock,
    pub verdict: tokio::sync::oneshot::Sender<Result<(), (String, ProposalRejectCode)>>,
}

/// One coherent view of what this node executed.
///
/// Published as a whole so that two routes cannot answer from two different
/// rounds: the tip, the chain that leads to it, and the cycle constants are read
/// together or not at all.
#[derive(Clone, Debug)]
struct Executed {
    tip: SealedTip,
    /// The followed tenures, bounded at the tip: what this node has executed,
    /// and nothing above it.
    chain: Vec<FollowedTenure>,
    /// The `PoX` constants, which are configuration rather than chain state, so
    /// they survive a tip the peer's view no longer reaches.
    pox: Option<PoxInfo>,
}

/// The validated node state exposed by the public HTTP API.
#[derive(Clone)]
pub struct RpcState {
    /// What the peer said, kept so that catching up is measurable and read by
    /// nothing else. Serving the peer's height as this node's own is how a node
    /// that had executed nothing at all reported itself within three blocks of
    /// mainnet for eighty minutes.
    followed: Arc<RwLock<Option<NodeView>>>,
    /// How far ahead the peer said it is.
    ///
    /// Kept apart from `followed` because a node that is far behind never walks
    /// the peer's tenure — that walk fails every round from there — so it has a
    /// height and no view, and the height is the whole of what "how far behind
    /// am I" needs.
    followed_height: Arc<RwLock<Option<u64>>>,
    /// The tip this node's own fork choice picked out of what its peers offered.
    ///
    /// The third of the three heights, and the one nothing else can be read off:
    /// peers *advertise* tips, this node *selects* one by signer weight and burn
    /// view, and it *executes* up to some height at or below it. A selection that
    /// is not the highest thing advertised is a tip this node refused, and
    /// reporting only the ends makes that invisible.
    selected: Arc<RwLock<Option<SelectedTip>>>,
    /// What this node executed and sealed, which every Stacks-compatible route
    /// answers from.
    executed: Arc<RwLock<Option<Executed>>>,
    events: broadcast::Sender<NodeEvent>,
    /// The executed Clarity state, when the node runs one.
    chain: Option<Arc<Mutex<dyn ChainAccess>>>,
    /// The blocks this node kept because it executed them, which is what
    /// `/v3/blocks/:id` and `/v3/tenures/:id` answer from when it has them.
    archive: Option<Arc<dyn ExecutedBlocks>>,
    /// The validator an uploaded block or a proposal has to pass.
    admission: Option<Arc<Mutex<dyn BlockAdmission>>>,
    /// Where a proposal goes to be executed and answered for.
    proposals: Option<mpsc::UnboundedSender<ProposalRequest>>,
    /// Where a chunk this node took is passed on, so a signer hosted here reaches
    /// the miner it is answering.
    chunks: Option<mpsc::UnboundedSender<(String, nano_stackerdb::Chunk)>>,
    /// Where a transaction this node admitted is passed on to the network.
    ///
    /// A node that keeps what it accepted to itself is a black hole with a `200`:
    /// it looks like acceptance and behaves like a drop, since only a miner that
    /// sees the transaction can ever mine it.
    submitted: Option<mpsc::UnboundedSender<Transaction>>,
    mempool: Option<Arc<Mutex<Mempool>>>,
    /// The chain this node is on, which no peer needs to be asked about.
    network: Network,
    /// The reward sets this node derived, keyed by cycle.
    stacker_sets: Arc<RwLock<BTreeMap<u64, Value>>>,
    /// The `StackerDB` contracts this node replicates.
    stackerdb: Arc<RwLock<StackerDbStore>>,
    /// Where an accepted block upload or proposal is handed to the node.
    blocks: Option<mpsc::UnboundedSender<NakamotoBlock>>,
    /// The `authorization` header `/v3/block_proposal` demands.
    proposal_token: Option<String>,
    /// Where the events a route produces are published.
    observers: Option<EventDispatcher>,
}

impl std::fmt::Debug for RpcState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RpcState")
            .field("chain", &self.chain.is_some())
            .field("mempool", &self.mempool.is_some())
            .finish_non_exhaustive()
    }
}

/// A validated block that became visible through the public API.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NodeEvent {
    pub block_id: String,
    pub stacks_height: u64,
    pub bitcoin_height: u64,
}

impl RpcState {
    /// Construct initially unavailable public state for a named chain.
    ///
    /// The network is an argument rather than a default because every default is
    /// wrong somewhere: it decides the boot principals `/v2/pox` and
    /// `/v2/contracts/call-read` name and the chain identifier a proposal is
    /// checked against, and a node that quietly served mainnet's answers for a
    /// hacknet chain would be believed.
    #[must_use]
    pub fn new(network: Network) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            followed: Arc::new(RwLock::new(None)),
            followed_height: Arc::new(RwLock::new(None)),
            selected: Arc::new(RwLock::new(None)),
            executed: Arc::new(RwLock::new(None)),
            events,
            chain: None,
            archive: None,
            admission: None,
            proposals: None,
            chunks: None,
            submitted: None,
            mempool: None,
            network,
            stacker_sets: Arc::new(RwLock::new(BTreeMap::new())),
            stackerdb: Arc::new(RwLock::new(StackerDbStore::new())),
            blocks: None,
            proposal_token: None,
            observers: None,
        }
    }

    /// The `StackerDB` replicas this node serves, so the node can configure
    /// their writers and read what a signer wrote.
    #[must_use]
    pub fn stackerdb(&self) -> Arc<RwLock<StackerDbStore>> {
        self.stackerdb.clone()
    }

    /// Serve accounts and read-only calls from this executed Clarity state.
    #[must_use]
    pub fn with_chain(mut self, chain: Arc<Mutex<dyn ChainAccess>>) -> Self {
        self.chain = Some(chain);
        self
    }

    /// Serve executed blocks and tenures out of this store.
    #[must_use]
    pub fn with_executed_blocks(mut self, archive: Arc<dyn ExecutedBlocks>) -> Self {
        self.archive = Some(archive);
        self
    }

    /// Admit uploaded blocks and proposals only through this validator.
    #[must_use]
    pub fn with_block_admission(mut self, admission: Arc<Mutex<dyn BlockAdmission>>) -> Self {
        self.admission = Some(admission);
        self
    }

    /// Vouch for proposals only after this validator has executed them.
    #[must_use]
    pub fn with_proposal_validator(
        mut self,
        proposals: mpsc::UnboundedSender<ProposalRequest>,
    ) -> Self {
        self.proposals = Some(proposals);
        self
    }

    /// Pass the transactions this node admits on to the network over this channel.
    #[must_use]
    pub fn with_transaction_relay(mut self, submitted: mpsc::UnboundedSender<Transaction>) -> Self {
        self.submitted = Some(submitted);
        self
    }

    /// Pass the chunks this node takes on to the network over this channel.
    #[must_use]
    pub fn with_chunk_relay(
        mut self,
        chunks: mpsc::UnboundedSender<(String, nano_stackerdb::Chunk)>,
    ) -> Self {
        self.chunks = Some(chunks);
        self
    }

    /// Publish the events a route produces to these observers.
    #[must_use]
    pub fn with_observers(mut self, observers: EventDispatcher) -> Self {
        self.observers = Some(observers);
        self
    }

    /// Admit submitted transactions into this mempool.
    #[must_use]
    pub fn with_mempool(mut self, mempool: Arc<Mutex<Mempool>>) -> Self {
        self.mempool = Some(mempool);
        self
    }

    /// Hand blocks accepted by upload or proposal to the node over this channel.
    #[must_use]
    pub fn with_block_sink(mut self, blocks: mpsc::UnboundedSender<NakamotoBlock>) -> Self {
        self.blocks = Some(blocks);
        self
    }

    /// Require this `authorization` header on `/v3/block_proposal`.
    #[must_use]
    pub fn with_proposal_token(mut self, token: String) -> Self {
        self.proposal_token = Some(token);
        self
    }

    /// Publish the reward set a cycle resolved to, as `/v3/stacker_set` reports it.
    pub async fn publish_stacker_set(&self, cycle: u64, stacker_set: Value) {
        self.stacker_sets.write().await.insert(cycle, stacker_set);
    }

    /// Publish the tip this node has executed and sealed, and the chain that
    /// leads to it.
    ///
    /// The snapshot is built here, from the latest followed view bounded at this
    /// tip, and written once — so a caller reading a block and a caller reading
    /// the tip are told about the same state.
    pub async fn publish_executed(&self, tip: SealedTip) {
        let followed = self.followed.read().await.clone();
        let pox = followed.as_ref().map(|view| view.pox_info.clone());
        let chain = followed.map_or_else(Vec::new, |view| executed_chain(view.tenures, &tip));
        *self.executed.write().await = Some(Executed { tip, chain, pox });
    }

    /// Say how far ahead the peer is, which is all a node this far behind knows.
    ///
    /// Catching up, the follower asks the peer only for its height: the tenure
    /// walk a full view needs fails every round from thousands of blocks back.
    /// Without this, `/nano/sync_status` answered `blocks_behind: null` for
    /// exactly the node the number exists for.
    pub async fn publish_followed_height(&self, height: u64) {
        *self.followed_height.write().await = Some(height);
    }

    /// Say which tip this node's fork choice picked, and off whom.
    ///
    /// Published by the choice itself rather than by whoever acts on it: the
    /// choice is remade on a timer whether or not the answer changes, and a
    /// node that reported it only when it changed peers would stop reporting it
    /// exactly when it settled.
    pub async fn publish_selected(&self, selected: SelectedTip) {
        *self.selected.write().await = Some(selected);
    }

    /// Publish a fully validated snapshot and notify subscribers about a new tip.
    pub async fn publish(&self, view: NodeView) {
        *self.followed_height.write().await = Some(view.node_info.stacks_height);
        let event = NodeEvent::from_view(&view);
        let changed = self
            .followed
            .read()
            .await
            .as_ref()
            .and_then(NodeEvent::from_view)
            != event;
        *self.followed.write().await = Some(view);
        if changed && let Some(event) = event {
            let _ = self.events.send(event);
        }
    }
}

/// The part of a followed view this node has actually executed.
///
/// The peer is ahead by construction, so its view names blocks this node has not
/// executed and may never execute; serving them is exactly the confusion
/// [[046-distinguish-followed-and-executed-chain-tips]] was about. So the chain
/// is walked back from the executed tip through parent links, and nothing off
/// that walk survives — a tip the view does not reach at all leaves nothing,
/// which is the honest answer for a node still catching up.
fn executed_chain(tenures: Vec<FollowedTenure>, tip: &SealedTip) -> Vec<FollowedTenure> {
    let parents: HashMap<StacksBlockId, StacksBlockId> = tenures
        .iter()
        .flat_map(|tenure| &tenure.blocks)
        .map(|block| (block.block_id(), block.header.parent_block_id))
        .collect();
    let mut executed = HashSet::new();
    let mut walk = Some(tip.stacks_tip);
    while let Some(block) = walk.filter(|block| !executed.contains(block)) {
        executed.insert(block);
        walk = parents.get(&block).copied();
    }
    tenures
        .into_iter()
        .filter_map(|mut tenure| {
            tenure
                .blocks
                .retain(|block| executed.contains(&block.block_id()));
            let last = tenure.blocks.last()?;
            // The tenure's own tip moves down with it: a tenure whose newest
            // blocks were dropped must not keep advertising them.
            tenure.info.tip_block_id = last.block_id();
            tenure.info.tip_height = last.header.chain_length;
            Some(tenure)
        })
        .collect()
}

impl NodeEvent {
    fn from_view(view: &NodeView) -> Option<Self> {
        let tenure = view.tenures.last()?;
        Some(Self {
            block_id: tenure.info.tip_block_id.to_string(),
            stacks_height: tenure.info.tip_height,
            bitcoin_height: tenure.sortition.bitcoin_height,
        })
    }
}

/// Build the RPC routes backed by the node's latest validated view.
pub fn router(state: RpcState) -> Router {
    Router::new()
        .route("/v2/info", get(node_info))
        .route("/nano/sync_status", get(sync_status))
        .route("/v2/pox", get(pox_info))
        .route("/v2/accounts/{principal}", get(account))
        .route(
            "/v2/contracts/call-read/{address}/{contract}/{function}",
            post(call_read_only),
        )
        .route("/v2/transactions", post(submit_transaction))
        .route(
            "/v2/stackerdb/{address}/{contract}",
            get(stackerdb_metadata),
        )
        .route(
            "/v2/stackerdb/{address}/{contract}/chunks",
            post(stackerdb_chunk_upload),
        )
        .route(
            "/v2/stackerdb/{address}/{contract}/{slot_id}",
            get(stackerdb_chunk),
        )
        .route(
            "/v2/stackerdb/{address}/{contract}/{slot_id}/{slot_version}",
            get(stackerdb_chunk_at_version),
        )
        .route("/v3/sortitions", get(latest_sortition))
        .route("/v3/sortitions/latest_and_last", get(latest_and_last_sortition))
        .route("/v3/sortitions/consensus/{consensus_hash}", get(sortition))
        .route("/v3/stacker_set/{cycle}", get(stacker_set))
        .route("/v3/tenures/info", get(tenure_info))
        .route("/v3/tenures/fork_info/{start}/{stop}", get(tenure_fork_info))
        .route(
            "/v3/tenures/tip_metadata/{consensus_hash}",
            get(tenure_tip_metadata),
        )
        .route("/v3/tenures/{start_block_id}", get(tenure))
        .route("/v3/blocks/upload", post(upload_block))
        .route("/v3/blocks/{block_id}", get(block))
        .route("/v3/block_proposal", post(block_proposal))
        .route("/events", get(events))
        .layer(axum::middleware::from_fn(trace))
        .with_state(state)
}

/// Serve the public RPC until the listener is stopped.
pub async fn serve(listener: tokio::net::TcpListener, state: RpcState) -> std::io::Result<()> {
    axum::serve(listener, router(state)).await
}

/// Whether every request is to name itself.
///
/// Off by default, because a node at tip answers a few requests a second and the
/// operator is reading for the one line that matters. On, it is the only record of
/// *which* routes a client actually used — the question "does a stock signer run
/// against nano" is answered by that list, and a signer's own log does not keep it.
static TRACE_REQUESTS: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| std::env::var_os("NANO_TRACE_RPC").is_some());

/// Say what was asked for and what was answered.
async fn trace(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    if !*TRACE_REQUESTS {
        return next.run(request).await;
    }
    let (method, path) = (
        request.method().clone(),
        request.uri().path().to_owned(),
    );
    let response = next.run(request).await;
    println!("rpc {method} {path} -> {}", response.status().as_u16());
    response
}

#[derive(Debug)]
enum RpcError {
    Unavailable,
    NotFound,
    BadRequest(String),
    /// A refusal that carries stacks-core's own JSON body.
    Rejected(Value),
    Unauthorized,
}

impl IntoResponse for RpcError {
    fn into_response(self) -> Response {
        match self {
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE.into_response(),
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            Self::Unauthorized => StatusCode::UNAUTHORIZED.into_response(),
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message).into_response(),
            Self::Rejected(body) => (StatusCode::BAD_REQUEST, axum::Json(body)).into_response(),
        }
    }
}

impl From<ChainAccessError> for RpcError {
    fn from(error: ChainAccessError) -> Self {
        match error {
            ChainAccessError::Unavailable(message) | ChainAccessError::Failed(message) => {
                Self::BadRequest(message)
            }
            ChainAccessError::NotReadOnly => Self::BadRequest("NotReadOnly".to_owned()),
        }
    }
}

impl RpcState {
    fn chain(&self) -> Result<Arc<Mutex<dyn ChainAccess>>, RpcError> {
        self.chain.clone().ok_or(RpcError::Unavailable)
    }

    /// Whether this node has already executed this block.
    async fn holds_block(&self, block: &NakamotoBlock) -> bool {
        self.executed.read().await.as_ref().is_some_and(|executed| {
            executed
                .chain
                .iter()
                .flat_map(|tenure| &tenure.blocks)
                .any(|known| known.block_id() == block.block_id())
        })
    }

    /// Whether this node stands on the block this one names as its parent.
    ///
    /// A block whose parent this node has not executed cannot be validated
    /// against anything: its state root is over a state this node does not hold.
    async fn holds_parent_of(&self, block: &NakamotoBlock) -> bool {
        let parent = block.header.parent_block_id;
        self.executed.read().await.as_ref().is_some_and(|executed| {
            executed.tip.stacks_tip == parent
                || executed
                    .chain
                    .iter()
                    .flat_map(|tenure| &tenure.blocks)
                    .any(|known| known.block_id() == parent)
        })
    }

    /// Put a block through the validator a followed block passes.
    ///
    /// Routed to, never reimplemented: the whole point is that the RPC cannot
    /// admit something the follow path would refuse.
    async fn authenticate(&self, block: &NakamotoBlock) -> Result<(), String> {
        match self.admission.as_ref() {
            Some(admission) => admission.lock().await.authenticate(block),
            // A node with no chain to authenticate against holds no blocks
            // either, so there is nothing for it to be talked onto.
            None => Err("this node runs no chain to authenticate a block against".to_owned()),
        }
    }

    fn offer_block(&self, block: NakamotoBlock) -> Result<(), RpcError> {
        self.blocks
            .as_ref()
            .ok_or(RpcError::Unavailable)?
            .send(block)
            .map_err(|_| RpcError::Unavailable)
    }

    fn dispatch(&self, kind: EventKind, payload: &Value) {
        if let Some(observers) = self.observers.as_ref() {
            observers.dispatch(kind, payload);
        }
    }
}

/// The one executed snapshot a request answers from.
async fn executed(state: &RpcState) -> Result<Executed, RpcError> {
    state
        .executed
        .read()
        .await
        .clone()
        .ok_or(RpcError::Unavailable)
}

/// The Stacks-compatible fields describe the chain this node executed, never
/// the chain its peer advertised: a caller reading an account and a caller
/// reading the tip have to be told about the same state.
async fn node_info(State(state): State<RpcState>) -> Result<axum::Json<NodeInfoWire>, RpcError> {
    // The chain identifier is this node's own, not something a peer tells it.
    // Reading it from a peer view made `/v2/info` unavailable whenever no peer
    // had been heard from, even with a perfectly good executed tip to report —
    // which is the opposite of what this route is for.
    let network_id = state.network.chain_id();
    let tip = executed(&state).await?.tip;
    Ok(axum::Json(NodeInfoWire {
        burn_block_height: tip.bitcoin_height,
        stacks_tip_height: tip.stacks_height,
        stacks_tip: tip.stacks_block_hash.to_string(),
        stacks_tip_consensus_hash: tip.consensus_hash.to_string(),
        // The burn view this node has *executed* under, which is the newest
        // sortition it derived and checked. A node reporting the burnchain tip it
        // had not yet elected a tenure from would be reporting its peer's view.
        pox_consensus: tip.consensus_hash.to_string(),
        server_version: format!("nano-stacks {}", env!("CARGO_PKG_VERSION")),
        network_id,
    }))
}

/// What this node has followed and what it has executed, as two separate facts.
///
/// Nothing in the Stacks API says how far behind its own peer a node is, so a
/// node that cannot execute looks identical to one at tip. This route is
/// nano's own, and exists so that catching up is measurable.
async fn sync_status(State(state): State<RpcState>) -> Result<axum::Json<SyncStatusWire>, RpcError> {
    let followed = *state.followed_height.read().await;
    let selected = state.selected.read().await.clone();
    let tip = state
        .executed
        .read()
        .await
        .as_ref()
        .map(|executed| executed.tip.clone());
    Ok(axum::Json(SyncStatusWire {
        followed_stacks_height: followed,
        selected_stacks_height: selected.as_ref().map(|selected| selected.stacks_height),
        selected_stacks_tip: selected
            .as_ref()
            .map(|selected| selected.stacks_tip.to_string()),
        selected_from_peer: selected.map(|selected| selected.peer),
        executed_stacks_height: tip.as_ref().map(|tip| tip.stacks_height),
        executed_stacks_tip: tip.as_ref().map(|tip| tip.stacks_tip.to_string()),
        executed_state_index_root: tip.as_ref().map(|tip| tip.state_index_root.to_string()),
        blocks_behind: followed
            .zip(tip.as_ref().map(|tip| tip.stacks_height))
            .map(|(followed, executed)| followed.saturating_sub(executed)),
    }))
}

/// The cycle constants are this node's own, and the height is the one it
/// executed: a caller told a burn height it can then ask no account about is
/// being told about the peer.
///
/// The shape is `RPCPoxInfoData`'s, in full, because a stock signer reads this
/// route into that type and serde refuses a document missing a field — so
/// answering with a useful subset is answering with nothing. Where nano has the
/// value it gives it; where it does not it gives the type's zero and says so
/// here, which is the same rule the `new_block` payload follows.
///
/// What nano genuinely does not know:
///
/// - `pox_activation_threshold_ustx`, `total_liquid_supply_ustx` and the cycles'
///   `stacked_ustx`: read from `.pox-5`'s `get-pox-info`, which nothing in nano
///   calls, and no consumer of this route needs.
/// - the epochs before 4.0. nano is a 4.0-only node started from a checkpoint at
///   or after the boundary, so the earlier epochs are not its history to report;
///   `epochs` carries the one it executes under and `current_epoch` names it,
///   which is the field a signer actually reads.
/// - the sBTC contracts, unless the operator configured them.
async fn pox_info(State(state): State<RpcState>) -> Result<axum::Json<Value>, RpcError> {
    let executed = executed(&state).await?;
    let network = state.network;
    let pox = executed.pox.ok_or(RpcError::Unavailable)?;
    let height = executed.tip.bitcoin_height;
    let cycle_length = u64::from(pox.reward_phase_length + pox.prepare_phase_length);
    let cycle = height.saturating_sub(pox.first_bitcoin_height) / cycle_length.max(1);
    let next_start = pox.first_bitcoin_height + (cycle + 1) * cycle_length;
    let prepare_start = next_start.saturating_sub(u64::from(pox.prepare_phase_length));
    // The reward-slot threshold nano derived for the cycle, from its own pox-5
    // state. Absent means nano has not resolved that cycle, which is also the
    // honest answer to whether PoX is active as far as this node is concerned.
    let (this_threshold, next_threshold, resolved) = {
        let sets = state.stacker_sets.read().await;
        let threshold = |cycle: u64| -> u64 {
            sets.get(&cycle)
                .and_then(|set| set.get("pox_ustx_threshold"))
                .and_then(Value::as_u64)
                .unwrap_or_default()
        };
        (
            threshold(cycle),
            threshold(cycle + 1),
            sets.contains_key(&cycle),
        )
    };
    let activation = u64::from(pox.pox_5_activation_height.unwrap_or_default());
    Ok(axum::Json(json!({
        "contract_id": network.boot_contract_id("pox-5"),
        "pox_activation_threshold_ustx": 0,
        "first_burnchain_block_height": pox.first_bitcoin_height,
        "current_burnchain_block_height": height,
        "prepare_phase_block_length": pox.prepare_phase_length,
        "reward_phase_block_length": pox.reward_phase_length,
        "reward_slots": pox.reward_slots,
        "rejection_fraction": pox.rejection_fraction,
        "total_liquid_supply_ustx": 0,
        "current_cycle": {
            "id": cycle,
            "min_threshold_ustx": this_threshold,
            "stacked_ustx": 0,
            "is_pox_active": resolved,
        },
        "next_cycle": {
            "id": cycle + 1,
            "min_threshold_ustx": next_threshold,
            "min_increment_ustx": 0,
            "stacked_ustx": 0,
            "prepare_phase_start_block_height": prepare_start,
            "blocks_until_prepare_phase": prepare_start.cast_signed() - height.cast_signed(),
            "reward_phase_start_block_height": next_start,
            "blocks_until_reward_phase": next_start.saturating_sub(height),
            "ustx_until_pox_rejection": Value::Null,
        },
        "epochs": [{
            "epoch_id": "Epoch40",
            "start_height": activation,
            "end_height": i64::MAX,
            "block_limit": {
                "write_length": 15_000_000,
                "write_count": 15_000,
                "read_length": 200_000_000,
                "read_count": 30_000,
                "runtime": 5_000_000_000u64,
            },
            "network_epoch": 16,
        }],
        "current_epoch": "Epoch40",
        "min_amount_ustx": this_threshold,
        "prepare_cycle_length": pox.prepare_phase_length,
        "reward_cycle_id": cycle,
        "reward_cycle_length": cycle_length,
        "rejection_votes_left_required": Value::Null,
        "next_reward_cycle_in": next_start.saturating_sub(height),
        "contract_versions": [{
            "contract_id": network.boot_contract_id("pox-5"),
            "activation_burnchain_block_height": activation,
            "first_reward_cycle_id": activation.saturating_sub(pox.first_bitcoin_height)
                / cycle_length.max(1),
        }],
        "pox_5_sbtc_contract": "",
        "pox_5_sbtc_registry_contract": "",
    })))
}

async fn tenure_info(
    State(state): State<RpcState>,
) -> Result<axum::Json<TenureInfoWire>, RpcError> {
    let latest = executed(&state)
        .await?
        .chain
        .last()
        .ok_or(RpcError::Unavailable)?
        .info
        .clone();
    Ok(axum::Json(TenureInfoWire::from(latest)))
}

async fn sortition(
    State(state): State<RpcState>,
    Path(consensus_hash): Path<String>,
) -> Result<axum::Json<Vec<SortitionInfoWire>>, RpcError> {
    let sortition = executed(&state)
        .await?
        .chain
        .into_iter()
        .map(|tenure| tenure.sortition)
        .find(|sortition| sortition.consensus_hash.to_string() == consensus_hash)
        .ok_or(RpcError::NotFound)?;
    Ok(axum::Json(vec![SortitionInfoWire::from(sortition)]))
}

/// The sortition this node is standing on, which is the one that elected its tip.
///
/// stacks-core answers this from its burnchain tip. nano answers from the tenure
/// its executed tip belongs to, because a burn block it has not executed a tenure
/// for is one it cannot describe: it would have to name a winner it never checked.
async fn latest_sortition(
    State(state): State<RpcState>,
) -> Result<axum::Json<Vec<SortitionInfoWire>>, RpcError> {
    let latest = executed(&state)
        .await?
        .chain
        .last()
        .ok_or(RpcError::Unavailable)?
        .sortition
        .clone();
    Ok(axum::Json(vec![SortitionInfoWire::from(latest)]))
}

/// The current sortition and the one before it that also had a winner.
///
/// A signer reads its whole view of who may mine from this one route, and refuses
/// to build one at all if the second entry is missing while the first names a
/// `last_sortition_ch` — so the pair is served together or not at all.
async fn latest_and_last_sortition(
    State(state): State<RpcState>,
) -> Result<axum::Json<Vec<SortitionInfoWire>>, RpcError> {
    let chain = executed(&state).await?.chain;
    let latest = chain.last().ok_or(RpcError::Unavailable)?.sortition.clone();
    let mut sortitions = vec![latest.clone()];
    if let Some(previous) = latest.last_sortition_consensus_hash {
        let last = chain
            .iter()
            .map(|tenure| &tenure.sortition)
            .rfind(|sortition| sortition.consensus_hash == previous)
            .ok_or(RpcError::NotFound)?;
        sortitions.push(last.clone());
    }
    Ok(axum::Json(
        sortitions.into_iter().map(SortitionInfoWire::from).collect(),
    ))
}

/// The last block of a tenure and the burn view it was built under.
///
/// A stock signer asks this on every tenure it evaluates, and it is the one route
/// here that serves a whole block header as JSON — `anchored_header` is
/// stacks-core's `StacksBlockHeaderTypes`, externally tagged, with every hash
/// written as bare hex because that is what its own reader accepts.
async fn tenure_tip_metadata(
    State(state): State<RpcState>,
    Path(consensus_hash): Path<String>,
) -> Result<axum::Json<Value>, RpcError> {
    let tenure = executed(&state)
        .await?
        .chain
        .into_iter()
        .rfind(|tenure| tenure.sortition.consensus_hash.to_string() == consensus_hash)
        .ok_or(RpcError::NotFound)?;
    let header = &tenure.blocks.last().ok_or(RpcError::NotFound)?.header;
    Ok(axum::Json(json!({
        "anchored_header": { "Nakamoto": {
            "version": header.version,
            "chain_length": header.chain_length,
            "burn_spent": header.bitcoin_spent,
            "consensus_hash": header.consensus_hash.to_string(),
            "parent_block_id": header.parent_block_id.to_string(),
            "tx_merkle_root": header.transaction_merkle_root.to_string(),
            "state_index_root": header.state_index_root.to_string(),
            "timestamp": header.timestamp,
            "miner_signature": hex::encode(header.miner_signature.as_bytes()),
            "signer_signature": header
                .signer_signatures
                .iter()
                .map(|signature| hex::encode(signature.as_bytes()))
                .collect::<Vec<_>>(),
            "pox_treatment": hex::encode(header.pox_treatment.wire_bytes()),
            "problematic_txs": header
                .problematic_transactions
                .iter()
                .map(|marker| json!({ "tx_index": marker.index, "category": marker.category }))
                .collect::<Vec<_>>(),
        }},
        // The tenure's burn view is the consensus hash that elected it: nano keeps
        // no separate per-block burn view, and for a tenure's own last block the
        // two are the same thing.
        "burn_view": header.consensus_hash.to_string(),
    })))
}

/// How many tenures a fork check walks back, as stacks-core's own limit.
const FORK_INFO_DEPTH: usize = 10;

/// The tenures between two sortitions, newest first, as a signer's fork check
/// asks for them: from `stop` back to the height of `start`.
async fn tenure_fork_info(
    State(state): State<RpcState>,
    Path((start, stop)): Path<(String, String)>,
) -> Result<axum::Json<Vec<TenureForkInfoWire>>, RpcError> {
    let chain = executed(&state).await?.chain;
    let position = |consensus_hash: &str| {
        chain
            .iter()
            .rposition(|tenure| tenure.sortition.consensus_hash.to_string() == consensus_hash)
    };
    let first = position(&start).ok_or(RpcError::NotFound)?;
    let last = position(&stop).ok_or(RpcError::NotFound)?;
    if last < first {
        return Err(RpcError::BadRequest(
            "the stop sortition is older than the start sortition".to_owned(),
        ));
    }
    Ok(axum::Json(
        chain[first..=last]
            .iter()
            .rev()
            .take(FORK_INFO_DEPTH)
            .map(TenureForkInfoWire::from)
            .collect(),
    ))
}

async fn stacker_set(
    State(state): State<RpcState>,
    Path(cycle): Path<u64>,
) -> Result<axum::Json<Value>, RpcError> {
    let stacker_set = state
        .stacker_sets
        .read()
        .await
        .get(&cycle)
        .cloned()
        .ok_or_else(|| {
            RpcError::Rejected(json!({
                "response": "error",
                "err_type": "not_available_try_again",
                "err_msg": format!("reward cycle {cycle} has no reward set yet"),
            }))
        })?;
    Ok(axum::Json(json!({ "stacker_set": stacker_set })))
}

async fn account(
    State(state): State<RpcState>,
    Path(principal): Path<String>,
) -> Result<axum::Json<AccountWire>, RpcError> {
    let principal = PrincipalData::parse(&principal)
        .map_err(|error| RpcError::BadRequest(format!("invalid principal: {error}")))?;
    let account = state.chain()?.lock().await.account(&principal)?;
    Ok(axum::Json(AccountWire {
        balance: format!("0x{:032x}", account.balance),
        locked: format!("0x{:032x}", account.locked),
        unlock_height: account.unlock_height,
        nonce: account.nonce,
    }))
}

#[derive(Deserialize)]
struct CallReadOnlyBody {
    sender: String,
    sponsor: Option<String>,
    arguments: Vec<String>,
}

async fn call_read_only(
    State(state): State<RpcState>,
    Path((address, contract, function)): Path<(String, String, String)>,
    axum::Json(body): axum::Json<CallReadOnlyBody>,
) -> Result<axum::Json<CallReadOnlyWire>, RpcError> {
    let principal = |value: &str| {
        PrincipalData::parse(value)
            .map_err(|error| RpcError::BadRequest(format!("invalid principal: {error}")))
    };
    let contract = QualifiedContractIdentifier::parse(&format!("{address}.{contract}"))
        .map_err(|error| RpcError::BadRequest(format!("invalid contract: {error}")))?;
    let arguments = body
        .arguments
        .iter()
        .map(|argument| {
            hex::decode(argument.trim_start_matches("0x"))
                .map_err(|error| RpcError::BadRequest(format!("invalid argument: {error}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let call = ReadOnlyCall {
        sender: principal(&body.sender)?,
        sponsor: body.sponsor.as_deref().map(principal).transpose()?,
        contract,
        function,
        arguments,
    };
    let chain = state.chain()?;
    let outcome = chain.lock().await.call_read_only(&call);
    // A call that ran and failed is a successful request reporting its cause,
    // which is what a client distinguishes an outage from.
    Ok(axum::Json(match outcome {
        Ok(value) => value.serialize_to_hex().map_or_else(
            |error| CallReadOnlyWire::failed(error.to_string()),
            |hex| CallReadOnlyWire {
                okay: true,
                result: Some(format!("0x{hex}")),
                cause: None,
            },
        ),
        Err(ChainAccessError::Unavailable(error)) => {
            return Err(RpcError::BadRequest(error));
        }
        Err(ChainAccessError::NotReadOnly) => CallReadOnlyWire::failed("NotReadOnly".to_owned()),
        Err(ChainAccessError::Failed(cause)) => CallReadOnlyWire::failed(cause),
    }))
}

async fn submit_transaction(
    State(state): State<RpcState>,
    body: Bytes,
) -> Result<axum::Json<String>, RpcError> {
    let (transaction, _) = Transaction::decode(&body)
        .map_err(|error| RpcError::BadRequest(format!("failed to decode transaction: {error}")))?;
    let txid = transaction.txid();
    let mempool = state.mempool.clone().ok_or(RpcError::Unavailable)?;
    let chain = state.chain()?;
    let mut mempool = mempool.lock().await;
    let mut chain = chain.lock().await;
    let admission = mempool.submit(
        transaction.clone(),
        &ExecutedTip::new(&mut *chain),
        now_seconds(),
    );
    drop(chain);
    drop(mempool);
    admission.map_err(|rejection| RpcError::Rejected(rejection.into_json(txid)))?;
    // Admitted here is admitted for the whole network: the pool this node keeps
    // is only read by its own miner, and a transaction nobody else hears about
    // cannot be mined by anybody else.
    if let Some(submitted) = &state.submitted {
        let _ = submitted.send(transaction);
    }
    Ok(axum::Json(txid.to_string()))
}

/// The executed state, read one account at a time as the pool asks for them.
///
/// Admission consults the origin, the payer and — for a transfer — the
/// recipient, and which of those it needs is the pool's business, so the tip
/// answers lazily instead of guessing the set up front.
struct ExecutedTip<'a> {
    chain: RefCell<&'a mut dyn ChainAccess>,
    accounts: RefCell<HashMap<StacksAddress, Account>>,
}

impl<'a> ExecutedTip<'a> {
    fn new(chain: &'a mut dyn ChainAccess) -> Self {
        Self {
            chain: RefCell::new(chain),
            accounts: RefCell::new(HashMap::new()),
        }
    }
}

impl ChainTip for ExecutedTip<'_> {
    fn account(&self, address: &StacksAddress) -> Account {
        if let Some(account) = self.accounts.borrow().get(address) {
            return *account;
        }
        let account = PrincipalData::parse(&address.to_string())
            .ok()
            .and_then(|principal| self.chain.borrow_mut().account(&principal).ok())
            .map_or_else(Account::default, |entry| Account {
                nonce: entry.nonce,
                balance: Some(entry.balance),
            });
        self.accounts.borrow_mut().insert(*address, account);
        account
    }
}

async fn stackerdb_metadata(
    State(state): State<RpcState>,
    Path((address, contract)): Path<(String, String)>,
) -> Result<axum::Json<Vec<SlotMetadataWire>>, RpcError> {
    let metadata = state
        .stackerdb
        .read()
        .await
        .metadata(&format!("{address}.{contract}"))
        .ok_or(RpcError::NotFound)?;
    Ok(axum::Json(
        metadata.iter().map(SlotMetadataWire::from).collect(),
    ))
}

async fn stackerdb_chunk(
    State(state): State<RpcState>,
    Path((address, contract, slot_id)): Path<(String, String, u32)>,
) -> Result<RawBlockStream, RpcError> {
    chunk_response(&state, &format!("{address}.{contract}"), slot_id, None).await
}

async fn stackerdb_chunk_at_version(
    State(state): State<RpcState>,
    Path((address, contract, slot_id, slot_version)): Path<(String, String, u32, u32)>,
) -> Result<RawBlockStream, RpcError> {
    chunk_response(
        &state,
        &format!("{address}.{contract}"),
        slot_id,
        Some(slot_version),
    )
    .await
}

async fn chunk_response(
    state: &RpcState,
    contract_id: &str,
    slot_id: u32,
    slot_version: Option<u32>,
) -> Result<RawBlockStream, RpcError> {
    let chunk = state
        .stackerdb
        .read()
        .await
        .chunk(contract_id, slot_id, slot_version)
        .map(<[u8]>::to_vec)
        .ok_or(RpcError::NotFound)?;
    Ok(RawBlockStream(chunk))
}

#[derive(Deserialize)]
struct ChunkUploadWire {
    slot_id: u32,
    slot_version: u32,
    sig: String,
    data: String,
}

async fn stackerdb_chunk_upload(
    State(state): State<RpcState>,
    Path((address, contract)): Path<(String, String)>,
    axum::Json(upload): axum::Json<ChunkUploadWire>,
) -> Result<axum::Json<Value>, RpcError> {
    let signature: [u8; 65] = hex::decode(upload.sig.trim_start_matches("0x"))
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| RpcError::BadRequest("invalid chunk signature".to_owned()))?;
    let chunk = nano_stackerdb::Chunk {
        slot_id: upload.slot_id,
        slot_version: upload.slot_version,
        signature: MessageSignature::from_bytes(signature),
        data: hex::decode(upload.data.trim_start_matches("0x"))
            .map_err(|error| RpcError::BadRequest(format!("invalid chunk data: {error}")))?,
    };
    let metadata = SlotMetadataWire::from(&chunk.metadata());
    let contract_id = format!("{address}.{contract}");
    let announce = chunk.clone();
    let slot_id = chunk.slot_id;
    let accepted = state.stackerdb.write().await.put(&contract_id, chunk);
    // What the slot holds *now*, which a refused writer needs: stacks-core answers
    // a refusal with the current metadata so the writer can pick up the version,
    // and a client told nothing walks its version number up one request at a time
    // — which is what a stock signer was seen doing against this route.
    let held = state
        .stackerdb
        .read()
        .await
        .metadata(&contract_id)
        .and_then(|slots| {
            slots
                .get(usize::try_from(slot_id).unwrap_or(usize::MAX))
                .map(SlotMetadataWire::from)
        });
    Ok(axum::Json(match accepted {
        Ok(()) => {
            // A chunk becomes news exactly when a slot takes it, which is here:
            // this route is the only way a chunk enters a nano node, so it is
            // the transition an observer — a signer watching its own reward
            // cycle's contracts — is waiting on.
            state.dispatch(
                EventKind::StackerDbChunks,
                &stackerdb_chunks_payload(&contract_id, std::slice::from_ref(&announce)),
            );
            // A chunk written here has to leave here, or a signer this node hosts
            // is talking to nobody: the miner counting its response reads the
            // chunk from its own replica, and nothing else carries it there.
            if let Some(chunks) = &state.chunks {
                let _ = chunks.send((contract_id.clone(), announce.clone()));
            }
            json!({ "accepted": true, "metadata": metadata })
        }
        Err(refusal) => json!({
            "accepted": false,
            "reason": refusal.reason(),
            "code": refusal.code(),
            "metadata": held,
        }),
    }))
}

/// Take a block off the network the way a peer's would be taken.
///
/// An uploaded block is somebody else's claim about the chain, so it passes the
/// authentication a followed block passes before this node will hold it at all,
/// and is then handed to the executor — which checks its state root as it would
/// any other. Answering `accepted` for a block that never reached the validator
/// is how a node becomes forkable through its own API.
async fn upload_block(
    State(state): State<RpcState>,
    body: Bytes,
) -> Result<axum::Json<BlockUploadWire>, RpcError> {
    let block = NakamotoBlock::decode(&body)
        .map_err(|error| RpcError::BadRequest(format!("failed to decode block: {error}")))?;
    state
        .authenticate(&block)
        .await
        .map_err(|error| RpcError::BadRequest(format!("block refused: {error}")))?;
    let stacks_block_id = format!("0x{}", block.block_id());
    // A node that already holds the block does not re-accept it, and does not
    // offer it again either: the executor would only walk past it.
    let held = state.holds_block(&block).await;
    if !held {
        state.offer_block(block)?;
    }
    Ok(axum::Json(BlockUploadWire {
        stacks_block_id,
        accepted: !held,
    }))
}

/// Judge a proposed block and say so through the event observer.
///
/// The shape is stacks-core's: the request is answered `202 Accepted` as soon as
/// it parses, and the verdict travels as a `proposal_response` event, because a
/// stock signer reads it from there rather than from this response body. A node
/// with no observer registered answers `400`, as stacks-core does — a proposal
/// whose result cannot be reported is a request nobody can act on.
///
/// What nano can and cannot say is the important part. Everything a followed
/// block is authenticated for is checked here and rejected with the code a signer
/// branches on. But a block that passes is only *admitted*: nano validates a
/// state root by executing the block, and it cannot execute a candidate off its
/// tip without leaving that candidate's state behind
/// ([[056-make-rejected-block-execution-leave-no-state]]). So a block this node
/// has not already executed is answered `Reject`, naming that as the reason,
/// rather than `Ok` — a signer must not sign on a validation that did not happen.
async fn block_proposal(
    State(state): State<RpcState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, axum::Json<Value>), RpcError> {
    let token = state
        .proposal_token
        .as_deref()
        .ok_or(RpcError::Unavailable)?;
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        != Some(token)
    {
        return Err(RpcError::Unauthorized);
    }
    if state.observers.is_none() {
        return Err(RpcError::Rejected(json!({
            "result": "Error",
            "message": "No `observer` registered for receiving proposal callbacks",
        })));
    }
    let proposal: BlockProposalWire = serde_json::from_slice(&body)
        .map_err(|error| RpcError::BadRequest(format!("failed to decode proposal: {error}")))?;
    let block = NakamotoBlock::decode(
        &hex::decode(proposal.block.trim_start_matches("0x"))
            .map_err(|error| RpcError::BadRequest(format!("invalid block hex: {error}")))?,
    )
    .map_err(|error| RpcError::BadRequest(format!("failed to decode block: {error}")))?;

    let digest = block.header.signer_signature_hash();
    match judge_proposal(&state, &proposal, &block).await {
        Verdict::Now(outcome) => state.dispatch(
            EventKind::ProposalResponse,
            &proposal_response_payload(digest, &outcome),
        ),
        // Executing a block takes as long as it takes, and stacks-core answers the
        // request before it starts — so the wait happens here, off the request, and
        // the verdict travels the way a signer already reads it.
        Verdict::Pending(answered, size) => {
            let announced = state.clone();
            tokio::spawn(async move {
                let started = SystemTime::now();
                let answer = answered.await;
                let elapsed = started
                    .elapsed()
                    .map_or(0, |elapsed| {
                        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
                    });
                announced.dispatch(
                    EventKind::ProposalResponse,
                    &proposal_response_payload(digest, &resolve(answer, size, elapsed)),
                );
            });
        }
    }
    Ok((
        StatusCode::ACCEPTED,
        axum::Json(json!({
            "result": "Accepted",
            "message": "Block proposal is processing, result will be returned via the event observer",
        })),
    ))
}

async fn judge_proposal(
    state: &RpcState,
    proposal: &BlockProposalWire,
    block: &NakamotoBlock,
) -> Verdict {
    let rejected = |reason: String, code| {
        Verdict::Now(ProposalOutcome::Rejected { reason, code })
    };
    // The chain identifier is in the request rather than in the block, and a
    // proposal for another chain is not a proposal at all.
    if let Some(chain_id) = proposal.chain_id
        && chain_id != state.network.chain_id()
    {
        return rejected(
            format!(
                "proposal names chain {chain_id:#010x}, this node is on {:#010x}",
                state.network.chain_id()
            ),
            ProposalRejectCode::NetworkChainMismatch,
        );
    }
    if proposal
        .replay_txs
        .as_ref()
        .is_some_and(|replay| !replay.is_empty())
    {
        return rejected(
            "this node does not validate against a transaction replay set".to_owned(),
            ProposalRejectCode::InvalidTransactionReplay,
        );
    }
    if let Err(error) = state.authenticate(block).await {
        return rejected(error, ProposalRejectCode::InvalidBlock);
    }
    if state.holds_block(block).await {
        // Already executed, so the state root was already checked. Zero cost is
        // the value stacks-core itself reports for a block it did not have to
        // execute, and a signer reads it that way.
        return Verdict::Now(ProposalOutcome::Accepted {
            cost: clarity::vm::costs::ExecutionCost::ZERO,
            size: block.encode().len() as u64,
            validation_time_ms: 0,
        });
    }
    if !state.holds_parent_of(block).await {
        return rejected(
            format!(
                "this node has not executed the parent {} this block builds on",
                block.header.parent_block_id
            ),
            ProposalRejectCode::UnknownParent,
        );
    }
    // Admitted for execution: the offer is what gets the block onto this node's
    // own chain once the network agrees on it, and it happens whether or not this
    // node is able to vouch for the block first.
    if let Err(error) = state.offer_block(block.clone()) {
        return rejected(
            format!("this node cannot take the block: {error:?}"),
            ProposalRejectCode::ChainstateError,
        );
    }
    let Some(proposals) = &state.proposals else {
        // Admitted, and that is all: a node with no proposal validator cannot run
        // the block off its tip, and a signer must not read "we will look at it"
        // as "we agree with it".
        return rejected(
            "this node validates a proposal by executing it, and has no proposal \
             validator configured to execute this one; it has been admitted and its \
             state root will be checked when the chain reaches it"
                .to_owned(),
            ProposalRejectCode::ChainstateError,
        );
    };
    let (verdict, answered) = tokio::sync::oneshot::channel();
    if proposals
        .send(ProposalRequest {
            block: block.clone(),
            verdict,
        })
        .is_err()
    {
        return rejected(
            "this node's proposal validator has stopped".to_owned(),
            ProposalRejectCode::ChainstateError,
        );
    }
    Verdict::Pending(answered, block.encode().len() as u64)
}

/// What a route can say about a proposal now, and what it has to wait for.
enum Verdict {
    Now(ProposalOutcome),
    /// The validator was asked; the answer and the block's size come later.
    Pending(
        tokio::sync::oneshot::Receiver<Result<(), (String, ProposalRejectCode)>>,
        u64,
    ),
}

/// Turn the validator's answer into the outcome an observer is told.
///
/// The cost is reported as zero rather than invented: the validator answers
/// whether the root holds, and stacks-core itself reports zero for a block it did
/// not have to execute, which is how a signer reads it
/// (`stacks-signer/src/v0/signer.rs:1569`).
fn resolve(
    answer: Result<Result<(), (String, ProposalRejectCode)>, tokio::sync::oneshot::error::RecvError>,
    size: u64,
    elapsed: u64,
) -> ProposalOutcome {
    match answer {
        Ok(Ok(())) => ProposalOutcome::Accepted {
            cost: clarity::vm::costs::ExecutionCost::ZERO,
            size,
            validation_time_ms: elapsed,
        },
        Ok(Err((reason, code))) => ProposalOutcome::Rejected { reason, code },
        Err(_) => ProposalOutcome::Rejected {
            reason: "this node's proposal validator stopped before answering".to_owned(),
            code: ProposalRejectCode::ChainstateError,
        },
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[derive(Deserialize)]
struct TenureQuery {
    stop: Option<String>,
}

/// A block identifier as a route spells it, `0x`-prefixed or bare.
fn block_id(value: &str) -> Option<StacksBlockId> {
    let bytes: [u8; 32] = hex::decode(value.trim_start_matches("0x"))
        .ok()?
        .try_into()
        .ok()?;
    Some(StacksBlockId::from_bytes(bytes))
}

/// Stream a tenure, from the durable store if this node kept it.
///
/// The store is asked first because it is the node's own record of what it
/// executed; the followed view is the fallback for a node that keeps no archive,
/// and for blocks executed before it had one.
async fn tenure(
    State(state): State<RpcState>,
    Path(start_block_id): Path<String>,
    Query(query): Query<TenureQuery>,
) -> Result<RawBlockStream, RpcError> {
    if let Some(archive) = state.archive.as_ref()
        && let Some(start) = block_id(&start_block_id)
    {
        let stop = query.stop.as_deref().and_then(block_id);
        let kept = archive.tenure(start, stop);
        if !kept.is_empty() {
            return Ok(RawBlockStream(kept.concat()));
        }
    }
    let tenure = executed(&state)
        .await?
        .chain
        .into_iter()
        .find(|tenure| tenure.info.tenure_start_block_id.to_string() == start_block_id)
        .ok_or(RpcError::NotFound)?;
    let mut bytes = Vec::new();
    for block in tenure.blocks {
        if query
            .stop
            .as_ref()
            .is_some_and(|stop| *stop == block.block_id().to_string())
        {
            break;
        }
        bytes.extend(block.encode());
    }
    Ok(RawBlockStream(bytes))
}

async fn block(
    State(state): State<RpcState>,
    Path(block_id_path): Path<String>,
) -> Result<RawBlockStream, RpcError> {
    if let Some(archive) = state.archive.as_ref()
        && let Some(kept) = block_id(&block_id_path).and_then(|id| archive.block(id))
    {
        return Ok(RawBlockStream(kept));
    }
    let block = executed(&state)
        .await?
        .chain
        .into_iter()
        .flat_map(|tenure| tenure.blocks)
        .find(|block| block.block_id().to_string() == block_id_path)
        .ok_or(RpcError::NotFound)?;
    Ok(RawBlockStream(block.encode()))
}

async fn events(
    State(state): State<RpcState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(state.events.subscribe()).filter_map(|event| {
        event.ok().and_then(|event| {
            serde_json::to_string(&event)
                .ok()
                .map(|data| Ok(Event::default().event("new_block").data(data)))
        })
    });
    Sse::new(stream)
}

struct RawBlockStream(Vec<u8>);

impl IntoResponse for RawBlockStream {
    fn into_response(self) -> Response {
        (
            [(header::CONTENT_TYPE, "application/octet-stream")],
            Bytes::from(self.0),
        )
            .into_response()
    }
}

#[derive(Serialize)]
struct AccountWire {
    balance: String,
    locked: String,
    unlock_height: u64,
    nonce: u64,
}

#[derive(Serialize)]
struct CallReadOnlyWire {
    okay: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cause: Option<String>,
}

impl CallReadOnlyWire {
    const fn failed(cause: String) -> Self {
        Self {
            okay: false,
            result: None,
            cause: Some(cause),
        }
    }
}

#[derive(Serialize)]
struct SlotMetadataWire {
    slot_id: u32,
    slot_version: u32,
    data_hash: String,
    signature: String,
}

impl From<&nano_stackerdb::SlotMetadata> for SlotMetadataWire {
    fn from(metadata: &nano_stackerdb::SlotMetadata) -> Self {
        Self {
            slot_id: metadata.slot_id,
            slot_version: metadata.slot_version,
            data_hash: metadata.data_hash.to_string(),
            signature: hex::encode(metadata.signature.as_bytes()),
        }
    }
}

#[derive(Serialize)]
struct BlockUploadWire {
    stacks_block_id: String,
    accepted: bool,
}

/// A proposal as a stock signer sends it (`NakamotoBlockProposal`).
///
/// `chain_id` and `replay_txs` are optional here where stacks-core requires
/// them, so that a hand-rolled proposal — which is how nano's own tests and
/// hacknet tooling send one — is not refused for a field it had no reason to set.
#[derive(Deserialize)]
struct BlockProposalWire {
    block: String,
    #[serde(default)]
    chain_id: Option<u32>,
    #[serde(default)]
    replay_txs: Option<Vec<Value>>,
}

/// The tip a node has executed and sealed, as opposed to the one it has seen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SealedTip {
    pub stacks_height: u64,
    pub stacks_tip: StacksBlockId,
    /// The tip's block hash, which is not its identifier: `/v2/info` reports the
    /// hash and every other route reports the identifier, and a signer reads the
    /// former into a `BlockHeaderHash`.
    pub stacks_block_hash: BlockHeaderHash,
    pub consensus_hash: ConsensusHash,
    pub bitcoin_height: u64,
    pub state_index_root: TrieHash,
}

/// The tip this node's fork choice settled on, and who offered it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedTip {
    pub stacks_height: u64,
    pub stacks_tip: StacksBlockId,
    /// The peer it came from, named the way an operator configured it.
    pub peer: String,
}

#[derive(Serialize)]
struct SyncStatusWire {
    followed_stacks_height: Option<u64>,
    selected_stacks_height: Option<u64>,
    selected_stacks_tip: Option<String>,
    selected_from_peer: Option<String>,
    executed_stacks_height: Option<u64>,
    executed_stacks_tip: Option<String>,
    executed_state_index_root: Option<String>,
    blocks_behind: Option<u64>,
}

#[derive(Serialize)]
struct NodeInfoWire {
    burn_block_height: u64,
    stacks_tip_height: u64,
    stacks_tip: String,
    stacks_tip_consensus_hash: String,
    /// The burn view, which a signer reads separately from the tenure's.
    pox_consensus: String,
    server_version: String,
    network_id: u32,
}

#[derive(Serialize)]
struct TenureInfoWire {
    consensus_hash: String,
    tenure_start_block_id: String,
    parent_consensus_hash: String,
    parent_tenure_start_block_id: String,
    tip_block_id: String,
    tip_height: u64,
    reward_cycle: u64,
}

impl From<nano_sync::TenureInfo> for TenureInfoWire {
    fn from(info: nano_sync::TenureInfo) -> Self {
        Self {
            consensus_hash: info.consensus_hash.to_string(),
            tenure_start_block_id: info.tenure_start_block_id.to_string(),
            parent_consensus_hash: info.parent_consensus_hash.to_string(),
            parent_tenure_start_block_id: info.parent_tenure_start_block_id.to_string(),
            tip_block_id: info.tip_block_id.to_string(),
            tip_height: info.tip_height,
            reward_cycle: info.reward_cycle,
        }
    }
}

#[derive(Serialize)]
struct SortitionInfoWire {
    burn_block_hash: String,
    burn_block_height: u64,
    burn_header_timestamp: u64,
    sortition_id: String,
    parent_sortition_id: String,
    consensus_hash: String,
    was_sortition: bool,
    miner_pk_hash160: Option<String>,
    stacks_parent_ch: Option<String>,
    last_sortition_ch: Option<String>,
    committed_block_hash: Option<String>,
    /// The seed this sortition produced, which stacks-core's own reader requires
    /// to be present: `prefix_opt_hex` deserializes a field, and a missing one is
    /// an error rather than a `None`.
    vrf_seed: Option<String>,
}

impl From<nano_sync::SortitionInfo> for SortitionInfoWire {
    fn from(sortition: nano_sync::SortitionInfo) -> Self {
        Self {
            burn_block_hash: format!("0x{}", sortition.bitcoin_block_hash),
            burn_block_height: sortition.bitcoin_height,
            burn_header_timestamp: sortition.bitcoin_timestamp,
            sortition_id: format!("0x{}", sortition.sortition_id),
            parent_sortition_id: format!("0x{}", sortition.parent_sortition_id),
            consensus_hash: format!("0x{}", sortition.consensus_hash),
            was_sortition: sortition.was_sortition,
            miner_pk_hash160: sortition
                .miner_public_key_hash
                .map(|hash| format!("0x{hash}")),
            stacks_parent_ch: sortition
                .stacks_parent_consensus_hash
                .map(|hash| format!("0x{hash}")),
            last_sortition_ch: sortition
                .last_sortition_consensus_hash
                .map(|hash| format!("0x{hash}")),
            committed_block_hash: sortition
                .committed_block_hash
                .map(|hash| format!("0x{hash}")),
            vrf_seed: sortition.vrf_seed.map(|seed| format!("0x{}", hex::encode(seed))),
        }
    }
}

/// One tenure as a signer's fork check reads it (`TenureForkingInfo`).
///
/// The blocks are served with the tenure, because that is what the check is: a
/// signer asks which tenures descend from a sortition it knows and compares the
/// blocks in them against the ones it was asked to sign on top of.
#[derive(Serialize)]
struct TenureForkInfoWire {
    burn_block_hash: String,
    burn_block_height: u64,
    sortition_id: String,
    parent_sortition_id: String,
    consensus_hash: String,
    was_sortition: bool,
    first_block_mined: Option<String>,
    /// The tenure's blocks, consensus-serialized as one length-prefixed vector
    /// and hex-encoded, which is what `prefix_opt_hex_codec` reads.
    nakamoto_blocks: Option<String>,
}

impl From<&FollowedTenure> for TenureForkInfoWire {
    fn from(tenure: &FollowedTenure) -> Self {
        let sortition = &tenure.sortition;
        Self {
            burn_block_hash: format!("0x{}", sortition.bitcoin_block_hash),
            burn_block_height: sortition.bitcoin_height,
            sortition_id: format!("0x{}", sortition.sortition_id),
            parent_sortition_id: format!("0x{}", sortition.parent_sortition_id),
            consensus_hash: format!("0x{}", sortition.consensus_hash),
            was_sortition: sortition.was_sortition,
            first_block_mined: tenure
                .blocks
                .first()
                .map(|block| format!("0x{}", block.block_id())),
            nakamoto_blocks: Some(format!("0x{}", hex::encode(encode_blocks(&tenure.blocks)))),
        }
    }
}

/// A block vector in the consensus encoding: a big-endian count, then the blocks.
fn encode_blocks(blocks: &[NakamotoBlock]) -> Vec<u8> {
    let count = u32::try_from(blocks.len()).unwrap_or(u32::MAX);
    let mut bytes = count.to_be_bytes().to_vec();
    for block in blocks {
        bytes.extend(block.encode());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use nano_sync::{Node, NodeView};
    use nano_primitives::{
        BitcoinHeaderHash, BlockHeaderHash, ConsensusHash, SortitionId, StacksBlockId, TrieHash,
    };
    use nano_sync::{FollowedTenure, NodeInfo, PoxInfo, SortitionInfo, SyncClient, TenureInfo};
    use reqwest::Url;
    use tower::ServiceExt;

    use std::{collections::HashMap, sync::Arc};

    use clarity::vm::{Value, types::PrincipalData};
    use nano_address::StacksAddress;
    use nano_codec::{
        AnchorMode, Principal, Transaction, TransactionPayloadData, TransactionVersion,
    };
    use nano_crypto::StacksPrivateKey;
    use nano_mempool::Mempool;
    use nano_primitives::Network;
    use serde_json::json;
    use tokio::sync::Mutex;

    use super::{
        AccountEntry, ChainAccess, ChainAccessError, EventDispatcher, NakamotoBlock, ReadOnlyCall,
        Router, RpcState, SealedTip, mpsc, router,
    };

    /// The tests reach for both `Value`s: Clarity's for a read-only answer, and
    /// JSON's for every payload.
    use serde_json::Value as Json;

    /// The events an observer has been sent, by path and payload.
    type Received = Arc<std::sync::Mutex<Vec<(String, Json)>>>;

    const NETWORK: Network = Network::TESTNET;

    /// A chain that answers from a fixed account table, so the routes can be
    /// exercised without a MARF behind them.
    #[derive(Default)]
    struct FixedChain {
        accounts: HashMap<String, AccountEntry>,
        answer: Option<Value>,
    }

    impl ChainAccess for FixedChain {
        fn account(&mut self, principal: &PrincipalData) -> Result<AccountEntry, ChainAccessError> {
            Ok(self
                .accounts
                .get(&principal.to_string())
                .copied()
                .unwrap_or_default())
        }

        fn call_read_only(&mut self, call: &ReadOnlyCall) -> Result<Value, ChainAccessError> {
            self.answer.clone().ok_or_else(|| {
                ChainAccessError::Failed(format!("no such function {}", call.function))
            })
        }
    }

    fn key(seed: &[u8]) -> StacksPrivateKey {
        StacksPrivateKey::from_seed(seed)
    }

    fn address(key: &StacksPrivateKey) -> StacksAddress {
        StacksAddress::single_signature(
            nano_primitives::hash160(&key.public_key().to_bytes_compressed()),
            NETWORK.is_mainnet(),
        )
    }

    fn transfer(sender: &StacksPrivateKey, nonce: u64) -> Transaction {
        Transaction::sign_standard(
            TransactionVersion::for_network(NETWORK),
            NETWORK.chain_id(),
            AnchorMode::OnChainOnly,
            sender,
            nonce,
            400,
            TransactionPayloadData::TokenTransfer {
                recipient: Principal::Standard(address(&key(b"recipient"))),
                amount: 1,
                memo: [0; 34],
            },
        )
        .expect("sign a transfer")
    }

    async fn body_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        serde_json::from_slice(&bytes).expect("decode body")
    }

    fn chain(
        accounts: &[(StacksAddress, AccountEntry)],
        answer: Option<Value>,
    ) -> Arc<Mutex<dyn ChainAccess>> {
        Arc::new(Mutex::new(FixedChain {
            accounts: accounts
                .iter()
                .map(|(address, entry)| (address.to_string(), *entry))
                .collect(),
            answer,
        }))
    }

    #[tokio::test]
    async fn reports_an_account_the_way_a_wallet_reads_it() {
        let sender = key(b"sender");
        let entry = AccountEntry {
            balance: 4_000_000,
            locked: 1_000,
            unlock_height: 42,
            nonce: 7,
        };
        let app = router(RpcState::new(NETWORK).with_chain(chain(&[(address(&sender), entry)], None)));

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/v2/accounts/{}", address(&sender)))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(
            body_json(response).await,
            json!({
                "balance": "0x000000000000000000000000003d0900",
                "locked": "0x000000000000000000000000000003e8",
                "unlock_height": 42,
                "nonce": 7,
            })
        );
    }

    #[tokio::test]
    async fn a_read_only_call_reports_its_value_and_its_cause() {
        let app = router(RpcState::new(NETWORK).with_chain(chain(&[], Some(Value::UInt(3)))));
        let call = |uri: &str, app: Router| {
            let uri = uri.to_owned();
            async move {
                app.oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(Body::from(
                            json!({"sender": address(&key(b"sender")).to_string(), "arguments": []})
                                .to_string(),
                        ))
                        .expect("request"),
                )
                .await
                .expect("response")
            }
        };

        let answered = call(
            "/v2/contracts/call-read/ST000000000000000000002AMW42H/pox-5/get-cycle",
            app.clone(),
        )
        .await;
        assert_eq!(
            body_json(answered).await,
            json!({ "okay": true, "result": "0x0100000000000000000000000000000003" })
        );

        let failed = router(RpcState::new(NETWORK).with_chain(chain(&[], None)));
        let response = call(
            "/v2/contracts/call-read/ST000000000000000000002AMW42H/pox-5/get-cycle",
            failed,
        )
        .await;
        assert_eq!(
            body_json(response).await,
            json!({ "okay": false, "cause": "no such function get-cycle" })
        );
    }

    #[tokio::test]
    async fn a_submitted_transfer_is_held_and_a_stale_nonce_is_refused() {
        let sender = key(b"sender");
        let funded = AccountEntry {
            balance: 4_000_000,
            locked: 0,
            unlock_height: 0,
            nonce: 1,
        };
        let mempool = Arc::new(Mutex::new(Mempool::new(NETWORK)));
        let app = router(
            RpcState::new(NETWORK)
                .with_chain(chain(&[(address(&sender), funded)], None))
                .with_mempool(mempool.clone()),
        );
        let submit = |transaction: Transaction, app: Router| async move {
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/transactions")
                    .header("content-type", "application/octet-stream")
                    .body(Body::from(transaction.encode()))
                    .expect("request"),
            )
            .await
            .expect("response")
        };

        let accepted = transfer(&sender, 1);
        let txid = accepted.txid();
        let response = submit(accepted, app.clone()).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await, json!(txid.to_string()));
        assert!(mempool.lock().await.contains(txid));

        let stale = transfer(&sender, 0);
        let response = submit(stale, app).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(body["reason"], json!("BadNonce"));
        assert_eq!(body["reason_data"]["expected"], json!(1));
    }

    #[tokio::test]
    async fn a_reward_set_is_served_once_the_node_derived_it() {
        let state = RpcState::new(NETWORK);
        let app = router(state.clone());
        let missing = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v3/stacker_set/15")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST);

        state
            .publish_stacker_set(15, json!({ "signers": [] }))
            .await;
        let found = app
            .oneshot(
                Request::builder()
                    .uri("/v3/stacker_set/15")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(
            body_json(found).await,
            json!({ "stacker_set": { "signers": [] } })
        );
    }

    #[tokio::test]
    async fn a_signer_writes_a_chunk_and_reads_it_back() {
        let writer = key(b"signer");
        let state = RpcState::new(NETWORK);
        state.stackerdb().write().await.configure(
            "ST000000000000000000002AMW42H.signers-0-1",
            vec![nano_primitives::hash160(
                &writer.public_key().to_bytes_compressed(),
            )],
        );
        let app = router(state);
        let mut chunk = nano_stackerdb::Chunk::new(0, 1, b"accepted".to_vec());
        chunk.sign(&writer).expect("sign chunk");

        let accepted = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/stackerdb/ST000000000000000000002AMW42H/signers-0-1/chunks")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "slot_id": 0,
                            "slot_version": 1,
                            "sig": hex::encode(chunk.signature.as_bytes()),
                            "data": hex::encode(&chunk.data),
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(body_json(accepted).await["accepted"], json!(true));

        let read = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v2/stackerdb/ST000000000000000000002AMW42H/signers-0-1/0")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let data = axum::body::to_bytes(read.into_body(), usize::MAX)
            .await
            .expect("read chunk");
        assert_eq!(data.as_ref(), b"accepted");

        let metadata = app
            .oneshot(
                Request::builder()
                    .uri("/v2/stackerdb/ST000000000000000000002AMW42H/signers-0-1")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let metadata = body_json(metadata).await;
        assert_eq!(metadata[0]["slot_version"], json!(1));
        assert_eq!(
            metadata[0]["signature"],
            json!(hex::encode(chunk.signature.as_bytes()))
        );
    }

    fn captured_view() -> NodeView {
        NodeView {
            node_info: NodeInfo {
                bitcoin_height: 11,
                stacks_height: 12,
                stacks_tip: BlockHeaderHash::from_bytes([1; 32]),
                consensus_hash: ConsensusHash::from_bytes([2; 20]),
                network_id: 2_147_483_648,
            },
            pox_info: PoxInfo {
                first_bitcoin_height: 0,
                bitcoin_height: 11,
                prepare_phase_length: 5,
                reward_phase_length: 15,
                reward_slots: 2,
                rejection_fraction: None,
                pox_5_activation_height: Some(262),
                v1_unlock_height: Some(205),
                v2_unlock_height: Some(207),
                v3_unlock_height: Some(210),
            },
            tenures: vec![FollowedTenure {
                info: TenureInfo {
                    consensus_hash: ConsensusHash::from_bytes([2; 20]),
                    tenure_start_block_id: StacksBlockId::from_bytes([3; 32]),
                    parent_consensus_hash: ConsensusHash::from_bytes([4; 20]),
                    parent_tenure_start_block_id: StacksBlockId::from_bytes([5; 32]),
                    tip_block_id: StacksBlockId::from_bytes([6; 32]),
                    tip_height: 12,
                    reward_cycle: 1,
                },
                sortition: SortitionInfo {
                    bitcoin_block_hash: BitcoinHeaderHash::from_bytes([7; 32]),
                    bitcoin_height: 11,
                    bitcoin_timestamp: 0,
                    sortition_id: SortitionId::from_bytes([8; 32]),
                    parent_sortition_id: SortitionId::from_bytes([9; 32]),
                    consensus_hash: ConsensusHash::from_bytes([2; 20]),
                    was_sortition: true,
                    miner_public_key_hash: None,
                    stacks_parent_consensus_hash: None,
                    last_sortition_consensus_hash: None,
                    committed_block_hash: None,
                    vrf_seed: None,
                },
                blocks: Vec::new(),
            }],
        }
    }

    #[tokio::test]
    async fn rejects_requests_until_the_node_has_a_validated_view() {
        let app = router(RpcState::new(NETWORK));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v2/info")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// A peer at one height and an executor at another are two facts, and the
    /// Stacks-compatible route has to answer with the second.
    ///
    /// This is the regression for a node that followed mainnet to within three
    /// blocks of the tip for eighty minutes while having executed nothing at
    /// all: it published the peer's height as its own, and the difference was
    /// invisible from every endpoint it served.
    #[tokio::test]
    async fn the_served_tip_is_the_executed_one_not_the_followed_one() {
        let state = RpcState::new(NETWORK);
        // The peer is at 12; this node has executed nothing beyond 4.
        state.publish(captured_view()).await;
        let sealed = SealedTip {
            stacks_height: 4,
            stacks_tip: StacksBlockId::from_bytes([7; 32]),
            stacks_block_hash: BlockHeaderHash::from_bytes([6; 32]),
            consensus_hash: ConsensusHash::from_bytes([8; 20]),
            bitcoin_height: 3,
            state_index_root: TrieHash::from_bytes([9; 32]),
        };
        state.publish_executed(sealed.clone()).await;

        let info = body_json(
            router(state.clone())
                .oneshot(
                    Request::builder()
                        .uri("/v2/info")
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response"),
        )
        .await;
        assert_eq!(info["stacks_tip_height"], json!(4));
        // The hash, not the identifier: this is the one route that reports the
        // block hash, because a signer reads it into a `BlockHeaderHash`.
        assert_eq!(
            info["stacks_tip"],
            json!(sealed.stacks_block_hash.to_string())
        );
        assert_eq!(info["burn_block_height"], json!(3));

        let status = body_json(
            router(state)
                .oneshot(
                    Request::builder()
                        .uri("/nano/sync_status")
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response"),
        )
        .await;
        assert_eq!(status["followed_stacks_height"], json!(12));
        assert_eq!(status["executed_stacks_height"], json!(4));
        assert_eq!(status["blocks_behind"], json!(8));
    }

    /// Three heights, three names, one route that reports all three.
    ///
    /// The peer *advertises* a tip, this node's fork choice *selects* one, and the
    /// executor *executes* up to a third. They are three different facts and the
    /// middle one is the one nothing else can be read off: a node whose selection
    /// sits below what its peers advertise has refused a tip, and a node whose
    /// execution sits below its selection is catching up. Reporting only the ends
    /// cannot tell those two apart, and that is the whole of what
    /// [[046-distinguish-followed-and-executed-chain-tips]] was about.
    #[tokio::test]
    async fn the_followed_selected_and_executed_tips_are_three_separate_answers() {
        let state = RpcState::new(NETWORK);
        // The peer says 12. The fork choice would not have the highest thing
        // offered and settled on 9. This node has executed 4.
        state.publish(captured_view()).await;
        state
            .publish_selected(super::SelectedTip {
                stacks_height: 9,
                stacks_tip: StacksBlockId::from_bytes([9; 32]),
                peer: "http://peer.example:20443/".to_owned(),
            })
            .await;
        state
            .publish_executed(SealedTip {
                stacks_height: 4,
                stacks_tip: StacksBlockId::from_bytes([4; 32]),
                stacks_block_hash: BlockHeaderHash::from_bytes([5; 32]),
                consensus_hash: ConsensusHash::from_bytes([6; 20]),
                bitcoin_height: 3,
                state_index_root: TrieHash::from_bytes([7; 32]),
            })
            .await;

        let status = body_json(
            router(state)
                .oneshot(
                    Request::builder()
                        .uri("/nano/sync_status")
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response"),
        )
        .await;
        assert_eq!(status["followed_stacks_height"], json!(12));
        assert_eq!(status["selected_stacks_height"], json!(9));
        assert_eq!(status["executed_stacks_height"], json!(4));
        assert_eq!(
            status["selected_stacks_tip"],
            json!(StacksBlockId::from_bytes([9; 32]).to_string())
        );
        assert_eq!(
            status["selected_from_peer"],
            json!("http://peer.example:20443/")
        );
        // Behind the peer, not behind the selection: how far there is left to go
        // is measured against what the network has, and a fork choice that
        // refused the peer's tip does not shorten the journey.
        assert_eq!(status["blocks_behind"], json!(8));
    }

    /// A node that has followed a peer but executed nothing must not answer the
    /// Stacks tip at all, rather than answering with the peer's.
    #[tokio::test]
    async fn a_node_that_executed_nothing_serves_no_tip() {
        let state = RpcState::new(NETWORK);
        state.publish(captured_view()).await;

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/v2/info")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn publishes_one_event_per_new_tip() {
        let state = RpcState::new(NETWORK);
        let mut events = state.events.subscribe();
        state.publish(captured_view()).await;
        let event = events.try_recv().expect("new tip event");
        assert_eq!(event.stacks_height, 12);
        assert_eq!(event.bitcoin_height, 11);

        state.publish(captured_view()).await;
        assert!(events.try_recv().is_err());
    }

    /// A chain of blocks that agree with each other, for the routes that serve
    /// them: `executed_chain` walks parent links, so the links have to be real.
    fn synthetic_block(
        height: u64,
        parent: StacksBlockId,
        consensus_hash: ConsensusHash,
    ) -> NakamotoBlock {
        NakamotoBlock {
            header: nano_chainstate::NakamotoBlockHeader {
                version: 1,
                chain_length: height,
                bitcoin_spent: height * 10,
                consensus_hash,
                parent_block_id: parent,
                transaction_merkle_root: nano_primitives::Sha256Sum::default(),
                state_index_root: TrieHash::from_bytes([u8::try_from(height % 256).unwrap_or(0); 32]),
                timestamp: 1_700_000_000 + height,
                miner_signature: nano_crypto::MessageSignature::from_bytes([0; 65]),
                signer_signatures: Vec::new(),
                pox_treatment: nano_primitives::BitVec::zeros(1).expect("a bit vector"),
                problematic_transactions: Vec::new(),
            },
            transactions: Vec::new(),
        }
    }

    /// A view whose one tenure carries three linked blocks, of which this node
    /// is meant to have executed only the first `executed`.
    fn view_with_blocks(count: u64) -> (NodeView, Vec<NakamotoBlock>) {
        let consensus_hash = ConsensusHash::from_bytes([2; 20]);
        let mut blocks = Vec::new();
        let mut parent = StacksBlockId::from_bytes([0; 32]);
        for height in 1..=count {
            let block = synthetic_block(height, parent, consensus_hash);
            parent = block.block_id();
            blocks.push(block);
        }
        let mut view = captured_view();
        view.tenures[0].info.tenure_start_block_id = blocks[0].block_id();
        view.tenures[0].blocks = blocks.clone();
        (view, blocks)
    }

    fn sealed_at(block: &NakamotoBlock) -> SealedTip {
        SealedTip {
            stacks_height: block.header.chain_length,
            stacks_tip: block.block_id(),
            stacks_block_hash: block.header.block_hash(),
            consensus_hash: block.header.consensus_hash,
            bitcoin_height: 11,
            state_index_root: block.header.state_index_root,
        }
    }

    /// Every route answers from what this node executed, so a block the peer has
    /// and this node has not is not served, and the tenure it belongs to reports
    /// the height this node actually reached.
    ///
    /// A peer's view is ahead by construction. Serving it is how a node reported
    /// itself at mainnet's tip with an empty MARF; the fix is not to bound one
    /// route but to publish one snapshot every route reads.
    #[tokio::test]
    async fn no_route_serves_a_block_the_node_has_not_executed() {
        let (view, blocks) = view_with_blocks(3);
        let state = RpcState::new(NETWORK);
        state.publish(view).await;
        state.publish_executed(sealed_at(&blocks[1])).await;
        let app = router(state);
        let get = |uri: String, app: Router| async move {
            app.oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response")
        };

        for (block, expected) in [
            (&blocks[0], StatusCode::OK),
            (&blocks[1], StatusCode::OK),
            (&blocks[2], StatusCode::NOT_FOUND),
        ] {
            let response = get(format!("/v3/blocks/{}", block.block_id()), app.clone()).await;
            assert_eq!(
                response.status(),
                expected,
                "block {} at height {}",
                block.block_id(),
                block.header.chain_length
            );
        }

        // The tenure's own tip comes down with it, rather than advertising a
        // block the same node will not serve.
        let tenure = body_json(get("/v3/tenures/info".to_owned(), app.clone()).await).await;
        assert_eq!(tenure["tip_height"], json!(2));
        assert_eq!(
            tenure["tip_block_id"],
            json!(blocks[1].block_id().to_string())
        );

        // And the tenure stream stops there too.
        let stream = get(
            format!("/v3/tenures/{}", blocks[0].block_id()),
            app.clone(),
        )
        .await;
        let bytes = axum::body::to_bytes(stream.into_body(), usize::MAX)
            .await
            .expect("read tenure");
        let expected: Vec<u8> = blocks[..2].iter().flat_map(NakamotoBlock::encode).collect();
        assert_eq!(bytes.as_ref(), expected.as_slice());

        // `/v2/pox` keeps the cycle constants, which are configuration, and
        // reports the burn height this node executed under.
        let pox = body_json(get("/v2/pox".to_owned(), app).await).await;
        assert_eq!(pox["prepare_phase_block_length"], json!(5));
        assert_eq!(pox["current_burnchain_block_height"], json!(11));
    }

    /// A store of the blocks this node executed, as the node keeps one.
    struct KeptBlocks(Vec<NakamotoBlock>);

    impl super::ExecutedBlocks for KeptBlocks {
        fn block(&self, block_id: StacksBlockId) -> Option<Vec<u8>> {
            self.0
                .iter()
                .find(|block| block.block_id() == block_id)
                .map(NakamotoBlock::encode)
        }

        fn tenure(
            &self,
            start_block_id: StacksBlockId,
            stop: Option<StacksBlockId>,
        ) -> Vec<Vec<u8>> {
            let Some(start) = self.0.iter().find(|block| block.block_id() == start_block_id)
            else {
                return Vec::new();
            };
            self.0
                .iter()
                .filter(|block| block.header.consensus_hash == start.header.consensus_hash)
                .skip_while(|block| block.block_id() != start_block_id)
                .take_while(|block| Some(block.block_id()) != stop)
                .map(NakamotoBlock::encode)
                .collect()
        }
    }

    /// A node serves the blocks it executed, whether or not a peer has just
    /// mentioned them.
    ///
    /// This is what the followed view could not do. Bounded at the executed tip it
    /// is honest, but it only ever holds what a peer *said*, so a node far enough
    /// behind that the tenure walk fails answered `404` for its own tip — with the
    /// state for it sealed on disk. The store answers from what this node ran.
    #[tokio::test]
    async fn the_blocks_this_node_executed_are_served_without_a_peer_view() {
        let (_, blocks) = view_with_blocks(3);
        let state = RpcState::new(NETWORK)
            .with_executed_blocks(Arc::new(KeptBlocks(blocks.clone())));
        // No followed view at all, and an executed tip the view could not have
        // reached: exactly the node that used to serve nothing.
        state.publish_executed(sealed_at(&blocks[2])).await;
        let app = router(state);
        let get = |uri: String, app: Router| async move {
            app.oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response")
        };

        for block in &blocks {
            let response = get(format!("/v3/blocks/{}", block.block_id()), app.clone()).await;
            assert_eq!(response.status(), StatusCode::OK, "block {}", block.block_id());
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read the block");
            assert_eq!(bytes.as_ref(), block.encode().as_slice());
        }

        // The whole tenure, from its first block, in the order it was executed.
        let stream = get(format!("/v3/tenures/{}", blocks[0].block_id()), app.clone()).await;
        let bytes = axum::body::to_bytes(stream.into_body(), usize::MAX)
            .await
            .expect("read the tenure");
        let whole: Vec<u8> = blocks.iter().flat_map(NakamotoBlock::encode).collect();
        assert_eq!(bytes.as_ref(), whole.as_slice());

        // And stopping before a block the caller already holds.
        let stream = get(
            format!(
                "/v3/tenures/{}?stop={}",
                blocks[0].block_id(),
                blocks[2].block_id()
            ),
            app.clone(),
        )
        .await;
        let bytes = axum::body::to_bytes(stream.into_body(), usize::MAX)
            .await
            .expect("read the tenure");
        let two: Vec<u8> = blocks[..2].iter().flat_map(NakamotoBlock::encode).collect();
        assert_eq!(bytes.as_ref(), two.as_slice());

        // A block this node never executed is still not served.
        let response = get(format!("/v3/blocks/{}", StacksBlockId::from_bytes([9; 32])), app).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// A tip the peer's view does not reach leaves nothing to serve, which is the
    /// honest answer for a node that is catching up: it holds the blocks, but it
    /// cannot prove from a peer's view that they are the ones it executed.
    #[tokio::test]
    async fn a_tip_outside_the_followed_view_serves_no_blocks() {
        let (view, blocks) = view_with_blocks(3);
        let state = RpcState::new(NETWORK);
        state.publish(view).await;
        state
            .publish_executed(sealed_at(&synthetic_block(
                9,
                StacksBlockId::from_bytes([9; 32]),
                ConsensusHash::from_bytes([9; 20]),
            )))
            .await;
        let app = router(state);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v3/blocks/{}", blocks[0].block_id()))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let tenure = app
            .oneshot(
                Request::builder()
                    .uri("/v3/tenures/info")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(tenure.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// A validator that records what it was asked about and answers as told, so
    /// the routes can be checked to *reach* it rather than to duplicate it.
    #[derive(Clone, Default)]
    struct RecordingAdmission {
        asked: Arc<std::sync::Mutex<Vec<String>>>,
        refusal: Option<String>,
    }

    impl super::BlockAdmission for RecordingAdmission {
        fn authenticate(&mut self, block: &NakamotoBlock) -> Result<(), String> {
            self.asked
                .lock()
                .expect("record")
                .push(block.block_id().to_string());
            self.refusal.clone().map_or(Ok(()), Err)
        }
    }

    /// An observer that keeps every body it is sent, at a real address, so the
    /// dispatch a route makes can be read back out of it.
    async fn recording_observer() -> (reqwest::Url, Received) {
        let received: Received = Arc::default();
        let state = received.clone();
        let app = Router::new()
            .route(
                "/{event}",
                axum::routing::post(
                    |axum::extract::State(state): axum::extract::State<Received>,
                     axum::extract::Path(event): axum::extract::Path<String>,
                     body: String| async move {
                        let payload = serde_json::from_str(&body).expect("a JSON payload");
                        state.lock().expect("record").push((event, payload));
                    },
                ),
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind observer");
        let address = listener.local_addr().expect("observer address");
        tokio::spawn(async move { axum::serve(listener, app).await });
        (
            reqwest::Url::parse(&format!("http://{address}/")).expect("observer URL"),
            received,
        )
    }

    /// An uploaded block goes through the validator a followed block goes
    /// through, and a node that refuses it does not hold it.
    ///
    /// The point is not that the check exists but that this route reaches the
    /// one check: a node that admits over its own API what it would refuse from
    /// a peer is forkable through its own API.
    #[tokio::test]
    async fn an_uploaded_block_is_refused_by_the_validator_that_refuses_a_followed_one() {
        let (view, blocks) = view_with_blocks(3);
        let refusing = RecordingAdmission {
            asked: Arc::default(),
            refusal: Some("unsupported Nakamoto block version 3".to_owned()),
        };
        let (blocks_out, mut offered) = mpsc::unbounded_channel();
        let state = RpcState::new(NETWORK)
            .with_block_admission(Arc::new(Mutex::new(refusing.clone())))
            .with_block_sink(blocks_out);
        state.publish(view.clone()).await;
        state.publish_executed(sealed_at(&blocks[1])).await;
        let upload = |block: &NakamotoBlock, app: Router| {
            let body = block.encode();
            async move {
                app.oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v3/blocks/upload")
                        .body(Body::from(body))
                        .expect("request"),
                )
                .await
                .expect("response")
            }
        };

        let refused = upload(&blocks[2], router(state.clone())).await;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
        assert_eq!(refusing.asked.lock().expect("record").len(), 1);
        assert!(
            offered.try_recv().is_err(),
            "a refused block was handed to the node anyway"
        );

        // The same block, from a node whose validator accepts it, is taken once
        // — and a block already executed is neither accepted again nor offered.
        let accepting = RecordingAdmission::default();
        let (blocks_out, mut offered) = mpsc::unbounded_channel();
        let state = RpcState::new(NETWORK)
            .with_block_admission(Arc::new(Mutex::new(accepting)))
            .with_block_sink(blocks_out);
        state.publish(view).await;
        state.publish_executed(sealed_at(&blocks[1])).await;

        let taken = body_json(upload(&blocks[2], router(state.clone())).await).await;
        assert_eq!(taken["accepted"], json!(true));
        assert_eq!(
            offered.try_recv().expect("the node was offered the block"),
            blocks[2]
        );

        let held = body_json(upload(&blocks[1], router(state)).await).await;
        assert_eq!(held["accepted"], json!(false));
        assert!(
            offered.try_recv().is_err(),
            "a block already executed was offered again"
        );
    }

    /// A node whose proposal route is fully wired: a validator that accepts, a
    /// real observer, a token, and an executed chain two of three blocks deep.
    struct ProposalNode {
        app: Router,
        blocks: Vec<NakamotoBlock>,
        received: Received,
        offered: mpsc::UnboundedReceiver<NakamotoBlock>,
    }

    async fn proposal_node() -> ProposalNode {
        let (view, blocks) = view_with_blocks(3);
        let (url, received) = recording_observer().await;
        let (blocks_out, offered) = mpsc::unbounded_channel();
        let state = RpcState::new(NETWORK)
            .with_block_admission(Arc::new(Mutex::new(RecordingAdmission::default())))
            .with_block_sink(blocks_out)
            .with_observers(EventDispatcher::new(vec![url]))
            .with_proposal_token("t0ken".to_owned());
        state.publish(view).await;
        state.publish_executed(sealed_at(&blocks[1])).await;
        ProposalNode {
            app: router(state),
            blocks,
            received,
            offered,
        }
    }

    impl ProposalNode {
        async fn propose(&self, body: Json, token: Option<&str>) -> axum::response::Response {
            let mut request = Request::builder()
                .method("POST")
                .uri("/v3/block_proposal")
                .header("content-type", "application/json");
            if let Some(token) = token {
                request = request.header("authorization", token);
            }
            self.app
                .clone()
                .oneshot(request.body(Body::from(body.to_string())).expect("request"))
                .await
                .expect("response")
        }

        /// The one verdict this node reached, once its observer has it.
        async fn verdict(&self) -> Json {
            for _ in 0..100 {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                let posts = self.received.lock().expect("record");
                if let Some((_, payload)) = posts
                    .iter()
                    .find(|(event, _)| event == "proposal_response")
                {
                    let payload = payload.clone();
                    drop(posts);
                    self.received.lock().expect("record").clear();
                    return payload;
                }
            }
            panic!("no verdict reached the observer");
        }
    }

    /// A proposal this node will not have is rejected by name, through the event
    /// observer, because that is where a stock signer reads the verdict from.
    #[tokio::test]
    async fn a_proposal_this_node_refuses_is_rejected_by_name() {
        let mut node = proposal_node().await;

        // No token, no proposal: unauthenticated, this route lets anyone make a
        // node execute a block of their choosing.
        let unauthorized = node
            .propose(json!({ "block": hex::encode(node.blocks[2].encode()) }), None)
            .await;
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        // A proposal for another chain is not a proposal at all.
        let response = node
            .propose(
                json!({
                    "block": hex::encode(node.blocks[2].encode()),
                    "chain_id": 0x1234_5678,
                }),
                Some("t0ken"),
            )
            .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let verdict = node.verdict().await;
        assert_eq!(verdict["result"], json!("Reject"));
        assert_eq!(verdict["reason_code"], json!("NetworkChainMismatch"));
        assert_eq!(
            verdict["signer_signature_hash"],
            json!(node.blocks[2].header.signer_signature_hash().to_string())
        );

        // A replay set is refused rather than ignored: ignoring it would validate
        // a different block than the one asked about.
        node.propose(
            json!({
                "block": hex::encode(node.blocks[2].encode()),
                "replay_txs": ["00"],
            }),
            Some("t0ken"),
        )
        .await;
        assert_eq!(
            node.verdict().await["reason_code"],
            json!("InvalidTransactionReplay")
        );

        // And a block whose parent this node has not executed cannot be judged
        // against anything: its state root is over a state this node has not got.
        let orphan = synthetic_block(
            30,
            StacksBlockId::from_bytes([30; 32]),
            ConsensusHash::from_bytes([2; 20]),
        );
        node.propose(
            json!({ "block": hex::encode(orphan.encode()) }),
            Some("t0ken"),
        )
        .await;
        assert_eq!(node.verdict().await["reason_code"], json!("UnknownParent"));

        assert!(
            node.offered.try_recv().is_err(),
            "a refused proposal was admitted anyway"
        );
    }

    /// The two verdicts a well-formed proposal can get: `Ok` for a block this node
    /// already executed, and a refusal for one it has only admitted.
    #[tokio::test]
    async fn a_proposal_is_vouched_for_only_once_this_node_has_executed_it() {
        let mut node = proposal_node().await;

        // Already executed, so its state root was already checked. Zero cost is
        // how stacks-core itself reports having executed nothing to answer.
        node.propose(
            json!({ "block": hex::encode(node.blocks[1].encode()) }),
            Some("t0ken"),
        )
        .await;
        let verdict = node.verdict().await;
        assert_eq!(verdict["result"], json!("Ok"));
        assert_eq!(verdict["cost"]["runtime"], json!(0));
        assert_eq!(verdict["size"], json!(node.blocks[1].encode().len()));

        // The extension of the tip is admitted for execution and *refused* rather
        // than vouched for: nano validates a state root by executing the block,
        // and it has not executed this one. A signer must not sign on a
        // validation that did not happen.
        node.propose(
            json!({ "block": hex::encode(node.blocks[2].encode()) }),
            Some("t0ken"),
        )
        .await;
        let verdict = node.verdict().await;
        assert_eq!(verdict["result"], json!("Reject"));
        assert_eq!(verdict["reason_code"], json!("ChainstateError"));
        assert_eq!(
            node.offered.try_recv().expect("the block was admitted"),
            node.blocks[2]
        );
    }

    /// A proposal nobody could be told the result of is a request nobody can act
    /// on, which is the one thing stacks-core refuses outright.
    #[tokio::test]
    async fn a_proposal_is_refused_when_no_observer_can_be_told_the_result() {
        let state = RpcState::new(NETWORK)
            .with_proposal_token("t0ken".to_owned());
        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v3/block_proposal")
                    .header("authorization", "t0ken")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({ "block": "00" }).to_string()))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(response).await["result"], json!("Error"));
    }

    /// A chunk becomes news when a slot takes it, and only then.
    #[tokio::test]
    async fn an_accepted_chunk_is_announced_and_a_refused_one_is_not() {
        let writer = key(b"signer");
        let (url, received) = recording_observer().await;
        let state = RpcState::new(NETWORK).with_observers(EventDispatcher::new(vec![url]));
        state.stackerdb().write().await.configure(
            "ST000000000000000000002AMW42H.signers-0-1",
            vec![nano_primitives::hash160(
                &writer.public_key().to_bytes_compressed(),
            )],
        );
        let app = router(state);
        let put = |chunk: nano_stackerdb::Chunk, app: Router| async move {
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/stackerdb/ST000000000000000000002AMW42H/signers-0-1/chunks")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "slot_id": chunk.slot_id,
                            "slot_version": chunk.slot_version,
                            "sig": hex::encode(chunk.signature.as_bytes()),
                            "data": hex::encode(&chunk.data),
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response")
        };

        let mut accepted = nano_stackerdb::Chunk::new(0, 1, b"a response".to_vec());
        accepted.sign(&writer).expect("sign chunk");
        assert_eq!(
            body_json(put(accepted.clone(), app.clone()).await).await["accepted"],
            json!(true)
        );
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if !received.lock().expect("record").is_empty() {
                break;
            }
        }
        let announced = received.lock().expect("record").clone();
        let [(event, payload)] = announced.as_slice() else {
            panic!("exactly one chunk was announced, got {announced:?}");
        };
        assert_eq!(event, "stackerdb_chunks");
        // Clarity's own identifier, not the `address.name` the route is keyed
        // by: the boot address is version 26 and twenty zero bytes.
        assert_eq!(payload["contract_id"]["name"], json!("signers-0-1"));
        assert_eq!(payload["contract_id"]["issuer"][0], json!(26));
        assert_eq!(payload["contract_id"]["issuer"][1], json!(vec![0u8; 20]));
        assert_eq!(
            payload["modified_slots"][0]["data"],
            json!(hex::encode(b"a response"))
        );

        // A chunk a slot refuses changed nothing, so there is nothing to say.
        let stranger = key(b"stranger");
        let mut forged = nano_stackerdb::Chunk::new(0, 2, b"forged".to_vec());
        forged.sign(&stranger).expect("sign chunk");
        assert_eq!(
            body_json(put(forged, app).await).await["accepted"],
            json!(false)
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(received.lock().expect("record").len(), 1);
    }

    /// The reward set this node derives and serves is one it can read back, which
    /// is what makes a nano node able to attest another nano node's checkpoint.
    #[tokio::test]
    async fn a_served_reward_set_parses_back_through_this_node_s_own_client() {
        let signers: Vec<super::RewardSetSigner> = [b"one".as_slice(), b"two".as_slice()]
            .iter()
            .map(|seed| super::RewardSetSigner {
                signing_key: key(seed).public_key().to_bytes_compressed(),
                stacked_amount: 0,
                weight: 4,
            })
            .collect();
        let state = RpcState::new(NETWORK);
        state
            .publish_stacker_set(
                140,
                super::stacker_set_payload(&signers, 50_000_000_000, None),
            )
            .await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the RPC");
        let address = listener.local_addr().expect("an address");
        tokio::spawn(async move { super::serve(listener, state).await });

        let client = SyncClient::new(
            Url::parse(&format!("http://{address}/")).expect("a URL"),
        )
        .expect("a client");
        let served = client.stacker_set(140).await.expect("the served reward set");
        assert_eq!(served.pox_ustx_threshold, 50_000_000_000);
        assert_eq!(served.signer_set.signers().len(), 2);
        assert_eq!(served.signer_set.weights(), vec![4, 4]);
        assert!(client.stacker_set(141).await.is_err());
    }

    #[tokio::test]
    #[ignore = "requires a running Hacknet node on localhost"]
    async fn serves_a_validated_hacknet_block() {
        let client = SyncClient::new(Url::parse("http://127.0.0.1:20443/").expect("valid URL"))
            .expect("create client");
        let mut node = Node::new(client);
        node.poll().await.expect("follow Hacknet");
        let block_id = node
            .latest_tenure()
            .expect("followed tenure")
            .tip_block_id
            .to_string();
        let consensus_hash = node
            .view()
            .expect("node view")
            .tenures
            .last()
            .expect("followed tenure")
            .sortition
            .consensus_hash
            .to_string();
        let state = RpcState::new(NETWORK);
        state.publish(node.view().expect("node view")).await;
        let app = router(state);

        let info = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v2/info")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(info.status(), StatusCode::OK);

        let block = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v3/blocks/{block_id}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(block.status(), StatusCode::OK);

        let sortition = app
            .oneshot(
                Request::builder()
                    .uri(format!("/v3/sortitions/consensus/{consensus_hash}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(sortition.status(), StatusCode::OK);
    }
}
