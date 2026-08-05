//! The payloads a node POSTs to its registered event observers.
//!
//! The shapes are stacks-core's (`stacks-node/src/event_dispatcher/payloads.rs`)
//! because an observer — the Hiro API, hacknet's tooling, a signer — reads them
//! by field name. `new_block` is also nano's own receipt oracle: the captured
//! fixtures are exactly what this module has to reproduce.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use clarity::vm::costs::ExecutionCost;
use nano_chainstate::{AppliedBlock, NakamotoBlock, TransactionReceipt, TransactionStatus};
use nano_primitives::{
    BitcoinHeaderHash, BlockHeaderHash, ConsensusHash, Sha256Sum, StacksBlockId,
};
use nano_stackerdb::Chunk;
use reqwest::Url;
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// A block whose transactions were all skipped still says nothing about
/// microblocks, which epoch 4.0 does not have.
const NO_MICROBLOCK: [u8; 32] = [0; 32];

/// The events a node publishes, and the path each is posted to.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EventKind {
    NewBlock,
    NewBurnBlock,
    StackerDbChunks,
    ProposalResponse,
    MinedNakamotoBlock,
}

impl EventKind {
    /// The observer path this event is posted to.
    #[must_use]
    pub const fn path(self) -> &'static str {
        match self {
            Self::NewBlock => "new_block",
            Self::NewBurnBlock => "new_burn_block",
            Self::StackerDbChunks => "stackerdb_chunks",
            Self::ProposalResponse => "proposal_response",
            Self::MinedNakamotoBlock => "mined_nakamoto_block",
        }
    }
}

/// One matured tenure reward, as an observer reads it back.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaturedReward {
    pub recipient: String,
    pub miner_address: String,
    pub coinbase: u128,
    pub tx_fees_anchored: u128,
    pub tx_fees_streamed_confirmed: u128,
    pub tx_fees_streamed_produced: u128,
    pub from_stacks_block_hash: BlockHeaderHash,
    pub from_index_consensus_hash: StacksBlockId,
}

/// One signer of the reward set a block anchored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RewardSetSigner {
    pub signing_key: [u8; 33],
    pub stacked_amount: u128,
    pub weight: u32,
}

/// The signer set a block anchored for a reward cycle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewardSetEvent {
    pub cycle_number: u64,
    pub signers: Vec<RewardSetSigner>,
    /// Absent under waterfall, which has no per-slot threshold.
    pub pox_ustx_threshold: Option<u128>,
}

/// What a `new_block` payload reports that the block itself does not carry.
///
/// These come from the sortition and the header index rather than from
/// execution, so the node supplies them alongside the executed block.
#[derive(Clone, Debug)]
#[derive(Default)]
pub struct BlockEventContext {
    pub parent_block_hash: BlockHeaderHash,
    pub bitcoin_block_hash: BitcoinHeaderHash,
    pub bitcoin_height: u64,
    pub bitcoin_timestamp: u64,
    pub parent_bitcoin_block_hash: BitcoinHeaderHash,
    pub parent_bitcoin_height: u64,
    pub parent_bitcoin_timestamp: u64,
    /// The block commitment that won this tenure's sortition.
    pub miner_txid: Sha256Sum,
    pub tenure_height: u64,
    pub v1_unlock_height: u32,
    pub v2_unlock_height: u32,
    pub v3_unlock_height: u32,
    pub pox_5_activation_height: u32,
    pub matured_rewards: Vec<MaturedReward>,
    pub reward_set: Option<RewardSetEvent>,
}

/// Build the `new_block` payload for a block this node executed.
#[must_use]
pub fn new_block_payload(
    block: &NakamotoBlock,
    applied: &AppliedBlock,
    context: &BlockEventContext,
) -> Value {
    let header = &block.header;
    let transactions: Vec<Value> = block
        .transactions
        .iter()
        .zip(&applied.receipts)
        .enumerate()
        .map(|(index, (transaction, receipt))| {
            transaction_payload(index, &transaction.encode(), receipt)
        })
        .collect();
    let (reward_set, cycle_number) = context
        .reward_set
        .as_ref()
        .map_or((Value::Null, Value::Null), |set| {
            (reward_set_payload(set), json!(set.cycle_number))
        });
    json!({
        "block_hash": format!("0x{}", header.block_hash()),
        "block_height": header.chain_length,
        "block_time": header.timestamp,
        "burn_block_hash": format!("0x{}", context.bitcoin_block_hash),
        "burn_block_height": context.bitcoin_height,
        "burn_block_time": context.bitcoin_timestamp,
        "miner_txid": format!("0x{}", context.miner_txid),
        "index_block_hash": format!("0x{}", block.block_id()),
        "consensus_hash": format!("0x{}", header.consensus_hash),
        "parent_block_hash": format!("0x{}", context.parent_block_hash),
        "parent_index_block_hash": format!("0x{}", header.parent_block_id),
        "parent_microblock": format!("0x{}", BlockHeaderHash::from_bytes(NO_MICROBLOCK)),
        "parent_microblock_sequence": 0,
        "parent_burn_block_hash": format!("0x{}", context.parent_bitcoin_block_hash),
        "parent_burn_block_height": context.parent_bitcoin_height,
        "parent_burn_block_timestamp": context.parent_bitcoin_timestamp,
        "matured_miner_rewards": context.matured_rewards.iter().map(matured_reward_payload).collect::<Vec<_>>(),
        "events": receipt_events(&applied.receipts),
        "transactions": transactions,
        "anchored_cost": cost_payload(&applied.execution_cost),
        "confirmed_microblocks_cost": cost_payload(&ExecutionCost::ZERO),
        "pox_v1_unlock_height": context.v1_unlock_height,
        "pox_v2_unlock_height": context.v2_unlock_height,
        "pox_v3_unlock_height": context.v3_unlock_height,
        "pox_v4_unlock_height": context.pox_5_activation_height,
        "signer_bitvec": hex::encode(header.pox_treatment.wire_bytes()),
        "reward_set": reward_set,
        "cycle_number": cycle_number,
        "tenure_height": context.tenure_height,
        "signer_signature_hash": format!("0x{}", header.signer_signature_hash()),
        "miner_signature": format!("0x{}", hex::encode(header.miner_signature.as_bytes())),
        "signer_signature": header
            .signer_signatures
            .iter()
            .map(|signature| hex::encode(signature.as_bytes()))
            .collect::<Vec<_>>(),
    })
}

/// Build the `new_burn_block` payload for a Bitcoin block a node processed.
///
/// Waterfall `PoX` pays a single sBTC output, so an observer sees one recipient
/// and one slot holder rather than a reward-set walk.
#[must_use]
pub fn new_burn_block_payload(
    bitcoin_block_hash: BitcoinHeaderHash,
    bitcoin_height: u64,
    consensus_hash: ConsensusHash,
    parent_bitcoin_block_hash: BitcoinHeaderHash,
    burned: u64,
) -> Value {
    json!({
        "burn_block_hash": format!("0x{bitcoin_block_hash}"),
        "burn_block_height": bitcoin_height,
        "consensus_hash": format!("0x{consensus_hash}"),
        "parent_burn_block_hash": format!("0x{parent_bitcoin_block_hash}"),
        "burn_amount": burned,
        "reward_recipients": Vec::<Value>::new(),
        "reward_slot_holders": Vec::<String>::new(),
        "pox_transactions": Vec::<Value>::new(),
    })
}

/// Build the `stackerdb_chunks` payload for chunks a node accepted.
#[must_use]
pub fn stackerdb_chunks_payload(contract_id: &str, chunks: &[Chunk]) -> Value {
    json!({
        "contract_id": contract_id,
        "modified_slots": chunks
            .iter()
            .map(|chunk| json!({
                "slot_id": chunk.slot_id,
                "slot_version": chunk.slot_version,
                "data": hex::encode(&chunk.data),
                "sig": hex::encode(chunk.signature.as_bytes()),
            }))
            .collect::<Vec<_>>(),
    })
}

/// Why a node refused a block proposal (`net/api/postblock_proposal.rs:87`).
///
/// A signer branches on this, so the names are stacks-core's: `define_u8_enum!`
/// derives `Serialize`, which writes the variant name as a bare string.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProposalRejectCode {
    BadBlockHash,
    BadTransaction,
    InvalidBlock,
    ChainstateError,
    UnknownParent,
    NonCanonicalTenure,
    NoSuchTenure,
    InvalidTransactionReplay,
    InvalidParentBlock,
    InvalidTimestamp,
    NetworkChainMismatch,
    NotFoundError,
    ProblematicTransaction,
}

impl ProposalRejectCode {
    /// The name a signer reads this code by.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::BadBlockHash => "BadBlockHash",
            Self::BadTransaction => "BadTransaction",
            Self::InvalidBlock => "InvalidBlock",
            Self::ChainstateError => "ChainstateError",
            Self::UnknownParent => "UnknownParent",
            Self::NonCanonicalTenure => "NonCanonicalTenure",
            Self::NoSuchTenure => "NoSuchTenure",
            Self::InvalidTransactionReplay => "InvalidTransactionReplay",
            Self::InvalidParentBlock => "InvalidParentBlock",
            Self::InvalidTimestamp => "InvalidTimestamp",
            Self::NetworkChainMismatch => "NetworkChainMismatch",
            Self::NotFoundError => "NotFoundError",
            Self::ProblematicTransaction => "ProblematicTransaction",
        }
    }
}

/// What a node answers a block proposal with, once it has judged it.
///
/// The wire shape is `BlockValidateResponse`, an internally tagged enum on
/// `result`, so both arms carry `signer_signature_hash` beside a `result` of
/// `Ok` or `Reject`. The hash is a *bare* hex string here, unlike `new_block`'s
/// `0x`-prefixed one, because stacks-core serializes the hash type directly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProposalOutcome {
    /// The node holds this block and stands behind the state it computed.
    ///
    /// `cost` zero is not a placeholder: a stock signer reads it as "nothing was
    /// executed to answer this", because the node already had the block, and
    /// records a validation time of zero accordingly
    /// (`stacks-signer/src/v0/signer.rs:1569`).
    Accepted {
        cost: ExecutionCost,
        size: u64,
        validation_time_ms: u64,
    },
    Rejected {
        reason: String,
        code: ProposalRejectCode,
    },
}

/// Build the `proposal_response` payload a signer waits for after proposing.
#[must_use]
pub fn proposal_response_payload(
    signer_signature_hash: Sha256Sum,
    outcome: &ProposalOutcome,
) -> Value {
    match outcome {
        ProposalOutcome::Accepted {
            cost,
            size,
            validation_time_ms,
        } => json!({
            "result": "Ok",
            "signer_signature_hash": signer_signature_hash.to_string(),
            "cost": cost_payload(cost),
            "size": size,
            "validation_time_ms": validation_time_ms,
            "replay_tx_hash": Value::Null,
            "replay_tx_exhausted": false,
        }),
        ProposalOutcome::Rejected { reason, code } => json!({
            "result": "Reject",
            "signer_signature_hash": signer_signature_hash.to_string(),
            "reason": reason,
            "reason_code": code.name(),
            "failed_txid": Value::Null,
        }),
    }
}

/// Build the `/v3/stacker_set` document for a reward cycle this node derived.
///
/// Shared with the event payload's `reward_set` rather than shaped twice: they
/// are the same set of signers, and nano's own `SyncClient` parses this document
/// back, which is what makes a served reward set usable as another node's
/// checkpoint attestation.
///
/// The amounts are JSON numbers, as stacks-core writes them and as nano's own
/// `SyncClient` reads them back.
///
/// The shape is `RewardSetV0`'s, which is the one a document with no
/// `reward_set_version` is read as. The 4.0 `Waterfall` shape requires
/// `sbtc_address`, and nano does not derive it: it comes from the sBTC registry's
/// aggregate public key through the taproot derivation, which nothing reads yet.
/// A version 1 document without it would not deserialize at all, so this serves
/// the version every reader accepts rather than claiming a version it cannot
/// fill.
#[must_use]
pub fn stacker_set_payload(signers: &[RewardSetSigner], pox_ustx_threshold: u128) -> Value {
    json!({
        "signers": signers
            .iter()
            .map(|signer| json!({
                // Bare hex, no `0x`: stacks-core writes the key type straight
                // out, and its own reader is not prefix-tolerant.
                "signing_key": hex::encode(signer.signing_key),
                "stacked_amt": microstx(signer.stacked_amount),
                "weight": signer.weight,
            }))
            .collect::<Vec<_>>(),
        "pox_ustx_threshold": microstx(pox_ustx_threshold),
        // Empty under waterfall, which pays one sBTC output rather than a set of
        // reward addresses, and so misses no slots either.
        "rewarded_addresses": Vec::<Value>::new(),
        "start_cycle_state": { "missed_reward_slots": Vec::<Value>::new() },
    })
}

/// A microSTX quantity as a JSON number.
///
/// Clarity counts in `u128` and JSON numbers stop at `u64`, which is four orders
/// of magnitude above the whole STX supply — so this only ever matters for a
/// quantity that is already impossible, and such a one is reported as absent
/// rather than truncated into a plausible smaller number or panicked on.
fn microstx(amount: u128) -> Value {
    serde_json::Number::from_u128(amount).map_or(Value::Null, Value::Number)
}

/// Build the `mined_nakamoto_block` payload for a block this node assembled.
#[must_use]
pub fn mined_nakamoto_block_payload(
    block: &NakamotoBlock,
    applied: &AppliedBlock,
    target_bitcoin_height: u64,
) -> Value {
    let header = &block.header;
    let encoded = block.encode();
    json!({
        "target_burn_height": target_bitcoin_height,
        "parent_block_id": format!("0x{}", header.parent_block_id),
        "block_hash": format!("0x{}", header.block_hash()),
        "block_id": format!("0x{}", block.block_id()),
        "stacks_height": header.chain_length,
        "block_size": encoded.len(),
        "cost": cost_payload(&applied.execution_cost),
        "miner_signature": hex::encode(header.miner_signature.as_bytes()),
        "miner_signature_hash": format!("0x{}", header.miner_signature_hash()),
        "signer_signature_hash": format!("0x{}", header.signer_signature_hash()),
        "tx_events": Vec::<Value>::new(),
        "signer_bitvec": hex::encode(header.pox_treatment.wire_bytes()),
        "signer_signature": header
            .signer_signatures
            .iter()
            .map(|signature| hex::encode(signature.as_bytes()))
            .collect::<Vec<_>>(),
    })
}

fn transaction_payload(
    index: usize,
    raw_transaction: &[u8],
    receipt: &TransactionReceipt,
) -> Value {
    let (status, vm_error) = match &receipt.status {
        TransactionStatus::Success => ("success", None),
        TransactionStatus::AbortedByResponse => ("abort_by_response", None),
        TransactionStatus::PostConditionAborted(_) => ("abort_by_post_condition", None),
        TransactionStatus::RuntimeFailure(error) => ("abort_by_response", Some(error.clone())),
    };
    json!({
        "txid": format!("0x{}", receipt.txid),
        "tx_index": index,
        "status": status,
        "raw_result": receipt
            .result
            .value
            .as_ref()
            .and_then(|value| value.serialize_to_hex().ok())
            .map_or(Value::Null, |hex| Value::from(format!("0x{hex}"))),
        "raw_tx": format!("0x{}", hex::encode(raw_transaction)),
        "contract_interface": Value::Null,
        "burnchain_op": Value::Null,
        "execution_cost": cost_payload(&receipt.result.cost),
        "microblock_sequence": Value::Null,
        "microblock_hash": Value::Null,
        "microblock_parent_hash": Value::Null,
        "vm_error": vm_error,
    })
}

/// Flatten the receipts' Clarity events into one block-wide, indexed list.
fn receipt_events(receipts: &[TransactionReceipt]) -> Vec<Value> {
    receipts
        .iter()
        .flat_map(|receipt| {
            receipt
                .result
                .events
                .iter()
                .map(move |event| (event, receipt.txid, receipt.committed))
        })
        .enumerate()
        .filter_map(|(index, (event, txid, committed))| {
            event.json_serialize(index, &txid, committed).ok()
        })
        .collect()
}

fn matured_reward_payload(reward: &MaturedReward) -> Value {
    json!({
        "recipient": reward.recipient,
        "miner_address": reward.miner_address,
        "coinbase_amount": reward.coinbase.to_string(),
        "tx_fees_anchored": reward.tx_fees_anchored.to_string(),
        "tx_fees_streamed_confirmed": reward.tx_fees_streamed_confirmed.to_string(),
        "tx_fees_streamed_produced": reward.tx_fees_streamed_produced.to_string(),
        "from_stacks_block_hash": format!("0x{}", reward.from_stacks_block_hash),
        "from_index_consensus_hash": format!("0x{}", reward.from_index_consensus_hash),
    })
}

fn reward_set_payload(reward_set: &RewardSetEvent) -> Value {
    json!({
        "rewarded_addresses": Vec::<String>::new(),
        "start_cycle_state": { "missed_reward_slots": Vec::<Value>::new() },
        "signers": reward_set
            .signers
            .iter()
            .map(|signer| json!({
                "signing_key": hex::encode(signer.signing_key),
                "stacked_amt": signer.stacked_amount.to_string(),
                "weight": signer.weight,
            }))
            .collect::<Vec<_>>(),
        "pox_ustx_threshold": reward_set.pox_ustx_threshold.map(|threshold| threshold.to_string()),
    })
}

fn cost_payload(cost: &ExecutionCost) -> Value {
    json!({
        "read_count": cost.read_count,
        "read_length": cost.read_length,
        "runtime": cost.runtime,
        "write_count": cost.write_count,
        "write_length": cost.write_length,
    })
}

/// How many times one observer is retried before an event is dropped.
pub const DEFAULT_DISPATCH_ATTEMPTS: u32 = 5;

/// How many bytes of undelivered events one observer may hold.
///
/// The queue is what stops a slow observer slowing the node down; its bound is
/// what stops a dead one being a memory leak. Bounded in bytes rather than in
/// events because payload sizes span three orders of magnitude — an empty
/// `stackerdb_chunks` is a couple of hundred bytes and a full mainnet
/// `new_block` is hundreds of kilobytes — so any event count either wastes the
/// budget or blows through it. At mainnet's ~50 KB average this is several
/// hundred blocks of slack, so an observer that restarts or pauses for a GC
/// catches up with no gap at all.
pub const DEFAULT_DISPATCH_QUEUE_BYTES: usize = 32 * 1024 * 1024;

/// How often a node repeats itself about an observer whose events it is
/// dropping. Per event it would be one line per block for as long as the
/// observer stays down, which buries every other line in the log.
const COMPLAINT_INTERVAL: Duration = Duration::from_secs(30);

/// The sequence number of this event in the node's stream to this observer.
///
/// Counts every event *offered*, delivered or not, so an observer that records
/// it sees a gap exactly where the node dropped something. The payload body is
/// stacks-core's byte for byte; this is a header, so an observer that does not
/// read it is unaffected.
const SEQUENCE_HEADER: &str = "x-nano-event-seq";

/// How many events this node has dropped for this observer, in total.
const DROPPED_HEADER: &str = "x-nano-events-dropped";

/// How hard one observer is tried, and how far behind it may fall.
#[derive(Clone, Copy, Debug)]
pub struct DispatchLimits {
    pub attempts: u32,
    pub queue_bytes: usize,
}

impl Default for DispatchLimits {
    fn default() -> Self {
        Self {
            attempts: DEFAULT_DISPATCH_ATTEMPTS,
            queue_bytes: DEFAULT_DISPATCH_QUEUE_BYTES,
        }
    }
}

/// What one observer has been sent, and what it has missed.
#[derive(Clone, Debug)]
pub struct ObserverStatus {
    pub url: Url,
    pub delivered: u64,
    pub dropped: u64,
    /// Events queued for it that have not been attempted yet.
    pub undelivered: usize,
    /// Whether its last attempted event was accepted.
    pub reachable: bool,
}

/// One event on its way to one observer.
struct Event {
    kind: EventKind,
    sequence: u64,
    /// Serialized once per dispatch and shared by every observer.
    body: Arc<Vec<u8>>,
}

/// The observers a node publishes its events to.
///
/// Dispatch does not block the caller. stacks-core POSTs to its observers from
/// the thread that processed the block and retries forever; nano hands the event
/// to a per-observer queue drained by its own task, because a node's block
/// execution must not be gated on an HTTP request to a third party. Awaited
/// inline, five attempts with backoff against an observer that does not answer
/// cost about a second per block — which is what most of a mainnet replay's
/// 28–34 blocks/min turned out to be.
///
/// One queue and one task **per observer**, so per-observer delivery order is
/// exactly dispatch order: an indexer applying `new_block` needs the parent
/// before the child, and a `stacks-signer` reads them as a sequence of state
/// transitions. Observers do not wait on each other.
#[derive(Clone, Debug)]
pub struct EventDispatcher {
    observers: Vec<Arc<Observer>>,
}

impl EventDispatcher {
    /// Publish to the supplied observer base URLs.
    ///
    /// Spawns one drain task per observer, so this must be called from inside a
    /// tokio runtime.
    #[must_use]
    pub fn new(observers: Vec<Url>) -> Self {
        Self::with_limits(observers, DispatchLimits::default())
    }

    /// Publish to the supplied observers under limits of your own.
    #[must_use]
    pub fn with_limits(observers: Vec<Url>, limits: DispatchLimits) -> Self {
        let client = reqwest::Client::new();
        let observers = observers
            .into_iter()
            .map(|url| {
                let (events, queue) = mpsc::unbounded_channel();
                let observer = Arc::new(Observer {
                    url,
                    events,
                    attempts: limits.attempts.max(1),
                    queue_bytes: limits.queue_bytes,
                    queued: AtomicUsize::new(0),
                    pending: AtomicUsize::new(0),
                    offered: AtomicU64::new(0),
                    delivered: AtomicU64::new(0),
                    dropped: AtomicU64::new(0),
                    reachable: AtomicBool::new(true),
                    complained: Mutex::new(None),
                });
                tokio::spawn(drain(Arc::clone(&observer), queue, client.clone()));
                observer
            })
            .collect();
        Self { observers }
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.observers.is_empty()
    }

    /// Queue one event for every observer and return.
    ///
    /// Serializes the payload once and shares the bytes, so a second observer
    /// costs a pointer rather than a copy of a mainnet block's receipts.
    pub fn dispatch(&self, kind: EventKind, payload: &Value) {
        if self.observers.is_empty() {
            return;
        }
        let body = match serde_json::to_vec(payload) {
            Ok(body) => Arc::new(body),
            // A payload that will not serialize is a bug here, not an observer's
            // problem, and saying so beats every observer reporting a gap.
            Err(error) => {
                eprintln!("the {} event could not be serialized: {error}", kind.path());
                return;
            }
        };
        for observer in &self.observers {
            observer.offer(kind, &body);
        }
    }

    /// What every observer has been sent, and what it has missed.
    #[must_use]
    pub fn status(&self) -> Vec<ObserverStatus> {
        self.observers
            .iter()
            .map(|observer| ObserverStatus {
                url: observer.url.clone(),
                delivered: observer.delivered.load(Ordering::Relaxed),
                dropped: observer.dropped.load(Ordering::Relaxed),
                undelivered: observer.pending.load(Ordering::Relaxed),
                reachable: observer.reachable.load(Ordering::Relaxed),
            })
            .collect()
    }

    /// Wait until every queued event has been attempted, or `timeout` elapses.
    ///
    /// Answers whether the queues emptied. Bounded on purpose: waiting on an
    /// observer without a limit is the stall this whole queue exists to remove,
    /// so a shutdown gives the drain a moment and then says what it abandoned.
    pub async fn settle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self
                .observers
                .iter()
                .all(|observer| observer.pending.load(Ordering::Relaxed) == 0)
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// One observer's queue and its delivery record.
#[derive(Debug)]
struct Observer {
    url: Url,
    events: mpsc::UnboundedSender<Event>,
    attempts: u32,
    queue_bytes: usize,
    /// Bytes of queued-but-unattempted payloads, held against `queue_bytes`.
    /// This is the bound; the channel itself is unbounded because a count of
    /// events says nothing about the memory they occupy.
    queued: AtomicUsize,
    /// Events queued and not yet attempted, which is what `settle` waits on.
    pending: AtomicUsize,
    offered: AtomicU64,
    delivered: AtomicU64,
    dropped: AtomicU64,
    reachable: AtomicBool,
    /// When this observer was last complained about.
    complained: Mutex<Option<Instant>>,
}

impl Observer {
    /// Queue one event, or drop it and say so.
    fn offer(&self, kind: EventKind, body: &Arc<Vec<u8>>) {
        // Numbered before it is admitted, so that a dropped event consumes a
        // sequence number and the observer sees where it went.
        let sequence = self.offered.fetch_add(1, Ordering::Relaxed);
        let size = body.len();
        let admitted = self
            .queued
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |queued| {
                (queued + size <= self.queue_bytes).then_some(queued + size)
            });
        if admitted.is_err() {
            self.drop_event(kind, sequence, "its queue of undelivered events is full");
            return;
        }
        self.pending.fetch_add(1, Ordering::Relaxed);
        if self
            .events
            .send(Event {
                kind,
                sequence,
                body: Arc::clone(body),
            })
            .is_err()
        {
            // The drain task is gone, which happens only as the process exits.
            self.queued.fetch_sub(size, Ordering::Relaxed);
            self.pending.fetch_sub(1, Ordering::Relaxed);
            self.drop_event(kind, sequence, "its delivery task has stopped");
        }
    }

    /// POST one event, retrying a live observer and giving up on a dead one.
    async fn deliver(&self, client: &reqwest::Client, event: &Event) {
        let Ok(url) = self.url.join(event.kind.path()) else {
            self.drop_event(event.kind, event.sequence, "its URL admits no event path");
            return;
        };
        // An observer already known to be down is tried once rather than five
        // times. The backoff exists to give a transient failure a second chance;
        // spending it on every event of a dead observer only makes its queue
        // fill faster, and it is what made the inline path cost a second a block.
        let attempts = if self.reachable.load(Ordering::Relaxed) {
            self.attempts
        } else {
            1
        };
        for attempt in 0..attempts {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(100 * u64::from(attempt))).await;
            }
            let answer = client
                .post(url.clone())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(SEQUENCE_HEADER, event.sequence)
                .header(DROPPED_HEADER, self.dropped.load(Ordering::Relaxed))
                .body(Vec::clone(&event.body))
                .send()
                .await;
            match answer {
                Ok(response) if response.status().is_success() => {
                    self.delivered.fetch_add(1, Ordering::Relaxed);
                    if !self.reachable.swap(true, Ordering::Relaxed) {
                        // Worth an unconditional line: it names how much of the
                        // chain this observer's view is missing, and nothing will
                        // resend it.
                        eprintln!(
                            "event observer {} is accepting events again, {} dropped in the meantime and not resent",
                            self.url,
                            self.dropped.load(Ordering::Relaxed)
                        );
                    }
                    return;
                }
                // An observer that answered has judged this event, and asking
                // again cannot change its mind: a 404 says it serves no such
                // endpoint, a 4xx that it will not have this payload. Only a
                // request that never arrived, or one the observer failed to
                // handle, is worth repeating.
                Ok(response) if !retryable(response.status()) => break,
                _ => {}
            }
        }
        self.reachable.store(false, Ordering::Relaxed);
        self.drop_event(
            event.kind,
            event.sequence,
            "it did not accept the event after every attempt",
        );
    }

    /// Count an event this observer will never see, and say so at most every
    /// [`COMPLAINT_INTERVAL`].
    fn drop_event(&self, kind: EventKind, sequence: u64, because: &str) {
        let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        if !self.due_to_complain() {
            return;
        }
        eprintln!(
            "event observer {}: dropped {} event {sequence} because {because}; \
             {dropped} of {} events dropped so far, visible to it as gaps in its \
             {SEQUENCE_HEADER} headers",
            self.url,
            kind.path(),
            self.offered.load(Ordering::Relaxed)
        );
    }

    /// Whether enough time has passed to say it again. Taken and released here
    /// so that nothing prints while holding the clock.
    fn due_to_complain(&self) -> bool {
        let mut complained = self.complained.lock().expect("the complaint clock");
        if complained.is_some_and(|at| at.elapsed() < COMPLAINT_INTERVAL) {
            return false;
        }
        *complained = Some(Instant::now());
        true
    }
}

/// Whether asking this observer again could get a different answer.
fn retryable(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

/// Deliver one observer's events, in the order they were dispatched.
async fn drain(
    observer: Arc<Observer>,
    mut queue: mpsc::UnboundedReceiver<Event>,
    client: reqwest::Client,
) {
    while let Some(event) = queue.recv().await {
        let size = event.body.len();
        observer.deliver(&client, &event).await;
        drop(event);
        observer.queued.fetch_sub(size, Ordering::Relaxed);
        observer.pending.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use axum::{
        Router,
        extract::{Path, State},
        http::HeaderMap,
        routing::post,
    };
    use serde_json::json;

    use super::{
        DEFAULT_DISPATCH_ATTEMPTS, DROPPED_HEADER, DispatchLimits, EventDispatcher, EventKind,
        SEQUENCE_HEADER, Url, Value,
    };

    /// One POST an observer received.
    #[derive(Clone, Debug)]
    struct Post {
        path: String,
        payload: Value,
        /// The event's place in the node's stream to this observer.
        sequence: u64,
        /// How many events the node had dropped for it when this one was sent.
        dropped: u64,
    }

    type Received = Arc<Mutex<Vec<Post>>>;

    /// How long a test waits for the queues to drain before calling it a stall.
    const PATIENCE: Duration = Duration::from_secs(10);

    /// An observer that records what it is sent, after `delay` per request.
    async fn observer(delay: Duration) -> (Url, Received) {
        let received: Received = Arc::default();
        let app = Router::new()
            .route(
                "/{event}",
                post(
                    move |State(received): State<Received>,
                          Path(event): Path<String>,
                          headers: HeaderMap,
                          body: String| async move {
                        tokio::time::sleep(delay).await;
                        let number = |name: &str| {
                            headers
                                .get(name)
                                .and_then(|value| value.to_str().ok())
                                .and_then(|value| value.parse().ok())
                                .unwrap_or_else(|| panic!("every POST carries {name}"))
                        };
                        received.lock().expect("record").push(Post {
                            path: event,
                            payload: serde_json::from_str(&body).expect("a JSON payload"),
                            sequence: number(SEQUENCE_HEADER),
                            dropped: number(DROPPED_HEADER),
                        });
                    },
                ),
            )
            .with_state(received.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind observer");
        let address = listener.local_addr().expect("observer address");
        tokio::spawn(async move { axum::serve(listener, app).await });
        (
            Url::parse(&format!("http://{address}/")).expect("observer URL"),
            received,
        )
    }

    #[tokio::test]
    async fn every_event_reaches_the_observer_on_its_own_path() {
        let (url, received) = observer(Duration::ZERO).await;
        let dispatcher = EventDispatcher::new(vec![url]);

        for kind in [
            EventKind::NewBlock,
            EventKind::NewBurnBlock,
            EventKind::StackerDbChunks,
            EventKind::ProposalResponse,
            EventKind::MinedNakamotoBlock,
        ] {
            dispatcher.dispatch(kind, &json!({ "kind": kind.path() }));
        }
        assert!(dispatcher.settle(PATIENCE).await, "the queue drained");

        let received = received.lock().expect("record").clone();
        assert_eq!(
            received
                .iter()
                .map(|post| post.path.as_str())
                .collect::<Vec<_>>(),
            [
                "new_block",
                "new_burn_block",
                "stackerdb_chunks",
                "proposal_response",
                "mined_nakamoto_block",
            ]
        );
        assert_eq!(received[0].payload, json!({ "kind": "new_block" }));
        // One stream per observer, numbered without gaps while nothing is dropped.
        assert_eq!(
            received.iter().map(|post| post.sequence).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        assert!(received.iter().all(|post| post.dropped == 0));
        let [status] = dispatcher.status().try_into().expect("one observer");
        assert_eq!((status.delivered, status.dropped), (5, 0));
    }

    /// The whole point of the queue: an observer that refuses connections costs
    /// the caller no more than the enqueue, five attempts with backoff or not.
    #[tokio::test]
    async fn dispatching_to_a_dead_observer_does_not_wait_for_it() {
        // Port 1 is not listening, so every attempt fails on connect.
        let dead = Url::parse("http://127.0.0.1:1/").expect("URL");
        let dispatcher = EventDispatcher::new(vec![dead]);

        let started = Instant::now();
        for height in 0..50 {
            dispatcher.dispatch(EventKind::NewBlock, &json!({ "height": height }));
        }
        let dispatching = started.elapsed();

        assert!(
            dispatching < Duration::from_millis(100),
            "dispatch waited on the observer: {dispatching:?}"
        );
        // Inline, this would have been 50 blocks x 4 backoff sleeps = 50 s.
        assert!(
            u32::try_from(dispatching.as_millis()).unwrap_or(u32::MAX)
                < 100 * DEFAULT_DISPATCH_ATTEMPTS,
            "dispatch paid the retry backoff: {dispatching:?}"
        );
    }

    /// An observer that refuses an event is asked once, not `attempts` times.
    ///
    /// The retries are backed off, so five attempts at a URL that answers 404 —
    /// which is what an event observer pointed at something serving no such
    /// endpoint does — cost a whole second per event, for an answer that was
    /// never going to change. Off the executor's thread now, but the drain task
    /// still falls a second per block behind for nothing.
    #[tokio::test]
    async fn an_observer_that_refuses_an_event_is_not_asked_again() {
        let asked = Arc::new(Mutex::new(0_usize));
        let counter = Arc::clone(&asked);
        let app = Router::new().route(
            "/{event}",
            post(move || {
                let counter = Arc::clone(&counter);
                async move {
                    *counter.lock().expect("count") += 1;
                    axum::http::StatusCode::NOT_FOUND
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind observer");
        let address = listener.local_addr().expect("observer address");
        tokio::spawn(async move { axum::serve(listener, app).await });
        let url = Url::parse(&format!("http://{address}/")).expect("observer URL");
        let dispatcher = EventDispatcher::new(vec![url]);

        dispatcher.dispatch(EventKind::NewBlock, &json!({}));
        assert!(dispatcher.settle(PATIENCE).await, "the queue drained");

        assert_eq!(*asked.lock().expect("count"), 1);
        let [status] = dispatcher.status().try_into().expect("one observer");
        assert_eq!((status.delivered, status.dropped), (0, 1));
    }

    /// A slow observer falls behind rather than holding the node back, and the
    /// events it misses are the ones over its byte budget — countable, and
    /// visible to it as a gap in the sequence numbers it does receive.
    #[tokio::test]
    async fn an_observer_that_falls_behind_is_dropped_from_and_told_so() {
        let (url, received) = observer(Duration::from_millis(50)).await;
        let dispatcher = EventDispatcher::with_limits(
            vec![url],
            DispatchLimits {
                attempts: 1,
                // Room for two of the payloads below and no more.
                queue_bytes: 64,
            },
        );

        let started = Instant::now();
        for height in 0..20 {
            dispatcher.dispatch(EventKind::NewBlock, &json!({ "height": height }));
        }
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "a slow observer stalled the dispatcher"
        );
        assert!(dispatcher.settle(PATIENCE).await, "the queue drained");

        let [status] = dispatcher.status().try_into().expect("one observer");
        assert!(status.dropped > 0, "the full queue dropped events");
        assert_eq!(status.delivered + status.dropped, 20);

        // The observer has caught up, so the next event reaches it -- and it is
        // the *next* event that carries the evidence: its sequence number has
        // skipped everything the full queue refused.
        dispatcher.dispatch(EventKind::NewBlock, &json!({ "height": 20 }));
        assert!(dispatcher.settle(PATIENCE).await, "the queue drained");

        let received = received.lock().expect("record").clone();
        // What arrived, arrived in dispatch order.
        let sequences: Vec<u64> = received.iter().map(|post| post.sequence).collect();
        assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
        let last = received.last().expect("the observer caught up").clone();
        assert_eq!(last.payload, json!({ "height": 20 }));
        assert_eq!(last.sequence, 20);
        assert!(
            sequences.len() < 21,
            "the burst was meant to overflow the queue"
        );
        // Two independent ways for the observer to notice, both in the headers
        // of an event it did receive: a jump in the sequence, and the count.
        assert!(
            last.sequence >= u64::try_from(sequences.len()).expect("a small count"),
            "a gap in {sequences:?} is what tells the observer it missed events"
        );
        assert_eq!(last.dropped, status.dropped);
    }
}
