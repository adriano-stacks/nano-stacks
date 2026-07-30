mod chain;
mod events;
mod stackerdb;

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
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
use nano_node::NodeView;
use nano_primitives::Network;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};
use tokio_stream::{StreamExt, wrappers::BroadcastStream};

pub use chain::{AccountEntry, ChainAccess, ChainAccessError, ReadOnlyCall};
pub use events::{
    BlockEventContext, DEFAULT_DISPATCH_ATTEMPTS, EventDispatcher, EventKind, MaturedReward,
    RewardSetEvent, RewardSetSigner, mined_nakamoto_block_payload, new_block_payload,
    new_burn_block_payload, stackerdb_chunks_payload,
};
pub use stackerdb::{ChunkRefusal, StackerDbStore};

/// The validated node state exposed by the public HTTP API.
#[derive(Clone)]
pub struct RpcState {
    view: Arc<RwLock<Option<NodeView>>>,
    events: broadcast::Sender<NodeEvent>,
    /// The executed Clarity state, when the node runs one.
    chain: Option<Arc<Mutex<dyn ChainAccess>>>,
    mempool: Option<Arc<Mutex<Mempool>>>,
    /// The reward sets this node derived, keyed by cycle.
    stacker_sets: Arc<RwLock<BTreeMap<u64, Value>>>,
    /// The `StackerDB` contracts this node replicates.
    stackerdb: Arc<RwLock<StackerDbStore>>,
    /// Where an accepted block upload or proposal is handed to the node.
    blocks: Option<mpsc::UnboundedSender<NakamotoBlock>>,
    /// The `authorization` header `/v3/block_proposal` demands.
    proposal_token: Option<String>,
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
    /// Construct initially unavailable public state.
    #[must_use]
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            view: Arc::new(RwLock::new(None)),
            events,
            chain: None,
            mempool: None,
            stacker_sets: Arc::new(RwLock::new(BTreeMap::new())),
            stackerdb: Arc::new(RwLock::new(StackerDbStore::new())),
            blocks: None,
            proposal_token: None,
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

    /// Publish a fully validated snapshot and notify subscribers about a new tip.
    pub async fn publish(&self, view: NodeView) {
        let event = NodeEvent::from_view(&view);
        let changed = self
            .view
            .read()
            .await
            .as_ref()
            .and_then(NodeEvent::from_view)
            != event;
        *self.view.write().await = Some(view);
        if changed && let Some(event) = event {
            let _ = self.events.send(event);
        }
    }
}

impl Default for RpcState {
    fn default() -> Self {
        Self::new()
    }
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
        .route("/v3/sortitions/consensus/{consensus_hash}", get(sortition))
        .route("/v3/stacker_set/{cycle}", get(stacker_set))
        .route("/v3/tenures/info", get(tenure_info))
        .route("/v3/tenures/{start_block_id}", get(tenure))
        .route("/v3/blocks/upload", post(upload_block))
        .route("/v3/blocks/{block_id}", get(block))
        .route("/v3/block_proposal", post(block_proposal))
        .route("/events", get(events))
        .with_state(state)
}

/// Serve the public RPC until the listener is stopped.
pub async fn serve(listener: tokio::net::TcpListener, state: RpcState) -> std::io::Result<()> {
    axum::serve(listener, router(state)).await
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

    /// Whether the latest validated view already carries this block.
    async fn holds_block(&self, block: &NakamotoBlock) -> bool {
        self.view.read().await.as_ref().is_some_and(|view| {
            view.tenures
                .iter()
                .flat_map(|tenure| &tenure.blocks)
                .any(|known| known.block_id() == block.block_id())
        })
    }

    fn offer_block(&self, block: NakamotoBlock) -> Result<(), RpcError> {
        self.blocks
            .as_ref()
            .ok_or(RpcError::Unavailable)?
            .send(block)
            .map_err(|_| RpcError::Unavailable)
    }
}

async fn view(state: &RpcState) -> Result<NodeView, RpcError> {
    state.view.read().await.clone().ok_or(RpcError::Unavailable)
}

async fn node_info(State(state): State<RpcState>) -> Result<axum::Json<NodeInfoWire>, RpcError> {
    let info = view(&state).await?.node_info;
    Ok(axum::Json(NodeInfoWire {
        burn_block_height: info.bitcoin_height,
        stacks_tip_height: info.stacks_height,
        stacks_tip: info.stacks_tip.to_string(),
        stacks_tip_consensus_hash: info.consensus_hash.to_string(),
        network_id: info.network_id,
    }))
}

async fn pox_info(State(state): State<RpcState>) -> Result<axum::Json<PoxInfoWire>, RpcError> {
    let view = view(&state).await?;
    let network = Network::from_chain_id(view.node_info.network_id);
    let pox = view.pox_info;
    Ok(axum::Json(PoxInfoWire {
        first_burnchain_block_height: pox.first_bitcoin_height,
        current_burnchain_block_height: pox.bitcoin_height,
        prepare_phase_block_length: pox.prepare_phase_length,
        reward_phase_block_length: pox.reward_phase_length,
        reward_slots: pox.reward_slots,
        rejection_fraction: pox.rejection_fraction,
        contract_versions: pox
            .pox_5_activation_height
            .map(|height| {
                vec![PoxContractVersionWire {
                    activation_burnchain_block_height: height,
                    contract_id: network.boot_contract_id("pox-5"),
                }]
            })
            .unwrap_or_default(),
    }))
}

async fn tenure_info(
    State(state): State<RpcState>,
) -> Result<axum::Json<TenureInfoWire>, RpcError> {
    let latest = view(&state)
        .await?
        .tenures
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
    let sortition = view(&state)
        .await?
        .tenures
        .into_iter()
        .map(|tenure| tenure.sortition)
        .find(|sortition| sortition.consensus_hash.to_string() == consensus_hash)
        .ok_or(RpcError::NotFound)?;
    Ok(axum::Json(vec![SortitionInfoWire::from(sortition)]))
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
    let admission = mempool.submit(transaction, &ExecutedTip::new(&mut *chain), now_seconds());
    drop(chain);
    drop(mempool);
    admission.map_err(|rejection| RpcError::Rejected(rejection.into_json(txid)))?;
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
    let accepted = state.stackerdb.write().await.put(&contract_id, chunk);
    Ok(axum::Json(match accepted {
        Ok(()) => json!({ "accepted": true, "metadata": metadata }),
        Err(refusal) => json!({
            "accepted": false,
            "reason": refusal.reason(),
            "code": refusal.code(),
        }),
    }))
}

async fn upload_block(
    State(state): State<RpcState>,
    body: Bytes,
) -> Result<axum::Json<BlockUploadWire>, RpcError> {
    let block = NakamotoBlock::decode(&body)
        .map_err(|error| RpcError::BadRequest(format!("failed to decode block: {error}")))?;
    let response = BlockUploadWire {
        stacks_block_id: format!("0x{}", block.block_id()),
        // A node that already holds the block does not re-accept it.
        accepted: !state.holds_block(&block).await,
    };
    state.offer_block(block)?;
    Ok(axum::Json(response))
}

async fn block_proposal(
    State(state): State<RpcState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, RpcError> {
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
    let proposal: BlockProposalWire = serde_json::from_slice(&body)
        .map_err(|error| RpcError::BadRequest(format!("failed to decode proposal: {error}")))?;
    let block = NakamotoBlock::decode(
        &hex::decode(proposal.block.trim_start_matches("0x"))
            .map_err(|error| RpcError::BadRequest(format!("invalid block hex: {error}")))?,
    )
    .map_err(|error| RpcError::BadRequest(format!("failed to decode block: {error}")))?;
    state.offer_block(block)?;
    Ok(StatusCode::ACCEPTED)
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

async fn tenure(
    State(state): State<RpcState>,
    Path(start_block_id): Path<String>,
    Query(query): Query<TenureQuery>,
) -> Result<RawBlockStream, RpcError> {
    let tenure = view(&state)
        .await?
        .tenures
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
    Path(block_id): Path<String>,
) -> Result<RawBlockStream, RpcError> {
    let block = view(&state)
        .await?
        .tenures
        .into_iter()
        .flat_map(|tenure| tenure.blocks)
        .find(|block| block.block_id().to_string() == block_id)
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

#[derive(Deserialize)]
struct BlockProposalWire {
    block: String,
}

#[derive(Serialize)]
struct NodeInfoWire {
    burn_block_height: u64,
    stacks_tip_height: u64,
    stacks_tip: String,
    stacks_tip_consensus_hash: String,
    network_id: u32,
}

#[derive(Serialize)]
struct PoxInfoWire {
    first_burnchain_block_height: u64,
    current_burnchain_block_height: u64,
    prepare_phase_block_length: u32,
    reward_phase_block_length: u32,
    reward_slots: u32,
    rejection_fraction: Option<u64>,
    contract_versions: Vec<PoxContractVersionWire>,
}

#[derive(Serialize)]
struct PoxContractVersionWire {
    activation_burnchain_block_height: u32,
    contract_id: String,
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
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use nano_node::{Node, NodeView};
    use nano_primitives::{
        BitcoinHeaderHash, BlockHeaderHash, ConsensusHash, SortitionId, StacksBlockId,
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
        AccountEntry, ChainAccess, ChainAccessError, ReadOnlyCall, Router, RpcState, router,
    };

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
        let app = router(RpcState::new().with_chain(chain(&[(address(&sender), entry)], None)));

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
        let app = router(RpcState::new().with_chain(chain(&[], Some(Value::UInt(3)))));
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

        let failed = router(RpcState::new().with_chain(chain(&[], None)));
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
            RpcState::new()
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
        let state = RpcState::new();
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
        let state = RpcState::new();
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
        let app = router(RpcState::new());

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

    #[tokio::test]
    async fn publishes_one_event_per_new_tip() {
        let state = RpcState::new();
        let mut events = state.events.subscribe();
        state.publish(captured_view()).await;
        let event = events.try_recv().expect("new tip event");
        assert_eq!(event.stacks_height, 12);
        assert_eq!(event.bitcoin_height, 11);

        state.publish(captured_view()).await;
        assert!(events.try_recv().is_err());
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
        let state = RpcState::new();
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
