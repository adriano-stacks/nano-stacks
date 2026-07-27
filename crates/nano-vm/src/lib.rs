#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use clarity::vm::ast::build_ast;
use clarity::vm::contexts::{ContractContext, GlobalContext};
use clarity::vm::costs::{ExecutionCost, LimitedCostTracker};
use clarity::vm::database::clarity_store::{ContractCommitment, make_contract_hash_key};
use clarity::vm::database::{
    ClarityBackingStore, ClarityDatabase, ClarityDeserializable, MemoryBackingStore,
    NULL_BURN_STATE_DB, NULL_HEADER_DB,
};
use clarity::vm::errors::{ClarityEvalError, RuntimeError, VmExecutionError, VmInternalError};
use clarity::vm::types::QualifiedContractIdentifier;
use clarity::vm::{ClarityVersion, Value, eval_all};
use nano_marf::{MarfError, MarfValue, StateRoot, VersionedMarf};
use nano_primitives::TrieHash;
use stacks_common::consts::CHAIN_ID_TESTNET;
use stacks_common::types::{
    StacksEpochId,
    chainstate::{BlockHeaderHash, StacksBlockId, TrieHash as ReferenceTrieHash},
};
use stacks_common::util::hash::Sha512Trunc256Sum;

/// M0 execution output. M8/M10 replace the marker with Clarity receipts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionResult {
    pub state_root: StateRoot,
}

/// The value and consensus-cost dimensions produced by one Clarity evaluation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Evaluation {
    pub value: Option<Value>,
    pub cost: ExecutionCost,
}

#[must_use]
pub const fn execute_stub() -> ExecutionResult {
    ExecutionResult {
        state_root: StateRoot::empty(),
    }
}

/// A versioned Clarity key/value store whose state roots are committed by the MARF.
#[derive(Debug)]
pub struct MarfStore {
    marf: VersionedMarf,
    side_store: rusqlite::Connection,
    states: BTreeMap<[u8; 32], StoreState>,
    parents: BTreeMap<[u8; 32], Option<[u8; 32]>>,
    heights: BTreeMap<[u8; 32], u32>,
    read_block: Option<[u8; 32]>,
    active: Option<ActiveStore>,
}

#[derive(Clone, Debug)]
struct ActiveStore {
    block: [u8; 32],
    parent: Option<[u8; 32]>,
    height: u32,
    state: StoreState,
}

#[derive(Clone, Debug, Default)]
struct StoreState {
    values: BTreeMap<String, String>,
    metadata: BTreeMap<(String, String), String>,
}

#[derive(Debug)]
pub enum MarfStoreError {
    Marf(MarfError),
    Sql(rusqlite::Error),
    NoActiveState,
}

impl std::fmt::Display for MarfStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Marf(error) => write!(formatter, "MARF error: {error}"),
            Self::Sql(error) => write!(formatter, "SQLite error: {error}"),
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

impl From<rusqlite::Error> for MarfStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error)
    }
}

impl MarfStore {
    /// Create an empty versioned store.
    pub fn new() -> Result<Self, MarfStoreError> {
        Ok(Self {
            marf: VersionedMarf::default(),
            side_store: rusqlite::Connection::open_in_memory()?,
            states: BTreeMap::new(),
            parents: BTreeMap::new(),
            heights: BTreeMap::new(),
            read_block: None,
            active: None,
        })
    }

    /// Create a Clarity database backed by this store.
    pub fn as_clarity_db(&mut self) -> ClarityDatabase<'_> {
        ClarityDatabase::new(self, &NULL_HEADER_DB, &NULL_BURN_STATE_DB)
    }

    /// Begin a new state, inheriting all values from `parent` when present.
    pub fn begin(
        &mut self,
        parent: Option<[u8; 32]>,
        block: [u8; 32],
    ) -> Result<(), MarfStoreError> {
        self.marf.begin(parent, block)?;
        let state = parent
            .and_then(|parent| self.states.get(&parent).cloned())
            .unwrap_or_default();
        let height = parent
            .and_then(|parent| self.heights.get(&parent).copied())
            .map_or(0, |height| height + 1);
        self.active = Some(ActiveStore {
            block,
            parent,
            height,
            state,
        });
        self.read_block = Some(block);
        Ok(())
    }

    /// Persist a Clarity database key and commit its value hash into the active MARF.
    pub fn put(&mut self, key: String, value: String) -> Result<(), MarfStoreError> {
        let active = self.active.as_mut().ok_or(MarfStoreError::NoActiveState)?;
        self.marf
            .insert(key.as_bytes(), MarfValue::from_value(value.as_bytes()))?;
        active.state.values.insert(key, value);
        Ok(())
    }

    /// Read a value from a sealed state.
    #[must_use]
    pub fn get(&self, block: [u8; 32], key: &str) -> Option<&str> {
        self.states.get(&block)?.values.get(key).map(String::as_str)
    }

    /// Seal the active state and return its MARF root.
    pub fn seal(&mut self) -> Result<StateRoot, MarfStoreError> {
        let active = self.active.take().ok_or(MarfStoreError::NoActiveState)?;
        let root = self.marf.seal()?;
        self.parents.insert(active.block, active.parent);
        self.heights.insert(active.block, active.height);
        self.states.insert(active.block, active.state);
        Ok(StateRoot(*root.as_bytes()))
    }

    /// Return a sealed state's MARF root.
    #[must_use]
    pub fn root(&self, block: [u8; 32]) -> Option<StateRoot> {
        self.marf
            .root(block)
            .map(|root: TrieHash| StateRoot(*root.as_bytes()))
    }

    fn read_state(&self) -> Option<&StoreState> {
        if let Some(active) = &self.active {
            return Some(&active.state);
        }
        self.read_block.and_then(|block| self.states.get(&block))
    }

    fn block_at_height(&self, mut block: [u8; 32], height: u32) -> Option<[u8; 32]> {
        loop {
            if self.heights.get(&block).copied() == Some(height) {
                return Some(block);
            }
            block = self.parents.get(&block).copied().flatten()?;
        }
    }

    fn current_block(&self) -> Option<[u8; 32]> {
        self.active
            .as_ref()
            .map(|active| active.block)
            .or(self.read_block)
    }
}

impl ClarityBackingStore for MarfStore {
    fn get_side_store(&mut self) -> &rusqlite::Connection {
        &self.side_store
    }

    fn put_all_data(&mut self, items: Vec<(String, String)>) -> Result<(), VmExecutionError> {
        for (key, value) in items {
            self.put(key, value)
                .map_err(|error| VmInternalError::Expect(error.to_string()))?;
        }
        Ok(())
    }

    fn get_data(&mut self, key: &str) -> Result<Option<String>, VmExecutionError> {
        Ok(self
            .read_state()
            .and_then(|state| state.values.get(key).cloned()))
    }

    fn get_data_from_path(
        &mut self,
        path: &ReferenceTrieHash,
    ) -> Result<Option<String>, VmExecutionError> {
        Ok(self.read_state().and_then(|state| {
            state.values.iter().find_map(|(key, value)| {
                (nano_marf::key_path(key.as_bytes()).as_bytes() == path.as_bytes())
                    .then(|| value.clone())
            })
        }))
    }

    fn get_data_with_proof(
        &mut self,
        key: &str,
    ) -> Result<Option<(String, Vec<u8>)>, VmExecutionError> {
        Ok(self.get_data(key)?.map(|value| (value, Vec::new())))
    }

    fn get_data_with_proof_from_path(
        &mut self,
        path: &ReferenceTrieHash,
    ) -> Result<Option<(String, Vec<u8>)>, VmExecutionError> {
        Ok(self
            .get_data_from_path(path)?
            .map(|value| (value, Vec::new())))
    }

    fn set_block_hash(&mut self, block: StacksBlockId) -> Result<StacksBlockId, VmExecutionError> {
        if !self.states.contains_key(&block.0) {
            return Err(RuntimeError::UnknownBlockHeaderHash(BlockHeaderHash(block.0)).into());
        }
        let previous = self.current_block().unwrap_or([0; 32]);
        self.read_block = Some(block.0);
        Ok(StacksBlockId(previous))
    }

    fn get_block_at_height(&mut self, height: u32) -> Option<StacksBlockId> {
        self.current_block()
            .and_then(|block| self.block_at_height(block, height))
            .map(StacksBlockId)
    }

    fn get_current_block_height(&mut self) -> u32 {
        self.active
            .as_ref()
            .map(|active| active.height + 1)
            .or_else(|| {
                self.current_block()
                    .and_then(|block| self.heights.get(&block).copied())
                    .map(|height| height + 1)
            })
            .unwrap_or(0)
    }

    fn get_open_chain_tip_height(&mut self) -> u32 {
        self.active.as_ref().map_or(0, |active| active.height)
    }

    fn get_open_chain_tip(&mut self) -> StacksBlockId {
        StacksBlockId(self.active.as_ref().map_or([0; 32], |active| active.block))
    }

    fn get_contract_hash(
        &mut self,
        contract: &QualifiedContractIdentifier,
    ) -> Result<(StacksBlockId, Sha512Trunc256Sum), VmExecutionError> {
        let commitment = self
            .get_data(&make_contract_hash_key(contract))?
            .map(|value| ContractCommitment::deserialize(&value))
            .transpose()?
            .ok_or_else(|| VmInternalError::Expect("unknown contract".to_owned()))?;
        let block = self
            .get_block_at_height(commitment.block_height)
            .ok_or_else(|| VmInternalError::Expect("unknown contract block height".to_owned()))?;
        Ok((block, commitment.hash))
    }

    fn insert_metadata(
        &mut self,
        contract: &QualifiedContractIdentifier,
        key: &str,
        value: &str,
    ) -> Result<(), VmExecutionError> {
        let active = self.active.as_mut().ok_or_else(|| {
            VmInternalError::Expect("metadata write without an active state".to_owned())
        })?;
        active
            .state
            .metadata
            .insert((contract.to_string(), key.to_owned()), value.to_owned());
        Ok(())
    }

    fn get_metadata(
        &mut self,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> Result<Option<String>, VmExecutionError> {
        Ok(self.read_state().and_then(|state| {
            state
                .metadata
                .get(&(contract.to_string(), key.to_owned()))
                .cloned()
        }))
    }

    fn get_metadata_manual(
        &mut self,
        height: u32,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> Result<Option<String>, VmExecutionError> {
        let block = self
            .current_block()
            .and_then(|block| self.block_at_height(block, height))
            .ok_or_else(|| RuntimeError::BadBlockHeight(height.to_string()))?;
        Ok(self.states.get(&block).and_then(|state| {
            state
                .metadata
                .get(&(contract.to_string(), key.to_owned()))
                .cloned()
        }))
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

/// Evaluate a Clarity 6 program against an active MARF-backed state.
pub fn evaluate_in_store(
    store: &mut MarfStore,
    source: &str,
) -> Result<Option<Value>, ClarityEvalError> {
    Ok(evaluate_with_tracker(store, source, LimitedCostTracker::new_free())?.value)
}

/// Evaluate a Clarity 6 program with the supplied consensus cost tracker.
pub fn evaluate_with_tracker(
    store: &mut MarfStore,
    source: &str,
    cost_tracker: LimitedCostTracker,
) -> Result<Evaluation, ClarityEvalError> {
    let contract_id = QualifiedContractIdentifier::transient();
    let database = store.as_clarity_db();
    let mut context = GlobalContext::new(
        false,
        CHAIN_ID_TESTNET,
        database,
        cost_tracker,
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

    let value = context
        .execute(|global| eval_all(&expressions, &mut contract, global, None))
        .map_err(ClarityEvalError::from)?;
    Ok(Evaluation {
        value,
        cost: context.cost_track.get_total(),
    })
}

#[cfg(test)]
mod tests {
    use clarity::vm::Value;

    use super::{MarfStore, evaluate, evaluate_in_store};

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
        let mut store = MarfStore::new().expect("create MARF store");

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

    #[test]
    fn evaluates_against_an_active_marf_store() {
        let mut store = MarfStore::new().expect("create MARF store");
        store.begin(None, [1; 32]).expect("begin state");

        let value =
            evaluate_in_store(&mut store, "(+ u20 u22)").expect("evaluate against MARF store");

        assert_eq!(value, Some(Value::UInt(42)));
        store.seal().expect("seal state");
    }

    #[test]
    fn persists_clarity_data_variables_in_the_marf_store() {
        let mut store = MarfStore::new().expect("create MARF store");
        let block = [1; 32];
        store.begin(None, block).expect("begin state");

        let value = evaluate_in_store(
            &mut store,
            "(define-data-var counter uint u1) (var-set counter u2) (var-get counter)",
        )
        .expect("evaluate persistent data variable");

        assert_eq!(value, Some(Value::UInt(2)));
        store.seal().expect("seal state");
        assert!(store.root(block).is_some());
    }
}
