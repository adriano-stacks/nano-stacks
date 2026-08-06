pub mod config;
pub mod miner;
pub mod runtime;
pub mod signer;
pub mod sortition;
pub mod staging;

use std::{fmt, path::Path, time::Duration};

use nano_bitcoin::BitcoinSource;
use nano_chainstate::{
    AppliedBlock, BitcoinBlockContext, ChainState, ChainStateError, NakamotoBlock,
    NakamotoBlockHeader, SignerSet, SignerSetError, TenureAccounting,
};
pub use nano_marf::{CheckpointAttestation, CheckpointManifest, CheckpointProvenance};
use nano_primitives::{Network, StacksBlockId, TrieHash};
use nano_sync::{FollowedTenure, Node, NodeView, PoxInfo, SyncClient, SyncError, TenureSource};

use crate::staging::{Staging, StagingError};

/// Executes a validated tenure stream from an imported checkpoint state.
/// How far back a burn-view walk goes before giving up: a tenure is bounded by
/// the Bitcoin block that ends it, so this only has to outlast one.
const TENURE_WALK_LIMIT: usize = 512;

/// How many burn blocks behind the current one to make readable from Clarity.
/// An sBTC sweep is confirmed within a few, and a Bitcoin header is cheap.
const BURN_HEADER_WINDOW: u64 = 32;

#[derive(Debug)]
pub struct CheckpointExecutor<S> {
    chainstate: ChainState,
    /// The sortitions this node derives for itself, when it has the history to.
    sortition: Option<crate::sortition::SortitionTracker>,
    /// Where the derived chain is written down, so a restart resumes it.
    sortition_state: Option<std::path::PathBuf>,
    /// The burn height a reported sortition gap was last complained about, so
    /// the complaint is made once rather than for every block behind it.
    sortition_gap: Option<u64>,
    tip: NakamotoBlock,
    /// The Bitcoin height the sealed tip was executed under.
    ///
    /// A block header carries the burn it spent, not the height it landed at,
    /// and nothing else records it per block — so the executor keeps it, since
    /// it is what a caller asking how far this node has come actually means.
    bitcoin_height: u64,
    /// The burn view the current tenure is standing on, which a tenure change
    /// states and the blocks after it inherit.
    bitcoin_view: Option<nano_primitives::ConsensusHash>,
    /// The sortition last fetched, and the burn view it describes.
    ///
    /// A sortition belongs to a *burn* block, and many Stacks blocks share one
    /// burn view — so asking a peer once per Stacks block reissues an identical
    /// request for an identical answer. At 0.44 s of round trip to a hosted API
    /// that was the whole cost of a replay: the process sat at 16% of one core
    /// waiting on the network while 40,000 already-staged blocks queued behind it.
    ///
    /// One entry rather than a map, because a replay walks burn views in order and
    /// never looks back. Keeping the earlier ones would be a leak dressed as a
    /// cache — a catch-up crosses thousands of them.
    sortition_view: Option<(nano_primitives::ConsensusHash, nano_sync::SortitionInfo)>,
    /// Where to announce the blocks this node executes.
    ///
    /// An observer's whole purpose is to see what a node *executed*, so this
    /// belongs on the executor rather than beside it: a `new_block` sent from
    /// anywhere else would announce a block that had only been downloaded.
    observers: Option<nano_rpc::EventDispatcher>,
    bitcoin: S,
}

/// What this node derived for the burn block a Stacks block executes under.
///
/// Both fields are validation inputs and nothing else: `check_tenure_vrf` reads
/// them and no Clarity word does, so filling them from the local burnchain rather
/// than from a peer moves no state root. Taking them from the peer would mean
/// trusting the peer for the input that decides whether a tenure is the one the
/// network elected, which is the whole point of deriving sortitions here.
#[derive(Clone, Copy, Debug)]
struct LocalSortition {
    sortition_hash: [u8; 32],
    /// The winning commitment's leader-key VRF public key, when this node can
    /// name the winner without leaning on the burn distribution.
    winner_vrf_public_key: Option<[u8; 32]>,
    /// The block-signing hash that key was registered with, when the registry
    /// carries one for it.
    winner_signing_key_hash: Option<[u8; 20]>,
}

/// Where a round of execution spent its time.
///
/// A follower is either executing or waiting, and a guess at which is worth
/// nothing: this counts both, per phase, and the round says so when asked.
#[derive(Default)]
struct ExecutionTiming {
    /// Distinct burn views the round's blocks stood on, which bounds how many
    /// sortition requests caching could ever save.
    views: usize,
    sortition: Duration,
    local: Duration,
    headers: Duration,
    coinbase: Duration,
    execution: Duration,
    /// Building each event payload and handing it to the observers' drain task.
    dispatch: Duration,
    staging: Duration,
}

/// How often a timed round says where its seconds went.
const TIMING_INTERVAL: usize = 25;

impl ExecutionTiming {
    fn report(&self, executed: usize) {
        if executed == 0 || std::env::var_os("NANO_TIMING").is_none() {
            return;
        }
        let (requests, waited) = nano_sync::request_stats();
        println!(
            "timing over {executed} blocks on {} views: sortition {:.2}s, local {:.2}s, \
             headers {:.2}s, coinbase {:.2}s, execution {:.2}s, \
             dispatch {:.2}s, staging {:.2}s; {requests} peer requests so far, {waited:.1}s waited",
            self.views,
            self.sortition.as_secs_f64(),
            self.local.as_secs_f64(),
            self.headers.as_secs_f64(),
            self.coinbase.as_secs_f64(),
            self.execution.as_secs_f64(),
            self.dispatch.as_secs_f64(),
            self.staging.as_secs_f64(),
        );
    }
}

/// The `PoX` payout calendar a node's own constants imply.
///
/// A commitment's burn is the sum of what it paid its `PoX` recipients, and how
/// many outputs those are moves with the reward cycle and again at the waterfall
/// — so this decides every candidate's weight in the distribution. The waterfall
/// opens with the reward cycle *after* the one pox-5 activates in
/// (`burnchains/mod.rs:636`).
///
/// These are the same constants every `BitcoinBlockContext` is already built
/// from, so reading them here adds no new trust.
fn payout_schedule(pox: &PoxInfo) -> Option<nano_sortition::PayoutSchedule> {
    let length = u64::from(pox.prepare_phase_length) + u64::from(pox.reward_phase_length);
    let waterfall = pox.pox_5_activation_height.map(|activation| {
        pox.first_bitcoin_height
            + (pox.reward_cycle(u64::from(activation)) + 1) * length
    });
    let cycles =
        nano_sortition::RewardCycleSchedule::new(pox.first_bitcoin_height, length, waterfall).ok()?;
    let schedule =
        nano_sortition::PayoutSchedule::new(cycles, u64::from(pox.prepare_phase_length)).ok()?;
    // Where epoch 4.0 begins collapses the mining window for the six blocks after
    // it — a block weighed over a window that reaches into the previous epoch is
    // weighed over that block alone, and mainnet's burn 960,230 and 960,233 name
    // different winners under the two rules. `validate_epochs` makes pox-5's
    // activation the epoch 4.0 start, so this is the same field the waterfall
    // above already reads and nothing new is configured.
    Some(pox.pox_5_activation_height.map_or(schedule, |activation| {
        schedule.activating_epoch_four_at(u64::from(activation))
    }))
}

/// Say where a locally derived sortition and the peer's answer part company.
///
/// Reported rather than enforced while the derivation is being brought up, and
/// per field, because which field disagrees names which arithmetic is wrong: the
/// consensus hash covers the operation set, the burn total and the `PoX` history
/// together, while the VRF seed is the winning commitment's alone.
fn report_disagreements(local: &nano_sortition::SortitionSnapshot, peer: &nano_sync::SortitionInfo) {
    if local.consensus_hash != peer.consensus_hash {
        eprintln!(
            "locally derived consensus hash at burn {} is {} where the peer says {}",
            peer.bitcoin_height, local.consensus_hash, peer.consensus_hash
        );
    }
    if let Some(seed) = peer.vrf_seed
        && local.winner_vrf_seed != Some(seed)
    {
        eprintln!(
            "locally derived VRF seed at burn {} is {:?} where the peer says {}",
            peer.bitcoin_height,
            local.winner_vrf_seed.map(hex::encode),
            hex::encode(seed)
        );
    }
}

/// Where a descent stops: the block this node has executed, by identity and by
/// height, because a batch can step over the one without reaching the other.
#[derive(Clone, Copy, Debug)]
struct Stop {
    block_id: StacksBlockId,
    height: u64,
}

/// How much one round of catching up is allowed to do.
#[derive(Clone, Copy, Debug)]
pub struct CatchUpBudget {
    /// Blocks this round will fetch from the peer.
    pub fetch: usize,
    /// Blocks this round will execute and seal.
    pub execute: usize,
}

/// What one round of catching up actually did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CatchUpRound {
    /// The block a fork switch stood on, when one happened.
    pub reorganized: Option<[u8; 32]>,
    pub fetched: usize,
    pub executed: usize,
    /// Blocks fetched but not yet executed.
    pub staged: u64,
    /// Whether the peer asked this node to slow down, which ends a round
    /// successfully rather than discarding it.
    pub rate_limited: bool,
}

/// A follower that executes each accepted tenure update from a checkpointed state.
#[derive(Debug)]
pub struct ExecutingNode<S> {
    node: Node,
    executor: CheckpointExecutor<S>,
    executed_view: Option<NodeView>,
}

#[derive(Debug)]
pub enum NodeExecutionError {
    Sync(SyncError),
    /// A tenure the descent asked for could not be used, and which one.
    Descent {
        tenure: StacksBlockId,
        error: SyncError,
    },
    Execution(CheckpointExecutionError),
    Staging(StagingError),
    MissingView,
}

impl From<StagingError> for NodeExecutionError {
    fn from(error: StagingError) -> Self {
        Self::Staging(error)
    }
}

impl fmt::Display for NodeExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sync(error) => write!(formatter, "node synchronization failed: {error}"),
            Self::Descent { tenure, error } => {
                write!(formatter, "descending through tenure {tenure} failed: {error}")
            }
            Self::Execution(error) => write!(formatter, "node execution failed: {error}"),
            Self::Staging(error) => write!(formatter, "node staging failed: {error}"),
            Self::MissingView => formatter.write_str("node has no complete validated view"),
        }
    }
}

impl std::error::Error for NodeExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sync(error) | Self::Descent { error, .. } => Some(error),
            Self::Execution(error) => Some(error),
            Self::Staging(error) => Some(error),
            Self::MissingView => None,
        }
    }
}

impl From<SyncError> for NodeExecutionError {
    fn from(error: SyncError) -> Self {
        Self::Sync(error)
    }
}

impl From<CheckpointExecutionError> for NodeExecutionError {
    fn from(error: CheckpointExecutionError) -> Self {
        Self::Execution(error)
    }
}

#[derive(Debug)]
pub enum CheckpointExecutionError {
    ChainState(ChainStateError),
    Bitcoin(String),
    Link(String),
}

impl fmt::Display for CheckpointExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChainState(error) => write!(formatter, "checkpoint execution failed: {error}"),
            Self::Bitcoin(error) => write!(formatter, "Bitcoin operation loading failed: {error}"),
            Self::Link(error) => {
                write!(formatter, "checkpoint execution chain link failed: {error}")
            }
        }
    }
}

impl std::error::Error for CheckpointExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ChainState(error) => Some(error),
            Self::Bitcoin(_) | Self::Link(_) => None,
        }
    }
}

impl From<ChainStateError> for CheckpointExecutionError {
    fn from(error: ChainStateError) -> Self {
        Self::ChainState(error)
    }
}

/// A checkpointed Clarity state, and what is needed to execute from it.
#[derive(Clone, Debug)]
pub struct Checkpoint<P> {
    /// The chain this state belongs to, which fixes how it is executed.
    pub network: Network,
    /// Path to the checkpoint's MARF.
    pub path: P,
    /// The MARF state the checkpoint was taken at.
    pub source: [u8; 32],
    /// The state root that state is published under.
    pub state_root: TrieHash,
    /// The matured native rewards the checkpoint still owes, if it records them.
    pub accounting: Option<TenureAccounting>,
}

impl<P> Checkpoint<P> {
    /// Take the state and root to import from what the checkpoint publishes.
    ///
    /// Attest the manifest first: `Checkpoint` carries no evidence, so whatever
    /// this is built from is what the node ends up believing.
    #[must_use]
    pub const fn from_manifest(network: Network, path: P, manifest: &CheckpointManifest) -> Self {
        Self {
            network,
            path,
            source: manifest.source_state_id,
            state_root: manifest.state_index_root,
            accounting: None,
        }
    }
}

/// Why a checkpoint is not the state the signed header at its height published.
#[derive(Debug)]
pub enum CheckpointTrustError {
    Height { claimed: u64, header: u64 },
    StateId { claimed: [u8; 32], header: [u8; 32] },
    StateRoot { claimed: TrieHash, header: TrieHash },
    Signers(SignerSetError),
    Provenance(nano_marf::CheckpointError),
}

impl fmt::Display for CheckpointTrustError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Height { claimed, header } => write!(
                formatter,
                "checkpoint claims Stacks height {claimed}, attesting header is at {header}"
            ),
            Self::StateId { claimed, header } => write!(
                formatter,
                "checkpoint claims state {claimed:02x?}, attesting header sealed {header:02x?}"
            ),
            Self::StateRoot { claimed, header } => write!(
                formatter,
                "checkpoint claims state root {claimed}, attesting header published {header}"
            ),
            Self::Signers(error) => {
                write!(formatter, "no reward set attested the checkpoint: {error}")
            }
            Self::Provenance(error) => {
                write!(formatter, "checkpoint provenance was refused: {error}")
            }
        }
    }
}

impl std::error::Error for CheckpointTrustError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Signers(error) => Some(error),
            Self::Provenance(error) => Some(error),
            Self::Height { .. } | Self::StateId { .. } | Self::StateRoot { .. } => None,
        }
    }
}

impl From<SignerSetError> for CheckpointTrustError {
    fn from(error: SignerSetError) -> Self {
        Self::Signers(error)
    }
}

impl From<nano_marf::CheckpointError> for CheckpointTrustError {
    fn from(error: nano_marf::CheckpointError) -> Self {
        Self::Provenance(error)
    }
}

/// Check a checkpoint against the signed Nakamoto header that published its state.
///
/// `signer_signature_hash` is taken over a preimage that contains
/// `state_index_root`, so a reward set that signed the header signed the root
/// too, and `block_id` binds both to one state. That moves the checkpoint's
/// root from something the operator asserts to something the chain's own
/// signers put threshold weight behind — as long as `signers` is the reward set
/// of that cycle obtained independently of the checkpoint. See
/// `docs/checkpoint-trust.md`.
pub fn attest_checkpoint(
    manifest: &CheckpointManifest,
    header: &NakamotoBlockHeader,
    signers: &SignerSet,
) -> Result<CheckpointAttestation, CheckpointTrustError> {
    if header.chain_length != manifest.stacks_height {
        return Err(CheckpointTrustError::Height {
            claimed: manifest.stacks_height,
            header: header.chain_length,
        });
    }
    if *header.block_id().as_bytes() != manifest.source_state_id {
        return Err(CheckpointTrustError::StateId {
            claimed: manifest.source_state_id,
            header: *header.block_id().as_bytes(),
        });
    }
    if header.state_index_root != manifest.state_index_root {
        return Err(CheckpointTrustError::StateRoot {
            claimed: manifest.state_index_root,
            header: header.state_index_root,
        });
    }
    Ok(CheckpointAttestation {
        attesting_block_id: *header.block_id().as_bytes(),
        signer_weight: signers.verify(header)?,
        approval_threshold: signers.approval_threshold()?,
    })
}

/// Attest a checkpoint and record it as the origin of a state directory.
///
/// This is the one place where what the operator configured, what the
/// checkpoint publishes and what the reward set signed are made to agree, and
/// where that agreement is written down — so a restart does not have to take
/// the configuration's word for it a second time.
pub fn adopt_checkpoint(
    state_directory: impl AsRef<Path>,
    manifest: &CheckpointManifest,
    header: &NakamotoBlockHeader,
    signers: &SignerSet,
) -> Result<CheckpointAttestation, CheckpointTrustError> {
    let attestation = attest_checkpoint(manifest, header, signers)?;
    CheckpointProvenance {
        checkpoint: manifest.clone(),
        attestation: Some(attestation),
    }
    .record(state_directory)?;
    Ok(attestation)
}

impl<S> CheckpointExecutor<S>
where
    S: BitcoinSource,
    S::Error: fmt::Display,
{
    /// Import a checkpoint and apply its first known descendant as the execution anchor.
    pub fn from_checkpoint(
        checkpoint: Checkpoint<impl AsRef<Path>>,
        anchor: NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
        bitcoin: S,
    ) -> Result<Self, CheckpointExecutionError> {
        let Checkpoint {
            network,
            path,
            source,
            state_root,
            accounting,
        } = checkpoint;
        let mut chainstate = ChainState::from_checkpoint(network, path, source, state_root)?;
        if let Some(accounting) = accounting {
            *chainstate.accounting_mut() = accounting;
        }
        Self::from_chainstate(chainstate, anchor, bitcoin_context, bitcoin)
    }

    /// Apply the checkpoint's first known descendant to an already-open state.
    pub fn from_chainstate(
        mut chainstate: ChainState,
        anchor: NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
        mut bitcoin: S,
    ) -> Result<Self, CheckpointExecutionError> {
        let operations = bitcoin
            .block_at(bitcoin_context.height)
            .map_err(|error| CheckpointExecutionError::Bitcoin(error.to_string()))?;
        let parent = chainstate.tip();
        chainstate.append_nakamoto_block_with_bitcoin_operations(
            bitcoin_context,
            &operations.operations,
            parent,
            &anchor,
        )?;
        Ok(Self {
            chainstate,
            sortition: None,
            sortition_state: None,
            sortition_gap: None,
            tip: anchor,
            bitcoin_height: bitcoin_context.height,
            bitcoin_view: None,
            sortition_view: None,
            observers: None,
            bitcoin,
        })
    }

    /// Continue from a chainstate that already holds the blocks up to `tip`.
    ///
    /// A durable chainstate outlives the process, so a restart adopts the block
    /// its state was sealed at instead of importing a checkpoint again.
    /// The Bitcoin height is not on disk, so a resumed executor reports zero
    /// until it applies a block of its own — an honest unknown rather than a
    /// height it made up.
    pub const fn resume(chainstate: ChainState, tip: NakamotoBlock, bitcoin: S) -> Self {
        Self {
            chainstate,
            sortition: None,
            sortition_state: None,
            sortition_gap: None,
            tip,
            bitcoin_height: 0,
            bitcoin_view: None,
            sortition_view: None,
            observers: None,
            bitcoin,
        }
    }

    /// Apply the blocks in a followed tenure that extend the current execution tip.
    pub fn apply_followed_tenure(
        &mut self,
        tenure: &FollowedTenure,
        pox: &PoxInfo,
    ) -> Result<Vec<AppliedBlock>, CheckpointExecutionError> {
        let mut bitcoin_context = pox.bitcoin_context();
        bitcoin_context.height = tenure.sortition.bitcoin_height;
        bitcoin_context.burn_header_hash = *tenure.sortition.bitcoin_block_hash.as_bytes();
        // `get-tenure-info? time` and `vrf-seed` read these back. Left zero they
        // answer zero, which is not a failure a replay notices — it is a wrong
        // number in a receipt, and a state root that differs for no visible
        // reason. The sortition already carries both.
        bitcoin_context.burn_block_time = tenure.sortition.bitcoin_timestamp;
        bitcoin_context.vrf_seed = tenure.sortition.vrf_seed.unwrap_or_default();
        // As in `execute_staged`: the tenure VRF rules read these two, and they
        // have to be this node's own answers rather than the peer's. The burn
        // total comes from the first block of the tenure, which is the one that
        // can start it.
        let bitcoin_spent = tenure
            .blocks
            .first()
            .map_or(0, |block| block.header.bitcoin_spent);
        if let Some(local) = self.local_sortition(pox, &tenure.sortition, bitcoin_spent) {
            bitcoin_context.sortition_hash = local.sortition_hash;
            bitcoin_context.winner_vrf_public_key = local.winner_vrf_public_key;
            bitcoin_context.winner_signing_key_hash = local.winner_signing_key_hash;
        }
        self.seed_burn_headers(tenure.sortition.bitcoin_height);
        let current_tip = self.tip.block_id();
        let blocks = tenure
            .blocks
            .iter()
            .skip_while(|block| block.block_id() != current_tip);
        let mut blocks = blocks.peekable();
        if blocks.peek().is_some() {
            blocks.next();
        }
        let mut applied = Vec::new();
        let operations = self
            .bitcoin
            .block_at(bitcoin_context.height)
            .map_err(|error| CheckpointExecutionError::Bitcoin(error.to_string()))?;
        for block in blocks {
            applied.push(self.apply_with_operations(
                block,
                bitcoin_context,
                &operations.operations,
            )?);
        }
        Ok(applied)
    }

    /// Validate and execute one direct descendant of the current execution tip.
    pub fn apply(
        &mut self,
        block: &NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
    ) -> Result<AppliedBlock, CheckpointExecutionError> {
        let operations = self
            .bitcoin
            .block_at(bitcoin_context.height)
            .map_err(|error| CheckpointExecutionError::Bitcoin(error.to_string()))?;
        self.apply_with_operations(block, bitcoin_context, &operations.operations)
    }

    /// Validate and execute one direct descendant with decoded Bitcoin operations.
    pub fn apply_with_operations(
        &mut self,
        block: &NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
        operations: &[nano_bitcoin::BitcoinOperation],
    ) -> Result<AppliedBlock, CheckpointExecutionError> {
        block.validate_successor(&self.tip.header).map_err(|error| {
            CheckpointExecutionError::Link(format!(
                "{error}: block {} at height {} names parent {}, but this node is at {} of height {}",
                block.block_id(),
                block.header.chain_length,
                block.header.parent_block_id,
                self.tip.block_id(),
                self.tip.header.chain_length,
            ))
        })?;
        let applied = self
            .chainstate
            .append_nakamoto_block_with_bitcoin_operations(
                bitcoin_context,
                operations,
                Some(*self.tip.block_id().as_bytes()),
                block,
            )?;
        self.tip = block.clone();
        self.bitcoin_height = bitcoin_context.height;
        Ok(applied)
    }

    /// The Bitcoin height the sealed tip was executed under.
    ///
    /// A resumed executor has not applied a block yet, so it asks the header it
    /// wrote down for the tip rather than reporting an unknown.
    #[must_use]
    pub fn bitcoin_height(&self) -> u64 {
        if self.bitcoin_height > 0 {
            return self.bitcoin_height;
        }
        self.chainstate
            .recorded_header(*self.tip.block_id().as_bytes())
            .map_or(0, |header| u64::from(header.burn_block_height))
    }

    /// The consensus hash that names the reward cycle this node's burn view sits in.
    ///
    /// This is how a `GetNakamotoInv` names a cycle — by the consensus hash of its
    /// *first sortition* — and it comes from this node's own derived sortition chain
    /// rather than from a peer, because a cycle identifier taken from a peer would
    /// make that peer's view of the burnchain the thing nano's inventory requests are
    /// keyed on.
    ///
    /// The boundary is found with the **cycle-keyed** rule and not the tip-keyed one.
    /// `starts_reward_cycle` is waterfall-aware: a cycle opens at offset 0 once the
    /// waterfall is on and at offset 1 before it, so a node that decided from where
    /// its tip happens to sit would move the boundary part-way through a prepare
    /// phase and name a cycle no peer recognises.
    #[must_use]
    pub fn cycle_start_consensus_hash(&self, pox: &PoxInfo) -> Option<nano_primitives::ConsensusHash> {
        self.sortition
            .as_ref()?
            .consensus_hash_at(self.cycle_start_height(pox)?)
    }

    /// The Bitcoin height the reward cycle this node's burn view sits in opens at.
    fn cycle_start_height(&self, pox: &PoxInfo) -> Option<u64> {
        let payouts = payout_schedule(pox)?;
        let mut height = self
            .bitcoin_height()
            .min(self.sortition.as_ref()?.tip().bitcoin_height);
        // The cycle is at most one length back, so the walk is bounded by the cycle
        // and cannot run away on a schedule that never says yes.
        let length = u64::from(pox.prepare_phase_length) + u64::from(pox.reward_phase_length);
        for _ in 0..=length {
            if payouts.starts_reward_cycle(height) {
                return Some(height);
            }
            height = height.checked_sub(1)?;
        }
        None
    }

    /// Which tenures of the cycle being walked this node has executed, for a peer
    /// that asks.
    ///
    /// A bit is set only where this node executed the tenure that began at that
    /// sortition and can therefore serve its blocks, so the vector says less than the
    /// node knows rather than more: an unset bit means "do not ask me", which costs a
    /// peer nothing, while a set bit it could not honour would cost that peer a failed
    /// fetch. It is the conservative direction on purpose.
    ///
    /// It says *much* less than the node knows, and the reason is worth recording:
    /// the executed ledger reaches `REORG_REACH` blocks back, so the honest answer
    /// covers the recent end of the cycle and nothing older. Answering fully needs a
    /// durable consensus-hash-to-tenure index, which is a chainstate change and not
    /// this one — but answering *partially and truthfully* is already strictly better
    /// than the `Nack` that came before it, because a nack tells a peer to give up on
    /// nano for the whole cycle.
    #[must_use]
    pub fn tenure_inventory(
        &self,
        pox: &PoxInfo,
    ) -> Option<(nano_primitives::ConsensusHash, nano_primitives::BitVec<2100>)> {
        let tracker = self.sortition.as_ref()?;
        let start = self.cycle_start_height(pox)?;
        let length = u64::from(pox.prepare_phase_length) + u64::from(pox.reward_phase_length);
        let executed: std::collections::HashSet<nano_primitives::ConsensusHash> =
            self.chainstate.executed_tenures().into_iter().collect();
        let mut tenures = nano_primitives::BitVec::<2100>::zeros(u16::try_from(length).ok()?).ok()?;
        // Walked by offset rather than by searching the history for each executed
        // tenure: the history is a quarter of a million hashes long, and the cycle is
        // two thousand.
        for offset in 0..length {
            if let Some(hash) = tracker.consensus_hash_at(start + offset)
                && executed.contains(&hash)
            {
                tenures.set(u16::try_from(offset).ok()?, true).ok()?;
            }
        }
        Some((tracker.consensus_hash_at(start)?, tenures))
    }

    /// Ask a peer for something, waiting out the limits it answers with.
    ///
    /// A node writing down the headers it is missing cannot execute anything
    /// until it has them, so it has nothing better to do than wait — unlike a
    /// round of following, which gives up early and asks again next poll.
    async fn wait_out_limits<T, F, Ask>(mut ask: F) -> Result<T, SyncError>
    where
        F: FnMut() -> Ask,
        Ask: std::future::Future<Output = Result<T, SyncError>>,
    {
        let mut wait = std::time::Duration::from_secs(1);
        loop {
            match ask().await {
                Err(error)
                    if error.is_rate_limited() && wait < std::time::Duration::from_secs(32) =>
                {
                    tokio::time::sleep(wait).await;
                    wait = wait.saturating_mul(2);
                }
                outcome => return outcome,
            }
        }
    }

    /// Write down the headers of blocks this node executed before it kept any.
    ///
    /// A contract reading the block it is standing on gets `none` otherwise,
    /// and the transaction carrying it fails against a network that answered.
    /// Refetching the blocks is far cheaper than executing them again.
    pub async fn backfill_headers(
        &mut self,
        node: &SyncClient,
        pox: &PoxInfo,
        from: [u8; 32],
    ) -> Result<usize, NodeExecutionError> {
        if self
            .chainstate
            .has_recorded_header(*self.tip.block_id().as_bytes())
        {
            return Ok(0);
        }
        let mut walk = Vec::new();
        let mut cursor = self.tip.block_id();
        while *cursor.as_bytes() != from {
            let block = match Self::wait_out_limits(|| node.block(cursor)).await {
                Ok(block) => block,
                // A peer that is rate limiting has not refused: what was walked
                // stands, and the next start carries on from the tip again.
                Err(error) if error.is_rate_limited() => {
                    // Says so rather than passing silently: the walk stops at
                    // the tip's own header being present, so a run cut short
                    // here leaves the deeper ones for the checkpoint export.
                    eprintln!(
                        "the peer cut the header backfill short at {cursor}, \
                         leaving the blocks below it unrecorded"
                    );
                    break;
                }
                Err(error) => return Err(error.into()),
            };
            cursor = block.header.parent_block_id;
            walk.push(block);
        }
        let mut recorded = 0;
        for block in walk.iter().rev() {
            let sortition =
                match Self::wait_out_limits(|| node.sortition(block.header.consensus_hash)).await {
                Ok(sortition) => sortition,
                Err(error) if error.is_rate_limited() => break,
                Err(error) => return Err(error.into()),
            };
            let mut bitcoin_context = pox.bitcoin_context();
            bitcoin_context.height = sortition.bitcoin_height;
            bitcoin_context.burn_header_hash = *sortition.bitcoin_block_hash.as_bytes();
            self.seed_burn_headers(sortition.bitcoin_height);
            bitcoin_context.burn_block_time = sortition.bitcoin_timestamp;
            bitcoin_context.vrf_seed = sortition.vrf_seed.unwrap_or_default();
            // No sortition hash or leader key here, deliberately: this walk
            // writes down headers for blocks already executed and validates
            // nothing, and a header records neither field. Deriving sortitions
            // backwards from the tip would also run the tracker in the one
            // direction its consensus-hash skip-list cannot go.
            self.chainstate
                .backfill_block_header(block, bitcoin_context)
                .map_err(CheckpointExecutionError::from)?;
            recorded += 1;
        }
        Ok(recorded)
    }

    /// Extend the staged descent toward this node's tip, then execute what it
    /// can, committing as it goes.
    ///
    /// Replaces a walk that buffered the whole gap in memory and executed only
    /// once it reached this node's tip. Against mainnet that gap was twenty
    /// thousand blocks and the walk never once completed: a single rate limit
    /// anywhere in it discarded every block fetched, and the next round began
    /// again from a tip that had since moved. Here the descent is on disk, so a
    /// round that ends early is progress kept, and execution runs in bounded
    /// chunks from whatever staging already holds.
    pub async fn catch_up(
        &mut self,
        node: &SyncClient,
        history: &mut TenureSource,
        pox: &PoxInfo,
        staging: &Staging,
        budget: CatchUpBudget,
    ) -> Result<CatchUpRound, NodeExecutionError> {
        let mut round = CatchUpRound::default();
        let peer_tip = node.tenure_info().await?.tip_block_id;

        // What the peer added since the last round sits above everything
        // staged, so this stops as soon as it meets a block already held.
        let executed_tip = self.tip.block_id();
        let executed_height = self.tip.header.chain_length;
        // A descent that overshot leaves blocks below the executed tip, which
        // no round will ever execute and every round would otherwise resume
        // from.
        staging.remove_to(executed_height)?;
        let mut fetched = Self::descend(
            history,
            staging,
            peer_tip,
            Stop {
                block_id: executed_tip,
                height: executed_height,
            },
            budget.fetch,
            &mut round,
        )
        .await?;
        // The descent itself continues from the furthest it has reached, which
        // is what makes a rate-limited round cost nothing but time.
        if let Some(resume) = staging.descent_resumes_at()? {
            fetched += Self::descend(
                history,
                staging,
                resume,
                Stop {
                    block_id: executed_tip,
                    height: executed_height,
                },
                budget.fetch.saturating_sub(fetched),
                &mut round,
            )
            .await?;
        }
        round.fetched = fetched;
        round.executed = self.execute_staged(node, pox, staging, budget.execute).await?;
        // A descent that fetched blocks and executed none, while the peer is
        // ahead, is what a fork looks like from here: the peer's chain walked
        // past this node's tip on another branch, so nothing staged extends it
        // and no later round ever will. Standing where the two chains agree is
        // what turns that from a stall into a reorganisation.
        if round.executed == 0
            && round.fetched > 0
            && let Ok(peer) = node.tenure_info().await
            && let Some(resume) = self.switch_to_fork(node, peer.consensus_hash).await?
        {
            round.reorganized = Some(resume);
            staging.clear()?;
        }
        round.staged = staging.len()?;
        Ok(round)
    }

    /// Walk back from `from`, staging each block, until this node's tip or a
    /// block already staged is reached, or the budget runs out.
    async fn descend(
        history: &mut TenureSource,
        staging: &Staging,
        from: StacksBlockId,
        until: Stop,
        budget: usize,
        round: &mut CatchUpRound,
    ) -> Result<usize, NodeExecutionError> {
        let mut cursor = from;
        let mut fetched = 0;
        while cursor != until.block_id && fetched < budget {
            if staging.holds(cursor)? {
                break;
            }
            // A whole tenure per request rather than a block per request: over
            // a gap of tens of thousands of blocks that is the difference
            // between catching up and being rate limited forever.
            // Spread over every peer discovery found, not sent down one client.
            // Twenty thousand blocks through one hosted API is a rate limit deciding
            // how fast nano can catch up, which is the thing joining the peer network
            // was for.
            let blocks = match history.blocks_of_tenure(cursor).await {
                Ok(blocks) => blocks,
                // A peer that is rate limiting has not failed, and neither has
                // the round: everything staged so far still stands. Reported only
                // when there is nobody left to ask, because with peers still willing
                // the right response is to ask one of them rather than to wait.
                Err(error) if error.is_rate_limited() && history.exhausted() => {
                    round.rate_limited = true;
                    break;
                }
                // Naming the tenure makes the next undecodable block one curl
                // and one offline decode away, instead of a node restart.
                Err(error) => {
                    return Err(NodeExecutionError::Descent {
                        tenure: cursor,
                        error,
                    });
                }
            };
            let lowest = blocks
                .iter()
                .min_by_key(|block| block.header.chain_length)
                .ok_or(NodeExecutionError::Sync(SyncError::EmptyTenure))?;
            let next = lowest.header.parent_block_id;
            for block in &blocks {
                staging.put(block)?;
            }
            fetched += blocks.len();
            // A tenure arrives whole, so a batch straddling the executed tip
            // never lands on it exactly — the cursor would step over it and
            // descend forever into history this node already has.
            if lowest.header.chain_length <= until.height {
                break;
            }
            // A peer that answers with only the block asked for still moves the
            // descent along, because that block's parent is the next cursor.
            if next == cursor {
                break;
            }
            cursor = next;
        }
        Ok(fetched)
    }

    /// Tell the observers what this node just executed.
    ///
    /// Only the fields a follower can answer from what it holds are filled in;
    /// the rest are left at their defaults rather than invented, since an
    /// observer comparing nano with stacks-core is better served by a field that
    /// is plainly absent than by one that is confidently wrong.
    fn announce_block(
        &self,
        block: &NakamotoBlock,
        applied: &AppliedBlock,
        context: BitcoinBlockContext,
    ) {
        let Some(observers) = self.observers.as_ref() else {
            return;
        };
        let event = nano_rpc::BlockEventContext {
            parent_block_hash: nano_primitives::BlockHeaderHash::from_bytes(
                *block.header.parent_block_id.as_bytes(),
            ),
            bitcoin_block_hash: nano_primitives::BitcoinHeaderHash::from_bytes(
                context.burn_header_hash,
            ),
            bitcoin_height: context.height,

            v1_unlock_height: context.v1_unlock_height,
            v2_unlock_height: context.v2_unlock_height,
            v3_unlock_height: context.v3_unlock_height,
            pox_5_activation_height: context.pox_5_activation_height,
            ..Default::default()
        };
        // Queued rather than posted: `dispatch` hands the payload to the
        // observer's own drain task, so an observer that is slow or gone costs
        // this loop the serialization and nothing else.
        observers.dispatch(
            nano_rpc::EventKind::NewBlock,
            &nano_rpc::new_block_payload(block, applied, &event),
        );
    }

    /// Announce every block this node executes to these observers.
    ///
    /// An observer's whole purpose is to see what a node *executed*, so this
    /// belongs on the executor: a `new_block` sent from anywhere else would be
    /// announcing a block that had only been downloaded.
    pub fn announce_to(&mut self, observers: nano_rpc::EventDispatcher) {
        self.observers = Some(observers);
    }

    /// Take over a derived sortition chain, and say where to keep it.
    ///
    /// A chain that is not written down is re-derived from the checkpoint's burn
    /// anchor on every start, one Bitcoin block fetch at a time, over a run that
    /// grows for as long as the chain does.
    pub fn track_sortitions(
        &mut self,
        tracker: crate::sortition::SortitionTracker,
        state: std::path::PathBuf,
    ) {
        self.sortition = Some(tracker);
        self.sortition_state = Some(state);
    }

    /// Derive the sortition a block executes under, from this node's burnchain.
    ///
    /// The tracker walks every burn block up to the one the block stands on —
    /// nothing is skipped, because a consensus hash mixes the ones behind it and
    /// a height left out changes every hash from there on. That walk is what the
    /// previous version of this could not do: it advanced exactly one block and
    /// bailed out otherwise, so on mainnet, where the checkpoint's sortition seed
    /// is twelve blocks older than the first block executed, the check never ran
    /// once.
    ///
    /// Two answers come back to the caller and are its own, not the peer's: the
    /// tenure's sortition hash, and the winning commitment's leader key when the
    /// burn block left no choice about which commitment won. Both are
    /// validation-only — no Clarity word reads either — so neither moves a state
    /// root.
    ///
    /// Differences with the peer are reported, not enforced, with one exception:
    /// a burn total that disagrees with a signed header means every consensus hash
    /// from here on is derived from a wrong number, so deriving stops rather than
    /// reporting the same wrongness at every block after it.
    /// The sortition for a burn view, asked of a peer at most once.
    async fn sortition_for(
        &mut self,
        node: &SyncClient,
        view: nano_primitives::ConsensusHash,
    ) -> Result<nano_sync::SortitionInfo, NodeExecutionError> {
        if let Some((known, sortition)) = self.sortition_view.as_ref()
            && *known == view
        {
            return Ok(sortition.clone());
        }
        let sortition = node.sortition(view).await?;
        self.sortition_view = Some((view, sortition.clone()));
        Ok(sortition)
    }

    fn local_sortition(
        &mut self,
        pox: &PoxInfo,
        peer: &nano_sync::SortitionInfo,
        bitcoin_spent: u64,
    ) -> Option<LocalSortition> {
        let payouts = payout_schedule(pox)?;
        // Split the borrow: the tracker reads burn blocks through the same
        // source the executor holds.
        let Self {
            sortition: Some(tracker),
            bitcoin,
            ..
        } = self
        else {
            return None;
        };
        let behind = peer
            .bitcoin_height
            .saturating_sub(tracker.tip().bitcoin_height);
        // Named before the walk, not after: every burn block costs a whole
        // Bitcoin block download, so a node closing a checkpoint's gap can be
        // busy for minutes, and a node that prints nothing for minutes teaches
        // an operator to guess at what it is doing.
        if behind > 1 {
            println!(
                "catching up the local sortition chain from burn {} to {}, {behind} blocks, \
                 one Bitcoin block download each",
                tracker.tip().bitcoin_height,
                peer.bitcoin_height
            );
        }
        let derived = match tracker.catch_up(
            |height| bitcoin.block_at(height),
            peer.bitcoin_height,
            payouts,
            crate::sortition::CATCH_UP_LIMIT,
        ) {
            // Priming counts as work worth reporting even when the chain was
            // already standing where it needed to be: it is six Bitcoin block
            // downloads, seven seconds on mainnet, and it is paid on every start
            // — the largest single item in this phase, and it used to print
            // nothing at all because no sortition came out of it.
            Ok(walk) if walk.advanced > 0 || walk.primed > 0 => {
                // The split, and not a total, because a total here was read as
                // a per-Stacks-block cost once and it is not one: a sortition
                // belongs to a burn block, and this line is printed once per
                // burn block. Reading is the burnchain, deriving is the hashes,
                // and priming is the six blocks behind a fresh seed that a
                // start pays for once.
                println!(
                    "derived {} sortitions locally, now standing on burn {} \
                     ({:.2}s reading {} burn blocks{}, {:.3}s deriving)",
                    walk.advanced,
                    tracker.tip().bitcoin_height,
                    walk.reading.as_secs_f64(),
                    walk.advanced + walk.primed,
                    if walk.primed > 0 {
                        format!(", {} of them priming the mining window", walk.primed)
                    } else {
                        String::new()
                    },
                    walk.deriving.as_secs_f64(),
                );
                walk.advanced
            }
            Ok(walk) => walk.advanced,
            Err(error) => {
                eprintln!("deriving the sortition locally failed: {error}");
                self.sortition = None;
                return None;
            }
        };
        let tip = tracker.tip();
        if tip.bitcoin_height != peer.bitcoin_height {
            // Said once per gap rather than per block: a validation that never
            // runs looks exactly like one that always passes, so the condition
            // has to be named, but not at every block behind it.
            if self.sortition_gap != Some(peer.bitcoin_height) {
                self.sortition_gap = Some(peer.bitcoin_height);
                eprintln!(
                    "the local sortition chain ends at burn {} and this block stands on {}, \
                     {} away — more than one round of catching up may walk. Until it \
                     closes, the sortition this node executes under is the peer's, unchecked.",
                    tip.bitcoin_height,
                    peer.bitcoin_height,
                    peer.bitcoin_height.saturating_sub(tip.bitcoin_height)
                );
            }
            return None;
        }
        self.sortition_gap = None;
        report_disagreements(tip, peer);
        // The winner's identity is published as derived. It used to be withheld
        // wherever more than one commitment competed, because the distribution
        // named 12 of the captured window's 14 — and the cause turned out not to
        // be the distribution at all but the *window*, which collapses to one
        // block across the epoch 4.0 boundary. All fourteen derive now, so
        // `candidates` is a report rather than a gate.
        // An unresolvable leader key used to be reported here as well as in
        // `check_tenure_vrf`, once a tenure each, in almost the same words — and
        // with the wrong reason here, since the winner has derived for every
        // captured sortition since the mining window's epoch-boundary rule
        // landed. It is said once now, at the rule that could not run, which is
        // also the only place that still holds for a chainstate driven without
        // this node. Nothing belongs here in its place: a burn block that elects
        // no winner while carrying commitments is ordinary — mainnet's 960,222
        // is one — so the count is a report on the tracker and not a condition.
        let local = LocalSortition {
            sortition_hash: *tip.sortition_hash.as_bytes(),
            winner_vrf_public_key: tip.winner_vrf_public_key,
            winner_signing_key_hash: tip.winner_signing_key_hash,
        };
        if !tracker.agrees_with_header(bitcoin_spent) {
            eprintln!(
                "the locally derived burn total at burn {} is {} where the header signed by \
                 the reward set says {} — every consensus hash after this one would be \
                 derived from a wrong total, so this node stops deriving and goes back to \
                 the peer's sortitions",
                peer.bitcoin_height,
                tip.total_burn,
                bitcoin_spent
            );
            self.sortition = None;
            return None;
        }
        // Written down as it advances rather than at shutdown, because a node
        // that is killed is exactly the one that must not start over — and only
        // as it advances: many Stacks blocks stand on one burn block, and
        // writing the whole derived history again for each of them cost a third
        // of a second per block on mainnet, where the history is 12 MB of JSON
        // that has not changed.
        if derived > 0
            && let (Some(tracker), Some(state)) =
                (self.sortition.as_ref(), self.sortition_state.as_ref())
        {
            let written = std::time::Instant::now();
            if let Err(error) = tracker.save(state) {
                eprintln!("the derived sortition chain could not be written down: {error}");
            } else {
                println!(
                    "the derived sortition chain is written down ({:.2}s)",
                    written.elapsed().as_secs_f64()
                );
            }
        }
        Some(local)
    }

    /// Stand on the last block a peer's chain and this one agree about.
    ///
    /// A peer that reorganises past this node used to strand it: the follower
    /// refuses anything that does not extend the history it holds, which is
    /// obedience to one peer rather than fork choice. Given the tenure a peer is
    /// on, this finds where the two chains parted and gives back everything
    /// after it, so the heavier branch can be executed instead of refused.
    ///
    /// Both sides of the comparison are checked: the peer's view of its own
    /// chain, and this node's view of what it *executed*. A fork point neither
    /// side reaches, or one naming a tenure this node never executed, changes
    /// nothing — a peer must not be able to talk a node off its own chain.
    ///
    /// Returns the block to resume from when a switch happened.
    pub async fn switch_to_fork(
        &mut self,
        node: &SyncClient,
        theirs: nano_primitives::ConsensusHash,
    ) -> Result<Option<[u8; 32]>, NodeExecutionError> {
        let ours = self.chainstate.executed_tenures();
        let Some(oldest) = ours.last().copied() else {
            return Ok(None);
        };
        let theirs = node.tenure_fork_info(theirs, oldest).await?;
        let Some(point) = nano_sync::fork_point_of(&ours, &theirs) else {
            return Ok(None);
        };
        let Some(block) = self.chainstate.last_block_of_tenure(point) else {
            return Ok(None);
        };
        let retraction = self.chainstate.retract_to(block);
        if retraction.discarded.is_empty() {
            return Ok(None);
        }
        println!(
            "a heavier fork parted at {point}: giving back {} blocks and standing on {}",
            retraction.discarded.len(),
            hex::encode(block)
        );
        Ok(retraction.resume_from)
    }

    /// Find the burn view a block inherits, by walking back through its tenure.
    ///
    /// Only a tenure change states one, so a block that carries none stands on
    /// the view of the last block before it that did. The walk stops when the
    /// tenure does, since a block cannot inherit a view across tenures.
    async fn bitcoin_view_of(
        node: &SyncClient,
        block: &NakamotoBlock,
    ) -> Result<Option<nano_primitives::ConsensusHash>, NodeExecutionError> {
        let mut parent = block.header.parent_block_id;
        for _ in 0..TENURE_WALK_LIMIT {
            let ancestor = node.block(parent).await?;
            if ancestor.header.consensus_hash != block.header.consensus_hash {
                return Ok(None);
            }
            if let Some(view) = ancestor.bitcoin_view_consensus_hash() {
                return Ok(Some(view));
            }
            parent = ancestor.header.parent_block_id;
        }
        Ok(None)
    }

    /// Tell the VM the header hash of the burn blocks just behind this one.
    ///
    /// Clarity can ask about any burn block, and a node that started at a
    /// checkpoint has executed under almost none of them. sBTC withdrawals name
    /// the Bitcoin block a sweep landed in, which is recent but not this one.
    fn seed_burn_headers(&mut self, height: u64) {
        for height in height.saturating_sub(BURN_HEADER_WINDOW)..=height {
            if self.chainstate.knows_burn_header(height) {
                continue;
            }
            match self.bitcoin.block_hash_at(height) {
                Ok(hash) => {
                    // Reported, not fatal: the header is in memory for this run
                    // whether or not it reached disk, and a run that comes after
                    // one that could not write it fetches it again.
                    if let Err(error) = self.chainstate.record_burn_header(height, hash) {
                        eprintln!("writing down the Bitcoin header at {height} failed: {error}");
                    }
                }
                // Worth saying: a burn block Clarity cannot be told about is a
                // withdrawal this node will reject and the network accepted.
                Err(_) => eprintln!("no Bitcoin header for burn block {height}"),
            }
        }
    }

    /// Execute staged blocks forward from this node's tip, up to `budget`.
    ///
    /// `NANO_TIMING=1` makes each round say where its seconds went.
    async fn execute_staged(
        &mut self,
        node: &SyncClient,
        pox: &PoxInfo,
        staging: &Staging,
        budget: usize,
    ) -> Result<usize, NodeExecutionError> {
        let mut executed = 0;
        let mut timing = ExecutionTiming::default();
        let mut previous_view = None;
        while executed < budget {
            let Some(block) = staging.child_of(self.tip.block_id())? else {
                break;
            };
            // The burn view, not the tenure. A tenure that outlives the burn
            // block that elected it is extended, and the extension moves the
            // view forward — so a block mid-tenure sees a later burn height
            // than its own sortition, and `burn-block-height` is what a
            // contract stores. Only a tenure change states the view, so it
            // carries forward to the blocks that follow.
            if let Some(view) = block.bitcoin_view_consensus_hash() {
                self.bitcoin_view = Some(view);
            } else if self.bitcoin_view.is_none() {
                // A resumed node did not execute the tenure change that stated
                // the view, so it walks back to it.
                self.bitcoin_view = Self::bitcoin_view_of(node, &block).await?;
            }
            let view = self.bitcoin_view.unwrap_or(block.header.consensus_hash);
            if previous_view.replace(view) != Some(view) {
                timing.views += 1;
            }
            let phase = std::time::Instant::now();
            let sortition = self.sortition_for(node, view).await?;
            timing.sortition += phase.elapsed();
            let phase = std::time::Instant::now();
            let local = self.local_sortition(pox, &sortition, block.header.bitcoin_spent);
            timing.local += phase.elapsed();
            let mut bitcoin_context = pox.bitcoin_context();
            bitcoin_context.height = sortition.bitcoin_height;
            // Clarity reads this back through `get-burn-block-info?`, and sBTC
            // compares it against the hash a withdrawal was signed for. A
            // context that leaves it zero makes every such call fail.
            bitcoin_context.burn_header_hash = *sortition.bitcoin_block_hash.as_bytes();
            // As in `apply_followed_tenure`: zero here is a wrong answer rather
            // than a stall.
            bitcoin_context.burn_block_time = sortition.bitcoin_timestamp;
            bitcoin_context.vrf_seed = sortition.vrf_seed.unwrap_or_default();
            // The two validation-only inputs the tenure VRF rules read, from
            // this node's own burnchain. Absent, `check_tenure_vrf` says which
            // rule it could not run and why, which is the honest state; filled
            // from the peer, it would be checking the peer against itself.
            if let Some(local) = local {
                bitcoin_context.sortition_hash = local.sortition_hash;
                bitcoin_context.winner_vrf_public_key = local.winner_vrf_public_key;
            bitcoin_context.winner_signing_key_hash = local.winner_signing_key_hash;
            }
            let phase = std::time::Instant::now();
            self.seed_burn_headers(sortition.bitcoin_height);
            timing.headers += phase.elapsed();
            let phase = std::time::Instant::now();
            let bitcoin_context = node
                .tenure_coinbase_context(
                    &block,
                    self.chainstate.accounting_mut().schedule(),
                    bitcoin_context,
                )
                .await?;
            timing.coinbase += phase.elapsed();
            // A burn block is news exactly once: when the tenure it elected
            // begins. `bitcoin_spent` is a running total, so the difference
            // between consecutive headers is what this burn block burned —
            // which is the one field of the event a follower could otherwise
            // not answer.
            let previous_spent = self.tip.header.bitcoin_spent;
            let announce_burn = self.tip.header.consensus_hash != block.header.consensus_hash;
            let burned = block.header.bitcoin_spent.saturating_sub(previous_spent);
            let phase = std::time::Instant::now();
            let applied = self.apply(&block, bitcoin_context)?;
            timing.execution += phase.elapsed();
            let phase = std::time::Instant::now();
            if announce_burn && let Some(observers) = self.observers.as_ref() {
                let payload = nano_rpc::new_burn_block_payload(
                    sortition.bitcoin_block_hash,
                    sortition.bitcoin_height,
                    sortition.consensus_hash,
                    nano_primitives::BitcoinHeaderHash::from_bytes(
                        self.bitcoin
                            .block_hash_at(sortition.bitcoin_height.saturating_sub(1))
                            .unwrap_or_default(),
                    ),
                    burned,
                );
                observers.dispatch(nano_rpc::EventKind::NewBurnBlock, &payload);
            }
            self.announce_block(&block, &applied, bitcoin_context);
            timing.dispatch += phase.elapsed();
            let phase = std::time::Instant::now();
            staging.remove(block.block_id())?;
            timing.staging += phase.elapsed();
            executed += 1;
            if executed % TIMING_INTERVAL == 0 {
                timing.report(executed);
            }
        }
        if executed % TIMING_INTERVAL != 0 {
            timing.report(executed);
        }
        Ok(executed)
    }

    /// Execute a candidate block on the current tip and seal its committed state root.
    pub fn assemble(
        &mut self,
        candidate: NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
        miner_key: &nano_crypto::StacksPrivateKey,
    ) -> Result<(NakamotoBlock, AppliedBlock), CheckpointExecutionError> {
        let operations = self
            .bitcoin
            .block_at(bitcoin_context.height)
            .map_err(|error| CheckpointExecutionError::Bitcoin(error.to_string()))?;
        Ok(self
            .chainstate
            .assemble_nakamoto_block_with_bitcoin_operations(
                bitcoin_context,
                &operations.operations,
                Some(*self.tip.block_id().as_bytes()),
                candidate,
                miner_key,
            )?)
    }

    /// Execute a candidate block together with transactions it may drop, and
    /// seal the state root the admitted set produces.
    pub fn assemble_selecting(
        &mut self,
        candidate: NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
        candidates: &[nano_codec::Transaction],
        miner_key: &nano_crypto::StacksPrivateKey,
    ) -> Result<(NakamotoBlock, AppliedBlock), CheckpointExecutionError> {
        let operations = self
            .bitcoin
            .block_at(bitcoin_context.height)
            .map_err(|error| CheckpointExecutionError::Bitcoin(error.to_string()))?;
        Ok(self.chainstate.assemble_nakamoto_block_selecting(
            bitcoin_context,
            &operations.operations,
            Some(*self.tip.block_id().as_bytes()),
            candidate,
            candidates,
            miner_key,
        )?)
    }

    /// Adopt a block this node produced as the new execution tip.
    pub fn accept_own_block(&mut self, block: NakamotoBlock) {
        self.tip = block;
    }

    /// Access the portable accounting ledger backing matured native rewards.
    pub const fn chainstate_mut(&mut self) -> &mut ChainState {
        &mut self.chainstate
    }

    /// Return the most recently executed block.
    #[must_use]
    pub const fn tip(&self) -> &NakamotoBlock {
        &self.tip
    }
}

/// The public RPC answers from the state this node executed, and from nothing
/// else: an account or a read-only call is read at the tip it sealed.
impl<S: Send> nano_rpc::ChainAccess for CheckpointExecutor<S> {
    fn account(
        &mut self,
        principal: &clarity::vm::types::PrincipalData,
    ) -> Result<nano_rpc::AccountEntry, nano_rpc::ChainAccessError> {
        nano_rpc::ChainAccess::account(&mut self.chainstate, principal)
    }

    fn call_read_only(
        &mut self,
        call: &nano_rpc::ReadOnlyCall,
    ) -> Result<clarity::vm::Value, nano_rpc::ChainAccessError> {
        nano_rpc::ChainAccess::call_read_only(&mut self.chainstate, call)
    }
}

impl<S> ExecutingNode<S>
where
    S: BitcoinSource,
    S::Error: fmt::Display,
{
    /// Couple a peer follower to a checkpoint executor.
    #[must_use]
    pub const fn new(node: Node, executor: CheckpointExecutor<S>) -> Self {
        Self {
            node,
            executor,
            executed_view: None,
        }
    }

    /// Follow, validate, and execute the peer's next tenure update.
    pub async fn poll(&mut self) -> Result<Option<FollowedTenure>, NodeExecutionError> {
        let followed = self.node.poll().await?;
        let view = self.node.view().ok_or(NodeExecutionError::MissingView)?;
        let current_tip = self.executor.tip().block_id();
        let first_tenure = view
            .tenures
            .iter()
            .position(|tenure| {
                tenure
                    .blocks
                    .iter()
                    .any(|block| block.block_id() == current_tip)
            })
            .unwrap_or_else(|| view.tenures.len().saturating_sub(1));
        for tenure in &view.tenures[first_tenure..] {
            self.executor
                .apply_followed_tenure(tenure, &view.pox_info)?;
        }
        self.executed_view = Some(view);
        Ok(followed)
    }

    /// Return the executed chain tip.
    #[must_use]
    pub const fn executed_tip(&self) -> &NakamotoBlock {
        self.executor.tip()
    }

    /// Return the latest completely executed node view.
    #[must_use]
    pub fn view(&self) -> Option<NodeView> {
        self.executed_view.clone()
    }
}
