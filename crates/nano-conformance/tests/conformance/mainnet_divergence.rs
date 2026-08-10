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

use clarity::vm::{
    costs::ExecutionCost,
    types::{PrincipalData, QualifiedContractIdentifier},
};
use nano_chainstate::{
    AppliedBlock, BitcoinBlockContext, ChainState, NakamotoBlock, TransactionReceipt,
    TransactionStatus, starts_new_tenure,
};
use nano_codec::TransactionPayloadData;
use nano_primitives::Network;
use nano_vm::{BlockHeader, ContractCallOutcome, TransactionResult};
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};
use stacks_common::types::StacksEpochId;

const TASK_064_PARENT_HEIGHT: u32 = 8_686_665;
const TASK_064_CHILD_HEIGHT: u32 = TASK_064_PARENT_HEIGHT + 1;
const TASK_064_BLOCK_FILE: &str = "block-8686666.hex";
const TASK_064_ORACLE_FILE: &str = "tx-f338-receipt.json";
const TASK_064_OLD_CONTRACT: &str = "SP4SZE494VC2YC5JYG7AYFQ44F5Q4PYV7DVMDPBG.reserve-v1";

const TASK_086_PARENT_HEIGHT: u32 = 8_708_125;
const TASK_086_CHILD_HEIGHT: u32 = TASK_086_PARENT_HEIGHT + 1;
const TASK_086_BLOCK_FILE: &str = "block-8708126.bin";
const TASK_086_ORACLE_FILE: &str = "tx-823f-receipt.json";
const TASK_098_PARENT_HEIGHT: u32 = 8_724_864;
const TASK_098_CHILD_HEIGHT: u32 = TASK_098_PARENT_HEIGHT + 1;
const TASK_098_BLOCK_FILE: &str = "block-8724865.hex";
const TASK_098_ORACLE_FILE: &str = "tx-24d632-receipt.json";
const TASK_111_PARENT_HEIGHT: u32 = 8_733_928;
const TASK_111_CHILD_HEIGHT: u32 = TASK_111_PARENT_HEIGHT + 1;
const TASK_111_BLOCK_FILE: &str = "block-8733929.hex";
const TASK_111_ORACLE_FILE: &str = "tx-6f8b-receipt.json";

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
    #[serde(default)]
    index_block_hash: Option<String>,
    #[serde(default)]
    parent_index_block_hash: Option<String>,
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

fn inputs(source_variable: &str, scratch_variable: &str) -> Inputs {
    let source = fs::canonicalize(
        env::var_os(source_variable)
            .unwrap_or_else(|| panic!("{source_variable} must name a chainstate directory")),
    )
    .unwrap_or_else(|error| panic!("canonical {source_variable}: {error}"));
    let scratch = fs::canonicalize(env::var_os(scratch_variable).unwrap_or_else(|| {
        panic!("{scratch_variable} must name a fresh reflink chainstate directory")
    }))
    .unwrap_or_else(|error| panic!("canonical {scratch_variable}: {error}"));
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
    parent_height: u32,
    child_height: u32,
) -> (BlockHeader, BlockHeader) {
    assert_eq!(source.network(), Network::MAINNET);
    assert_eq!(scratch.network(), Network::MAINNET);
    assert_eq!(
        scratch.tip().expect("scratch tip"),
        source.tip().expect("source tip")
    );
    assert_eq!(block.header.chain_length, u64::from(child_height));
    assert!(
        !starts_new_tenure(block),
        "fixture must be a tenure extension"
    );

    let parent = *block.header.parent_block_id.as_bytes();
    let child = *block.block_id().as_bytes();
    assert_eq!(
        source.height_of(parent).expect("source parent height"),
        Some(parent_height)
    );
    assert_eq!(
        source.height_of(child).expect("source child height"),
        Some(child_height)
    );
    assert_eq!(
        scratch.height_of(parent).expect("scratch parent height"),
        Some(parent_height)
    );
    assert_eq!(
        scratch.height_of(child).expect("scratch child height"),
        Some(child_height)
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
    if let Some(index_block_hash) = &oracle.index_block_hash {
        assert_eq!(index_block_hash, &block.block_id().to_string());
    }
    if let Some(parent_index_block_hash) = &oracle.parent_index_block_hash {
        assert_eq!(
            parent_index_block_hash,
            &block.header.parent_block_id.to_string()
        );
    }
    // Hiro reports the tenure's sortition height. A Nakamoto extension may stand
    // on a later burn view, which is the Clarity-visible height recorded here.
    assert!(
        oracle.burn_block_height <= source_child.burn_block_height,
        "the block cannot execute before its tenure's sortition"
    );
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
        "ft_burn_event" => {
            let body = &event["ft_burn_event"];
            json!({
                "kind": "fungible_token",
                "operation": "burn",
                "asset": body["asset_identifier"],
                "sender": body["sender"],
                "amount": body["amount"],
            })
        }
        "nft_burn_event" => {
            let body = &event["nft_burn_event"];
            let value = body["raw_value"]
                .as_str()
                .expect("NFT burn raw value")
                .strip_prefix("0x")
                .expect("NFT burn value has a 0x prefix");
            json!({
                "kind": "non_fungible_token",
                "operation": "burn",
                "asset": body["asset_identifier"],
                "sender": body["sender"],
                "value_hex": value,
            })
        }
        "stx_transfer_event" => {
            let body = &event["stx_transfer_event"];
            json!({
                "kind": "stx",
                "operation": "transfer",
                "sender": body["sender"],
                "recipient": body["recipient"],
                "amount": body["amount"],
            })
        }
        "stx_lock_event" => {
            let body = &event["stx_lock_event"];
            json!({
                "kind": "stx",
                "operation": "lock",
                "locked_address": body["locked_address"],
                "locked_amount": body["locked_amount"],
                "unlock_height": body["unlock_height"],
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

fn observe(applied: &AppliedBlock, oracle: &Oracle, receipt_count: usize) -> Observation {
    assert_eq!(
        applied.receipts.len(),
        receipt_count,
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
    parent_height: u32,
) -> Observation {
    assert!(
        scratch
            .discard_above(parent_height)
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
    observe(&applied, oracle, block.transactions.len())
}

fn hex_fixture(block_file: &str, oracle_file: &str) -> (NakamotoBlock, Oracle) {
    let fixture = fixtures();
    let block = NakamotoBlock::decode(
        &hex::decode(
            fs::read_to_string(fixture.join(block_file))
                .expect("fixture block")
                .trim(),
        )
        .expect("fixture block is hexadecimal"),
    )
    .expect("decode fixture block");
    let oracle = serde_json::from_slice(
        &fs::read(fixture.join(oracle_file)).expect("canonical receipt oracle"),
    )
    .expect("decode canonical receipt oracle");
    (block, oracle)
}

fn assert_task_064_contract_epoch(source: &mut ChainState) {
    let contract = QualifiedContractIdentifier::parse(TASK_064_OLD_CONTRACT)
        .expect("task 064 contract identifier");
    let (contract_source, _) = source
        .vm_mut()
        .contract_source(&contract)
        .expect("task 064 contract source");
    assert!(
        contract_source.contains("(at-block"),
        "the captured contract still contains the word Epoch 3.4 removed"
    );
    let epoch = source
        .vm_mut()
        .recorded_deploy_epoch(&contract)
        .expect("task 064 recorded deploy epoch");
    assert!(
        epoch < StacksEpochId::Epoch34,
        "the captured contract's analysis must predate at-block removal: {epoch:?}"
    );
}

fn successful_call(outcome: ContractCallOutcome) -> Box<TransactionResult> {
    match outcome {
        ContractCallOutcome::Success(result) => result,
        ContractCallOutcome::AbortedByResponse(result) => {
            panic!("task 064 call aborted with {:?}", result.value)
        }
        ContractCallOutcome::RuntimeFailure { error, .. } => {
            panic!("task 064 call failed: {error:?}")
        }
    }
}

fn call_at_exact_fixture_prestate(
    scratch: &mut ChainState,
    block: &NakamotoBlock,
    context: BitcoinBlockContext,
    transaction_index: usize,
    parent_height: u32,
    interpreted: bool,
) -> TransactionResult {
    let parent = *block.header.parent_block_id.as_bytes();
    scratch
        .discard_above(parent_height)
        .expect("discard exact-prestate scratch");
    assert_eq!(
        scratch.tip().expect("exact-prestate parent tip"),
        Some(parent)
    );
    scratch
        .seed_unauthenticated_fixture_extension_from_parent_header(block.header.parent_block_id)
        .expect("seed exact-prestate fixture continuity");
    let prefix_cost = scratch
        .begin_unauthenticated_fixture_transaction_prestate(
            context,
            &[],
            Some(parent),
            block,
            transaction_index,
        )
        .expect("execute exact transaction prefix");
    let transaction = &block.transactions[transaction_index];
    let TransactionPayloadData::ContractCall {
        address,
        contract_name,
        function_name,
        arguments,
    } = transaction.payload().data()
    else {
        panic!("oracle transaction is a contract call")
    };
    let sender = PrincipalData::parse(
        &transaction
            .origin_address()
            .expect("oracle transaction origin")
            .to_string(),
    )
    .expect("oracle sender principal");
    let sponsor = transaction
        .sponsor_address()
        .map(|address| PrincipalData::parse(&address.to_string()).expect("sponsor principal"));
    let contract = QualifiedContractIdentifier::parse(&format!("{address}.{contract_name}"))
        .expect("oracle called contract");
    let arguments = arguments
        .iter()
        .map(|value| value.as_bytes().to_vec())
        .collect::<Vec<_>>();
    let tracker = scratch
        .vm_mut()
        .transaction_cost_tracker_with_total(prefix_cost.clone())
        .expect("exact-prestate cost tracker");
    let outcome = if interpreted {
        nano_oracle::interpret_contract_call(
            scratch.vm_mut(),
            nano_oracle::ContractCall {
                sender,
                sponsor,
                contract,
                function: function_name,
                arguments: &arguments,
            },
            tracker,
        )
        .expect("interpret oracle transaction")
    } else {
        scratch
            .vm_mut()
            .execute_contract_call_outcome(
                sender,
                sponsor,
                contract,
                function_name,
                &arguments,
                &tracker,
            )
            .expect("compile oracle transaction")
    };
    let mut result = *successful_call(outcome);
    result
        .cost
        .sub(&prefix_cost)
        .expect("target cost contains the prefix cost");
    scratch
        .vm_mut()
        .rollback_transaction()
        .expect("roll back target transaction");
    scratch
        .vm_mut()
        .abort_block()
        .expect("abort exact-prestate block");
    result
}

fn compare_engines_at_exact_fixture_prestate(
    scratch: &mut ChainState,
    block: &NakamotoBlock,
    context: BitcoinBlockContext,
    oracle: &Oracle,
    parent_height: u32,
) {
    let compiled = call_at_exact_fixture_prestate(
        scratch,
        block,
        context,
        oracle.tx_index,
        parent_height,
        false,
    );
    let interpreted = call_at_exact_fixture_prestate(
        scratch,
        block,
        context,
        oracle.tx_index,
        parent_height,
        true,
    );
    assert_eq!(compiled.value, interpreted.value);
    assert_eq!(compiled.cost, interpreted.cost);
    assert_eq!(compiled.events, interpreted.events);
    assert_eq!(compiled.assets, interpreted.assets);
    assert_eq!(compiled.cost, oracle_execution_cost(&oracle.cost));
}

const fn oracle_execution_cost(cost: &OracleCost) -> ExecutionCost {
    ExecutionCost {
        read_count: cost.read_count,
        read_length: cost.read_length,
        runtime: cost.runtime,
        write_count: cost.write_count,
        write_length: cost.write_length,
    }
}

fn compare_task_064_engines(
    scratch: &mut ChainState,
    block: &NakamotoBlock,
    context: BitcoinBlockContext,
) {
    let transaction = &block.transactions[0];
    let TransactionPayloadData::ContractCall {
        address,
        contract_name,
        function_name,
        arguments,
    } = transaction.payload().data()
    else {
        panic!("task 064 transaction is a contract call")
    };
    let sender = PrincipalData::parse(
        &transaction
            .origin_address()
            .expect("task 064 transaction origin")
            .to_string(),
    )
    .expect("task 064 sender principal");
    let sponsor = transaction
        .sponsor_address()
        .map(|address| PrincipalData::parse(&address.to_string()).expect("sponsor principal"));
    let contract = QualifiedContractIdentifier::parse(&format!("{address}.{contract_name}"))
        .expect("task 064 called contract");
    let arguments = arguments
        .iter()
        .map(|value| value.as_bytes().to_vec())
        .collect::<Vec<_>>();
    let parent = *block.header.parent_block_id.as_bytes();

    scratch
        .vm_mut()
        .begin_block_with_bitcoin_context(Some(parent), [0x64; 32], context)
        .expect("begin compiled comparison block");
    let tracker = scratch
        .vm_mut()
        .transaction_cost_tracker()
        .expect("compiled comparison cost tracker");
    let compiled = successful_call(
        scratch
            .vm_mut()
            .execute_contract_call_outcome(
                sender.clone(),
                sponsor.clone(),
                contract.clone(),
                function_name,
                &arguments,
                &tracker,
            )
            .expect("compiled task 064 call"),
    );
    scratch
        .vm_mut()
        .abort_block()
        .expect("abort compiled comparison block");

    scratch
        .vm_mut()
        .begin_block_with_bitcoin_context(Some(parent), [0x65; 32], context)
        .expect("begin interpreter comparison block");
    let tracker = scratch
        .vm_mut()
        .transaction_cost_tracker()
        .expect("interpreter comparison cost tracker");
    let interpreted = successful_call(
        nano_oracle::interpret_contract_call(
            scratch.vm_mut(),
            nano_oracle::ContractCall {
                sender,
                sponsor,
                contract,
                function: function_name,
                arguments: &arguments,
            },
            tracker,
        )
        .expect("interpreted task 064 call"),
    );
    scratch
        .vm_mut()
        .abort_block()
        .expect("abort interpreter comparison block");

    assert_eq!(compiled.value, interpreted.value);
    assert_eq!(compiled.cost, interpreted.cost);
    assert_eq!(compiled.events, interpreted.events);
    assert_eq!(compiled.assets, interpreted.assets);
}

#[test]
fn the_mainnet_8686666_old_epoch_fixture_is_self_consistent() {
    let (block, oracle) = hex_fixture(TASK_064_BLOCK_FILE, TASK_064_ORACLE_FILE);
    assert_eq!(block.header.chain_length, u64::from(TASK_064_CHILD_HEIGHT));
    assert_eq!(block.transactions.len(), 2);
    assert_eq!(oracle.tx_index, 0);
    assert_eq!(
        block.transactions[oracle.tx_index].txid().to_string(),
        oracle.txid
    );
    assert_eq!(oracle.events.len(), 4);
    assert_eq!(oracle.result.hex, "070100000000000000000000000000bd2585");
    assert_eq!(oracle.result.repr, "(ok u12395909)");
}

#[test]
#[ignore = "requires immutable mainnet source state and a fresh writable reflink scratch"]
fn the_mainnet_8686666_old_epoch_receipt_and_root_match_the_canonical_oracle() {
    let inputs = inputs("NANO_064_SOURCE", "NANO_064_SCRATCH");
    let (block, oracle) = hex_fixture(TASK_064_BLOCK_FILE, TASK_064_ORACLE_FILE);
    let mut source = ChainState::open_existing(&inputs.source).expect("open source read-only");
    let mut scratch = ChainState::open(source.network(), &inputs.scratch).expect("open scratch");
    let (parent, child) = validate_fixture(
        &mut source,
        &mut scratch,
        &block,
        &oracle,
        TASK_064_PARENT_HEIGHT,
        TASK_064_CHILD_HEIGHT,
    );
    assert_task_064_contract_epoch(&mut source);
    let context = context(parent, child);
    let first = replay_once(
        &mut scratch,
        &block,
        context,
        &oracle,
        TASK_064_PARENT_HEIGHT,
    );
    let second = replay_once(
        &mut scratch,
        &block,
        context,
        &oracle,
        TASK_064_PARENT_HEIGHT,
    );

    assert_eq!(first, second, "two scratch replays must be deterministic");
    assert_eq!(first.state_root, *block.header.state_index_root.as_bytes());
    assert_eq!(first.result_hex, oracle.result.hex);
    assert_eq!(first.result_repr, oracle.result.repr);
    assert_eq!(first.events, oracle.events);
    assert!(
        scratch
            .discard_above(TASK_064_PARENT_HEIGHT)
            .expect("discard task 064 comparison scratch")
            > 0
    );
    compare_task_064_engines(&mut scratch, &block, context);
    assert_eq!(first.cost, oracle.cost);
    source_is_unchanged(&inputs);
}

#[test]
#[ignore = "requires immutable mainnet source state and a fresh writable reflink scratch"]
fn the_mainnet_8708126_receipt_and_root_match_the_canonical_oracle() {
    let inputs = inputs("NANO_086_SOURCE", "NANO_086_SCRATCH");
    let fixture = fixtures();
    let block =
        NakamotoBlock::decode(&fs::read(fixture.join(TASK_086_BLOCK_FILE)).expect("fixture block"))
            .expect("decode fixture block");
    let oracle: Oracle = serde_json::from_slice(
        &fs::read(fixture.join(TASK_086_ORACLE_FILE)).expect("canonical receipt oracle"),
    )
    .expect("decode canonical receipt oracle");

    let mut source = ChainState::open_existing(&inputs.source).expect("open source read-only");
    let mut scratch = ChainState::open(source.network(), &inputs.scratch).expect("open scratch");
    let (parent, child) = validate_fixture(
        &mut source,
        &mut scratch,
        &block,
        &oracle,
        TASK_086_PARENT_HEIGHT,
        TASK_086_CHILD_HEIGHT,
    );
    let context = context(parent, child);
    let first = replay_once(
        &mut scratch,
        &block,
        context,
        &oracle,
        TASK_086_PARENT_HEIGHT,
    );
    let second = replay_once(
        &mut scratch,
        &block,
        context,
        &oracle,
        TASK_086_PARENT_HEIGHT,
    );

    assert_eq!(first, second, "two scratch replays must be deterministic");
    assert_eq!(first.state_root, *block.header.state_index_root.as_bytes());
    assert_eq!(first.result_hex, oracle.result.hex);
    assert_eq!(first.result_repr, oracle.result.repr);
    compare_engines_at_exact_fixture_prestate(
        &mut scratch,
        &block,
        context,
        &oracle,
        TASK_086_PARENT_HEIGHT,
    );
    assert_eq!(first.cost, oracle.cost);
    assert_eq!(first.events.len(), 8, "oracle event count changed");
    assert_eq!(first.events, oracle.events);
    source_is_unchanged(&inputs);
}

fn task_098_fixture() -> (NakamotoBlock, Oracle) {
    hex_fixture(TASK_098_BLOCK_FILE, TASK_098_ORACLE_FILE)
}

#[test]
fn the_mainnet_8724865_nested_trait_fixture_is_self_consistent() {
    let (block, oracle) = task_098_fixture();
    assert_eq!(block.header.chain_length, u64::from(TASK_098_CHILD_HEIGHT));
    assert_eq!(block.transactions.len(), 3);
    assert_eq!(oracle.tx_index, 2);
    assert_eq!(
        block.transactions[oracle.tx_index].txid().to_string(),
        oracle.txid
    );
    assert_eq!(oracle.events.len(), 21);
    assert_eq!(oracle.result.hex, "0703");
    assert_eq!(oracle.result.repr, "(ok true)");
}

#[test]
#[ignore = "requires immutable mainnet source state and a fresh writable reflink scratch"]
fn the_mainnet_8724865_nested_trait_receipt_and_root_match_the_canonical_oracle() {
    let inputs = inputs("NANO_098_SOURCE", "NANO_098_SCRATCH");
    let (block, oracle) = task_098_fixture();
    let mut source = ChainState::open_existing(&inputs.source).expect("open source read-only");
    let mut scratch = ChainState::open(source.network(), &inputs.scratch).expect("open scratch");
    let (parent, child) = validate_fixture(
        &mut source,
        &mut scratch,
        &block,
        &oracle,
        TASK_098_PARENT_HEIGHT,
        TASK_098_CHILD_HEIGHT,
    );
    let context = context(parent, child);
    let first = replay_once(
        &mut scratch,
        &block,
        context,
        &oracle,
        TASK_098_PARENT_HEIGHT,
    );
    let second = replay_once(
        &mut scratch,
        &block,
        context,
        &oracle,
        TASK_098_PARENT_HEIGHT,
    );

    assert_eq!(first, second, "two scratch replays must be deterministic");
    assert_eq!(first.state_root, *block.header.state_index_root.as_bytes());
    assert_eq!(first.result_hex, oracle.result.hex);
    assert_eq!(first.result_repr, oracle.result.repr);
    let expected_events = oracle
        .events
        .iter()
        .cloned()
        .map(|mut event| {
            if let Some(body) = event.as_object_mut() {
                body.retain(|_, value| value.as_str() != Some(""));
            }
            event
        })
        .collect::<Vec<_>>();
    assert_eq!(first.events, expected_events);
    compare_engines_at_exact_fixture_prestate(
        &mut scratch,
        &block,
        context,
        &oracle,
        TASK_098_PARENT_HEIGHT,
    );
    assert_eq!(first.cost, oracle.cost);
    assert_eq!(first.events.len(), 21, "oracle event count changed");
    source_is_unchanged(&inputs);
}

fn task_111_fixture() -> (NakamotoBlock, Oracle) {
    hex_fixture(TASK_111_BLOCK_FILE, TASK_111_ORACLE_FILE)
}

#[test]
fn the_mainnet_8733929_fastpool_fixture_is_self_consistent() {
    let (block, oracle) = task_111_fixture();
    assert_eq!(block.header.chain_length, u64::from(TASK_111_CHILD_HEIGHT));
    assert_eq!(block.transactions.len(), 2);
    assert_eq!(oracle.tx_index, 1);
    assert_eq!(
        block.transactions[oracle.tx_index].txid().to_string(),
        oracle.txid
    );
    assert_eq!(oracle.events.len(), 2);
    assert_eq!(
        oracle.result.hex,
        "070c000000070b616d6f756e742d7573747801000000000000000000000836928d74001266697273742d7265776172642d6379636c65010000000000000000000000000000008d0a6e756d2d6379636c65730100000000000000000000000000000060067369676e65720616296a283b358830510c486f90c13d924b92a6590e1e66617374706f6f6c2d6d61783530302d7369676e65722d6d616e61676572067374616b6572051685dc5a6a081acba8e773d6549953f6a2020ac17f12756e6c6f636b2d6275726e2d686569676874010000000000000000000000000011c1e60c756e6c6f636b2d6379636c6501000000000000000000000000000000ed"
    );
    assert_eq!(
        oracle.result.repr,
        "(ok (tuple (amount-ustx u9030480000000) (first-reward-cycle u141) (num-cycles u96) (signer 'SPMPMA1V6P430M8C91QS1G9XJ95S59JS1TZFZ4Q4.fastpool-max500-signer-manager) (staker 'SP22XRPKA10DCQA77EFB596AKYTH042P1FZH0BMQZ) (unlock-burn-height u1163750) (unlock-cycle u237)))"
    );
}

#[test]
#[ignore = "requires immutable mainnet source state and a fresh writable reflink scratch"]
fn the_mainnet_8733929_fastpool_receipt_and_root_match_the_canonical_oracle() {
    let inputs = inputs("NANO_111_SOURCE", "NANO_111_SCRATCH");
    let (block, oracle) = task_111_fixture();
    let mut source = ChainState::open_existing(&inputs.source).expect("open source read-only");
    let mut scratch = ChainState::open(source.network(), &inputs.scratch).expect("open scratch");
    let (parent, child) = validate_fixture(
        &mut source,
        &mut scratch,
        &block,
        &oracle,
        TASK_111_PARENT_HEIGHT,
        TASK_111_CHILD_HEIGHT,
    );
    let context = context(parent, child);
    let first = replay_once(
        &mut scratch,
        &block,
        context,
        &oracle,
        TASK_111_PARENT_HEIGHT,
    );
    let second = replay_once(
        &mut scratch,
        &block,
        context,
        &oracle,
        TASK_111_PARENT_HEIGHT,
    );

    assert_eq!(first, second, "two scratch replays must be deterministic");
    assert_eq!(first.state_root, *block.header.state_index_root.as_bytes());
    assert_eq!(first.result_hex, oracle.result.hex);
    assert_eq!(first.result_repr, oracle.result.repr);
    assert_eq!(first.events, oracle.events);
    compare_engines_at_exact_fixture_prestate(
        &mut scratch,
        &block,
        context,
        &oracle,
        TASK_111_PARENT_HEIGHT,
    );
    assert_eq!(first.cost, oracle.cost);
    source_is_unchanged(&inputs);
}
