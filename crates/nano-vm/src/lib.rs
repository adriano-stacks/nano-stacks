use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};

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
};
use clarity::vm::errors::{ClarityEvalError, RuntimeError, VmExecutionError, VmInternalError};
use clarity::vm::events::StacksTransactionEvent;
use clarity::vm::representations::SymbolicExpression;
use clarity::vm::types::{
    BuffData, PrincipalData, QualifiedContractIdentifier, SequenceData,
};
use clarity::vm::{ClarityVersion, Value};
use nano_marf::{
    CheckpointError, MarfError, MarfSnapshot, MarfValue, StateRoot, TriePointer, VersionedMarf,
    import_checkpoint, import_checkpoint_into,
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

/// The MARF root a block sealed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionResult {
    pub state_root: StateRoot,
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
    /// The tenure's sortition hash, which a coinbase VRF proof is over.
    ///
    /// Validation only: no Clarity word reads it, so it moves no state root.
    pub sortition_hash: [u8; 32],
    /// The winning commitment's leader-key VRF public key, when this node saw
    /// the burn block that registered it.
    ///
    /// Validation only, and `None` means "not known here", not "no key".
    pub winner_vrf_public_key: Option<[u8; 32]>,
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
            sortition_hash: [0; 32],
            winner_vrf_public_key: None,
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

/// Everything an accepted block makes durable outside the MARF.
///
/// The two travel together because they land in one side-store transaction, and
/// that transaction runs *before* the MARF commit. The MARF's own commit is
/// therefore the only decision point: a block is in the MARF only if everything
/// describing it is already on disk. A crash between the two leaves rows for a
/// block the MARF never got, and nothing can read them, because the only way to
/// reach them is to stand on that block.
#[derive(Clone, Debug)]
pub struct BlockCommit {
    /// What Clarity may read about the block.
    pub header: BlockHeader,
    /// The state its caller keeps beside the MARF, opaque here.
    pub ledger: Vec<u8>,
}

/// Everything outside the MARF that Clarity may read.
///
/// The burn state and the block headers travel together because Clarity reads
/// them through one database, and keeping them in one value spares every
/// evaluation path a second parameter.
/// The native side of a `PoX` contract call: locks, unlocks and lock events.
pub mod pox;

pub trait ChainContext: BurnStateDB + HeadersDB {}

impl<T: BurnStateDB + HeadersDB> ChainContext for T {}

/// Bitcoin state and executed headers available while evaluating.
#[derive(Debug, Default)]
struct BitcoinContext {
    /// Which chain this is, because only mainnet's epoch boundaries are known.
    mainnet: bool,
    height: u32,
    first_height: u32,
    prepare_phase_length: u32,
    reward_phase_length: u32,
    rejection_fraction: u64,
    v1_unlock_height: u32,
    v2_unlock_height: u32,
    v3_unlock_height: u32,
    pox_5_activation_height: u32,
    /// Headers this node has executed, which a query consults before the
    /// store — a block being executed is asked about before it is written.
    headers: BTreeMap<[u8; 32], BlockHeader>,
    /// Every block this node knows about, including the checkpoint's ancestry.
    headers_db: Option<Mutex<rusqlite::Connection>>,
    /// Blocks a contract asked about whose headers are not held, so the node
    /// can fetch them and try the block again rather than seal a root built on
    /// a `none` the network never answered.
    missing_headers: Mutex<Vec<[u8; 32]>>,
    /// Stacks height each tenure started at, for the tenure-height mapping.
    tenure_starts: BTreeMap<u32, u32>,
    /// The Bitcoin block at each burn height, as `get-burn-block-info?` reads
    /// it back.
    ///
    /// Answering `none` here is not a harmless gap: sBTC's withdrawal path
    /// compares the hash it was given against this to check Bitcoin has not
    /// forked, so a node with no answer rejects a withdrawal the network
    /// accepted and diverges on the block that carries it.
    burn_headers: BTreeMap<u32, [u8; 32]>,
}

/// A header's fixed byte layout, so a store can hold one without serde.
fn encode_block_header(header: &BlockHeader) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(180);
    bytes.extend_from_slice(&header.burn_header_hash);
    bytes.extend_from_slice(&header.burn_block_height.to_be_bytes());
    bytes.extend_from_slice(&header.burn_block_time.to_be_bytes());
    bytes.extend_from_slice(&header.stacks_block_time.to_be_bytes());
    bytes.extend_from_slice(&header.block_header_hash);
    bytes.extend_from_slice(&header.consensus_hash);
    bytes.extend_from_slice(&header.vrf_seed);
    bytes.push(header.miner_address.0);
    bytes.extend_from_slice(&header.miner_address.1);
    bytes.extend_from_slice(&header.burn_spend_total.to_be_bytes());
    bytes.extend_from_slice(&header.burn_spend_winner.to_be_bytes());
    bytes.extend_from_slice(&header.block_reward.to_be_bytes());
    bytes.extend_from_slice(&header.tenure_height.to_be_bytes());
    bytes.extend_from_slice(&header.tenure_start_height.to_be_bytes());
    bytes
}

fn decode_block_header(bytes: &[u8]) -> Option<BlockHeader> {
    let mut reader = bytes;
    let mut take = |count: usize| -> Option<&[u8]> {
        let (head, rest) = reader.split_at_checked(count)?;
        reader = rest;
        Some(head)
    };
    let header = BlockHeader {
        burn_header_hash: take(32)?.try_into().ok()?,
        burn_block_height: u32::from_be_bytes(take(4)?.try_into().ok()?),
        burn_block_time: u64::from_be_bytes(take(8)?.try_into().ok()?),
        stacks_block_time: u64::from_be_bytes(take(8)?.try_into().ok()?),
        block_header_hash: take(32)?.try_into().ok()?,
        consensus_hash: take(20)?.try_into().ok()?,
        vrf_seed: take(32)?.try_into().ok()?,
        miner_address: (take(1)?[0], take(20)?.try_into().ok()?),
        burn_spend_total: u128::from_be_bytes(take(16)?.try_into().ok()?),
        burn_spend_winner: u128::from_be_bytes(take(16)?.try_into().ok()?),
        block_reward: u128::from_be_bytes(take(16)?.try_into().ok()?),
        tenure_height: u32::from_be_bytes(take(4)?.try_into().ok()?),
        tenure_start_height: u32::from_be_bytes(take(4)?.try_into().ok()?),
    };
    Some(header)
}

/// The epochs a stored contract may be rebuilt under, oldest first.
const ACCEPTING_EPOCHS: [StacksEpochId; 6] = [
    StacksEpochId::Epoch20,
    StacksEpochId::Epoch21,
    StacksEpochId::Epoch25,
    StacksEpochId::Epoch30,
    StacksEpochId::Epoch33,
    StacksEpochId::Epoch34,
];

/// Where mainnet's epochs begin, by Bitcoin height.
///
/// Transcribed from `stackslib::core`, and pinned against it by a test rather
/// than trusted: a boundary that is wrong recompiles a contract under rules it
/// was never written for.
const MAINNET_EPOCHS: [(u64, StacksEpochId); 12] = [
    (0, StacksEpochId::Epoch20),
    (713_000, StacksEpochId::Epoch2_05),
    (781_551, StacksEpochId::Epoch21),
    (787_651, StacksEpochId::Epoch22),
    (788_240, StacksEpochId::Epoch23),
    (791_551, StacksEpochId::Epoch24),
    (840_360, StacksEpochId::Epoch25),
    (867_867, StacksEpochId::Epoch30),
    (875_000, StacksEpochId::Epoch31),
    (907_740, StacksEpochId::Epoch32),
    (923_222, StacksEpochId::Epoch33),
    (943_333, StacksEpochId::Epoch34),
];

/// The epoch a Bitcoin height falls in, on the network it belongs to.
///
/// Only mainnet's boundaries are known here. Another network's are a matter of
/// its own configuration, so it is treated as being at the epoch this node
/// runs — which is what a fresh chain is.
fn epoch_at_burn_height(mainnet: bool, height: u64) -> StacksEpochId {
    if !mainnet {
        return StacksEpochId::Epoch40;
    }
    if height >= MAINNET_EPOCH_40_HEIGHT {
        return StacksEpochId::Epoch40;
    }
    MAINNET_EPOCHS
        .iter()
        .rev()
        .find(|(start, _)| height >= *start)
        .map_or(StacksEpochId::Epoch20, |(_, epoch)| *epoch)
}

/// Where epoch 4.0 begins on mainnet, which is where pox-5 activates.
const MAINNET_EPOCH_40_HEIGHT: u64 = 960_230;

/// A sortition identifier naming the burn height it belongs to.
///
/// Sortition identifiers appear in no consensus preimage, and a follower holds
/// one fork's burn view, so the height is the whole of what one has to carry:
/// Clarity only ever uses one to ask what happened at a burn height.
fn sortition_of_burn_height(height: u32) -> SortitionId {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&height.to_be_bytes());
    SortitionId(bytes)
}

/// The burn height a sortition identifier names.
fn burn_height_of(sortition: &SortitionId) -> u32 {
    u32::from_be_bytes(sortition.0[..4].try_into().unwrap_or([0; 4]))
}

/// A context that knows no chain, for reads that need none.
#[must_use]
pub fn null_context() -> &'static (impl ChainContext + 'static) {
    &NULL_CONTEXT
}

/// A context that knows no chain, for evaluating programs that read none.
static NULL_CONTEXT: BitcoinContext = BitcoinContext {
    missing_headers: Mutex::new(Vec::new()),
    mainnet: false,
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
    headers_db: None,
    tenure_starts: BTreeMap::new(),
    burn_headers: BTreeMap::new(),
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
        // The block about to execute can be asked about its own burn block,
        // which no header records until it has been executed.
        if context.burn_header_hash != [0; 32] {
            self.burn_headers.insert(self.height, context.burn_header_hash);
        }
        Ok(())
    }

    pub(crate) fn header(&self, id: &StacksBlockId) -> Option<BlockHeader> {
        if let Some(header) = self.headers.get(id.as_bytes()) {
            return Some(*header);
        }
        let bytes: Option<Vec<u8>> = {
            let guard = self.headers_db.as_ref()?.lock().ok()?;
            guard
                .query_row(
                    "SELECT data FROM block_header WHERE block_id = ?1",
                    params![id.as_bytes().as_slice()],
                    |row| row.get(0),
                )
                .ok()
        };
        if bytes.is_none() {
            // A checkpointed node holds the block index for all of history but
            // headers only from the anchor forward, so a contract asking about
            // an older block gets `none` — and a contract that reads `none` as
            // "no such block" takes its error path and seals a state root the
            // network never had. That is silent: unlike the epoch lookup, which
            // fails loudly, nothing here says a header was wanted and missing.
            //
            // So it is written down, and the node backfills what was asked for
            // before it tries the block again.
            if let Ok(mut missing) = self.missing_headers.lock()
                && !missing.contains(id.as_bytes())
            {
                missing.push(*id.as_bytes());
            }
            if std::env::var_os("NANO_TRACE_WRITES").is_some() {
                println!("no header for block {id}");
            }
        }
        decode_block_header(&bytes?)
    }

    /// The Bitcoin block at a burn height, from memory or from the store.
    ///
    /// The store is consulted because this map is memory: a restart that
    /// answered only from what it had re-seeded would answer `none` where the
    /// run before it answered a hash, and an sBTC withdrawal naming a burn block
    /// deeper than the re-seeded window would be rejected on a chain that
    /// accepted it.
    fn burn_header(&self, height: u32) -> Option<[u8; 32]> {
        if let Some(hash) = self.burn_headers.get(&height) {
            return Some(*hash);
        }
        let hash: Option<Vec<u8>> = {
            let guard = self.headers_db.as_ref()?.lock().ok()?;
            guard
                .query_row(
                    "SELECT hash FROM burn_header WHERE height = ?1",
                    params![height],
                    |row| row.get(0),
                )
                .ok()
        };
        <[u8; 32]>::try_from(hash?.as_slice()).ok()
    }

    /// Blocks a contract asked about whose headers this node does not hold.
    pub(crate) fn take_missing_headers(&self) -> Vec<[u8; 32]> {
        self.missing_headers
            .lock()
            .map(|mut missing| std::mem::take(&mut *missing))
            .unwrap_or_default()
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

    /// Name the sortition of the block being executed.
    ///
    /// From epoch 3 on, `clarity_uses_tip_burn_block()` sends every burn-block
    /// read through this rather than through the parent's consensus hash, so
    /// answering `none` makes `get-burn-block-info?` answer `none` for every
    /// height without raising anything. sBTC's withdrawal path reads it to
    /// check Bitcoin has not forked, so nano rejected a withdrawal mainnet
    /// accepted and diverged on the block that carried it.
    ///
    /// A sortition identifier appears in no consensus preimage, and a follower
    /// holds one fork's burn headers, so the burn height it is standing on
    /// names its own sortition.
    fn get_tip_sortition_id(&self) -> Option<SortitionId> {
        Some(sortition_of_burn_height(self.height))
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

    fn get_burn_block_height(&self, sortition_id: &SortitionId) -> Option<u32> {
        Some(burn_height_of(sortition_id))
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
        height: u32,
        _sortition_id: &SortitionId,
    ) -> Option<BurnchainHeaderHash> {
        // A burn block Clarity cannot be told about is a withdrawal this node
        // rejects and the network accepted, so it is worth being able to see
        // what every lookup answered.
        let found = self.burn_header(height);
        if std::env::var_os("NANO_TRACE_BURN_HEADERS").is_some() {
            println!(
                "burn header {height} -> {}",
                found.map_or_else(|| "<none>".to_owned(), hex::encode)
            );
        }
        found.map(BurnchainHeaderHash)
    }

    /// Name a sortition for a consensus hash, so that a burn header can be
    /// looked up at all.
    ///
    /// Clarity reaches a burn header through this, and answering `none` stops
    /// `get-burn-block-info?` before it starts. A sortition identifier appears
    /// in no consensus preimage, and a follower holds exactly one fork's
    /// headers, so the consensus hash naming its own sortition is enough: the
    /// height alone identifies the burn block on the chain this node executed.
    fn get_sortition_id_from_consensus_hash(
        &self,
        consensus_hash: &ConsensusHash,
    ) -> Option<SortitionId> {
        self.headers
            .values()
            .find(|header| header.consensus_hash == consensus_hash.0)
            .map(|header| sortition_of_burn_height(header.burn_block_height))
            .or_else(|| Some(sortition_of_burn_height(self.height)))
    }

    /// The epoch a Bitcoin height falls in.
    ///
    /// Answering "epoch 4.0" for every height is only right at the tip. A
    /// contract deployed years ago was analysed under the epoch of its own
    /// time, and recompiling it needs to know which — otherwise every contract
    /// written against an older Clarity has to be discovered by failing.
    fn get_stacks_epoch(&self, height: u32) -> Option<StacksEpoch<ExecutionCost>> {
        let epoch_id = epoch_at_burn_height(self.mainnet, u64::from(height));
        Some(StacksEpoch {
            epoch_id,
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
        Ok(Self::over(MarfStore::from_checkpoint(
            network,
            path,
            source,
            expected_root,
        )?))
    }

    /// Open, creating if absent, the durable chainstate held in `directory`.
    pub fn open(network: Network, directory: impl AsRef<Path>) -> Result<Self, MarfStoreError> {
        Ok(Self::over(MarfStore::open(network, directory)?))
    }

    /// Open a durable chainstate, importing `checkpoint` the first time only.
    pub fn open_from_checkpoint(
        network: Network,
        directory: impl AsRef<Path>,
        checkpoint: impl AsRef<Path>,
        source: [u8; 32],
        expected_root: TrieHash,
    ) -> Result<Self, MarfStoreError> {
        Ok(Self::over(MarfStore::open_from_checkpoint(
            network,
            directory,
            checkpoint,
            source,
            expected_root,
        )?))
    }

    fn over(store: MarfStore) -> Self {
        // The context answers header reads from the same file the store writes
        // them to, through a connection of its own because the two are
        // borrowed at once while Clarity runs.
        let store_network = store.network();
        let mainnet = store_network.is_mainnet();
        let headers_db = store
            .side_store_path()
            .and_then(|path| rusqlite::Connection::open(path).ok())
            .map(Mutex::new);
        Self {
            modules: native_module_cache(store.side_store_path().as_deref()),
            store,
            context: BitcoinContext {
                mainnet,
                headers_db,
                ..BitcoinContext::default()
            },
        }
    }

    /// Whether this node has written down what Clarity may read about a block.
    ///
    /// Asked of the sealed tip rather than of the store as a whole, so a
    /// backfill that a peer cut short resumes rather than counting itself done.
    #[must_use]
    pub fn has_recorded_header(&self, block: [u8; 32]) -> bool {
        self.recorded_header(block).is_some()
    }

    /// What this node knows about a block, for tests and diagnostics.
    #[must_use]
    pub fn recorded_header(&self, block: [u8; 32]) -> Option<BlockHeader> {
        self.context.header(&StacksBlockId(block))
    }

    /// The deepest state on disk, which is where a restart resumes.
    #[must_use]
    pub fn tip(&self) -> Option<[u8; 32]> {
        self.store.tip()
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

    /// The state a block was built on, as the store recorded it.
    #[must_use]
    pub fn parent_of(&self, block: [u8; 32]) -> Option<[u8; 32]> {
        self.store.parent_of(block)
    }

    /// Record what Clarity may later read about a block nano has executed.
    /// The Stacks height of a block, from the block index the checkpoint
    /// imports — which covers every block, including ones with no state.
    #[must_use]
    pub fn height_of(&self, block: [u8; 32]) -> Option<u32> {
        self.store.height_of(block)
    }

    /// Blocks a contract asked about whose headers this node does not hold.
    ///
    /// Taken, not read: each caller sees what has been missed since it last
    /// looked, so a retry that fetches them does not keep refetching.
    pub fn take_missing_headers(&self) -> Vec<[u8; 32]> {
        self.context.take_missing_headers()
    }

    /// The header this node holds for a block, if it holds one.
    #[must_use]
    pub fn recorded_block_header(&self, block: &[u8; 32]) -> Option<BlockHeader> {
        self.context.header(&StacksBlockId(*block))
    }

    /// Write a header down for a block this node is not sealing.
    ///
    /// The failure is returned rather than printed: a header a later block reads
    /// through is not optional, and a caller that cannot write one has to stop
    /// rather than carry on answering `none` where the chain has an answer.
    pub fn record_block_header(
        &mut self,
        block: [u8; 32],
        header: BlockHeader,
    ) -> Result<(), MarfStoreError> {
        self.store.write_block_header(block, &header)?;
        self.remember_block_header(block, header);
        Ok(())
    }

    /// Keep in memory what a written-down header answers.
    ///
    /// `tenure_starts` is first-write-wins, which is why this is separate: for a
    /// block being sealed it must not run until the MARF has committed, or a
    /// block that failed to seal would fix its tenure's start height for every
    /// later block.
    fn remember_block_header(&mut self, block: [u8; 32], header: BlockHeader) {
        self.context
            .tenure_starts
            .entry(header.tenure_height)
            .or_insert(header.tenure_start_height);
        self.context
            .burn_headers
            .insert(header.burn_block_height, header.burn_header_hash);
        self.context.headers.insert(block, header);
    }

    /// Seal a block together with everything that describes it.
    ///
    /// One call because there is one commit: the header, the caller's ledger and
    /// the block's Clarity metadata are written in a single side-store
    /// transaction and the MARF commit follows, so no sealed root can be missing
    /// any of them. Before this they were four writes in four transactions, and
    /// a crash between any two left a state nothing could reconcile.
    pub fn commit_block(
        &mut self,
        block: [u8; 32],
        commit: &BlockCommit,
    ) -> Result<StateRoot, MarfStoreError> {
        let root = self.store.commit_to(block, commit)?;
        self.remember_block_header(block, commit.header);
        Ok(root)
    }

    /// The state the run that sealed this block kept beside the MARF.
    #[must_use]
    pub fn recorded_ledger(&self, block: [u8; 32]) -> Option<Vec<u8>> {
        self.store.ledger_at(block)
    }

    /// Put back the tenure-start answers an earlier run recorded.
    ///
    /// `get-tenure-info?` reads these, and they live in memory, so a restart
    /// without them reports the first block it executes as its tenure's start.
    /// First-write-wins as everywhere else, so this cannot overwrite an answer
    /// the current run has already given.
    pub fn restore_tenure_starts(&mut self, starts: impl IntoIterator<Item = (u32, u32)>) {
        for (tenure_height, stacks_height) in starts {
            self.context
                .tenure_starts
                .entry(tenure_height)
                .or_insert(stacks_height);
        }
    }

    /// Record the Stacks height of an imported checkpoint when it is not stored in the MARF.
    pub fn set_checkpoint_height(&mut self, block: [u8; 32], height: u32) {
        self.store.set_checkpoint_height(block, height);
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
        // There is no other engine to deploy with. A contract clarity-wasm
        // cannot build is a compiler conformance bug, and deploying it another
        // way would hide that behind a chain that appears to advance — which is
        // how a replay reached 8,673,863 while the compiler had actually stopped
        // at 8,668,161. It has to stop and name the contract.
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
        cost_tracker: &LimitedCostTracker,
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
        sender: &PrincipalData,
        contract: &QualifiedContractIdentifier,
        function: &str,
        arguments: &[Value],
    ) -> Result<Value, VmExecutionError> {
        let Self {
            store,
            context,
            modules,
            ..
        } = self;
        match call_contract_values_in_context(
            store,
            context,
            modules,
            sender,
            None,
            contract,
            function,
            arguments,
            &LimitedCostTracker::new_free(),
        )? {
            ContractCallOutcome::Success(result)
            | ContractCallOutcome::AbortedByResponse(result) => result.value.ok_or_else(|| {
                VmInternalError::Expect("contract call returned no value".to_owned()).into()
            }),
            ContractCallOutcome::RuntimeFailure { error, .. } => Err(error),
        }
    }

    /// Compile a contract in this state and report whether its module loads.
    ///
    /// This is the path a node takes when it meets a contract deployed before
    /// it started, resolving references against the state on disk. Pointing it
    /// at an edited source is how a contract whose module the runtime refuses
    /// gets narrowed down to the definition that causes it.
    pub fn check_module(
        &mut self,
        contract: &QualifiedContractIdentifier,
        version: ClarityVersion,
        source: &str,
        epoch: StacksEpochId,
    ) -> Result<(), VmExecutionError> {
        compile_under(&mut self.store, contract, source, version, epoch)
            .and_then(|compiled| loadable(contract, compiled, self.modules.engine()))
            .map(|_| ())
    }

    /// The source and Clarity version this state holds for a contract.
    ///
    /// A contract the compiler refuses is one already on the chain, so the only
    /// source worth compiling is the state's own — reading it from anywhere else
    /// is a chance to bisect the wrong text.
    pub fn contract_source(
        &mut self,
        contract: &QualifiedContractIdentifier,
    ) -> Result<(String, ClarityVersion), VmExecutionError> {
        contract_source(&mut self.store, &self.context, contract)
    }

    /// Record what a burn block's header hash is, for Clarity to read back.
    ///
    /// `get-burn-block-info? header-hash` answers from the burn blocks this node
    /// has executed under, and a node that started at a checkpoint has none from
    /// before it. sBTC's withdrawal path checks a sweep's Bitcoin block that
    /// way, so an unanswered height rejects a withdrawal the network accepted.
    /// Make a burn block's header hash readable from Clarity, now and after a
    /// restart.
    ///
    /// Written outside any block's commit because it is a fact about Bitcoin
    /// rather than about a Stacks block: it is true before the block that reads
    /// it exists, and re-reading it costs a Bitcoin block download.
    pub fn record_burn_header(
        &mut self,
        height: u64,
        hash: [u8; 32],
    ) -> Result<(), MarfStoreError> {
        if let Ok(height) = u32::try_from(height) {
            self.context.burn_headers.insert(height, hash);
            self.store.write_burn_header(height, hash)?;
        }
        Ok(())
    }

    /// Whether Clarity can already answer for this burn block.
    #[must_use]
    pub fn knows_burn_header(&self, height: u64) -> bool {
        u32::try_from(height).is_ok_and(|height| self.context.burn_header(height).is_some())
    }

    pub fn execute_contract_call_outcome(
        &mut self,
        sender: PrincipalData,
        sponsor: Option<PrincipalData>,
        contract: QualifiedContractIdentifier,
        function: &str,
        arguments: &[Vec<u8>],
        cost_tracker: &LimitedCostTracker,
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
            &ContractCall {
                sender,
                sponsor,
                contract,
                function,
                arguments,
            },
            cost_tracker,
        )
    }

    /// The state and the chain context the engine reads, for a differential
    /// oracle built outside this crate.
    ///
    /// Handing these out cannot make the node execute anything differently: an
    /// engine has to be linked in to be run, and this crate contains one.
    pub const fn state_and_context(&mut self) -> (&mut MarfStore, &impl ChainContext) {
        (&mut self.store, &self.context)
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

    /// Read an account's locked and unlocked STX and its nonce, from state.
    pub fn account_state(
        &mut self,
        principal: &PrincipalData,
    ) -> Result<AccountState, VmExecutionError> {
        let Self { store, context, .. } = self;
        account_state_in_context(store, context, principal)
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
    pub fn get(&self, block: [u8; 32], key: &str) -> Option<String> {
        self.store.get(block, key)
    }
}

/// A versioned Clarity key/value store whose state roots are committed by the MARF.
///
/// Values live in the MARF and its side store, so nothing a sealed block wrote
/// is held in memory: a read resolves the key through the trie and then the
/// value by its hash. Only the block being executed keeps writes in memory, and
/// only its metadata, which the MARF does not commit.
#[derive(Debug)]
pub struct MarfStore {
    network: Network,
    marf: VersionedMarf,
    side_store: rusqlite::Connection,
    metadata: BTreeMap<(String, String), String>,
    checkpoint_heights: BTreeMap<[u8; 32], u32>,
    read_block: Option<[u8; 32]>,
    active: Option<ActiveStore>,
    transaction: Option<StoreTransaction>,
}

#[derive(Clone, Copy, Debug)]
struct ActiveStore {
    block: [u8; 32],
    parent: Option<[u8; 32]>,
    height: u32,
}

#[derive(Clone, Debug)]
struct StoreTransaction {
    marf: MarfSnapshot,
    metadata: BTreeMap<(String, String), String>,
    read_block: Option<[u8; 32]>,
    active: Option<ActiveStore>,
}

#[derive(Debug)]
pub enum MarfStoreError {
    Marf(MarfError),
    Checkpoint(CheckpointError),
    Sql(rusqlite::Error),
    Io(std::io::Error),
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
            Self::Io(error) => write!(formatter, "chainstate I/O error: {error}"),
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
        Ok(Self::assemble(
            network,
            VersionedMarf::default(),
            create_side_store()?,
            None,
        ))
    }

    /// Open, creating if absent, the durable store held in `directory`.
    ///
    /// A store that already holds state resumes from the tip it left there.
    pub fn open(network: Network, directory: impl AsRef<Path>) -> Result<Self, MarfStoreError> {
        let directory = directory.as_ref();
        std::fs::create_dir_all(directory).map_err(MarfStoreError::Io)?;
        let marf = VersionedMarf::open(directory.join(MARF_FILE))?;
        let side_store = open_side_store(&directory.join(CLARITY_FILE))?;
        let tip = marf.tip();
        Ok(Self::assemble(network, marf, side_store, tip))
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
        let side_store = create_side_store()?;
        import_side_store(&side_store, path, None)?;
        Ok(Self::assemble(network, marf, side_store, Some(source)))
    }

    /// Open a durable store in `directory`, importing `checkpoint` the first time.
    ///
    /// A directory that already holds state is resumed from its tip, so a
    /// restart replays only the blocks that came after it.
    pub fn open_from_checkpoint(
        network: Network,
        directory: impl AsRef<Path>,
        checkpoint: impl AsRef<Path>,
        source: [u8; 32],
        expected_root: TrieHash,
    ) -> Result<Self, MarfStoreError> {
        let directory = directory.as_ref();
        std::fs::create_dir_all(directory).map_err(MarfStoreError::Io)?;
        let marf_path = directory.join(MARF_FILE);
        let clarity_path = directory.join(CLARITY_FILE);
        if VersionedMarf::open(&marf_path)?.tip().is_some() {
            return Self::open(network, directory);
        }
        let marf = import_checkpoint_into(&marf_path, checkpoint.as_ref(), source, expected_root)?;
        {
            // Journalling off only while the import runs; the store is then
            // reopened normally, so everything executed on top of it is
            // durable.
            let importing = open_side_store_for_import(&clarity_path)?;
            import_side_store(&importing, checkpoint.as_ref(), Some(&marf_path))?;
        }
        let side_store = open_side_store(&clarity_path)?;
        Ok(Self::assemble(network, marf, side_store, Some(source)))
    }

    const fn assemble(
        network: Network,
        marf: VersionedMarf,
        side_store: rusqlite::Connection,
        read_block: Option<[u8; 32]>,
    ) -> Self {
        Self {
            network,
            marf,
            side_store,
            metadata: BTreeMap::new(),
            checkpoint_heights: BTreeMap::new(),
            read_block,
            active: None,
            transaction: None,
        }
    }

    /// The deepest sealed state, which is where a reopened store resumes.
    #[must_use]
    pub fn tip(&self) -> Option<[u8; 32]> {
        self.marf.tip()
    }

    /// The chain this state belongs to.
    #[must_use]
    pub const fn network(&self) -> Network {
        self.network
    }

    /// The state a block was built on, as the store recorded it.
    #[must_use]
    pub fn parent_of(&self, block: [u8; 32]) -> Option<[u8; 32]> {
        self.marf.parent(block).flatten()
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
        let height = parent
            .and_then(|parent| self.height_of(parent))
            .map_or(0, |height| height + 1);
        self.metadata.clear();
        self.active = Some(ActiveStore {
            block,
            parent,
            height,
        });
        self.read_block = Some(block);
        Ok(())
    }

    #[must_use]
    pub fn height_of(&self, block: [u8; 32]) -> Option<u32> {
        self.marf
            .height(block)
            .or_else(|| self.checkpoint_heights.get(&block).copied())
    }

    /// Start an atomic transaction within the active block state.
    pub fn begin_transaction(&mut self) -> Result<(), MarfStoreError> {
        if self.transaction.is_some() {
            return Err(MarfStoreError::TransactionInProgress);
        }
        self.transaction = Some(StoreTransaction {
            marf: self.marf.snapshot(),
            metadata: self.metadata.clone(),
            read_block: self.read_block,
            active: self.active,
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
        self.marf.restore(transaction.marf);
        self.metadata = transaction.metadata;
        self.read_block = transaction.read_block;
        self.active = transaction.active;
        Ok(())
    }

    /// Persist a Clarity database key and commit its value hash into the active MARF.
    pub fn put(&mut self, key: &str, value: &str) -> Result<(), MarfStoreError> {
        let value_hash = MarfValue::from_value(value.as_bytes());
        // A root that differs while every receipt matches is a write-ordering
        // or MARF fault, and the only way to narrow it is to see the writes
        // themselves — in the order they were made, which is what the trie
        // packs pointers in.
        if std::env::var_os("NANO_TRACE_WRITES").is_some() {
            println!("write {key} = {}", marf_value_key(value_hash));
        }
        self.marf.insert(key.as_bytes(), value_hash)?;
        self.write_value(value_hash, value)
    }

    /// Keep what Clarity may read about a block.
    fn write_block_header(
        &self,
        block: [u8; 32],
        header: &BlockHeader,
    ) -> Result<(), MarfStoreError> {
        self.side_store
            .prepare_cached("INSERT OR REPLACE INTO block_header (block_id, data) VALUES (?1, ?2)")?
            .execute(params![block.as_slice(), encode_block_header(header)])?;
        Ok(())
    }

    /// Keep the Bitcoin block at a burn height.
    fn write_burn_header(&self, height: u32, hash: [u8; 32]) -> Result<(), MarfStoreError> {
        // Replaced rather than ignored: a Bitcoin reorganization gives a height
        // a different block, and the memory this mirrors overwrites too.
        self.side_store
            .prepare_cached("INSERT OR REPLACE INTO burn_header (height, hash) VALUES (?1, ?2)")?
            .execute(params![height, hash.as_slice()])?;
        Ok(())
    }

    /// Keep the state its caller holds beside the MARF, as of this block.
    fn write_ledger(&self, block: [u8; 32], ledger: &[u8]) -> Result<(), MarfStoreError> {
        self.side_store
            .prepare_cached(
                "INSERT OR REPLACE INTO chain_ledger (block_id, sequence, data) \
                 VALUES (?1, (SELECT COALESCE(MAX(sequence), 0) + 1 FROM chain_ledger), ?2)",
            )?
            .execute(params![block.as_slice(), ledger])?;
        self.side_store
            .prepare_cached(
                "DELETE FROM chain_ledger \
                 WHERE sequence <= (SELECT MAX(sequence) FROM chain_ledger) - ?1",
            )?
            .execute(params![LEDGER_HISTORY])?;
        Ok(())
    }

    /// The state the run that sealed this block kept beside the MARF.
    #[must_use]
    pub fn ledger_at(&self, block: [u8; 32]) -> Option<Vec<u8>> {
        self.side_store
            .prepare_cached("SELECT data FROM chain_ledger WHERE block_id = ?1")
            .ok()?
            .query_row(params![block.as_slice()], |row| row.get(0))
            .optional()
            .ok()?
    }

    /// Where this store keeps what is not in the trie, when it is on disk.
    fn side_store_path(&self) -> Option<std::path::PathBuf> {
        self.side_store.path().map(std::path::PathBuf::from)
    }

    /// Replace a contract's stored definition with one the interpreter can run.
    ///
    /// `insert_contract` refuses to overwrite, which is right for a deploy and
    /// wrong for a repair. This writes the side store directly, which is safe
    /// precisely because that store is not the MARF: no state root moves.
    pub fn replace_contract_definition(
        &self,
        contract: &QualifiedContractIdentifier,
        definition: &str,
    ) -> Result<(), MarfStoreError> {
        let key = format!("clr-meta::{contract}::vm-metadata::9::contract");
        self.side_store
            .prepare_cached(
                "UPDATE metadata_table SET value = ?2 WHERE key = ?1",
            )?
            .execute(params![key, definition])?;
        Ok(())
    }

    /// A contract's stored definition, read straight from the side store.
    ///
    /// `get_contract` answers from the block being executed and did not find
    /// contracts that are plainly present, which is the wrong question when the
    /// point is to repair a definition rather than to run one.
    #[must_use]
    pub fn stored_contract(&self, contract: &QualifiedContractIdentifier) -> Option<String> {
        let key = format!("clr-meta::{contract}::vm-metadata::9::contract");
        self.side_store
            .prepare_cached("SELECT value FROM metadata_table WHERE key = ?1 LIMIT 1")
            .ok()?
            .query_row(params![key], |row| row.get::<_, String>(0))
            .ok()
    }

    /// Every contract here whose stored definition is a placeholder.
    #[must_use]
    pub fn stubbed_contracts(&self) -> Vec<QualifiedContractIdentifier> {
        let Ok(mut statement) = self.side_store.prepare(
            "SELECT key FROM metadata_table WHERE key LIKE '%::vm-metadata::9::contract' \
             AND value LIKE '%\"body\":{\"expr\":{\"LiteralValue\":{\"Int\":0}}%'",
        ) else {
            return Vec::new();
        };
        let Ok(rows) = statement.query_map([], |row| row.get::<_, String>(0)) else {
            return Vec::new();
        };
        rows.filter_map(Result::ok)
            .filter_map(|key| {
                let name = key
                    .strip_prefix("clr-meta::")?
                    .strip_suffix("::vm-metadata::9::contract")?;
                QualifiedContractIdentifier::parse(name).ok()
            })
            .collect()
    }

    /// Whether a contract's stored definition can be run by the interpreter.
    ///
    /// clar2wasm's deploy writes placeholder function bodies — the real ones
    /// live in the module — so a contract the compiler deployed is not
    /// executable by the interpreter at all: it evaluates the placeholder and
    /// returns whatever that is. This makes that answerable before the call
    /// rather than after, when it looks like a type error in the contract.
    ///
    /// The repair this enables is safe to store, which is not obvious: contract
    /// definitions live in `metadata_table`, a side store, and never reach the
    /// MARF, so rewriting one changes no state root.
    #[must_use]
    pub fn contract_is_interpretable(&self, contract: &QualifiedContractIdentifier) -> bool {
        let key = format!("clr-meta::{contract}::vm-metadata::9::contract");
        self.side_store
            .prepare_cached("SELECT value FROM metadata_table WHERE key = ?1 LIMIT 1")
            .and_then(|mut statement| {
                statement.query_row(params![key], |row| row.get::<_, String>(0))
            })
            .is_ok_and(|stored| !stored.contains(r#""body":{"expr":{"LiteralValue":{"Int":0}}"#))
    }

    fn write_value(&self, value_hash: MarfValue, value: &str) -> Result<(), MarfStoreError> {
        self.side_store
            .prepare_cached("INSERT OR REPLACE INTO data_table (key, value) VALUES (?1, ?2)")?
            .execute(params![marf_value_key(value_hash), value])?;
        Ok(())
    }

    /// Read a value from a sealed state.
    #[must_use]
    pub fn get(&self, block: [u8; 32], key: &str) -> Option<String> {
        self.marf
            .get(block, key.as_bytes())
            .and_then(|value| self.data_from_side_store(value).ok().flatten())
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
        self.active.take().ok_or(MarfStoreError::NoActiveState)?;
        self.flush_metadata(block)?;
        self.read_block = Some(block);
        Ok(StateRoot(*root.as_bytes()))
    }

    /// Seal the active state under `block`, with everything that describes it.
    ///
    /// Two durability boundaries, in this order and no other: one side-store
    /// transaction carrying the block's Clarity metadata, its header and its
    /// caller's ledger, and then the MARF's own commit. The MARF commit is last
    /// and atomic, so it *is* the decision: the block exists exactly when
    /// everything beside it is already durable. A crash in between leaves rows
    /// for a block the MARF never got, which nothing can reach, because reaching
    /// them means standing on that block.
    pub fn commit_to(
        &mut self,
        block: [u8; 32],
        commit: &BlockCommit,
    ) -> Result<StateRoot, MarfStoreError> {
        if self.transaction.is_some() {
            return Err(MarfStoreError::TransactionInProgress);
        }
        if self.active.is_none() {
            return Err(MarfStoreError::NoActiveState);
        }
        self.prepare_commit(block, commit)?;
        // A failure here must leave the active state intact so the caller can
        // still abort it.
        let root = self.marf.seal_to(block)?;
        self.active.take().ok_or(MarfStoreError::NoActiveState)?;
        self.metadata.clear();
        self.read_block = Some(block);
        Ok(StateRoot(*root.as_bytes()))
    }

    /// Write everything the block owns outside the MARF, in one transaction.
    ///
    /// Committed with `synchronous = FULL`, for this one transaction only. The
    /// ordering between the two stores is only an ordering on disk if this half
    /// reaches it first: at the store's usual `NORMAL` a power cut could keep the
    /// MARF commit and lose the ledger row, which is exactly the sealed root with
    /// a ledger a block behind that committing them in this order prevents. One
    /// fsync per block buys it — left switched on, every Clarity value write, each
    /// its own autocommit and thousands to a block, would fsync too.
    fn prepare_commit(&self, block: [u8; 32], commit: &BlockCommit) -> Result<(), MarfStoreError> {
        let prepared = (|| -> Result<(), MarfStoreError> {
            self.side_store.execute_batch("PRAGMA synchronous = FULL")?;
            let transaction = self.side_store.unchecked_transaction()?;
            self.write_metadata(block)?;
            self.write_block_header(block, &commit.header)?;
            self.write_ledger(block, &commit.ledger)?;
            transaction.commit()?;
            Ok(())
        })();
        let restored = self.side_store.execute_batch("PRAGMA synchronous = NORMAL");
        prepared?;
        restored?;
        Ok(())
    }

    /// Move the block's Clarity metadata into the side store, keyed by the
    /// block that defined it, which is how a later block finds it again.
    fn flush_metadata(&mut self, block: [u8; 32]) -> Result<(), MarfStoreError> {
        let transaction = self.side_store.unchecked_transaction()?;
        self.write_metadata(block)?;
        transaction.commit()?;
        self.metadata.clear();
        Ok(())
    }

    fn write_metadata(&self, block: [u8; 32]) -> Result<(), MarfStoreError> {
        let blockhash = block_hex(block);
        for ((contract, key), value) in &self.metadata {
            self.side_store
                .prepare_cached(
                    "INSERT OR REPLACE INTO metadata_table (key, blockhash, value) \
                     VALUES (?1, ?2, ?3)",
                )?
                .execute(params![
                    format!("clr-meta::{contract}::{key}"),
                    &blockhash,
                    value
                ])?;
        }
        Ok(())
    }

    /// Discard the active state without registering a new MARF version.
    pub fn abort(&mut self) -> Result<(), MarfStoreError> {
        let active = self.active.take().ok_or(MarfStoreError::NoActiveState)?;
        self.marf.abort()?;
        self.metadata.clear();
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
            self.checkpoint_heights.entry(block).or_insert(height);
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

    /// Whether reads currently see the state being written.
    fn reads_active_state(&self) -> bool {
        self.active.is_some_and(|active| {
            self.read_block
                .is_none_or(|block| block == active.block)
        })
    }

    fn block_at_height(&self, block: [u8; 32], height: u32) -> Option<[u8; 32]> {
        if let Some(active) = self.active.filter(|active| active.block == block) {
            if active.height == height {
                return Some(block);
            }
            return active
                .parent
                .and_then(|parent| self.marf.block_at_height(parent, height));
        }
        self.marf.block_at_height(block, height)
    }

    fn current_block(&self) -> Option<[u8; 32]> {
        self.active.map(|active| active.block).or(self.read_block)
    }

    /// Resolve a key against whichever state reads are pointed at.
    fn value_of(&self, path: [u8; 32]) -> Option<MarfValue> {
        if self.reads_active_state() {
            return self.marf.get_active_path(path);
        }
        self.marf.get_path(self.read_block?, path)
    }

    fn data_from_side_store(&self, value: MarfValue) -> Result<Option<String>, VmExecutionError> {
        Ok(self
            .side_store
            .prepare_cached("SELECT value FROM data_table WHERE key = ?1")
            .and_then(|mut statement| {
                statement
                    .query_row(params![marf_value_key(value)], |row| row.get(0))
                    .optional()
            })
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
            .prepare_cached("SELECT value FROM metadata_table WHERE blockhash = ?1 AND key = ?2")
            .and_then(|mut statement| {
                statement
                    .query_row(
                        params![block_hex(block), format!("clr-meta::{contract}::{key}")],
                        |row| row.get(0),
                    )
                    .optional()
            })
            .map_err(|error| VmInternalError::Expect(format!("metadata read failed: {error}")))?)
    }
}

/// The files a durable store keeps in its directory.
const MARF_FILE: &str = "marf.sqlite";
const CLARITY_FILE: &str = "clarity.sqlite";

const SIDE_STORE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS data_table (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS metadata_table (
    key TEXT NOT NULL,
    blockhash TEXT,
    value TEXT NOT NULL,
    UNIQUE (key, blockhash)
);
CREATE INDEX IF NOT EXISTS md_blockhashes ON metadata_table(blockhash);
-- What Clarity may read about a block, for every block this node knows about.
-- Held here rather than in memory because a contract may ask about any
-- ancestor, including ones from before the checkpoint that this node never
-- executed, and because a map that grows with the chain is not a map.
CREATE TABLE IF NOT EXISTS block_header (
    block_id BLOB PRIMARY KEY,
    data BLOB NOT NULL
) WITHOUT ROWID;
-- The state a node keeps beside the MARF, as of the block that sealed it:
-- tenure accounting, tenure start heights, the executed suffix a reorganization
-- walks back over, the previous tenure's VRF proof. Written in the same
-- transaction as that block's header and metadata, and before the MARF commit.
--
-- A history rather than one row, because a restart may find its tip is no longer
-- on the chain and walk back to an ancestor the network still has: the ledger it
-- then stands on has to be the one that ancestor sealed.
CREATE TABLE IF NOT EXISTS chain_ledger (
    block_id BLOB PRIMARY KEY,
    sequence INTEGER NOT NULL,
    data BLOB NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS chain_ledger_sequence ON chain_ledger(sequence);
-- The Bitcoin block at each burn height. A fact about Bitcoin, not about any
-- Stacks block, so it is written as it is learned rather than with a seal.
CREATE TABLE IF NOT EXISTS burn_header (
    height INTEGER PRIMARY KEY,
    hash BLOB NOT NULL
) WITHOUT ROWID;
";

/// How many blocks of ledger history the side store keeps.
///
/// `nano-node` looks this far back for a tip the network still has, so a walk
/// that goes further has nothing to stand on anyway.
const LEDGER_HISTORY: u32 = 256;

fn create_side_store() -> Result<rusqlite::Connection, rusqlite::Error> {
    let connection = rusqlite::Connection::open_in_memory()?;
    connection.execute_batch(SIDE_STORE_SCHEMA)?;
    Ok(connection)
}

fn open_side_store(path: &Path) -> Result<rusqlite::Connection, rusqlite::Error> {
    open_side_store_with_journal(path, true)
}

/// The side store while a checkpoint is being imported into it.
///
/// 369,694,685 values arrive in one transaction, and under WAL that grows a
/// log which cannot be checkpointed until the end — after which every page
/// lookup searches it and throughput decays. Journalling buys nothing during an
/// import that is discarded if it does not finish.
fn open_side_store_for_import(path: &Path) -> Result<rusqlite::Connection, rusqlite::Error> {
    open_side_store_with_journal(path, false)
}

fn open_side_store_with_journal(
    path: &Path,
    journal: bool,
) -> Result<rusqlite::Connection, rusqlite::Error> {
    let connection = rusqlite::Connection::open(path)?;
    let mode = if journal { "WAL" } else { "OFF" };
    connection.query_row(&format!("PRAGMA journal_mode = {mode}"), [], |_| Ok(()))?;
    if !journal {
        connection.execute_batch("PRAGMA synchronous = OFF;")?;
    }
    // Importing a mainnet checkpoint copies 369,694,685 values keyed by a hex
    // string, in the source's row order rather than key order, so every insert
    // lands somewhere else in the destination's B-tree. The same two megabytes
    // of default page cache that slowed the MARF slows this more.
    connection.execute_batch(
        "PRAGMA synchronous = NORMAL;
         PRAGMA cache_size = -1000000;
         PRAGMA temp_store = MEMORY;",
    )?;
    connection.execute_batch(SIDE_STORE_SCHEMA)?;
    Ok(connection)
}

/// Copy the Clarity values a checkpoint's trie refers to, in `SQLite`, so no
/// row crosses into this process on the way.
///
/// A mainnet checkpoint carries 369,694,685 values, nearly all of them
/// history: a node starting at one height needs only what the trie at that
/// height refers to. A leaf record ends with its forty-byte `MarfValue`, and
/// the side store is keyed by that value's hex, so the set can be taken
/// straight out of the imported nodes without decoding one.
///
/// `metadata_table` is copied whole. It holds contract analyses rather than
/// per-key state, so it is small beside the values and its keys cannot be read
/// off the trie the same way.
fn import_side_store(
    destination: &rusqlite::Connection,
    checkpoint: &Path,
    marf: Option<&Path>,
) -> Result<(), MarfStoreError> {
    destination.execute(
        "ATTACH DATABASE ?1 AS checkpoint",
        params![format!("file:{}?immutable=1", checkpoint.display())],
    )?;
    // Without a MARF on disk to read the leaves from — an in-memory import,
    // which is only ever given a small checkpoint — take the values whole.
    let Some(marf) = marf else {
        let copied = destination.execute_batch(
            "INSERT OR REPLACE INTO data_table (key, value) \
                 SELECT key, value FROM checkpoint.data_table;
             INSERT OR REPLACE INTO metadata_table (key, blockhash, value) \
                 SELECT key, blockhash, value FROM checkpoint.metadata_table;",
        );
        destination.execute_batch("DETACH DATABASE checkpoint")?;
        return Ok(copied?);
    };
    destination.execute(
        "ATTACH DATABASE ?1 AS imported",
        params![format!("file:{}?mode=ro", marf.display())],
    )?;
    let copied = destination.execute_batch(
        "CREATE TEMP TABLE needed_value (key TEXT PRIMARY KEY) WITHOUT ROWID;
         INSERT OR IGNORE INTO needed_value (key)
             SELECT lower(hex(substr(data, -40))) FROM imported.marf_node
             WHERE substr(data, 1, 1) = x'00';
         INSERT OR REPLACE INTO data_table (key, value)
             SELECT key, value FROM checkpoint.data_table
             WHERE key IN (SELECT key FROM needed_value);
         INSERT OR REPLACE INTO metadata_table (key, blockhash, value)
             SELECT key, blockhash, value FROM checkpoint.metadata_table;
         DROP TABLE needed_value;",
    );
    destination.execute_batch("DETACH DATABASE checkpoint")?;
    destination.execute_batch("DETACH DATABASE imported")?;
    Ok(copied?)
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
        Some(&pox::handle_contract_call)
    }

    fn put_all_data(&mut self, items: Vec<(String, String)>) -> Result<(), VmExecutionError> {
        for (key, value) in items {
            self.put(&key, &value)
                .map_err(|error| VmInternalError::Expect(error.to_string()))?;
        }
        Ok(())
    }

    fn get_data(&mut self, key: &str) -> Result<Option<String>, VmExecutionError> {
        let found = self
            .get_data_from_path(&ReferenceTrieHash(*nano_marf::key_path(key.as_bytes()).as_bytes()));
        // A value that is wrong rather than missing is only visible by reading
        // it, so this says what every read answered.
        if std::env::var_os("NANO_TRACE_READS").is_some() {
            match &found {
                Ok(Some(value)) => println!("read {key} = {value}"),
                Ok(None) => println!("read {key} = <none>"),
                Err(error) => println!("read {key} failed: {error}"),
            }
        }
        found
    }

    fn get_data_from_path(
        &mut self,
        path: &ReferenceTrieHash,
    ) -> Result<Option<String>, VmExecutionError> {
        let Some(value) = self.value_of(*path.as_bytes()) else {
            return Ok(None);
        };
        let found = self.data_from_side_store(value)?;
        if found.is_none() {
            // The trie names a value the side store does not hold, which is a
            // different fault from a key the trie never had — and only this
            // one means the import dropped something it needed.
            eprintln!(
                "the trie names value {} but the side store has no such row",
                hex::encode(value.as_bytes())
            );
        }
        Ok(found)
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
        if !self.marf.contains(block.0)
            && self.active.is_none_or(|active| active.block != block.0)
        {
            return Err(RuntimeError::UnknownBlockHeaderHash(BlockHeaderHash(block.0)).into());
        }
        let previous = self.current_block().unwrap_or([0; 32]);
        self.read_block = Some(block.0);
        Ok(StacksBlockId(previous))
    }

    fn get_block_at_height(&mut self, height: u32) -> Option<StacksBlockId> {
        let found = self
            .current_block()
            .and_then(|block| self.block_at_height(block, height))
            .map(StacksBlockId);
        if found.is_none() && std::env::var_os("NANO_TRACE_WRITES").is_some() {
            println!("no block at height {height}");
        }
        found
    }

    /// The height of the block being executed.
    ///
    /// The two branches disagreed: an active store already records the height
    /// of the block it opened, where a sealed block has to be counted from. The
    /// extra increment made every contract reading the height see one more than
    /// the network did — mainnet stored 8,665,699 where nano stored 8,665,700,
    /// so the receipt matched and the state root did not.
    fn get_current_block_height(&mut self) -> u32 {
        self.active
            .map(|active| active.height)
            .or_else(|| {
                self.current_block()
                    .and_then(|block| self.height_of(block))
                    .map(|height| height + 1)
            })
            .unwrap_or(0)
    }

    fn get_open_chain_tip_height(&mut self) -> u32 {
        self.active.map_or(0, |active| active.height)
    }

    fn get_open_chain_tip(&mut self) -> StacksBlockId {
        StacksBlockId(self.active.map_or([0; 32], |active| active.block))
    }

    fn get_contract_hash(
        &mut self,
        contract: &QualifiedContractIdentifier,
    ) -> Result<(StacksBlockId, Sha512Trunc256Sum), VmExecutionError> {
        let commitment = self
            .get_data(&make_contract_hash_key(contract))?
            .map(|value| ContractCommitment::deserialize(&value))
            .transpose()?
            .ok_or_else(|| {
                // Clarity's analysis loader swallows this with `.ok()` and
                // reports the contract as unresolved, so without saying it here
                // a missing commitment is indistinguishable from a contract
                // that was never deployed.
                //
                // Behind a switch because it is not rare: every deployment that
                // names a contract this node does not hold asks once per
                // analysis pass, and a replay that retries a block emits it
                // thousands of times — which buries the divergence the log is
                // being read for.
                if std::env::var_os("NANO_TRACE_MISSING_CONTRACTS").is_some() {
                    eprintln!(
                        "no contract commitment for {contract} at key {}",
                        make_contract_hash_key(contract)
                    );
                }
                VmInternalError::Expect(format!("unknown contract {contract}"))
            })?;
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
        if self.active.is_none() {
            return Err(
                VmInternalError::Expect("metadata write without an active state".to_owned()).into(),
            );
        }
        self.metadata
            .insert((contract.to_string(), key.to_owned()), value.to_owned());
        Ok(())
    }

    fn get_metadata(
        &mut self,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> Result<Option<String>, VmExecutionError> {
        if let Some(value) = self.pending_metadata(contract, key) {
            return Ok(Some(value));
        }
        let (block, _) = self.get_contract_hash(contract)?;
        let found = self.metadata_from_side_store(block.0, contract, key)?;
        if found.is_none() {
            // A miss here is indistinguishable from a contract that never
            // wrote the key, and the two need different fixes: say which block
            // was consulted so an imported checkpoint can be checked against
            // the block its metadata actually landed under.
            eprintln!(
                "no {key} for {contract} under block {}",
                hex::encode(block.0)
            );
        }
        Ok(found)
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
        if self.active.is_some_and(|active| active.block == block)
            && let Some(value) = self.pending_metadata(contract, key)
        {
            return Ok(Some(value));
        }
        self.metadata_from_side_store(block, contract, key)
    }
}

impl MarfStore {
    /// Metadata the block being executed has written but not yet sealed.
    fn pending_metadata(
        &self,
        contract: &QualifiedContractIdentifier,
        key: &str,
    ) -> Option<String> {
        if !self.reads_active_state() {
            return None;
        }
        self.metadata
            .get(&(contract.to_string(), key.to_owned()))
            .cloned()
    }
}

/// Publish a versioned Clarity contract in an active MARF-backed state.
/// Every contract a contract's source names, so their modules can be built
/// before the call that needs them rather than by failing it.
#[must_use]
pub fn referenced_contracts(
    contract: &QualifiedContractIdentifier,
    source: &str,
    version: ClarityVersion,
) -> Vec<QualifiedContractIdentifier> {
    // `build_ast` rather than `ast::parse`: the latter is a testing-gated
    // wrapper around exactly this call, and the `testing` feature it needs would
    // reach the whole build graph.
    let Ok(parsed) = build_ast(
        contract,
        source,
        &mut LimitedCostTracker::new_free(),
        version,
        epoch_for_version(version),
    ) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    collect_contracts(&parsed.expressions, contract, &mut found);
    found
}

fn collect_contracts(
    expressions: &[SymbolicExpression],
    within: &QualifiedContractIdentifier,
    found: &mut Vec<QualifiedContractIdentifier>,
) {
    for expression in expressions {
        match &expression.expr {
            clarity::vm::representations::SymbolicExpressionType::List(list) => {
                collect_contracts(list, within, found);
            }
            clarity::vm::representations::SymbolicExpressionType::LiteralValue(
                Value::Principal(PrincipalData::Contract(identifier)),
            ) if identifier != within && !found.contains(identifier) => {
                found.push(identifier.clone());
            }
            _ => {}
        }
    }
}

/// Compile a stored contract's source under one epoch.
fn compile_under(
    store: &mut MarfStore,
    contract: &QualifiedContractIdentifier,
    source: &str,
    version: ClarityVersion,
    epoch: StacksEpochId,
) -> Result<clar2wasm::CompiledContract, VmExecutionError> {
    let mut analysis = AnalysisDatabase::new(store);
    analysis
        .execute::<_, _, StaticCheckError>(|analysis_db| {
            Ok(clar2wasm::compile_for_cost_epoch(
                source,
                contract,
                LimitedCostTracker::new_free(),
                version,
                epoch,
                // Whatever epoch accepts the contract, the chain charges it at
                // the rate it is running at.
                StacksEpochId::Epoch40,
                analysis_db,
                true,
            )
            .map(clar2wasm::CompileResult::into_compiled_contract)
            .map_err(|error: clar2wasm::CompileError| {
                // Name the contract: a compile failure surfaces at whatever
                // call needed the module, which is routinely a different
                // contract, and chasing that down took a day once already.
                StaticCheckErrorKind::Unreachable(format!(
                    "{contract}: {}",
                    wasm_compile_error(error)
                ))
            })?)
        })
        .map_err(|error: StaticCheckError| VmInternalError::Expect(error.to_string()).into())
}

/// The epoch a contract of this Clarity version was first deployable in.
///
/// Recompiling on demand has to reconstruct what the network already accepted,
/// not re-judge it. Compiling everything as epoch 4.0 rejects any contract
/// using a word later epochs removed — `at-block`, which 3.4 dropped and which
/// mainnet contracts written before it still use and still run, because
/// stacks-core stores the analysis rather than redoing it.
///
/// The cost table this bakes in is the deploy epoch's rather than the current
/// one's. Costs are not in a block's state root, so a replay still matches;
/// receipts for such contracts may not.
#[must_use]
pub const fn epoch_for_version(version: ClarityVersion) -> StacksEpochId {
    match version {
        ClarityVersion::Clarity1 => StacksEpochId::Epoch20,
        ClarityVersion::Clarity2 => StacksEpochId::Epoch21,
        ClarityVersion::Clarity3 => StacksEpochId::Epoch30,
        ClarityVersion::Clarity4 => StacksEpochId::Epoch33,
        ClarityVersion::Clarity5 => StacksEpochId::Epoch34,
        ClarityVersion::Clarity6 => StacksEpochId::Epoch40,
    }
}

/// The source a deployed contract was published with, and its Clarity version.
pub fn contract_source(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    contract: &QualifiedContractIdentifier,
) -> Result<(String, ClarityVersion), VmExecutionError> {
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
            Ok(value)
        }
        Err(error) => {
            database.roll_back()?;
            Err(error)
        }
    }
}

/// Compile a contract and everything its source names, before the call runs.
///
/// A module that turns out to be missing is otherwise only discovered by
/// running the whole call and failing, so a transaction reaching twenty-three
/// contracts ran twenty-three times — and one reaching more than
/// `MISSING_MODULE_ATTEMPTS` failed for want of attempts rather than for any
/// reason of its own.
///
/// One level, and iterative: contracts reference each other in cycles, and a
/// contract is not in the cache until it has finished compiling, so following
/// references as they are found recurses until the stack is gone.
fn ensure_wasm_module_and_references(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    modules: &mut ModuleCache,
    contract: &QualifiedContractIdentifier,
) -> Result<(), VmExecutionError> {
    let referenced = contract_source(store, bitcoin_context, contract)
        .map(|(source, version)| referenced_contracts(contract, &source, version))
        .unwrap_or_default();

    for referenced in referenced {
        if modules.get(&referenced).is_none() {
            // A contract that will not compile is the caller's problem to
            // report, not a reason to give up before running anything.
            let _ = ensure_wasm_module(store, bitcoin_context, modules, &referenced);
        }
    }
    ensure_wasm_module(store, bitcoin_context, modules, contract)
}

/// Compile a contract a call needs, reporting a bad contract as a failed call.
fn needed_module(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    modules: &mut ModuleCache,
    contract: &QualifiedContractIdentifier,
    cost_tracker: &LimitedCostTracker,
) -> Result<Option<ContractCallOutcome>, VmExecutionError> {
    match ensure_wasm_module_and_references(store, bitcoin_context, modules, contract) {
        Ok(()) => Ok(None),
        Err(error) if reports_analysis_failure(&error) => {
            Ok(Some(ContractCallOutcome::RuntimeFailure {
                cost: cost_tracker.get_total(),
                error,
            }))
        }
        Err(error) => Err(error),
    }
}

/// Marks a compile failure as the contract's fault rather than the node's.
///
/// stacks-core turns a static check that is not `rejectable_in_epoch` into a
/// transaction receipt and carries on; only `Unreachable` and a few others stop
/// the block. `clar2wasm` reports every failure as one diagnostic-carrying
/// variant, so without a mark of our own the two are indistinguishable and a
/// contract naming one that does not exist yet — an ordinary failed deployment
/// on mainnet — stops a node dead.
const ANALYSIS_FAILED: &str = "contract analysis failed";

/// Whether a failure means the compiler cannot run this contract.
///
/// Either it refused the source, or it produced a module that will not load.
/// Both are the compiler's business rather than the node's, and both leave the
/// interpreter — which needs no module — able to answer.
#[must_use]
pub fn is_contract_analysis_failure(error: &ClarityEvalError) -> bool {
    reports_analysis_failure(error)
}

/// The same question of a message, for callers holding the text rather than
/// the error — a deploy that crossed an engine boundary is one.
#[must_use]
pub fn is_contract_analysis_failure_message(message: &str) -> bool {
    reports_analysis_failure(&message)
}

/// The same question of anything that can say what went wrong.
fn reports_analysis_failure(error: &impl std::fmt::Display) -> bool {
    let text = error.to_string();
    text.contains(ANALYSIS_FAILED) || text.contains("UnableToLoadModule")
}

/// Type-check a deployment and store its analysis, as stacks-core does.
pub fn analyse_for_deployment(
    store: &mut MarfStore,
    contract: &QualifiedContractIdentifier,
    version: ClarityVersion,
    source: &str,
    cost_tracker: &LimitedCostTracker,
) -> Result<(), ClarityEvalError> {
    let mut analysis = AnalysisDatabase::new(store);
    analysis
        .execute::<_, _, StaticCheckError>(|analysis_db| {
            let expressions = build_ast(
                contract,
                source,
                &mut LimitedCostTracker::new_free(),
                version,
                StacksEpochId::Epoch40,
            )
            .map_err(|error| StaticCheckErrorKind::Unreachable(error.to_string()))?
            .expressions;
            clarity::vm::analysis::run_analysis(
                contract,
                &expressions,
                analysis_db,
                true,
                cost_tracker.clone(),
                StacksEpochId::Epoch40,
                version,
                false,
                clarity::vm::resource_limiter::ResourceLimiter::unlimited(),
            )
            .map_err(|boxed| boxed.0)?;
            Ok(())
        })
        .map_err(|error: StaticCheckError| {
            ClarityEvalError::from(VmExecutionError::Internal(VmInternalError::Expect(format!(
                "{ANALYSIS_FAILED}: {error}"
            ))))
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
                // Inside the analysis bracket on purpose: a module wasmtime
                // will refuse must take its contract analysis back out with it,
                // or the deploy leaves an analysis behind for a contract that
                // was never stored, and the interpreter fallback below deploys
                // on top of it.
                Ok(
                    loadable(&contract, compiled.into_compiled_contract(), modules.engine())
                        .map_err(|error| StaticCheckErrorKind::Unreachable(error.to_string()))?,
                )
            })
            .map_err(|error: StaticCheckError| {
                ClarityEvalError::from(VmExecutionError::Internal(VmInternalError::Expect(
                    error.to_string(),
                )))
            })?;
        compiled
    };

    // A contract's top-level expressions run at deploy time and may call other
    // contracts, whose modules have to be built first. Only the *call* path did
    // this, so a deploy that calls anything died with the callee reported
    // missing — and against mainnet a block of dependent deploys is routine.
    for referenced in referenced_contracts(&contract, source, version) {
        if modules.get(&referenced).is_none() {
            // A contract that will not compile is the caller's problem to
            // report, not a reason to give up before deploying anything.
            let _ = ensure_wasm_module(store, bitcoin_context, modules, &referenced);
        }
    }

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

struct ContractCall<'a> {
    sender: PrincipalData,
    sponsor: Option<PrincipalData>,
    contract: QualifiedContractIdentifier,
    function: &'a str,
    arguments: &'a [Vec<u8>],
}

fn execute_contract_call_outcome_with_wasm_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    modules: &mut ModuleCache,
    call: &ContractCall<'_>,
    cost_tracker: &LimitedCostTracker,
) -> Result<ContractCallOutcome, VmExecutionError> {
    // One engine, and no route around it. A module clarity-wasm will not build,
    // a trap and a host failure all reject the candidate: answering them another
    // way means the chain advances on results the consensus engine never
    // produced. The reference interpreter is a differential oracle and lives in
    // `nano-oracle`, which nothing this crate can reach depends on.
    wasm_outcome(store, bitcoin_context, modules, call, cost_tracker)
}

fn wasm_outcome(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    modules: &mut ModuleCache,
    call: &ContractCall<'_>,
    cost_tracker: &LimitedCostTracker,
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
        &call.sender,
        call.sponsor.as_ref(),
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
    sender: &PrincipalData,
    sponsor: Option<&PrincipalData>,
    contract: &QualifiedContractIdentifier,
    function: &str,
    arguments: &[Value],
    cost_tracker: &LimitedCostTracker,
) -> Result<ContractCallOutcome, VmExecutionError> {
    // A contract principal passed as an argument is compiled ahead of the call,
    // because a trait dispatch will need it and the call cannot say in advance
    // which. But the network does not require such an argument to name a
    // contract that exists — it fails only if the call actually invokes it. So
    // one that cannot be compiled is left alone rather than failing the call:
    // mainnet passes `.native-pool-signer`, which is not deployed anywhere, and
    // gets a value back.
    for argument in arguments.iter().filter_map(contract_argument) {
        let _ = needed_module(store, bitcoin_context, modules, argument, cost_tracker);
    }
    if let Some(failed) = needed_module(store, bitcoin_context, modules, contract, cost_tracker)? {
        return Ok(failed);
    }
    // A `contract-call?` reaches a contract this node never compiled — the
    // checkpoint carried its source and its state, not a module, and which
    // contracts a call reaches is not knowable in advance when a trait
    // decides it. So compile what the run turns out to want and run it
    // again: the failed attempt rolled its database back, so the retry
    // starts where the first one did.
    let mut attempts = 0;
    loop {
        match call_compiled_contract(
            store,
            bitcoin_context,
            modules,
            sender.clone(),
            sponsor.cloned(),
            contract,
            function,
            arguments,
            cost_tracker.clone(),
        ) {
            // A module the compiler produced but wasmtime will not load is
            // the compiler's failure, not the node's, and the interpreter can
            // still run the contract — so it becomes a failed call rather than
            // a stopped node, like a source the compiler refuses.
            Err(error) if reports_analysis_failure(&error) => {
                return Ok(ContractCallOutcome::RuntimeFailure {
                    cost: cost_tracker.get_total(),
                    error,
                });
            }
            Err(error) => {
                let Some(missing) = missing_compiled_contract(&error)
                    .filter(|_| attempts < MISSING_MODULE_ATTEMPTS)
                else {
                    return Err(error);
                };
                attempts += 1;
                // A contract that cannot be compiled is not the reason the
                // run stopped — report what stopped it, not what the repair
                // ran into. Say what the repair ran into all the same, because
                // otherwise the run just keeps failing for its original reason
                // and never says why the repair could not help.
                match ensure_wasm_module(store, bitcoin_context, modules, &missing) {
                    Ok(()) => {}
                    // A contract that will not compile is the contract's fault,
                    // and a call into one fails like any other failing call.
                    // Only a node fault stops the node.
                    Err(repair) if reports_analysis_failure(&repair) => {
                        return Ok(ContractCallOutcome::RuntimeFailure {
                            cost: cost_tracker.get_total(),
                            error: repair,
                        });
                    }
                    Err(repair) => {
                        eprintln!("compiling {missing} on demand failed: {repair}");
                        return Err(error);
                    }
                }
            }
            outcome => return outcome,
        }
    }
}

/// How many contracts one call may turn out to need compiling.
const MISSING_MODULE_ATTEMPTS: usize = 64;

/// The contract a run stopped for want of, if that is why it stopped.
/// The contract a run stopped for want of, whichever way the VM said so.
///
/// A call into a contract whose module is not loaded is reported two ways: the
/// store says it has no compiled contract, and the linker says the contract is
/// unresolved. Recognising only the first leaves the second fatal, and against
/// mainnet that is any contract called by a contract — the second hop is where
/// the linker, not the store, notices.
fn missing_compiled_contract(error: &VmExecutionError) -> Option<QualifiedContractIdentifier> {
    let text = error.to_string();
    let rest = text
        .split_once("compiled contract ")
        .or_else(|| text.split_once("unresolved contract "))?
        .1;
    let name = rest
        .trim_start_matches(['\'', '"'])
        .split(['\'', '"', ')', '\\'])
        .next()?
        .trim();
    QualifiedContractIdentifier::parse(name).ok()
}

#[allow(clippy::too_many_arguments)]
fn call_compiled_contract(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    modules: &ModuleCache,
    sender: PrincipalData,
    sponsor: Option<PrincipalData>,
    contract: &QualifiedContractIdentifier,
    function: &str,
    arguments: &[Value],
    cost_tracker: LimitedCostTracker,
) -> Result<ContractCallOutcome, VmExecutionError> {
    let network = store.network();
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
    // A call from outside a contract is its own caller: `contract-caller`
    // reads as the sender until a `contract-call?` moves it. Leaving it unset
    // made every boot function that reads it fail with `NoCallerInContext`,
    // which is what a mainnet node hit applying its first block.
    let result = clar2wasm::initialize::call_function(
        function,
        arguments,
        module,
        &mut global,
        &contract_context,
        &mut call_stack,
        Some(sender.clone()),
        Some(sender),
        // `tx-sponsor?` is a consensus-visible read, and a sponsored contract
        // call that answers `none` here answers something the chain does not.
        sponsor,
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

/// The module cache a store on disk keeps its native code in.
///
/// Cranelift's output goes beside the databases, so a restart does not compile
/// every contract it touches all over again. An in-memory store — a test, a
/// checkpoint being read — has nowhere to put it and does without.
fn native_module_cache(side_store: Option<&Path>) -> ModuleCache {
    side_store
        .and_then(Path::parent)
        .and_then(|directory| nano_wasm_cache::NativeModuleCache::open(directory).ok())
        .map_or_else(ModuleCache::default, |cache| {
            ModuleCache::persisting_in(Arc::new(cache))
        })
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

    let (source, version) = contract_source(store, bitcoin_context, contract)?;
    // The current epoch first, so the costs baked in are the ones the chain
    // charges now. Only a contract it rejects — one using a word a later epoch
    // removed — is rebuilt under the epoch it was deployable in.
    let engine = modules.engine().clone();
    let compiled = match compile_under(store, contract, &source, version, StacksEpochId::Epoch40)
        .and_then(|compiled| loadable(contract, compiled, &engine))
    {
        Ok(compiled) => compiled,
        Err(rejected) => {
            // Worth saying: a contract built under an older epoch is charged
            // that epoch's costs, and its receipts will not match the network's.
            // Newest first: a contract keeps the semantics of the epoch it was
            // written for, and the newest epoch that still accepts it is the
            // closest guess at that — where the oldest its version allows sends
            // a Clarity 1 contract back to epoch 2.0, whose code paths are the
            // least exercised in the compiler.
            let (epoch, compiled) = ACCEPTING_EPOCHS
                .iter()
                .rev()
                .filter(|epoch| **epoch >= epoch_for_version(version))
                .find_map(|epoch| {
                    compile_under(store, contract, &source, version, *epoch)
                        .and_then(|compiled| loadable(contract, compiled, &engine))
                        .ok()
                        .map(|compiled| (*epoch, compiled))
                })
                .ok_or(rejected)?;
            eprintln!("{contract} does not compile under epoch 4.0, rebuilt as {epoch:?}");
            compiled
        }
    };
    modules.insert(contract.clone(), compiled);
    Ok(())
}

/// Reject a module the runtime would refuse to load.
///
/// A contract can compile and still emit wasm wasmtime rejects — nothing checks
/// the two agree, and the failure otherwise surfaces at the call, naming the
/// contract that was *called* rather than the one whose module is bad. Checking
/// here names the culprit and lets the epoch retry above look for a build that
/// does load, which is the difference between a stalled replay and a running
/// one. The engine is the one clar2wasm instantiates with, so this is the same
/// judgement the runtime would make.
fn loadable(
    contract: &QualifiedContractIdentifier,
    compiled: CompiledContract,
    engine: &wasmtime::Engine,
) -> Result<CompiledContract, VmExecutionError> {
    if let Err(error) = wasmtime::Module::validate(engine, &compiled.wasm) {
        // A module the runtime refuses can only be read as bytes, so leave them
        // somewhere a disassembler can reach when asked.
        if let Some(path) = std::env::var_os("NANO_DUMP_REFUSED_WASM") {
            let _ = std::fs::write(path, &compiled.wasm);
        }
        return Err(VmInternalError::Expect(format!(
            "{ANALYSIS_FAILED}: {contract} compiles to a module that will not load: {error}"
        ))
        .into());
    }
    Ok(compiled)
}

fn wasm_compile_error(error: clar2wasm::CompileError) -> String {
    match error {
        clar2wasm::CompileError::Generic { diagnostics, .. } => {
            let joined = diagnostics
                .into_iter()
                .map(|diagnostic| diagnostic.message)
                .collect::<Vec<_>>()
                .join("; ");
            format!("{ANALYSIS_FAILED}: {joined}")
        }
    }
}

#[must_use]
pub fn is_acceptable_runtime_failure(error: &VmExecutionError) -> bool {
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
        // `.lockup` lives at the boot address of the chain being executed:
        // hardcoding the testnet one made a mainnet node look for a contract
        // that is not there.
        let lockup_contract = clarity::boot_util::boot_code_id("lockup", network.is_mainnet());
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

/// What an account holds, locked and unlocked, and when the lock lifts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccountState {
    pub unlocked: u128,
    pub locked: u128,
    pub unlock_height: u64,
    pub nonce: u64,
}

/// Read an account's whole position straight from the state.
///
/// `stx-account` is the Clarity view of this, and evaluating it would need an
/// execution engine to answer an RPC — which is not a thing a node with one
/// consensus engine should do for a read the database already holds. These are
/// the same three reads `special_stx_account` makes, in the same order.
fn account_state_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    principal: &PrincipalData,
) -> Result<AccountState, VmExecutionError> {
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
        let balance = global
            .database
            .get_stx_balance_snapshot(principal)?
            .canonical_balance_repr()?;
        let unlock_height = balance.effective_unlock_height(
            global.database.get_v1_unlock_height(),
            global.database.get_v2_unlock_height()?,
            global.database.get_v3_unlock_height()?,
            global.database.get_v4_unlock_height()?,
        );
        Ok(AccountState {
            unlocked: balance.amount_unlocked(),
            locked: balance.amount_locked(),
            unlock_height,
            nonce: global.database.get_account_nonce(principal)?,
        })
    })
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

pub fn clarity_database<'a>(
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

    use super::{BlockCommit, BlockHeader};
    use stacks_common::codec::StacksMessageCodec;
    use stacks_common::types::chainstate::StacksBlockId;

    use super::{ContractCallOutcome, MarfStore, Vm};
    use clarity::vm::costs::LimitedCostTracker;

    /// Read a Clarity expression the only way this node evaluates anything:
    /// through a deployed contract, compiled and run by clarity-wasm.
    fn read_through_a_contract(vm: &mut Vm, name: &str, body: &str) -> Value {
        let contract = QualifiedContractIdentifier::parse(&format!(
            "ST000000000000000000002AMW42H.{name}"
        ))
        .expect("a contract identifier");
        vm.deploy_contract(
            contract.clone(),
            ClarityVersion::Clarity6,
            &format!("(define-read-only (answer) {body})"),
            LimitedCostTracker::new_free(),
        )
        .expect("deploy the reader");
        let sender: PrincipalData = contract.issuer.clone().into();
        vm.call_contract_values(&sender, &contract, "answer", &[])
            .expect("read through the reader")
    }




    #[test]
    fn marf_store_keeps_forked_values_and_roots() {
        let first = [1; 32];
        let second = [2; 32];
        let fork = [3; 32];
        let mut store = MarfStore::new(Network::TESTNET).expect("create MARF store");

        store.begin(None, first).expect("begin first state");
        store
            .put("counter", "one")
            .expect("write first state");
        let pending_first_root = store.pending_root().expect("derive first state root");
        let first_root = store.seal().expect("seal first state");
        assert_eq!(pending_first_root.as_bytes(), &first_root.0);

        store
            .begin(Some(first), second)
            .expect("begin second state");
        store
            .put("counter", "two")
            .expect("write second state");
        let second_root = store.seal().expect("seal second state");

        store.begin(Some(first), fork).expect("begin fork state");
        store
            .put("counter", "fork")
            .expect("write fork state");
        let fork_root = store.seal().expect("seal fork state");

        assert_eq!(store.get(first, "counter").as_deref(), Some("one"));
        assert_eq!(store.get(second, "counter").as_deref(), Some("two"));
        assert_eq!(store.get(fork, "counter").as_deref(), Some("fork"));
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
    fn credits_liquid_stx_without_a_transaction_event() {
        let principal =
            PrincipalData::parse("ST000000000000000000002AMW42H").expect("valid principal");
        let mut vm = Vm::new(Network::TESTNET).expect("create VM");
        vm.begin_block(None, [1; 32]).expect("begin checkpoint");
        vm.record_block_header([1; 32], BlockHeader::default())
            .expect("record a header");
        vm.seal_block().expect("seal checkpoint");
        vm.begin_block(Some([1; 32]), [2; 32])
            .expect("begin successor");
        vm.record_block_header([2; 32], BlockHeader::default())
            .expect("record a header");
        vm.credit_stx(&principal, 42).expect("credit STX");

        let value = read_through_a_contract(
            &mut vm,
            "balance",
            "(stx-get-balance 'ST000000000000000000002AMW42H)",
        );

        assert_eq!(value, Value::UInt(42));
    }


    #[test]
    fn transaction_rollback_restores_active_state() {
        let block = [1; 32];
        let mut store = MarfStore::new(Network::TESTNET).expect("create MARF store");
        store.begin(None, block).expect("begin block");
        store
            .put("counter", "one")
            .expect("write baseline value");

        store.begin_transaction().expect("begin transaction");
        store
            .put("counter", "two")
            .expect("write transactional value");
        store.rollback_transaction().expect("roll back transaction");
        store.seal().expect("seal block");

        assert_eq!(store.get(block, "counter").as_deref(), Some("one"));
    }

    /// A block executes at its own height, not one past it.
    ///
    /// Every contract reading the height saw one more than the network did:
    /// mainnet stored 8,665,699 for a block nano stored 8,665,700 for, so the
    /// receipt matched, the costs matched, and only the state root did not.
    #[test]
    fn a_block_executes_at_its_own_height() {
        let mut vm = Vm::new(Network::TESTNET).expect("create VM");
        vm.begin_block(None, [1; 32]).expect("begin genesis");
        vm.seal_block().expect("seal genesis");
        vm.begin_block(Some([1; 32]), [2; 32]).expect("begin child");
        vm.seal_block().expect("seal child");
        vm.begin_block(Some([2; 32]), [3; 32])
            .expect("begin grandchild");

        let height = read_through_a_contract(&mut vm, "height", "stacks-block-height");

        // Genesis is 0, its child 1, this one 2.
        assert_eq!(height, Value::UInt(2));
    }






    /// A header a node recorded is still there after it restarts.
    ///
    /// They lived in a map that only held blocks this process had executed, so
    /// every block before the checkpoint — and every block before the last
    /// restart — read back as `none`, and a contract consulting chain history
    /// got an answer the network never gave.
    #[test]
    fn a_recorded_header_survives_a_restart() {
        let directory = tempfile::tempdir().expect("a directory");
        let header = BlockHeader {
            burn_header_hash: [0x5b; 32],
            burn_block_height: 960_232,
            burn_block_time: 1_785_400_701,
            stacks_block_time: 1_785_402_038,
            block_header_hash: [0x2c; 32],
            consensus_hash: [0x1d; 20],
            vrf_seed: [0x3e; 32],
            miner_address: (22, [0x4f; 20]),
            burn_spend_total: 1_234_567,
            burn_spend_winner: 89_012,
            block_reward: 1_000_000_000,
            tenure_height: 251_321,
            tenure_start_height: 8_665_600,
        };

        {
            let mut vm = Vm::open(Network::MAINNET, directory.path()).expect("open");
            vm.record_block_header([7; 32], header)
                .expect("record a header");
        }

        let vm = Vm::open(Network::MAINNET, directory.path()).expect("reopen");
        assert_eq!(vm.recorded_header([7; 32]), Some(header));
    }

    /// A contract answers the same whether its native code was compiled now or
    /// read back from the state directory.
    ///
    /// That equality is the whole safety claim of the persistent module cache:
    /// the middle call runs code the previous process compiled, and the last one
    /// runs code compiled from scratch after the cache was deleted underneath
    /// it. A cache that ever answered differently would be a consensus bug.
    #[test]
    fn a_cached_module_answers_what_a_compiled_one_does() {
        let directory = tempfile::tempdir().expect("a directory");
        let entries = directory.path().join("native-modules").join("1");
        let contract =
            QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.counter")
                .expect("a contract identifier");
        let sender: PrincipalData = contract.issuer.clone().into();
        let ask = |vm: &mut Vm| {
            vm.call_contract_values(&sender, &contract, "answer", &[])
                .expect("call the contract")
        };
        let count = || {
            std::fs::read_dir(&entries)
                .expect("the cache directory")
                .count()
        };

        let compiled = {
            let mut vm = Vm::open(Network::TESTNET, directory.path()).expect("open");
            vm.begin_block(None, [21; 32]).expect("begin a block");
            vm.deploy_contract(
                contract.clone(),
                ClarityVersion::Clarity6,
                "(define-read-only (answer) (+ u1 u41))",
                LimitedCostTracker::new_free(),
            )
            .expect("deploy the contract");
            let answer = ask(&mut vm);
            vm.seal_block().expect("seal the block");
            answer
        };
        assert_eq!(compiled, Value::UInt(42));
        assert!(count() > 0, "the deploy left native code behind");

        let cached = {
            let mut vm = Vm::open(Network::TESTNET, directory.path()).expect("reopen");
            vm.begin_block(Some([21; 32]), [22; 32]).expect("begin a block");
            ask(&mut vm)
        };

        std::fs::remove_dir_all(&entries).expect("delete the cache");
        let recompiled = {
            let mut vm = Vm::open(Network::TESTNET, directory.path()).expect("reopen");
            vm.begin_block(Some([21; 32]), [23; 32]).expect("begin a block");
            ask(&mut vm)
        };

        assert_eq!(compiled, cached);
        assert_eq!(compiled, recompiled);
        assert!(count() > 0, "a deleted cache is written again");
    }

    /// A block is sealed with its header and its caller's ledger, or with none of
    /// them.
    #[test]
    fn a_committed_block_carries_its_header_and_ledger_across_a_restart() {
        let directory = tempfile::tempdir().expect("a directory");
        let header = BlockHeader {
            burn_block_height: 960_240,
            tenure_height: 251_400,
            tenure_start_height: 8_665_700,
            ..BlockHeader::default()
        };
        {
            let mut vm = Vm::open(Network::MAINNET, directory.path()).expect("open");
            vm.begin_block(None, [1; 32]).expect("begin");
            vm.commit_block(
                [1; 32],
                &BlockCommit {
                    header,
                    ledger: b"owed as of block one".to_vec(),
                },
            )
            .expect("commit");
        }

        let vm = Vm::open(Network::MAINNET, directory.path()).expect("reopen");
        assert_eq!(vm.tip(), Some([1; 32]));
        assert_eq!(vm.recorded_header([1; 32]), Some(header));
        assert_eq!(
            vm.recorded_ledger([1; 32]).as_deref(),
            Some(b"owed as of block one".as_slice())
        );
    }

    /// A crash between the two durability boundaries leaves the parent, whole.
    ///
    /// `prepare_commit` is everything a block writes outside the MARF; the MARF
    /// commit follows and is what makes the block real. Stopping in between is the
    /// one window a crash can land in, and this is that window: the prepared rows
    /// are on disk and the block is not. A reopened store must stand on the
    /// parent, with the parent's ledger — not on a block that never sealed, and
    /// not on a tip whose ledger is a block behind.
    #[test]
    fn a_crash_between_the_two_boundaries_leaves_the_parent_and_its_ledger() {
        let directory = tempfile::tempdir().expect("a directory");
        {
            let mut store = MarfStore::open(Network::MAINNET, directory.path()).expect("open");
            store.begin(None, [1; 32]).expect("begin the parent");
            store
                .commit_to(
                    [1; 32],
                    &BlockCommit {
                        header: BlockHeader::default(),
                        ledger: b"as of the parent".to_vec(),
                    },
                )
                .expect("commit the parent");

            // The child, prepared and then abandoned where a SIGKILL would land.
            store.begin(Some([1; 32]), [2; 32]).expect("begin the child");
            store
                .prepare_commit(
                    [2; 32],
                    &BlockCommit {
                        header: BlockHeader::default(),
                        ledger: b"as of the child".to_vec(),
                    },
                )
                .expect("prepare the child");
        }

        let store = MarfStore::open(Network::MAINNET, directory.path()).expect("reopen");
        assert_eq!(
            store.tip(),
            Some([1; 32]),
            "the child never committed, so the parent is the tip"
        );
        assert_eq!(
            store.ledger_at([1; 32]).as_deref(),
            Some(b"as of the parent".as_slice()),
            "and the ledger the reopened store stands on is the parent's"
        );
        // The child's rows are on disk and unreachable: nothing addresses them
        // but the child, and the child is not in the MARF.
        assert!(store.ledger_at([2; 32]).is_some());
    }

    /// The size at which a contract of trait calls stops compiling.
    ///
    /// A mainnet contract — one trait returning
    /// `(response (list 20 (response uint uint)) uint)` and forty near
    /// identical functions calling it — compiles to a module wasmtime refuses.
    /// Neither shape does so alone, so this grows one until it breaks, which is
    /// the only way to say where the generator gives out.
    #[test]
    #[ignore = "a search, run when hunting the generator bug"]
    fn a_contract_of_trait_calls_compiles_at_size() {
        let mut source = String::from(
            "(define-trait z ((ss ((buff 400)) (response (list 20 (response uint uint)) uint))))\n",
        );
        for count in 1..=64 {
            use std::fmt::Write;
            if count == 1 {
                writeln!(
                    source,
                    "(define-public (r (b (buff 400)) (t <z>)) \
                     (if (> (len b) u0) (contract-call? t ss b) (ok (list))))"
                )
            } else {
                // The rest pass the trait on rather than calling it, which is
                // the shape the mainnet contract actually has.
                writeln!(
                    source,
                    "(define-public (r{count} (b (buff 400)) (t <z>)) (r b t))"
                )
            }
            .expect("writing to a string cannot fail");

            let contract = QualifiedContractIdentifier::parse(&format!(
                "ST000000000000000000002AMW42H.grow{count}"
            ))
            .expect("valid contract identifier");
            let mut vm = Vm::new(Network::TESTNET).expect("create VM");
            vm.begin_block(None, [9; 32]).expect("begin block");
            if let Err(error) = vm.deploy_contract(
                contract,
                ClarityVersion::Clarity3,
                &source,
                LimitedCostTracker::new_free(),
            ) {
                panic!("{count} functions is where it gives out: {error}");
            }
        }
    }

    /// A trait call whose response carries a list of responses.
    ///
    /// A mainnet contract declares
    /// `(response (list 20 (response uint uint)) uint)` on a trait and calls
    /// it dynamically, and clarity-wasm compiled the caller to a module
    /// wasmtime refused: "expected i64, found i32", which is a response's
    /// payload width against its indicator's.
    #[test]
    fn a_trait_returning_a_list_of_responses_compiles() {
        let caller =
            QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.caller")
                .expect("valid contract identifier");
        let mut vm = Vm::new(Network::TESTNET).expect("create VM");
        vm.begin_block(None, [9; 32]).expect("begin block");

        vm.deploy_contract(
            caller,
            ClarityVersion::Clarity3,
            "(define-trait z ((ss ((buff 400)) (response (list 20 (response uint uint)) uint))))
             (define-public (r (b (buff 400)) (t <z>))
               (if (> (len b) u0) (contract-call? t ss b) (ok (list))))",
            LimitedCostTracker::new_free(),
        )
        .expect("a contract calling such a trait deploys");
    }

    /// An empty list of responses is still a list of responses.
    ///
    /// A mainnet contract returns `(ok (list))` where the declared type is
    /// `(list 20 (response uint uint))`, and clarity-wasm compiled it to a
    /// module wasmtime refused — "expected i64, found i32", which is a
    /// response's payload width against its indicator's.
    #[test]
    fn an_empty_list_of_responses_compiles() {
        let contract =
            QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.empty")
                .expect("valid contract identifier");
        let mut vm = Vm::new(Network::TESTNET).expect("create VM");
        vm.begin_block(None, [9; 32]).expect("begin block");

        vm.deploy_contract(
            contract.clone(),
            ClarityVersion::Clarity3,
            "(define-public (go (n uint))
               (if (> n u0) (ok (list (ok u1))) (ok (list))))",
            LimitedCostTracker::new_free(),
        )
        .expect("a contract returning an empty list of responses deploys");

        let mut argument = Vec::new();
        Value::UInt(0)
            .consensus_serialize(&mut argument)
            .expect("serialize");
        let outcome = vm
            .execute_contract_call_outcome(
                contract.issuer.clone().into(),
                None,
                contract,
                "go",
                &[argument],
                &LimitedCostTracker::new_free(),
            )
            .expect("the call runs");
        assert!(
            matches!(outcome, ContractCallOutcome::Success(_)),
            "the empty branch runs: {outcome:?}"
        );
    }

    /// A height falls in the epoch mainnet was in at the time.
    ///
    /// Answering "epoch 4.0" everywhere is only right at the tip, and a
    /// contract deployed years ago was analysed under the rules of its own.
    #[test]
    fn a_burn_height_names_the_epoch_mainnet_was_in() {
        use clarity::types::StacksEpochId;

        for (height, expected) in [
            (666_050, StacksEpochId::Epoch20),
            (712_999, StacksEpochId::Epoch20),
            (713_000, StacksEpochId::Epoch2_05),
            (781_551, StacksEpochId::Epoch21),
            (840_360, StacksEpochId::Epoch25),
            (867_867, StacksEpochId::Epoch30),
            (943_333, StacksEpochId::Epoch34),
            (960_229, StacksEpochId::Epoch34),
            (960_230, StacksEpochId::Epoch40),
            (u64::MAX, StacksEpochId::Epoch40),
        ] {
            assert_eq!(
                super::epoch_at_burn_height(true, height),
                expected,
                "mainnet burn height {height}"
            );
        }

        // Another network's boundaries are its own configuration, so it is
        // treated as being at the epoch this node runs.
        assert_eq!(
            super::epoch_at_burn_height(false, 1),
            StacksEpochId::Epoch40
        );
    }

    /// A sortition identifier round-trips the burn height it names.
    ///
    /// Clarity only ever uses one to ask what happened at a burn height, so
    /// that is the whole of what one has to carry — and a node that hands one
    /// out but cannot read it back answers `none` to `get-burn-block-info?`
    /// again by another route.
    #[test]
    fn a_sortition_identifier_names_its_burn_height() {
        for height in [0, 1, 960_232, u32::MAX] {
            assert_eq!(
                super::burn_height_of(&super::sortition_of_burn_height(height)),
                height
            );
        }
    }

    /// `get-burn-block-info? header-hash` has to answer for the block being
    /// executed, not `none`.
    ///
    /// From epoch 3 on Clarity resolves the burn block through the tip
    /// sortition rather than the parent's consensus hash, so a node that names
    /// no tip sortition answers `none` for every height without raising
    /// anything. sBTC's withdrawal path compares the hash it was signed for
    /// against this, so nano rejected a withdrawal mainnet accepted.
    #[test]
    fn a_block_with_a_parent_still_reads_its_burn_header() {
        let hash = [0x5b; 32];
        let parent = [8; 32];
        let mut vm = Vm::new(Network::TESTNET).expect("create VM");
        vm.record_block_header(
            parent,
            BlockHeader {
                burn_header_hash: [0x4a; 32],
                burn_block_height: 960_231,
                ..BlockHeader::default()
            },
        )
        .expect("record a header");
        vm.begin_block(None, parent).expect("begin parent");
        vm.seal_block().expect("seal parent");

        vm.record_block_header(
            [9; 32],
            BlockHeader {
                burn_header_hash: hash,
                burn_block_height: 960_232,
                ..BlockHeader::default()
            },
        )
        .expect("record a header");
        let mut context = super::BitcoinBlockContext::at_height(960_232);
        context.burn_header_hash = hash;
        vm.begin_block_with_bitcoin_context(Some(parent), [9; 32], context)
            .expect("begin block");

        let value = read_through_a_contract(
            &mut vm,
            "burn-header",
            "(get-burn-block-info? header-hash u960232)",
        );

        assert_eq!(
            value,
            Value::some(Value::buff_from(hash.to_vec()).expect("a buffer")).expect("an optional")
        );
    }

    #[test]
    fn vm_executes_and_seals_a_block_state() {
        let block = [9; 32];
        let mut vm = Vm::new(Network::TESTNET).expect("create VM");
        vm.begin_block(None, block).expect("begin block");

        let contract = QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.counter")
            .expect("a contract identifier");
        vm.deploy_contract(
            contract.clone(),
            ClarityVersion::Clarity6,
            "(define-data-var counter uint u1)
             (var-set counter u2)
             (define-read-only (answer) (var-get counter))",
            LimitedCostTracker::new_free(),
        )
        .expect("deploy in block");
        let sender: PrincipalData = contract.issuer.clone().into();
        let value = vm
            .call_contract_values(&sender, &contract, "answer", &[])
            .expect("read the variable a deploy set");
        let root = vm.seal_block().expect("seal block");

        assert_eq!(value, Value::UInt(2));
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
                &LimitedCostTracker::new_free(),
            )
            .expect("call contract");

        assert_eq!(
            result.value,
            Some(Value::okay(Value::UInt(42)).expect("valid response"))
        );
        vm.seal_block().expect("seal block");
    }

    /// `unwrap!` in a `let` binding must return from the whole function, not
    /// just abandon the binding. `PoX`-5 guards its entry points this way.
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
                &LimitedCostTracker::new_free(),
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
                &LimitedCostTracker::new_free(),
            )
            .expect("call contract");

        assert_eq!(result.value, Some(Value::err_uint(1)));
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
            .put("nano-checkpoint-extension", "value")
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
            &LimitedCostTracker::new_free(),
        );

        assert!(
            matches!(result, Ok(ref result) if matches!(result.value, Some(Value::UInt(_))),),
            "{result:?}"
        );
    }
}

