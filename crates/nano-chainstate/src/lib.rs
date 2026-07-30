#![forbid(unsafe_code)]

mod nakamoto;
mod signers;

pub use nakamoto::{
    NakamotoBlock, NakamotoBlockHeader, NakamotoCodecError, Signer, SignerSet, SignerSetError,
    TenureError,
};

use clarity::vm::ClarityVersion as VmClarityVersion;
use clarity::vm::Value;
use clarity::vm::costs::{ExecutionCost, LimitedCostTracker};
use clarity::vm::errors::{ClarityEvalError, VmExecutionError};
use clarity::vm::events::{STXEventType, STXMintEventData, StacksTransactionEvent};
use std::collections::{BTreeMap, HashMap, HashSet};

use clarity::vm::contexts::{AssetMap, AssetMapEntry};
use clarity::vm::representations::ClarityName;
use clarity::vm::types::{AssetIdentifier, PrincipalData, QualifiedContractIdentifier, TupleData};
use nano_address::PoxAddress;
use nano_bitcoin::{BitcoinOperation, BitcoinOperationKind};
use nano_codec::{
    AssetInfo, ClarityVersion, FungibleCondition, NonFungibleCondition, PostConditionData,
    PostConditionMode, PostConditionPrincipal, PoxCondition, Principal, TenureChangeCause,
    Transaction, TransactionPayloadData,
};
use nano_crypto::{StacksPrivateKey, Vrf, VrfProof, VrfPublicKey};
use nano_marf::{MarfValue, TriePointer};
use nano_primitives::{ConsensusHash, Network, Sha256Sum, TrieHash, sha512_256};
use nano_sortition::{SortitionReorg, SortitionSnapshot};
pub use nano_vm::BitcoinBlockContext;
use nano_vm::{ContractCallOutcome, ExecutionResult, MarfStoreError, TransactionResult, Vm};
use serde::Deserialize;
use std::path::Path;

/// How a block's committed state root is established during execution.
#[derive(Clone, Copy)]
enum RootPolicy<'a> {
    /// Derive the root, write it into the header, and sign as the miner.
    Mine(&'a StacksPrivateKey),
    /// Reject the block before sealing when the derived root disagrees.
    Verify,
    /// Accept whatever root execution produces.
    Trust,
}

/// What executing one block produced: its sealed state, what it cost, and the
/// receipt of every transaction it carried.
#[derive(Clone, Debug, PartialEq)]
pub struct AppliedBlock {
    pub bitcoin_height: u64,
    pub execution: ExecutionResult,
    pub execution_cost: ExecutionCost,
    pub receipts: Vec<TransactionReceipt>,
}

/// Native accounting that is applied when an epoch-4 block is finalized.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NativeBlockEffects {
    pub credits: Vec<NativeStxCredit>,
    pub liquid_supply_increase: u128,
}

/// One liquid-STX credit produced by native chainstate accounting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeStxCredit {
    pub recipient: PrincipalData,
    pub amount: u128,
}

/// Number of sortition-created tenures before a miner reward matures.
pub const MINER_REWARD_MATURITY: u64 = 100;

/// What a tenure earned for the recipients it pays once it matures.
///
/// `MINER_REWARD_MATURITY` tenures later the coinbase lands on the tenure's own
/// recipient and the fees its transactions paid land on the recipient of the
/// tenure before it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TenureEarnings {
    pub recipient: PrincipalData,
    pub coinbase: u128,
    pub fees: u128,
}

impl TenureEarnings {
    /// Read a tenure's reward recipient from the coinbase transaction that
    /// starts it: the recipient the coinbase names, or else the miner that
    /// signed it (`nakamoto/tenure.rs`, `make_scheduled_miner_reward`).
    #[must_use]
    pub fn from_tenure_start(block: &NakamotoBlock, coinbase: u128) -> Option<Self> {
        let transaction = block.transactions.iter().find(|transaction| {
            matches!(
                transaction.payload().data(),
                TransactionPayloadData::NakamotoCoinbase { .. }
                    | TransactionPayloadData::CoinbaseToAltRecipient { .. }
                    | TransactionPayloadData::Coinbase { .. }
            )
        })?;
        let named = match transaction.payload().data() {
            TransactionPayloadData::NakamotoCoinbase { recipient, .. } => recipient.clone(),
            TransactionPayloadData::CoinbaseToAltRecipient { recipient, .. } => {
                Some(recipient.clone())
            }
            _ => None,
        };
        let recipient = match named {
            Some(recipient) => principal_from_codec(&recipient).ok()?,
            None => principal_from_address(transaction.origin_address()?).ok()?,
        };
        Some(Self {
            recipient,
            coinbase,
            fees: 0,
        })
    }
}

/// The coinbase a sortition emits, which the burnchain schedule fixes.
///
/// From epoch 3.1 the emission follows SIP-029's interval table, and every
/// sortition also collects the per-block bonus the pre-mine funded plus the
/// emissions of the burn blocks since the last sortition
/// (`burn/sortition.rs`, `accumulated_coinbase_ustx`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoinbaseSchedule {
    pub mainnet: bool,
    pub first_bitcoin_height: u64,
    pub initial_mining_bonus: u128,
}

/// SIP-029 emissions in uSTX, by effective height, for testnet then mainnet.
const TESTNET_EMISSIONS: [(u64, u128); 6] = [
    (0, 1_000_000_000),
    (77_777, 500_000_000),
    (77_777 * 7, 250_000_000),
    (77_777 * 14, 125_000_000),
    (77_777 * 21, 62_500_000),
    (3_605_000, 1_000_000_000),
];
const MAINNET_EMISSIONS: [(u64, u128); 5] = [
    (0, 1_000_000_000),
    (666_050, 500_000_000),
    (2_197_560, 250_000_000),
    (4_249_920, 125_000_000),
    (6_302_280, 62_500_000),
];

impl CoinbaseSchedule {
    /// The emission of one sortition at a Bitcoin height.
    #[must_use]
    pub fn emission_at(&self, bitcoin_height: u64) -> u128 {
        let effective = bitcoin_height.saturating_sub(self.first_bitcoin_height);
        let intervals: &[(u64, u128)] = if self.mainnet {
            &MAINNET_EMISSIONS
        } else {
            &TESTNET_EMISSIONS
        };
        intervals
            .iter()
            .rev()
            .find(|(start, _)| effective >= *start)
            .map_or(0, |(_, emission)| *emission)
    }

    /// What a sortition collects on top of its own emission: the bonus for
    /// every burn block since the last sortition, and those blocks' emissions.
    ///
    /// `previous_sortition` is the last Bitcoin height that chose a miner, and
    /// is absent only for the first sortition a chain ever holds.
    #[must_use]
    pub fn accumulated_at(&self, bitcoin_height: u64, previous_sortition: Option<u64>) -> u128 {
        let Some(previous) = previous_sortition else {
            return 0;
        };
        let mut accumulated = 0_u128;
        let mut height = bitcoin_height;
        while height > previous {
            accumulated = accumulated.saturating_add(self.initial_mining_bonus);
            height -= 1;
            if height > previous {
                accumulated = accumulated.saturating_add(self.emission_at(height));
            }
        }
        accumulated
    }
}

/// SIP-031 emissions in uSTX, by the Bitcoin height each interval starts at.
///
/// Testnet runs the schedule from 71,525 at one interval every 360 burn blocks
/// (`stacks-common/src/types/mod.rs`, `SIP031_EMISSION_INTERVALS_*`).
const TESTNET_SIP_031_EMISSIONS: [(u64, u128); 6] = [
    (71_525 + 360, 1_000),
    (71_525 + 360 * 2, 2_000),
    (71_525 + 360 * 3, 3_000),
    (71_525 + 360 * 4, 4_000),
    (71_525 + 360 * 5, 5_000),
    (71_525 + 360 * 6, 0),
];
const MAINNET_SIP_031_EMISSIONS: [(u64, u128); 6] = [
    (907_740, 475_000_000),
    (960_300, 1_140_000_000),
    (1_012_860, 1_705_000_000),
    (1_065_420, 1_305_000_000),
    (1_117_980, 1_155_000_000),
    (1_170_540, 0),
];

/// The SIP-031 emission a tenure starting at this Bitcoin height mints.
///
/// Unlike the coinbase, the schedule is keyed on the absolute Bitcoin height
/// rather than an offset from the chain's first burn block.
#[must_use]
pub fn sip_031_emission(network: Network, bitcoin_height: u64) -> u128 {
    let intervals: &[(u64, u128)] = if network.is_mainnet() {
        &MAINNET_SIP_031_EMISSIONS
    } else {
        &TESTNET_SIP_031_EMISSIONS
    };
    intervals
        .iter()
        .rev()
        .find(|(start, _)| bitcoin_height >= *start)
        .map_or(0, |(_, amount)| *amount)
}

/// Checkpointed native accounting required to finalize future tenure-start blocks.
///
/// A checkpoint carries the effects that mature over the tenures right after it,
/// because those were earned before nano had any history. Everything later is
/// derived from the tenures nano executed itself.
#[derive(Clone, Debug, Default)]
pub struct TenureAccounting {
    matured_effects: BTreeMap<u64, NativeBlockEffects>,
    earnings: BTreeMap<u64, TenureEarnings>,
    schedule: Option<CoinbaseSchedule>,
    /// The tenure whose own start block was executed here, and so the only one
    /// whose fees can be counted from the blocks that follow.
    started: Option<u64>,
}

impl TenureAccounting {
    /// Decode portable checkpoint accounting from JSON.
    pub fn from_json(bytes: &[u8]) -> Result<Self, TenureAccountingError> {
        let checkpoint: TenureAccountingCheckpoint = serde_json::from_slice(bytes)
            .map_err(|error| TenureAccountingError::InvalidCheckpoint(error.to_string()))?;
        let mut accounting = Self {
            schedule: checkpoint
                .coinbase_schedule
                .map(|schedule| CoinbaseSchedule {
                    mainnet: schedule.mainnet,
                    first_bitcoin_height: schedule.first_bitcoin_height,
                    initial_mining_bonus: schedule.initial_mining_bonus_ustx,
                }),
            ..Self::default()
        };
        for tenure in checkpoint.tenures {
            accounting.seed_earnings(
                tenure.coinbase_height,
                TenureEarnings {
                    recipient: PrincipalData::parse(&tenure.recipient).map_err(|error| {
                        TenureAccountingError::InvalidCheckpoint(error.to_string())
                    })?,
                    coinbase: tenure.coinbase,
                    fees: tenure.fees,
                },
            );
        }
        for entry in checkpoint.matured_effects {
            let credits = entry
                .credits
                .into_iter()
                .map(|credit| {
                    Ok(NativeStxCredit {
                        recipient: PrincipalData::parse(&credit.recipient).map_err(|error| {
                            TenureAccountingError::InvalidCheckpoint(error.to_string())
                        })?,
                        amount: credit.amount,
                    })
                })
                .collect::<Result<Vec<_>, TenureAccountingError>>()?;
            accounting.record_matured_effects(
                entry.coinbase_height,
                NativeBlockEffects {
                    credits,
                    liquid_supply_increase: entry.liquid_supply_increase,
                },
            )?;
        }
        Ok(accounting)
    }

    /// Record the effects that mature when the given coinbase height is reached.
    pub fn record_matured_effects(
        &mut self,
        coinbase_height: u64,
        effects: NativeBlockEffects,
    ) -> Result<(), TenureAccountingError> {
        if self
            .matured_effects
            .insert(coinbase_height, effects)
            .is_some()
        {
            return Err(TenureAccountingError::DuplicateCoinbaseHeight);
        }
        Ok(())
    }

    /// The burnchain coinbase schedule, without which nothing is derived.
    #[must_use]
    pub const fn schedule(&self) -> Option<CoinbaseSchedule> {
        self.schedule
    }

    /// Adopt the earnings of a tenure that completed before this checkpoint.
    ///
    /// A checkpoint starts mid-tenure, so the fees of the tenure it lands in
    /// are only knowable from the node that produced the checkpoint.
    pub fn seed_earnings(&mut self, coinbase_height: u64, earnings: TenureEarnings) {
        self.earnings.insert(coinbase_height, earnings);
    }

    /// Record a tenure whose start block was executed here.
    /// Forget every tenure from `coinbase_height` on, which a Bitcoin
    /// reorganization takes off the canonical chain along with its sortitions.
    pub fn retract_from(&mut self, coinbase_height: u64) {
        self.earnings.retain(|height, _| *height < coinbase_height);
        self.matured_effects
            .retain(|height, _| *height < coinbase_height);
        if self.started.is_some_and(|started| started >= coinbase_height) {
            self.started = None;
        }
    }

    /// The rewards a tenure earned, once they have been recorded.
    #[must_use]
    pub fn earnings_at(&self, coinbase_height: u64) -> Option<&TenureEarnings> {
        self.earnings.get(&coinbase_height)
    }

    /// The STX a tenure earned, as `get-tenure-info? block-reward` reports it.
    #[must_use]
    pub fn reward_for_tenure(&self, coinbase_height: u64) -> u128 {
        self.earnings
            .get(&coinbase_height)
            .map_or(0, |earnings| earnings.coinbase.saturating_add(earnings.fees))
    }

    pub fn record_earnings(&mut self, coinbase_height: u64, earnings: TenureEarnings) {
        self.earnings.insert(coinbase_height, earnings);
        self.started = Some(coinbase_height);
    }

    /// Add the fees a block paid to the tenure that mined it, which only counts
    /// for a tenure this accounting saw from its first block.
    pub fn add_fees(&mut self, coinbase_height: u64, fees: u128) {
        if self.started != Some(coinbase_height) {
            return;
        }
        if let Some(earnings) = self.earnings.get_mut(&coinbase_height) {
            earnings.fees = earnings.fees.saturating_add(fees);
        }
    }

    /// Return the effects that mature at this tenure start.
    ///
    /// Failing loudly matters: a silently empty payout produces a state root
    /// that differs from the network's only once the block is already sealed.
    pub fn effects_for_tenure(
        &self,
        network: Network,
        coinbase_height: u64,
    ) -> Result<NativeBlockEffects, TenureAccountingError> {
        if let Some(effects) = self.matured_effects.get(&coinbase_height) {
            return Ok(effects.clone());
        }
        if coinbase_height <= MINER_REWARD_MATURITY {
            return Ok(NativeBlockEffects::default());
        }
        let matured = coinbase_height - MINER_REWARD_MATURITY;
        let earned = self
            .earnings
            .get(&matured)
            .ok_or(TenureAccountingError::UnknownTenure(matured))?;
        // The fees a tenure paid mature one tenure after its coinbase, and land
        // on the boot address when no tenure earned them.
        let previous = self.earnings.get(&matured.saturating_sub(1));
        let boot = || {
            PrincipalData::parse(network.boot_address())
                .expect("the boot address is a valid principal")
        };
        Ok(NativeBlockEffects {
            credits: vec![
                NativeStxCredit {
                    recipient: earned.recipient.clone(),
                    amount: earned.coinbase,
                },
                NativeStxCredit {
                    recipient: previous.map_or_else(boot, |earned| earned.recipient.clone()),
                    amount: previous.map_or(0, |earned| earned.fees),
                },
            ],
            liquid_supply_increase: earned.coinbase,
        })
    }
}

/// Errors raised while loading checkpointed tenure accounting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TenureAccountingError {
    DuplicateCoinbaseHeight,
    InvalidCheckpoint(String),
    /// A payout matured for a tenure neither the checkpoint nor execution knows.
    UnknownTenure(u64),
}

impl std::fmt::Display for TenureAccountingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateCoinbaseHeight => {
                formatter.write_str("duplicate matured accounting entry for a coinbase height")
            }
            Self::InvalidCheckpoint(error) => {
                write!(formatter, "invalid tenure accounting checkpoint: {error}")
            }
            Self::UnknownTenure(height) => write!(
                formatter,
                "tenure {height} matured without accounting: its rewards are neither checkpointed nor executed"
            ),
        }
    }
}

impl std::error::Error for TenureAccountingError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TenureAccountingCheckpoint {
    matured_effects: Vec<TenureAccountingCheckpointEntry>,
    /// Earnings of the tenures right before the checkpoint, which the first
    /// payout nano derives itself still has to pay out.
    #[serde(default)]
    tenures: Vec<TenureAccountingCheckpointTenure>,
    /// Burnchain emission schedule, needed to derive later tenures' coinbases.
    coinbase_schedule: Option<TenureAccountingCheckpointSchedule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TenureAccountingCheckpointTenure {
    coinbase_height: u64,
    recipient: String,
    coinbase: u128,
    fees: u128,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TenureAccountingCheckpointSchedule {
    mainnet: bool,
    first_bitcoin_height: u64,
    initial_mining_bonus_ustx: u128,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TenureAccountingCheckpointEntry {
    coinbase_height: u64,
    credits: Vec<TenureAccountingCheckpointCredit>,
    liquid_supply_increase: u128,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TenureAccountingCheckpointCredit {
    recipient: String,
    amount: u128,
}

/// A transaction result retained while applying a Nakamoto block.
#[derive(Clone, Debug, PartialEq)]
pub struct TransactionReceipt {
    pub txid: Sha256Sum,
    pub status: TransactionStatus,
    pub committed: bool,
    pub result: TransactionResult,
}

/// The canonical outcome of executing a transaction in a block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransactionStatus {
    Success,
    /// The contract call returned an error response, discarding its writes.
    AbortedByResponse,
    PostConditionAborted(String),
    RuntimeFailure(String),
}

enum PayloadOutcome {
    Success(TransactionResult),
    AbortedByResponse(TransactionResult),
    RuntimeFailure {
        result: TransactionResult,
        error: String,
    },
}

/// What executing one block needs beyond the state it runs against.
struct BlockExecution<'a> {
    bitcoin_context: BitcoinBlockContext,
    operations: &'a [BitcoinOperation],
    parent: Option<[u8; 32]>,
    root: RootPolicy<'a>,
    effects: NativeBlockEffects,
    /// Transactions the block may carry if execution admits them.
    candidates: &'a [Transaction],
}

/// A chainstate execution context backed by versioned VM state.
#[derive(Debug)]
pub struct ChainState {
    vm: Vm,
    accounting: TenureAccounting,
    /// Stacks height each tenure started at, which `get-tenure-info?` maps back.
    tenure_start_heights: BTreeMap<u32, u32>,
    /// The blocks executed since the checkpoint, oldest first.
    executed: Vec<ExecutedBlock>,
}

/// A block nano has executed, and the tenure it belongs to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExecutedBlock {
    block_id: [u8; 32],
    consensus_hash: ConsensusHash,
    tenure_height: u32,
}

/// What a Bitcoin reorganization took off nano's executed chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChainRetraction {
    /// The state to resume execution from, or the checkpoint when every
    /// executed block was retracted.
    pub resume_from: Option<[u8; 32]>,
    /// The blocks that left the canonical chain, oldest first.
    pub discarded: Vec<[u8; 32]>,
}

#[derive(Debug)]
pub enum ChainStateError {
    Storage(MarfStoreError),
    Evaluation(ClarityEvalError),
    Execution(VmExecutionError),
    InvalidTransaction(String),
    TransactionFailure {
        result: Box<TransactionResult>,
        status: TransactionStatus,
    },
    UnsupportedPayload,
    StateRootMismatch {
        expected: TrieHash,
        actual: TrieHash,
    },
}

impl std::fmt::Display for ChainStateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "state storage error: {error}"),
            Self::Evaluation(error) => write!(formatter, "Clarity evaluation error: {error}"),
            Self::Execution(error) => write!(formatter, "Clarity execution error: {error}"),
            Self::InvalidTransaction(error) => write!(formatter, "invalid transaction: {error}"),
            Self::TransactionFailure { status, .. } => {
                write!(formatter, "transaction failed: {status:?}")
            }
            Self::UnsupportedPayload => formatter.write_str("unsupported transaction payload"),
            Self::StateRootMismatch { expected, actual } => write!(
                formatter,
                "state root mismatch: expected {expected}, got {actual}"
            ),
        }
    }
}

impl std::error::Error for ChainStateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            Self::Evaluation(_)
            | Self::Execution(_)
            | Self::InvalidTransaction(_)
            | Self::TransactionFailure { .. }
            | Self::UnsupportedPayload
            | Self::StateRootMismatch { .. } => None,
        }
    }
}

impl From<MarfStoreError> for ChainStateError {
    fn from(error: MarfStoreError) -> Self {
        Self::Storage(error)
    }
}

impl From<ClarityEvalError> for ChainStateError {
    fn from(error: ClarityEvalError) -> Self {
        Self::Evaluation(error)
    }
}

impl From<VmExecutionError> for ChainStateError {
    fn from(error: VmExecutionError) -> Self {
        Self::Execution(error)
    }
}

impl ChainState {
    /// Create an empty chainstate for the supplied network.
    pub fn new(network: Network) -> Result<Self, ChainStateError> {
        Ok(Self {
            vm: Vm::new(network)?,
            accounting: TenureAccounting::default(),
            tenure_start_heights: BTreeMap::new(),
            executed: Vec::new(),
        })
    }

    /// Open chainstate from a checkpointed Clarity MARF.
    pub fn from_checkpoint(
        network: Network,
        path: impl AsRef<Path>,
        source: [u8; 32],
        expected_root: TrieHash,
    ) -> Result<Self, ChainStateError> {
        Ok(Self {
            vm: Vm::from_checkpoint(network, path, source, expected_root)?,
            accounting: TenureAccounting::default(),
            tenure_start_heights: BTreeMap::new(),
            executed: Vec::new(),
        })
    }

    /// Open chainstate that keeps its state in `directory`, importing the
    /// checkpoint only the first time.
    ///
    /// This is the route a node takes: a restart resumes from the tip on disk
    /// instead of re-importing and replaying everything since.
    pub fn open_from_checkpoint(
        network: Network,
        directory: impl AsRef<Path>,
        checkpoint: impl AsRef<Path>,
        source: [u8; 32],
        expected_root: TrieHash,
    ) -> Result<Self, ChainStateError> {
        Ok(Self {
            vm: Vm::open_from_checkpoint(network, directory, checkpoint, source, expected_root)?,
            accounting: TenureAccounting::default(),
            tenure_start_heights: BTreeMap::new(),
            executed: Vec::new(),
        })
    }

    /// The chain this state belongs to.
    #[must_use]
    pub const fn network(&self) -> Network {
        self.vm.network()
    }

    /// The executed state, for reads that answer the public RPC.
    ///
    /// Handing out the VM is deliberate: a read-only call and an account query
    /// are evaluations against the state this chainstate has sealed, and
    /// wrapping each one here would only restate what the VM already offers.
    pub const fn vm_mut(&mut self) -> &mut Vm {
        &mut self.vm
    }

    /// Evaluate a read-only Clarity program against the state just executed.
    pub fn evaluate(&mut self, source: &str) -> Result<Option<Value>, ChainStateError> {
        Ok(self
            .vm
            .execute(source, LimitedCostTracker::new_free())?
            .value)
    }

    /// Read an account's spendable STX at the state this chainstate reads from.
    pub fn account_balance(
        &mut self,
        principal: &PrincipalData,
    ) -> Result<u128, ChainStateError> {
        Ok(self.vm.account_balance(principal)?)
    }

    /// Access the portable accounting ledger associated with this chainstate.
    pub const fn accounting_mut(&mut self) -> &mut TenureAccounting {
        &mut self.accounting
    }

    /// Return the committed MARF leaves for a block state.
    #[must_use]
    pub fn state_leaves(&self, block: [u8; 32]) -> Option<Vec<(TrieHash, MarfValue)>> {
        self.vm.state_leaves(block)
    }

    /// Whether this block's state has already been executed and sealed.
    #[must_use]
    pub fn has_block_state(&self, block: [u8; 32]) -> bool {
        self.vm.content_root(block).is_some()
    }

    /// Return the MARF content hash before ancestry is incorporated.
    #[must_use]
    pub fn state_content_root(&self, block: [u8; 32]) -> Option<TrieHash> {
        self.vm.content_root(block)
    }

    /// Return the root pointers in their consensus serialization order.
    #[must_use]
    pub fn state_root_pointers(&self, block: [u8; 32]) -> Option<Vec<TriePointer>> {
        self.vm.root_pointers(block)
    }

    /// Return the pointers and child hashes a block state holds under a prefix.
    #[must_use]
    pub fn state_pointers_at(
        &self,
        block: [u8; 32],
        prefix: &[u8],
    ) -> Option<Vec<(TriePointer, TrieHash)>> {
        self.vm.pointers_at(block, prefix)
    }

    /// Execute a Clarity program for a block and seal its consensus state root.
    pub fn append_program(
        &mut self,
        snapshot: &SortitionSnapshot,
        parent: Option<[u8; 32]>,
        block: [u8; 32],
        source: &str,
    ) -> Result<AppliedBlock, ChainStateError> {
        self.vm.begin_block(parent, block)?;
        self.vm.execute(source, LimitedCostTracker::new_free())?;
        let state_root = self.vm.seal_block()?;
        Ok(AppliedBlock {
            bitcoin_height: snapshot.bitcoin_height,
            execution: ExecutionResult { state_root },
            execution_cost: ExecutionCost::ZERO,
            receipts: Vec::new(),
        })
    }

    /// Execute the supported transaction forms in a block and verify its committed state root.
    pub fn append_nakamoto_block(
        &mut self,
        snapshot: &SortitionSnapshot,
        parent: Option<[u8; 32]>,
        block: &NakamotoBlock,
    ) -> Result<AppliedBlock, ChainStateError> {
        self.append_nakamoto_block_with_bitcoin_context(
            BitcoinBlockContext::at_height(snapshot.bitcoin_height),
            parent,
            block,
        )
    }

    /// Execute a Nakamoto block with its complete Bitcoin context.
    pub fn append_nakamoto_block_with_bitcoin_context(
        &mut self,
        bitcoin_context: BitcoinBlockContext,
        parent: Option<[u8; 32]>,
        block: &NakamotoBlock,
    ) -> Result<AppliedBlock, ChainStateError> {
        self.append_nakamoto_block_with_effects(
            bitcoin_context,
            parent,
            block,
            NativeBlockEffects::default(),
        )
    }

    /// Execute a Nakamoto block with the decoded Bitcoin operations for its tenure start.
    pub fn append_nakamoto_block_with_bitcoin_operations(
        &mut self,
        bitcoin_context: BitcoinBlockContext,
        operations: &[BitcoinOperation],
        parent: Option<[u8; 32]>,
        block: &NakamotoBlock,
    ) -> Result<AppliedBlock, ChainStateError> {
        let mut block = block.clone();
        self.execute_nakamoto_block(
            &mut block,
            BlockExecution {
                bitcoin_context,
                operations,
                parent,
                root: RootPolicy::Verify,
                effects: NativeBlockEffects::default(),
                candidates: &[],
            },
        )
    }

    /// Execute native accounting and verify the state root committed by a Nakamoto block.
    pub fn append_nakamoto_block_with_effects(
        &mut self,
        bitcoin_context: BitcoinBlockContext,
        parent: Option<[u8; 32]>,
        block: &NakamotoBlock,
        effects: NativeBlockEffects,
    ) -> Result<AppliedBlock, ChainStateError> {
        let mut block = block.clone();
        self.execute_nakamoto_block(
            &mut block,
            BlockExecution {
                bitcoin_context,
                operations: &[],
                parent,
                root: RootPolicy::Verify,
                effects,
                candidates: &[],
            },
        )
    }

    /// Execute a Nakamoto block without checking its header's committed state root.
    pub fn execute_nakamoto_block_with_bitcoin_context(
        &mut self,
        bitcoin_context: BitcoinBlockContext,
        parent: Option<[u8; 32]>,
        block: &NakamotoBlock,
    ) -> Result<AppliedBlock, ChainStateError> {
        self.execute_nakamoto_block_with_effects(
            bitcoin_context,
            parent,
            block,
            NativeBlockEffects::default(),
        )
    }

    /// Execute a Nakamoto block with the decoded Bitcoin operations for its tenure start.
    pub fn execute_nakamoto_block_with_bitcoin_operations(
        &mut self,
        bitcoin_context: BitcoinBlockContext,
        operations: &[BitcoinOperation],
        parent: Option<[u8; 32]>,
        block: &NakamotoBlock,
    ) -> Result<AppliedBlock, ChainStateError> {
        let mut block = block.clone();
        self.execute_nakamoto_block(
            &mut block,
            BlockExecution {
                bitcoin_context,
                operations,
                parent,
                root: RootPolicy::Trust,
                effects: NativeBlockEffects::default(),
                candidates: &[],
            },
        )
    }

    /// Execute a Nakamoto block with native accounting effects derived from its Bitcoin context.
    pub fn execute_nakamoto_block_with_effects(
        &mut self,
        bitcoin_context: BitcoinBlockContext,
        parent: Option<[u8; 32]>,
        block: &NakamotoBlock,
        effects: NativeBlockEffects,
    ) -> Result<AppliedBlock, ChainStateError> {
        let mut block = block.clone();
        self.execute_nakamoto_block(
            &mut block,
            BlockExecution {
                bitcoin_context,
                operations: &[],
                parent,
                root: RootPolicy::Trust,
                effects,
                candidates: &[],
            },
        )
    }

    /// Execute a block candidate, derive its committed state root, and finalize its block ID.
    pub fn assemble_nakamoto_block_with_bitcoin_context(
        &mut self,
        bitcoin_context: BitcoinBlockContext,
        parent: Option<[u8; 32]>,
        mut block: NakamotoBlock,
        miner_key: &StacksPrivateKey,
    ) -> Result<(NakamotoBlock, AppliedBlock), ChainStateError> {
        let applied = self.execute_nakamoto_block(
            &mut block,
            BlockExecution {
                bitcoin_context,
                operations: &[],
                parent,
                root: RootPolicy::Mine(miner_key),
                effects: NativeBlockEffects::default(),
                candidates: &[],
            },
        )?;
        Ok((block, applied))
    }

    /// Execute a block candidate with the decoded Bitcoin operations for its tenure start.
    pub fn assemble_nakamoto_block_with_bitcoin_operations(
        &mut self,
        bitcoin_context: BitcoinBlockContext,
        operations: &[BitcoinOperation],
        parent: Option<[u8; 32]>,
        block: NakamotoBlock,
        miner_key: &StacksPrivateKey,
    ) -> Result<(NakamotoBlock, AppliedBlock), ChainStateError> {
        self.assemble_nakamoto_block_selecting(
            bitcoin_context,
            operations,
            parent,
            block,
            &[],
            miner_key,
        )
    }

    /// Execute a candidate block together with transactions it may drop.
    ///
    /// A miner cannot know whether a pending transaction is admissible until it
    /// runs against the state the block has built so far — the nonce may have
    /// moved, the fee may no longer be payable — so a candidate that cannot be
    /// admitted is left out of the block instead of failing it. The block's
    /// transaction Merkle root is derived from the transactions that remain.
    pub fn assemble_nakamoto_block_selecting(
        &mut self,
        bitcoin_context: BitcoinBlockContext,
        operations: &[BitcoinOperation],
        parent: Option<[u8; 32]>,
        mut block: NakamotoBlock,
        candidates: &[Transaction],
        miner_key: &StacksPrivateKey,
    ) -> Result<(NakamotoBlock, AppliedBlock), ChainStateError> {
        let applied = self.execute_nakamoto_block(
            &mut block,
            BlockExecution {
                bitcoin_context,
                operations,
                parent,
                root: RootPolicy::Mine(miner_key),
                effects: NativeBlockEffects::default(),
                candidates,
            },
        )?;
        Ok((block, applied))
    }

    /// Add the candidates execution admits to a block being assembled.
    ///
    /// A miner cannot know whether a pending transaction is admissible until it
    /// runs against the state the block has built so far, so one that is not is
    /// left out instead of failing the block. Filling stops at the epoch's block
    /// limit, which is what the network would reject the block for exceeding.
    fn admit_candidates(
        &mut self,
        block: &mut NakamotoBlock,
        candidates: &[Transaction],
        execution_cost: &mut ExecutionCost,
        receipts: &mut Vec<TransactionReceipt>,
    ) {
        if candidates.is_empty() {
            return;
        }
        for candidate in candidates {
            if candidate.verify_authorization().is_err() {
                continue;
            }
            let Ok(receipt) = self.execute_transaction(candidate, execution_cost) else {
                continue;
            };
            if execution_cost.add(&receipt.result.cost).is_err()
                || execution_cost.exceeds(&nano_vm::EPOCH_4_BLOCK_LIMIT)
            {
                break;
            }
            block.transactions.push(candidate.clone());
            receipts.push(receipt);
        }
        block.header.transaction_merkle_root =
            nano_codec::transaction_merkle_root(&block.transactions);
    }

    /// Advance the tenure a block starts, and return the rewards that mature
    /// with it.
    ///
    /// Recording what the new tenure earns is what lets the payout maturing a
    /// hundred tenures from now be derived rather than carried in a checkpoint
    /// of bounded length.
    fn start_tenure(
        &mut self,
        bitcoin_context: BitcoinBlockContext,
        operations: &[BitcoinOperation],
        block: &NakamotoBlock,
    ) -> Result<NativeBlockEffects, ChainStateError> {
        let next_height = self.vm.tenure_height()?.checked_add(1).ok_or_else(|| {
            ChainStateError::InvalidTransaction("tenure height overflow".to_owned())
        })?;
        self.vm.set_tenure_height(next_height)?;
        self.execute_bitcoin_operations(operations, bitcoin_context.height)?;
        if let Some(schedule) = self.accounting.schedule() {
            let coinbase = schedule
                .emission_at(bitcoin_context.height)
                .saturating_add(bitcoin_context.accumulated_coinbase);
            let earnings = TenureEarnings::from_tenure_start(block, coinbase).ok_or_else(|| {
                ChainStateError::InvalidTransaction(
                    "tenure-start block has no coinbase transaction".to_owned(),
                )
            })?;
            self.accounting
                .record_earnings(u64::from(next_height), earnings);
        }
        self.accounting
            .effects_for_tenure(self.vm.network(), u64::from(next_height))
            .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))
    }

    /// Execute one block, optionally rejecting it before it is sealed when the
    /// state it produces does not match the root its header commits to.
    fn execute_nakamoto_block(
        &mut self,
        block: &mut NakamotoBlock,
        execution: BlockExecution<'_>,
    ) -> Result<AppliedBlock, ChainStateError> {
        let BlockExecution {
            bitcoin_context,
            operations,
            parent,
            root,
            effects,
            candidates,
        } = execution;
        if let Some(parent) = parent {
            let parent_height = block.header.chain_length.checked_sub(2).ok_or_else(|| {
                ChainStateError::InvalidTransaction(
                    "Nakamoto block cannot extend the genesis height".to_owned(),
                )
            })?;
            self.vm.set_checkpoint_height(
                parent,
                u32::try_from(parent_height).map_err(|_| {
                    ChainStateError::InvalidTransaction("Stacks height overflows u32".to_owned())
                })?,
            );
        }
        self.vm
            .begin_block_execution(parent, temporary_state_id(), bitcoin_context)?;
        let mut effects = effects;
        let result = (|| {
            self.vm.setup_block_metadata(block.header.timestamp)?;
            if block_starts_new_tenure(block) {
                let matured = self.start_tenure(bitcoin_context, operations, block)?;
                effects.credits.extend(matured.credits);
                effects.liquid_supply_increase = effects
                    .liquid_supply_increase
                    .checked_add(matured.liquid_supply_increase)
                    .ok_or_else(|| {
                        ChainStateError::InvalidTransaction(
                            "native liquid supply increase overflow".to_owned(),
                        )
                    })?;
            }
            // The signer set is written before the block's transactions, so it
            // must be computed here rather than alongside the matured rewards.
            let coinbase_height = u64::from(self.vm.tenure_height()?);
            signers::update_signer_set(&mut self.vm, bitcoin_context, coinbase_height)?;
            let mut execution_cost = ExecutionCost::ZERO;
            let mut receipts = Vec::with_capacity(block.transactions.len());
            for transaction in &block.transactions {
                transaction.verify_authorization().map_err(|error| {
                    ChainStateError::InvalidTransaction(format!(
                        "transaction authorization failed: {error}"
                    ))
                })?;
                let receipt = self.execute_transaction(transaction, &execution_cost)?;
                execution_cost.add(&receipt.result.cost).map_err(|error| {
                    ChainStateError::InvalidTransaction(format!("block cost overflow: {error}"))
                })?;
                receipts.push(receipt);
            }
            self.admit_candidates(block, candidates, &mut execution_cost, &mut receipts);
            let coinbase_height = u64::from(self.vm.tenure_height()?);
            self.accounting.add_fees(coinbase_height, block_fees(block));
            for credit in effects.credits {
                self.vm.credit_stx(&credit.recipient, credit.amount)?;
            }
            self.vm
                .increment_liquid_stx_supply(effects.liquid_supply_increase)?;
            let unlocked = self.vm.process_scheduled_unlocks()?;
            self.vm.increment_liquid_stx_supply(unlocked)?;
            if block_starts_new_tenure(block) {
                self.mint_sip_031(bitcoin_context.height, block, &mut receipts)?;
            }
            match root {
                RootPolicy::Mine(miner_key) => {
                    block.header.state_index_root =
                        TrieHash::from_bytes(self.vm.pending_state_root()?.0);
                    block.header.miner_signature =
                        miner_key.sign(block.header.miner_signature_hash().as_bytes());
                }
                RootPolicy::Verify => {
                    let actual = TrieHash::from_bytes(self.vm.pending_state_root()?.0);
                    if actual != block.header.state_index_root {
                        return Err(ChainStateError::StateRootMismatch {
                            expected: block.header.state_index_root,
                            actual,
                        });
                    }
                }
                RootPolicy::Trust => {}
            }
            self.record_block_header(block, bitcoin_context)?;
            let state_root = self.vm.seal_block_to(*block.block_id().as_bytes())?;
            Ok(AppliedBlock {
                bitcoin_height: bitcoin_context.height,
                execution: ExecutionResult { state_root },
                execution_cost,
                receipts,
            })
        })();
        if result.is_err() {
            // Report why execution failed, not why the rollback did.
            drop(self.vm.abort_block());
        }
        result
    }

    /// Take back the blocks a Bitcoin reorganization removed from the chain.
    ///
    /// A retracted sortition takes its whole tenure with it, so every block
    /// under an invalidated consensus hash is discarded and execution resumes
    /// from the last block that survived. The MARF keeps the states those
    /// blocks sealed: they are addressed by block identifier and nothing
    /// reaches them once the chain no longer runs through them, so they cost
    /// space rather than correctness until storage reclaims them
    /// ([[021-hold-mainnet-scale-state-on-disk]]).
    pub fn retract(&mut self, reorg: &SortitionReorg) -> ChainRetraction {
        let invalidated: HashSet<_> = reorg.invalidated_consensus_hashes().into_iter().collect();
        let Some(fork) = self
            .executed
            .iter()
            .position(|block| invalidated.contains(&block.consensus_hash))
        else {
            return ChainRetraction {
                resume_from: self.executed.last().map(|block| block.block_id),
                discarded: Vec::new(),
            };
        };
        let discarded: Vec<_> = self.executed.split_off(fork);
        if let Some(first) = discarded.first() {
            self.accounting.retract_from(u64::from(first.tenure_height));
            self.tenure_start_heights
                .retain(|tenure, _| *tenure < first.tenure_height);
        }
        ChainRetraction {
            resume_from: self.executed.last().map(|block| block.block_id),
            discarded: discarded.into_iter().map(|block| block.block_id).collect(),
        }
    }

    /// The blocks executed since the checkpoint, oldest first.
    #[must_use]
    pub fn executed_blocks(&self) -> Vec<[u8; 32]> {
        self.executed.iter().map(|block| block.block_id).collect()
    }

    /// Record what Clarity may later read about the block just executed.
    ///
    /// Every block of a tenure reports that tenure's burn block, which is what
    /// `get-tenure-info?` returns and what stacks-core stores per header.
    fn record_block_header(
        &mut self,
        block: &NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
    ) -> Result<(), ChainStateError> {
        let tenure_height = self.vm.tenure_height()?;
        let stacks_height = u32::try_from(block.header.chain_length).map_err(|_| {
            ChainStateError::InvalidTransaction("Stacks height overflows u32".to_owned())
        })?;
        if block_starts_new_tenure(block) {
            self.tenure_start_heights.insert(tenure_height, stacks_height);
        }
        self.executed.push(ExecutedBlock {
            block_id: *block.block_id().as_bytes(),
            consensus_hash: block.header.consensus_hash,
            tenure_height,
        });
        let miner = self
            .accounting
            .earnings_at(u64::from(tenure_height))
            .and_then(|earnings| match &earnings.recipient {
                PrincipalData::Standard(address) => Some((address.version(), address.1)),
                PrincipalData::Contract(_) => None,
            })
            .unwrap_or((0, [0; 20]));
        self.vm.record_block_header(
            *block.block_id().as_bytes(),
            nano_vm::BlockHeader {
                burn_header_hash: bitcoin_context.burn_header_hash,
                burn_block_height: u32::try_from(bitcoin_context.height).map_err(|_| {
                    ChainStateError::InvalidTransaction("Bitcoin height overflows u32".to_owned())
                })?,
                burn_block_time: bitcoin_context.burn_block_time,
                stacks_block_time: block.header.timestamp,
                block_header_hash: *block.header.block_hash().as_bytes(),
                consensus_hash: *block.header.consensus_hash.as_bytes(),
                vrf_seed: bitcoin_context.vrf_seed,
                miner_address: miner,
                burn_spend_total: bitcoin_context.burn_spend_total,
                burn_spend_winner: bitcoin_context.burn_spend_winner,
                block_reward: self.accounting.reward_for_tenure(u64::from(tenure_height)),
                tenure_height,
                tenure_start_height: self
                    .tenure_start_heights
                    .get(&tenure_height)
                    .copied()
                    .unwrap_or(stacks_height),
            },
        );
        Ok(())
    }

    /// Mint the SIP-031 emission a new tenure owes, to the `.sip-031` contract.
    ///
    /// The supply is raised before the recipient is credited, and the mint event
    /// is reported on the coinbase, both because that is the order and the place
    /// stacks-core uses (`chainstate/nakamoto/mod.rs`,
    /// `sip_031_mint_and_transfer_on_new_tenure`) and a receipt or a write in the
    /// wrong order is a divergence.
    fn mint_sip_031(
        &mut self,
        bitcoin_height: u64,
        block: &NakamotoBlock,
        receipts: &mut [TransactionReceipt],
    ) -> Result<(), ChainStateError> {
        let network = self.vm.network();
        let amount = sip_031_emission(network, bitcoin_height);
        if amount == 0 {
            return Ok(());
        }
        let recipient = PrincipalData::Contract(
            QualifiedContractIdentifier::parse(&network.boot_contract_id("sip-031"))
                .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))?,
        );
        self.vm.increment_liquid_stx_supply(amount)?;
        self.vm.credit_stx(&recipient, amount)?;
        let coinbase_txid = block
            .transactions
            .iter()
            .find(|transaction| is_coinbase(transaction))
            .map(Transaction::txid);
        if let Some(coinbase) =
            coinbase_txid.and_then(|txid| receipts.iter_mut().find(|receipt| receipt.txid == txid))
        {
            coinbase
                .result
                .events
                .push(StacksTransactionEvent::STXEvent(
                    STXEventType::STXMintEvent(STXMintEventData { recipient, amount }),
                ));
        }
        Ok(())
    }

    fn execute_bitcoin_operations(
        &mut self,
        operations: &[BitcoinOperation],
        bitcoin_height: u64,
    ) -> Result<(), ChainStateError> {
        for kind in [
            BitcoinOperationClass::Stack,
            BitcoinOperationClass::Transfer,
            BitcoinOperationClass::Delegate,
            BitcoinOperationClass::Vote,
        ] {
            for operation in operations
                .iter()
                .filter(|operation| kind.matches(operation))
            {
                self.execute_bitcoin_operation(operation, bitcoin_height)?;
            }
        }
        Ok(())
    }

    fn execute_bitcoin_operation(
        &mut self,
        operation: &BitcoinOperation,
        bitcoin_height: u64,
    ) -> Result<(), ChainStateError> {
        match &operation.kind {
            BitcoinOperationKind::StackStx {
                sender,
                reward_address,
                amount,
                cycles,
                ..
            } => self.execute_native_contract_call(
                principal_from_address(*sender)?,
                &self.vm.network().boot_contract_id("pox-5"),
                "stack-stx",
                &[
                    Value::UInt(*amount),
                    pox_address_value(reward_address)?,
                    Value::UInt(u128::from(bitcoin_height)),
                    Value::UInt(u128::from(*cycles)),
                ],
            ),
            BitcoinOperationKind::TransferStx {
                sender,
                recipient,
                amount,
                memo,
            } => {
                self.vm.begin_transaction()?;
                let result = self.vm.transfer_stx(
                    &principal_from_address(*sender)?,
                    &principal_from_address(*recipient)?,
                    *amount,
                    memo,
                    LimitedCostTracker::new_free(),
                );
                finalize_native_transfer(&mut self.vm, &result)
            }
            BitcoinOperationKind::DelegateStx {
                sender,
                delegate,
                amount,
                reward_address,
                until_bitcoin_height,
                ..
            } => self.execute_native_contract_call(
                principal_from_address(*sender)?,
                &self.vm.network().boot_contract_id("pox-5"),
                "delegate-stx",
                &[
                    Value::UInt(*amount),
                    Value::Principal(principal_from_address(*delegate)?),
                    optional_uint(*until_bitcoin_height)?,
                    optional_pox_address(reward_address.as_ref())?,
                ],
            ),
            BitcoinOperationKind::VoteForAggregateKey {
                sender,
                signer_index,
                aggregate_key,
                round,
                reward_cycle,
            } => self.execute_native_contract_call(
                principal_from_address(*sender)?,
                &self.vm.network().boot_contract_id("signers-voting"),
                "vote-for-aggregate-public-key",
                &[
                    Value::UInt(u128::from(*signer_index)),
                    Value::buff_from(aggregate_key.to_vec())
                        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))?,
                    Value::UInt(u128::from(*round)),
                    Value::UInt(u128::from(*reward_cycle)),
                ],
            ),
            BitcoinOperationKind::LeaderBlockCommit { .. }
            | BitcoinOperationKind::LeaderKeyRegistration { .. }
            | BitcoinOperationKind::PreStx { .. } => Ok(()),
        }
    }

    fn execute_native_contract_call(
        &mut self,
        sender: PrincipalData,
        contract: &str,
        function: &str,
        arguments: &[Value],
    ) -> Result<(), ChainStateError> {
        let arguments = arguments
            .iter()
            .map(Value::serialize_to_vec)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))?;
        self.vm.begin_transaction()?;
        let result = self.vm.execute_contract_call_outcome(
            sender,
            None,
            QualifiedContractIdentifier::parse(contract)
                .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))?,
            function,
            &arguments,
            LimitedCostTracker::new_free(),
        );
        finalize_native_contract_call(&mut self.vm, &result)
    }

    fn execute_transaction(
        &mut self,
        transaction: &Transaction,
        execution_cost: &ExecutionCost,
    ) -> Result<TransactionReceipt, ChainStateError> {
        self.vm.begin_transaction()?;
        let result = self.execute_transaction_in_transaction(transaction, execution_cost);
        match result {
            Ok(receipt) => {
                self.vm.commit_transaction()?;
                Ok(receipt)
            }
            Err(ChainStateError::TransactionFailure { result, status }) => {
                self.vm.rollback_transaction()?;
                self.vm.begin_transaction()?;
                let receipt = self.apply_transaction_failure(transaction, *result, status);
                match receipt {
                    Ok(receipt) => {
                        self.vm.commit_transaction()?;
                        Ok(receipt)
                    }
                    Err(error) => {
                        self.vm.rollback_transaction()?;
                        Err(error)
                    }
                }
            }
            Err(error) => {
                self.vm.rollback_transaction()?;
                Err(error)
            }
        }
    }

    fn execute_transaction_in_transaction(
        &mut self,
        transaction: &Transaction,
        execution_cost: &ExecutionCost,
    ) -> Result<TransactionReceipt, ChainStateError> {
        let origin = transaction.origin_address().ok_or_else(|| {
            ChainStateError::InvalidTransaction("transaction has no recognized network".to_owned())
        })?;
        let sender = principal_from_address(origin)?;
        if let Some(receipt) = system_receipt(transaction) {
            return self.execute_system_transaction(transaction, &sender, receipt);
        }
        let sponsor = transaction
            .sponsor_address()
            .map(principal_from_address)
            .transpose()?;
        let origin_condition = transaction.auth().origin();
        let payer_condition = transaction.auth().payer();
        if self.vm.account_nonce(&sender)? != origin_condition.nonce() {
            return Err(ChainStateError::InvalidTransaction(
                "origin nonce does not match account state".to_owned(),
            ));
        }
        let payer = sponsor.as_ref().unwrap_or(&sender);
        if self.vm.account_nonce(payer)? != payer_condition.nonce() {
            return Err(ChainStateError::InvalidTransaction(
                "payer nonce does not match account state".to_owned(),
            ));
        }
        self.vm.debit_fee(payer, payer_condition.fee())?;
        let result = self.execute_payload(
            transaction,
            origin,
            &sender,
            sponsor.as_ref(),
            execution_cost,
        )?;
        let result = match result {
            PayloadOutcome::Success(result) => result,
            PayloadOutcome::AbortedByResponse(result) => {
                // The call already discarded its writes; the fee and nonce stand.
                self.update_transaction_nonces(&sender, payer, sponsor.is_some(), transaction)?;
                return Ok(TransactionReceipt {
                    txid: transaction.txid(),
                    status: TransactionStatus::AbortedByResponse,
                    committed: false,
                    result,
                });
            }
            PayloadOutcome::RuntimeFailure { result, error } => {
                return Err(ChainStateError::TransactionFailure {
                    result: Box::new(result),
                    status: TransactionStatus::RuntimeFailure(error),
                });
            }
        };
        if matches!(
            transaction.payload().data(),
            TransactionPayloadData::TokenTransfer { .. }
        ) {
            if !transaction.post_conditions().is_empty() {
                return Err(ChainStateError::InvalidTransaction(
                    "token transfers cannot have post-conditions".to_owned(),
                ));
            }
        } else if let Some(reason) = check_postconditions(transaction, &sender, &result.assets)? {
            return Err(ChainStateError::TransactionFailure {
                result: Box::new(result),
                status: TransactionStatus::PostConditionAborted(reason),
            });
        }
        self.update_transaction_nonces(&sender, payer, sponsor.is_some(), transaction)?;
        Ok(TransactionReceipt {
            txid: transaction.txid(),
            status: TransactionStatus::Success,
            committed: true,
            result,
        })
    }

    fn apply_transaction_failure(
        &mut self,
        transaction: &Transaction,
        result: TransactionResult,
        status: TransactionStatus,
    ) -> Result<TransactionReceipt, ChainStateError> {
        let origin = transaction.origin_address().ok_or_else(|| {
            ChainStateError::InvalidTransaction("transaction has no recognized network".to_owned())
        })?;
        let sender = principal_from_address(origin)?;
        let sponsor = transaction
            .sponsor_address()
            .map(principal_from_address)
            .transpose()?;
        let payer = sponsor.as_ref().unwrap_or(&sender);
        self.vm.debit_fee(payer, transaction.auth().payer().fee())?;
        self.update_transaction_nonces(&sender, payer, sponsor.is_some(), transaction)?;
        Ok(TransactionReceipt {
            txid: transaction.txid(),
            status,
            committed: false,
            result,
        })
    }

    fn execute_system_transaction(
        &mut self,
        transaction: &Transaction,
        sender: &PrincipalData,
        receipt: TransactionReceipt,
    ) -> Result<TransactionReceipt, ChainStateError> {
        let fee = transaction.auth().payer().fee();
        if fee == 0 {
            self.vm.touch_stx_balance(sender)?;
        } else {
            self.vm.debit_fee(sender, fee)?;
        }
        self.vm.set_account_nonce(
            sender,
            increment_nonce(transaction.auth().origin().nonce())?,
        )?;
        Ok(receipt)
    }

    fn execute_payload(
        &mut self,
        transaction: &Transaction,
        origin: nano_address::StacksAddress,
        sender: &PrincipalData,
        sponsor: Option<&PrincipalData>,
        execution_cost: &ExecutionCost,
    ) -> Result<PayloadOutcome, ChainStateError> {
        let cost_tracker = self
            .vm
            .transaction_cost_tracker_with_total(execution_cost.clone())?;
        let mut runtime_error = None;
        let mut aborted_by_response = false;
        let mut result = match transaction.payload().data() {
            TransactionPayloadData::TokenTransfer {
                recipient,
                amount,
                memo,
            } => {
                let recipient = principal_from_codec(recipient)?;
                self.vm
                    .transfer_stx(sender, &recipient, u128::from(*amount), memo, cost_tracker)?
            }
            TransactionPayloadData::SmartContract {
                contract_name,
                source,
            } => self.vm.deploy_contract(
                contract_identifier(origin, contract_name)?,
                VmClarityVersion::Clarity6,
                source,
                cost_tracker,
            )?,
            TransactionPayloadData::VersionedSmartContract {
                clarity_version,
                contract_name,
                source,
            } => self.vm.deploy_contract(
                contract_identifier(origin, contract_name)?,
                clarity_version_to_vm(*clarity_version),
                source,
                cost_tracker,
            )?,
            TransactionPayloadData::ContractCall {
                address,
                contract_name,
                function_name,
                arguments,
            } => match self.vm.execute_contract_call_outcome(
                sender.clone(),
                sponsor.cloned(),
                contract_identifier(*address, contract_name)?,
                function_name,
                &arguments
                    .iter()
                    .map(|argument| argument.as_bytes().to_vec())
                    .collect::<Vec<_>>(),
                cost_tracker,
            )? {
                ContractCallOutcome::Success(result) => *result,
                ContractCallOutcome::AbortedByResponse(result) => {
                    aborted_by_response = true;
                    *result
                }
                ContractCallOutcome::RuntimeFailure { cost, error } => {
                    runtime_error = Some(error.to_string());
                    TransactionResult {
                        value: Some(Value::err_none()),
                        cost,
                        assets: AssetMap::new(),
                        events: Vec::new(),
                    }
                }
            },
            _ => return Err(ChainStateError::UnsupportedPayload),
        };
        result.cost.sub(execution_cost).map_err(|error| {
            ChainStateError::InvalidTransaction(format!("transaction cost underflow: {error}"))
        })?;
        Ok(match runtime_error {
            Some(error) => PayloadOutcome::RuntimeFailure { result, error },
            None if aborted_by_response => PayloadOutcome::AbortedByResponse(result),
            None => PayloadOutcome::Success(result),
        })
    }

    fn update_transaction_nonces(
        &mut self,
        sender: &PrincipalData,
        payer: &PrincipalData,
        sponsored: bool,
        transaction: &Transaction,
    ) -> Result<(), ChainStateError> {
        let origin_nonce = increment_nonce(transaction.auth().origin().nonce())?;
        self.vm.set_account_nonce(sender, origin_nonce)?;
        if sponsored {
            self.vm
                .set_account_nonce(payer, increment_nonce(transaction.auth().payer().nonce())?)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum BitcoinOperationClass {
    Stack,
    Transfer,
    Delegate,
    Vote,
}

impl BitcoinOperationClass {
    const fn matches(self, operation: &BitcoinOperation) -> bool {
        matches!(
            (self, &operation.kind),
            (Self::Stack, BitcoinOperationKind::StackStx { .. })
                | (Self::Transfer, BitcoinOperationKind::TransferStx { .. })
                | (Self::Delegate, BitcoinOperationKind::DelegateStx { .. })
                | (Self::Vote, BitcoinOperationKind::VoteForAggregateKey { .. })
        )
    }
}

fn pox_address_value(address: &PoxAddress) -> Result<Value, ChainStateError> {
    let (version, hashbytes) = match address {
        PoxAddress::Standard { address, hash_mode } => {
            let hash_mode = hash_mode.ok_or_else(|| {
                ChainStateError::InvalidTransaction(
                    "Bitcoin operation has no address hash mode".to_owned(),
                )
            })?;
            (hash_mode as u8, address.hash160().as_bytes().to_vec())
        }
        PoxAddress::Addr20 {
            address_type,
            bytes,
            ..
        } => (*address_type as u8, bytes.to_vec()),
        PoxAddress::Addr32 {
            address_type,
            bytes,
            ..
        } => (*address_type as u8, bytes.to_vec()),
    };
    let hashbytes = Value::buff_from(hashbytes)
        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))?;
    let tuple = TupleData::from_data(vec![
        (
            ClarityName::from_literal("version"),
            Value::buff_from_byte(version),
        ),
        (ClarityName::from_literal("hashbytes"), hashbytes),
    ])
    .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))?;
    Ok(Value::Tuple(tuple))
}

fn optional_uint(height: Option<u64>) -> Result<Value, ChainStateError> {
    height.map_or_else(
        || Ok(Value::none()),
        |height| {
            Value::some(Value::UInt(u128::from(height)))
                .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))
        },
    )
}

fn optional_pox_address(address: Option<&PoxAddress>) -> Result<Value, ChainStateError> {
    address.map_or_else(
        || Ok(Value::none()),
        |address| {
            Value::some(pox_address_value(address)?)
                .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))
        },
    )
}

fn finalize_native_transfer(
    vm: &mut Vm,
    result: &Result<TransactionResult, VmExecutionError>,
) -> Result<(), ChainStateError> {
    match result {
        Ok(_) => vm.commit_transaction().map_err(ChainStateError::from),
        Err(_) => vm.rollback_transaction().map_err(ChainStateError::from),
    }
}

fn finalize_native_contract_call(
    vm: &mut Vm,
    result: &Result<ContractCallOutcome, VmExecutionError>,
) -> Result<(), ChainStateError> {
    match result {
        Ok(ContractCallOutcome::Success(_)) => {
            vm.commit_transaction().map_err(ChainStateError::from)
        }
        Ok(
            ContractCallOutcome::AbortedByResponse(_) | ContractCallOutcome::RuntimeFailure { .. },
        )
        | Err(_) => vm.rollback_transaction().map_err(ChainStateError::from),
    }
}

/// The fees a block's transactions paid, which its tenure collects.
fn block_fees(block: &NakamotoBlock) -> u128 {
    block
        .transactions
        .iter()
        .map(|transaction| u128::from(transaction.auth().payer().fee()))
        .sum()
}

/// Whether a block begins a tenure a sortition awarded, and so pays a coinbase.
#[must_use]
pub fn starts_new_tenure(block: &NakamotoBlock) -> bool {
    block_starts_new_tenure(block)
}

/// The VRF proof a tenure-start block's coinbase carries.
#[must_use]
pub fn coinbase_vrf_proof(block: &NakamotoBlock) -> Option<[u8; 80]> {
    block
        .transactions
        .iter()
        .find_map(|transaction| match transaction.payload().data() {
            TransactionPayloadData::NakamotoCoinbase { vrf_proof, .. } => Some(*vrf_proof),
            _ => None,
        })
}

/// The seed a miner must commit for the tenure that follows one whose coinbase
/// carried this proof (`stacks-common`, `VRFSeed::from_proof`).
#[must_use]
pub fn vrf_seed_from_proof(proof: &[u8; 80]) -> [u8; 32] {
    *sha512_256(proof).as_bytes()
}

/// A tenure-start block whose VRF does not hold up.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TenureVrfError {
    /// A tenure-start block reached execution without a coinbase proof.
    MissingProof,
    /// The proof is not 80 well-formed bytes.
    MalformedProof,
    /// The proof was not produced by the winning miner's registered VRF key.
    ProofNotFromLeaderKey,
    /// The seed committed on Bitcoin is not the hash of the parent tenure's proof.
    SeedNotFromParentProof,
}

impl std::fmt::Display for TenureVrfError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::MissingProof => "tenure-start block has no coinbase VRF proof",
            Self::MalformedProof => "coinbase VRF proof is malformed",
            Self::ProofNotFromLeaderKey => {
                "coinbase VRF proof was not produced by the winning leader key"
            }
            Self::SeedNotFromParentProof => {
                "committed seed is not the hash of the parent tenure's VRF proof"
            }
        })
    }
}

impl std::error::Error for TenureVrfError {}

/// Verify a tenure-start block's coinbase proof against the winning miner's key.
///
/// The proof is over the tenure's sortition hash, which already mixes the seed
/// the winning commitment carried, so a miner cannot produce it for a sortition
/// it did not win (`chainstate/nakamoto/mod.rs`, `check_normal_coinbase_tx`).
pub fn verify_coinbase_vrf_proof(
    block: &NakamotoBlock,
    leader_vrf_public_key: &[u8; 32],
    sortition_hash: &[u8; 32],
) -> Result<(), TenureVrfError> {
    let proof = coinbase_vrf_proof(block).ok_or(TenureVrfError::MissingProof)?;
    let public_key =
        VrfPublicKey::from_bytes(*leader_vrf_public_key)
        .map_err(|_| TenureVrfError::MalformedProof)?;
    let proof = VrfProof::from_bytes(&proof).map_err(|_| TenureVrfError::MalformedProof)?;
    match Vrf::verify(&public_key, &proof, sortition_hash) {
        Ok(true) => Ok(()),
        Ok(false) | Err(_) => Err(TenureVrfError::ProofNotFromLeaderKey),
    }
}

/// Verify that the seed a tenure's winning commitment carried was derived from
/// the parent tenure's VRF proof (`chainstate/nakamoto`, `validate_vrf_seed`).
///
/// Without this a miner could commit any seed and steer the sortition that
/// follows, so it is checked separately from the proof itself.
pub fn verify_committed_vrf_seed(
    committed_seed: &[u8; 32],
    parent_tenure_proof: &[u8; 80],
) -> Result<(), TenureVrfError> {
    if vrf_seed_from_proof(parent_tenure_proof) == *committed_seed {
        Ok(())
    } else {
        Err(TenureVrfError::SeedNotFromParentProof)
    }
}

/// Whether a block begins a tenure or extends the one it belongs to, which is
/// what restarts the clock a tenure runs against.
#[must_use]
pub fn starts_or_extends_tenure(block: &NakamotoBlock) -> bool {
    block.transactions.iter().any(|transaction| {
        matches!(
            transaction.payload().data(),
            TransactionPayloadData::TenureChange(payload)
                if matches!(
                    payload.cause,
                    TenureChangeCause::BlockFound | TenureChangeCause::Extended
                )
        )
    })
}

fn block_starts_new_tenure(block: &NakamotoBlock) -> bool {
    block.transactions.iter().any(|transaction| {
        matches!(
            transaction.payload().data(),
            TransactionPayloadData::TenureChange(payload)
                if payload.cause == TenureChangeCause::BlockFound
        )
    })
}

fn temporary_state_id() -> [u8; 32] {
    *sha512_256(&[1; 52]).as_bytes()
}

/// Whether a transaction is the block's coinbase, which carries the events a
/// block produces outside any transaction.
const fn is_coinbase(transaction: &Transaction) -> bool {
    matches!(
        transaction.payload().data(),
        TransactionPayloadData::Coinbase { .. }
            | TransactionPayloadData::CoinbaseToAltRecipient { .. }
            | TransactionPayloadData::NakamotoCoinbase { .. }
    )
}

fn increment_nonce(nonce: u64) -> Result<u64, ChainStateError> {
    nonce
        .checked_add(1)
        .ok_or_else(|| ChainStateError::InvalidTransaction("origin nonce overflow".to_owned()))
}

fn system_receipt(transaction: &Transaction) -> Option<TransactionReceipt> {
    (is_coinbase(transaction)
        || matches!(
            transaction.payload().data(),
            TransactionPayloadData::TenureChange(_)
        ))
    .then(|| TransactionReceipt {
        txid: transaction.txid(),
        status: TransactionStatus::Success,
        committed: true,
        result: TransactionResult {
            value: Some(Value::okay(Value::Bool(true)).expect("boolean is a valid response")),
            cost: ExecutionCost::ZERO,
            assets: AssetMap::new(),
            events: Vec::new(),
        },
    })
}

fn check_postconditions(
    transaction: &Transaction,
    origin: &PrincipalData,
    assets: &AssetMap,
) -> Result<Option<String>, ChainStateError> {
    let mut checked_fungible = HashMap::<PrincipalData, HashSet<AssetIdentifier>>::new();
    let mut checked_nonfungible =
        HashMap::<PrincipalData, HashMap<AssetIdentifier, Vec<Value>>>::new();
    let mut checked_staking = HashSet::new();
    let mut checked_pox = HashSet::new();

    for postcondition in transaction.post_conditions() {
        match postcondition.data() {
            PostConditionData::Stx {
                principal,
                condition,
                amount,
            } => {
                let principal = postcondition_principal(principal, origin)?;
                let transferred = assets.get_stx(&principal).unwrap_or(0);
                let burned = assets.get_stx_burned(&principal).unwrap_or(0);
                let sent = transferred.checked_add(burned).ok_or_else(|| {
                    ChainStateError::InvalidTransaction(
                        "STX post-condition amount overflow".to_owned(),
                    )
                })?;
                if !matches_fungible_condition(*condition, u128::from(*amount), sent) {
                    return Ok(Some(format!(
                        "STX post-condition failed for {principal}: expected {condition:?} {amount}, got {sent}"
                    )));
                }
                let covered = checked_fungible.entry(principal).or_default();
                if transferred > 0 {
                    covered.insert(AssetIdentifier::STX());
                }
                if burned > 0 {
                    covered.insert(AssetIdentifier::STX_burned());
                }
            }
            PostConditionData::Fungible {
                principal,
                asset,
                condition,
                amount,
            } => {
                let principal = postcondition_principal(principal, origin)?;
                let asset = asset_identifier(asset)?;
                let sent = assets.get_fungible_tokens(&principal, &asset).unwrap_or(0);
                if !matches_fungible_condition(*condition, u128::from(*amount), sent) {
                    return Ok(Some(format!(
                        "fungible post-condition failed for {asset} owned by {principal}: expected {condition:?} {amount}, got {sent}"
                    )));
                }
                checked_fungible.entry(principal).or_default().insert(asset);
            }
            PostConditionData::NonFungible {
                principal,
                asset,
                asset_value,
                condition,
            } => {
                let principal = postcondition_principal(principal, origin)?;
                let asset = asset_identifier(asset)?;
                let value = deserialize_clarity_value(asset_value.as_bytes())?;
                let sent = assets
                    .get_nonfungible_tokens(&principal, &asset)
                    .map_or(&[][..], Vec::as_slice);
                let moved = sent.contains(&value);
                let passes = match condition {
                    NonFungibleCondition::DoesSend => moved,
                    NonFungibleCondition::DoesNotSend => !moved,
                };
                if !passes {
                    return Ok(Some(format!(
                        "non-fungible post-condition failed for {asset} owned by {principal}"
                    )));
                }
                checked_nonfungible
                    .entry(principal)
                    .or_default()
                    .entry(asset)
                    .or_default()
                    .push(value);
            }
            PostConditionData::Staking { .. } | PostConditionData::Pox { .. } => {
                if let Some(reason) = check_epoch4_postcondition(
                    postcondition.data(),
                    origin,
                    assets,
                    &mut checked_staking,
                    &mut checked_pox,
                )? {
                    return Ok(Some(reason));
                }
            }
        }
    }

    Ok(finish_postconditions(
        transaction.post_condition_mode(),
        origin,
        assets,
        &checked_fungible,
        &checked_nonfungible,
        &checked_staking,
        &checked_pox,
    ))
}

fn finish_postconditions(
    mode: PostConditionMode,
    origin: &PrincipalData,
    assets: &AssetMap,
    checked_fungible: &HashMap<PrincipalData, HashSet<AssetIdentifier>>,
    checked_nonfungible: &HashMap<PrincipalData, HashMap<AssetIdentifier, Vec<Value>>>,
    checked_staking: &HashSet<PrincipalData>,
    checked_pox: &HashSet<PrincipalData>,
) -> Option<String> {
    check_unchecked_assets(mode, origin, assets, checked_fungible, checked_nonfungible)
        .or_else(|| check_unchecked_pox_actions(mode, origin, assets, checked_staking, checked_pox))
}

fn check_epoch4_postcondition(
    postcondition: &PostConditionData,
    origin: &PrincipalData,
    assets: &AssetMap,
    checked_staking: &mut HashSet<PrincipalData>,
    checked_pox: &mut HashSet<PrincipalData>,
) -> Result<Option<String>, ChainStateError> {
    match postcondition {
        PostConditionData::Staking {
            principal,
            condition,
            amount,
        } => {
            let principal = postcondition_principal(principal, origin)?;
            let staked = assets.get_stacking(&principal).unwrap_or(0);
            if !matches_fungible_condition(*condition, u128::from(*amount), staked) {
                return Ok(Some(format!(
                    "staking post-condition failed for {principal}: expected {condition:?} {amount}, got {staked}"
                )));
            }
            checked_staking.insert(principal);
        }
        PostConditionData::Pox {
            principal,
            condition,
        } => {
            let principal = postcondition_principal(principal, origin)?;
            let performed = assets.did_pox_action(&principal);
            let passes = match condition {
                PoxCondition::NotPerformed => !performed,
                PoxCondition::MaybePerformed => true,
                PoxCondition::Performed => performed,
            };
            if !passes {
                return Ok(Some(format!("PoX post-condition failed for {principal}")));
            }
            checked_pox.insert(principal);
        }
        _ => unreachable!("only epoch 4 post-conditions are dispatched here"),
    }
    Ok(None)
}

fn check_unchecked_pox_actions(
    mode: PostConditionMode,
    origin: &PrincipalData,
    assets: &AssetMap,
    checked_staking: &HashSet<PrincipalData>,
    checked_pox: &HashSet<PrincipalData>,
) -> Option<String> {
    if mode == PostConditionMode::Allow {
        return None;
    }
    let requires_check =
        |principal: &PrincipalData| mode == PostConditionMode::Deny || principal == origin;
    if assets
        .get_all_stacking()
        .keys()
        .any(|principal| requires_check(principal) && !checked_staking.contains(principal))
    {
        return Some("staking action was not covered by a post-condition".to_owned());
    }
    if assets
        .get_all_pox_actions()
        .iter()
        .any(|principal| requires_check(principal) && !checked_pox.contains(principal))
    {
        return Some("PoX action was not covered by a post-condition".to_owned());
    }
    None
}

fn check_unchecked_assets(
    mode: PostConditionMode,
    origin: &PrincipalData,
    assets: &AssetMap,
    checked_fungible: &HashMap<PrincipalData, HashSet<AssetIdentifier>>,
    checked_nonfungible: &HashMap<PrincipalData, HashMap<AssetIdentifier, Vec<Value>>>,
) -> Option<String> {
    if mode == PostConditionMode::Allow {
        return None;
    }
    for (principal, assets) in assets.clone().to_table() {
        if mode == PostConditionMode::Originator && principal != *origin {
            continue;
        }
        for (asset, entry) in assets {
            match entry {
                AssetMapEntry::Asset(values) => {
                    let covered = checked_nonfungible
                        .get(&principal)
                        .and_then(|assets| assets.get(&asset));
                    if values
                        .iter()
                        .any(|value| !covered.is_some_and(|covered| covered.contains(value)))
                    {
                        return Some(format!(
                            "non-fungible asset {asset} moved by {principal} was not covered"
                        ));
                    }
                }
                _ => {
                    if !checked_fungible
                        .get(&principal)
                        .is_some_and(|covered| covered.contains(&asset))
                    {
                        return Some(format!(
                            "fungible asset {asset} moved by {principal} was not covered"
                        ));
                    }
                }
            }
        }
    }
    None
}

fn postcondition_principal(
    principal: &PostConditionPrincipal,
    origin: &PrincipalData,
) -> Result<PrincipalData, ChainStateError> {
    match principal {
        PostConditionPrincipal::Origin => Ok(origin.clone()),
        PostConditionPrincipal::Standard(address) => principal_from_address(*address),
        PostConditionPrincipal::Contract {
            address,
            contract_name,
        } => principal_from_codec(&Principal::Contract {
            address: *address,
            contract_name: contract_name.clone(),
        }),
    }
}

fn asset_identifier(asset: &AssetInfo) -> Result<AssetIdentifier, ChainStateError> {
    let asset_name = ClarityName::try_from(asset.asset_name.clone())
        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))?;
    Ok(AssetIdentifier {
        contract_identifier: contract_identifier(asset.address, &asset.contract_name)?,
        asset_name,
    })
}

fn deserialize_clarity_value(bytes: &[u8]) -> Result<Value, ChainStateError> {
    let mut bytes = bytes;
    let value = Value::deserialize_read(&mut bytes, None, false)
        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))?;
    if bytes.is_empty() {
        Ok(value)
    } else {
        Err(ChainStateError::InvalidTransaction(
            "non-fungible post-condition value has trailing bytes".to_owned(),
        ))
    }
}

const fn matches_fungible_condition(
    condition: FungibleCondition,
    expected: u128,
    actual: u128,
) -> bool {
    match condition {
        FungibleCondition::SentEqual => actual == expected,
        FungibleCondition::SentGreater => actual > expected,
        FungibleCondition::SentGreaterEqual => actual >= expected,
        FungibleCondition::SentLess => actual < expected,
        FungibleCondition::SentLessEqual => actual <= expected,
    }
}

fn principal_from_address(
    address: nano_address::StacksAddress,
) -> Result<PrincipalData, ChainStateError> {
    PrincipalData::parse(&address.to_string())
        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))
}

fn principal_from_codec(principal: &Principal) -> Result<PrincipalData, ChainStateError> {
    match principal {
        Principal::Standard(address) => principal_from_address(*address),
        Principal::Contract {
            address,
            contract_name,
        } => QualifiedContractIdentifier::parse(&format!("{address}.{contract_name}"))
            .map(PrincipalData::Contract)
            .map_err(|error| ChainStateError::InvalidTransaction(error.to_string())),
    }
}

fn contract_identifier(
    address: nano_address::StacksAddress,
    contract_name: &str,
) -> Result<QualifiedContractIdentifier, ChainStateError> {
    QualifiedContractIdentifier::parse(&format!("{address}.{contract_name}"))
        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))
}

const fn clarity_version_to_vm(version: ClarityVersion) -> VmClarityVersion {
    match version {
        ClarityVersion::Clarity1 => VmClarityVersion::Clarity1,
        ClarityVersion::Clarity2 => VmClarityVersion::Clarity2,
        ClarityVersion::Clarity3 => VmClarityVersion::Clarity3,
        ClarityVersion::Clarity4 => VmClarityVersion::Clarity4,
        ClarityVersion::Clarity5 => VmClarityVersion::Clarity5,
        ClarityVersion::Clarity6 => VmClarityVersion::Clarity6,
    }
}

#[cfg(test)]
mod tests {
    use super::{CoinbaseSchedule, TenureEarnings};
    use clarity::vm::contexts::AssetMap;
    use clarity::vm::costs::ExecutionCost;
    use clarity::vm::types::PrincipalData;
    use std::{fs, path::Path};

    use nano_address::StacksAddress;
    use nano_codec::Transaction;
    use nano_primitives::{BitcoinHeaderHash, Hash160, Network, TrieHash};
    use nano_sortition::SortitionSnapshot;

    use super::{
        BitcoinBlockContext, ChainState, NakamotoBlock, NativeBlockEffects, NativeStxCredit,
        TenureAccounting, TenureAccountingError, TransactionStatus, check_postconditions,
        principal_from_address,
    };

    #[test]
    fn append_program_seals_the_vm_state_root() {
        let snapshot = SortitionSnapshot::genesis(42, BitcoinHeaderHash::from_bytes([0; 32]));
        let mut chainstate = ChainState::new(Network::TESTNET).expect("create chainstate");

        let applied = chainstate
            .append_program(
                &snapshot,
                None,
                [1; 32],
                "(define-data-var counter uint u1) (var-set counter u2) (var-get counter)",
            )
            .expect("append program");

        assert_eq!(applied.bitcoin_height, 42);
        assert_ne!(applied.execution.state_root, nano_marf::StateRoot::empty());
    }

    #[test]
    fn executes_decoded_contract_deployments_and_calls() {
        let mut chainstate = ChainState::new(Network::TESTNET).expect("create chainstate");
        chainstate
            .vm
            .begin_block(None, [1; 32])
            .expect("begin block");

        let deployment_payload = versioned_contract_payload(
            "counter",
            "(define-public (increment (value uint)) (ok (+ value u1)))",
        );
        let call_payload = contract_call_payload("counter", "increment", 41);
        let deployment = decoded_transaction(&deployment_payload, 0, 0);
        let call = decoded_transaction(&call_payload, 1, 0);

        let deployed = chainstate
            .execute_transaction(&deployment, &ExecutionCost::ZERO)
            .expect("deploy decoded contract");
        let called = chainstate
            .execute_transaction(&call, &deployed.result.cost)
            .expect("call decoded contract");

        assert_eq!(deployed.result.value, None);
        assert_eq!(
            called.result.value,
            Some(clarity::vm::Value::okay(clarity::vm::Value::UInt(42)).expect("response"))
        );
        chainstate.vm.seal_block().expect("seal block");
    }

    /// The captured corpus is recaptured wholesale, so tests read its checkpoint
    /// from the manifest and address its blocks by position.
    fn captured_checkpoint() -> (std::path::PathBuf, [u8; 32], TrieHash, u64) {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../nano-conformance/fixtures");
        let manifest = fs::read_to_string(fixtures.join("chainstate/checkpoint-H/checkpoint.toml"))
            .expect("read checkpoint manifest");
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
            fixtures.join("chainstate/checkpoint-H/marf.sqlite"),
            decode(&field("source_state_id")),
            TrieHash::from_bytes(decode(&field("published_state_index_root"))),
            field("first_bitcoin_height")
                .parse()
                .expect("Bitcoin height"),
        )
    }

    fn captured_first_block() -> NakamotoBlock {
        let blocks = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../nano-conformance/fixtures/nakamoto/blocks");
        let mut paths = fs::read_dir(blocks)
            .expect("read captured blocks")
            .map(|entry| entry.expect("captured block entry").path())
            .collect::<Vec<_>>();
        paths.sort();
        NakamotoBlock::decode(&fs::read(&paths[0]).expect("read fixture block"))
            .expect("decode fixture block")
    }

    #[test]
    fn executes_a_captured_checkpoint_token_transfer() {
        let (checkpoint, source, root, _) = captured_checkpoint();
        let block = captured_first_block();
        let mut chainstate =
            ChainState::from_checkpoint(Network::TESTNET, checkpoint, source, root)
                .expect("open checkpoint");
        chainstate
            .vm
            .begin_block(Some(source), *block.block_id().as_bytes())
            .expect("begin block");

        for transaction in &block.transactions {
            chainstate
                .execute_transaction(transaction, &ExecutionCost::ZERO)
                .expect("execute captured transaction");
        }
    }

    #[test]
    fn applies_native_credits_after_block_transactions() {
        let (checkpoint, source, root, bitcoin_height) = captured_checkpoint();
        let block = captured_first_block();
        let context = BitcoinBlockContext::at_height(bitcoin_height);
        let baseline = ChainState::from_checkpoint(Network::TESTNET, &checkpoint, source, root)
            .expect("open checkpoint")
            .execute_nakamoto_block_with_bitcoin_context(context, Some(source), &block)
            .expect("execute baseline block");
        let effects = NativeBlockEffects {
            credits: vec![NativeStxCredit {
                recipient: PrincipalData::parse("ST000000000000000000002AMW42H")
                    .expect("valid recipient"),
                amount: 1,
            }],
            liquid_supply_increase: 1,
        };
        let applied = ChainState::from_checkpoint(Network::TESTNET, checkpoint, source, root)
            .expect("open checkpoint")
            .execute_nakamoto_block_with_effects(context, Some(source), &block, effects)
            .expect("execute native effects");

        assert_ne!(baseline.execution.state_root, applied.execution.state_root);
    }

    #[test]
    fn tenure_accounting_applies_effects_at_the_recorded_height() {
        let recipient =
            PrincipalData::parse("ST000000000000000000002AMW42H").expect("valid recipient");
        let effects = NativeBlockEffects {
            credits: vec![NativeStxCredit {
                recipient,
                amount: 42,
            }],
            liquid_supply_increase: 42,
        };
        let mut accounting = TenureAccounting::default();

        accounting
            .record_matured_effects(100, effects.clone())
            .expect("record effects");
        assert_eq!(
            accounting.effects_for_tenure(Network::TESTNET, 99),
            Ok(NativeBlockEffects::default())
        );
        assert_eq!(
            accounting.effects_for_tenure(Network::TESTNET, 100),
            Ok(effects)
        );
        assert_eq!(
            accounting.effects_for_tenure(Network::TESTNET, 101),
            Err(TenureAccountingError::UnknownTenure(1)),
            "a payout with neither a checkpoint nor an executed tenure must fail loudly"
        );
        assert_eq!(
            accounting.record_matured_effects(100, NativeBlockEffects::default()),
            Err(TenureAccountingError::DuplicateCoinbaseHeight)
        );
    }

    /// The values a node's own snapshots hold: with a sortition in the parent
    /// burn block the accumulation is one bonus, and a burn block that chose
    /// nobody adds its emission and another bonus to the next winner.
    #[test]
    fn coinbase_accumulation_matches_a_node_snapshot() {
        let schedule = CoinbaseSchedule {
            mainnet: false,
            first_bitcoin_height: 0,
            initial_mining_bonus: 20_400_000,
        };

        assert_eq!(schedule.emission_at(440), 1_000_000_000);
        assert_eq!(schedule.accumulated_at(440, Some(439)), 20_400_000);
        // Burn block 441 chose nobody, so the tenure at 442 collects its
        // emission and a second bonus, as snapshot 442 records.
        assert_eq!(schedule.accumulated_at(442, Some(440)), 1_040_800_000);
        assert_eq!(schedule.accumulated_at(440, None), 0);
    }

    /// Rewards derived from executed tenures pay the coinbase to the tenure
    /// that earned it and the fees to the tenure before it.
    #[test]
    fn derived_effects_split_a_matured_tenure() {
        let recipient = |address: &str| PrincipalData::parse(address).expect("valid recipient");
        let mut accounting = TenureAccounting::default();
        accounting.record_earnings(
            10,
            TenureEarnings {
                recipient: recipient("ST24VB7FBXCBV6P0SRDSPSW0Y2J9XHDXNHW9Q8S7H"),
                coinbase: 7,
                fees: 3,
            },
        );
        accounting.record_earnings(
            11,
            TenureEarnings {
                recipient: recipient("ST2XAK68AR2TKBQBFNYSK9KN2AY9CVA91A7CSK63Z"),
                coinbase: 9,
                fees: 0,
            },
        );
        accounting.add_fees(11, 5);
        // A tenure seeded from a checkpoint keeps the fees the checkpoint
        // measured, because the blocks before the checkpoint are not replayed.
        accounting.seed_earnings(
            12,
            TenureEarnings {
                recipient: recipient("ST1J9R0VMA5GQTW65QVHW1KVSKD7MCGT27X37A551"),
                coinbase: 11,
                fees: 13,
            },
        );
        accounting.add_fees(12, 17);
        accounting.record_earnings(
            13,
            TenureEarnings {
                recipient: recipient("ST332DWHNM323264X869MKXFZABSE5WZ60EA07TJ1"),
                coinbase: 19,
                fees: 0,
            },
        );
        assert_eq!(
            accounting
                .effects_for_tenure(Network::TESTNET, 113)
                .expect("seeded and executed tenures")
                .credits[1]
                .amount,
            13
        );

        let effects = accounting
            .effects_for_tenure(Network::TESTNET, 111)
            .expect("both tenures were executed");
        assert_eq!(effects.liquid_supply_increase, 9);
        assert_eq!(
            effects.credits,
            vec![
                NativeStxCredit {
                    recipient: recipient("ST2XAK68AR2TKBQBFNYSK9KN2AY9CVA91A7CSK63Z"),
                    amount: 9,
                },
                NativeStxCredit {
                    recipient: recipient("ST24VB7FBXCBV6P0SRDSPSW0Y2J9XHDXNHW9Q8S7H"),
                    amount: 3,
                },
            ]
        );
    }

    #[test]
    fn loads_portable_tenure_accounting() {
        let accounting = TenureAccounting::from_json(
            br#"{
                "matured_effects": [{
                    "coinbase_height": 100,
                    "credits": [{
                        "recipient": "ST000000000000000000002AMW42H",
                        "amount": 42
                    }],
                    "liquid_supply_increase": 42
                }]
            }"#,
        )
        .expect("parse accounting checkpoint");

        assert_eq!(
            accounting
                .effects_for_tenure(Network::TESTNET, 100)
                .expect("checkpointed effects")
                .liquid_supply_increase,
            42
        );
        assert!(TenureAccounting::from_json(br#"{"matured_effects": [], "extra": 1}"#).is_err());
    }

    #[test]
    fn checks_strict_stx_postconditions_against_clarity_asset_accounting() {
        let transaction = decoded_transaction_with_postconditions(
            &token_transfer_payload(),
            2,
            &[stx_postcondition(1, 10)],
            0,
            0,
        );
        let origin = principal_from_address(
            StacksAddress::new(26, Hash160::from_bytes([0; 20])).expect("testnet address"),
        )
        .expect("origin");
        let mut assets = AssetMap::new();
        assets
            .add_stx_transfer(&origin, 10)
            .expect("record transfer");

        assert_eq!(
            check_postconditions(&transaction, &origin, &assets).expect("check postconditions"),
            None
        );

        let unchecked =
            decoded_transaction_with_postconditions(&token_transfer_payload(), 2, &[], 0, 0);
        assert!(
            check_postconditions(&unchecked, &origin, &assets)
                .expect("check postconditions")
                .is_some()
        );
    }

    #[test]
    fn failed_postcondition_rolls_back_contract_writes_and_consumes_nonce() {
        let mut chainstate = ChainState::new(Network::TESTNET).expect("create chainstate");
        chainstate
            .vm
            .begin_block(None, [2; 32])
            .expect("begin block");

        let deployment = decoded_transaction(
            &versioned_contract_payload(
                "counter",
                "(define-data-var counter uint u0) (define-public (increment) (begin (var-set counter (+ (var-get counter) u1)) (ok (var-get counter)))) (define-read-only (read-counter) (var-get counter))",
            ),
            0,
            0,
        );
        let deployed = chainstate
            .execute_transaction(&deployment, &ExecutionCost::ZERO)
            .expect("deploy contract");

        let failed = decoded_transaction_with_postconditions(
            &contract_call_payload_without_arguments("counter", "increment"),
            2,
            &[stx_postcondition(1, 1)],
            1,
            0,
        );
        let failed = chainstate
            .execute_transaction(&failed, &deployed.result.cost)
            .expect("abort transaction");
        assert!(matches!(
            failed.status,
            TransactionStatus::PostConditionAborted(_)
        ));
        assert!(!failed.committed);

        let get = decoded_transaction(
            &contract_call_payload_without_arguments("counter", "read-counter"),
            2,
            0,
        );
        let read = chainstate
            .execute_transaction(&get, &failed.result.cost)
            .expect("read contract state");
        assert_eq!(read.result.value, Some(clarity::vm::Value::UInt(0)));
    }

    #[test]
    fn runtime_failure_rolls_back_contract_writes_and_consumes_nonce() {
        let mut chainstate = ChainState::new(Network::TESTNET).expect("create chainstate");
        chainstate
            .vm
            .begin_block(None, [3; 32])
            .expect("begin block");

        let deployment = decoded_transaction(
            &versioned_contract_payload(
                "counter",
                "(define-data-var counter uint u0) (define-public (fail) (begin (var-set counter u1) (ok (/ u1 u0)))) (define-read-only (read-counter) (var-get counter))",
            ),
            0,
            0,
        );
        let deployed = chainstate
            .execute_transaction(&deployment, &ExecutionCost::ZERO)
            .expect("deploy contract");

        let failed = decoded_transaction(
            &contract_call_payload_without_arguments("counter", "fail"),
            1,
            0,
        );
        let failed = chainstate
            .execute_transaction(&failed, &deployed.result.cost)
            .expect("accept runtime failure");
        assert!(matches!(
            failed.status,
            TransactionStatus::RuntimeFailure(_)
        ));
        assert!(!failed.committed);
        assert_eq!(failed.result.value, Some(clarity::vm::Value::err_none()));

        let get = decoded_transaction(
            &contract_call_payload_without_arguments("counter", "read-counter"),
            2,
            0,
        );
        let read = chainstate
            .execute_transaction(&get, &failed.result.cost)
            .expect("read contract state");
        assert_eq!(read.result.value, Some(clarity::vm::Value::UInt(0)));
    }

    fn decoded_transaction(payload: &[u8], nonce: u64, fee: u64) -> Transaction {
        decoded_transaction_with_postconditions(payload, 1, &[], nonce, fee)
    }

    fn decoded_transaction_with_postconditions(
        payload: &[u8],
        post_condition_mode: u8,
        post_conditions: &[Vec<u8>],
        nonce: u64,
        fee: u64,
    ) -> Transaction {
        let mut bytes = vec![0x80];
        bytes.extend_from_slice(&0x8000_0000_u32.to_be_bytes());
        bytes.push(4);
        bytes.push(0);
        bytes.extend_from_slice(&[0; 20]);
        bytes.extend_from_slice(&nonce.to_be_bytes());
        bytes.extend_from_slice(&fee.to_be_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&[0; 65]);
        bytes.push(3);
        bytes.push(post_condition_mode);
        bytes.extend_from_slice(
            &u32::try_from(post_conditions.len())
                .expect("short post-condition list")
                .to_be_bytes(),
        );
        for post_condition in post_conditions {
            bytes.extend_from_slice(post_condition);
        }
        bytes.extend_from_slice(payload);
        Transaction::decode(&bytes).expect("decode transaction").0
    }

    fn stx_postcondition(condition: u8, amount: u64) -> Vec<u8> {
        let mut postcondition = vec![0, 1, condition];
        postcondition.extend_from_slice(&amount.to_be_bytes());
        postcondition
    }

    fn token_transfer_payload() -> Vec<u8> {
        let mut payload = vec![0, 5, 26];
        payload.extend_from_slice(&[0; 20]);
        payload.extend_from_slice(&0_u64.to_be_bytes());
        payload.extend_from_slice(&[0; 34]);
        payload
    }

    fn versioned_contract_payload(name: &str, source: &str) -> Vec<u8> {
        let mut payload = vec![6, 6, u8::try_from(name.len()).expect("short contract name")];
        payload.extend_from_slice(name.as_bytes());
        payload.extend_from_slice(
            &u32::try_from(source.len())
                .expect("short source")
                .to_be_bytes(),
        );
        payload.extend_from_slice(source.as_bytes());
        payload
    }

    fn contract_call_payload(contract: &str, function: &str, value: u128) -> Vec<u8> {
        let mut payload = vec![2, 26];
        payload.extend_from_slice(&[0; 20]);
        payload.push(u8::try_from(contract.len()).expect("short contract name"));
        payload.extend_from_slice(contract.as_bytes());
        payload.push(u8::try_from(function.len()).expect("short function name"));
        payload.extend_from_slice(function.as_bytes());
        payload.extend_from_slice(&1_u32.to_be_bytes());
        payload.push(1);
        payload.extend_from_slice(&value.to_be_bytes());
        payload
    }

    fn contract_call_payload_without_arguments(contract: &str, function: &str) -> Vec<u8> {
        let mut payload = vec![2, 26];
        payload.extend_from_slice(&[0; 20]);
        payload.push(u8::try_from(contract.len()).expect("short contract name"));
        payload.extend_from_slice(contract.as_bytes());
        payload.push(u8::try_from(function.len()).expect("short function name"));
        payload.extend_from_slice(function.as_bytes());
        payload.extend_from_slice(&0_u32.to_be_bytes());
        payload
    }
}
