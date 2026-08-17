use std::{
    fmt::Write as _,
    sync::{
        Arc,
        atomic::{AtomicI64, AtomicU64},
    },
    time::Duration,
};

use axum::{Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use clarity::vm::costs::ExecutionCost;
use prometheus_client::{
    encoding::{EncodeLabelSet, EncodeLabelValue, LabelValueEncoder, text::encode},
    metrics::{
        counter::Counter,
        family::Family,
        gauge::Gauge,
        histogram::{Histogram, exponential_buckets},
    },
    registry::Registry,
};

use crate::QueueReport;

type IntGauge = Gauge<i64, AtomicI64>;
type FloatGauge = Gauge<f64, AtomicU64>;

/// Execution-cache residency sampled while the executor is already owned.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExecutionCacheReport {
    pub marf_node_entries: usize,
    pub marf_node_bytes: usize,
    pub marf_auxiliary_bytes: usize,
    pub clarity_value_entries: usize,
    pub clarity_value_bytes: usize,
    pub wasm_module_entries: usize,
    pub wasm_module_bytes: usize,
}

/// A bounded ingress queue whose producer reports its own accounting.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum IngressQueue {
    BlockUploads,
    Proposals,
    StackerDbChunks,
    Transactions,
    RelayOffered,
    RelayAnnouncing,
    EventObserver(usize),
    PeerPushes,
}

impl EncodeLabelValue for IngressQueue {
    fn encode(&self, encoder: &mut LabelValueEncoder<'_>) -> std::fmt::Result {
        let name = match self {
            Self::BlockUploads => "block_uploads",
            Self::Proposals => "proposals",
            Self::StackerDbChunks => "stackerdb_chunks",
            Self::Transactions => "transactions",
            Self::RelayOffered => "relay_offered",
            Self::RelayAnnouncing => "relay_announcing",
            Self::PeerPushes => "peer_pushes",
            Self::EventObserver(index) => return write!(encoder, "event_observer_{index}"),
        };
        encoder.write_str(name)
    }
}

/// Current retained work and cumulative shedding for one ingress queue.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IngressQueueStatus {
    pub items: usize,
    pub bytes: usize,
    pub item_limit: usize,
    pub byte_limit: usize,
    pub oldest_age: Option<Duration>,
    pub dropped: u64,
    pub saturations: u64,
}

/// Current use and cumulative saturation of a global/per-subject admission budget.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AdmissionStatus {
    pub used: usize,
    pub subjects: usize,
    pub limit: usize,
    pub per_subject_limit: usize,
    pub saturations: u64,
}

#[derive(Clone, Copy)]
struct AdmissionMetricNames {
    used: (&'static str, &'static str),
    subjects: (&'static str, &'static str),
    limit: (&'static str, &'static str),
    per_subject_limit: (&'static str, &'static str),
    saturations: (&'static str, &'static str),
}

struct AdmissionGauges {
    used: IntGauge,
    subjects: IntGauge,
    limit: IntGauge,
    per_subject_limit: IntGauge,
    saturations: IntGauge,
}

impl AdmissionGauges {
    fn register(registry: &mut Registry, names: AdmissionMetricNames) -> Self {
        Self {
            used: gauge(registry, names.used.0, names.used.1),
            subjects: gauge(registry, names.subjects.0, names.subjects.1),
            limit: gauge(registry, names.limit.0, names.limit.1),
            per_subject_limit: gauge(
                registry,
                names.per_subject_limit.0,
                names.per_subject_limit.1,
            ),
            saturations: gauge(registry, names.saturations.0, names.saturations.1),
        }
    }

    fn publish(&self, status: AdmissionStatus) {
        self.used.set(as_i64(status.used));
        self.subjects.set(as_i64(status.subjects));
        self.limit.set(as_i64(status.limit));
        self.per_subject_limit.set(as_i64(status.per_subject_limit));
        self.saturations.set(as_i64(status.saturations));
    }
}

impl From<nano_queue::Status> for IngressQueueStatus {
    fn from(status: nano_queue::Status) -> Self {
        Self {
            items: status.items,
            bytes: status.bytes,
            item_limit: status.item_limit,
            byte_limit: status.byte_limit,
            oldest_age: status.oldest_age,
            dropped: status.dropped,
            saturations: status.saturations,
        }
    }
}

#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct IngressQueueLabels {
    queue: IngressQueue,
}

struct IngressQueueGauges {
    items: Family<IngressQueueLabels, IntGauge>,
    bytes: Family<IngressQueueLabels, IntGauge>,
    item_limit: Family<IngressQueueLabels, IntGauge>,
    byte_limit: Family<IngressQueueLabels, IntGauge>,
    oldest_age_seconds: Family<IngressQueueLabels, FloatGauge>,
    dropped: Family<IngressQueueLabels, IntGauge>,
    saturations: Family<IngressQueueLabels, IntGauge>,
}

#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct RpcRouteLabels {
    route: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum RpcRefusal {
    Concurrency,
    Rate,
}

impl EncodeLabelValue for RpcRefusal {
    fn encode(&self, encoder: &mut LabelValueEncoder<'_>) -> std::fmt::Result {
        encoder.write_str(match self {
            Self::Concurrency => "concurrency",
            Self::Rate => "rate",
        })
    }
}

#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct RpcRefusalLabels {
    route: &'static str,
    reason: RpcRefusal,
}

struct RpcAdmissionGauges {
    active: Family<RpcRouteLabels, IntGauge>,
    body_byte_limit: Family<RpcRouteLabels, IntGauge>,
    concurrency_limit: Family<RpcRouteLabels, IntGauge>,
    rate_limit: Family<RpcRouteLabels, IntGauge>,
    refusals: Family<RpcRefusalLabels, Counter>,
    global_active: IntGauge,
    global_limit: IntGauge,
    global_refusals: Counter,
}

impl RpcAdmissionGauges {
    fn register(registry: &mut Registry) -> Self {
        let active = Family::default();
        let body_byte_limit = Family::default();
        let concurrency_limit = Family::default();
        let rate_limit = Family::default();
        let refusals = Family::default();
        registry.register(
            "rpc_route_active",
            "Requests currently admitted into each public RPC route.",
            active.clone(),
        );
        registry.register(
            "rpc_route_body_byte_limit",
            "Maximum request-body bytes accepted by each public RPC route.",
            body_byte_limit.clone(),
        );
        registry.register(
            "rpc_route_concurrency_limit",
            "Maximum concurrent requests admitted into each public RPC route.",
            concurrency_limit.clone(),
        );
        registry.register(
            "rpc_route_rate_limit",
            "Maximum requests admitted per second into each public RPC route.",
            rate_limit.clone(),
        );
        registry.register(
            "rpc_route_refusals",
            "Requests refused at a route concurrency or rate boundary.",
            refusals.clone(),
        );
        let global_active = gauge(
            registry,
            "rpc_requests_active",
            "Requests currently admitted across all public RPC routes.",
        );
        let global_limit = gauge(
            registry,
            "rpc_request_concurrency_limit",
            "Maximum requests admitted across all public RPC routes.",
        );
        let global_refusals = counter(
            registry,
            "rpc_request_concurrency_refusals",
            "Requests refused because the node-wide RPC concurrency budget was full.",
        );
        Self {
            active,
            body_byte_limit,
            concurrency_limit,
            rate_limit,
            refusals,
            global_active,
            global_limit,
            global_refusals,
        }
    }

    fn route(
        &self,
        route: &'static str,
        body_bytes: usize,
        concurrent: usize,
        per_second: u64,
        global: usize,
    ) -> RpcRouteMetrics {
        let labels = RpcRouteLabels { route };
        self.body_byte_limit
            .get_or_create(&labels)
            .set(as_i64(body_bytes));
        self.concurrency_limit
            .get_or_create(&labels)
            .set(as_i64(concurrent));
        self.rate_limit
            .get_or_create(&labels)
            .set(as_i64(per_second));
        self.global_limit.set(as_i64(global));
        let active = self.active.get_or_create(&labels).clone();
        let concurrency_refusals = self
            .refusals
            .get_or_create(&RpcRefusalLabels {
                route,
                reason: RpcRefusal::Concurrency,
            })
            .clone();
        let rate_refusals = self
            .refusals
            .get_or_create(&RpcRefusalLabels {
                route,
                reason: RpcRefusal::Rate,
            })
            .clone();
        RpcRouteMetrics {
            active,
            concurrency_refusals,
            rate_refusals,
            global_active: self.global_active.clone(),
            global_refusals: self.global_refusals.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RpcRouteMetrics {
    active: IntGauge,
    concurrency_refusals: Counter,
    rate_refusals: Counter,
    global_active: IntGauge,
    global_refusals: Counter,
}

impl RpcRouteMetrics {
    pub(crate) fn enter(&self) {
        self.active.inc();
        self.global_active.inc();
    }

    pub(crate) fn leave(&self) {
        self.active.dec();
        self.global_active.dec();
    }

    pub(crate) fn refuse_concurrency(&self, global: bool) {
        self.concurrency_refusals.inc();
        if global {
            self.global_refusals.inc();
        }
    }

    pub(crate) fn refuse_rate(&self) {
        self.rate_refusals.inc();
    }
}

impl IngressQueueGauges {
    fn register(registry: &mut Registry) -> Self {
        let items = Family::default();
        let bytes = Family::default();
        let item_limit = Family::default();
        let byte_limit = Family::default();
        let oldest_age_seconds = Family::default();
        let dropped = Family::default();
        let saturations = Family::default();
        registry.register(
            "ingress_queue_items",
            "Items retained by each externally fed bounded queue.",
            items.clone(),
        );
        registry.register(
            "ingress_queue_bytes",
            "Bytes retained by each externally fed bounded queue.",
            bytes.clone(),
        );
        registry.register(
            "ingress_queue_item_limit",
            "Maximum items each externally fed bounded queue retains.",
            item_limit.clone(),
        );
        registry.register(
            "ingress_queue_byte_limit",
            "Maximum bytes each externally fed bounded queue retains.",
            byte_limit.clone(),
        );
        registry.register(
            "ingress_queue_oldest_age_seconds",
            "Age of the oldest item retained by each externally fed bounded queue.",
            oldest_age_seconds.clone(),
        );
        registry.register(
            "ingress_queue_dropped",
            "Items cumulatively dropped by each externally fed bounded queue.",
            dropped.clone(),
        );
        registry.register(
            "ingress_queue_saturations",
            "Times each externally fed bounded queue reached an item or byte limit.",
            saturations.clone(),
        );
        Self {
            items,
            bytes,
            item_limit,
            byte_limit,
            oldest_age_seconds,
            dropped,
            saturations,
        }
    }

    fn publish(&self, queue: IngressQueue, status: IngressQueueStatus) {
        let labels = IngressQueueLabels { queue };
        self.items.get_or_create(&labels).set(as_i64(status.items));
        self.bytes.get_or_create(&labels).set(as_i64(status.bytes));
        self.item_limit
            .get_or_create(&labels)
            .set(as_i64(status.item_limit));
        self.byte_limit
            .get_or_create(&labels)
            .set(as_i64(status.byte_limit));
        self.oldest_age_seconds
            .get_or_create(&labels)
            .set(status.oldest_age.map_or(0.0, |age| age.as_secs_f64()));
        self.dropped
            .get_or_create(&labels)
            .set(as_i64(status.dropped));
        self.saturations
            .get_or_create(&labels)
            .set(as_i64(status.saturations));
    }
}

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
    stackerdb_rounds_unanswered: Counter,
    pushed_blocks_accepted: Counter,
    pushed_blocks_refused: Counter,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ServingRole {
    Follower,
    ProposalValidator,
    StackerDbReplication,
}

impl EncodeLabelValue for ServingRole {
    fn encode(&self, encoder: &mut LabelValueEncoder<'_>) -> std::fmt::Result {
        encoder.write_str(match self {
            Self::Follower => "follower",
            Self::ProposalValidator => "proposal_validator",
            Self::StackerDbReplication => "stackerdb_replication",
        })
    }
}

#[derive(Clone, Debug, EncodeLabelSet, Eq, Hash, PartialEq)]
struct ServingPeerLabels {
    role: ServingRole,
}

struct PeerGauges {
    follower: IntGauge,
    proposal_validator: IntGauge,
    stackerdb_replication: IntGauge,
    p2p_connected: IntGauge,
    p2p_known: IntGauge,
}

struct ResourceGauges {
    tenure_history_window: IntGauge,
    mempool_transactions: IntGauge,
    mempool_bytes: IntGauge,
    mempool_transaction_limit: IntGauge,
    mempool_byte_limit: IntGauge,
    mempool_saturations: IntGauge,
    stackerdb_chunks: IntGauge,
    stackerdb_bytes: IntGauge,
    stackerdb_chunk_limit: IntGauge,
    stackerdb_byte_limit: IntGauge,
    stackerdb_saturations: IntGauge,
    marf_node_cache_entries: IntGauge,
    marf_node_cache_bytes: IntGauge,
    marf_auxiliary_cache_bytes: IntGauge,
    clarity_value_cache_entries: IntGauge,
    clarity_value_cache_bytes: IntGauge,
    wasm_module_cache_entries: IntGauge,
    wasm_module_cache_bytes: IntGauge,
}

/// The last executed block's spend, as stacks-core's monitoring reports it:
/// each cost dimension as the fraction of the block limit it consumed
/// (`stacks_node_last_block_*`), plus how long executing the block actually
/// took — the number stacks-core only prints to its log.
struct ExecutionGauges {
    read_count: FloatGauge,
    read_length: FloatGauge,
    write_count: FloatGauge,
    write_length: FloatGauge,
    runtime: FloatGauge,
    transaction_count: IntGauge,
    contract_calls: Counter,
    block_execution_seconds: Histogram,
}

impl ExecutionGauges {
    fn register(registry: &mut Registry) -> Self {
        let fraction = |registry: &mut Registry, name, help| {
            let gauge = FloatGauge::default();
            registry.register(name, help, gauge.clone());
            gauge
        };
        let block_execution_seconds = Histogram::new(exponential_buckets(0.005, 2.0, 12));
        registry.register(
            "block_execution_seconds",
            "Wall time this node took to execute and seal one block.",
            block_execution_seconds.clone(),
        );
        Self {
            read_count: fraction(
                registry,
                "last_block_read_count",
                "Reads of the last executed block, as a fraction of the block limit.",
            ),
            read_length: fraction(
                registry,
                "last_block_read_length",
                "Bytes read by the last executed block, as a fraction of the block limit.",
            ),
            write_count: fraction(
                registry,
                "last_block_write_count",
                "Writes of the last executed block, as a fraction of the block limit.",
            ),
            write_length: fraction(
                registry,
                "last_block_write_length",
                "Bytes written by the last executed block, as a fraction of the block limit.",
            ),
            runtime: fraction(
                registry,
                "last_block_runtime",
                "Charged runtime of the last executed block, as a fraction of the block limit.",
            ),
            transaction_count: gauge(
                registry,
                "last_block_transaction_count",
                "Transactions in the last executed block.",
            ),
            contract_calls: counter(
                registry,
                "contract_calls_processed",
                "Contract-call transactions executed by this node.",
            ),
            block_execution_seconds,
        }
    }
}

struct ProgressGauges {
    executed_stacks_height: IntGauge,
    followed_stacks_height: IntGauge,
    selected_stacks_height: IntGauge,
    burn_height: IntGauge,
    last_sealed_timestamp_seconds: IntGauge,
}

impl ProgressGauges {
    fn register(registry: &mut Registry) -> Self {
        Self {
            executed_stacks_height: gauge(
                registry,
                "executed_stacks_height",
                "Latest Stacks height this node executed and sealed.",
            ),
            followed_stacks_height: gauge(
                registry,
                "followed_stacks_height",
                "Latest Stacks height advertised by the followed peers.",
            ),
            selected_stacks_height: gauge(
                registry,
                "selected_stacks_height",
                "Latest Stacks height selected by local fork choice.",
            ),
            burn_height: gauge(
                registry,
                "burn_height",
                "Bitcoin height of the latest locally executed Stacks block.",
            ),
            last_sealed_timestamp_seconds: gauge(
                registry,
                "last_sealed_timestamp_seconds",
                "Unix timestamp when the latest Stacks block was sealed.",
            ),
        }
    }
}

impl ResourceGauges {
    fn register(registry: &mut Registry) -> Self {
        Self {
            tenure_history_window: gauge(
                registry,
                "tenure_history_window",
                "Executed tenures retained in the served history window.",
            ),
            mempool_transactions: gauge(
                registry,
                "mempool_transactions",
                "Transactions currently retained in the local mempool.",
            ),
            mempool_bytes: gauge(
                registry,
                "mempool_bytes",
                "Canonical transaction bytes currently retained in the local mempool.",
            ),
            mempool_transaction_limit: gauge(
                registry,
                "mempool_transaction_limit",
                "Maximum transactions retained in the local mempool.",
            ),
            mempool_byte_limit: gauge(
                registry,
                "mempool_byte_limit",
                "Maximum canonical transaction bytes retained in the local mempool.",
            ),
            mempool_saturations: gauge(
                registry,
                "mempool_saturations",
                "Transactions refused because a local mempool limit was full.",
            ),
            stackerdb_chunks: gauge(
                registry,
                "stackerdb_chunks",
                "Signed chunks currently retained in local StackerDB replicas.",
            ),
            stackerdb_bytes: gauge(
                registry,
                "stackerdb_bytes",
                "Chunk payload bytes currently retained in local StackerDB replicas.",
            ),
            stackerdb_chunk_limit: gauge(
                registry,
                "stackerdb_chunk_limit",
                "Maximum signed chunks retained in local StackerDB replicas.",
            ),
            stackerdb_byte_limit: gauge(
                registry,
                "stackerdb_byte_limit",
                "Maximum chunk payload bytes retained in local StackerDB replicas.",
            ),
            stackerdb_saturations: gauge(
                registry,
                "stackerdb_saturations",
                "Signed chunks refused because a local StackerDB limit was full.",
            ),
            marf_node_cache_entries: gauge(
                registry,
                "marf_node_cache_entries",
                "Decoded MARF trie nodes resident in memory.",
            ),
            marf_node_cache_bytes: gauge(
                registry,
                "marf_node_cache_bytes",
                "Estimated bytes held by decoded MARF trie nodes.",
            ),
            marf_auxiliary_cache_bytes: gauge(
                registry,
                "marf_auxiliary_cache_bytes",
                "Estimated bytes held by MARF block and node-hash caches.",
            ),
            clarity_value_cache_entries: gauge(
                registry,
                "clarity_value_cache_entries",
                "Clarity values resident in the side-store read cache.",
            ),
            clarity_value_cache_bytes: gauge(
                registry,
                "clarity_value_cache_bytes",
                "Estimated bytes held by the Clarity side-store read cache.",
            ),
            wasm_module_cache_entries: gauge(
                registry,
                "wasm_module_cache_entries",
                "Compiled Clarity contracts resident in memory.",
            ),
            wasm_module_cache_bytes: gauge(
                registry,
                "wasm_module_cache_bytes",
                "Estimated bytes held by compiled Clarity contracts.",
            ),
        }
    }
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
            stackerdb_rounds_unanswered: counter(
                registry,
                "stackerdb_rounds_unanswered",
                "StackerDB replication rounds no serving peer answered.",
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

impl PeerGauges {
    fn register(registry: &mut Registry) -> Self {
        let family = Family::<ServingPeerLabels, IntGauge>::default();
        let serving_gauge = |role| family.get_or_create(&ServingPeerLabels { role }).clone();
        let peers = Self {
            follower: serving_gauge(ServingRole::Follower),
            proposal_validator: serving_gauge(ServingRole::ProposalValidator),
            stackerdb_replication: serving_gauge(ServingRole::StackerDbReplication),
            p2p_connected: gauge(registry, "p2p_connected", "Live binary P2P sessions."),
            p2p_known: gauge(
                registry,
                "p2p_known",
                "Peers known to the binary P2P table.",
            ),
        };
        registry.register(
            "serving_peers",
            "Peers available to each network-facing node role.",
            family,
        );
        peers
    }
}

struct Inner {
    registry: Registry,
    block_refusals: RefusalCounters,
    sync: SyncCounters,
    progress: ProgressGauges,
    execution: ExecutionGauges,
    peers: PeerGauges,
    staged_blocks: IntGauge,
    relay_offered: IntGauge,
    relay_announcing: IntGauge,
    relay_dropped: IntGauge,
    queued_blocks: IntGauge,
    queued_proposals: IntGauge,
    queued_stackerdb_chunks: IntGauge,
    queued_transactions: IntGauge,
    ingress_queues: IngressQueueGauges,
    rpc_admission: RpcAdmissionGauges,
    rpc_connections: AdmissionGauges,
    p2p_frames: AdmissionGauges,
    p2p_inbound_sessions: AdmissionGauges,
    resources: ResourceGauges,
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

fn register_admission_gauges(
    registry: &mut Registry,
) -> (AdmissionGauges, AdmissionGauges, AdmissionGauges) {
    let rpc_connections = AdmissionGauges::register(
        registry,
        AdmissionMetricNames {
            used: ("rpc_connections_active", "Open public RPC TCP connections."),
            subjects: (
                "rpc_connection_addresses",
                "Client addresses with an open public RPC connection.",
            ),
            limit: (
                "rpc_connection_limit",
                "Maximum public RPC TCP connections.",
            ),
            per_subject_limit: (
                "rpc_connection_per_address_limit",
                "Maximum public RPC TCP connections from one address.",
            ),
            saturations: (
                "rpc_connection_saturations",
                "TCP connections refused by a global or per-address RPC limit.",
            ),
        },
    );
    let p2p_frames = AdmissionGauges::register(
        registry,
        AdmissionMetricNames {
            used: (
                "p2p_frame_bytes",
                "Peer-controlled frame bytes reserved in memory.",
            ),
            subjects: (
                "p2p_frame_addresses",
                "Peer addresses currently holding frame-byte reservations.",
            ),
            limit: (
                "p2p_frame_global_byte_limit",
                "Maximum peer-controlled frame bytes reserved across the node.",
            ),
            per_subject_limit: (
                "p2p_frame_per_address_byte_limit",
                "Maximum frame bytes reserved by one peer address.",
            ),
            saturations: (
                "p2p_frame_saturations",
                "Frames refused by a global or per-address byte limit.",
            ),
        },
    );
    let p2p_inbound_sessions = AdmissionGauges::register(
        registry,
        AdmissionMetricNames {
            used: ("p2p_inbound_sessions", "Open inbound P2P conversations."),
            subjects: (
                "p2p_inbound_addresses",
                "Peer addresses with an open inbound P2P conversation.",
            ),
            limit: (
                "p2p_inbound_session_limit",
                "Maximum inbound P2P conversations.",
            ),
            per_subject_limit: (
                "p2p_inbound_per_address_limit",
                "Maximum inbound P2P conversations from one address.",
            ),
            saturations: (
                "p2p_inbound_saturations",
                "Inbound P2P connections refused by a global or per-address limit.",
            ),
        },
    );
    (rpc_connections, p2p_frames, p2p_inbound_sessions)
}

impl Default for NodeMetrics {
    fn default() -> Self {
        let mut registry = Registry::with_prefix("nano");
        let block_refusals = RefusalCounters::register(&mut registry);
        let sync = SyncCounters::register(&mut registry);
        let resources = ResourceGauges::register(&mut registry);
        let progress = ProgressGauges::register(&mut registry);
        let execution = ExecutionGauges::register(&mut registry);
        let peers = PeerGauges::register(&mut registry);
        let ingress_queues = IngressQueueGauges::register(&mut registry);
        let rpc_admission = RpcAdmissionGauges::register(&mut registry);
        let (rpc_connections, p2p_frames, p2p_inbound_sessions) =
            register_admission_gauges(&mut registry);
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
            progress,
            execution,
            peers,
            staged_blocks,
            relay_offered,
            relay_announcing,
            relay_dropped,
            queued_blocks,
            queued_proposals,
            queued_stackerdb_chunks,
            queued_transactions,
            ingress_queues,
            rpc_admission,
            rpc_connections,
            p2p_frames,
            p2p_inbound_sessions,
            resources,
        }))
    }
}

impl NodeMetrics {
    /// Publish the producer-owned accounting for one externally fed queue.
    pub fn publish_ingress_queue(&self, queue: IngressQueue, status: IngressQueueStatus) {
        self.0.ingress_queues.publish(queue, status);
    }

    pub(crate) fn rpc_route(
        &self,
        route: &'static str,
        body_bytes: usize,
        concurrent: usize,
        per_second: u64,
        global: usize,
    ) -> RpcRouteMetrics {
        self.0
            .rpc_admission
            .route(route, body_bytes, concurrent, per_second, global)
    }

    pub(crate) fn publish_rpc_connections(&self, status: AdmissionStatus) {
        self.0.rpc_connections.publish(status);
    }

    pub fn publish_p2p_frames(&self, status: AdmissionStatus) {
        self.0.p2p_frames.publish(status);
    }

    pub fn publish_p2p_inbound_sessions(&self, status: AdmissionStatus) {
        self.0.p2p_inbound_sessions.publish(status);
    }

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

    /// Record a `StackerDB` replication round that no serving peer answered.
    pub fn record_stackerdb_round_unanswered(&self) {
        self.0.sync.stackerdb_rounds_unanswered.inc();
    }

    /// Record peer-pushed blocks authenticated during one follow round.
    pub fn record_pushed_blocks(&self, accepted: usize, refused: usize) {
        self.0.sync.pushed_blocks_accepted.inc_by(as_u64(accepted));
        self.0.sync.pushed_blocks_refused.inc_by(as_u64(refused));
    }

    /// Publish what executing one block spent and how long it took.
    ///
    /// The fractions mirror stacks-core's `stacks_node_last_block_*` gauges:
    /// each dimension of the block's `ExecutionCost` divided by its own block
    /// limit. The wall time is ours alone — stacks-core only logs it.
    pub fn publish_block_execution(
        &self,
        cost: &ExecutionCost,
        transactions: usize,
        contract_calls: usize,
        duration: std::time::Duration,
    ) {
        let execution = &self.0.execution;
        let limit = &nano_vm::EPOCH_4_BLOCK_LIMIT;
        execution
            .read_count
            .set(fraction(cost.read_count, limit.read_count));
        execution
            .read_length
            .set(fraction(cost.read_length, limit.read_length));
        execution
            .write_count
            .set(fraction(cost.write_count, limit.write_count));
        execution
            .write_length
            .set(fraction(cost.write_length, limit.write_length));
        execution.runtime.set(fraction(cost.runtime, limit.runtime));
        execution.transaction_count.set(as_i64(transactions));
        execution.contract_calls.inc_by(as_u64(contract_calls));
        execution
            .block_execution_seconds
            .observe(duration.as_secs_f64());
    }

    /// Publish the producer-owned accounting for the local mempool.
    pub fn publish_mempool(&self, status: nano_mempool::MempoolStatus) {
        let resources = &self.0.resources;
        resources
            .mempool_transactions
            .set(as_i64(status.transactions));
        resources.mempool_bytes.set(as_i64(status.bytes));
        resources
            .mempool_transaction_limit
            .set(as_i64(status.transaction_limit));
        resources.mempool_byte_limit.set(as_i64(status.byte_limit));
        resources
            .mempool_saturations
            .set(as_i64(status.saturations));
    }

    /// Publish the producer-owned accounting for local `StackerDB` replicas.
    pub fn publish_stackerdb(&self, status: crate::stackerdb::StackerDbStatus) {
        let resources = &self.0.resources;
        resources.stackerdb_chunks.set(as_i64(status.chunks));
        resources.stackerdb_bytes.set(as_i64(status.bytes));
        resources
            .stackerdb_chunk_limit
            .set(as_i64(status.chunk_limit));
        resources
            .stackerdb_byte_limit
            .set(as_i64(status.byte_limit));
        resources
            .stackerdb_saturations
            .set(as_i64(status.saturations));
    }

    /// Publish execution-cache residency observed while the VM was already owned.
    pub fn publish_execution_caches(&self, usage: ExecutionCacheReport) {
        let resources = &self.0.resources;
        resources
            .marf_node_cache_entries
            .set(as_i64(usage.marf_node_entries));
        resources
            .marf_node_cache_bytes
            .set(as_i64(usage.marf_node_bytes));
        resources
            .marf_auxiliary_cache_bytes
            .set(as_i64(usage.marf_auxiliary_bytes));
        resources
            .clarity_value_cache_entries
            .set(as_i64(usage.clarity_value_entries));
        resources
            .clarity_value_cache_bytes
            .set(as_i64(usage.clarity_value_bytes));
        resources
            .wasm_module_cache_entries
            .set(as_i64(usage.wasm_module_entries));
        resources
            .wasm_module_cache_bytes
            .set(as_i64(usage.wasm_module_bytes));
    }

    /// Publish the peers available to the hosted proposal validator.
    pub fn publish_proposal_peers(&self, peers: usize) {
        self.0.peers.proposal_validator.set(as_i64(peers));
    }

    /// Publish the peers available to `StackerDB` replication.
    pub fn publish_stackerdb_peers(&self, peers: usize) {
        self.0.peers.stackerdb_replication.set(as_i64(peers));
    }

    pub(crate) fn publish_tenure_history(&self, tenures: usize) {
        self.0.resources.tenure_history_window.set(as_i64(tenures));
    }

    pub(crate) fn publish_executed(&self, stacks_height: u64, burn_height: u64, now: u64) {
        self.0
            .progress
            .executed_stacks_height
            .set(as_i64(stacks_height));
        self.0.progress.burn_height.set(as_i64(burn_height));
        self.0
            .progress
            .last_sealed_timestamp_seconds
            .set(as_i64(now));
    }

    pub(crate) fn publish_followed(&self, height: u64) {
        self.0.progress.followed_stacks_height.set(as_i64(height));
    }

    pub(crate) fn publish_selected(&self, height: u64) {
        self.0.progress.selected_stacks_height.set(as_i64(height));
    }

    pub(crate) fn publish_peers(&self, serving: usize, connected: usize, known: usize) {
        self.0.peers.follower.set(as_i64(serving));
        self.0.peers.p2p_connected.set(as_i64(connected));
        self.0.peers.p2p_known.set(as_i64(known));
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

    pub(crate) fn encode(&self) -> Result<String, std::fmt::Error> {
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

// The dimensions are bounded by their block limits (at most 5e9), where f64
// is exact; the lint guards magnitudes a cost can never reach.
#[allow(clippy::cast_precision_loss)]
fn fraction(spent: u64, limit: u64) -> f64 {
    spent as f64 / limit as f64
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

    use super::{
        AdmissionStatus, ExecutionCacheReport, IngressQueue, IngressQueueStatus, NodeMetrics,
        RefusalReason, router, serve,
    };
    use crate::{PeerReport, QueueReport, RpcState, SealedTip, SelectedTip};
    use nano_primitives::{BlockHeaderHash, ConsensusHash, Network, StacksBlockId, TrieHash};
    use nano_sync::PoxInfo;

    /// Every sample the golden scrape must contain, one per published metric.
    const GOLDEN_SAMPLES: &[&str] = &[
        "nano_followed_stacks_height 12",
        "nano_selected_stacks_height 9",
        "nano_executed_stacks_height 4",
        "nano_burn_height 3",
        "nano_serving_peers{role=\"follower\"} 3",
        "nano_serving_peers{role=\"proposal_validator\"} 2",
        "nano_serving_peers{role=\"stackerdb_replication\"} 4",
        "nano_staged_blocks 7",
        "nano_ingress_queue_items{queue=\"block_uploads\"} 3",
        "nano_ingress_queue_bytes{queue=\"block_uploads\"} 1024",
        "nano_ingress_queue_item_limit{queue=\"block_uploads\"} 8",
        "nano_ingress_queue_byte_limit{queue=\"block_uploads\"} 2048",
        "nano_ingress_queue_oldest_age_seconds{queue=\"block_uploads\"} 2.5",
        "nano_ingress_queue_dropped{queue=\"block_uploads\"} 4",
        "nano_ingress_queue_saturations{queue=\"block_uploads\"} 5",
        "nano_rpc_connections_active 6",
        "nano_rpc_connection_addresses 2",
        "nano_rpc_connection_limit 256",
        "nano_rpc_connection_per_address_limit 16",
        "nano_rpc_connection_saturations 7",
        "nano_p2p_frame_bytes 8",
        "nano_p2p_frame_addresses 3",
        "nano_p2p_frame_global_byte_limit 80",
        "nano_p2p_frame_per_address_byte_limit 20",
        "nano_p2p_frame_saturations 9",
        "nano_p2p_inbound_sessions 10",
        "nano_p2p_inbound_addresses 4",
        "nano_p2p_inbound_session_limit 64",
        "nano_p2p_inbound_per_address_limit 4",
        "nano_p2p_inbound_saturations 11",
        "nano_block_refusals_total{reason=\"compiler_gap\"} 1",
        "nano_block_refusals_total{reason=\"root_mismatch\"} 1",
        "nano_block_refusals_total{reason=\"signature\"} 1",
        "nano_block_refusals_total{reason=\"missing_context\"} 1",
        "nano_block_refusals_total{reason=\"other\"} 1",
        "nano_peer_failovers_total 1",
        "nano_sync_rounds_unanswered_total 1",
        "nano_stackerdb_rounds_unanswered_total 1",
        "nano_pushed_blocks_accepted_total 3",
        "nano_pushed_blocks_refused_total 2",
        "nano_mempool_transactions 11",
        "nano_mempool_bytes 13",
        "nano_mempool_transaction_limit 17",
        "nano_mempool_byte_limit 19",
        "nano_mempool_saturations 23",
        "nano_stackerdb_chunks 29",
        "nano_stackerdb_bytes 31",
        "nano_stackerdb_chunk_limit 37",
        "nano_stackerdb_byte_limit 41",
        "nano_stackerdb_saturations 43",
        "nano_tenure_history_window 0",
        "nano_marf_node_cache_entries 13",
        "nano_marf_node_cache_bytes 17",
        "nano_marf_auxiliary_cache_bytes 19",
        "nano_clarity_value_cache_entries 23",
        "nano_clarity_value_cache_bytes 29",
        "nano_wasm_module_cache_entries 31",
        "nano_wasm_module_cache_bytes 37",
        "nano_last_block_read_count 0.5",
        "nano_last_block_read_length 0.5",
        "nano_last_block_write_count 0.5",
        "nano_last_block_write_length 0.5",
        "nano_last_block_runtime 0.5",
        "nano_last_block_transaction_count 3",
        "nano_contract_calls_processed_total 2",
        "nano_block_execution_seconds_sum 0.25",
        "nano_block_execution_seconds_count 1",
        "# EOF",
    ];

    /// Exactly half of every dimension of `EPOCH_4_BLOCK_LIMIT`, so each
    /// exported fraction is a clean 0.5.
    fn half_limit_cost() -> clarity::vm::costs::ExecutionCost {
        let limit = nano_vm::EPOCH_4_BLOCK_LIMIT;
        clarity::vm::costs::ExecutionCost {
            write_length: limit.write_length / 2,
            write_count: limit.write_count / 2,
            read_length: limit.read_length / 2,
            read_count: limit.read_count / 2,
            runtime: limit.runtime / 2,
        }
    }

    const fn execution_cache_fixture() -> ExecutionCacheReport {
        ExecutionCacheReport {
            marf_node_entries: 13,
            marf_node_bytes: 17,
            marf_auxiliary_bytes: 19,
            clarity_value_entries: 23,
            clarity_value_bytes: 29,
            wasm_module_entries: 31,
            wasm_module_bytes: 37,
        }
    }

    const fn pox_fixture() -> PoxInfo {
        PoxInfo {
            first_bitcoin_height: 0,
            bitcoin_height: 3,
            prepare_phase_length: 5,
            reward_phase_length: 15,
            reward_slots: 2,
            rejection_fraction: None,
            pox_5_activation_height: Some(262),
            v1_unlock_height: Some(205),
            v2_unlock_height: Some(207),
            v3_unlock_height: Some(210),
        }
    }

    fn publish_ingress_metrics(metrics: &NodeMetrics) {
        metrics.publish_ingress_queue(
            IngressQueue::BlockUploads,
            IngressQueueStatus {
                items: 3,
                bytes: 1024,
                item_limit: 8,
                byte_limit: 2048,
                oldest_age: Some(std::time::Duration::from_millis(2500)),
                dropped: 4,
                saturations: 5,
            },
        );
        metrics.publish_rpc_connections(AdmissionStatus {
            used: 6,
            subjects: 2,
            limit: 256,
            per_subject_limit: 16,
            saturations: 7,
        });
        metrics.publish_p2p_frames(AdmissionStatus {
            used: 8,
            subjects: 3,
            limit: 80,
            per_subject_limit: 20,
            saturations: 9,
        });
        metrics.publish_p2p_inbound_sessions(AdmissionStatus {
            used: 10,
            subjects: 4,
            limit: 64,
            per_subject_limit: 4,
            saturations: 11,
        });
    }

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
        metrics.record_stackerdb_round_unanswered();
        metrics.record_pushed_blocks(3, 2);
        metrics.publish_proposal_peers(2);
        metrics.publish_stackerdb_peers(4);
        metrics.publish_mempool(nano_mempool::MempoolStatus {
            transactions: 11,
            bytes: 13,
            transaction_limit: 17,
            byte_limit: 19,
            saturations: 23,
        });
        metrics.publish_stackerdb(crate::stackerdb::StackerDbStatus {
            chunks: 29,
            bytes: 31,
            chunk_limit: 37,
            byte_limit: 41,
            saturations: 43,
        });
        metrics.publish_execution_caches(execution_cache_fixture());
        publish_ingress_metrics(&metrics);
        metrics.publish_block_execution(
            &half_limit_cost(),
            3,
            2,
            std::time::Duration::from_millis(250),
        );
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
                pox_fixture(),
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
        for sample in GOLDEN_SAMPLES {
            assert!(body.contains(sample), "missing {sample:?} in {body}");
        }
    }

    #[tokio::test]
    async fn observer_queue_limits_and_saturation_are_published() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("observer listener");
        let address = listener.local_addr().expect("observer address");
        let (accepted, connected) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.expect("observer connection");
            let _ = accepted.send(());
            std::future::pending::<()>().await;
        });
        let observer = reqwest::Url::parse(&format!("http://{address}/")).expect("observer URL");
        let dispatcher = crate::EventDispatcher::with_limits(
            vec![observer],
            crate::events::DispatchLimits {
                attempts: 1,
                queue_items: 2,
                queue_bytes: 64,
            },
        );
        let state = RpcState::new(Network::TESTNET).with_observers(dispatcher.clone());

        dispatcher.dispatch(crate::EventKind::NewBlock, &serde_json::json!({"n": 1}));
        tokio::time::timeout(std::time::Duration::from_secs(1), connected)
            .await
            .expect("the observer connects")
            .expect("the observer task remains alive");
        dispatcher.dispatch(crate::EventKind::NewBlock, &serde_json::json!({"n": 2}));
        dispatcher.dispatch(crate::EventKind::NewBlock, &serde_json::json!({"n": 3}));

        let body = state.metrics().encode().expect("metrics encode");
        assert!(body.contains("nano_ingress_queue_items{queue=\"event_observer_0\"} 2"));
        assert!(body.contains("nano_ingress_queue_item_limit{queue=\"event_observer_0\"} 2"));
        assert!(body.contains("nano_ingress_queue_byte_limit{queue=\"event_observer_0\"} 64"));
        assert!(body.contains("nano_ingress_queue_dropped{queue=\"event_observer_0\"} 1"));
        assert!(body.contains("nano_ingress_queue_saturations{queue=\"event_observer_0\"} 1"));
        server.abort();
    }

    #[tokio::test]
    async fn a_tcp_scrape_exposes_a_refused_followed_tip() {
        let metrics = NodeMetrics::default();
        metrics.publish_executed(4, 3, 1_786_310_400);
        metrics.publish_followed(4);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("metrics listener");
        let address = listener.local_addr().expect("metrics address");
        let server = tokio::spawn(serve(listener, metrics.clone()));
        let url = format!("http://{address}/metrics");

        let at_tip = reqwest::get(&url)
            .await
            .expect("metrics response")
            .text()
            .await
            .expect("metrics body");
        assert!(at_tip.contains("nano_executed_stacks_height 4"));
        assert!(at_tip.contains("nano_followed_stacks_height 4"));

        metrics.publish_followed(5);
        metrics.record_block_refusal("Wasm module will not load");
        let refused = reqwest::get(&url)
            .await
            .expect("metrics response")
            .text()
            .await
            .expect("metrics body");
        assert!(refused.contains("nano_executed_stacks_height 4"));
        assert!(refused.contains("nano_followed_stacks_height 5"));
        assert!(refused.contains("nano_block_refusals_total{reason=\"compiler_gap\"} 1"));

        server.abort();
        assert!(server.await.expect_err("server stopped").is_cancelled());
    }

    #[test]
    fn node_metrics_instances_are_isolated() {
        let first = NodeMetrics::default();
        let second = NodeMetrics::default();
        first.record_peer_failover();

        assert!(
            first
                .encode()
                .expect("encode first registry")
                .contains("nano_peer_failovers_total 1")
        );
        assert!(
            second
                .encode()
                .expect("encode second registry")
                .contains("nano_peer_failovers_total 0")
        );
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
