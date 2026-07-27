#![forbid(unsafe_code)]

use clarity::vm::ast::build_ast;
use clarity::vm::contexts::{ContractContext, GlobalContext};
use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::database::MemoryBackingStore;
use clarity::vm::errors::ClarityEvalError;
use clarity::vm::types::QualifiedContractIdentifier;
use clarity::vm::{ClarityVersion, Value, eval_all};
use nano_marf::StateRoot;
use stacks_common::consts::CHAIN_ID_TESTNET;
use stacks_common::types::StacksEpochId;

/// M0 execution output. M8/M10 replace the marker with Clarity receipts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionResult {
    pub state_root: StateRoot,
}

#[must_use]
pub const fn execute_stub() -> ExecutionResult {
    ExecutionResult {
        state_root: StateRoot::empty(),
    }
}

/// Evaluate a Clarity 6 program under the consensus Epoch 4.0 rules.
///
/// The current backing store is deliberately ephemeral while the MARF adapter is
/// implemented. The interpreter, language version, epoch, and cost tracker are
/// nevertheless the production consensus implementation.
pub fn evaluate(source: &str) -> Result<Option<Value>, ClarityEvalError> {
    let contract_id = QualifiedContractIdentifier::transient();
    let mut backing_store = MemoryBackingStore::new();
    let database = backing_store.as_clarity_db();
    let mut context = GlobalContext::new(
        false,
        CHAIN_ID_TESTNET,
        database,
        LimitedCostTracker::new_free(),
        StacksEpochId::Epoch40,
    );
    let expressions = build_ast(
        &contract_id,
        source,
        &mut context.cost_track,
        ClarityVersion::Clarity6,
        StacksEpochId::Epoch40,
    )?
    .expressions;
    let mut contract = ContractContext::new(contract_id, ClarityVersion::Clarity6);

    context
        .execute(|global| eval_all(&expressions, &mut contract, global, None))
        .map_err(ClarityEvalError::from)
}

#[cfg(test)]
mod tests {
    use clarity::vm::Value;

    use super::evaluate;

    #[test]
    fn evaluates_clarity_six_programs() {
        let value = evaluate("(+ u20 u22)").expect("Clarity 6 program should evaluate");

        assert_eq!(value, Some(Value::UInt(42)));
    }

    #[test]
    fn rejects_invalid_programs() {
        assert!(evaluate("(unknown-word u1)").is_err());
    }
}
