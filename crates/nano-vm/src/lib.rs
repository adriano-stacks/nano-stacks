#![forbid(unsafe_code)]

use std::{collections::BTreeMap, path::Path};

use clarity::vm::ast::build_ast;
use clarity::vm::contexts::{ContractContext, GlobalContext, OwnedEnvironment};
use clarity::vm::costs::{ExecutionCost, LimitedCostTracker};
use clarity::vm::database::clarity_store::{ContractCommitment, make_contract_hash_key};
use clarity::vm::database::{
    BurnStateDB, ClarityBackingStore, ClarityDatabase, ClarityDeserializable, MemoryBackingStore,
    NULL_BURN_STATE_DB, NULL_HEADER_DB,
};
use clarity::vm::errors::{ClarityEvalError, RuntimeError, VmExecutionError, VmInternalError};
use clarity::vm::events::StacksTransactionEvent;
use clarity::vm::representations::SymbolicExpression;
use clarity::vm::types::{BuffData, PrincipalData, QualifiedContractIdentifier, SequenceData};
use clarity::vm::{ClarityVersion, Value, eval_all};
use nano_marf::{
    CheckpointError, MarfError, MarfValue, StateRoot, TriePointer, VersionedMarf, import_checkpoint,
};
use nano_primitives::TrieHash;
use rusqlite::{OptionalExtension, params};
use stacks_common::consts::CHAIN_ID_TESTNET;
use stacks_common::types::{
    StacksEpoch, StacksEpochId,
    chainstate::{
        BlockHeaderHash, BurnchainHeaderHash, ConsensusHash, SortitionId, StacksBlockId,
        TrieHash as ReferenceTrieHash,
    },
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

/// The observable result of a transaction executed by the Clarity VM.
#[derive(Clone, Debug, PartialEq)]
pub struct TransactionResult {
    pub value: Option<Value>,
    pub cost: ExecutionCost,
    pub events: Vec<StacksTransactionEvent>,
}

/// Bitcoin context required while executing one block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BitcoinBlockContext {
    pub height: u64,
    pub first_height: u64,
    pub prepare_phase_length: u32,
    pub reward_phase_length: u32,
    pub rejection_fraction: u64,
    pub v1_unlock_height: u32,
    pub v2_unlock_height: u32,
    pub v3_unlock_height: u32,
    pub v4_unlock_height: u32,
}

impl BitcoinBlockContext {
    /// Construct a context when only the current Bitcoin height is available.
    #[must_use]
    pub const fn at_height(height: u64) -> Self {
        Self {
            height,
            first_height: 0,
            prepare_phase_length: 0,
            reward_phase_length: 0,
            rejection_fraction: 0,
            v1_unlock_height: u32::MAX,
            v2_unlock_height: u32::MAX,
            v3_unlock_height: u32::MAX,
            v4_unlock_height: u32::MAX,
        }
    }
}

/// Bitcoin state available while executing one block.
#[derive(Debug, Default)]
struct BitcoinContext {
    height: u32,
    first_height: u32,
    prepare_phase_length: u32,
    reward_phase_length: u32,
    rejection_fraction: u64,
    v1_unlock_height: u32,
    v2_unlock_height: u32,
    v3_unlock_height: u32,
    v4_unlock_height: u32,
}

impl BitcoinContext {
    fn new(context: BitcoinBlockContext) -> Result<Self, MarfStoreError> {
        Ok(Self {
            height: u32::try_from(context.height)
                .map_err(|_| MarfStoreError::BitcoinHeightOverflow(context.height))?,
            first_height: u32::try_from(context.first_height)
                .map_err(|_| MarfStoreError::BitcoinHeightOverflow(context.first_height))?,
            prepare_phase_length: context.prepare_phase_length,
            reward_phase_length: context.reward_phase_length,
            rejection_fraction: context.rejection_fraction,
            v1_unlock_height: context.v1_unlock_height,
            v2_unlock_height: context.v2_unlock_height,
            v3_unlock_height: context.v3_unlock_height,
            v4_unlock_height: context.v4_unlock_height,
        })
    }
}

impl BurnStateDB for BitcoinContext {
    fn get_tip_burn_block_height(&self) -> Option<u32> {
        Some(self.height)
    }

    fn get_tip_sortition_id(&self) -> Option<SortitionId> {
        None
    }

    fn get_v1_unlock_height(&self) -> u32 {
        self.v1_unlock_height
    }

    fn get_v2_unlock_height(&self) -> u32 {
        self.v2_unlock_height
    }

    fn get_v3_unlock_height(&self) -> u32 {
        self.v3_unlock_height
    }

    fn get_pox_3_activation_height(&self) -> u32 {
        u32::MAX
    }

    fn get_pox_4_activation_height(&self) -> u32 {
        u32::MAX
    }

    fn get_pox_5_activation_height(&self) -> u32 {
        self.v4_unlock_height
    }

    fn get_burn_block_height(&self, _sortition_id: &SortitionId) -> Option<u32> {
        None
    }

    fn get_burn_start_height(&self) -> u32 {
        self.first_height
    }

    fn get_pox_prepare_length(&self) -> u32 {
        self.prepare_phase_length
    }

    fn get_pox_reward_cycle_length(&self) -> u32 {
        self.reward_phase_length
            .saturating_add(self.prepare_phase_length)
    }

    fn get_pox_rejection_fraction(&self) -> u64 {
        self.rejection_fraction
    }

    fn get_burn_header_hash(
        &self,
        _height: u32,
        _sortition_id: &SortitionId,
    ) -> Option<BurnchainHeaderHash> {
        None
    }

    fn get_sortition_id_from_consensus_hash(
        &self,
        _consensus_hash: &ConsensusHash,
    ) -> Option<SortitionId> {
        None
    }

    fn get_stacks_epoch(&self, _height: u32) -> Option<StacksEpoch<ExecutionCost>> {
        Some(StacksEpoch {
            epoch_id: StacksEpochId::Epoch40,
            start_height: 0,
            end_height: u64::MAX,
            block_limit: ExecutionCost::max_value(),
            network_epoch: 0,
        })
    }

    fn get_stacks_epoch_by_epoch_id(
        &self,
        _epoch_id: &StacksEpochId,
    ) -> Option<StacksEpoch<ExecutionCost>> {
        self.get_stacks_epoch(self.height)
    }

    fn get_pox_payout_addrs(
        &self,
        _height: u32,
        _sortition_id: &SortitionId,
    ) -> Option<(Vec<clarity::vm::types::TupleData>, u128)> {
        None
    }
}

/// Epoch 4 Clarity execution over a versioned MARF-backed store.
#[derive(Debug)]
pub struct Vm {
    store: MarfStore,
    context: BitcoinContext,
}

impl Vm {
    /// Create an empty VM.
    pub fn new() -> Result<Self, MarfStoreError> {
        Ok(Self {
            store: MarfStore::new()?,
            context: BitcoinContext::default(),
        })
    }

    /// Open a VM at a checkpointed Clarity state.
    pub fn from_checkpoint(
        path: impl AsRef<Path>,
        source: [u8; 32],
        expected_root: TrieHash,
    ) -> Result<Self, MarfStoreError> {
        Ok(Self {
            store: MarfStore::from_checkpoint(path, source, expected_root)?,
            context: BitcoinContext::default(),
        })
    }

    /// Begin execution for a block state.
    pub fn begin_block(
        &mut self,
        parent: Option<[u8; 32]>,
        block: [u8; 32],
    ) -> Result<(), MarfStoreError> {
        self.begin_block_at_bitcoin_height(parent, block, 0)
    }

    /// Begin execution for a block state at the supplied Bitcoin height.
    pub fn begin_block_at_bitcoin_height(
        &mut self,
        parent: Option<[u8; 32]>,
        block: [u8; 32],
        bitcoin_height: u64,
    ) -> Result<(), MarfStoreError> {
        self.begin_block_with_bitcoin_context(
            parent,
            block,
            BitcoinBlockContext::at_height(bitcoin_height),
        )
    }

    /// Begin execution with the complete Bitcoin context for this block.
    pub fn begin_block_with_bitcoin_context(
        &mut self,
        parent: Option<[u8; 32]>,
        block: [u8; 32],
        bitcoin_context: BitcoinBlockContext,
    ) -> Result<(), MarfStoreError> {
        self.context = BitcoinContext::new(bitcoin_context)?;
        self.store.begin(parent, block)
    }

    /// Begin block execution using the supplied temporary MARF state ID.
    pub fn begin_block_execution(
        &mut self,
        parent: Option<[u8; 32]>,
        temporary_state_id: [u8; 32],
        bitcoin_context: BitcoinBlockContext,
    ) -> Result<(), MarfStoreError> {
        self.begin_block_with_bitcoin_context(parent, temporary_state_id, bitcoin_context)
    }

    /// Execute a Clarity 6 program with the supplied consensus cost tracker.
    pub fn execute(
        &mut self,
        source: &str,
        cost_tracker: LimitedCostTracker,
    ) -> Result<Evaluation, ClarityEvalError> {
        let Self { store, context } = self;
        evaluate_with_tracker_in_context(store, context, source, cost_tracker)
    }

    /// Store the timestamp supplied by the current Nakamoto block header.
    pub fn setup_block_metadata(&mut self, timestamp: u64) -> Result<(), VmExecutionError> {
        let Self { store, context } = self;
        setup_block_metadata_in_context(store, context, timestamp)
    }

    /// Read the tenure height stored in the active Clarity state.
    pub fn tenure_height(&mut self) -> Result<u32, VmExecutionError> {
        let Self { store, context } = self;
        tenure_height_in_context(store, context)
    }

    /// Store the tenure height for a newly started tenure.
    pub fn set_tenure_height(&mut self, height: u32) -> Result<(), VmExecutionError> {
        let Self { store, context } = self;
        set_tenure_height_in_context(store, context, height)
    }

    /// Credit STX scheduled to unlock at the current Stacks block height.
    pub fn process_scheduled_unlocks(&mut self) -> Result<u128, VmExecutionError> {
        let Self { store, context } = self;
        process_scheduled_unlocks_in_context(store, context)
    }

    /// Increase the liquid STX supply by a block-finalization amount.
    pub fn increment_liquid_stx_supply(&mut self, amount: u128) -> Result<(), VmExecutionError> {
        let Self { store, context } = self;
        increment_liquid_stx_supply_in_context(store, context, amount)
    }

    /// Publish a Clarity contract in the active block state.
    pub fn deploy_contract(
        &mut self,
        contract: QualifiedContractIdentifier,
        version: ClarityVersion,
        source: &str,
        cost_tracker: LimitedCostTracker,
    ) -> Result<TransactionResult, ClarityEvalError> {
        let Self { store, context } = self;
        deploy_contract_in_context(store, context, contract, version, source, cost_tracker)
    }

    /// Call a published contract with consensus-serialized Clarity arguments.
    pub fn execute_contract_call(
        &mut self,
        sender: PrincipalData,
        sponsor: Option<PrincipalData>,
        contract: QualifiedContractIdentifier,
        function: &str,
        arguments: &[Vec<u8>],
        cost_tracker: LimitedCostTracker,
    ) -> Result<TransactionResult, VmExecutionError> {
        let Self { store, context } = self;
        execute_contract_call_in_context(
            store,
            context,
            ContractCall {
                sender,
                sponsor,
                contract,
                function,
                arguments,
            },
            cost_tracker,
        )
    }

    /// Transfer STX between principals in the active block state.
    pub fn transfer_stx(
        &mut self,
        sender: &PrincipalData,
        recipient: &PrincipalData,
        amount: u128,
        memo: &[u8],
        cost_tracker: LimitedCostTracker,
    ) -> Result<TransactionResult, VmExecutionError> {
        let Self { store, context } = self;
        transfer_stx_in_context(
            store,
            context,
            sender,
            recipient,
            amount,
            memo,
            cost_tracker,
        )
    }

    /// Read an account nonce from the active block state.
    pub fn account_nonce(&mut self, principal: &PrincipalData) -> Result<u64, VmExecutionError> {
        let Self { store, context } = self;
        account_nonce_in_context(store, context, principal)
    }

    /// Debit a transaction fee from an account's available STX balance.
    pub fn debit_fee(&mut self, payer: &PrincipalData, fee: u64) -> Result<(), VmExecutionError> {
        let Self { store, context } = self;
        debit_fee_in_context(store, context, payer, fee)
    }

    /// Store a transaction nonce in the active block state.
    pub fn set_account_nonce(
        &mut self,
        principal: &PrincipalData,
        nonce: u64,
    ) -> Result<(), VmExecutionError> {
        let Self { store, context } = self;
        set_account_nonce_in_context(store, context, principal, nonce)
    }

    /// Seal the active block state.
    pub fn seal_block(&mut self) -> Result<StateRoot, MarfStoreError> {
        self.store.seal()
    }

    /// Seal the active state and store it under the committed block ID.
    pub fn seal_block_to(&mut self, block: [u8; 32]) -> Result<StateRoot, MarfStoreError> {
        self.store.seal_to(block)
    }

    /// Access the state root for a sealed block.
    #[must_use]
    pub fn root(&self, block: [u8; 32]) -> Option<StateRoot> {
        self.store.root(block)
    }

    /// Return the MARF content hash before ancestry is incorporated.
    #[must_use]
    pub fn content_root(&self, block: [u8; 32]) -> Option<TrieHash> {
        self.store.content_root(block)
    }

    /// Return the committed MARF leaves for a block state.
    #[must_use]
    pub fn state_leaves(&self, block: [u8; 32]) -> Option<Vec<(TrieHash, MarfValue)>> {
        self.store.leaves(block)
    }

    /// Return the root pointers in their consensus serialization order.
    #[must_use]
    pub fn root_pointers(&self, block: [u8; 32]) -> Option<Vec<TriePointer>> {
        self.store.root_pointers(block)
    }

    /// Access a stored Clarity database value for a sealed block.
    #[must_use]
    pub fn get(&self, block: [u8; 32], key: &str) -> Option<&str> {
        self.store.get(block, key)
    }
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
    Checkpoint(CheckpointError),
    Sql(rusqlite::Error),
    NoActiveState,
    BitcoinHeightOverflow(u64),
}

impl std::fmt::Display for MarfStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Marf(error) => write!(formatter, "MARF error: {error}"),
            Self::Checkpoint(error) => write!(formatter, "checkpoint error: {error}"),
            Self::Sql(error) => write!(formatter, "SQLite error: {error}"),
            Self::NoActiveState => formatter.write_str("no active VM state"),
            Self::BitcoinHeightOverflow(height) => {
                write!(formatter, "Bitcoin height {height} exceeds u32")
            }
        }
    }
}

impl std::error::Error for MarfStoreError {}

impl From<MarfError> for MarfStoreError {
    fn from(error: MarfError) -> Self {
        Self::Marf(error)
    }
}

impl From<CheckpointError> for MarfStoreError {
    fn from(error: CheckpointError) -> Self {
        Self::Checkpoint(error)
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
            side_store: create_side_store()?,
            states: BTreeMap::new(),
            parents: BTreeMap::new(),
            heights: BTreeMap::new(),
            read_block: None,
            active: None,
        })
    }

    /// Load a checkpointed Clarity MARF and its corresponding `SQLite` side tables.
    pub fn from_checkpoint(
        path: impl AsRef<Path>,
        source: [u8; 32],
        expected_root: TrieHash,
    ) -> Result<Self, MarfStoreError> {
        let path = path.as_ref();
        let marf = import_checkpoint(path, source, expected_root)?;
        let side_store = copy_side_store(path)?;
        let mut states = BTreeMap::new();
        states.insert(source, StoreState::default());

        Ok(Self {
            marf,
            side_store,
            states,
            parents: BTreeMap::new(),
            heights: BTreeMap::new(),
            read_block: Some(source),
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
            .and_then(|parent| {
                self.heights
                    .get(&parent)
                    .copied()
                    .or_else(|| self.marf.height(parent))
            })
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
        let value_hash = MarfValue::from_value(value.as_bytes());
        self.marf.insert(key.as_bytes(), value_hash)?;
        self.side_store.execute(
            "INSERT OR REPLACE INTO data_table (key, value) VALUES (?1, ?2)",
            params![marf_value_key(value_hash), &value],
        )?;
        let active = self.active.as_mut().ok_or(MarfStoreError::NoActiveState)?;
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
        let block = self
            .active
            .as_ref()
            .ok_or(MarfStoreError::NoActiveState)?
            .block;
        self.seal_to(block)
    }

    /// Seal the active state and register it under its committed block ID.
    pub fn seal_to(&mut self, block: [u8; 32]) -> Result<StateRoot, MarfStoreError> {
        let active = self.active.take().ok_or(MarfStoreError::NoActiveState)?;
        let root = self.marf.seal_to(block)?;
        self.parents.insert(block, active.parent);
        self.heights.insert(block, active.height);
        self.states.insert(block, active.state);
        self.read_block = Some(block);
        Ok(StateRoot(*root.as_bytes()))
    }

    /// Return a sealed state's MARF root.
    #[must_use]
    pub fn root(&self, block: [u8; 32]) -> Option<StateRoot> {
        self.marf
            .root(block)
            .map(|root: TrieHash| StateRoot(*root.as_bytes()))
    }

    /// Return the MARF content hash before ancestry is incorporated.
    #[must_use]
    pub fn content_root(&self, block: [u8; 32]) -> Option<TrieHash> {
        self.marf.content_root(block)
    }

    /// Return the committed MARF leaves for a block state.
    #[must_use]
    pub fn leaves(&self, block: [u8; 32]) -> Option<Vec<(TrieHash, MarfValue)>> {
        self.marf.leaves(block)
    }

    /// Return the root pointers in their consensus serialization order.
    #[must_use]
    pub fn root_pointers(&self, block: [u8; 32]) -> Option<Vec<TriePointer>> {
        self.marf.root_pointers(block)
    }

    fn read_state(&self) -> Option<&StoreState> {
        if let Some(block) = self.read_block {
            if self
                .active
                .as_ref()
                .is_some_and(|active| active.block == block)
            {
                return self.active.as_ref().map(|active| &active.state);
            }
            return self.states.get(&block);
        }
        self.active.as_ref().map(|active| &active.state)
    }

    fn block_at_height(&self, mut block: [u8; 32], height: u32) -> Option<[u8; 32]> {
        loop {
            if self.heights.get(&block).copied() == Some(height) {
                return Some(block);
            }
            block = self.parents.get(&block).copied().flatten()?;
        }
    }

    fn checkpoint_block_at_height(&self, block: [u8; 32], height: u32) -> Option<[u8; 32]> {
        self.marf.block_at_height(block, height).or_else(|| {
            self.active
                .as_ref()
                .filter(|active| active.block == block)
                .and_then(|active| active.parent)
                .and_then(|parent| self.marf.block_at_height(parent, height))
        })
    }

    fn current_block(&self) -> Option<[u8; 32]> {
        self.active
            .as_ref()
            .map(|active| active.block)
            .or(self.read_block)
    }

    fn selected_state(&self) -> Option<(&StoreState, Option<[u8; 32]>)> {
        if let Some(block) = self.read_block {
            if let Some(active) = self.active.as_ref().filter(|active| active.block == block) {
                return Some((&active.state, active.parent));
            }
            return self.states.get(&block).map(|state| (state, Some(block)));
        }
        self.active
            .as_ref()
            .map(|active| (&active.state, active.parent))
    }

    fn data_from_marf(
        &self,
        block: Option<[u8; 32]>,
        key: &str,
    ) -> Result<Option<String>, VmExecutionError> {
        block
            .and_then(|block| self.marf.get(block, key.as_bytes()))
            .map_or(Ok(None), |value| self.data_from_side_store(value))
    }

    fn data_from_path(
        &self,
        block: Option<[u8; 32]>,
        path: [u8; 32],
    ) -> Result<Option<String>, VmExecutionError> {
        block
            .and_then(|block| self.marf.get_path(block, path))
            .map_or(Ok(None), |value| self.data_from_side_store(value))
    }

    fn data_from_side_store(&self, value: MarfValue) -> Result<Option<String>, VmExecutionError> {
        Ok(self
            .side_store
            .query_row(
                "SELECT value FROM data_table WHERE key = ?1",
                params![marf_value_key(value)],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| VmInternalError::Expect(format!("side-store read failed: {error}")))?)
    }

    fn metadata_from_side_store(
        &self,
        block: [u8; 32],
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> Result<Option<String>, VmExecutionError> {
        Ok(self
            .side_store
            .query_row(
                "SELECT value FROM metadata_table WHERE blockhash = ?1 AND key = ?2",
                params![block_hex(block), format!("clr-meta::{contract}::{key}"),],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| VmInternalError::Expect(format!("metadata read failed: {error}")))?)
    }
}

fn create_side_store() -> Result<rusqlite::Connection, rusqlite::Error> {
    let connection = rusqlite::Connection::open_in_memory()?;
    connection.execute_batch(
        "CREATE TABLE data_table (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE metadata_table (
             key TEXT NOT NULL,
             blockhash TEXT,
             value TEXT NOT NULL,
             UNIQUE (key, blockhash)
         );
         CREATE INDEX md_blockhashes ON metadata_table(blockhash);",
    )?;
    Ok(connection)
}

fn copy_side_store(path: &Path) -> Result<rusqlite::Connection, MarfStoreError> {
    let source_uri = format!("file:{}?immutable=1", path.display());
    let source = rusqlite::Connection::open_with_flags(
        source_uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    let destination = create_side_store()?;

    let mut data = source.prepare("SELECT key, value FROM data_table")?;
    let data_rows = data.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in data_rows {
        let (key, value) = row?;
        destination.execute(
            "INSERT INTO data_table (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
    }

    let mut metadata = source.prepare("SELECT key, blockhash, value FROM metadata_table")?;
    let metadata_rows = metadata.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in metadata_rows {
        let (key, blockhash, value) = row?;
        destination.execute(
            "INSERT INTO metadata_table (key, blockhash, value) VALUES (?1, ?2, ?3)",
            params![key, blockhash, value],
        )?;
    }
    Ok(destination)
}

fn marf_value_key(value: MarfValue) -> String {
    hex_bytes(value.as_bytes())
}

fn block_hex(block: [u8; 32]) -> String {
    hex_bytes(&block)
}

fn hex_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write;

    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(hex, "{byte:02x}").expect("writing to a string cannot fail");
    }
    hex
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
        let Some((state, fallback_block)) = self.selected_state() else {
            return Ok(None);
        };
        state.values.get(key).cloned().map_or_else(
            || self.data_from_marf(fallback_block, key),
            |value| Ok(Some(value)),
        )
    }

    fn get_data_from_path(
        &mut self,
        path: &ReferenceTrieHash,
    ) -> Result<Option<String>, VmExecutionError> {
        let Some((state, fallback_block)) = self.selected_state() else {
            return Ok(None);
        };
        state
            .values
            .iter()
            .find_map(|(key, value)| {
                (nano_marf::key_path(key.as_bytes()).as_bytes() == path.as_bytes())
                    .then(|| value.clone())
            })
            .map_or_else(
                || self.data_from_path(fallback_block, *path.as_bytes()),
                |value| Ok(Some(value)),
            )
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
        if !self.states.contains_key(&block.0)
            && self
                .active
                .as_ref()
                .is_none_or(|active| active.block != block.0)
        {
            return Err(RuntimeError::UnknownBlockHeaderHash(BlockHeaderHash(block.0)).into());
        }
        let previous = self.current_block().unwrap_or([0; 32]);
        self.read_block = Some(block.0);
        Ok(StacksBlockId(previous))
    }

    fn get_block_at_height(&mut self, height: u32) -> Option<StacksBlockId> {
        self.current_block()
            .and_then(|block| {
                self.block_at_height(block, height)
                    .or_else(|| self.checkpoint_block_at_height(block, height))
            })
            .map(StacksBlockId)
    }

    fn get_current_block_height(&mut self) -> u32 {
        self.active
            .as_ref()
            .map(|active| active.height + 1)
            .or_else(|| {
                self.current_block()
                    .and_then(|block| {
                        self.marf
                            .height(block)
                            .or_else(|| self.heights.get(&block).copied())
                    })
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
        if let Some(value) = self.read_state().and_then(|state| {
            state
                .metadata
                .get(&(contract.to_string(), key.to_owned()))
                .cloned()
        }) {
            return Ok(Some(value));
        }
        let (block, _) = self.get_contract_hash(contract)?;
        self.metadata_from_side_store(block.0, contract, key)
    }

    fn get_metadata_manual(
        &mut self,
        height: u32,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> Result<Option<String>, VmExecutionError> {
        let block = self
            .current_block()
            .and_then(|block| {
                self.block_at_height(block, height)
                    .or_else(|| self.checkpoint_block_at_height(block, height))
            })
            .ok_or_else(|| RuntimeError::BadBlockHeight(height.to_string()))?;
        if let Some(value) = self.states.get(&block).and_then(|state| {
            state
                .metadata
                .get(&(contract.to_string(), key.to_owned()))
                .cloned()
        }) {
            return Ok(Some(value));
        }
        self.metadata_from_side_store(block, contract, key)
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
    evaluate_with_tracker_in_context(store, &NULL_BURN_STATE_DB, source, cost_tracker)
}

fn evaluate_with_tracker_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn BurnStateDB,
    source: &str,
    cost_tracker: LimitedCostTracker,
) -> Result<Evaluation, ClarityEvalError> {
    let contract_id = QualifiedContractIdentifier::transient();
    let database = clarity_database(store, bitcoin_context);
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

/// Publish a versioned Clarity contract in an active MARF-backed state.
pub fn deploy_contract(
    store: &mut MarfStore,
    contract: QualifiedContractIdentifier,
    version: ClarityVersion,
    source: &str,
    cost_tracker: LimitedCostTracker,
) -> Result<TransactionResult, ClarityEvalError> {
    deploy_contract_in_context(
        store,
        &NULL_BURN_STATE_DB,
        contract,
        version,
        source,
        cost_tracker,
    )
}

fn deploy_contract_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn BurnStateDB,
    contract: QualifiedContractIdentifier,
    version: ClarityVersion,
    source: &str,
    cost_tracker: LimitedCostTracker,
) -> Result<TransactionResult, ClarityEvalError> {
    let database = clarity_database(store, bitcoin_context);
    let mut environment = OwnedEnvironment::new_cost_limited(
        false,
        CHAIN_ID_TESTNET,
        database,
        cost_tracker,
        StacksEpochId::Epoch40,
    );
    let ((), _, events) =
        environment.initialize_versioned_contract(contract, version, source, None)?;

    Ok(TransactionResult {
        value: None,
        cost: environment.get_cost_total(),
        events,
    })
}

/// Call a Clarity contract using the encoded arguments found in a transaction payload.
pub fn execute_contract_call(
    store: &mut MarfStore,
    sender: PrincipalData,
    sponsor: Option<PrincipalData>,
    contract: QualifiedContractIdentifier,
    function: &str,
    arguments: &[Vec<u8>],
    cost_tracker: LimitedCostTracker,
) -> Result<TransactionResult, VmExecutionError> {
    execute_contract_call_in_context(
        store,
        &NULL_BURN_STATE_DB,
        ContractCall {
            sender,
            sponsor,
            contract,
            function,
            arguments,
        },
        cost_tracker,
    )
}

struct ContractCall<'a> {
    sender: PrincipalData,
    sponsor: Option<PrincipalData>,
    contract: QualifiedContractIdentifier,
    function: &'a str,
    arguments: &'a [Vec<u8>],
}

fn execute_contract_call_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn BurnStateDB,
    call: ContractCall<'_>,
    cost_tracker: LimitedCostTracker,
) -> Result<TransactionResult, VmExecutionError> {
    let arguments = call
        .arguments
        .iter()
        .map(|argument| {
            let mut bytes = argument.as_slice();
            let value = Value::deserialize_read(&mut bytes, None, false).map_err(|error| {
                VmInternalError::Expect(format!("invalid transaction argument: {error}"))
            })?;
            if !bytes.is_empty() {
                return Err(VmInternalError::Expect(
                    "transaction argument has trailing bytes".to_owned(),
                )
                .into());
            }
            Ok(SymbolicExpression::atom_value(value))
        })
        .collect::<Result<Vec<_>, VmExecutionError>>()?;
    let database = clarity_database(store, bitcoin_context);
    let mut environment = OwnedEnvironment::new_cost_limited(
        false,
        CHAIN_ID_TESTNET,
        database,
        cost_tracker,
        StacksEpochId::Epoch40,
    );
    let (value, _, events) = environment.execute_transaction(
        call.sender,
        call.sponsor,
        call.contract,
        call.function,
        &arguments,
    )?;

    Ok(TransactionResult {
        value: Some(value),
        cost: environment.get_cost_total(),
        events,
    })
}

/// Transfer STX using the Clarity VM's account and event machinery.
pub fn transfer_stx(
    store: &mut MarfStore,
    sender: &PrincipalData,
    recipient: &PrincipalData,
    amount: u128,
    memo: &[u8],
    cost_tracker: LimitedCostTracker,
) -> Result<TransactionResult, VmExecutionError> {
    transfer_stx_in_context(
        store,
        &NULL_BURN_STATE_DB,
        sender,
        recipient,
        amount,
        memo,
        cost_tracker,
    )
}

fn transfer_stx_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn BurnStateDB,
    sender: &PrincipalData,
    recipient: &PrincipalData,
    amount: u128,
    memo: &[u8],
    cost_tracker: LimitedCostTracker,
) -> Result<TransactionResult, VmExecutionError> {
    let database = clarity_database(store, bitcoin_context);
    let mut environment = OwnedEnvironment::new_cost_limited(
        false,
        CHAIN_ID_TESTNET,
        database,
        cost_tracker,
        StacksEpochId::Epoch40,
    );
    let (value, _, events) = environment.stx_transfer(
        sender,
        recipient,
        amount,
        &BuffData {
            data: memo.to_vec(),
        },
    )?;

    Ok(TransactionResult {
        value: Some(value),
        cost: environment.get_cost_total(),
        events,
    })
}

/// Debit an account's available STX balance in an isolated database transaction.
pub fn debit_fee(
    store: &mut MarfStore,
    payer: &PrincipalData,
    fee: u64,
) -> Result<(), VmExecutionError> {
    debit_fee_in_context(store, &NULL_BURN_STATE_DB, payer, fee)
}

fn debit_fee_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn BurnStateDB,
    payer: &PrincipalData,
    fee: u64,
) -> Result<(), VmExecutionError> {
    if fee == 0 {
        return Ok(());
    }
    let database = clarity_database(store, bitcoin_context);
    let mut context = GlobalContext::new(
        false,
        CHAIN_ID_TESTNET,
        database,
        LimitedCostTracker::new_free(),
        StacksEpochId::Epoch40,
    );
    context.execute(|global| {
        let mut balance = global.database.get_stx_balance_snapshot(payer)?;
        if !balance.can_transfer(u128::from(fee))? {
            return Err(VmInternalError::InsufficientBalance.into());
        }
        balance.debit(u128::from(fee))?;
        balance.save()
    })
}

fn process_scheduled_unlocks_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn BurnStateDB,
) -> Result<u128, VmExecutionError> {
    let database = clarity_database(store, bitcoin_context);
    let mut context = GlobalContext::new(
        false,
        CHAIN_ID_TESTNET,
        database,
        LimitedCostTracker::new_free(),
        StacksEpochId::Epoch40,
    );
    context.execute(|global| {
        let block_height = Value::UInt(u128::from(global.database.get_current_block_height()));
        let lockup_contract = clarity::boot_util::boot_code_id("lockup", false);
        let entries = match global.database.fetch_entry_unknown_descriptor(
            &lockup_contract,
            "lockups",
            &block_height,
            &global.epoch_id,
        )? {
            Value::Optional(optional) => match optional.data.map(|value| *value) {
                Some(Value::Sequence(SequenceData::List(entries))) => entries.data,
                _ => return Ok(0),
            },
            _ => return Ok(0),
        };
        let mut total = 0_u128;
        for entry in entries {
            let schedule = entry.expect_tuple()?;
            let amount = schedule.get("amount")?.to_owned().expect_u128()?;
            let recipient = schedule.get("recipient")?.to_owned().expect_principal()?;
            let mut balance = global.database.get_stx_balance_snapshot(&recipient)?;
            balance.credit(amount)?;
            balance.save()?;
            total = total
                .checked_add(amount)
                .ok_or(RuntimeError::ArithmeticOverflow)?;
        }
        Ok(total)
    })
}

fn increment_liquid_stx_supply_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn BurnStateDB,
    amount: u128,
) -> Result<(), VmExecutionError> {
    let database = clarity_database(store, bitcoin_context);
    let mut context = GlobalContext::new(
        false,
        CHAIN_ID_TESTNET,
        database,
        LimitedCostTracker::new_free(),
        StacksEpochId::Epoch40,
    );
    context.execute(|global| global.database.increment_ustx_liquid_supply(amount))
}

/// Read an account nonce in an isolated database transaction.
pub fn account_nonce(
    store: &mut MarfStore,
    principal: &PrincipalData,
) -> Result<u64, VmExecutionError> {
    account_nonce_in_context(store, &NULL_BURN_STATE_DB, principal)
}

fn account_nonce_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn BurnStateDB,
    principal: &PrincipalData,
) -> Result<u64, VmExecutionError> {
    let database = clarity_database(store, bitcoin_context);
    let mut context = GlobalContext::new(
        false,
        CHAIN_ID_TESTNET,
        database,
        LimitedCostTracker::new_free(),
        StacksEpochId::Epoch40,
    );
    context.execute(|global| global.database.get_account_nonce(principal))
}

/// Store an account nonce in an isolated database transaction.
pub fn set_account_nonce(
    store: &mut MarfStore,
    principal: &PrincipalData,
    nonce: u64,
) -> Result<(), VmExecutionError> {
    set_account_nonce_in_context(store, &NULL_BURN_STATE_DB, principal, nonce)
}

fn set_account_nonce_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn BurnStateDB,
    principal: &PrincipalData,
    nonce: u64,
) -> Result<(), VmExecutionError> {
    let database = clarity_database(store, bitcoin_context);
    let mut context = GlobalContext::new(
        false,
        CHAIN_ID_TESTNET,
        database,
        LimitedCostTracker::new_free(),
        StacksEpochId::Epoch40,
    );
    context.execute(|global| global.database.set_account_nonce(principal, nonce))
}

fn clarity_database<'a>(
    store: &'a mut MarfStore,
    bitcoin_context: &'a dyn BurnStateDB,
) -> ClarityDatabase<'a> {
    ClarityDatabase::new(store, &NULL_HEADER_DB, bitcoin_context)
}

fn setup_block_metadata_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn BurnStateDB,
    timestamp: u64,
) -> Result<(), VmExecutionError> {
    let database = clarity_database(store, bitcoin_context);
    let mut context = GlobalContext::new(
        false,
        CHAIN_ID_TESTNET,
        database,
        LimitedCostTracker::new_free(),
        StacksEpochId::Epoch40,
    );
    context.execute(|global| global.database.setup_block_metadata(Some(timestamp)))
}

fn tenure_height_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn BurnStateDB,
) -> Result<u32, VmExecutionError> {
    let database = clarity_database(store, bitcoin_context);
    let mut context = GlobalContext::new(
        false,
        CHAIN_ID_TESTNET,
        database,
        LimitedCostTracker::new_free(),
        StacksEpochId::Epoch40,
    );
    context.execute(|global| global.database.get_tenure_height())
}

fn set_tenure_height_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn BurnStateDB,
    height: u32,
) -> Result<(), VmExecutionError> {
    let database = clarity_database(store, bitcoin_context);
    let mut context = GlobalContext::new(
        false,
        CHAIN_ID_TESTNET,
        database,
        LimitedCostTracker::new_free(),
        StacksEpochId::Epoch40,
    );
    context.execute(|global| global.database.set_tenure_height(height))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clarity::vm::Value;
    use clarity::vm::database::ClarityBackingStore;
    use clarity::vm::database::clarity_store::make_contract_hash_key;
    use clarity::vm::types::QualifiedContractIdentifier;
    use nano_primitives::TrieHash;
    use stacks_common::codec::StacksMessageCodec;
    use stacks_common::types::chainstate::StacksBlockId;

    use super::{MarfStore, Vm, evaluate, evaluate_in_store};
    use clarity::vm::costs::LimitedCostTracker;

    #[test]
    fn evaluates_clarity_six_programs() {
        let value = evaluate("(+ u20 u22)").expect("Clarity 6 program should evaluate");

        assert_eq!(value, Some(Value::UInt(42)));
    }

    #[test]
    fn supports_epoch_four_clarity_six_words() {
        let concatenated =
            evaluate("(concat 0x01 0x02 0x03)").expect("variadic concat should evaluate");
        let parsed_bitcoin = evaluate("(get-bitcoin-tx-output? 0x00 u0)")
            .expect("bitcoin transaction parser should evaluate");

        assert_eq!(
            concatenated,
            Some(Value::buff_from(vec![1, 2, 3]).expect("valid buffer"))
        );
        assert_eq!(parsed_bitcoin, Some(Value::err_uint(1)));
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

        store
            .set_block_hash(StacksBlockId(first))
            .expect("select first state");
        assert_eq!(
            store.get_data("counter").expect("read first state"),
            Some("one".to_owned())
        );
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

    #[test]
    fn vm_executes_and_seals_a_block_state() {
        let block = [9; 32];
        let mut vm = Vm::new().expect("create VM");
        vm.begin_block(None, block).expect("begin block");

        let evaluation = vm
            .execute(
                "(define-data-var counter uint u1) (var-set counter u2) (var-get counter)",
                LimitedCostTracker::new_free(),
            )
            .expect("execute block");
        let root = vm.seal_block().expect("seal block");

        assert_eq!(evaluation.value, Some(Value::UInt(2)));
        assert_eq!(vm.root(block), Some(root));
    }

    #[test]
    fn deploys_and_calls_a_contract_with_encoded_arguments() {
        let block = [9; 32];
        let contract = QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.counter")
            .expect("valid contract identifier");
        let sender = contract.issuer.clone().into();
        let mut argument = Vec::new();
        Value::UInt(41)
            .consensus_serialize(&mut argument)
            .expect("serialize Clarity value");
        let mut vm = Vm::new().expect("create VM");
        vm.begin_block(None, block).expect("begin block");
        vm.deploy_contract(
            contract.clone(),
            clarity::vm::ClarityVersion::Clarity6,
            "(define-public (increment (value uint)) (ok (+ value u1)))",
            LimitedCostTracker::new_free(),
        )
        .expect("deploy contract");

        let result = vm
            .execute_contract_call(
                sender,
                None,
                contract,
                "increment",
                &[argument],
                LimitedCostTracker::new_free(),
            )
            .expect("call contract");

        assert_eq!(
            result.value,
            Some(Value::okay(Value::UInt(42)).expect("valid response"))
        );
        vm.seal_block().expect("seal block");
    }

    #[test]
    fn loads_clarity_values_and_metadata_from_a_checkpoint() {
        let source = [
            0x73, 0xd5, 0x36, 0xfd, 0x05, 0x5e, 0x08, 0x3f, 0x60, 0xbe, 0x70, 0x35, 0x0e, 0x72,
            0x9d, 0x99, 0xcc, 0xea, 0xc3, 0x47, 0xc5, 0xbf, 0xaa, 0xa7, 0x9f, 0xd4, 0x62, 0xd1,
            0xb8, 0x21, 0x53, 0xf3,
        ];
        let root = TrieHash::from_bytes([
            0x8f, 0xdf, 0xf0, 0x9f, 0xd8, 0x7a, 0xe7, 0x9f, 0x97, 0x0a, 0x23, 0x36, 0x27, 0x01,
            0x3f, 0x09, 0x47, 0x8e, 0xe1, 0x71, 0x53, 0x79, 0xa7, 0x34, 0x42, 0x58, 0x4b, 0xb4,
            0x3a, 0x64, 0xc0, 0x71,
        ]);
        let checkpoint = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../nano-conformance/fixtures/chainstate/checkpoint-H/marf.sqlite");
        let contract = QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.pox")
            .expect("valid boot contract identifier");
        let mut store =
            MarfStore::from_checkpoint(checkpoint, source, root).expect("load checkpoint");

        assert_eq!(
            store.root(source).map(|root| root.0),
            Some(*root.as_bytes())
        );
        assert!(
            store
                .get_data(&make_contract_hash_key(&contract))
                .expect("read contract commitment")
                .is_some()
        );
        assert!(
            store
                .get_metadata(&contract, "vm-metadata::9::contract-src")
                .expect("read contract source")
                .is_some()
        );

        store
            .begin(Some(source), [0x42; 32])
            .expect("extend checkpoint state");
        assert!(
            store
                .get_data(&make_contract_hash_key(&contract))
                .expect("read inherited contract commitment")
                .is_some()
        );
        store
            .put("nano-checkpoint-extension".to_owned(), "value".to_owned())
            .expect("write extension");
        store.seal().expect("seal checkpoint extension");
    }
}
