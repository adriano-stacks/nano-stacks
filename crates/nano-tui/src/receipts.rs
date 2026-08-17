//! Live execution outcomes from the node's existing observer stream.

use std::{
    io::{BufRead, BufReader, Read},
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
    time::Duration,
};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::node::{self, Node};

const RECONNECT_AFTER: Duration = Duration::from_secs(1);
const STREAM_BUFFER: usize = 256;

pub struct Listener {
    updates: Receiver<Update>,
}

impl Listener {
    pub fn start(node: Node) -> Self {
        let (updates, receiver) = mpsc::sync_channel(STREAM_BUFFER);
        thread::spawn(move || listen(&node, &updates));
        Self { updates: receiver }
    }

    pub fn try_recv(&self) -> Option<Update> {
        self.updates.try_recv().ok()
    }
}

#[derive(Debug)]
pub enum Update {
    Connected,
    Disconnected(String),
    Event(StreamEvent),
}

#[derive(Debug)]
pub struct StreamEvent {
    pub sequence: Option<u64>,
    pub kind: String,
    pub data: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BlockOutcomes {
    pub index_block_hash: String,
    pub block_height: u64,
    pub burn_block_height: u64,
    pub transactions: Vec<TransactionOutcome>,
    #[serde(default)]
    events: Vec<Value>,
}

impl BlockOutcomes {
    pub fn parse(data: &str) -> Result<Self, String> {
        serde_json::from_str(data).map_err(|error| format!("invalid new_block event: {error}"))
    }

    pub fn transaction(&self, txid: &str) -> Option<TransactionView<'_>> {
        let outcome = self
            .transactions
            .iter()
            .find(|outcome| same_id(&outcome.txid, txid))?;
        let events = self
            .events
            .iter()
            .filter(|event| {
                event
                    .get("txid")
                    .and_then(Value::as_str)
                    .is_some_and(|event_txid| same_id(event_txid, txid))
            })
            .map(ReadableEvent)
            .collect();
        Some(TransactionView { outcome, events })
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct TransactionOutcome {
    pub txid: String,
    pub status: String,
    pub raw_result: Option<String>,
    pub vm_error: Option<String>,
    pub execution_cost: ExecutionCost,
}

impl TransactionOutcome {
    pub fn status(&self) -> &'static str {
        match (self.status.as_str(), self.vm_error.is_some()) {
            ("success", _) => "success · committed",
            (_, true) => "VM error · not committed",
            ("abort_by_response", false) => "aborted by response · not committed",
            ("abort_by_post_condition", false) => "aborted by post condition · not committed",
            (_, false) => "unknown outcome · commit state unavailable",
        }
    }

    pub fn result(&self) -> String {
        let Some(raw) = self.raw_result.as_deref() else {
            return "none".to_owned();
        };
        hex::decode(raw.trim_start_matches("0x")).map_or_else(
            |_| format!("{raw} (could not decode)"),
            |bytes| node::clarity_value(&bytes),
        )
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
pub struct ExecutionCost {
    pub write_length: u64,
    pub write_count: u64,
    pub read_length: u64,
    pub read_count: u64,
    pub runtime: u64,
}

pub struct TransactionView<'a> {
    pub outcome: &'a TransactionOutcome,
    pub events: Vec<ReadableEvent<'a>>,
}

pub struct ReadableEvent<'a>(&'a Value);

impl ReadableEvent<'_> {
    pub fn description(&self) -> String {
        let kind = text(self.0, "type").unwrap_or("event");
        let details = self
            .0
            .get(kind)
            .and_then(Value::as_object)
            .or_else(|| event_body(self.0));
        match (kind, details) {
            ("stx_transfer_event", Some(body)) => transfer(body, "uSTX", None),
            ("stx_mint_event", Some(body)) => mint(body, "uSTX", None),
            ("stx_burn_event", Some(body)) => burn(body, "uSTX", None),
            ("ft_transfer_event", Some(body)) => {
                transfer(body, "units", text_map(body, "asset_identifier"))
            }
            ("ft_mint_event", Some(body)) => {
                mint(body, "units", text_map(body, "asset_identifier"))
            }
            ("ft_burn_event", Some(body)) => {
                burn(body, "units", text_map(body, "asset_identifier"))
            }
            ("nft_transfer_event", Some(body)) => nft(body, "transfer"),
            ("nft_mint_event", Some(body)) => nft(body, "mint"),
            ("nft_burn_event", Some(body)) => nft(body, "burn"),
            ("contract_event", Some(body)) => format!(
                "contract {}::{} value {}",
                text_map(body, "contract_identifier").unwrap_or("unknown"),
                text_map(body, "topic").unwrap_or("unknown"),
                text_map(body, "raw_value").unwrap_or("unavailable")
            ),
            ("stx_lock_event", Some(body)) => format!(
                "lock {} uSTX from {} until burn height {}",
                text_map(body, "locked_amount").unwrap_or("?"),
                text_map(body, "locked_address").unwrap_or("unknown"),
                text_map(body, "unlock_height").unwrap_or("?")
            ),
            (_, Some(body)) => summarize(kind, body),
            (_, None) => kind.replace('_', " "),
        }
    }

    pub fn index(&self) -> Option<u64> {
        self.0.get("event_index").and_then(Value::as_u64)
    }
}

fn listen(node: &Node, updates: &SyncSender<Update>) {
    loop {
        match node.events() {
            Ok(reader) => {
                if updates.send(Update::Connected).is_err() {
                    return;
                }
                if let Err(error) = read_stream(reader, updates)
                    && updates.send(Update::Disconnected(error)).is_err()
                {
                    return;
                }
            }
            Err(error) => {
                if updates.send(Update::Disconnected(error)).is_err() {
                    return;
                }
            }
        }
        thread::sleep(RECONNECT_AFTER);
    }
}

fn read_stream(reader: impl Read, updates: &SyncSender<Update>) -> Result<(), String> {
    let mut reader = BufReader::new(reader);
    let mut frame = Frame::default();
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| format!("/events read: {error}"))?;
        if read == 0 {
            dispatch(&mut frame, updates)?;
            return Err("/events closed by the node".to_owned());
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            dispatch(&mut frame, updates)?;
        } else if !line.starts_with(':') {
            frame.push(line);
        }
    }
}

#[derive(Default)]
struct Frame {
    event: Option<String>,
    id: Option<String>,
    data: Vec<String>,
}

impl Frame {
    fn push(&mut self, line: &str) {
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = Some(value.to_owned()),
            "id" => self.id = Some(value.to_owned()),
            "data" => self.data.push(value.to_owned()),
            _ => {}
        }
    }
}

fn dispatch(frame: &mut Frame, updates: &SyncSender<Update>) -> Result<(), String> {
    if frame.event.is_none() && frame.id.is_none() && frame.data.is_empty() {
        return Ok(());
    }
    let frame = std::mem::take(frame);
    updates
        .send(Update::Event(StreamEvent {
            sequence: frame.id.and_then(|id| id.parse().ok()),
            kind: frame.event.unwrap_or_else(|| "message".to_owned()),
            data: frame.data.join("\n"),
        }))
        .map_err(|_| "receipt view closed".to_owned())
}

fn event_body(event: &Value) -> Option<&Map<String, Value>> {
    event.as_object()?.iter().find_map(|(name, value)| {
        (name.ends_with("_event") && name != "type")
            .then(|| value.as_object())
            .flatten()
    })
}

fn transfer(body: &Map<String, Value>, unit: &str, asset: Option<&str>) -> String {
    format!(
        "transfer {} {} from {} to {}",
        text_map(body, "amount").unwrap_or("?"),
        asset.map_or_else(|| unit.to_owned(), |asset| format!("{unit} of {asset}")),
        text_map(body, "sender").unwrap_or("unknown"),
        text_map(body, "recipient").unwrap_or("unknown")
    )
}

fn mint(body: &Map<String, Value>, unit: &str, asset: Option<&str>) -> String {
    format!(
        "mint {} {} to {}",
        text_map(body, "amount").unwrap_or("?"),
        asset.map_or_else(|| unit.to_owned(), |asset| format!("{unit} of {asset}")),
        text_map(body, "recipient").unwrap_or("unknown")
    )
}

fn burn(body: &Map<String, Value>, unit: &str, asset: Option<&str>) -> String {
    format!(
        "burn {} {} from {}",
        text_map(body, "amount").unwrap_or("?"),
        asset.map_or_else(|| unit.to_owned(), |asset| format!("{unit} of {asset}")),
        text_map(body, "sender").unwrap_or("unknown")
    )
}

fn nft(body: &Map<String, Value>, operation: &str) -> String {
    let endpoints = match operation {
        "transfer" => format!(
            " from {} to {}",
            text_map(body, "sender").unwrap_or("unknown"),
            text_map(body, "recipient").unwrap_or("unknown")
        ),
        "mint" => format!(" to {}", text_map(body, "recipient").unwrap_or("unknown")),
        "burn" => format!(" from {}", text_map(body, "sender").unwrap_or("unknown")),
        _ => String::new(),
    };
    format!(
        "{operation} NFT {} value {}{endpoints}",
        text_map(body, "asset_identifier").unwrap_or("unknown"),
        text_map(body, "raw_value").unwrap_or("unavailable")
    )
}

fn summarize(kind: &str, body: &Map<String, Value>) -> String {
    let fields = body
        .iter()
        .filter_map(|(name, value)| scalar(value).map(|value| format!("{name}={value}")))
        .collect::<Vec<_>>()
        .join(" · ");
    format!("{} {fields}", kind.replace('_', " "))
}

fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

fn text<'a>(value: &'a Value, name: &str) -> Option<&'a str> {
    value.get(name).and_then(Value::as_str)
}

fn text_map<'a>(value: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    value.get(name).and_then(Value::as_str)
}

fn same_id(left: &str, right: &str) -> bool {
    left.trim_start_matches("0x")
        .eq_ignore_ascii_case(right.trim_start_matches("0x"))
}

#[cfg(test)]
mod tests {
    use std::{io::Cursor, sync::mpsc};

    use super::{BlockOutcomes, Update, read_stream};

    const BLOCK: &str = r#"{
      "index_block_hash":"0xblock", "block_height":42, "burn_block_height":30,
      "transactions":[
        {"txid":"0xsuccess","status":"success","raw_result":"0x0703","vm_error":null,
         "execution_cost":{"write_length":1,"write_count":2,"read_length":3,"read_count":4,"runtime":5}},
        {"txid":"0xabort","status":"abort_by_response","raw_result":null,"vm_error":null,
         "execution_cost":{"write_length":0,"write_count":0,"read_length":0,"read_count":0,"runtime":0}},
        {"txid":"0xerror","status":"abort_by_response","raw_result":null,"vm_error":"division by zero",
         "execution_cost":{"write_length":0,"write_count":0,"read_length":0,"read_count":0,"runtime":7}}
      ],
      "events":[
        {"txid":"0xsuccess","event_index":0,"type":"stx_transfer_event",
         "stx_transfer_event":{"amount":"9","sender":"A","recipient":"B"}},
        {"txid":"0xsuccess","event_index":1,"type":"ft_mint_event",
         "ft_mint_event":{"amount":"2","asset_identifier":"C.token::coin","recipient":"D"}},
        {"txid":"0xsuccess","event_index":2,"type":"nft_transfer_event",
         "nft_transfer_event":{"asset_identifier":"C.nft::item","raw_value":"0x01","sender":"D","recipient":"E"}},
        {"txid":"0xsuccess","event_index":3,"type":"contract_event",
         "contract_event":{"contract_identifier":"C.app","topic":"print","raw_value":"0x03"}}
      ]
    }"#;

    #[test]
    fn outcomes_keep_success_abort_error_event_order_and_empty() {
        let block = BlockOutcomes::parse(BLOCK).expect("block event");
        let success = block.transaction("success").expect("successful outcome");
        assert_eq!(success.outcome.status(), "success · committed");
        assert_eq!(success.outcome.result(), "(ok true)");
        assert_eq!(success.events.len(), 4);
        assert!(success.events[0].description().contains("9 uSTX"));
        assert!(success.events[1].description().contains("C.token::coin"));
        assert!(success.events[2].description().contains("NFT C.nft::item"));
        assert!(success.events[3].description().contains("C.app::print"));

        let abort = block.transaction("abort").expect("aborted outcome");
        assert_eq!(
            abort.outcome.status(),
            "aborted by response · not committed"
        );
        assert!(abort.events.is_empty(), "known empty event list");

        let error = block.transaction("error").expect("failed outcome");
        assert_eq!(error.outcome.status(), "VM error · not committed");
        assert_eq!(error.outcome.vm_error.as_deref(), Some("division by zero"));
    }

    #[test]
    fn sse_frames_retain_sequence_kind_and_multiline_data() {
        let input = Cursor::new("id: 7\nevent: new_block\ndata: {\"a\":\ndata: 1}\n\n");
        let (updates, receiver) = mpsc::sync_channel(1);
        let error = read_stream(input, &updates).expect_err("EOF closes a stream");
        assert!(error.contains("closed"));
        let Update::Event(event) = receiver.recv().expect("event") else {
            panic!("expected stream event");
        };
        assert_eq!(event.sequence, Some(7));
        assert_eq!(event.kind, "new_block");
        assert_eq!(event.data, "{\"a\":\n1}");
    }
}
