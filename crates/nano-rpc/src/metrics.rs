use std::{
    fmt::Write as _,
    sync::{Arc, atomic::AtomicI64},
};

use axum::{Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use prometheus_client::{
    encoding::{EncodeLabelSet, EncodeLabelValue, LabelValueEncoder, text::encode},
    metrics::{counter::Counter, family::Family, gauge::Gauge},
    registry::Registry,
};

use crate::QueueReport;

type IntGauge = Gauge<i64, AtomicI64>;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum RefusalReason {
    CompilerGap,
    RootMismatch,
    Signature,
    MissingContext,
    Other,
}

impl EncodeLabelValue for RefusalReason {
    fn encode(&self, encoder: &mut LabelValueEncoder<'_>) -> std::fmt::Result {
        encoder.write_str(match self {
            Self::CompilerGap => "compiler_gap",
            Self::RootMismatch => "root_mismatch",
            Self::Signature => "signature",
            Self::MissingContext => "missing_context",
            Self::Other => "other",
        })
    }
}

impl RefusalReason {
    fn classify(message: &str) -> Self {
        let message = message.to_ascii_lowercase();
        if message.contains("compil")
            || message.contains("wasm")
            || message.contains("module that will not load")
            || message.contains("contract analysis failed")
        {
            Self::CompilerGap
        } else if message.contains("root")
            && (message.contains("mismatch")
                || message.contains("does not match")
                || message.contains("disagree"))
        {
            Self::RootMismatch
        } else if message.contains("signature")
            || message.contains("signer weight")
            || message.contains("vrf")
            || message.contains("leader key")
        {
            Self::Signature
        } else if message.contains("missing")
            || message.contains("absent")
            || message.contains("unknown parent")
            || message.contains("has not executed")
            || message.contains("no snapshot")
            || message.contains("cannot check")
            || message.contains("unavailable")
            || message.contains("no signer set")
            || message.contains("does not hold")
            || message.contains("runs no chain")
        {
            Self::MissingContext
        } else {
            Self::Other
        }
    }

    fn consensus(message: &str) -> Option<Self> {
        match Self::classify(message) {
            Self::Other => None,
            reason => Some(reason),
        }
    }
}

#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct RefusalLabels {
    reason: RefusalReason,
}

struct RefusalCounters {
    compiler_gap: Counter,
    root_mismatch: Counter,
    signature: Counter,
    missing_context: Counter,
    other: Counter,
}

impl RefusalCounters {
    fn register(registry: &mut Registry) -> Self {
        let family = Family::<RefusalLabels, Counter>::default();
        let counter = |reason| family.get_or_create(&RefusalLabels { reason }).clone();
        let counters = Self {
            compiler_gap: counter(RefusalReason::CompilerGap),
            root_mismatch: counter(RefusalReason::RootMismatch),
            signature: counter(RefusalReason::Signature),
            missing_context: counter(RefusalReason::MissingContext),
            other: counter(RefusalReason::Other),
        };
        registry.register(
            "block_refusals",
            "Blocks refused by this node, classified at the refusal boundary.",
            family,
        );
        counters
    }

    fn record(&self, reason: RefusalReason) {
        match reason {
            RefusalReason::CompilerGap => self.compiler_gap.inc(),
            RefusalReason::RootMismatch => self.root_mismatch.inc(),
            RefusalReason::Signature => self.signature.inc(),
            RefusalReason::MissingContext => self.missing_context.inc(),
            RefusalReason::Other => self.other.inc(),
        };
    }
}

struct SyncCounters {
    peer_failovers: Counter,
    rounds_unanswered: Counter,
    pushed_blocks_accepted: Counter,
    pushed_blocks_refused: Counter,
}

impl SyncCounters {
    fn register(registry: &mut Registry) -> Self {
        Self {
            peer_failovers: counter(
                registry,
                "peer_failovers",
                "Times the follower selected a different serving peer.",
            ),
            rounds_unanswered: counter(
                registry,
                "sync_rounds_unanswered",
                "Follow polls the selected peer did not answer.",
            ),
            pushed_blocks_accepted: counter(
                registry,
                "pushed_blocks_accepted",
                "Peer-pushed blocks accepted by local authentication.",
            ),
            pushed_blocks_refused: counter(
                registry,
                "pushed_blocks_refused",
                "Peer-pushed blocks refused by local authentication.",
            ),
        }
    }
}

struct Inner {
    registry: Registry,
    block_refusals: RefusalCounters,
    sync: SyncCounters,
    executed_stacks_height: IntGauge,
    followed_stacks_height: IntGauge,
    selected_stacks_height: IntGauge,
    burn_height: IntGauge,
    last_sealed_timestamp_seconds: IntGauge,
    serving_peers: IntGauge,
    p2p_connected: IntGauge,
    p2p_known: IntGauge,
    staged_blocks: IntGauge,
    relay_offered: IntGauge,
    relay_announcing: IntGauge,
    relay_dropped: IntGauge,
    queued_blocks: IntGauge,
    queued_proposals: IntGauge,
    queued_stackerdb_chunks: IntGauge,
    queued_transactions: IntGauge,
}

/// Metrics updated at the node's existing publication boundaries.
#[derive(Clone)]
pub struct NodeMetrics(Arc<Inner>);

impl std::fmt::Debug for NodeMetrics {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NodeMetrics")
            .finish_non_exhaustive()
    }
}

impl Default for NodeMetrics {
    fn default() -> Self {
        let mut registry = Registry::with_prefix("nano");
        let block_refusals = RefusalCounters::register(&mut registry);
        let sync = SyncCounters::register(&mut registry);
        let executed_stacks_height = gauge(
            &mut registry,
            "executed_stacks_height",
            "Latest Stacks height this node executed and sealed.",
        );
        let followed_stacks_height = gauge(
            &mut registry,
            "followed_stacks_height",
            "Latest Stacks height advertised by the followed peers.",
        );
        let selected_stacks_height = gauge(
            &mut registry,
            "selected_stacks_height",
            "Latest Stacks height selected by local fork choice.",
        );
        let burn_height = gauge(
            &mut registry,
            "burn_height",
            "Bitcoin height of the latest locally executed Stacks block.",
        );
        let last_sealed_timestamp_seconds = gauge(
            &mut registry,
            "last_sealed_timestamp_seconds",
            "Unix timestamp when the latest Stacks block was sealed.",
        );
        let serving_peers = gauge(
            &mut registry,
            "serving_peers",
            "Peers currently serving the follower.",
        );
        let p2p_connected = gauge(&mut registry, "p2p_connected", "Live binary P2P sessions.");
        let p2p_known = gauge(
            &mut registry,
            "p2p_known",
            "Peers known to the binary P2P table.",
        );
        let staged_blocks = gauge(
            &mut registry,
            "staged_blocks",
            "Blocks acquired but not yet executed.",
        );
        let relay_offered = gauge(
            &mut registry,
            "relay_offered",
            "Peer pushes waiting for local validation.",
        );
        let relay_announcing = gauge(
            &mut registry,
            "relay_announcing",
            "Accepted relay items waiting to be announced.",
        );
        let relay_dropped = gauge(
            &mut registry,
            "relay_dropped",
            "Relay items dropped because a bounded queue was full.",
        );
        let queued_blocks = gauge(
            &mut registry,
            "queued_blocks",
            "RPC blocks waiting for the follow loop.",
        );
        let queued_proposals = gauge(
            &mut registry,
            "queued_proposals",
            "Proposals waiting for the hosted validator.",
        );
        let queued_stackerdb_chunks = gauge(
            &mut registry,
            "queued_stackerdb_chunks",
            "StackerDB chunks waiting for the replication loop.",
        );
        let queued_transactions = gauge(
            &mut registry,
            "queued_transactions",
            "Transactions waiting for the follow loop.",
        );
        Self(Arc::new(Inner {
            registry,
            block_refusals,
            sync,
            executed_stacks_height,
            followed_stacks_height,
            selected_stacks_height,
            burn_height,
            last_sealed_timestamp_seconds,
            serving_peers,
            p2p_connected,
            p2p_known,
            staged_blocks,
            relay_offered,
            relay_announcing,
            relay_dropped,
            queued_blocks,
            queued_proposals,
            queued_stackerdb_chunks,
            queued_transactions,
        }))
    }
}

impl NodeMetrics {
    /// Record a block rejected at an explicit validation boundary.
    pub fn record_block_refusal(&self, message: &str) {
        self.0
            .block_refusals
            .record(RefusalReason::classify(message));
    }

    /// Record a catch-up failure only when it names a consensus refusal.
    pub fn record_consensus_refusal(&self, message: &str) {
        if let Some(reason) = RefusalReason::consensus(message) {
            self.0.block_refusals.record(reason);
        }
    }

    /// Record a follower moving to another selected peer.
    pub fn record_peer_failover(&self) {
        self.0.sync.peer_failovers.inc();
    }

    /// Record a selected peer failing to answer the follow poll.
    pub fn record_sync_round_unanswered(&self) {
        self.0.sync.rounds_unanswered.inc();
    }

    /// Record peer-pushed blocks authenticated during one follow round.
    pub fn record_pushed_blocks(&self, accepted: usize, refused: usize) {
        self.0.sync.pushed_blocks_accepted.inc_by(as_u64(accepted));
        self.0.sync.pushed_blocks_refused.inc_by(as_u64(refused));
    }

    pub(crate) fn publish_executed(&self, stacks_height: u64, burn_height: u64, now: u64) {
        self.0.executed_stacks_height.set(as_i64(stacks_height));
        self.0.burn_height.set(as_i64(burn_height));
        self.0.last_sealed_timestamp_seconds.set(as_i64(now));
    }

    pub(crate) fn publish_followed(&self, height: u64) {
        self.0.followed_stacks_height.set(as_i64(height));
    }

    pub(crate) fn publish_selected(&self, height: u64) {
        self.0.selected_stacks_height.set(as_i64(height));
    }

    pub(crate) fn publish_peers(&self, serving: usize, connected: usize, known: usize) {
        self.0.serving_peers.set(as_i64(serving));
        self.0.p2p_connected.set(as_i64(connected));
        self.0.p2p_known.set(as_i64(known));
    }

    pub(crate) fn publish_queues(&self, queues: &QueueReport) {
        set_option(&self.0.staged_blocks, queues.staged_blocks);
        set_option(&self.0.relay_offered, queues.relay_offered);
        set_option(&self.0.relay_announcing, queues.relay_announcing);
        set_option(&self.0.relay_dropped, queues.relay_dropped);
        set_option(&self.0.queued_blocks, queues.queued_blocks);
        set_option(&self.0.queued_proposals, queues.queued_proposals);
        set_option(
            &self.0.queued_stackerdb_chunks,
            queues.queued_stackerdb_chunks,
        );
        set_option(&self.0.queued_transactions, queues.queued_transactions);
    }

    fn encode(&self) -> Result<String, std::fmt::Error> {
        let mut body = String::new();
        encode(&mut body, &self.0.registry)?;
        Ok(body)
    }
}

fn gauge(registry: &mut Registry, name: &'static str, help: &'static str) -> IntGauge {
    let gauge = IntGauge::default();
    registry.register(name, help, gauge.clone());
    gauge
}

fn counter(registry: &mut Registry, name: &'static str, help: &'static str) -> Counter {
    let counter = Counter::default();
    registry.register(name, help, counter.clone());
    counter
}

fn set_option<T>(gauge: &IntGauge, value: Option<T>)
where
    T: TryInto<i64>,
{
    if let Some(value) = value.and_then(|value| value.try_into().ok()) {
        gauge.set(value);
    }
}

fn as_i64<T>(value: T) -> i64
where
    T: TryInto<i64>,
{
    value.try_into().unwrap_or(i64::MAX)
}

fn as_u64<T>(value: T) -> u64
where
    T: TryInto<u64>,
{
    value.try_into().unwrap_or(u64::MAX)
}

/// Build the separately bound observability surface.
pub fn router(metrics: NodeMetrics) -> Router {
    Router::new()
        .route("/metrics", get(scrape))
        .with_state(metrics)
}

/// Serve metrics until the listener is stopped.
pub async fn serve(listener: tokio::net::TcpListener, metrics: NodeMetrics) -> std::io::Result<()> {
    axum::serve(listener, router(metrics)).await
}

async fn scrape(State(metrics): State<NodeMetrics>) -> impl IntoResponse {
    match metrics.encode() {
        Ok(body) => (
            [(
                axum::http::header::CONTENT_TYPE,
                "application/openmetrics-text; version=1.0.0; charset=utf-8",
            )],
            body,
        )
            .into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt as _;

    use super::{RefusalReason, router};
    use crate::{PeerReport, QueueReport, RpcState, SealedTip, SelectedTip};
    use nano_primitives::{BlockHeaderHash, ConsensusHash, Network, StacksBlockId, TrieHash};

    #[tokio::test]
    async fn metrics_are_well_formed_and_name_the_three_chain_heights() {
        let state = RpcState::new(Network::MAINNET);
        let metrics = state.metrics();
        metrics.record_block_refusal("contract compilation failed");
        metrics.record_block_refusal("state root mismatch");
        metrics.record_block_refusal("signer signature is invalid");
        metrics.record_block_refusal("the reward set is absent");
        metrics.record_block_refusal("proposal names the wrong chain");
        metrics.record_consensus_refusal("the peer timed out");
        metrics.record_peer_failover();
        metrics.record_sync_round_unanswered();
        metrics.record_pushed_blocks(3, 2);
        state.publish_followed_height(12).await;
        state
            .publish_selected(SelectedTip {
                stacks_height: 9,
                stacks_tip: StacksBlockId::from_bytes([9; 32]),
                peer: "http://peer.example:20443/".to_owned(),
            })
            .await;
        state
            .publish_executed(
                SealedTip {
                    stacks_height: 4,
                    stacks_tip: StacksBlockId::from_bytes([4; 32]),
                    stacks_block_hash: BlockHeaderHash::from_bytes([5; 32]),
                    consensus_hash: ConsensusHash::from_bytes([6; 20]),
                    bitcoin_height: 3,
                    state_index_root: TrieHash::from_bytes([7; 32]),
                },
                Vec::new(),
            )
            .await;
        state
            .publish_peers(PeerReport {
                fetching: vec!["one".to_owned(), "two".to_owned(), "three".to_owned()],
                p2p_connected: 5,
                p2p_known: 8,
            })
            .await;
        state
            .publish_queues(QueueReport {
                staged_blocks: Some(7),
                ..QueueReport::default()
            })
            .await;

        let response = router(metrics)
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let body = std::str::from_utf8(&body).expect("UTF-8 metrics");
        for sample in [
            "nano_followed_stacks_height 12",
            "nano_selected_stacks_height 9",
            "nano_executed_stacks_height 4",
            "nano_burn_height 3",
            "nano_serving_peers 3",
            "nano_staged_blocks 7",
            "nano_block_refusals_total{reason=\"compiler_gap\"} 1",
            "nano_block_refusals_total{reason=\"root_mismatch\"} 1",
            "nano_block_refusals_total{reason=\"signature\"} 1",
            "nano_block_refusals_total{reason=\"missing_context\"} 1",
            "nano_block_refusals_total{reason=\"other\"} 1",
            "nano_peer_failovers_total 1",
            "nano_sync_rounds_unanswered_total 1",
            "nano_pushed_blocks_accepted_total 3",
            "nano_pushed_blocks_refused_total 2",
            "# EOF",
        ] {
            assert!(body.contains(sample), "missing {sample:?} in {body}");
        }
    }

    #[test]
    fn block_refusals_are_classified_into_bounded_reasons() {
        for (message, expected) in [
            ("Wasm module will not load", RefusalReason::CompilerGap),
            ("state root does not match", RefusalReason::RootMismatch),
            ("signer weight is too low", RefusalReason::Signature),
            ("unknown parent block", RefusalReason::MissingContext),
            ("proposal names another chain", RefusalReason::Other),
        ] {
            assert_eq!(RefusalReason::classify(message), expected, "{message}");
        }
        assert_eq!(RefusalReason::consensus("peer timed out"), None);
    }
}
