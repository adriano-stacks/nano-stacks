//! Print what a raw Nakamoto block commits to, for the hold's comparisons.
//!
//! Two valid representations of one block may differ in bytes: the signer
//! signature vector is outside the block hash, so a node that accepted the
//! block at fourteen signatures and a node that holds it at fifteen both
//! serve the same block. Byte equality is therefore the wrong comparison —
//! the first 24-hour hold attempt failed on exactly that — and this prints
//! the parts that are consensus: the block id, the block hash (which is the
//! signer signature hash and covers every header field and the transactions),
//! the sealed state root, and the signature count for the record.

use std::process::ExitCode;

use nano_chainstate::NakamotoBlock;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let [path] = arguments.as_slice() else {
        eprintln!("usage: block-identity <raw-nakamoto-block>");
        return ExitCode::FAILURE;
    };
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("{path}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let block = match NakamotoBlock::decode(&bytes) {
        Ok(block) => block,
        Err(error) => {
            eprintln!("the block does not decode: {error:?}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "{}",
        serde_json::json!({
            "block_id": block.block_id().to_string(),
            "block_hash": block.header.block_hash().to_string(),
            "height": block.header.chain_length,
            "consensus_hash": block.header.consensus_hash.to_string(),
            "state_index_root": block.header.state_index_root.to_string(),
            "transaction_merkle_root": block.header.transaction_merkle_root.to_string(),
            "transactions": block.transactions.len(),
            "signer_signatures": block.header.signer_signatures.len(),
        })
    );
    ExitCode::SUCCESS
}
