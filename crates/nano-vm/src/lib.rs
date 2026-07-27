#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use clarity::vm::ast::build_ast;
use clarity::vm::contexts::{ContractContext, GlobalContext};
use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::database::MemoryBackingStore;
use clarity::vm::errors::ClarityEvalError;
use clarity::vm::types::QualifiedContractIdentifier;
use clarity::vm::{ClarityVersion, Value, eval_all};
use nano_marf::{MarfError, MarfValue, StateRoot, VersionedMarf};
use nano_primitives::TrieHash;
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

/// A versioned Clarity key/value store whose state roots are committed by the MARF.
#[derive(Clone, Debug, Default)]
pub struct MarfStore {
    marf: VersionedMarf,
    states: BTreeMap<[u8; 32], BTreeMap<String, String>>,
    active: Option<ActiveStore>,
}

#[derive(Clone, Debug)]
struct ActiveStore {
    block: [u8; 32],
    values: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MarfStoreError {
    Marf(MarfError),
    NoActiveState,
}

impl std::fmt::Display for MarfStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Marf(error) => write!(formatter, "MARF error: {error}"),
            Self::NoActiveState => formatter.write_str("no active VM state"),
        }
    }
}

impl std::error::Error for MarfStoreError {}

impl From<MarfError> for MarfStoreError {
    fn from(error: MarfError) -> Self {
        Self::Marf(error)
    }
}

impl MarfStore {
    /// Begin a new state, inheriting all values from `parent` when present.
    pub fn begin(
        &mut self,
        parent: Option<[u8; 32]>,
        block: [u8; 32],
    ) -> Result<(), MarfStoreError> {
        self.marf.begin(parent, block)?;
        let values = parent
            .and_then(|parent| self.states.get(&parent).cloned())
            .unwrap_or_default();
        self.active = Some(ActiveStore { block, values });
        Ok(())
    }

    /// Persist a Clarity database key and commit its value hash into the active MARF.
    pub fn put(&mut self, key: String, value: String) -> Result<(), MarfStoreError> {
        let active = self.active.as_mut().ok_or(MarfStoreError::NoActiveState)?;
        self.marf
            .insert(key.as_bytes(), MarfValue::from_value(value.as_bytes()))?;
        active.values.insert(key, value);
        Ok(())
    }

    /// Read a value from a sealed state.
    #[must_use]
    pub fn get(&self, block: [u8; 32], key: &str) -> Option<&str> {
        self.states.get(&block)?.get(key).map(String::as_str)
    }

    /// Seal the active state and return its MARF root.
    pub fn seal(&mut self) -> Result<StateRoot, MarfStoreError> {
        let active = self.active.take().ok_or(MarfStoreError::NoActiveState)?;
        let root = self.marf.seal()?;
        self.states.insert(active.block, active.values);
        Ok(StateRoot(*root.as_bytes()))
    }

    /// Return a sealed state's MARF root.
    #[must_use]
    pub fn root(&self, block: [u8; 32]) -> Option<StateRoot> {
        self.marf
            .root(block)
            .map(|root: TrieHash| StateRoot(*root.as_bytes()))
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

    use super::{MarfStore, evaluate};

    #[test]
    fn evaluates_clarity_six_programs() {
        let value = evaluate("(+ u20 u22)").expect("Clarity 6 program should evaluate");

        assert_eq!(value, Some(Value::UInt(42)));
    }

    #[test]
    fn rejects_invalid_programs() {
        assert!(evaluate("(unknown-word u1)").is_err());
    }

    #[test]
    fn marf_store_keeps_forked_values_and_roots() {
        let first = [1; 32];
        let second = [2; 32];
        let fork = [3; 32];
        let mut store = MarfStore::default();

        store.begin(None, first).expect("begin first state");
        store
            .put("counter".to_owned(), "one".to_owned())
            .expect("write first state");
        let first_root = store.seal().expect("seal first state");

        store
            .begin(Some(first), second)
            .expect("begin second state");
        store
            .put("counter".to_owned(), "two".to_owned())
            .expect("write second state");
        let second_root = store.seal().expect("seal second state");

        store.begin(Some(first), fork).expect("begin fork state");
        store
            .put("counter".to_owned(), "fork".to_owned())
            .expect("write fork state");
        let fork_root = store.seal().expect("seal fork state");

        assert_eq!(store.get(first, "counter"), Some("one"));
        assert_eq!(store.get(second, "counter"), Some("two"));
        assert_eq!(store.get(fork, "counter"), Some("fork"));
        assert_eq!(store.root(first), Some(first_root));
        assert_eq!(store.root(second), Some(second_root));
        assert_eq!(store.root(fork), Some(fork_root));
        assert_ne!(second_root, fork_root);
    }
}
