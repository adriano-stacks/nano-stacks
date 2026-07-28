#![forbid(unsafe_code)]

use std::sync::Arc;

use axum::{
    Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use nano_node::{Node, NodeView};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Shared local node state served by the HTTP API.
pub type SharedNode = Arc<RwLock<Node>>;

/// Build the read-only RPC routes backed by the node's latest validated view.
pub fn router(node: SharedNode) -> Router {
    Router::new()
        .route("/v2/info", get(node_info))
        .route("/v2/pox", get(pox_info))
        .route("/v3/tenures/info", get(tenure_info))
        .route("/v3/tenures/{start_block_id}", get(tenure))
        .route("/v3/blocks/{block_id}", get(block))
        .with_state(node)
}

/// Serve the public RPC until the listener is stopped.
pub async fn serve(listener: tokio::net::TcpListener, node: SharedNode) -> std::io::Result<()> {
    axum::serve(listener, router(node)).await
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

async fn view(node: &SharedNode) -> Result<NodeView, RpcError> {
    node.read().await.view().ok_or(RpcError::Unavailable)
}

async fn node_info(State(node): State<SharedNode>) -> Result<axum::Json<NodeInfoWire>, RpcError> {
    let info = view(&node).await?.node_info;
    Ok(axum::Json(NodeInfoWire {
        burn_block_height: info.bitcoin_height,
        stacks_tip_height: info.stacks_height,
        stacks_tip: info.stacks_tip.to_string(),
        stacks_tip_consensus_hash: info.consensus_hash.to_string(),
        network_id: info.network_id,
    }))
}

async fn pox_info(State(node): State<SharedNode>) -> Result<axum::Json<PoxInfoWire>, RpcError> {
    let pox = view(&node).await?.pox_info;
    Ok(axum::Json(PoxInfoWire {
        first_burnchain_block_height: pox.first_bitcoin_height,
        current_burnchain_block_height: pox.bitcoin_height,
        prepare_phase_block_length: pox.prepare_phase_length,
        reward_phase_block_length: pox.reward_phase_length,
        reward_slots: pox.reward_slots,
        rejection_fraction: pox.rejection_fraction,
    }))
}

async fn tenure_info(
    State(node): State<SharedNode>,
) -> Result<axum::Json<TenureInfoWire>, RpcError> {
    let latest = view(&node)
        .await?
        .tenures
        .last()
        .ok_or(RpcError::Unavailable)?
        .info
        .clone();
    Ok(axum::Json(TenureInfoWire::from(latest)))
}

#[derive(Deserialize)]
struct TenureQuery {
    stop: Option<String>,
}

async fn tenure(
    State(node): State<SharedNode>,
    Path(start_block_id): Path<String>,
    Query(query): Query<TenureQuery>,
) -> Result<RawBlockStream, RpcError> {
    let tenure = view(&node)
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
    State(node): State<SharedNode>,
    Path(block_id): Path<String>,
) -> Result<RawBlockStream, RpcError> {
    let block = view(&node)
        .await?
        .tenures
        .into_iter()
        .flat_map(|tenure| tenure.blocks)
        .find(|block| block.block_id().to_string() == block_id)
        .ok_or(RpcError::NotFound)?;
    Ok(RawBlockStream(block.encode()))
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use nano_node::Node;
    use nano_sync::SyncClient;
    use reqwest::Url;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    use super::router;

    #[tokio::test]
    async fn rejects_requests_until_the_node_has_a_validated_view() {
        let client = SyncClient::new(Url::parse("http://127.0.0.1:20443/").expect("valid URL"))
            .expect("create client");
        let app = router(Arc::new(RwLock::new(Node::new(client))));

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
        let app = router(Arc::new(RwLock::new(node)));

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
            .oneshot(
                Request::builder()
                    .uri(format!("/v3/blocks/{block_id}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(block.status(), StatusCode::OK);
    }
}
