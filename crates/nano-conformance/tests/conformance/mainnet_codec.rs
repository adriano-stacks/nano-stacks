//! Decode real mainnet blocks against stacks-core's own codec.
//!
//! A block nano cannot decode stops a mainnet descent dead, and finding out
//! which byte by restarting a node against a live peer costs minutes an
//! attempt. This reads a captured `/v3/tenures/:id` response off disk and, when
//! nano refuses a transaction, decodes the same bytes with `stacks-codec` and
//! prints what it made of them — so the answer is a diff rather than a guess.
//!
//! Point `NANO_MAINNET_BLOCKS` at the file to run it:
//!
//! ```text
//! curl -s https://api.mainnet.hiro.so/v3/tenures/<id> -o /tmp/tenure.bin
//! NANO_MAINNET_BLOCKS=/tmp/tenure.bin cargo test -p nano-conformance --test mainnet_codec
//! ```

use std::{env, fs};

use nano_chainstate::NakamotoBlock;

use blockstack_lib::chainstate::nakamoto::{NakamotoBlock as CoreBlock, TxToProcess};
use stacks_common::codec::StacksMessageCodec;

/// The transactions of a block, however stacks-core wraps them.
fn core_transactions(
    block: &CoreBlock,
) -> Vec<&blockstack_lib::chainstate::stacks::StacksTransaction> {
    block
        .txs()
        .map(|transaction| match transaction {
            TxToProcess::Execute(transaction)
            | TxToProcess::Skip {
                tx: transaction, ..
            } => transaction,
        })
        .collect()
}

/// Split a concatenated block stream using stacks-core, which is the oracle for
/// where each block ends when nano cannot get that far itself.
fn core_blocks(bytes: &[u8]) -> Vec<CoreBlock> {
    let mut cursor = std::io::Cursor::new(bytes);
    let mut blocks = Vec::new();
    while usize::try_from(cursor.position()).unwrap_or(usize::MAX) < bytes.len() {
        match CoreBlock::consensus_deserialize(&mut cursor) {
            Ok(block) => blocks.push(block),
            Err(error) => panic!("stacks-core cannot decode the capture either: {error}"),
        }
    }
    blocks
}

#[test]
fn nano_decodes_every_mainnet_block_stacks_core_does() {
    let Ok(path) = env::var("NANO_MAINNET_BLOCKS") else {
        nano_conformance::skip_gate("NANO_MAINNET_BLOCKS must name a captured block stream");
        return;
    };
    let bytes = fs::read(&path).expect("read the captured blocks");
    let expected = core_blocks(&bytes);
    assert!(!expected.is_empty(), "the capture holds blocks");

    let mut offset = 0;
    for (index, block) in expected.iter().enumerate() {
        let (decoded, consumed) = match NakamotoBlock::decode_prefix(&bytes[offset..]) {
            Ok(decoded) => decoded,
            Err(error) => {
                // stacks-core got this far, so the disagreement is nano's and
                // the block it disagrees about is the one to describe.
                for (position, transaction) in core_transactions(block).into_iter().enumerate() {
                    let encoded = transaction.serialize_to_vec();
                    assert!(
                        nano_codec::Transaction::decode(&encoded).is_ok(),
                        "block {index} height {} transaction {position} is not decodable: \
                         {error}\n  post conditions: {:?}\n  payload: {:?}\n  bytes: {}",
                        block.header.chain_length,
                        transaction.post_conditions,
                        transaction.payload,
                        hex::encode(&encoded),
                    );
                }
                panic!(
                    "block {index} height {} does not decode though every transaction does: \
                     {error}",
                    block.header.chain_length
                );
            }
        };
        assert_eq!(
            decoded.header.chain_length, block.header.chain_length,
            "block {index} is the same block"
        );
        assert_eq!(
            decoded.transactions.len(),
            core_transactions(block).len(),
            "block {index} at height {} has the same transactions",
            block.header.chain_length
        );
        offset += consumed;
    }
    assert_eq!(offset, bytes.len(), "the whole capture was consumed");
}
