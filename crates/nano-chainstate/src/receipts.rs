//! The bounded commitment over everything consensus-visible a block's
//! execution produced.
//!
//! One digest per block covering every transaction's identity, status,
//! serialized result and all five cost dimensions, and every ordered event.
//! It is how two independently executing implementations are compared without
//! shipping receipts around: the follower archives it beside each executed
//! block, the 24-hour hold compares it with an independent witness's
//! `new_block` payload digest, and the consensus firewall's decision record
//! carries it as the receipts half of a verdict. The payload-side twin lives
//! in `nano-conformance::receipt_digest`; the packaged follower gate asserts
//! the two reduce captured stacks-core payloads and applied blocks to the same
//! value.

use serde::{Deserialize, Serialize};

use crate::nakamoto::NakamotoBlock;
use crate::{AppliedBlock, TransactionReceipt, TransactionStatus};

/// The consensus-visible receipt fields produced while one block ran.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReceiptCommitment {
    pub height: u64,
    pub block: String,
    pub transactions: usize,
    pub events: usize,
    pub digest: String,
}

/// Reduce an applied block's receipts to their bounded commitment.
pub fn receipt_commitment(
    block: &NakamotoBlock,
    applied: &AppliedBlock,
) -> Result<ReceiptCommitment, String> {
    receipt_commitment_parts(block, &applied.receipts, &applied.observer_transactions)
}

/// The same commitment from its parts, for the seal, where no
/// [`AppliedBlock`] has been assembled yet.
pub fn receipt_commitment_parts(
    block: &NakamotoBlock,
    block_receipts: &[TransactionReceipt],
    observer_transactions: &[crate::ObservedTransaction],
) -> Result<ReceiptCommitment, String> {
    let receipts = block_receipts
        .iter()
        .chain(
            observer_transactions
                .iter()
                .map(|observed| &observed.receipt),
        )
        .collect::<Vec<_>>();
    let mut preimage = Vec::new();
    for receipt in &receipts {
        preimage.extend_from_slice(receipt.txid.to_string().as_bytes());
        preimage.extend_from_slice(receipt_status(receipt).as_bytes());
        if let Some(value) = receipt.result.value.as_ref() {
            preimage.extend_from_slice(
                value
                    .serialize_to_hex()
                    .map_err(|error| error.to_string())?
                    .as_bytes(),
            );
        }
        let cost = &receipt.result.cost;
        for dimension in [
            cost.runtime,
            cost.read_count,
            cost.read_length,
            cost.write_count,
            cost.write_length,
        ] {
            preimage.extend_from_slice(dimension.to_string().as_bytes());
        }
    }
    let events = receipts
        .iter()
        .flat_map(|receipt| {
            receipt
                .result
                .events
                .iter()
                .map(move |event| (event, receipt.txid, receipt.committed))
        })
        .enumerate()
        .map(|(index, (event, txid, committed))| {
            event
                .json_serialize(index, &txid, committed)
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    for event in &events {
        preimage.extend_from_slice(
            json_string(event, "txid")?
                .trim_start_matches("0x")
                .as_bytes(),
        );
        preimage.extend_from_slice(
            json_string(event, "type")?
                .trim_start_matches("0x")
                .as_bytes(),
        );
        preimage.extend_from_slice(event["committed"].to_string().as_bytes());
        preimage.extend_from_slice(&serde_json::to_vec(event).map_err(|error| error.to_string())?);
    }
    Ok(ReceiptCommitment {
        height: block.header.chain_length,
        block: block.header.block_hash().to_string(),
        transactions: receipts.len(),
        events: events.len(),
        digest: hex::encode(nano_primitives::sha512_256(&preimage).as_bytes()),
    })
}

const fn receipt_status(receipt: &TransactionReceipt) -> &'static str {
    match &receipt.status {
        TransactionStatus::Success => "success",
        TransactionStatus::PostConditionAborted(_) => "abort_by_post_condition",
        TransactionStatus::AbortedByResponse | TransactionStatus::RuntimeFailure(_) => {
            "abort_by_response"
        }
    }
}

fn json_string<'a>(value: &'a serde_json::Value, field: &str) -> Result<&'a str, String> {
    value[field]
        .as_str()
        .ok_or_else(|| format!("event has no string {field}"))
}
