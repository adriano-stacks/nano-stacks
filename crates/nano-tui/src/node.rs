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
    /// The pool the sync source was picked from.
    pub fetching_from_peers: Option<Vec<String>>,
    pub p2p_sessions: Option<u64>,
    pub p2p_known_peers: Option<u64>,
    /// The only one that means this node computed anything.
    pub executed_stacks_height: Option<u64>,
    pub executed_stacks_tip: Option<String>,
    pub blocks_behind: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct NodeInfo {
    pub burn_block_height: Option<u64>,
    pub server_version: Option<String>,
    pub network_id: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Sortition {
    pub burn_block_hash: Option<String>,
    pub burn_block_height: Option<u64>,
    pub burn_header_timestamp: Option<u64>,
    pub consensus_hash: Option<String>,
    #[serde(rename = "was_sortition")]
    pub elected: Option<bool>,
    pub miner_pk_hash160: Option<String>,
    pub stacks_parent_ch: Option<String>,
    pub committed_block_hash: Option<String>,
    pub vrf_seed: Option<String>,
    pub mining_competition: Option<MiningCompetition>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct MiningCompetition {
    pub winner_txid: Option<String>,
    pub block_burn_sats: u64,
    pub window_median_burn_sats: u64,
    pub sampled_window_blocks: u8,
    #[serde(default)]
    pub participants: Vec<SortitionParticipant>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct SortitionParticipant {
    pub txid: String,
    pub signing_key_hash: Option<String>,
    pub vrf_public_key: Option<String>,
    pub committed_block_hash: String,
    pub burn_sats: u64,
    pub effective_burn_sats: u64,
    pub median_burn_sats: u64,
    pub frequency: u8,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct TenureInfo {
    pub consensus_hash: Option<String>,
    pub tenure_start_block_id: Option<String>,
    pub parent_consensus_hash: Option<String>,
    pub parent_tenure_start_block_id: Option<String>,
    pub tip_height: Option<u64>,
    pub reward_cycle: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Pox {
    pub current_cycle: Option<Cycle>,
    pub next_cycle: Option<NextCycle>,
    pub current_epoch: Option<String>,
    #[serde(default)]
    pub epochs: Vec<Epoch>,
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

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Epoch {
    pub epoch_id: Option<String>,
    pub block_limit: Option<ExecutionBudget>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct ExecutionBudget {
    pub write_length: Option<u64>,
    pub write_count: Option<u64>,
    pub read_length: Option<u64>,
    pub read_count: Option<u64>,
    pub runtime: Option<u64>,
}

impl Pox {
    pub fn current_budget(&self) -> Option<ExecutionBudget> {
        let current = self.current_epoch.as_deref()?;
        self.epochs
            .iter()
            .find(|epoch| epoch.epoch_id.as_deref() == Some(current))
            .and_then(|epoch| epoch.block_limit)
    }
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
    pub summary: String,
    pub origin: Option<String>,
    pub sponsor: Option<String>,
    pub origin_nonce: u64,
    pub sponsor_nonce: Option<u64>,
    pub fee: u64,
    pub authorization: String,
    pub version: String,
    pub chain_id: u32,
    pub anchor_mode: String,
    pub post_condition_mode: String,
    pub post_conditions: usize,
    pub tenure_change: Option<TenureChange>,
    pub fields: Vec<(String, String)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TenureChange {
    pub is_extension: bool,
    pub cause: String,
    pub reset: String,
    pub previous_blocks: u32,
}

/// A node, and the answers it gave to the last poll.
#[derive(Clone)]
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

    fn json<T: for<'a> Deserialize<'a>>(&self, route: &str) -> Result<T, String> {
        self.agent
            .get(&format!("{}{route}", self.url))
            .call()
            .map_err(|error| format!("{route}: {error}"))?
            .into_json()
            .map_err(|error| format!("{route}: {error}"))
    }

    pub fn sync_status(&self) -> Result<SyncStatus, String> {
        self.json("/nano/sync_status")
    }

    pub fn info(&self) -> Result<NodeInfo, String> {
        self.json("/v2/info")
    }

    pub fn pox(&self) -> Result<Pox, String> {
        self.json("/v2/pox")
    }

    pub fn tenure(&self) -> Result<TenureInfo, String> {
        self.json("/v3/tenures/info")
    }

    /// The pair, because that is what the route serves and what a signer reads.
    pub fn sortitions(&self) -> Result<Vec<Sortition>, String> {
        self.json("/v3/sortitions/latest_and_last")
            .or_else(|_| self.json("/v3/sortitions"))
    }

    /// A block, decoded rather than described: the bytes are consensus-serialized
    /// and `nano-codec` is what the node itself reads them with.
    pub fn block(&self, block_id: &str, height: u64) -> Result<Block, String> {
        let mut bytes = Vec::new();
        self.agent
            .get(&format!("{}/v3/blocks/{block_id}", self.url))
            .call()
            .map_err(|error| format!("/v3/blocks/{block_id}: {error}"))?
            .into_reader()
            .read_to_end(&mut bytes)
            .map_err(|error| format!("/v3/blocks/{block_id}: {error}"))?;
        decode(&bytes, height).ok_or_else(|| format!("/v3/blocks/{block_id}: invalid block bytes"))
    }
}

fn decode(bytes: &[u8], height: u64) -> Option<Block> {
    use nano_chainstate::NakamotoBlock;
    use nano_codec::TransactionAuth;

    let block = NakamotoBlock::decode(bytes).ok()?;
    let transactions = block
        .transactions
        .iter()
        .map(|transaction| {
            let tenure_change = match transaction.payload().data() {
                nano_codec::TransactionPayloadData::TenureChange(payload) => {
                    Some(tenure_change(payload))
                }
                _ => None,
            };
            let (kind, summary, fields) = payload_details(transaction.payload().data());
            let (authorization, sponsor_nonce) = match transaction.auth() {
                TransactionAuth::Standard(origin) => (condition_name(origin), None),
                TransactionAuth::Sponsored { origin, sponsor } => (
                    format!(
                        "sponsored; origin {}; sponsor {}",
                        condition_name(origin),
                        condition_name(sponsor)
                    ),
                    Some(sponsor.nonce()),
                ),
            };
            Transaction {
                txid: transaction.txid().to_string(),
                kind,
                summary,
                origin: transaction
                    .origin_address()
                    .map(|address| address.to_string()),
                sponsor: transaction
                    .sponsor_address()
                    .map(|address| address.to_string()),
                origin_nonce: transaction.auth().origin().nonce(),
                sponsor_nonce,
                fee: transaction.auth().payer().fee(),
                authorization,
                version: format!("{:?}", transaction.version()),
                chain_id: transaction.chain_id(),
                anchor_mode: format!("{:?}", transaction.anchor_mode()),
                post_condition_mode: format!("{:?}", transaction.post_condition_mode()),
                post_conditions: transaction.post_condition_count(),
                tenure_change,
                fields,
            }
        })
        .collect();
    Some(Block {
        height: if height > 0 {
            height
        } else {
            block.header.chain_length
        },
        id: block.block_id().to_string(),
        parent_id: block.header.parent_block_id.to_string(),
        consensus_hash: block.header.consensus_hash.to_string(),
        state_index_root: block.header.state_index_root.to_string(),
        signatures: block.header.signer_signatures.len(),
        timestamp: block.header.timestamp,
        transactions,
    })
}

fn condition_name(condition: &nano_codec::SpendingCondition) -> String {
    match condition {
        nano_codec::SpendingCondition::Singlesig(_) => "single signature".to_owned(),
        nano_codec::SpendingCondition::Multisig(condition) => format!(
            "ordered multisig ({} required, {} fields)",
            condition.signatures_required,
            condition.fields.len()
        ),
        nano_codec::SpendingCondition::OrderIndependentMultisig(condition) => format!(
            "order-independent multisig ({} required, {} fields)",
            condition.signatures_required,
            condition.fields.len()
        ),
    }
}

fn payload_details(
    payload: &nano_codec::TransactionPayloadData,
) -> (String, String, Vec<(String, String)>) {
    use nano_codec::TransactionPayloadData;

    match payload {
        TransactionPayloadData::TokenTransfer {
            recipient,
            amount,
            memo,
        } => transfer_details(recipient, *amount, memo),
        TransactionPayloadData::ContractCall {
            address,
            contract_name,
            function_name,
            arguments,
        } => contract_call_details(
            &format!("{address}.{contract_name}"),
            function_name,
            arguments,
        ),
        TransactionPayloadData::SmartContract {
            contract_name,
            source,
        } => (
            "deploy".to_owned(),
            contract_name.clone(),
            vec![
                ("contract".to_owned(), contract_name.clone()),
                ("source".to_owned(), source.clone()),
            ],
        ),
        TransactionPayloadData::VersionedSmartContract {
            clarity_version,
            contract_name,
            source,
        } => (
            "deploy".to_owned(),
            format!("{contract_name} ({clarity_version:?})"),
            vec![
                ("contract".to_owned(), contract_name.clone()),
                ("clarity".to_owned(), format!("{clarity_version:?}")),
                ("source".to_owned(), source.clone()),
            ],
        ),
        TransactionPayloadData::Coinbase { payload } => (
            "coinbase".to_owned(),
            String::new(),
            vec![("payload".to_owned(), format!("0x{}", hex::encode(payload)))],
        ),
        TransactionPayloadData::CoinbaseToAltRecipient { payload, recipient } => (
            "coinbase".to_owned(),
            format!("to {}", principal(recipient)),
            vec![
                ("recipient".to_owned(), principal(recipient)),
                ("payload".to_owned(), format!("0x{}", hex::encode(payload))),
            ],
        ),
        TransactionPayloadData::NakamotoCoinbase {
            payload,
            recipient,
            vrf_proof,
        } => nakamoto_coinbase_details(payload, recipient.as_ref(), vrf_proof),
        TransactionPayloadData::TenureChange(payload) => tenure_change_details(payload),
        TransactionPayloadData::PoisonMicroblock { first, second } => (
            "poison".to_owned(),
            format!("microblock sequence {}", first.sequence),
            vec![
                ("sequence".to_owned(), first.sequence.to_string()),
                ("first parent".to_owned(), first.previous_block.to_string()),
                (
                    "second parent".to_owned(),
                    second.previous_block.to_string(),
                ),
            ],
        ),
    }
}

fn transfer_details(
    recipient: &nano_codec::Principal,
    amount: u64,
    memo: &[u8; 34],
) -> (String, String, Vec<(String, String)>) {
    (
        "transfer".to_owned(),
        format!("{amount} uSTX to {}", principal(recipient)),
        vec![
            ("recipient".to_owned(), principal(recipient)),
            ("amount".to_owned(), format!("{amount} uSTX")),
            ("memo".to_owned(), memo_text(memo)),
        ],
    )
}

fn contract_call_details(
    contract: &str,
    function: &str,
    arguments: &[nano_codec::ClarityValue],
) -> (String, String, Vec<(String, String)>) {
    let mut fields = vec![
        ("contract".to_owned(), contract.to_owned()),
        ("function".to_owned(), function.to_owned()),
    ];
    fields.extend(arguments.iter().enumerate().map(|(index, argument)| {
        (
            format!("argument {index}"),
            clarity_value(argument.as_bytes()),
        )
    }));
    ("call".to_owned(), format!("{contract}::{function}"), fields)
}

fn nakamoto_coinbase_details(
    payload: &[u8; 32],
    recipient: Option<&nano_codec::Principal>,
    vrf_proof: &[u8; 80],
) -> (String, String, Vec<(String, String)>) {
    let recipient = recipient.map(principal);
    (
        "coinbase".to_owned(),
        recipient
            .as_ref()
            .map_or_else(String::new, |recipient| format!("to {recipient}")),
        vec![
            (
                "recipient".to_owned(),
                recipient.unwrap_or_else(|| "default miner recipient".to_owned()),
            ),
            ("payload".to_owned(), format!("0x{}", hex::encode(payload))),
            (
                "VRF proof".to_owned(),
                format!("0x{}", hex::encode(vrf_proof)),
            ),
        ],
    )
}

fn tenure_change_details(
    payload: &nano_codec::TenureChangePayload,
) -> (String, String, Vec<(String, String)>) {
    (
        "tenure".to_owned(),
        format!(
            "{:?} after {} blocks",
            payload.cause, payload.previous_tenure_blocks
        ),
        vec![
            ("cause".to_owned(), format!("{:?}", payload.cause)),
            (
                "tenure".to_owned(),
                payload.tenure_consensus_hash.to_string(),
            ),
            (
                "previous tenure".to_owned(),
                payload.previous_tenure_consensus_hash.to_string(),
            ),
            (
                "bitcoin view".to_owned(),
                payload.bitcoin_view_consensus_hash.to_string(),
            ),
            (
                "previous end".to_owned(),
                payload.previous_tenure_end.to_string(),
            ),
            (
                "previous blocks".to_owned(),
                payload.previous_tenure_blocks.to_string(),
            ),
            (
                "miner key hash".to_owned(),
                payload.public_key_hash.to_string(),
            ),
        ],
    )
}

fn tenure_change(payload: &nano_codec::TenureChangePayload) -> TenureChange {
    use nano_codec::TenureChangeCause;

    let (cause, reset) = match payload.cause {
        TenureChangeCause::BlockFound => ("tenure started", "all dimensions"),
        TenureChangeCause::Extended => ("tenure extended", "all dimensions"),
        TenureChangeCause::ExtendedRuntime => ("runtime limit reached", "runtime"),
        TenureChangeCause::ExtendedReadCount => ("read-count limit reached", "read count"),
        TenureChangeCause::ExtendedReadLength => ("read-size limit reached", "read size"),
        TenureChangeCause::ExtendedWriteCount => ("write-count limit reached", "write count"),
        TenureChangeCause::ExtendedWriteLength => ("write-size limit reached", "write size"),
    };
    TenureChange {
        is_extension: payload.cause != TenureChangeCause::BlockFound,
        cause: cause.to_owned(),
        reset: reset.to_owned(),
        previous_blocks: payload.previous_tenure_blocks,
    }
}

fn principal(principal: &nano_codec::Principal) -> String {
    match principal {
        nano_codec::Principal::Standard(address) => address.to_string(),
        nano_codec::Principal::Contract {
            address,
            contract_name,
        } => format!("{address}.{contract_name}"),
    }
}

fn clarity_value(bytes: &[u8]) -> String {
    let mut input = bytes;
    match clarity::vm::Value::deserialize_read(&mut input, None, false) {
        Ok(value) if input.is_empty() => value.to_string(),
        Ok(_) | Err(_) => format!("0x{} (could not decode)", hex::encode(bytes)),
    }
}

fn memo_text(memo: &[u8; 34]) -> String {
    let memo = memo.strip_suffix(&[0]).unwrap_or(memo);
    let memo = &memo[..memo
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |at| at + 1)];
    if memo.is_empty() {
        return "empty".to_owned();
    }
    std::str::from_utf8(memo).map_or_else(
        |_| format!("0x{}", hex::encode(memo)),
        |text| {
            if text.chars().any(char::is_control) {
                format!("0x{}", hex::encode(memo))
            } else {
                text.to_owned()
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{clarity_value, decode, memo_text};

    #[test]
    fn clarity_arguments_are_human_readable() {
        let mut uint = vec![1];
        uint.extend_from_slice(&42_u128.to_be_bytes());
        assert_eq!(clarity_value(&uint), "u42");
        assert_eq!(clarity_value(&[3]), "true");
    }

    #[test]
    fn transfer_memos_are_text_or_hex() {
        let mut text = [0; 34];
        text[..5].copy_from_slice(b"hello");
        assert_eq!(memo_text(&text), "hello");

        let mut binary = [0; 34];
        binary[0] = 0xff;
        assert_eq!(memo_text(&binary), "0xff");
    }

    #[test]
    fn a_mainnet_call_keeps_its_function_and_arguments() {
        let fixtures: [&[u8]; 6] = [
            include_bytes!(
                "../../nano-conformance/fixtures/mainnet/blocks/65254a9885c773777269b2f0a5d93d03629a791443d33f9a47ad8391983d36f5.bin"
            ),
            include_bytes!(
                "../../nano-conformance/fixtures/mainnet/blocks/f54950f0b58dd90ac7dda9f48d4c2005ff94c1c2810b5741ce708c01a59bb491.bin"
            ),
            include_bytes!(
                "../../nano-conformance/fixtures/mainnet/blocks/23dc0ca49c1ba4225fabeae02a59683e1d71c580b53bb697debfd1af310fb80c.bin"
            ),
            include_bytes!(
                "../../nano-conformance/fixtures/mainnet/blocks/39739e620f31c1c1518b46975950c7c48bb3f111a06bfd0f78025362ebeed0ce.bin"
            ),
            include_bytes!(
                "../../nano-conformance/fixtures/mainnet/blocks/d3f249acd193124aa06fd7eac6d800c39068950383afdef1fa78145715fcffc8.bin"
            ),
            include_bytes!("../../nano-conformance/fixtures/mainnet/checkpoint-block.bin"),
        ];
        let call = fixtures
            .into_iter()
            .find_map(|bytes| {
                decode(bytes, 0).and_then(|block| {
                    block
                        .transactions
                        .into_iter()
                        .find(|transaction| transaction.kind == "call")
                })
            })
            .expect("the mainnet fixtures contain a contract call");

        assert!(call.fields.iter().any(|(name, _)| name == "contract"));
        assert!(call.fields.iter().any(|(name, _)| name == "function"));
        let arguments: Vec<_> = call
            .fields
            .iter()
            .filter(|(name, _)| name.starts_with("argument "))
            .collect();
        assert!(!arguments.is_empty());
        assert!(
            arguments
                .iter()
                .all(|(_, value)| !value.contains("could not decode"))
        );
    }
}
