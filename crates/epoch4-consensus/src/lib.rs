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

pub mod host;
mod request;

pub use request::{ContextWire, DecisionRequest, OpenedRequest, REQUEST_SCHEMA};

use nano_chainstate::{
    AppliedBlock, AuthenticatedBlock, ChainState, NakamotoBlock,
    decision::{record_scaffold, refused_record},
};
pub use nano_chainstate::{
    CostSummary, DECISION_SCHEMA, DecisionRecord, RefusalKind, Verdict, refusal_kind,
};

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
            let (record, applied) = committed.into_record_and_applied();
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

/// Judge one wire request end to end: linkage, authentication, execution.
///
/// The child process's whole loop is this call. The tip is the child's own —
/// adopted at start and advanced only by its own accepted decisions — so a
/// parent process cannot make it stand somewhere it never executed.
pub fn judge(
    chainstate: &mut ChainState,
    request: &OpenedRequest,
    tip: &NakamotoBlock,
    waterfall_registry: Option<&str>,
) -> Decision {
    if let Err(error) = request.block.validate_successor(&tip.header) {
        let mut record = record_scaffold(&request.block, request.context);
        record.verdict = Verdict::Refused {
            kind: RefusalKind::ParentMismatch,
            detail: format!("{error:?}: the candidate does not extend this executor's tip"),
        };
        return Decision {
            record,
            applied: None,
        };
    }
    let parent = request.parent.unwrap_or_else(|| *tip.block_id().as_bytes());
    match chainstate.authenticate_nakamoto_block_with_bitcoin_operations(
        request.context,
        &request.operations,
        Some(parent),
        request.block.clone(),
    ) {
        Ok(authenticated) => decide(chainstate, authenticated, waterfall_registry),
        Err(error) => Decision {
            record: refused_record(&request.block, request.context, &error),
            applied: None,
        },
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
