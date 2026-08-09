//! Isolated execution oracle for the mainnet block that exposed task 086.
//!
//! The source is opened read-only and only establishes that the supplied scratch
//! is a faithful, fresh copy. All discards and execution happen in the scratch.
//! The fixture seam deliberately seeds no consensus authentication: this proves
//! the VM receipt and `RootPolicy::Verify`, not signer, tenure or VRF validity.

use std::{
    env, fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::SystemTime,
};

use nano_chainstate::{
    AppliedBlock, BitcoinBlockContext, ChainState, NakamotoBlock, TransactionReceipt,
    TransactionStatus, starts_new_tenure,
};
use nano_primitives::Network;
use nano_vm::BlockHeader;
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};

const PARENT_HEIGHT: u32 = 8_708_125;
const CHILD_HEIGHT: u32 = PARENT_HEIGHT + 1;
const BLOCK_FILE: &str = "block-8708126.bin";
const ORACLE_FILE: &str = "tx-823f-receipt.json";

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct OracleCost {
    read_count: u64,
    read_length: u64,
    runtime: u64,
    write_count: u64,
    write_length: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OracleResult {
    hex: String,
    repr: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Oracle {
    source: String,
    retrieved_at: String,
    txid: String,
    block_height: u64,
    block_hash: String,
    burn_block_height: u32,
    tx_index: usize,
    canonical: bool,
    status: String,
    result: OracleResult,
    cost: OracleCost,
    events: Vec<JsonValue>,
}

#[derive(Debug, PartialEq)]
struct Observation {
    state_root: [u8; 32],
    result_hex: String,
    result_repr: String,
    cost: OracleCost,
    events: Vec<JsonValue>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileStamp {
    device: u64,
    inode: u64,
    length: u64,
    modified: SystemTime,
}

#[derive(Debug)]
struct Inputs {
    source: PathBuf,
    scratch: PathBuf,
    source_stamps: Vec<(PathBuf, FileStamp)>,
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/mainnet/divergence")
}

fn file_stamp(path: &Path) -> FileStamp {
    let metadata = fs::metadata(path).expect("chainstate database metadata");
    assert!(metadata.is_file(), "{} is not a file", path.display());
    FileStamp {
        device: metadata.dev(),
        inode: metadata.ino(),
        length: metadata.len(),
        modified: metadata.modified().expect("database modification time"),
    }
}

fn inputs() -> Inputs {
    let source = fs::canonicalize(
        env::var_os("NANO_086_SOURCE").expect("NANO_086_SOURCE must name a chainstate directory"),
    )
    .expect("canonical NANO_086_SOURCE");
    let scratch = fs::canonicalize(
        env::var_os("NANO_086_SCRATCH")
            .expect("NANO_086_SCRATCH must name a fresh reflink chainstate directory"),
    )
    .expect("canonical NANO_086_SCRATCH");
    assert_ne!(
        source, scratch,
        "source and scratch resolve to the same path"
    );

    let source_directory = fs::metadata(&source).expect("source directory metadata");
    let scratch_directory = fs::metadata(&scratch).expect("scratch directory metadata");
    assert!(source_directory.is_dir(), "source is not a directory");
    assert!(scratch_directory.is_dir(), "scratch is not a directory");
    assert_ne!(
        (source_directory.dev(), source_directory.ino()),
        (scratch_directory.dev(), scratch_directory.ino()),
        "source and scratch are the same directory inode"
    );

    let source_stamps = ["marf.sqlite", "clarity.sqlite"]
        .into_iter()
        .map(|name| {
            let source_file = source.join(name);
            let scratch_file = scratch.join(name);
            let source_stamp = file_stamp(&source_file);
            let scratch_stamp = file_stamp(&scratch_file);
            assert_ne!(
                (source_stamp.device, source_stamp.inode),
                (scratch_stamp.device, scratch_stamp.inode),
                "source and scratch {name} share an inode"
            );
            (source_file, source_stamp)
        })
        .collect();
    Inputs {
        source,
        scratch,
        source_stamps,
    }
}

fn source_is_unchanged(inputs: &Inputs) {
    for (path, before) in &inputs.source_stamps {
        assert_eq!(&file_stamp(path), before, "{} changed", path.display());
    }
}

fn root(chainstate: &mut ChainState, block: [u8; 32]) -> [u8; 32] {
    chainstate
        .vm_mut()
        .root(block)
        .expect("read sealed root")
        .expect("block has a sealed root")
        .0
}

fn complete_header(chainstate: &ChainState, block: [u8; 32]) -> BlockHeader {
    chainstate
        .recorded_header(block)
        .expect("block has a complete recorded header")
}

fn context(parent: BlockHeader, child: BlockHeader) -> BitcoinBlockContext {
    let mut context = BitcoinBlockContext::at_height(u64::from(parent.burn_block_height));
    context.extend_view_to(u64::from(child.burn_block_height));
    context.first_height = 666_050;
    context.prepare_phase_length = 100;
    context.reward_phase_length = 2_000;
    context.rejection_fraction = 25;
    context.v1_unlock_height = 781_552;
    context.v2_unlock_height = 787_652;
    context.v3_unlock_height = 840_361;
    context.pox_5_activation_height = 960_230;
    context.burn_header_hash = child.burn_header_hash;
    context.burn_block_time = child.burn_block_time;
    context.vrf_seed = child.vrf_seed;
    context.burn_spend_total = child.burn_spend_total;
    context.burn_spend_winner = child.burn_spend_winner;
    context
}

fn validate_fixture(
    source: &mut ChainState,
    scratch: &mut ChainState,
    block: &NakamotoBlock,
    oracle: &Oracle,
) -> (BlockHeader, BlockHeader) {
    assert_eq!(source.network(), Network::MAINNET);
    assert_eq!(scratch.network(), Network::MAINNET);
    assert_eq!(
        scratch.tip().expect("scratch tip"),
        source.tip().expect("source tip")
    );
    assert_eq!(block.header.chain_length, u64::from(CHILD_HEIGHT));
    assert!(
        !starts_new_tenure(block),
        "fixture must be a tenure extension"
    );

    let parent = *block.header.parent_block_id.as_bytes();
    let child = *block.block_id().as_bytes();
    assert_eq!(
        source.height_of(parent).expect("source parent height"),
        Some(PARENT_HEIGHT)
    );
    assert_eq!(
        source.height_of(child).expect("source child height"),
        Some(CHILD_HEIGHT)
    );
    assert_eq!(
        scratch.height_of(parent).expect("scratch parent height"),
        Some(PARENT_HEIGHT)
    );
    assert_eq!(
        scratch.height_of(child).expect("scratch child height"),
        Some(CHILD_HEIGHT)
    );

    let source_parent = complete_header(source, parent);
    let source_child = complete_header(source, child);
    assert_eq!(complete_header(scratch, parent), source_parent);
    assert_eq!(complete_header(scratch, child), source_child);
    assert_eq!(root(scratch, parent), root(source, parent));
    assert_eq!(root(scratch, child), root(source, child));
    assert_eq!(
        root(source, child),
        *block.header.state_index_root.as_bytes()
    );
    assert_eq!(
        source_child.block_header_hash,
        *block.header.block_hash().as_bytes()
    );
    assert_eq!(
        source_child.consensus_hash,
        *block.header.consensus_hash.as_bytes()
    );
    assert_eq!(source_child.stacks_block_time, block.header.timestamp);

    assert_eq!(oracle.block_height, block.header.chain_length);
    assert_eq!(oracle.block_hash, block.header.block_hash().to_string());
    assert_eq!(oracle.burn_block_height, source_child.burn_block_height);
    assert!(oracle.canonical, "oracle transaction must be canonical");
    assert_eq!(oracle.status, "success");
    assert!(oracle.source.starts_with("https://api.hiro.so/"));
    assert!(!oracle.retrieved_at.is_empty());
    (source_parent, source_child)
}

fn normalized_event(event: &JsonValue) -> JsonValue {
    let event_type = event
        .get("type")
        .and_then(JsonValue::as_str)
        .expect("serialized event type");
    match event_type {
        "ft_transfer_event" => {
            let body = &event["ft_transfer_event"];
            json!({
                "kind": "fungible_token",
                "operation": "transfer",
                "asset": body["asset_identifier"],
                "sender": body["sender"],
                "recipient": body["recipient"],
                "amount": body["amount"],
            })
        }
        "ft_mint_event" => {
            let body = &event["ft_mint_event"];
            json!({
                "kind": "fungible_token",
                "operation": "mint",
                "asset": body["asset_identifier"],
                "recipient": body["recipient"],
                "amount": body["amount"],
            })
        }
        "contract_event" => {
            let body = &event["contract_event"];
            let value = body["raw_value"]
                .as_str()
                .expect("contract event raw value")
                .strip_prefix("0x")
                .expect("contract event value has a 0x prefix");
            json!({
                "kind": "contract_log",
                "contract": body["contract_identifier"],
                "topic": body["topic"],
                "value_hex": value,
            })
        }
        other => panic!("unexpected event type {other}"),
    }
}

fn observe(applied: &AppliedBlock, oracle: &Oracle) -> Observation {
    assert_eq!(
        applied.receipts.len(),
        6,
        "fixture transaction count changed"
    );
    let mut matches = applied
        .receipts
        .iter()
        .enumerate()
        .filter(|(_, receipt)| receipt.txid.to_string() == oracle.txid);
    let (index, receipt) = matches.next().expect("oracle transaction receipt exists");
    assert!(
        matches.next().is_none(),
        "oracle txid is unique in the block"
    );
    assert_eq!(index, oracle.tx_index);
    assert_eq!(receipt.status, TransactionStatus::Success);
    assert!(receipt.committed);
    receipt_observation(receipt, applied.execution.state_root.0)
}

fn receipt_observation(receipt: &TransactionReceipt, state_root: [u8; 32]) -> Observation {
    let value = receipt
        .result
        .value
        .as_ref()
        .expect("successful receipt has a result");
    let events = receipt
        .result
        .events
        .iter()
        .enumerate()
        .map(|(index, event)| {
            let serialized = event
                .json_serialize(index, &receipt.txid, receipt.committed)
                .expect("serialize receipt event");
            normalized_event(&serialized)
        })
        .collect();
    let cost = &receipt.result.cost;
    Observation {
        state_root,
        result_hex: value.serialize_to_hex().expect("serialize receipt result"),
        result_repr: value.to_string(),
        cost: OracleCost {
            read_count: cost.read_count,
            read_length: cost.read_length,
            runtime: cost.runtime,
            write_count: cost.write_count,
            write_length: cost.write_length,
        },
        events,
    }
}

fn replay_once(
    scratch: &mut ChainState,
    block: &NakamotoBlock,
    context: BitcoinBlockContext,
    oracle: &Oracle,
) -> Observation {
    assert!(
        scratch
            .discard_above(PARENT_HEIGHT)
            .expect("discard scratch above the parent")
            > 0,
        "scratch must contain the child before each replay"
    );
    let parent = block.header.parent_block_id;
    assert_eq!(
        scratch.tip().expect("scratch parent tip"),
        Some(*parent.as_bytes())
    );
    scratch
        .seed_unauthenticated_fixture_extension_from_parent_header(parent)
        .expect("seed fixture-only extension continuity from the parent header");
    let applied = scratch
        .append_unauthenticated_fixture_block_with_bitcoin_operations(
            context,
            &[],
            Some(*parent.as_bytes()),
            block,
        )
        .expect("execute fixture and verify its committed root");
    assert_eq!(
        scratch.tip().expect("scratch child tip"),
        Some(*block.block_id().as_bytes())
    );
    observe(&applied, oracle)
}

#[test]
#[ignore = "requires immutable mainnet source state and a fresh writable reflink scratch"]
fn the_mainnet_8708126_receipt_and_root_match_the_canonical_oracle() {
    let inputs = inputs();
    let fixture = fixtures();
    let block = NakamotoBlock::decode(&fs::read(fixture.join(BLOCK_FILE)).expect("fixture block"))
        .expect("decode fixture block");
    let oracle: Oracle = serde_json::from_slice(
        &fs::read(fixture.join(ORACLE_FILE)).expect("canonical receipt oracle"),
    )
    .expect("decode canonical receipt oracle");

    let mut source = ChainState::open_existing(&inputs.source).expect("open source read-only");
    let mut scratch = ChainState::open(source.network(), &inputs.scratch).expect("open scratch");
    let (parent, child) = validate_fixture(&mut source, &mut scratch, &block, &oracle);
    let context = context(parent, child);
    let first = replay_once(&mut scratch, &block, context, &oracle);
    let second = replay_once(&mut scratch, &block, context, &oracle);

    assert_eq!(first, second, "two scratch replays must be deterministic");
    assert_eq!(first.state_root, *block.header.state_index_root.as_bytes());
    assert_eq!(first.result_hex, oracle.result.hex);
    assert_eq!(first.result_repr, oracle.result.repr);
    assert_eq!(first.cost, oracle.cost);
    assert_eq!(first.events.len(), 8, "oracle event count changed");
    assert_eq!(first.events, oracle.events);
    source_is_unchanged(&inputs);
}
