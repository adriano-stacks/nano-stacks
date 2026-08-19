//! The deterministic Epoch 4.0 consensus decision boundary.
//!
//! One entry point judges one candidate block against one parent state and one
//! authenticated Bitcoin view, and answers with a canonical decision record:
//! verdict, sealed state root, execution cost, and the bounded receipt
//! commitment. The record is a pure function of its inputs and the state
//! directory — nothing here reads a clock, the environment, a socket or a file
//! outside the state directory — which is what lets a supervised executor
//! process host the decision and a shadow run compare two implementations
//! record for record.
//!
//! Refusals are typed: [`RefusalKind`] gives every way a block can be refused
//! a stable discriminant, so "refused, because" survives serialization and can
//! be compared across processes, revisions and implementations, where a
//! `Display` string cannot.

use nano_chainstate::{
    AppliedBlock, AuthenticatedBlock, BitcoinBlockContext, ChainState, ChainStateError,
    ConsensusError, NakamotoBlock, ReceiptCommitment, receipt_commitment,
};
use serde::{Deserialize, Serialize};

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

/// What one decision produced: the record, and the applied block when accepted.
///
/// The applied block stays out of the record because it is not portable — it
/// is what the in-process caller feeds to observers and archives. The record
/// alone crosses the process boundary.
#[derive(Debug)]
pub struct Decision {
    pub record: DecisionRecord,
    pub applied: Option<AppliedBlock>,
}

/// Judge one authenticated candidate and commit it when it holds.
///
/// The authentication itself happened on the same chainstate through
/// [`ChainState::authenticate_nakamoto_block_with_bitcoin_operations`]; this
/// is the second half, and the two together are the whole decision. A refusal
/// commits nothing — the chainstate's own seal discipline guarantees it — and
/// still produces a record, because a refusal *is* a decision.
pub fn decide(
    chainstate: &mut ChainState,
    authenticated: AuthenticatedBlock,
    waterfall_registry: Option<&str>,
) -> Decision {
    let block = authenticated.block().clone();
    let context = authenticated.bitcoin_context();
    match chainstate.commit_authenticated_nakamoto_block(authenticated, waterfall_registry) {
        Ok(committed) => {
            let applied = committed.into_applied();
            let record = accepted_record(&block, context, &applied);
            Decision {
                record,
                applied: Some(applied),
            }
        }
        Err(error) => Decision {
            record: refused_record(&block, context, &error),
            applied: None,
        },
    }
}

fn record_scaffold(block: &NakamotoBlock, context: BitcoinBlockContext) -> DecisionRecord {
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

fn accepted_record(
    block: &NakamotoBlock,
    context: BitcoinBlockContext,
    applied: &AppliedBlock,
) -> DecisionRecord {
    let mut record = record_scaffold(block, context);
    record.state_root = Some(hex::encode(applied.execution.state_root.0));
    record.cost = Some(CostSummary {
        runtime: applied.execution_cost.runtime,
        read_count: applied.execution_cost.read_count,
        read_length: applied.execution_cost.read_length,
        write_count: applied.execution_cost.write_count,
        write_length: applied.execution_cost.write_length,
    });
    match receipt_commitment(block, applied) {
        Ok(receipts) => record.receipts = Some(receipts),
        Err(detail) => {
            // A block whose receipts cannot be committed to is not a block this
            // record can vouch for, whatever the seal said.
            record.verdict = Verdict::Refused {
                kind: RefusalKind::Execution,
                detail,
            };
        }
    }
    record
}

fn refused_record(
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

#[cfg(test)]
mod tests {
    use super::{DECISION_SCHEMA, DecisionRecord, RefusalKind, Verdict, refusal_kind};
    use nano_chainstate::{ChainStateError, ConsensusError};

    fn record() -> DecisionRecord {
        DecisionRecord {
            schema: DECISION_SCHEMA.to_owned(),
            block_id: "aa".repeat(32),
            parent_block_id: "bb".repeat(32),
            height: 8_700_000,
            burn_view_height: 961_000,
            verdict: Verdict::Accepted,
            state_root: Some("cc".repeat(32)),
            cost: None,
            receipts: None,
            compiler_identity: "sha256:test".to_owned(),
            profile_fingerprint: "test".to_owned(),
        }
    }

    #[test]
    fn the_content_hash_is_deterministic_and_binds_every_field() {
        let one = record().content_hash().expect("hash");
        assert_eq!(one, record().content_hash().expect("hash"));
        let mut refused = record();
        refused.verdict = Verdict::Refused {
            kind: RefusalKind::SignerWeight,
            detail: "below threshold".to_owned(),
        };
        assert_ne!(one, refused.content_hash().expect("hash"));
        let mut moved = record();
        moved.height += 1;
        assert_ne!(one, moved.content_hash().expect("hash"));
    }

    /// The wire names are part of the record format: a rename is a new schema.
    #[test]
    fn refusal_kinds_serialize_to_pinned_names() {
        for (kind, name) in [
            (RefusalKind::Envelope, "\"envelope\""),
            (RefusalKind::TenureContinuity, "\"tenure-continuity\""),
            (RefusalKind::MinerAuthentication, "\"miner-authentication\""),
            (RefusalKind::SignerWeight, "\"signer-weight\""),
            (RefusalKind::SignerSet, "\"signer-set\""),
            (RefusalKind::VrfProof, "\"vrf-proof\""),
            (RefusalKind::Transaction, "\"transaction\""),
            (RefusalKind::Execution, "\"execution\""),
            (RefusalKind::StateRootMismatch, "\"state-root-mismatch\""),
            (RefusalKind::Storage, "\"storage\""),
            (RefusalKind::ParentMismatch, "\"parent-mismatch\""),
        ] {
            assert_eq!(serde_json::to_string(&kind).expect("serialize"), name);
        }
    }

    #[test]
    fn typed_consensus_refusals_map_to_their_stable_identity() {
        for (error, kind) in [
            (ConsensusError::EmptyBlock, RefusalKind::Envelope),
            (
                ConsensusError::TenureChangeParent,
                RefusalKind::TenureContinuity,
            ),
            (
                ConsensusError::WinnerVrfKeyUnavailable,
                RefusalKind::MinerAuthentication,
            ),
            (
                ConsensusError::SignerSetUnavailable(141),
                RefusalKind::SignerSet,
            ),
        ] {
            assert_eq!(refusal_kind(&ChainStateError::Consensus(error)), kind);
        }
        assert_eq!(
            refusal_kind(&ChainStateError::NoSignerSet(141)),
            RefusalKind::SignerSet
        );
    }
}
