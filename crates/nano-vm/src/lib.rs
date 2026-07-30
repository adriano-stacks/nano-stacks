#![forbid(unsafe_code)]

use std::{collections::BTreeMap, path::Path};

use clar2wasm::{CompiledContract, ModuleCache};
use clarity::vm::analysis::{AnalysisDatabase, StaticCheckError, StaticCheckErrorKind};
use clarity::vm::ast::build_ast;
use clarity::vm::contexts::{
    AssetMap, CallStack, ContractContext, GlobalContext, OwnedEnvironment,
};
use clarity::vm::costs::{CostErrors, CostTracker, ExecutionCost, LimitedCostTracker};
use clarity::vm::database::clarity_store::{
    ContractCommitment, SpecialCaseHandler, make_contract_hash_key,
};
use clarity::vm::database::{
    BurnStateDB, ClarityBackingStore, ClarityDatabase, ClarityDeserializable, HeadersDB,
    MemoryBackingStore,
};
use clarity::vm::errors::{ClarityEvalError, RuntimeError, VmExecutionError, VmInternalError};
use clarity::vm::events::StacksTransactionEvent;
use clarity::vm::representations::SymbolicExpression;
use clarity::vm::types::{BuffData, PrincipalData, QualifiedContractIdentifier, SequenceData};
use clarity::vm::{ClarityVersion, Value, eval_all};
use nano_marf::{
    CheckpointError, MarfError, MarfValue, StateRoot, TriePointer, VersionedMarf, import_checkpoint,
};
use nano_primitives::{Network, TrieHash};
use rusqlite::{OptionalExtension, params};
use stacks_common::types::{
    StacksEpoch, StacksEpochId,
    chainstate::{
        BlockHeaderHash, BurnchainHeaderHash, ConsensusHash, SortitionId, StacksAddress,
        StacksBlockId, TrieHash as ReferenceTrieHash, VRFSeed,
    },
};
use stacks_common::util::hash::Hash160;
use stacks_common::util::hash::Sha512Trunc256Sum;

/// The consensus execution-cost limit for an Epoch 4 block.
///
/// Epoch 4 doubles what a block may read and leaves writing and runtime where
/// Epoch 3 left them (`core/mod.rs`, `BLOCK_LIMIT_MAINNET_40`).
pub const EPOCH_4_BLOCK_LIMIT: ExecutionCost = ExecutionCost {
    write_length: 15_000_000,
    write_count: 15_000,
    read_length: 200_000_000,
    read_count: 30_000,
    runtime: 5_000_000_000,
};

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
    pub assets: AssetMap,
    pub events: Vec<StacksTransactionEvent>,
}

/// The outcome of invoking a published contract.
#[derive(Debug)]
pub enum ContractCallOutcome {
    Success(Box<TransactionResult>),
    /// The call returned an error response, so its writes were discarded.
    AbortedByResponse(Box<TransactionResult>),
    RuntimeFailure {
        cost: ExecutionCost,
        error: VmExecutionError,
    },
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
    pub pox_5_activation_height: u32,
    /// Coinbase a sortition at this height collects beyond its own emission,
    /// mirroring a snapshot's `accumulated_coinbase_ustx`.
    pub accumulated_coinbase: u128,
    /// The burn block this tenure won, as Clarity reads it back.
    pub burn_header_hash: [u8; 32],
    pub burn_block_time: u64,
    /// The seed the winning commitment carried.
    pub vrf_seed: [u8; 32],
    /// Bitcoin every miner spent on this sortition, and the winner's share.
    pub burn_spend_total: u128,
    pub burn_spend_winner: u128,
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
            pox_5_activation_height: u32::MAX,
            accumulated_coinbase: 0,
            burn_header_hash: [0; 32],
            burn_block_time: 0,
            vrf_seed: [0; 32],
            burn_spend_total: 0,
            burn_spend_winner: 0,
        }
    }
}

/// What Clarity may read about a block nano has already executed.
///
/// These are the fields behind `get-stacks-block-info?` and `get-tenure-info?`.
/// A follower that answers them from nowhere returns `none` where the network
/// returns a value, so every contract that consults chain history diverges.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BlockHeader {
    pub burn_header_hash: [u8; 32],
    pub burn_block_height: u32,
    pub burn_block_time: u64,
    pub stacks_block_time: u64,
    pub block_header_hash: [u8; 32],
    pub consensus_hash: [u8; 20],
    pub vrf_seed: [u8; 32],
    /// The miner's address as its version byte and `Hash160`.
    pub miner_address: (u8, [u8; 20]),
    /// Bitcoin every miner spent on the sortition this block's tenure won.
    pub burn_spend_total: u128,
    /// Bitcoin the winning miner alone spent on it.
    pub burn_spend_winner: u128,
    /// STX the tenure earned, once its rewards matured.
    pub block_reward: u128,
    /// The tenure this block belongs to, counted in tenures.
    pub tenure_height: u32,
    /// The Stacks height that tenure's first block sits at.
    pub tenure_start_height: u32,
}

/// Everything outside the MARF that Clarity may read.
///
/// The burn state and the block headers travel together because Clarity reads
/// them through one database, and keeping them in one value spares every
/// evaluation path a second parameter.
pub trait ChainContext: BurnStateDB + HeadersDB {}

impl<T: BurnStateDB + HeadersDB> ChainContext for T {}

/// Bitcoin state and executed headers available while evaluating.
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
    pox_5_activation_height: u32,
    headers: BTreeMap<[u8; 32], BlockHeader>,
    /// Stacks height each tenure started at, for the tenure-height mapping.
    tenure_starts: BTreeMap<u32, u32>,
}

/// A context that knows no chain, for evaluating programs that read none.
static NULL_CONTEXT: BitcoinContext = BitcoinContext {
    height: 0,
    first_height: 0,
    prepare_phase_length: 0,
    reward_phase_length: 0,
    rejection_fraction: 0,
    v1_unlock_height: 0,
    v2_unlock_height: 0,
    v3_unlock_height: 0,
    pox_5_activation_height: 0,
    headers: BTreeMap::new(),
    tenure_starts: BTreeMap::new(),
};

impl BitcoinContext {
    /// Point the context at the block about to execute, keeping the headers of
    /// the blocks already executed.
    fn set_block(&mut self, context: BitcoinBlockContext) -> Result<(), MarfStoreError> {
        self.height = u32::try_from(context.height)
            .map_err(|_| MarfStoreError::BitcoinHeightOverflow(context.height))?;
        self.first_height = u32::try_from(context.first_height)
            .map_err(|_| MarfStoreError::BitcoinHeightOverflow(context.first_height))?;
        self.prepare_phase_length = context.prepare_phase_length;
        self.reward_phase_length = context.reward_phase_length;
        self.rejection_fraction = context.rejection_fraction;
        self.v1_unlock_height = context.v1_unlock_height;
        self.v2_unlock_height = context.v2_unlock_height;
        self.v3_unlock_height = context.v3_unlock_height;
        self.pox_5_activation_height = context.pox_5_activation_height;
        Ok(())
    }

    fn header(&self, id: &StacksBlockId) -> Option<&BlockHeader> {
        self.headers.get(id.as_bytes())
    }
}

impl HeadersDB for BitcoinContext {
    fn get_stacks_block_header_hash_for_block(
        &self,
        id_bhh: &StacksBlockId,
        _epoch: &StacksEpochId,
    ) -> Option<BlockHeaderHash> {
        self.header(id_bhh)
            .map(|header| BlockHeaderHash(header.block_header_hash))
    }

    fn get_burn_header_hash_for_block(
        &self,
        id_bhh: &StacksBlockId,
    ) -> Option<BurnchainHeaderHash> {
        self.header(id_bhh)
            .map(|header| BurnchainHeaderHash(header.burn_header_hash))
    }

    fn get_consensus_hash_for_block(
        &self,
        id_bhh: &StacksBlockId,
        _epoch: &StacksEpochId,
    ) -> Option<ConsensusHash> {
        self.header(id_bhh)
            .map(|header| ConsensusHash(header.consensus_hash))
    }

    fn get_vrf_seed_for_block(
        &self,
        id_bhh: &StacksBlockId,
        _tip: &StacksBlockId,
        _epoch: &StacksEpochId,
    ) -> Option<VRFSeed> {
        self.header(id_bhh).map(|header| VRFSeed(header.vrf_seed))
    }

    fn get_stacks_block_time_for_block(&self, id_bhh: &StacksBlockId) -> Option<u64> {
        self.header(id_bhh).map(|header| header.stacks_block_time)
    }

    fn get_burn_block_time_for_block(
        &self,
        id_bhh: &StacksBlockId,
        _epoch: Option<&StacksEpochId>,
    ) -> Option<u64> {
        self.header(id_bhh).map(|header| header.burn_block_time)
    }

    fn get_burn_block_height_for_block(&self, id_bhh: &StacksBlockId) -> Option<u32> {
        self.header(id_bhh).map(|header| header.burn_block_height)
    }

    fn get_miner_address(
        &self,
        id_bhh: &StacksBlockId,
        _tip: &StacksBlockId,
        _epoch: &StacksEpochId,
    ) -> Option<StacksAddress> {
        self.header(id_bhh).and_then(|header| {
            StacksAddress::new(header.miner_address.0, Hash160(header.miner_address.1)).ok()
        })
    }

    fn get_burnchain_tokens_spent_for_block(
        &self,
        id_bhh: &StacksBlockId,
        _tip: &StacksBlockId,
        _epoch: &StacksEpochId,
    ) -> Option<u128> {
        self.header(id_bhh).map(|header| header.burn_spend_total)
    }

    fn get_burnchain_tokens_spent_for_winning_block(
        &self,
        id_bhh: &StacksBlockId,
        _tip: &StacksBlockId,
        _epoch: &StacksEpochId,
    ) -> Option<u128> {
        self.header(id_bhh).map(|header| header.burn_spend_winner)
    }

    fn get_tokens_earned_for_block(
        &self,
        id_bhh: &StacksBlockId,
        _tip: &StacksBlockId,
        _epoch: &StacksEpochId,
    ) -> Option<u128> {
        self.header(id_bhh).map(|header| header.block_reward)
    }

    fn get_stacks_height_for_tenure_height(
        &self,
        _tip: &StacksBlockId,
        tenure_height: u32,
    ) -> Option<u32> {
        self.tenure_starts.get(&tenure_height).copied()
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
        self.pox_5_activation_height
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
            block_limit: EPOCH_4_BLOCK_LIMIT,
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
    modules: ModuleCache,
}

impl Vm {
    /// Create an empty VM for the supplied network.
    pub fn new(network: Network) -> Result<Self, MarfStoreError> {
        Ok(Self {
            store: MarfStore::new(network)?,
            context: BitcoinContext::default(),
            modules: ModuleCache::default(),
        })
    }

    /// Open a VM at a checkpointed Clarity state.
    pub fn from_checkpoint(
        network: Network,
        path: impl AsRef<Path>,
        source: [u8; 32],
        expected_root: TrieHash,
    ) -> Result<Self, MarfStoreError> {
        Ok(Self {
            store: MarfStore::from_checkpoint(network, path, source, expected_root)?,
            context: BitcoinContext::default(),
            modules: ModuleCache::default(),
        })
    }

    /// The chain this VM executes against.
    #[must_use]
    pub const fn network(&self) -> Network {
        self.store.network()
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
        self.context.set_block(bitcoin_context)?;
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

    /// Record what Clarity may later read about a block nano has executed.
    pub fn record_block_header(&mut self, block: [u8; 32], header: BlockHeader) {
        self.context
            .tenure_starts
            .entry(header.tenure_height)
            .or_insert(header.tenure_start_height);
        self.context.headers.insert(block, header);
    }

    /// Record the Stacks height of an imported checkpoint when it is not stored in the MARF.
    pub fn set_checkpoint_height(&mut self, block: [u8; 32], height: u32) {
        self.store.set_checkpoint_height(block, height);
    }

    /// Execute a Clarity 6 program with the supplied consensus cost tracker.
    pub fn execute(
        &mut self,
        source: &str,
        cost_tracker: LimitedCostTracker,
    ) -> Result<Evaluation, ClarityEvalError> {
        let Self { store, context, .. } = self;
        evaluate_with_tracker_in_context(store, context, source, cost_tracker)
    }

    /// Store the timestamp supplied by the current Nakamoto block header.
    pub fn setup_block_metadata(&mut self, timestamp: u64) -> Result<(), VmExecutionError> {
        let Self { store, context, .. } = self;
        setup_block_metadata_in_context(store, context, timestamp)
    }

    /// Read the tenure height stored in the active Clarity state.
    pub fn tenure_height(&mut self) -> Result<u32, VmExecutionError> {
        let Self { store, context, .. } = self;
        tenure_height_in_context(store, context)
    }

    /// Store the tenure height for a newly started tenure.
    pub fn set_tenure_height(&mut self, height: u32) -> Result<(), VmExecutionError> {
        let Self { store, context, .. } = self;
        set_tenure_height_in_context(store, context, height)
    }

    /// Credit STX scheduled to unlock at the current Stacks block height.
    pub fn process_scheduled_unlocks(&mut self) -> Result<u128, VmExecutionError> {
        let Self { store, context, .. } = self;
        process_scheduled_unlocks_in_context(store, context)
    }

    /// Increase the liquid STX supply by a block-finalization amount.
    pub fn increment_liquid_stx_supply(&mut self, amount: u128) -> Result<(), VmExecutionError> {
        let Self { store, context, .. } = self;
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
        let Self {
            store,
            context,
            modules,
        } = self;
        deploy_contract_with_wasm_in_context(
            store,
            context,
            modules,
            contract,
            version,
            source,
            cost_tracker,
        )
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
        match self.execute_contract_call_outcome(
            sender,
            sponsor,
            contract,
            function,
            arguments,
            cost_tracker,
        )? {
            ContractCallOutcome::Success(result)
            | ContractCallOutcome::AbortedByResponse(result) => Ok(*result),
            ContractCallOutcome::RuntimeFailure { error, .. } => Err(error),
        }
    }

    /// Invoke a contract function, including a private one, as the given sender.
    ///
    /// The node itself calls private boot-contract functions when it maintains
    /// consensus state that no transaction owns.
    pub fn call_contract_values(
        &mut self,
        sender: PrincipalData,
        contract: &QualifiedContractIdentifier,
        function: &str,
        arguments: &[Value],
    ) -> Result<Value, VmExecutionError> {
        let Self {
            store,
            context,
            modules,
        } = self;
        match call_contract_values_in_context(
            store,
            context,
            modules,
            sender,
            contract,
            function,
            arguments,
            LimitedCostTracker::new_free(),
        )? {
            ContractCallOutcome::Success(result)
            | ContractCallOutcome::AbortedByResponse(result) => result.value.ok_or_else(|| {
                VmInternalError::Expect("contract call returned no value".to_owned()).into()
            }),
            ContractCallOutcome::RuntimeFailure { error, .. } => Err(error),
        }
    }

    /// Invoke a contract and retain acceptable runtime failures as transaction outcomes.
    pub fn execute_contract_call_outcome(
        &mut self,
        sender: PrincipalData,
        sponsor: Option<PrincipalData>,
        contract: QualifiedContractIdentifier,
        function: &str,
        arguments: &[Vec<u8>],
        cost_tracker: LimitedCostTracker,
    ) -> Result<ContractCallOutcome, VmExecutionError> {
        let Self {
            store,
            context,
            modules,
        } = self;
        execute_contract_call_outcome_with_wasm_in_context(
            store,
            context,
            modules,
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
        let Self { store, context, .. } = self;
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
        let Self { store, context, .. } = self;
        account_nonce_in_context(store, context, principal)
    }

    /// Read an account's spendable STX.
    pub fn account_balance(
        &mut self,
        principal: &PrincipalData,
    ) -> Result<u128, VmExecutionError> {
        let Self { store, context, .. } = self;
        account_balance_in_context(store, context, principal)
    }

    /// Debit a transaction fee from an account's available STX balance.
    pub fn debit_fee(&mut self, payer: &PrincipalData, fee: u64) -> Result<(), VmExecutionError> {
        let Self { store, context, .. } = self;
        debit_fee_in_context(store, context, payer, fee)
    }

    /// Persist an account balance without changing it.
    pub fn touch_stx_balance(&mut self, principal: &PrincipalData) -> Result<(), VmExecutionError> {
        let Self { store, context, .. } = self;
        touch_stx_balance_in_context(store, context, principal)
    }

    /// Credit liquid STX without emitting a transaction event.
    pub fn credit_stx(
        &mut self,
        principal: &PrincipalData,
        amount: u128,
    ) -> Result<(), VmExecutionError> {
        let Self { store, context, .. } = self;
        credit_stx_in_context(store, context, principal, amount)
    }

    /// Create a consensus cost tracker from the active chain state.
    ///
    /// Empty development states do not have the boot cost contracts yet and use a free tracker.
    pub fn transaction_cost_tracker(&mut self) -> Result<LimitedCostTracker, VmExecutionError> {
        self.transaction_cost_tracker_with_total(ExecutionCost::ZERO)
    }

    /// Create a consensus cost tracker carrying costs already consumed by this block.
    pub fn transaction_cost_tracker_with_total(
        &mut self,
        total: ExecutionCost,
    ) -> Result<LimitedCostTracker, VmExecutionError> {
        let Self { store, context, .. } = self;
        transaction_cost_tracker_in_context(store, context, total)
    }

    /// Store a transaction nonce in the active block state.
    pub fn set_account_nonce(
        &mut self,
        principal: &PrincipalData,
        nonce: u64,
    ) -> Result<(), VmExecutionError> {
        let Self { store, context, .. } = self;
        set_account_nonce_in_context(store, context, principal, nonce)
    }

    /// Seal the active block state.
    pub fn seal_block(&mut self) -> Result<StateRoot, MarfStoreError> {
        self.store.seal()
    }

    /// Return the root that would be committed by sealing the active block.
    pub fn pending_state_root(&self) -> Result<StateRoot, MarfStoreError> {
        Ok(StateRoot(*self.store.pending_root()?.as_bytes()))
    }

    /// Seal the active state and store it under the committed block ID.
    pub fn seal_block_to(&mut self, block: [u8; 32]) -> Result<StateRoot, MarfStoreError> {
        self.store.seal_to(block)
    }

    /// Discard an unsealed block state after execution fails.
    pub fn abort_block(&mut self) -> Result<(), MarfStoreError> {
        self.store.abort()
    }

    /// Begin an atomic transaction within the active block state.
    pub fn begin_transaction(&mut self) -> Result<(), MarfStoreError> {
        self.store.begin_transaction()
    }

    /// Commit the active transaction's writes to the block state.
    pub fn commit_transaction(&mut self) -> Result<(), MarfStoreError> {
        self.store.commit_transaction()
    }

    /// Discard the active transaction's writes while keeping the block active.
    pub fn rollback_transaction(&mut self) -> Result<(), MarfStoreError> {
        self.store.rollback_transaction()
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

    /// Return the pointers and child hashes stored under a path prefix.
    #[must_use]
    pub fn pointers_at(
        &self,
        block: [u8; 32],
        prefix: &[u8],
    ) -> Option<Vec<(TriePointer, nano_primitives::TrieHash)>> {
        self.store.pointers_at(block, prefix)
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
    network: Network,
    marf: VersionedMarf,
    side_store: rusqlite::Connection,
    states: BTreeMap<[u8; 32], StoreState>,
    parents: BTreeMap<[u8; 32], Option<[u8; 32]>>,
    heights: BTreeMap<[u8; 32], u32>,
    read_block: Option<[u8; 32]>,
    active: Option<ActiveStore>,
    transaction: Option<StoreTransaction>,
}

#[derive(Clone, Debug)]
struct ActiveStore {
    block: [u8; 32],
    parent: Option<[u8; 32]>,
    height: u32,
    state: StoreState,
}

#[derive(Clone, Debug)]
struct StoreTransaction {
    marf: VersionedMarf,
    read_block: Option<[u8; 32]>,
    active: Option<ActiveStore>,
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
    TransactionInProgress,
    NoTransaction,
    BitcoinHeightOverflow(u64),
}

impl std::fmt::Display for MarfStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Marf(error) => write!(formatter, "MARF error: {error}"),
            Self::Checkpoint(error) => write!(formatter, "checkpoint error: {error}"),
            Self::Sql(error) => write!(formatter, "SQLite error: {error}"),
            Self::NoActiveState => formatter.write_str("no active VM state"),
            Self::TransactionInProgress => formatter.write_str("VM transaction is already active"),
            Self::NoTransaction => formatter.write_str("no active VM transaction"),
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
    /// Create an empty versioned store for the supplied network.
    pub fn new(network: Network) -> Result<Self, MarfStoreError> {
        Ok(Self {
            network,
            marf: VersionedMarf::default(),
            side_store: create_side_store()?,
            states: BTreeMap::new(),
            parents: BTreeMap::new(),
            heights: BTreeMap::new(),
            read_block: None,
            active: None,
            transaction: None,
        })
    }

    /// Load a checkpointed Clarity MARF and its corresponding `SQLite` side tables.
    ///
    /// A checkpoint belongs to exactly one chain, so the network it is executed
    /// as is fixed when it is opened.
    pub fn from_checkpoint(
        network: Network,
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
            network,
            marf,
            side_store,
            states,
            parents: BTreeMap::new(),
            heights: BTreeMap::new(),
            read_block: Some(source),
            active: None,
            transaction: None,
        })
    }

    /// The chain this state belongs to.
    #[must_use]
    pub const fn network(&self) -> Network {
        self.network
    }

    /// Create a Clarity database backed by this store.
    pub fn as_clarity_db(&mut self) -> ClarityDatabase<'_> {
        ClarityDatabase::new(self, &NULL_CONTEXT, &NULL_CONTEXT)
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

    /// Start an atomic transaction within the active block state.
    pub fn begin_transaction(&mut self) -> Result<(), MarfStoreError> {
        if self.transaction.is_some() {
            return Err(MarfStoreError::TransactionInProgress);
        }
        self.transaction = Some(StoreTransaction {
            marf: self.marf.clone(),
            read_block: self.read_block,
            active: self.active.clone(),
        });
        Ok(())
    }

    /// Commit the active transaction's writes to the current block state.
    pub fn commit_transaction(&mut self) -> Result<(), MarfStoreError> {
        self.transaction
            .take()
            .map(|_| ())
            .ok_or(MarfStoreError::NoTransaction)
    }

    /// Restore the active block state to its state before the transaction began.
    pub fn rollback_transaction(&mut self) -> Result<(), MarfStoreError> {
        let transaction = self
            .transaction
            .take()
            .ok_or(MarfStoreError::NoTransaction)?;
        self.marf = transaction.marf;
        self.read_block = transaction.read_block;
        self.active = transaction.active;
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

    /// Return the active state's root without sealing it.
    pub fn pending_root(&self) -> Result<TrieHash, MarfStoreError> {
        self.marf.pending_root().map_err(MarfStoreError::from)
    }

    /// Seal the active state and register it under its committed block ID.
    pub fn seal_to(&mut self, block: [u8; 32]) -> Result<StateRoot, MarfStoreError> {
        if self.transaction.is_some() {
            return Err(MarfStoreError::TransactionInProgress);
        }
        if self.active.is_none() {
            return Err(MarfStoreError::NoActiveState);
        }
        // Seal the MARF first: a failure here must leave the active state intact
        // so the caller can still abort it.
        let root = self.marf.seal_to(block)?;
        let active = self.active.take().ok_or(MarfStoreError::NoActiveState)?;
        self.parents.insert(block, active.parent);
        self.heights.insert(block, active.height);
        self.states.insert(block, active.state);
        self.read_block = Some(block);
        Ok(StateRoot(*root.as_bytes()))
    }

    /// Discard the active state without registering a new MARF version.
    pub fn abort(&mut self) -> Result<(), MarfStoreError> {
        let active = self.active.take().ok_or(MarfStoreError::NoActiveState)?;
        self.marf.abort()?;
        self.read_block = active.parent;
        Ok(())
    }

    /// Return a sealed state's MARF root.
    #[must_use]
    pub fn root(&self, block: [u8; 32]) -> Option<StateRoot> {
        self.marf
            .root(block)
            .map(|root: TrieHash| StateRoot(*root.as_bytes()))
    }

    /// Record an imported checkpoint's Stacks height for Clarity balance history lookups.
    pub fn set_checkpoint_height(&mut self, block: [u8; 32], height: u32) {
        if self.marf.height(block).is_none() {
            self.heights.entry(block).or_insert(height);
        }
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

    /// Return the pointers and child hashes stored under a path prefix.
    #[must_use]
    pub fn pointers_at(
        &self,
        block: [u8; 32],
        prefix: &[u8],
    ) -> Option<Vec<(TriePointer, nano_primitives::TrieHash)>> {
        self.marf.pointers_at(block, prefix)
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
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.block == block && active.height == height)
        {
            return Some(block);
        }
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

    fn get_cc_special_cases_handler(&self) -> Option<SpecialCaseHandler> {
        Some(&pox_locking::handle_contract_call_special_cases)
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

/// Evaluate a Clarity 6 program under the consensus Epoch 4.0 rules against an
/// ephemeral store, for programs that read and write nothing.
pub fn evaluate(network: Network, source: &str) -> Result<Option<Value>, ClarityEvalError> {
    let contract_id = QualifiedContractIdentifier::transient();
    let mut backing_store = MemoryBackingStore::new();
    let database = backing_store.as_clarity_db();
    let mut context = GlobalContext::new(
        network.is_mainnet(),
        network.chain_id(),
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
    evaluate_with_tracker_in_context(store, &NULL_CONTEXT, source, cost_tracker)
}

fn evaluate_with_tracker_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    source: &str,
    cost_tracker: LimitedCostTracker,
) -> Result<Evaluation, ClarityEvalError> {
    let network = store.network();
    let contract_id = QualifiedContractIdentifier::transient();
    let database = clarity_database(store, bitcoin_context);
    let mut context = GlobalContext::new(
        network.is_mainnet(),
        network.chain_id(),
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
        &NULL_CONTEXT,
        contract,
        version,
        source,
        cost_tracker,
    )
}

fn deploy_contract_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    contract: QualifiedContractIdentifier,
    version: ClarityVersion,
    source: &str,
    cost_tracker: LimitedCostTracker,
) -> Result<TransactionResult, ClarityEvalError> {
    let network = store.network();
    let database = clarity_database(store, bitcoin_context);
    let mut environment = OwnedEnvironment::new_cost_limited(
        network.is_mainnet(),
        network.chain_id(),
        database,
        cost_tracker,
        StacksEpochId::Epoch40,
    );
    let ((), assets, events) =
        environment.initialize_versioned_contract(contract, version, source, None)?;

    Ok(TransactionResult {
        value: None,
        cost: environment.get_cost_total(),
        assets,
        events,
    })
}

fn deploy_contract_with_wasm_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    modules: &mut ModuleCache,
    contract: QualifiedContractIdentifier,
    version: ClarityVersion,
    source: &str,
    cost_tracker: LimitedCostTracker,
) -> Result<TransactionResult, ClarityEvalError> {
    let network = store.network();
    let compiled = {
        let mut analysis = AnalysisDatabase::new(store);
        let compiled: CompiledContract = analysis
            .execute::<_, _, StaticCheckError>(|analysis_db| {
                let compiled = clar2wasm::compile(
                    source,
                    &contract,
                    LimitedCostTracker::new_free(),
                    version,
                    StacksEpochId::Epoch40,
                    analysis_db,
                    true,
                )
                .map_err(|error: clar2wasm::CompileError| {
                    StaticCheckErrorKind::Unreachable(wasm_compile_error(error))
                })?;
                analysis_db.insert_contract(&contract, &compiled.contract_analysis)?;
                Ok(compiled.into_compiled_contract())
            })
            .map_err(|error: StaticCheckError| {
                ClarityEvalError::from(VmExecutionError::Internal(VmInternalError::Expect(
                    error.to_string(),
                )))
            })?;
        compiled
    };

    let database = clarity_database(store, bitcoin_context);
    let mut global = GlobalContext::new(
        network.is_mainnet(),
        network.chain_id(),
        database,
        cost_tracker,
        StacksEpochId::Epoch40,
    );
    global.begin();
    global.database.insert_contract_hash(&contract, source)?;
    let mut contract_context = ContractContext::new(contract.clone(), version);
    let initialized = clar2wasm::initialize::initialize_contract(
        &mut global,
        &mut contract_context,
        None,
        &compiled.analysis,
        &compiled.wasm,
        modules,
    )?;
    let data_size = contract_context.data_size;
    global
        .database
        .insert_contract(&contract, contract_context.into())?;
    global
        .database
        .set_contract_data_size(&contract, data_size)?;
    let (assets, events) = global.commit()?;
    global
        .cost_track
        .add_cost(initialized.cost.into())
        .map_err(VmExecutionError::from)?;
    modules.insert(contract, compiled);

    Ok(TransactionResult {
        value: initialized.ret,
        cost: global.cost_track.get_total(),
        assets: assets.unwrap_or_default(),
        events: events.map_or_else(Vec::new, |batch| batch.events),
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
    match execute_contract_call_outcome(
        store,
        sender,
        sponsor,
        contract,
        function,
        arguments,
        cost_tracker,
    )? {
        ContractCallOutcome::Success(result) | ContractCallOutcome::AbortedByResponse(result) => {
            Ok(*result)
        }
        ContractCallOutcome::RuntimeFailure { error, .. } => Err(error),
    }
}

/// Call a contract while retaining acceptable runtime failures and their costs.
pub fn execute_contract_call_outcome(
    store: &mut MarfStore,
    sender: PrincipalData,
    sponsor: Option<PrincipalData>,
    contract: QualifiedContractIdentifier,
    function: &str,
    arguments: &[Vec<u8>],
    cost_tracker: LimitedCostTracker,
) -> Result<ContractCallOutcome, VmExecutionError> {
    execute_contract_call_outcome_in_context(
        store,
        &NULL_CONTEXT,
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

fn execute_contract_call_outcome_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    call: ContractCall<'_>,
    cost_tracker: LimitedCostTracker,
) -> Result<ContractCallOutcome, VmExecutionError> {
    let network = store.network();
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
    let mut database = clarity_database(store, bitcoin_context);
    database.begin();
    let mut environment = OwnedEnvironment::new_cost_limited(
        network.is_mainnet(),
        network.chain_id(),
        database,
        cost_tracker,
        StacksEpochId::Epoch40,
    );
    let result = environment.execute_transaction(
        call.sender,
        call.sponsor,
        call.contract,
        call.function,
        &arguments,
    );
    let (mut database, cost_tracker) = environment.destruct().ok_or_else(|| {
        VmExecutionError::Internal(VmInternalError::Expect(
            "contract execution left the database in an invalid state".to_owned(),
        ))
    })?;
    match result {
        Ok((value, assets, events)) => {
            database.commit()?;
            Ok(ContractCallOutcome::Success(Box::new(TransactionResult {
                value: Some(value),
                cost: cost_tracker.get_total(),
                assets,
                events,
            })))
        }
        Err(error) => {
            database.roll_back()?;
            if is_acceptable_runtime_failure(&error) {
                Ok(ContractCallOutcome::RuntimeFailure {
                    cost: cost_tracker.get_total(),
                    error,
                })
            } else {
                Err(error)
            }
        }
    }
}

fn execute_contract_call_outcome_with_wasm_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    modules: &mut ModuleCache,
    call: ContractCall<'_>,
    cost_tracker: LimitedCostTracker,
) -> Result<ContractCallOutcome, VmExecutionError> {
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
            Ok(value)
        })
        .collect::<Result<Vec<_>, VmExecutionError>>()?;
    call_contract_values_in_context(
        store,
        bitcoin_context,
        modules,
        call.sender,
        &call.contract,
        call.function,
        &arguments,
        cost_tracker,
    )
}

#[allow(clippy::too_many_arguments)]
fn call_contract_values_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    modules: &mut ModuleCache,
    sender: PrincipalData,
    contract: &QualifiedContractIdentifier,
    function: &str,
    arguments: &[Value],
    cost_tracker: LimitedCostTracker,
) -> Result<ContractCallOutcome, VmExecutionError> {
    let network = store.network();
    for argument in arguments.iter().filter_map(contract_argument) {
        ensure_wasm_module(store, bitcoin_context, modules, argument)?;
    }
    ensure_wasm_module(store, bitcoin_context, modules, contract)?;
    let module = modules
        .get(contract)
        .ok_or_else(|| VmInternalError::Expect(format!("missing WASM module for {contract}")))?;
    let database = clarity_database(store, bitcoin_context);
    let mut global = GlobalContext::new(
        network.is_mainnet(),
        network.chain_id(),
        database,
        cost_tracker,
        StacksEpochId::Epoch40,
    );
    global.begin();
    let contract_context = global.database.get_contract(contract)?;
    let mut call_stack = CallStack::new();
    let result = clar2wasm::initialize::call_function(
        function,
        arguments,
        module,
        &mut global,
        &contract_context,
        &mut call_stack,
        Some(sender),
        None,
        None,
        modules,
    );
    match result {
        Ok(value) => {
            // A call that returns an error response keeps its cost and its
            // value, but none of the state it wrote.
            let aborted = matches!(&value, Value::Response(response) if !response.committed);
            let (assets, events) = if aborted {
                global.roll_back()?;
                (None, None)
            } else {
                global.commit()?
            };
            let result = Box::new(TransactionResult {
                value: Some(value),
                cost: global.cost_track.get_total(),
                assets: assets.unwrap_or_default(),
                events: events.map_or_else(Vec::new, |batch| batch.events),
            });
            Ok(if aborted {
                ContractCallOutcome::AbortedByResponse(result)
            } else {
                ContractCallOutcome::Success(result)
            })
        }
        Err(error) => {
            let cost = global.cost_track.get_total();
            global.roll_back()?;
            if is_acceptable_runtime_failure(&error) {
                Ok(ContractCallOutcome::RuntimeFailure { cost, error })
            } else {
                Err(error)
            }
        }
    }
}

const fn contract_argument(value: &Value) -> Option<&QualifiedContractIdentifier> {
    match value {
        Value::Principal(PrincipalData::Contract(contract)) => Some(contract),
        _ => None,
    }
}

fn ensure_wasm_module(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    modules: &mut ModuleCache,
    contract: &QualifiedContractIdentifier,
) -> Result<(), VmExecutionError> {
    if modules.get(contract).is_some() {
        return Ok(());
    }

    let (source, version) = {
        let mut database = clarity_database(store, bitcoin_context);
        database.begin();
        let result = (|| {
            let contract_context = database.get_contract(contract)?;
            let source = database
                .get_contract_src(contract)
                .ok_or_else(|| VmInternalError::Expect(format!("missing source for {contract}")))?;
            Ok((source, *contract_context.get_clarity_version()))
        })();
        match result {
            Ok(value) => {
                database.commit()?;
                value
            }
            Err(error) => {
                database.roll_back()?;
                return Err(error);
            }
        }
    };
    let compiled = {
        let mut analysis = AnalysisDatabase::new(store);
        analysis
            .execute::<_, _, StaticCheckError>(|analysis_db| {
                Ok(clar2wasm::compile(
                    &source,
                    contract,
                    LimitedCostTracker::new_free(),
                    version,
                    StacksEpochId::Epoch40,
                    analysis_db,
                    true,
                )
                .map(clar2wasm::CompileResult::into_compiled_contract)
                .map_err(|error: clar2wasm::CompileError| {
                    StaticCheckErrorKind::Unreachable(wasm_compile_error(error))
                })?)
            })
            .map_err(|error: StaticCheckError| VmInternalError::Expect(error.to_string()))?
    };
    modules.insert(contract.clone(), compiled);
    Ok(())
}

fn wasm_compile_error(error: clar2wasm::CompileError) -> String {
    match error {
        clar2wasm::CompileError::Generic { diagnostics, .. } => diagnostics
            .into_iter()
            .map(|diagnostic| diagnostic.message)
            .collect::<Vec<_>>()
            .join("; "),
    }
}

fn is_acceptable_runtime_failure(error: &VmExecutionError) -> bool {
    matches!(
        error,
        VmExecutionError::Runtime(_, _) | VmExecutionError::EarlyReturn(_)
    ) || matches!(error, VmExecutionError::RuntimeCheck(error) if !error.rejectable())
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
        &NULL_CONTEXT,
        sender,
        recipient,
        amount,
        memo,
        cost_tracker,
    )
}

fn transfer_stx_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    sender: &PrincipalData,
    recipient: &PrincipalData,
    amount: u128,
    memo: &[u8],
    cost_tracker: LimitedCostTracker,
) -> Result<TransactionResult, VmExecutionError> {
    let network = store.network();
    let database = clarity_database(store, bitcoin_context);
    let mut environment = OwnedEnvironment::new_cost_limited(
        network.is_mainnet(),
        network.chain_id(),
        database,
        cost_tracker,
        StacksEpochId::Epoch40,
    );
    let (value, assets, events) = environment.stx_transfer(
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
        assets,
        events,
    })
}

/// Debit an account's available STX balance in an isolated database transaction.
pub fn debit_fee(
    store: &mut MarfStore,
    payer: &PrincipalData,
    fee: u64,
) -> Result<(), VmExecutionError> {
    debit_fee_in_context(store, &NULL_CONTEXT, payer, fee)
}

fn debit_fee_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    payer: &PrincipalData,
    fee: u64,
) -> Result<(), VmExecutionError> {
    let network = store.network();
    if fee == 0 {
        return Ok(());
    }
    let database = clarity_database(store, bitcoin_context);
    let mut context = GlobalContext::new(
        network.is_mainnet(),
        network.chain_id(),
        database,
        LimitedCostTracker::new_free(),
        StacksEpochId::Epoch40,
    );
    context.execute(|global| {
        let mut balance = global.database.get_stx_balance_snapshot(payer)?;
        if fee != 0 && !balance.can_transfer(u128::from(fee))? {
            return Err(VmInternalError::InsufficientBalance.into());
        }
        balance.debit(u128::from(fee))?;
        balance.save()
    })
}

fn touch_stx_balance_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    principal: &PrincipalData,
) -> Result<(), VmExecutionError> {
    let network = store.network();
    let database = clarity_database(store, bitcoin_context);
    let mut context = GlobalContext::new(
        network.is_mainnet(),
        network.chain_id(),
        database,
        LimitedCostTracker::new_free(),
        StacksEpochId::Epoch40,
    );
    context.execute(|global| {
        let mut balance = global.database.get_stx_balance_snapshot(principal)?;
        balance.debit(0)?;
        balance.save()
    })
}

fn credit_stx_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    principal: &PrincipalData,
    amount: u128,
) -> Result<(), VmExecutionError> {
    let network = store.network();
    let database = clarity_database(store, bitcoin_context);
    let mut context = GlobalContext::new(
        network.is_mainnet(),
        network.chain_id(),
        database,
        LimitedCostTracker::new_free(),
        StacksEpochId::Epoch40,
    );
    context.execute(|global| {
        let mut balance = global.database.get_stx_balance_snapshot(principal)?;
        balance.credit(amount)?;
        balance.save()
    })
}

fn transaction_cost_tracker_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    total: ExecutionCost,
) -> Result<LimitedCostTracker, VmExecutionError> {
    let network = store.network();
    let mut database = clarity_database(store, bitcoin_context);
    database.begin();
    database.set_clarity_epoch_version(StacksEpochId::Epoch40)?;
    let result = LimitedCostTracker::new_mid_block(
        network.is_mainnet(),
        network.chain_id(),
        EPOCH_4_BLOCK_LIMIT,
        &mut database,
        StacksEpochId::Epoch40,
    );
    database.roll_back()?;
    match result {
        Ok(mut tracker) => {
            tracker.set_total(total);
            Ok(tracker)
        }
        Err(CostErrors::CostContractLoadFailure | CostErrors::CostComputationFailed(_)) => {
            Ok(LimitedCostTracker::new_free())
        }
        Err(error) => Err(error.into()),
    }
}

fn process_scheduled_unlocks_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
) -> Result<u128, VmExecutionError> {
    let network = store.network();
    let database = clarity_database(store, bitcoin_context);
    let mut context = GlobalContext::new(
        network.is_mainnet(),
        network.chain_id(),
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
    bitcoin_context: &dyn ChainContext,
    amount: u128,
) -> Result<(), VmExecutionError> {
    let network = store.network();
    let database = clarity_database(store, bitcoin_context);
    let mut context = GlobalContext::new(
        network.is_mainnet(),
        network.chain_id(),
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
    account_nonce_in_context(store, &NULL_CONTEXT, principal)
}

/// Read an account's spendable STX in an isolated database transaction.
fn account_balance_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    principal: &PrincipalData,
) -> Result<u128, VmExecutionError> {
    let network = store.network();
    let database = clarity_database(store, bitcoin_context);
    let mut context = GlobalContext::new(
        network.is_mainnet(),
        network.chain_id(),
        database,
        LimitedCostTracker::new_free(),
        StacksEpochId::Epoch40,
    );
    context.execute(|global| {
        global
            .database
            .get_stx_balance_snapshot(principal)?
            .get_available_balance()
    })
}

fn account_nonce_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    principal: &PrincipalData,
) -> Result<u64, VmExecutionError> {
    let network = store.network();
    let database = clarity_database(store, bitcoin_context);
    let mut context = GlobalContext::new(
        network.is_mainnet(),
        network.chain_id(),
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
    set_account_nonce_in_context(store, &NULL_CONTEXT, principal, nonce)
}

fn set_account_nonce_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    principal: &PrincipalData,
    nonce: u64,
) -> Result<(), VmExecutionError> {
    let network = store.network();
    let database = clarity_database(store, bitcoin_context);
    let mut context = GlobalContext::new(
        network.is_mainnet(),
        network.chain_id(),
        database,
        LimitedCostTracker::new_free(),
        StacksEpochId::Epoch40,
    );
    context.execute(|global| global.database.set_account_nonce(principal, nonce))
}

fn clarity_database<'a>(
    store: &'a mut MarfStore,
    bitcoin_context: &'a dyn ChainContext,
) -> ClarityDatabase<'a> {
    ClarityDatabase::new(store, bitcoin_context, bitcoin_context)
}

fn setup_block_metadata_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    timestamp: u64,
) -> Result<(), VmExecutionError> {
    let network = store.network();
    let database = clarity_database(store, bitcoin_context);
    let mut context = GlobalContext::new(
        network.is_mainnet(),
        network.chain_id(),
        database,
        LimitedCostTracker::new_free(),
        StacksEpochId::Epoch40,
    );
    context.execute(|global| global.database.setup_block_metadata(Some(timestamp)))
}

fn tenure_height_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
) -> Result<u32, VmExecutionError> {
    let network = store.network();
    let database = clarity_database(store, bitcoin_context);
    let mut context = GlobalContext::new(
        network.is_mainnet(),
        network.chain_id(),
        database,
        LimitedCostTracker::new_free(),
        StacksEpochId::Epoch40,
    );
    context.execute(|global| global.database.get_tenure_height())
}

fn set_tenure_height_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    height: u32,
) -> Result<(), VmExecutionError> {
    let network = store.network();
    let database = clarity_database(store, bitcoin_context);
    let mut context = GlobalContext::new(
        network.is_mainnet(),
        network.chain_id(),
        database,
        LimitedCostTracker::new_free(),
        StacksEpochId::Epoch40,
    );
    context.execute(|global| global.database.set_tenure_height(height))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clar2wasm::ModuleCache;
    use clarity::vm::database::ClarityBackingStore;
    use clarity::vm::database::clarity_store::make_contract_hash_key;
    use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier};
    use clarity::vm::{ClarityVersion, Value};
    use nano_primitives::{Network, TrieHash};

    use super::BlockHeader;
    use stacks_common::codec::StacksMessageCodec;
    use stacks_common::types::chainstate::StacksBlockId;

    use super::{
        ContractCallOutcome, MarfStore, Vm, deploy_contract, evaluate, evaluate_in_store,
        execute_contract_call, execute_contract_call_outcome,
    };
    use clarity::vm::costs::LimitedCostTracker;

    #[test]
    fn evaluates_clarity_six_programs() {
        let value = evaluate(Network::TESTNET, "(+ u20 u22)").expect("Clarity 6 program should evaluate");

        assert_eq!(value, Some(Value::UInt(42)));
    }

    #[test]
    fn supports_epoch_four_clarity_six_words() {
        let concatenated =
            evaluate(Network::TESTNET, "(concat 0x01 0x02 0x03)").expect("variadic concat should evaluate");
        let parsed_bitcoin = evaluate(Network::TESTNET, "(get-bitcoin-tx-output? 0x00 u0)")
            .expect("bitcoin transaction parser should evaluate");

        assert_eq!(
            concatenated,
            Some(Value::buff_from(vec![1, 2, 3]).expect("valid buffer"))
        );
        assert_eq!(parsed_bitcoin, Some(Value::err_uint(1)));
    }

    #[test]
    fn rejects_invalid_programs() {
        assert!(evaluate(Network::TESTNET, "(unknown-word u1)").is_err());
    }

    #[test]
    fn marf_store_keeps_forked_values_and_roots() {
        let first = [1; 32];
        let second = [2; 32];
        let fork = [3; 32];
        let mut store = MarfStore::new(Network::TESTNET).expect("create MARF store");

        store.begin(None, first).expect("begin first state");
        store
            .put("counter".to_owned(), "one".to_owned())
            .expect("write first state");
        let pending_first_root = store.pending_root().expect("derive first state root");
        let first_root = store.seal().expect("seal first state");
        assert_eq!(pending_first_root.as_bytes(), &first_root.0);

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
        let mut store = MarfStore::new(Network::TESTNET).expect("create MARF store");
        store.begin(None, [1; 32]).expect("begin state");

        let value =
            evaluate_in_store(&mut store, "(+ u20 u22)").expect("evaluate against MARF store");

        assert_eq!(value, Some(Value::UInt(42)));
        store.seal().expect("seal state");
    }

    #[test]
    fn credits_liquid_stx_without_a_transaction_event() {
        let principal =
            PrincipalData::parse("ST000000000000000000002AMW42H").expect("valid principal");
        let mut vm = Vm::new(Network::TESTNET).expect("create VM");
        vm.begin_block(None, [1; 32]).expect("begin checkpoint");
        vm.record_block_header([1; 32], BlockHeader::default());
        vm.seal_block().expect("seal checkpoint");
        vm.begin_block(Some([1; 32]), [2; 32])
            .expect("begin successor");
        vm.record_block_header([2; 32], BlockHeader::default());
        vm.credit_stx(&principal, 42).expect("credit STX");

        let value = vm
            .execute(
                "(stx-get-balance 'ST000000000000000000002AMW42H)",
                LimitedCostTracker::new_free(),
            )
            .expect("read balance");

        assert_eq!(value.value, Some(Value::UInt(42)));
    }

    #[test]
    fn persists_clarity_data_variables_in_the_marf_store() {
        let mut store = MarfStore::new(Network::TESTNET).expect("create MARF store");
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
    fn transaction_rollback_restores_active_state() {
        let block = [1; 32];
        let mut store = MarfStore::new(Network::TESTNET).expect("create MARF store");
        store.begin(None, block).expect("begin block");
        store
            .put("counter".to_owned(), "one".to_owned())
            .expect("write baseline value");

        store.begin_transaction().expect("begin transaction");
        store
            .put("counter".to_owned(), "two".to_owned())
            .expect("write transactional value");
        store.rollback_transaction().expect("roll back transaction");
        store.seal().expect("seal block");

        assert_eq!(store.get(block, "counter"), Some("one"));
    }

    #[test]
    fn vm_executes_and_seals_a_block_state() {
        let block = [9; 32];
        let mut vm = Vm::new(Network::TESTNET).expect("create VM");
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
        let sender: PrincipalData = contract.issuer.clone().into();
        let mut argument = Vec::new();
        Value::UInt(41)
            .consensus_serialize(&mut argument)
            .expect("serialize Clarity value");
        let mut vm = Vm::new(Network::TESTNET).expect("create VM");
        vm.begin_block(None, block).expect("begin block");
        vm.deploy_contract(
            contract.clone(),
            clarity::vm::ClarityVersion::Clarity6,
            "(define-public (increment (value uint)) (ok (+ value u1)))",
            LimitedCostTracker::new_free(),
        )
        .expect("deploy contract");
        vm.modules = ModuleCache::default();

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

    /// `unwrap!` in a `let` binding must return from the whole function, not
    /// just abandon the binding. PoX-5 guards its entry points this way.
    #[test]
    fn unwrap_in_a_let_binding_returns_from_the_function() {
        let block = [11; 32];
        let contract = QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.guard")
            .expect("valid contract identifier");
        let sender: PrincipalData = contract.issuer.clone().into();
        let mut vm = Vm::new(Network::TESTNET).expect("create VM");
        vm.begin_block(None, block).expect("begin block");
        vm.deploy_contract(
            contract.clone(),
            clarity::vm::ClarityVersion::Clarity6,
            "(define-map absent uint uint)
             (define-public (guarded)
               (let ((value (unwrap! (map-get? absent u1) (err u27)))) (ok value)))",
            LimitedCostTracker::new_free(),
        )
        .expect("deploy contract");
        vm.modules = ModuleCache::default();

        let result = vm
            .execute_contract_call(
                sender,
                None,
                contract,
                "guarded",
                &[],
                LimitedCostTracker::new_free(),
            )
            .expect("call contract");

        assert_eq!(
            result.value,
            Some(Value::error(Value::UInt(27)).expect("valid response"))
        );
        vm.seal_block().expect("seal block");
    }

    #[test]
    fn vm_calls_clarity6_bitcoin_words_through_wasm() {
        let block = [10; 32];
        let contract = QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.bitcoin")
            .expect("valid contract identifier");
        let sender = contract.issuer.clone().into();
        let mut vm = Vm::new(Network::TESTNET).expect("create VM");
        vm.begin_block(None, block).expect("begin block");
        vm.deploy_contract(
            contract.clone(),
            clarity::vm::ClarityVersion::Clarity6,
            "(define-read-only (output) (get-bitcoin-tx-output? 0x00 u0))",
            LimitedCostTracker::new_free(),
        )
        .expect("deploy contract");

        let result = vm
            .execute_contract_call(
                sender,
                None,
                contract,
                "output",
                &[],
                LimitedCostTracker::new_free(),
            )
            .expect("call contract");

        assert_eq!(result.value, Some(Value::err_uint(1)));
    }

    fn assert_successful_wasm_call_matches_interpreter(
        wasm: &mut Vm,
        store: &mut MarfStore,
        sender: PrincipalData,
        contract: QualifiedContractIdentifier,
        function: &str,
        arguments: &[Vec<u8>],
    ) {
        let wasm_result = wasm
            .execute_contract_call(
                sender.clone(),
                None,
                contract.clone(),
                function,
                arguments,
                LimitedCostTracker::new_free(),
            )
            .expect("call WASM contract");
        let interpreter_result = execute_contract_call(
            store,
            sender,
            None,
            contract,
            function,
            arguments,
            LimitedCostTracker::new_free(),
        )
        .expect("call interpreter contract");

        assert_eq!(wasm_result.value, interpreter_result.value);
        assert_eq!(wasm_result.cost, interpreter_result.cost);
        assert_eq!(wasm_result.assets, interpreter_result.assets);
        assert_eq!(wasm_result.events, interpreter_result.events);
    }

    fn assert_wasm_failure_matches_interpreter(
        wasm: &mut Vm,
        store: &mut MarfStore,
        sender: PrincipalData,
        contract: QualifiedContractIdentifier,
        function: &str,
        arguments: &[Vec<u8>],
    ) {
        let wasm_failure = wasm
            .execute_contract_call_outcome(
                sender.clone(),
                None,
                contract.clone(),
                function,
                arguments,
                LimitedCostTracker::new_free(),
            )
            .expect("execute WASM failure");
        let interpreter_failure = execute_contract_call_outcome(
            store,
            sender,
            None,
            contract,
            function,
            arguments,
            LimitedCostTracker::new_free(),
        )
        .expect("execute interpreter failure");
        let (
            ContractCallOutcome::RuntimeFailure {
                cost: wasm_cost,
                error: wasm_error,
            },
            ContractCallOutcome::RuntimeFailure {
                cost: interpreter_cost,
                error: interpreter_error,
            },
        ) = (wasm_failure, interpreter_failure)
        else {
            panic!("{function} should fail at runtime")
        };
        assert_eq!(wasm_cost, interpreter_cost);
        assert_eq!(wasm_error.to_string(), interpreter_error.to_string());
    }

    #[test]
    fn wasm_calls_match_the_clarity_six_interpreter() {
        let contract =
            QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.crosscheck")
                .expect("valid contract identifier");
        let source = "
            (define-public (describe (value (optional int)) (items (list 3 int)))
                (let ((count (len items)) (number (default-to 0 value)))
                    (ok (tuple (count count) (number number)))))
            (define-public (must-have (value (optional int)))
                (ok (unwrap-panic value)))
        ";
        let arguments = [
            Value::some(Value::Int(7))
                .expect("valid optional")
                .serialize_to_vec()
                .expect("serialize optional"),
            Value::list_from(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
                .expect("valid list")
                .serialize_to_vec()
                .expect("serialize list"),
        ];
        let sender: PrincipalData = contract.issuer.clone().into();

        let mut wasm = Vm::new(Network::TESTNET).expect("create WASM VM");
        wasm.begin_block(None, [0x71; 32])
            .expect("begin WASM block");
        wasm.deploy_contract(
            contract.clone(),
            ClarityVersion::Clarity6,
            source,
            LimitedCostTracker::new_free(),
        )
        .expect("deploy WASM contract");
        let mut store = MarfStore::new(Network::TESTNET).expect("create interpreter store");
        store
            .begin(None, [0x72; 32])
            .expect("begin interpreter block");
        deploy_contract(
            &mut store,
            contract.clone(),
            ClarityVersion::Clarity6,
            source,
            LimitedCostTracker::new_free(),
        )
        .expect("deploy interpreter contract");
        assert_successful_wasm_call_matches_interpreter(
            &mut wasm,
            &mut store,
            sender,
            contract.clone(),
            "describe",
            &arguments,
        );

        let none = Value::none()
            .serialize_to_vec()
            .expect("serialize optional none");
        assert_wasm_failure_matches_interpreter(
            &mut wasm,
            &mut store,
            contract.issuer.clone().into(),
            contract,
            "must-have",
            std::slice::from_ref(&none),
        );
    }

    /// The captured corpus is recaptured wholesale, so tests read its checkpoint
    /// identity from the manifest rather than pinning one capture's hashes.
    fn captured_checkpoint() -> (std::path::PathBuf, [u8; 32], TrieHash) {
        let checkpoint = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../nano-conformance/fixtures/chainstate/checkpoint-H");
        let manifest =
            std::fs::read_to_string(checkpoint.join("checkpoint.toml")).expect("read manifest");
        let field = |name: &str| {
            manifest
                .lines()
                .find_map(|line| line.trim().strip_prefix(&format!("{name} = ")))
                .expect("checkpoint manifest field")
                .trim_matches('"')
                .to_owned()
        };
        let decode = |value: &str| -> [u8; 32] {
            hex::decode(value)
                .expect("checkpoint manifest hash")
                .try_into()
                .expect("checkpoint manifest hash length")
        };
        (
            checkpoint.join("marf.sqlite"),
            decode(&field("source_state_id")),
            TrieHash::from_bytes(decode(&field("published_state_index_root"))),
        )
    }

    #[test]
    fn loads_clarity_values_and_metadata_from_a_checkpoint() {
        let (checkpoint, source, root) = captured_checkpoint();
        let contract = QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.pox")
            .expect("valid boot contract identifier");
        let mut store =
            MarfStore::from_checkpoint(Network::TESTNET, checkpoint, source, root).expect("load checkpoint");

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

    #[test]
    fn hydrates_the_pox5_wasm_module_from_checkpoint_contract_source() {
        let (checkpoint, source, root) = captured_checkpoint();
        let contract = QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.pox-5")
            .expect("valid checkpoint contract identifier");
        let sender = contract.issuer.clone().into();
        let mut vm = Vm::from_checkpoint(Network::TESTNET, checkpoint, source, root).expect("load checkpoint");
        vm.begin_block(Some(source), [0x43; 32])
            .expect("extend checkpoint state");

        let result = vm.execute_contract_call(
            sender,
            None,
            contract,
            "get-last-reward-compute-height",
            &[],
            LimitedCostTracker::new_free(),
        );

        assert!(
            matches!(result, Ok(ref result) if matches!(result.value, Some(Value::UInt(_))),),
            "{result:?}"
        );
    }
}
