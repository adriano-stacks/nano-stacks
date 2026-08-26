//! The canonical-record oracle for the `filter` cost differentials.
//!
//! Block 8,832,029 is the block that stopped mainnet following. Its first
//! transaction calls `unstake-lp-tokens` on
//! `SM1793C4R5PZ4NS4VQ4WMP7SKKYVH8JZEWSZ9HCCR.stableswap-staking-stx-ststx-v-1-4`,
//! which filters a `(list 12000 uint)` holding nothing — and the compiler
//! charged it `read_count` **303,863** against a 30,000 block limit where the
//! network charged **7**, so it aborted on the limit instead of on the
//! post-condition the chain recorded. Two defects were behind it, both fixed:
//!
//! * `filter` emitted a do-while with no zero-length guard, so an empty
//!   sequence read out of bounds and looped ([[149]]).
//! * a `filter` result was sized by what it kept rather than by the capacity it
//!   inherited, and a tuple built from a widened field lost that field's
//!   capacity ([[149]], [[150]]).
//!
//! The oracle here is the chain, not the interpreter: what
//! `https://api.hiro.so/extended/v1/tx/0x8979c764…` reports for the transaction
//! the network executed. Both the abort *kind* and all five cost dimensions are
//! pinned, because the failure changed the receipt as well as the cost — a
//! root-only check would have said nothing.
//!
//! Modelled on `mainnet_canonical_cost`, which does the same for task 146's
//! block. It is a separate module for the same reason that one is: the oracle
//! is legible on its own.

use std::{fs, path::PathBuf};

use nano_chainstate::{BitcoinBlockContext, ChainState, NakamotoBlock};
use nano_primitives::Network;
use nano_vm::BlockHeader;
use serde::Deserialize;

const PARENT_HEIGHT: u32 = 8_832_028;
const CHILD_HEIGHT: u32 = PARENT_HEIGHT + 1;
const BLOCK_FILE: &str = "block-8832029.hex";
const ORACLE_FILE: &str = "tx-8979-receipt.json";

#[derive(Deserialize)]
struct Cost {
    read_count: u64,
    read_length: u64,
    runtime: u64,
    write_count: u64,
    write_length: u64,
}

#[derive(Deserialize)]
struct Outcome {
    hex: String,
    repr: String,
}

#[derive(Deserialize)]
struct Oracle {
    txid: String,
    block_height: u32,
    index_block_hash: String,
    parent_index_block_hash: String,
    burn_block_height: u32,
    tx_index: usize,
    status: String,
    result: Outcome,
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

/// The fixture pair describes the transaction it claims to, and pins its numbers.
///
/// Runs everywhere, with no state. This is what stops the block bytes and the
/// canonical record from drifting apart, and it fixes the exact charge the chain
/// made so a later compiler change cannot quietly redefine what "matches
/// canonical" means for the transaction that stopped a mainnet follower.
#[test]
fn the_canonical_cost_fixture_describes_the_filter_transaction_of_block_8832029() {
    let (block, oracle) = fixture();
    assert_eq!(oracle.block_height, CHILD_HEIGHT);
    assert_eq!(block.header.chain_length, u64::from(CHILD_HEIGHT));
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
    assert_eq!(oracle.burn_block_height, 963_864);
    // The receipt, not only the cost: the compiler used to abort this on the
    // block limit, so the *kind* of failure is part of what regressed.
    assert_eq!(oracle.status, "abort_by_post_condition");
    assert_eq!(oracle.result.repr, "(ok u0)");
    assert_eq!(oracle.result.hex, "070100000000000000000000000000000000");
    assert!(oracle.events.is_empty());
    // What the chain charged. The compiler charged 303,863 read_count,
    // 5,491,601 read_length and 56,779,882 runtime before 149 and 150.
    assert_eq!(oracle.cost.read_count, 7);
    assert_eq!(oracle.cost.read_length, 22_193);
    assert_eq!(oracle.cost.runtime, 3_480_582);
    assert_eq!(oracle.cost.write_count, 1);
    assert_eq!(oracle.cost.write_length, 18);
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
/// `NANO_149_SOURCE` names an immutable state holding both the parent and the
/// child; `NANO_149_SCRATCH` names a fresh writable copy of it, which is the
/// only thing opened for writing. Unlike task 146's block this one carries four
/// transactions, so the comparison is against the receipt at the oracle's own
/// index rather than against a block total — which is the honest comparison
/// anyway, and the one a block with a transaction prefix forces.
#[test]
#[ignore = "requires an immutable mainnet source state and a fresh writable reflink scratch"]
fn the_block_8832029_receipt_matches_the_canonical_record() {
    let source_path = std::env::var("NANO_149_SOURCE").expect("NANO_149_SOURCE names a state");
    let scratch_path = std::env::var("NANO_149_SCRATCH").expect("NANO_149_SCRATCH names a scratch");
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

    let receipt = applied
        .receipts
        .get(oracle.tx_index)
        .expect("the block holds the oracle's transaction");
    let cost = &receipt.result.cost;
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
