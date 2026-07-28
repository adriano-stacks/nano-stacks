#![forbid(unsafe_code)]

use std::{convert::Infallible, sync::Arc};

use axum::{
    Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, Sse},
    },
    routing::get,
};
use nano_node::NodeView;
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, broadcast};
use tokio_stream::{StreamExt, wrappers::BroadcastStream};

/// The validated node state exposed by the public HTTP API.
#[derive(Clone, Debug)]
pub struct RpcState {
    view: Arc<RwLock<Option<NodeView>>>,
    events: broadcast::Sender<NodeEvent>,
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
        }
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

/// Build the read-only RPC routes backed by the node's latest validated view.
pub fn router(state: RpcState) -> Router {
    Router::new()
        .route("/v2/info", get(node_info))
        .route("/v2/pox", get(pox_info))
        .route("/v3/sortitions/consensus/{consensus_hash}", get(sortition))
        .route("/v3/tenures/info", get(tenure_info))
        .route("/v3/tenures/{start_block_id}", get(tenure))
        .route("/v3/blocks/{block_id}", get(block))
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
}

impl IntoResponse for RpcError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::NotFound => StatusCode::NOT_FOUND,
        };
        status.into_response()
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
    let pox = view(&state).await?.pox_info;
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
                    contract_id: "ST000000000000000000002AMW42H.pox-5".to_owned(),
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

    use super::{RpcState, router};

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
