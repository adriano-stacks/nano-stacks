//! The canonical decision record a block commits under.
//!
//! Built where the decision is made — at the seal, from the verified header
//! root, the executed receipts and the block's own identity — and durable in
//! the same transaction as the block's metadata and ledger. Task 141 makes it
//! the committed-block visibility point; task 140's process boundary carries
//! it as the answer to a decision request.

use serde::{Deserialize, Serialize};

use crate::nakamoto::NakamotoBlock;
use crate::receipts::{ReceiptCommitment, receipt_commitment};
use crate::{AppliedBlock, BitcoinBlockContext, ChainStateError, ConsensusError};

/// The stable identity of a refusal, comparable across processes and revisions.
///
/// Coarser than the refusing error on purpose: two implementations must agree
/// on *why* a block is refused without agreeing on prose, and a new sentence
/// for an old reason must not read as a divergence. The discriminants are part
/// of the record format and never reused.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RefusalKind {
    /// The block's own envelope: version, structure, transaction shape.
    Envelope,
    /// Tenure continuity against the executed chain.
    TenureContinuity,
    /// The miner's signature, or a miner that did not win the sortition.
    MinerAuthentication,
    /// The signer set's weight threshold was not met.
    SignerWeight,
    /// The cycle's signer set is absent or unusable.
    SignerSet,
    /// A VRF proof or committed seed the tenure start cannot justify.
    VrfProof,
    /// A transaction the chain does not admit.
    Transaction,
    /// Execution failed inside the VM or its compiler.
    Execution,
    /// The sealed root differs from the header's commitment.
    StateRootMismatch,
    /// The state directory itself failed.
    Storage,
    /// The request named a parent that is not the execution tip.
    ParentMismatch,
}

/// The canonical outcome of judging one candidate block.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DecisionRecord {
    pub schema: String,
    pub block_id: String,
    pub parent_block_id: String,
    pub height: u64,
    pub burn_view_height: u64,
    pub verdict: Verdict,
    pub state_root: Option<String>,
    pub cost: Option<CostSummary>,
    pub receipts: Option<ReceiptCommitment>,
    pub compiler_identity: String,
    pub profile_fingerprint: String,
}

/// Accepted, or refused with a stable identity and the refusing sentence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum Verdict {
    Accepted,
    Refused { kind: RefusalKind, detail: String },
}

/// The five cost dimensions a block's execution consumed.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CostSummary {
    pub runtime: u64,
    pub read_count: u64,
    pub read_length: u64,
    pub write_count: u64,
    pub write_length: u64,
}

pub const DECISION_SCHEMA: &str = "nano-stacks/epoch4-decision/v1";

impl DecisionRecord {
    /// The content address of this record: its canonical JSON, hashed.
    ///
    /// Field order is the struct's, which serde keeps; the schema string pins
    /// the format so a reader can never mistake one version for another.
    pub fn content_hash(&self) -> Result<[u8; 32], String> {
        let bytes = serde_json::to_vec(self).map_err(|error| error.to_string())?;
        Ok(*nano_primitives::sha512_256(&bytes).as_bytes())
    }
}

#[must_use]
pub fn record_scaffold(block: &NakamotoBlock, context: BitcoinBlockContext) -> DecisionRecord {
    DecisionRecord {
        schema: DECISION_SCHEMA.to_owned(),
        block_id: block.block_id().to_string(),
        parent_block_id: block.header.parent_block_id.to_string(),
        height: block.header.chain_length,
        burn_view_height: context.height,
        verdict: Verdict::Accepted,
        state_root: None,
        cost: None,
        receipts: None,
        compiler_identity: nano_vm::COMPILER_IDENTITY.to_owned(),
        profile_fingerprint: nano_vm::compatibility_profile_fingerprint().to_string(),
    }
}

/// The accepted record from its parts, which is how the seal builds it.
///
/// The root is the header's own, already verified against the pending trie,
/// and the commitment covers exactly the receipts the block produced.
#[must_use]
pub fn accepted_record_parts(
    block: &NakamotoBlock,
    context: BitcoinBlockContext,
    cost: &clarity::vm::costs::ExecutionCost,
    receipts: ReceiptCommitment,
) -> DecisionRecord {
    let mut record = record_scaffold(block, context);
    record.state_root = Some(block.header.state_index_root.to_string());
    record.cost = Some(CostSummary {
        runtime: cost.runtime,
        read_count: cost.read_count,
        read_length: cost.read_length,
        write_count: cost.write_count,
        write_length: cost.write_length,
    });
    record.receipts = Some(receipts);
    record
}

#[must_use]
pub fn accepted_record(
    block: &NakamotoBlock,
    context: BitcoinBlockContext,
    applied: &AppliedBlock,
) -> DecisionRecord {
    match receipt_commitment(block, applied) {
        Ok(receipts) => accepted_record_parts(block, context, &applied.execution_cost, receipts),
        Err(detail) => {
            // A block whose receipts cannot be committed to is not a block this
            // record can vouch for, whatever the seal said.
            let mut record = record_scaffold(block, context);
            record.verdict = Verdict::Refused {
                kind: RefusalKind::Execution,
                detail,
            };
            record
        }
    }
}

/// Serialize a record for the seal, content-addressed.
pub fn sealed(record: &DecisionRecord) -> Result<nano_vm::SealedDecision, String> {
    Ok(nano_vm::SealedDecision {
        content_hash: record.content_hash()?,
        record: serde_json::to_vec(record).map_err(|error| error.to_string())?,
    })
}

#[must_use]
pub fn refused_record(
    block: &NakamotoBlock,
    context: BitcoinBlockContext,
    error: &ChainStateError,
) -> DecisionRecord {
    let mut record = record_scaffold(block, context);
    record.verdict = Verdict::Refused {
        kind: refusal_kind(error),
        detail: error.to_string(),
    };
    record
}

/// The stable identity of a chainstate refusal.
#[must_use]
pub const fn refusal_kind(error: &ChainStateError) -> RefusalKind {
    match error {
        ChainStateError::Consensus(consensus) => consensus_kind(*consensus),
        ChainStateError::Storage(_) | ChainStateError::Ledger(_) => RefusalKind::Storage,
        ChainStateError::StateRootMismatch { .. } => RefusalKind::StateRootMismatch,
        ChainStateError::NoSignerSet(_) => RefusalKind::SignerSet,
        ChainStateError::Evaluation(_)
        | ChainStateError::Execution(_)
        | ChainStateError::TransactionExecution { .. }
        | ChainStateError::TransactionFailure { .. } => RefusalKind::Execution,
        ChainStateError::FixtureExtensionParentTip { .. }
        | ChainStateError::FixtureExtensionParentHeader(_) => RefusalKind::ParentMismatch,
        ChainStateError::InvalidTransaction(_) | ChainStateError::UnsupportedPayload => {
            RefusalKind::Transaction
        }
    }
}

const fn consensus_kind(error: ConsensusError) -> RefusalKind {
    use ConsensusError as E;
    match error {
        E::HeaderVersion(_)
        | E::EmptyBlock
        | E::TransactionNetwork { .. }
        | E::TransactionChainId { .. }
        | E::TransactionAnchorMode { .. }
        | E::CoinbaseWithoutVrfProof { .. }
        | E::TenureTransactionCount { .. }
        | E::CoinbaseWithoutTenureChange
        | E::TenureTransactionPosition { .. }
        | E::ProblematicMarkerCount(_)
        | E::ProblematicMarkerOrder(_)
        | E::ProblematicMarkerOutOfBounds(_)
        | E::ProblematicMarkerTarget(_)
        | E::BitcoinSpent { .. } => RefusalKind::Envelope,
        E::TenureChangeCause(_)
        | E::TenureChangeParent
        | E::TenureChangeConsensusHash
        | E::TenureChangePreviousTenure
        | E::TenureChangeParentTenure
        | E::TenureChangeParentUnavailable
        | E::TenureChangeLengthUnavailable
        | E::TenureChangeBlockCount { .. } => RefusalKind::TenureContinuity,
        E::MinerSignatureUnrecoverable
        | E::TenureChangeMinerKey
        | E::MinerIsNotTheSortitionWinner { .. }
        | E::WinnerVrfKeyUnavailable
        | E::WinnerSigningKeyUnavailable => RefusalKind::MinerAuthentication,
        E::SignerWeight(_) => RefusalKind::SignerWeight,
        E::SignerSetUnavailable(_) => RefusalKind::SignerSet,
    }
}
