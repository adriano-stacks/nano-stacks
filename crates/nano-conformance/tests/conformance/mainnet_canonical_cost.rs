//! The canonical-record oracle for task 146's cost differential.
//!
//! Block 8,808,752 is the block that exposed it, and it is uniquely useful:
//! it carries exactly one transaction, so its block cost *is* that
//! transaction's cost and an exact comparison with the chain's own record is
//! possible without reconstructing a transaction prefix.
//!
//! That matters because the interpreter cannot arbitrate this differential. On
//! at least one mainnet shape nano's sealed runtime sits *below* the canonical
//! record while the compiler sits above the interpreter, so a
//! compiler-versus-interpreter comparison there measures agreement with the
//! wrong thing. Here the oracle is the chain: the frozen record is what
//! `https://api.hiro.so/extended/v1/tx/0xfc71c88f…` reports for the
//! transaction the network executed.
//!
//! This module is deliberately standalone rather than another case inside
//! `mainnet_divergence`: the replay is a dozen lines of public API, and keeping
//! it separate keeps the canonical-cost oracle legible as the thing task 146
//! closes against.

use std::{fs, path::PathBuf};

use nano_chainstate::{BitcoinBlockContext, ChainState, NakamotoBlock};
use nano_primitives::Network;
use nano_vm::BlockHeader;
use serde::Deserialize;

const PARENT_HEIGHT: u32 = 8_808_751;
const CHILD_HEIGHT: u32 = PARENT_HEIGHT + 1;
const BLOCK_FILE: &str = "block-8808752.hex";
const ORACLE_FILE: &str = "tx-fc71-receipt.json";

#[derive(Deserialize)]
struct Cost {
    read_count: u64,
    read_length: u64,
    runtime: u64,
    write_count: u64,
    write_length: u64,
}

#[derive(Deserialize)]
struct Result_ {
    hex: String,
    repr: String,
}

#[derive(Deserialize)]
struct Oracle {
    txid: String,
    block_height: u32,
    index_block_hash: String,
    parent_index_block_hash: String,
    tx_index: usize,
    status: String,
    result: Result_,
    cost: Cost,
    events: Vec<serde_json::Value>,
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/mainnet/divergence")
}

fn fixture() -> (NakamotoBlock, Oracle) {
    let directory = fixtures();
    let hex = fs::read_to_string(directory.join(BLOCK_FILE)).expect("read block fixture");
    let bytes = hex::decode(hex.trim()).expect("block fixture is hexadecimal");
    let block = NakamotoBlock::decode(&bytes).expect("decode block fixture");
    let oracle: Oracle = serde_json::from_str(
        &fs::read_to_string(directory.join(ORACLE_FILE)).expect("read oracle fixture"),
    )
    .expect("parse oracle fixture");
    (block, oracle)
}

/// The fixture pair describes one transaction of one block, and says so.
///
/// Runs everywhere, with no state: it is what stops the block bytes and the
/// canonical record from drifting apart, and it pins the exact numbers the
/// chain charged so a future change to the compiler cannot quietly redefine
/// what "matches canonical" means.
#[test]
fn the_canonical_cost_fixture_describes_one_transaction_of_block_8808752() {
    let (block, oracle) = fixture();
    assert_eq!(oracle.block_height, CHILD_HEIGHT);
    assert_eq!(block.header.chain_length, u64::from(CHILD_HEIGHT));
    assert_eq!(
        block.transactions.len(),
        1,
        "the block's single transaction is what makes an exact canonical \
         comparison possible without a transaction prefix"
    );
    assert_eq!(oracle.tx_index, 0);
    assert_eq!(
        block.transactions[oracle.tx_index].txid().to_string(),
        oracle.txid
    );
    assert_eq!(
        hex::encode(block.block_id().as_bytes()),
        oracle.index_block_hash
    );
    assert_eq!(
        hex::encode(block.header.parent_block_id.as_bytes()),
        oracle.parent_index_block_hash
    );
    assert_eq!(oracle.status, "success");
    assert_eq!(oracle.result.hex, "0703");
    assert_eq!(oracle.result.repr, "(ok true)");
    assert_eq!(oracle.events.len(), 2);
    // The differential this task opened on, as the chain recorded it: nano
    // charged 77,622 / 165,496 / 901 for the three dimensions that were wrong.
    assert_eq!(oracle.cost.read_count, 30);
    assert_eq!(oracle.cost.read_length, 76_653);
    assert_eq!(oracle.cost.runtime, 161_448);
    assert_eq!(oracle.cost.write_count, 4);
    assert_eq!(oracle.cost.write_length, 341);
}

fn complete_header(chainstate: &ChainState, block: [u8; 32]) -> BlockHeader {
    chainstate
        .recorded_header(block)
        .expect("block has a complete recorded header")
}

fn context(parent: &BlockHeader, child: &BlockHeader) -> BitcoinBlockContext {
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

/// Execute the block at its exact prestate and match the chain's own record.
///
/// `NANO_146_SOURCE` names an immutable state holding both the parent and the
/// child; `NANO_146_SCRATCH` names a fresh writable copy of it, which is the
/// only thing opened for writing. The append runs under the usual root policy,
/// so a different VM result cannot seal; what this adds is that the *cost* is
/// the chain's, dimension by dimension, rather than the other engine's.
#[test]
#[ignore = "requires an immutable mainnet source state and a fresh writable reflink scratch"]
fn the_block_8808752_receipt_matches_the_canonical_record() {
    let source_path = std::env::var("NANO_146_SOURCE").expect("NANO_146_SOURCE names a state");
    let scratch_path = std::env::var("NANO_146_SCRATCH").expect("NANO_146_SCRATCH names a scratch");
    assert_ne!(
        fs::canonicalize(&source_path).expect("canonical source"),
        fs::canonicalize(&scratch_path).expect("canonical scratch"),
        "the scratch must not be the source"
    );
    let (block, oracle) = fixture();

    let source = ChainState::open_existing(&source_path).expect("open source read-only");
    assert_eq!(source.network(), Network::MAINNET);
    let parent = *block.header.parent_block_id.as_bytes();
    let parent_header = complete_header(&source, parent);
    let child_header = complete_header(&source, *block.block_id().as_bytes());
    drop(source);

    let mut scratch = ChainState::open(Network::MAINNET, &scratch_path).expect("open scratch");
    assert!(
        scratch
            .discard_above(PARENT_HEIGHT)
            .expect("discard the scratch above the parent")
            > 0,
        "the scratch must hold the child before the replay"
    );
    assert_eq!(scratch.tip().expect("scratch tip"), Some(parent));
    scratch
        .seed_unauthenticated_fixture_extension_from_parent_header(block.header.parent_block_id)
        .expect("seed fixture continuity from the parent header");
    let applied = scratch
        .append_unauthenticated_fixture_block_with_bitcoin_operations(
            context(&parent_header, &child_header),
            &[],
            Some(parent),
            &block,
        )
        .expect("execute the block and verify its committed root");

    assert_eq!(applied.receipts.len(), 1, "the block has one transaction");
    let cost = &applied.receipts[oracle.tx_index].result.cost;
    assert_eq!(
        (
            cost.read_count,
            cost.read_length,
            cost.runtime,
            cost.write_count,
            cost.write_length
        ),
        (
            oracle.cost.read_count,
            oracle.cost.read_length,
            oracle.cost.runtime,
            oracle.cost.write_count,
            oracle.cost.write_length
        ),
        "every cost dimension must equal the canonical record"
    );
}
