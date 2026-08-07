//! Everything the dashboard knows, read from one node over its own public RPC.
//!
//! One poll builds one snapshot, and a field the node could not answer is `None`
//! rather than a zero: a dashboard that shows `0` for a height it failed to fetch
//! teaches its reader to distrust every other number on the screen.

use std::time::Duration;

use serde::Deserialize;

/// How long any one request may take.
///
/// Short on purpose. A node mid-round can be slow to answer, and a dashboard that
/// blocks on it stops redrawing — which looks exactly like the node being dead.
const TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Clone, Debug, Default, Deserialize)]
pub struct SyncStatus {
    /// What a peer said it had, which is not what this node did with it.
    pub followed_stacks_height: Option<u64>,
    /// The tip this node's own fork choice picked out of what peers offered.
    pub selected_stacks_height: Option<u64>,
    pub selected_from_peer: Option<String>,
    /// The only one that means this node computed anything.
    pub executed_stacks_height: Option<u64>,
    pub executed_stacks_tip: Option<String>,
    pub executed_state_index_root: Option<String>,
    pub blocks_behind: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct NodeInfo {
    pub burn_block_height: Option<u64>,
    // `stacks_tip_height` is deliberately not read: `/nano/sync_status` names the
    // same number as *executed*, and two sources for one fact is how a dashboard
    // comes to show a height nothing computed.
    pub stacks_tip_consensus_hash: Option<String>,
    pub server_version: Option<String>,
    pub network_id: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Sortition {
    pub burn_block_height: Option<u64>,
    pub consensus_hash: Option<String>,
    #[serde(rename = "was_sortition")]
    pub elected: Option<bool>,
    pub miner_pk_hash160: Option<String>,
    pub committed_block_hash: Option<String>,
    pub last_sortition_ch: Option<String>,
    pub vrf_seed: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct TenureInfo {
    pub consensus_hash: Option<String>,
    pub tenure_start_block_id: Option<String>,
    pub parent_consensus_hash: Option<String>,
    pub tip_height: Option<u64>,
    pub reward_cycle: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Pox {
    pub current_cycle: Option<Cycle>,
    pub next_cycle: Option<NextCycle>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Cycle {
    pub id: Option<u64>,
    pub stacked_ustx: Option<u128>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct NextCycle {
    pub id: Option<u64>,
    pub blocks_until_prepare_phase: Option<i64>,
    pub blocks_until_reward_phase: Option<i64>,
}

/// One block as the explorer shows it, decoded with the node's own codec.
#[derive(Clone, Debug)]
pub struct Block {
    pub height: u64,
    pub id: String,
    /// What this block was built on, so the explorer can walk back and fill in the
    /// blocks the node executed between two polls rather than sampling its tip.
    pub parent_id: String,
    pub consensus_hash: String,
    pub state_index_root: String,
    pub transactions: Vec<Transaction>,
    pub signatures: usize,
    pub timestamp: u64,
}

#[derive(Clone, Debug)]
pub struct Transaction {
    pub txid: String,
    pub kind: String,
    pub detail: String,
}

/// A node, and the answers it gave to the last poll.
pub struct Node {
    url: String,
    agent: ureq::Agent,
}

impl Node {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.trim_end_matches('/').to_owned(),
            agent: ureq::AgentBuilder::new()
                .timeout(TIMEOUT)
                .user_agent("nano-tui")
                .build(),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    fn json<T: for<'a> Deserialize<'a>>(&self, route: &str) -> Option<T> {
        self.agent
            .get(&format!("{}{route}", self.url))
            .call()
            .ok()?
            .into_json()
            .ok()
    }

    pub fn sync_status(&self) -> Option<SyncStatus> {
        self.json("/nano/sync_status")
    }

    pub fn info(&self) -> Option<NodeInfo> {
        self.json("/v2/info")
    }

    pub fn pox(&self) -> Option<Pox> {
        self.json("/v2/pox")
    }

    pub fn tenure(&self) -> Option<TenureInfo> {
        self.json("/v3/tenures/info")
    }

    /// The pair, because that is what the route serves and what a signer reads.
    pub fn sortitions(&self) -> Option<Vec<Sortition>> {
        self.json("/v3/sortitions/latest_and_last")
            .or_else(|| self.json("/v3/sortitions"))
    }

    /// A block, decoded rather than described: the bytes are consensus-serialized
    /// and `nano-codec` is what the node itself reads them with.
    pub fn block(&self, block_id: &str, height: u64) -> Option<Block> {
        let mut bytes = Vec::new();
        self.agent
            .get(&format!("{}/v3/blocks/{block_id}", self.url))
            .call()
            .ok()?
            .into_reader()
            .read_to_end(&mut bytes)
            .ok()?;
        decode(&bytes, height)
    }
}

fn decode(bytes: &[u8], height: u64) -> Option<Block> {
    use nano_chainstate::NakamotoBlock;
    use nano_codec::TransactionPayloadData;

    let block = NakamotoBlock::decode(bytes).ok()?;
    let transactions = block
        .transactions
        .iter()
        .map(|transaction| {
            let (kind, detail) = match transaction.payload().data() {
                TransactionPayloadData::TokenTransfer { amount, .. } => {
                    ("transfer".to_owned(), format!("{amount} uSTX"))
                }
                TransactionPayloadData::ContractCall {
                    address,
                    contract_name,
                    function_name,
                    arguments,
                } => (
                    "call".to_owned(),
                    format!(
                        "{address}.{contract_name}::{function_name} ({} args)",
                        arguments.len()
                    ),
                ),
                TransactionPayloadData::SmartContract { contract_name, .. } => {
                    ("deploy".to_owned(), contract_name.clone())
                }
                TransactionPayloadData::VersionedSmartContract {
                    contract_name,
                    clarity_version,
                    ..
                } => (
                    "deploy".to_owned(),
                    format!("{contract_name} ({clarity_version:?})"),
                ),
                TransactionPayloadData::Coinbase { .. }
                | TransactionPayloadData::CoinbaseToAltRecipient { .. }
                | TransactionPayloadData::NakamotoCoinbase { .. } => {
                    ("coinbase".to_owned(), String::new())
                }
                TransactionPayloadData::TenureChange(payload) => (
                    "tenure".to_owned(),
                    format!("{:?} after {} blocks", payload.cause, payload.previous_tenure_blocks),
                ),
                TransactionPayloadData::PoisonMicroblock { .. } => {
                    ("poison".to_owned(), String::new())
                }
            };
            Transaction {
                txid: transaction.txid().to_string(),
                kind,
                detail,
            }
        })
        .collect();
    Some(Block {
        height: if height > 0 { height } else { block.header.chain_length },
        id: block.block_id().to_string(),
        parent_id: block.header.parent_block_id.to_string(),
        consensus_hash: block.header.consensus_hash.to_string(),
        state_index_root: block.header.state_index_root.to_string(),
        signatures: block.header.signer_signatures.len(),
        timestamp: block.header.timestamp,
        transactions,
    })
}
