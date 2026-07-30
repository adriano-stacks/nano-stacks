//! The payloads a node POSTs to its registered event observers.
//!
//! The shapes are stacks-core's (`stacks-node/src/event_dispatcher/payloads.rs`)
//! because an observer — the Hiro API, hacknet's tooling, a signer — reads them
//! by field name. `new_block` is also nano's own receipt oracle: the captured
//! fixtures are exactly what this module has to reproduce.

use std::{fmt, time::Duration};

use clarity::vm::costs::ExecutionCost;
use nano_chainstate::{AppliedBlock, NakamotoBlock, TransactionReceipt, TransactionStatus};
use nano_primitives::{BitcoinHeaderHash, BlockHeaderHash, Sha256Sum, StacksBlockId};
use reqwest::Url;
use serde_json::{Value, json};

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

impl fmt::Display for EventKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.path())
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
    consensus_hash: nano_primitives::ConsensusHash,
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
pub fn stackerdb_chunks_payload(contract_id: &str, chunks: &[nano_stackerdb::Chunk]) -> Value {
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

/// The observers a node publishes its events to.
///
/// stacks-core retries an observer forever; nano gives up after a bounded
/// number of attempts so that one dead observer cannot stall block processing.
#[derive(Clone, Debug)]
pub struct EventDispatcher {
    client: reqwest::Client,
    observers: Vec<Url>,
    attempts: u32,
}

/// How many times one observer is retried before an event is dropped.
pub const DEFAULT_DISPATCH_ATTEMPTS: u32 = 5;

impl EventDispatcher {
    /// Publish to the supplied observer base URLs.
    #[must_use]
    pub fn new(observers: Vec<Url>) -> Self {
        Self {
            client: reqwest::Client::new(),
            observers,
            attempts: DEFAULT_DISPATCH_ATTEMPTS,
        }
    }

    /// Retry each observer this many times before dropping an event.
    #[must_use]
    pub fn with_attempts(mut self, attempts: u32) -> Self {
        self.attempts = attempts.max(1);
        self
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.observers.is_empty()
    }

    /// POST one event to every observer, reporting the ones that never accepted it.
    pub async fn dispatch(&self, kind: EventKind, payload: &Value) -> Vec<Url> {
        let mut failed = Vec::new();
        for observer in &self.observers {
            if !self.post(observer, kind, payload).await {
                failed.push(observer.clone());
            }
        }
        failed
    }

    async fn post(&self, observer: &Url, kind: EventKind, payload: &Value) -> bool {
        let Ok(url) = observer.join(kind.path()) else {
            return false;
        };
        for attempt in 0..self.attempts {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(100 * u64::from(attempt))).await;
            }
            if self
                .client
                .post(url.clone())
                .json(payload)
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{
        Router,
        extract::{Path, State},
        routing::post,
    };
    use serde_json::json;

    use super::{EventDispatcher, EventKind, Url, Value};

    /// What an observer received, and the path each payload arrived on.
    type Received = Arc<Mutex<Vec<(String, Value)>>>;

    async fn observer() -> (Url, Received) {
        let received: Received = Arc::default();
        let app = Router::new()
            .route(
                "/{event}",
                post(
                    |State(received): State<Received>,
                     Path(event): Path<String>,
                     axum::Json(payload): axum::Json<Value>| async move {
                        received.lock().expect("record").push((event, payload));
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
        let (url, received) = observer().await;
        let dispatcher = EventDispatcher::new(vec![url]);

        for kind in [
            EventKind::NewBlock,
            EventKind::NewBurnBlock,
            EventKind::StackerDbChunks,
            EventKind::ProposalResponse,
            EventKind::MinedNakamotoBlock,
        ] {
            assert!(
                dispatcher
                    .dispatch(kind, &json!({ "kind": kind.path() }))
                    .await
                    .is_empty()
            );
        }

        let received = received.lock().expect("record").clone();
        assert_eq!(
            received
                .iter()
                .map(|(path, _)| path.as_str())
                .collect::<Vec<_>>(),
            [
                "new_block",
                "new_burn_block",
                "stackerdb_chunks",
                "proposal_response",
                "mined_nakamoto_block",
            ]
        );
        assert_eq!(received[0].1, json!({ "kind": "new_block" }));
    }

    #[tokio::test]
    async fn an_observer_that_never_answers_is_reported_rather_than_waited_on() {
        let unreachable = Url::parse("http://127.0.0.1:1/").expect("URL");
        let dispatcher = EventDispatcher::new(vec![unreachable.clone()]).with_attempts(1);

        assert_eq!(
            dispatcher.dispatch(EventKind::NewBlock, &json!({})).await,
            vec![unreachable]
        );
    }
}
