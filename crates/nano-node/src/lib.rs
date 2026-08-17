pub mod archive;
pub mod checkpoint_bundle;
pub mod checkpoint_signatures;
pub mod config;
pub mod hosting;
pub mod miner;
pub mod runtime;
pub mod signer;
pub mod sortition;
pub mod staging;

use std::{collections::HashMap, fmt, path::Path, time::Duration};

use nano_address::PoxAddress;
use nano_bitcoin::BitcoinSource;
use nano_chainstate::{
    AppliedBlock, AuthenticatedBlock, BitcoinBlockContext, ChainState, ChainStateError,
    NakamotoBlock, NakamotoBlockHeader, SignerSet, SignerSetError, SignerWeights, TenureAccounting,
};
pub use nano_marf::{CheckpointAttestation, CheckpointManifest, CheckpointProvenance};
use nano_miner::{BitcoinTenureView, ParentTenure, TenureTip};
use nano_primitives::{Network, StacksBlockId, TrieHash};
use nano_sync::{PoxInfo, SyncClient, SyncError, TenureSource};

use crate::staging::{Staging, StagingError};

/// The committed source revision compiled into this artifact.
pub const SOURCE_REVISION: &str = env!("NANO_SOURCE_REVISION");

/// The Rust compiler selected by the pinned build closure.
pub const RUSTC_VERSION: &str = env!("NANO_RUSTC_VERSION");

/// The target triple this artifact was compiled for.
pub const BUILD_TARGET: &str = env!("NANO_BUILD_TARGET");

/// Executes a validated tenure stream from an imported checkpoint state.
/// How far back a burn-view walk goes before giving up: a tenure is bounded by
/// the Bitcoin block that ends it, so this only has to outlast one.
const TENURE_WALK_LIMIT: usize = 512;

/// How many burn blocks behind the current one to make readable from Clarity.
/// An sBTC sweep is confirmed within a few, and a Bitcoin header is cheap.
const BURN_HEADER_WINDOW: u64 = 32;

/// How many tenures one round may take from the inventory schedule.
///
/// The bound the forward download is bounded *by*, and it is a count of tenures
/// rather than of blocks because that is the unit a request buys: a mainnet tenure
/// is tens of blocks, so this is between several hundred and a couple of thousand
/// blocks a round, of the same order as the fetch budget the descent works under.
/// Small enough that a round ends and execution gets its turn, which is the mistake
/// an unbounded schedule would repeat.
const SCHEDULED_TENURES: usize = 32;

/// One reward cycle's locally-derived, peer-facing tenure inventory.
pub type TenureInventory = (
    u64,
    nano_primitives::ConsensusHash,
    nano_primitives::BitVec<2100>,
);

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
    /// Where to announce executed Stacks blocks and locally derived Bitcoin blocks.
    observers: Option<nano_rpc::EventDispatcher>,
    /// Where the blocks this node executes are kept, so it can serve them.
    ///
    /// Beside the observers and for the same reason: only the executor knows a
    /// block was executed rather than downloaded, and it is the executed ones a
    /// node answers `/v3/blocks` and `/v3/tenures` with.
    archive: Option<std::sync::Arc<crate::archive::Archive>>,
    /// Where execution is measured: block cost against the block limit and the
    /// wall time a block took. Beside the observers for the same reason they
    /// are here — only the executor knows a block was executed.
    metrics: Option<nano_rpc::NodeMetrics>,
    /// The non-mainnet sBTC registry used when execution computes a new cycle.
    waterfall_registry: Option<String>,
    /// A transition computed while applying a checkpoint anchor, before the
    /// derived sortition tracker is attached.
    pending_waterfall_payout: Option<(u64, u64, PoxAddress)>,
    bitcoin: S,
}

/// What this node derived for the burn block a Stacks block executes under.
///
/// Two kinds of thing, and the distinction is worth keeping in view. The
/// sortition hash and the winner's registration are **validation** inputs:
/// `check_tenure_vrf` reads them and no Clarity word does, so where they come from
/// moves no state root — but taking them from a peer would mean trusting that peer
/// for the input deciding whether a tenure is the one the network elected.
///
/// Everything else here is **Clarity-visible** and therefore does move state
/// roots: `burn-block-time`, `get-burn-block-info? header-hash`, `vrf-seed`, and
/// `miner-spend-total`/`miner-spend-winner`. All of them are the sortition
/// arithmetic's own answers, so a node holding a burnchain has no reason to ask a
/// stranger — and a stranger's wrong answer would seal a root the block's own
/// header refuses, which is how a substitution this deep can be made at all.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LocalSortition {
    bitcoin_height: u64,
    sortition_hash: [u8; 32],
    /// The winning commitment's leader-key VRF public key, when this node can
    /// name the winner without leaning on the burn distribution.
    winner_vrf_public_key: Option<[u8; 32]>,
    /// The block-signing hash that key was registered with, when the registry
    /// carries one for it.
    winner_signing_key_hash: Option<[u8; 20]>,
    /// What the sortition's miners spent, and the winner's share.
    burn_spends: Option<nano_sortition::BurnSpends>,
    /// The burn block's own header hash and time, from this node's burnchain.
    burn_header_hash: [u8; 32],
    burn_block_time: u64,
    /// The winning commitment's new seed, which `get-block-info? vrf-seed` answers.
    ///
    /// `None` at a burn block that elected nobody, where there is no seed to state
    /// and no tenure stands to ask.
    winner_vrf_seed: Option<[u8; 32]>,
}

impl LocalSortition {
    /// Fill in everything a block's execution reads from its burn block.
    ///
    /// Optional validation fields stay absent when local derivation has no
    /// answer. No peer value is present to preserve or substitute.
    /// Everything about one burn block, as this node's own sortition chain derived
    /// it.
    pub(crate) const fn from_snapshot(snapshot: &nano_sortition::SortitionSnapshot) -> Self {
        Self {
            bitcoin_height: snapshot.bitcoin_height,
            sortition_hash: *snapshot.sortition_hash.as_bytes(),
            winner_vrf_public_key: snapshot.winner_vrf_public_key,
            winner_signing_key_hash: snapshot.winner_signing_key_hash,
            burn_spends: snapshot.burn_spends,
            burn_header_hash: *snapshot.bitcoin_header_hash.as_bytes(),
            burn_block_time: snapshot.bitcoin_timestamp,
            winner_vrf_seed: snapshot.winner_vrf_seed,
        }
    }

    pub(crate) fn record(self, bitcoin_context: &mut BitcoinBlockContext) {
        self.record_authentication(bitcoin_context);
        bitcoin_context.move_to_burn_block(self.bitcoin_height);
        bitcoin_context.burn_header_hash = self.burn_header_hash;
        if self.burn_block_time > 0 {
            bitcoin_context.burn_block_time = self.burn_block_time;
        }
        if let Some(seed) = self.winner_vrf_seed {
            bitcoin_context.vrf_seed = seed;
        }
        if let Some(spends) = self.burn_spends {
            bitcoin_context.burn_spend_total = u128::from(spends.total);
            bitcoin_context.burn_spend_winner = u128::from(spends.winner);
        }
    }

    /// Fill in the tenure authentication inputs without changing its execution view.
    pub(crate) const fn record_authentication(self, context: &mut BitcoinBlockContext) {
        context.sortition_hash = self.sortition_hash;
        context.winner_vrf_public_key = self.winner_vrf_public_key;
        context.winner_signing_key_hash = self.winner_signing_key_hash;
    }
}

/// What this node's own sortition chain can say about a burn view.
///
/// Three answers rather than two, because "not yet" and "never" are different
/// things to a follower and only one of them is a reason to ask anybody anything.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LocalView {
    /// The chain derived this view's consensus hash, at this burn height.
    At(u64),
    /// The chain cannot name the view, and stands here. One of three causes, all
    /// reported: this node's burnchain has not reached that block, the round ran out
    /// of walk, or the view belongs to a chain of Bitcoin blocks this node does not
    /// read. None of them is answered by asking a peer where the view is — that is
    /// the substitution this whole task removes — so the chunk ends and the next
    /// round tries again.
    Unreached { standing_on: u64 },
    /// There is no local chain at all: no checkpoint sortition history, or no payout
    /// calendar to derive one with.
    ///
    /// This used to be the one place a peer's `/v3/sortitions` answer became the
    /// execution context. It filled the Bitcoin height, the burn header hash, the
    /// timestamp and the VRF seed -- three of which Clarity reads back and which
    /// therefore move a state root, and one of which decides whether a tenure is the
    /// one the network elected. A stranger could choose all four.
    ///
    /// So it refuses now, and startup refuses earlier still: a node with an executing
    /// role and no way to derive sortitions does not begin
    /// ([[077-remove-peer-derived-consensus-execution-fallbacks]]).
    NoChain,
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

/// Whether every executed block is to name itself.
///
/// A round already reports the height it reached and the root it sealed, which on
/// mainnet is one line per twenty thousand blocks — enough to see that a catch-up
/// is moving and not enough to be the record [[053]]'s release gate asks for. This
/// is that record, and it is a switch rather than the default because a mainnet
/// catch-up would otherwise print thirty thousand lines an operator has to read
/// past to find the one that matters.
static TRACE_ROOTS: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| std::env::var_os("NANO_TRACE_ROOTS").is_some());

/// Name a block this node executed and the root its header commits to.
///
/// The header's root and not a separately computed one, deliberately: the seal
/// already refused the block if the two differed, so this line *is* the verified
/// root — printing something computed here again would invite the reader to
/// believe the check happens at the printing.
fn trace_executed_block(block: &NakamotoBlock, bitcoin_height: u64) {
    if *TRACE_ROOTS {
        println!(
            "executed {} at burn {bitcoin_height}, block {}, verified root {}",
            block.header.chain_length,
            block.block_id(),
            block.header.state_index_root
        );
    }
}

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
#[must_use]
pub fn payout_schedule(pox: &PoxInfo) -> Option<nano_sortition::PayoutSchedule> {
    let length = u64::from(pox.prepare_phase_length) + u64::from(pox.reward_phase_length);
    let waterfall = pox.pox_5_activation_height.map(|activation| {
        pox.first_bitcoin_height + (pox.reward_cycle(u64::from(activation)) + 1) * length
    });
    let cycles =
        nano_sortition::RewardCycleSchedule::new(pox.first_bitcoin_height, length, waterfall)
            .ok()?;
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

fn tenure_inventories_from_history(
    pox: &PoxInfo,
    through: u64,
    consensus_hash_at: impl Fn(u64) -> Option<nano_primitives::ConsensusHash>,
    executed: &std::collections::HashSet<nano_primitives::ConsensusHash>,
) -> Vec<TenureInventory> {
    let length = u64::from(pox.prepare_phase_length) + u64::from(pox.reward_phase_length);
    if length == 0 {
        return Vec::new();
    }
    let Ok(length_u16) = u16::try_from(length) else {
        return Vec::new();
    };
    let Some(first) = pox
        .first_bitcoin_height
        .checked_add(1)
        .filter(|first| *first <= through)
    else {
        return Vec::new();
    };

    std::iter::successors(Some(first), |start| {
        start.checked_add(length).filter(|next| *next <= through)
    })
    .filter_map(|start| {
        let cycle_start = consensus_hash_at(start)?;
        let mut tenures = nano_primitives::BitVec::<2100>::zeros(length_u16).ok()?;
        for offset in 0..length {
            let height = start.checked_add(offset)?;
            if height > through {
                break;
            }
            if consensus_hash_at(height).is_some_and(|hash| executed.contains(&hash)) {
                tenures.set(u16::try_from(offset).ok()?, true).ok()?;
            }
        }
        Some((start, cycle_start, tenures))
    })
    .collect()
}

#[cfg(test)]
mod tenure_inventory_tests {
    use std::collections::HashSet;

    use nano_primitives::ConsensusHash;
    use nano_sync::PoxInfo;

    use super::tenure_inventories_from_history;

    fn hash(height: u64) -> ConsensusHash {
        let mut bytes = [0; 20];
        bytes[..8].copy_from_slice(&height.to_be_bytes());
        ConsensusHash::from_bytes(bytes)
    }

    #[test]
    fn inventories_keep_wire_boundaries_after_the_waterfall() {
        let pox = PoxInfo {
            first_bitcoin_height: 0,
            bitcoin_height: 42,
            prepare_phase_length: 2,
            reward_phase_length: 8,
            reward_slots: 10,
            rejection_fraction: None,
            pox_5_activation_height: Some(30),
            v1_unlock_height: None,
            v2_unlock_height: None,
            v3_unlock_height: None,
        };
        let executed = HashSet::from([hash(12), hash(31), hash(42)]);

        let inventories =
            tenure_inventories_from_history(&pox, 42, |height| Some(hash(height)), &executed);

        assert_eq!(
            inventories
                .iter()
                .map(|(height, _, _)| *height)
                .collect::<Vec<_>>(),
            vec![1, 11, 21, 31, 41]
        );
        assert!(
            (0..inventories[0].2.len()).all(|offset| inventories[0].2.get(offset) == Some(false)),
            "a known historical cycle with no served tenure is an empty answer, not unknown"
        );
        assert_eq!(inventories[1].2.get(1), Some(true));
        assert_eq!(inventories[3].2.get(0), Some(true));
        assert_eq!(inventories[4].2.get(1), Some(true));
    }
}

#[cfg(test)]
mod bitcoin_view_tests {
    use nano_crypto::StacksPrivateKey;
    use nano_miner::{
        TenureExtension, TenureTip, build_tenure_continuation_block, build_tenure_extend_block,
    };
    use nano_primitives::{ConsensusHash, Network, StacksBlockId};

    use super::{adopted_bitcoin_view, immediate_bitcoin_view};

    #[test]
    fn a_resumed_continuation_inherits_its_parent_s_burn_view() {
        let miner = StacksPrivateKey::from_seed(b"view inheritance");
        let tenure = TenureTip {
            consensus_hash: ConsensusHash::from_bytes([1; 20]),
            block_id: StacksBlockId::from_bytes([2; 32]),
            height: 10,
            bitcoin_spent: 30,
            timestamp: 40,
        };
        let view = ConsensusHash::from_bytes([3; 20]);
        let parent = build_tenure_extend_block(
            &tenure,
            TenureExtension {
                burn_view_consensus_hash: view,
                blocks_in_tenure: 2,
                nonce: 0,
                now: 41,
            },
            Network::TESTNET,
            &miner,
            Vec::new(),
        )
        .expect("build the parent that states the view");
        let child = build_tenure_continuation_block(
            &TenureTip {
                consensus_hash: parent.header.consensus_hash,
                block_id: parent.block_id(),
                height: parent.header.chain_length,
                bitcoin_spent: parent.header.bitcoin_spent,
                timestamp: parent.header.timestamp,
            },
            Vec::new(),
            42,
        );

        assert_eq!(immediate_bitcoin_view(&child, &parent), Some(view));

        let other_tenure = build_tenure_continuation_block(
            &TenureTip {
                consensus_hash: ConsensusHash::from_bytes([4; 20]),
                block_id: parent.block_id(),
                height: parent.header.chain_length,
                bitcoin_spent: parent.header.bitcoin_spent,
                timestamp: parent.header.timestamp,
            },
            Vec::new(),
            42,
        );
        assert_eq!(immediate_bitcoin_view(&other_tenure, &parent), None);
    }

    #[test]
    fn a_mined_tenure_updates_the_view_its_continuations_inherit() {
        let miner = StacksPrivateKey::from_seed(b"mined view inheritance");
        let previous = ConsensusHash::from_bytes([1; 20]);
        let view = ConsensusHash::from_bytes([2; 20]);
        let tenure = TenureTip {
            consensus_hash: ConsensusHash::from_bytes([3; 20]),
            block_id: StacksBlockId::from_bytes([4; 32]),
            height: 10,
            bitcoin_spent: 30,
            timestamp: 40,
        };
        let start = build_tenure_extend_block(
            &tenure,
            TenureExtension {
                burn_view_consensus_hash: view,
                blocks_in_tenure: 2,
                nonce: 0,
                now: 41,
            },
            Network::TESTNET,
            &miner,
            Vec::new(),
        )
        .expect("build the mined tenure block");
        let continuation = build_tenure_continuation_block(
            &TenureTip {
                consensus_hash: start.header.consensus_hash,
                block_id: start.block_id(),
                height: start.header.chain_length,
                bitcoin_spent: start.header.bitcoin_spent,
                timestamp: start.header.timestamp,
            },
            Vec::new(),
            42,
        );

        let adopted = adopted_bitcoin_view(Some(previous), &start);
        assert_eq!(adopted, Some(view));
        assert_eq!(adopted_bitcoin_view(adopted, &continuation), Some(view));
    }
}

/// Say what one round of catching up the sortition chain did.
///
/// The split, and not a total, because a total here was read as a per-Stacks-block
/// cost once and it is not one: a sortition belongs to a burn block, and many
/// Stacks blocks stand on one, so this is printed once per burn block. Reading is
/// the burnchain, deriving is the hashes, and priming is the six blocks behind a
/// fresh seed that a start pays for once — the largest single item in the phase,
/// and it used to print nothing at all because no sortition came out of it.
fn report_sortition_walk(walk: &crate::sortition::CatchUp, standing_on: u64) {
    println!(
        "derived {} sortitions locally, now standing on burn {standing_on} \
         ({:.2}s reading {} burn blocks{}, {:.3}s deriving)",
        walk.advanced,
        walk.reading.as_secs_f64(),
        walk.advanced + walk.primed,
        if walk.primed > 0 {
            format!(", {} of them priming the mining window", walk.primed)
        } else {
            String::new()
        },
        walk.deriving.as_secs_f64(),
    );
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

/// What one bounded chunk of execution did, and why it stopped.
#[derive(Clone, Copy, Debug, Default)]
struct ExecutedChunk {
    blocks: usize,
    /// Successfully sealed tenure-start blocks. Each one passed the winner,
    /// coinbase VRF and parent-seed checks in addition to the per-block checks.
    tenure_starts: usize,
    /// Whether the peer asked this node to slow down part-way through.
    rate_limited: bool,
}

/// Turn a peer's rate limit into the end of a chunk rather than a failure.
///
/// `Ok(None)` is "the peer asked this node to slow down": everything sealed
/// before it stands, and what is still staged waits for the next round.
fn ended_by_a_rate_limit<T>(
    outcome: Result<T, NodeExecutionError>,
) -> Result<Option<T>, NodeExecutionError> {
    match outcome {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.is_rate_limited() => Ok(None),
        Err(error) => Err(error),
    }
}

/// The burn view stated by this block or its immediate same-tenure parent.
fn immediate_bitcoin_view(
    block: &NakamotoBlock,
    parent: &NakamotoBlock,
) -> Option<nano_primitives::ConsensusHash> {
    block.bitcoin_view_consensus_hash().or_else(|| {
        if parent.header.consensus_hash == block.header.consensus_hash {
            parent.bitcoin_view_consensus_hash()
        } else {
            None
        }
    })
}

fn adopted_bitcoin_view(
    current: Option<nano_primitives::ConsensusHash>,
    block: &NakamotoBlock,
) -> Option<nano_primitives::ConsensusHash> {
    block.bitcoin_view_consensus_hash().or(current)
}

/// What one round of catching up actually did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CatchUpRound {
    /// The block a fork switch stood on, when one happened.
    pub reorganized: Option<[u8; 32]>,
    pub fetched: usize,
    pub executed: usize,
    /// Tenure starts among `executed`, for positive authentication evidence.
    pub authenticated_tenure_starts: usize,
    /// Tenures this round took from the inventory schedule rather than from the
    /// backward parent-walk.
    ///
    /// Its own counter because it is the measurement the schedule exists for: a round
    /// that fetched blocks and scheduled no tenures did all of its work by walking
    /// parents from a peer's tip, whatever the peers claimed.
    pub scheduled: usize,
    /// Blocks fetched but not yet executed.
    pub staged: u64,
    /// Whether the peer asked this node to slow down, which ends a round
    /// successfully rather than discarding it.
    pub rate_limited: bool,
}

impl CatchUpRound {
    const fn record_execution(&mut self, executed: ExecutedChunk) {
        self.executed = executed.blocks;
        self.authenticated_tenure_starts = executed.tenure_starts;
        self.rate_limited |= executed.rate_limited;
    }
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

impl NodeExecutionError {
    /// Whether this is a peer asking the node to slow down.
    ///
    /// A round that stopped for this has not failed: it keeps every block it
    /// sealed and asks again next poll.
    #[must_use]
    pub fn is_rate_limited(&self) -> bool {
        match self {
            Self::Sync(error) | Self::Descent { error, .. } => error.is_rate_limited(),
            Self::Execution(_) | Self::Staging(_) | Self::MissingView => false,
        }
    }
}

impl fmt::Display for NodeExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sync(error) => write!(formatter, "node synchronization failed: {error}"),
            Self::Descent { tenure, error } => {
                write!(
                    formatter,
                    "descending through tenure {tenure} failed: {error}"
                )
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
    /// The header's cumulative burn is not the total this node derived from its
    /// own burnchain, so this block was built over a different chain of Bitcoin
    /// blocks than the one this node read.
    BitcoinSpent {
        bitcoin_height: u64,
        header: u64,
        derived: u64,
    },
    /// A peer placed a burn view at a height this node's own chain says is
    /// another burn block's.
    BurnViewHeight {
        view: nano_primitives::ConsensusHash,
        peer: u64,
        derived: u64,
    },
}

impl fmt::Display for CheckpointExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChainState(error) => write!(formatter, "checkpoint execution failed: {error}"),
            Self::Bitcoin(error) => write!(formatter, "Bitcoin operation loading failed: {error}"),
            Self::Link(error) => {
                write!(formatter, "checkpoint execution chain link failed: {error}")
            }
            Self::BitcoinSpent {
                bitcoin_height,
                header,
                derived,
            } => write!(
                formatter,
                "the block at burn {bitcoin_height} says {header} burn has been spent and \
                 this node's own burnchain makes it {derived}. That total is what the reward \
                 set signed, so a disagreement means this block was built over a different \
                 chain of Bitcoin blocks than the one this node read -- which no state root \
                 would catch, because executing over the wrong burnchain produces a perfectly \
                 consistent state for a chain nobody else is on."
            ),
            Self::BurnViewHeight {
                view,
                peer,
                derived,
            } => write!(
                formatter,
                "a peer places burn view {view} at height {peer} and this node's own \
                 sortition chain derived it at {derived}. The height decides which Bitcoin \
                 block every Clarity-visible burn field is read from, so this is not a \
                 disagreement to report and carry on from."
            ),
        }
    }
}

impl std::error::Error for CheckpointExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ChainState(error) => Some(error),
            Self::Bitcoin(_)
            | Self::Link(_)
            | Self::BitcoinSpent { .. }
            | Self::BurnViewHeight { .. } => None,
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

fn waterfall_transition(
    applied: &AppliedBlock,
    context: BitcoinBlockContext,
) -> Option<(u64, u64, PoxAddress)> {
    let reward_set = applied.reward_set.as_ref()?;
    let recipient = applied.waterfall_payout?;
    let cycle_length = u64::from(context.prepare_phase_length)
        .checked_add(u64::from(context.reward_phase_length))?;
    let offset = reward_set.reward_cycle.checked_mul(cycle_length)?;
    Some((
        context.first_height.checked_add(offset)?,
        context.height,
        recipient,
    ))
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
        chainstate: ChainState,
        anchor: NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
        bitcoin: S,
    ) -> Result<Self, CheckpointExecutionError> {
        Self::from_chainstate_using_registry(chainstate, anchor, bitcoin_context, bitcoin, None)
    }

    /// Apply a checkpoint anchor with the non-mainnet sBTC registry whose
    /// aggregate key determines future waterfall payouts.
    pub fn from_chainstate_using_registry(
        mut chainstate: ChainState,
        anchor: NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
        mut bitcoin: S,
        waterfall_registry: Option<String>,
    ) -> Result<Self, CheckpointExecutionError> {
        let operations = bitcoin
            .block_at(bitcoin_context.height)
            .map_err(|error| CheckpointExecutionError::Bitcoin(error.to_string()))?;
        let parent = chainstate.tip()?;
        let applied = chainstate.append_nakamoto_block_with_bitcoin_operations_using_registry(
            bitcoin_context,
            &operations.operations,
            parent,
            &anchor,
            waterfall_registry.as_deref(),
        )?;
        let pending_waterfall_payout = waterfall_transition(&applied, bitcoin_context);
        Ok(Self {
            chainstate,
            sortition: None,
            sortition_state: None,
            sortition_gap: None,
            tip: anchor,
            bitcoin_height: bitcoin_context.height,
            bitcoin_view: None,
            observers: None,
            archive: None,
            metrics: None,
            waterfall_registry,
            pending_waterfall_payout,
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
            observers: None,
            archive: None,
            metrics: None,
            waterfall_registry: None,
            pending_waterfall_payout: None,
            bitcoin,
        }
    }

    /// Configure the registry whose locally executed state determines future
    /// waterfall payouts. Mainnet deliberately uses its fixed registry when this
    /// is `None`.
    pub fn use_waterfall_registry(&mut self, registry: Option<String>) {
        self.waterfall_registry = registry;
    }

    /// Consume an authenticated executor so a proposal validator can use its state.
    pub(crate) fn into_validator_parts(self) -> (ChainState, NakamotoBlock, u64, S) {
        let bitcoin_height = self.bitcoin_height();
        (self.chainstate, self.tip, bitcoin_height, self.bitcoin)
    }

    /// Validate and execute one direct descendant of the current execution tip.
    pub fn apply(
        &mut self,
        block: &NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
    ) -> Result<AppliedBlock, CheckpointExecutionError> {
        let authenticated = self.authenticate(block, bitcoin_context)?;
        self.apply_authenticated(authenticated)
    }

    /// Authenticate one direct descendant without executing or staging it.
    pub fn authenticate(
        &mut self,
        block: &NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
    ) -> Result<AuthenticatedBlock, CheckpointExecutionError> {
        let operations = self
            .bitcoin
            .block_at(bitcoin_context.height)
            .map_err(|error| CheckpointExecutionError::Bitcoin(error.to_string()))?;
        self.authenticate_with_operations(block, bitcoin_context, &operations.operations)
    }

    /// Validate and execute one direct descendant with decoded Bitcoin operations.
    pub fn apply_with_operations(
        &mut self,
        block: &NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
        operations: &[nano_bitcoin::BitcoinOperation],
    ) -> Result<AppliedBlock, CheckpointExecutionError> {
        let authenticated =
            self.authenticate_with_operations(block, bitcoin_context, operations)?;
        self.apply_authenticated(authenticated)
    }

    /// Authenticate one direct descendant with decoded Bitcoin operations.
    pub fn authenticate_with_operations(
        &mut self,
        block: &NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
        operations: &[nano_bitcoin::BitcoinOperation],
    ) -> Result<AuthenticatedBlock, CheckpointExecutionError> {
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
        self.chainstate
            .authenticate_nakamoto_block_with_bitcoin_operations(
                bitcoin_context,
                operations,
                Some(*self.tip.block_id().as_bytes()),
                block.clone(),
            )
            .map_err(CheckpointExecutionError::from)
    }

    /// Commit a block whose exact parent and Bitcoin inputs were authenticated.
    pub fn apply_authenticated(
        &mut self,
        authenticated: AuthenticatedBlock,
    ) -> Result<AppliedBlock, CheckpointExecutionError> {
        let block = authenticated.block().clone();
        let bitcoin_context = authenticated.bitcoin_context();
        let applied = self
            .chainstate
            .commit_authenticated_nakamoto_block(authenticated, self.waterfall_registry.as_deref())?
            .into_applied();
        self.remember_waterfall_payout(&applied, bitcoin_context);
        self.adopt_executed_tip(block, bitcoin_context);
        Ok(applied)
    }

    fn adopt_executed_tip(&mut self, block: NakamotoBlock, context: BitcoinBlockContext) {
        self.bitcoin_view = adopted_bitcoin_view(self.bitcoin_view, &block);
        self.tip = block;
        self.bitcoin_height = context.height;
    }

    fn remember_waterfall_payout(&mut self, applied: &AppliedBlock, context: BitcoinBlockContext) {
        let Some((start, observed_at, recipient)) = waterfall_transition(applied, context) else {
            if applied.reward_set.is_some() && applied.waterfall_payout.is_none() {
                eprintln!(
                    "a locally computed reward set carries no waterfall payout; the derived \
                     sortition chain will refuse its cycle"
                );
            }
            return;
        };
        if let Some(tracker) = self.sortition.as_mut() {
            tracker.record_waterfall_payout(start, observed_at, recipient);
        } else {
            self.pending_waterfall_payout = Some((start, observed_at, recipient));
        }
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

    /// The consensus hash that names the inventory cycle this node's burn view sits in.
    ///
    /// This is how a `GetNakamotoInv` names a cycle — by the consensus hash of its
    /// *first sortition* — and it comes from this node's own derived sortition chain
    /// rather than from a peer, because a cycle identifier taken from a peer would
    /// make that peer's view of the burnchain the thing nano's inventory requests are
    /// keyed on.
    ///
    /// This wire boundary is always modulo one. It deliberately differs from the
    /// waterfall-aware modulo-zero boundary used for Nakamoto signer accounting.
    #[must_use]
    pub fn cycle_start_consensus_hash(
        &self,
        pox: &PoxInfo,
    ) -> Option<nano_primitives::ConsensusHash> {
        self.sortition
            .as_ref()?
            .consensus_hash_at(self.cycle_start_height(pox)?)
    }

    /// The Bitcoin height the reward cycle this node's burn view sits in opens at.
    fn cycle_start_height(&self, pox: &PoxInfo) -> Option<u64> {
        let height = self
            .bitcoin_height()
            .min(self.sortition.as_ref()?.tip().bitcoin_height);
        pox.inventory_cycle_start(height)
    }

    /// Which tenures of every locally known reward cycle this node has executed.
    ///
    /// A bit is set only where this node executed the tenure that began at that
    /// sortition and can therefore serve its blocks, so the vector says less than the
    /// node knows rather than more: an unset bit means "do not ask me", which costs a
    /// peer nothing, while a set bit it could not honour would cost that peer a failed
    /// fetch. It is the conservative direction on purpose.
    ///
    /// The executed-block archive is the source of the set bits because it is also
    /// what answers `/v3/tenures`: a bit means both "executed" and "still serveable".
    /// Most old cycles are empty. They are still answers rather than unknowns: a stock
    /// node begins its Nakamoto inventory walk at the epoch boundary and a NACK stops
    /// it before it can reach the recent cycles nano can serve.
    /// `nano_p2p::ServedTenures` folds the set bits into durable rows, so a tenure nano
    /// did run remains advertised after a restart.
    ///
    /// The burn height the cycle opens at comes back with the answer because that is
    /// what the durable store keys a row by: a reorganization renames the cycle's first
    /// sortition, and a store keyed by the name would keep claiming tenures on the fork
    /// nano abandoned.
    #[must_use]
    pub fn tenure_inventories(&self, pox: &PoxInfo) -> Vec<TenureInventory> {
        let Some(tracker) = self.sortition.as_ref() else {
            return Vec::new();
        };
        let through = self.bitcoin_height().min(tracker.tip().bitcoin_height);
        let executed: std::collections::HashSet<nano_primitives::ConsensusHash> = self
            .archive
            .as_ref()
            .and_then(|archive| match archive.executed_tenures() {
                Ok(tenures) => Some(tenures),
                Err(error) => {
                    eprintln!("cannot inventory the tenures this node can serve: {error}");
                    None
                }
            })
            .unwrap_or_default()
            .into_iter()
            .collect();
        tenure_inventories_from_history(
            pox,
            through,
            |height| tracker.consensus_hash_at(height),
            &executed,
        )
    }

    /// The tenures of the cycle this node is walking that it has yet to execute,
    /// forward from its own tip.
    ///
    /// Every offset from the tip's own burn block upward, which is the whole of the
    /// derivation and is deliberately not the complement of
    /// [`Self::tenure_inventories`]: its set bits reach only `REORG_REACH` blocks back,
    /// so most of a cycle reads unexecuted in it even where this node ran it, and
    /// wanting those would send a round backwards. Above the tip nothing has been
    /// executed by definition, so no lookup is needed to know it is wanted.
    ///
    /// The tip's *own* offset is included, because its tenure is the one still growing:
    /// this node has executed some of it and the rest is exactly what a forward
    /// schedule should ask for first.
    ///
    /// Burn blocks that elected nobody are in here too and are dropped by the
    /// scheduler, not by this: a bit set in no peer's inventory is a tenure no peer has
    /// — which is what a burn block with no sortition looks like from the wire, and
    /// which is a fact this node's hash history cannot answer on its own, since it
    /// keeps consensus hashes and not snapshots.
    ///
    /// `from` is the burn view of the furthest block this node has *acquired* rather
    /// than executed, when there is one. The two are far apart while catching up, and
    /// anchoring at the executed tip made every round re-derive nearly the window the
    /// last one had: a tip advances by a tenure while the window is dozens of tenures
    /// long, so a round asked again for the tenures it had already staged and paid for
    /// them again. Its own tenure is still included, because a tenure straddling the
    /// furthest block is one this node holds only part of.
    fn wanted_tenures(
        &self,
        pox: &PoxInfo,
        from: Option<nano_primitives::ConsensusHash>,
        bound: usize,
    ) -> Option<(u64, Vec<u16>)> {
        let start = self.cycle_start_height(pox)?;
        let length =
            u16::try_from(u64::from(pox.prepare_phase_length) + u64::from(pox.reward_phase_length))
                .ok()?;
        let acquired = from
            .and_then(|view| self.sortition.as_ref()?.height_of_consensus_hash(view))
            .unwrap_or_default()
            .max(self.bitcoin_height());
        let first = u16::try_from(acquired.checked_sub(start)?).ok()?;
        Some((start, (first..length).take(bound).collect()))
    }

    /// Where to ask for each tenure this node wants next, and of whom.
    ///
    /// The half of inventory sync that was missing for four slices, and it is a pure
    /// function of what this node knows plus what its peers said. Two answers make
    /// each entry, and neither comes from the other side: the burn view is
    /// [`crate::sortition::SortitionTracker`]'s own derivation for that Bitcoin
    /// height, and the endpoint is the peer whose inventory claimed *that* tenure.
    ///
    /// Three things about it are the point:
    ///
    /// * **A forward order exists at all.** The backward parent-walk cannot start
    ///   anywhere but a peer's tip, because a block's identifier lives in the answer
    ///   above it — so from a checkpoint twenty thousand blocks behind mainnet, all
    ///   twenty thousand have to be downloaded before the first can execute. A burn
    ///   view is derivable ahead of time, so the tenure directly above the executed
    ///   tip can be asked for first and a round is progress rather than a step.
    /// * **Only peers that claimed a tenure are asked for it**, which is
    ///   [`nano_p2p::assign_tenures`] and is what an inventory is *for*. A tenure no
    ///   peer claims is absent rather than guessed at, and that is also how the burn
    ///   blocks that elected nobody leave the wanted list — no peer has a tenure for
    ///   them, so no bit is set.
    /// * **Nothing here decides anything.** What comes back goes into the same staging
    ///   store the descent's does and through the same authenticated execution, so
    ///   which peer an inventory named cannot change what this node accepts. What
    ///   makes it safe to ask a stranger for a tenure *by name* is that
    ///   [`SyncClient::tenure_at`] refuses an answer carrying another view's blocks.
    fn schedule_tenures(
        &self,
        pox: &PoxInfo,
        claims: &[nano_p2p::TenureClaim],
        acquired: Option<nano_primitives::ConsensusHash>,
    ) -> Vec<(nano_primitives::ConsensusHash, String)> {
        if claims.is_empty() {
            return Vec::new();
        }
        let Some((tracker, (start, wanted))) =
            self.sortition
                .as_ref()
                .zip(self.wanted_tenures(pox, acquired, SCHEDULED_TENURES))
        else {
            return Vec::new();
        };
        nano_p2p::assign_tenures(claims, &wanted)
            .into_iter()
            .filter_map(|(offset, endpoint)| {
                tracker
                    .consensus_hash_at(start + u64::from(offset))
                    .map(|view| (view, endpoint))
            })
            .collect()
    }

    /// Refuse a legacy state whose sealed tip has no local execution header.
    ///
    /// A peer can supply the block bytes but not the consensus context under
    /// which this node executed them. Fresh checkpoint histories and every block
    /// nano seals carry the header; an older state needs explicit migration.
    pub fn backfill_headers(&self) -> Result<usize, NodeExecutionError> {
        if self
            .chainstate
            .has_recorded_header(*self.tip.block_id().as_bytes())
        {
            return Ok(0);
        }
        Err(NodeExecutionError::Execution(
            CheckpointExecutionError::Link(format!(
                "sealed tip {} has no locally recorded execution header; peer sortitions are not authentication evidence. Re-open from a checkpoint carrying `authentication_history` or migrate this legacy state before synchronization",
                self.tip.block_id()
            )),
        ))
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
        claims: &[nano_p2p::TenureClaim],
    ) -> Result<CatchUpRound, NodeExecutionError> {
        let mut round = CatchUpRound::default();
        // A throttle is set aside for a *round*, and this is the round. Without
        // this the pool was set aside for the life of the process: the first 429
        // marked the peer throttled, `TenureSource` skipped it on every later
        // round, and with nobody left to ask the descent answered an error before
        // execution ever ran. That is the mainnet stall — hundreds of rounds after
        // one 429, with the executed tip never moving off its anchor.
        history.forgive_throttles();
        // What the peer added since the last round sits above everything
        // staged, so the descent stops as soon as it meets a block already held.
        let executed_tip = self.tip.block_id();
        let executed_height = self.tip.header.chain_length;
        // A descent that overshot leaves blocks below the executed tip, which
        // no round will ever execute and every round would otherwise resume
        // from.
        //
        // Below it, and not up to it. A block staged at the executed tip's own
        // height is either that block — dropped by name just below, having been
        // executed — or its **sibling**, which is the root of a branch that
        // parted from this one and the only thing linking what sits above it to
        // the block both branches descend from. Clearing the whole height threw
        // that root away on every round, so the branch above it could never be
        // reached: the mainnet stall at 8724697
        // ([[096-cross-a-stacks-fork-inside-one-sortition-chain]]).
        staging.remove_to(executed_height.saturating_sub(1))?;
        staging.remove(executed_tip)?;
        // A peer that will not even say where its tip is does not end the round:
        // everything already staged can still be executed and sealed, and the
        // descent picks up at the next poll. This was the first `?` of the round,
        // so a throttled peer meant a node with twenty thousand blocks on disk
        // executed none of them and reported a failure instead.
        let stop = Stop {
            block_id: executed_tip,
            height: executed_height,
        };
        // A staged branch with an unexecuted parent already says exactly what is
        // missing, and nothing above that gap can run. Close it before asking for
        // newer inventory: on the pristine mainnet replay the forward schedule spent
        // every peer each round while 66,000 linked blocks waited above a 753-block
        // hole, so the descent was skipped forever.
        let mut fetched = 0;
        if let Some(resume) = staging
            .descent_resumes_at()?
            .filter(|(resume, _)| !self.chainstate.has_executed(*resume.as_bytes()))
            .map(|(resume, _)| resume)
        {
            fetched +=
                Self::descend(history, staging, resume, stop, budget.fetch, &mut round).await?;
        }

        // With no known staged gap, forward first. A backward descent has to reach
        // this node's tip before a single staged block can execute, while the schedule
        // starts at the tip and asks directly for the next tenure.
        let acquired = staging.highest()?.map(|block| block.header.consensus_hash);
        let schedule = self.schedule_tenures(pox, claims, acquired);
        let scheduled = if round.rate_limited {
            0
        } else {
            Self::fetch_scheduled(
                history,
                staging,
                &schedule,
                stop,
                budget.fetch.saturating_sub(fetched),
                &mut round,
            )
            .await?
        };
        fetched += scheduled;
        let peer_tip = if round.rate_limited {
            None
        } else {
            match node.tenure_info().await {
                Ok(info) => Some(info.tip_block_id),
                Err(error) if error.is_rate_limited() => {
                    round.rate_limited = true;
                    None
                }
                Err(error) => return Err(error.into()),
            }
        };
        round.fetched = fetched;
        if let Some(peer_tip) = peer_tip {
            fetched += Self::descend(
                history,
                staging,
                peer_tip,
                stop,
                budget.fetch.saturating_sub(fetched),
                &mut round,
            )
            .await?;
            // The descent itself continues from the furthest it has reached,
            // which is what makes a rate-limited round cost nothing but time.
            //
            // Two conditions on it, and each is a stall that was measured. Not
            // while this round is already out of peers to ask: asking a pool that
            // has nothing left to give used to answer `NoPeer`, which is an error,
            // and the round returned it instead of executing what it had just
            // staged. And only while a gap is left to close: a tenure arrives
            // whole, so the answer that reached the executed tip also staged
            // blocks below it, and the lowest of those points at a tenure this
            // node has already sealed — which was then asked for again on every
            // round for as long as the peer's tip stayed inside one tenure, seven
            // times over one tenure in the harness that found it.
            //
            // The gap is a question about a *block*, not about a height. Asking
            // whether the lowest staged block sits above the executed tip reads
            // as the same question and is not: a branch that parted from this one
            // has a block at every height this node has already sealed, so its
            // lowest block was `executed_height + 1` — no gap by that measure —
            // while its parent was this node's tip's sibling and nothing above it
            // could ever execute. Naming the parent answers both: a tenure that
            // straddles the tip points at a block this node executed and is left
            // alone, and a branch that parted points at one it did not and is
            // followed down to where the two agree
            // ([[096-cross-a-stacks-fork-inside-one-sortition-chain]]).
            if let Some(resume) = staging
                .descent_resumes_at()?
                .filter(|_| !round.rate_limited)
                .filter(|(resume, _)| !self.chainstate.has_executed(*resume.as_bytes()))
                .map(|(resume, _)| resume)
            {
                fetched += Self::descend(
                    history,
                    staging,
                    resume,
                    stop,
                    budget.fetch.saturating_sub(fetched),
                    &mut round,
                )
                .await?;
            }
            round.fetched = fetched;
        }
        // A tenure response may include blocks below the requested stop. The
        // start-of-round trim cannot remove those because they were fetched
        // afterwards; leaving them here makes branch selection mistake an
        // already-executed ancestor for the sibling branch's root and retract one
        // block too far. Keep the sibling at `executed_height`, but not anything
        // below it or the exact executed tip.
        staging.remove_to(executed_height.saturating_sub(1))?;
        staging.remove(executed_tip)?;
        // Before executing anything, ask Bitcoin whether the burn blocks this
        // node's sortitions were derived from still hold. A round is the right
        // place: a sortition is a fact about a Bitcoin block and many Stacks
        // blocks stand on one, so asking per block would reinstate a per-block
        // burnchain round trip that measurement says does not exist
        // ([[049-derive-canonical-sortitions-from-the-local-burncha]]).
        if let Some(resumed) = self.check_burnchain(node, staging).await? {
            round.reorganized = Some(resumed);
        }
        let executed = self
            .execute_staged(history, pox, staging, budget.execute)
            .await?;
        round.record_execution(executed);
        // A descent that fetched blocks and executed none, while the peer is
        // ahead, is what a fork looks like from here: the peer's chain walked
        // past this node's tip on another branch, so nothing staged extends it
        // and no later round ever will. Standing where the two chains agree is
        // what turns that from a stall into a reorganisation.
        //
        // Not on a round the peer cut short: "executed nothing" then means the
        // peer stopped answering, which says nothing about which chain it is on,
        // and the two requests this asks would be the round's error instead of
        // its progress.
        //
        // The branch this node already holds is asked first, and it is asked of
        // nobody: a fork whose two sides share a sortition chain has no burn view
        // to name and no peer to ask about it, and the evidence is sitting in
        // staging. Only when that answers nothing is the burn view a peer holds
        // worth a request.
        if !round.rate_limited && round.executed == 0 && round.fetched > 0 {
            if let Some(resume) = self.switch_to_staged_branch(node, staging).await? {
                round.reorganized = Some(resume);
            } else if let Ok(peer) = node.tenure_info().await
                && let Some(resume) = self.switch_to_fork(node, peer.consensus_hash).await?
            {
                round.reorganized = Some(resume);
                staging.clear()?;
            }
        }
        round.staged = staging.len()?;
        Ok(round)
    }

    /// Fetch a schedule and stage what comes back, in the order it was scheduled.
    ///
    /// The I/O half of [`Self::schedule_tenures`], separated because the schedule is a
    /// pure function of local state and this is not — and because a shared borrow of
    /// the executor held across an await makes the whole future non-`Send`, which is
    /// the same wall `&Swarm` hit in `nano-p2p`.
    ///
    /// A tenure nobody serves is skipped rather than ending the run, and it took a test
    /// to settle that. Stopping at the first gap reads as the frugal choice — nothing
    /// above a gap can execute until it closes — but most gaps here are not gaps at all:
    /// the wanted list is every burn *block* of the cycle above the tip, and a burn
    /// block that elected nobody has no tenure for any peer to hold. On mainnet the next
    /// such block is never more than a few away, so a run that stopped at one scheduled
    /// a single tenure and gave up. Nothing is wasted by carrying on, either, because
    /// staging is durable and a block above a gap is executed by the round that closes
    /// it.
    ///
    /// The one thing that does end the run is every peer rate limiting, which is the
    /// signal that means wait. A peer that claimed a tenure and could not serve it is
    /// not penalised anywhere: an inventory that has moved on is far commoner than a
    /// lie, and scoring for it would repeat the mistake of isolating the busiest peers.
    async fn fetch_scheduled(
        history: &mut TenureSource,
        staging: &Staging,
        schedule: &[(nano_primitives::ConsensusHash, String)],
        until: Stop,
        budget: usize,
        round: &mut CatchUpRound,
    ) -> Result<usize, NodeExecutionError> {
        let mut fetched = 0;
        for (view, endpoint) in schedule {
            if fetched >= budget {
                break;
            }
            let served = history.tenure_at(Some(endpoint), *view).await;
            match served {
                Ok(blocks) => {
                    // Which peer served this tenure, said as it happens rather than
                    // counted at the end. "Several peers were open" and "several peers
                    // served the history" are different claims, and only a record kept
                    // per tenure can tell them apart afterwards.
                    println!(
                        "burn view {view}: {} blocks from {}",
                        blocks.len(),
                        history.last_served().unwrap_or("an unnamed peer")
                    );
                    for block in &blocks {
                        staging.download(block)?;
                    }
                    // Only what is above the executed tip counts against the budget,
                    // and the reason is a stall this measured. The schedule starts at
                    // the tip's *own* burn block, because a tenure straddling the tip
                    // is the one the executor wants next and only this can ask for it —
                    // but its blocks are mostly ones this node has already run. Counting
                    // those, a tenure as long as the budget bought a round that fetched
                    // its own history and stopped: on the captured chain, whose longest
                    // tenure is twelve blocks, the schedule sat on that tenure for
                    // sixty-four rounds and the tip never moved. The budget bounds
                    // progress, and the tenure count bounds requests.
                    fetched += blocks
                        .iter()
                        .filter(|block| block.header.chain_length > until.height)
                        .count();
                    round.scheduled += 1;
                }
                Err(error) if error.is_rate_limited() && history.exhausted() => {
                    round.rate_limited = true;
                    break;
                }
                Err(SyncError::EmptyTenure) => {}
                Err(error) => {
                    eprintln!("no peer served the scheduled tenure at burn view {view}: {error}");
                }
            }
        }
        Ok(fetched)
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
            if staging.has_representation(cursor)? {
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
            // Everything the answer carried, including what sits at or below the
            // executed tip. Those blocks are dead weight the next round's
            // `remove_to` drops — but they are also the only evidence that a peer
            // is on another branch, since a fork's blocks are at heights this node
            // has already sealed. Dropping them here silenced the fork switch.
            for block in &blocks {
                staging.download(block)?;
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
    /// Every field here is one this node holds an answer for, and the answers
    /// come from three places: the block's own header, the header the parent left
    /// in the store when it sealed, and the sortition this node derived for the
    /// burn view. Nothing is taken from a peer, and a field with no answer is
    /// left at its default rather than invented — an observer comparing nano
    /// against stacks-core is better served by a field that is plainly absent
    /// than by one that is confidently wrong.
    ///
    /// The parent's recorded header is why this runs after the seal: the block's
    /// `parent_block_hash` is the parent's *block hash*, which its identifier is
    /// not, and its burn view is the one the parent executed under.
    fn announce_block(
        &self,
        block: &NakamotoBlock,
        applied: &AppliedBlock,
        context: BitcoinBlockContext,
    ) {
        let Some(observers) = self.observers.as_ref() else {
            return;
        };
        let parent = self
            .chainstate
            .recorded_header(*block.header.parent_block_id.as_bytes());
        let sealed = self
            .chainstate
            .recorded_header(*block.block_id().as_bytes());
        let tenure_height = sealed.map_or(0, |header| header.tenure_height);
        let matured_source = if applied.matured_rewards.is_empty() {
            None
        } else {
            self.matured_reward_source(tenure_height)
        };
        let event = nano_rpc::BlockEventContext {
            parent_block_hash: nano_primitives::BlockHeaderHash::from_bytes(
                parent.map_or_else(<[u8; 32]>::default, |header| header.block_header_hash),
            ),
            bitcoin_block_hash: nano_primitives::BitcoinHeaderHash::from_bytes(
                context.burn_header_hash,
            ),
            bitcoin_height: context.height,
            bitcoin_timestamp: context.burn_block_time,
            parent_bitcoin_block_hash: nano_primitives::BitcoinHeaderHash::from_bytes(
                parent.map_or_else(<[u8; 32]>::default, |header| header.burn_header_hash),
            ),
            parent_bitcoin_height: parent.map_or(0, |header| u64::from(header.burn_block_height)),
            parent_bitcoin_timestamp: parent.map_or(0, |header| header.burn_block_time),
            // The commitment that won this tenure's sortition, out of the chain
            // this node derived from Bitcoin itself. A node deriving no
            // sortitions has no answer and says nothing.
            miner_txid: nano_primitives::Sha256Sum::from_bytes(
                self.sortition
                    .as_ref()
                    .and_then(|tracker| tracker.snapshot_at(context.height))
                    .and_then(|snapshot| snapshot.winner_txid)
                    .unwrap_or_default(),
            ),
            tenure_height: u64::from(tenure_height),
            v1_unlock_height: context.v1_unlock_height,
            v2_unlock_height: context.v2_unlock_height,
            v3_unlock_height: context.v3_unlock_height,
            pox_5_activation_height: context.pox_5_activation_height,
            matured_rewards: nano_rpc::matured_rewards(
                &applied.matured_rewards,
                applied.matured_coinbase,
                applied.matured_anchored_fees,
                matured_source.as_ref(),
            ),
            reward_set: applied
                .reward_set
                .as_ref()
                .map(nano_rpc::RewardSetEvent::from_derived),
        };
        // Queued rather than posted: `dispatch` hands the payload to the
        // observer's own drain task, so an observer that is slow or gone costs
        // this loop the serialization and nothing else.
        observers.dispatch(
            nano_rpc::EventKind::NewBlock,
            &nano_rpc::new_block_payload(block, applied, &event),
        );
    }

    fn keep_executed_block(&self, block: &NakamotoBlock) {
        if let Some(archive) = self.archive.as_ref()
            && let Err(error) = archive.keep(block)
        {
            eprintln!(
                "cannot keep the executed block {} for serving: {error}",
                block.block_id()
            );
        }
    }

    /// Which tenure the rewards this block matured were earned in, and by whom.
    ///
    /// The three fields of a matured reward that a credit cannot be read backwards
    /// into: the tenure-start block that scheduled the payout, and the miner that
    /// signed its coinbase — who is not the recipient whenever the coinbase named
    /// one. What answers them is that block itself, and a node that keeps the
    /// blocks it executed still has it a hundred tenures later.
    ///
    /// Read back rather than remembered. Carrying provenance forward in the tenure
    /// accounting would work too, and it is the wrong place: that ledger is
    /// serialized with **every** block and holds two hundred tenures, so it would
    /// roughly double in size per block for a field no consensus rule reads.
    ///
    /// A node whose archive does not reach back that far — one started from a
    /// checkpoint, for its first hundred tenures — answers nothing, which is the
    /// truth: a checkpoint carries what is owed and not where it was earned.
    fn matured_reward_source(&self, tenure_height: u32) -> Option<nano_rpc::MaturedRewardSource> {
        let archive = self.archive.as_ref()?;
        let matured =
            u64::from(tenure_height).checked_sub(nano_chainstate::MINER_REWARD_MATURITY)?;
        let start = self.tenure_start_block(archive, matured)?;
        // The fees are the tenure *before* the maturing one's, and so is the miner
        // they are paid to. A chain that does not reach it names the coinbase's
        // miner and leaves the other absent rather than reporting one twice.
        let previous = matured
            .checked_sub(1)
            .and_then(|tenure| self.tenure_start_block(archive, tenure));
        let miner = |block: &NakamotoBlock| {
            nano_chainstate::tenure_miner_address(block)
                .as_ref()
                .map_or_else(String::new, ToString::to_string)
        };
        Some(nano_rpc::MaturedRewardSource {
            from_stacks_block_hash: start.header.block_hash(),
            from_index_consensus_hash: start.block_id(),
            coinbase_miner: miner(&start),
            fee_miner: previous.as_ref().map_or_else(String::new, miner),
        })
    }

    /// The block a tenure started with, as this node executed and kept it.
    ///
    /// Refused unless it really does start a tenure. The durable map answers with
    /// the first block of a tenure that this node *executed*, which for the tenure
    /// it began mid-way through — at a checkpoint, or at a restart — is not that
    /// tenure's start block at all. Naming that block would be a confidently wrong
    /// `from_stacks_block_hash` rather than an absent one.
    fn tenure_start_block(
        &self,
        archive: &crate::archive::Archive,
        tenure_height: u64,
    ) -> Option<NakamotoBlock> {
        let height = self
            .chainstate
            .tenure_start_height(u32::try_from(tenure_height).ok()?)?;
        let block = archive.block_at_height(u64::from(height))?;
        nano_chainstate::starts_new_tenure(&block).then_some(block)
    }

    /// Announce executed Stacks blocks and locally derived Bitcoin blocks.
    ///
    /// The executor owns both boundaries: its chainstate executes Stacks blocks,
    /// and its sortition tracker derives Bitcoin blocks.
    pub fn announce_to(&mut self, observers: nano_rpc::EventDispatcher) {
        self.observers = Some(observers);
    }

    /// Measure every block this node executes into these metrics.
    pub fn publish_execution_to(&mut self, metrics: nano_rpc::NodeMetrics) {
        self.metrics = Some(metrics);
    }

    /// Keep every block this node executes in this store.
    ///
    /// Sealing a block already forgets it from staging; this is the other half,
    /// and it is what lets a node serve a block it executed rather than one a peer
    /// has just told it about.
    pub fn keep_executed_blocks(&mut self, archive: std::sync::Arc<crate::archive::Archive>) {
        self.archive = Some(archive);
    }

    /// Give back sealed states above a height. See `ChainState::discard_above`.
    ///
    /// Exposed because the residue of an abandoned block can appear *while the node
    /// is up*, not only across a restart: a block sealed and then not committed leaves
    /// the MARF ahead of the ledger, and the MARF refuses to begin a version it
    /// already holds, so every later round fails on that same block until something
    /// gives the state back.
    pub fn discard_above(&mut self, height: u32) -> Result<usize, ChainStateError> {
        self.chainstate.discard_above(height)
    }

    /// Take over a derived sortition chain, and say where to keep it.
    ///
    /// A chain that is not written down is re-derived from the checkpoint's burn
    /// anchor on every start, one Bitcoin block fetch at a time, over a run that
    /// grows for as long as the chain does.
    pub fn track_sortitions(
        &mut self,
        mut tracker: crate::sortition::SortitionTracker,
        state: std::path::PathBuf,
    ) {
        if let Some((bitcoin_height, observed_at, recipient)) = self.pending_waterfall_payout.take()
        {
            tracker.record_waterfall_payout(bitcoin_height, observed_at, recipient);
        }
        // Every tenure this node executed stands on a burn block that elected
        // somebody -- a tenure exists only because a sortition chose its miner -- so
        // the executed chain already knows the answer the resumed sortition chain
        // cannot reach back for. Taken from what this node executed itself, never
        // from a peer, and free: the consensus hashes are in the ledger and the
        // heights are a lookup in the history the checkpoint carries.
        //
        // This is what repairs a state written before the run of heights was kept.
        // Without it a resumed chain answers only at or above the burn block it was
        // seeded at, and a staged block standing lower stops execution -- mainnet at
        // 8,712,512, asked about burn 961,320 from a chain seeded at 961,342.
        let executed = self.chainstate.executed_tenures();
        let mut heights: Vec<u64> = executed
            .iter()
            .filter_map(|tenure| tracker.height_of_consensus_hash(*tenure))
            .collect();
        if !heights.is_empty() {
            heights.sort_unstable();
            heights.dedup();
            println!(
                "the sortition chain takes {} elected burn heights from the tenures this node \
                 executed, {} to {}",
                heights.len(),
                heights.first().copied().unwrap_or_default(),
                heights.last().copied().unwrap_or_default(),
            );
            tracker.remember_elected_heights(heights);
        }
        self.sortition = Some(tracker);
        self.sortition_state = Some(state);
    }

    /// Name the burn view a block stands on, from this node's own sortition chain.
    ///
    /// This is what used to require a peer, and the reason it did is worth stating
    /// exactly: the *only* thing that advanced the chain was the block being
    /// executed, so the chain's tip was always precisely the view under execution, a
    /// view arriving for the first time was always at least one burn block above the
    /// history, and no local lookup could ever answer for it. The chain is walked
    /// forward from this node's own Bitcoin source instead — bounded by that source's
    /// tip and by what one round may spend — until a burn block derives the hash
    /// asked about.
    ///
    /// Nothing here consults a peer, and nothing here falls back to one. A view the
    /// chain cannot name is reported and the chunk ends: the alternative is executing
    /// under a burn block a stranger picked, which is the whole of what
    /// [[049-derive-canonical-sortitions-from-the-local-burncha]] is about.
    fn local_view(&mut self, pox: &PoxInfo, view: nano_primitives::ConsensusHash) -> LocalView {
        let Some(payouts) = payout_schedule(pox) else {
            return LocalView::NoChain;
        };
        // Before anything walks: the retained snapshot window has to reach the burn
        // view *execution* is standing on, which is not near the tip this walk is
        // about to move. Locating one view runs the tracker to Bitcoin's tip while
        // a batch of blocks moves execution a dozen burn blocks, so the fixed window
        // dropped snapshots this node had derived itself and then refused to execute
        // under them. Said here because this is the one place that knows both.
        let executed = self.bitcoin_height();
        if executed > 0
            && let Some(tracker) = self.sortition.as_mut()
        {
            tracker.keep_for_execution(executed);
        }
        // Split the borrow: the tracker reads burn blocks through the same
        // source the executor holds.
        let Self {
            sortition: Some(tracker),
            bitcoin,
            ..
        } = self
        else {
            return LocalView::NoChain;
        };
        // The cheap answer first, and the order is the point. A view this chain has
        // already derived is a lookup in its own history and needs no burnchain at
        // all; asking Bitcoin where its chain ends before that would put one network
        // round trip on every *Stacks* block, and many Stacks blocks stand on one
        // burn block — which is the per-block cost [[049]] measured out of existence
        // and must not reinstate.
        if tracker.is_primed()
            && let Some(height) = tracker.height_of_consensus_hash(view)
        {
            self.sortition_gap = None;
            return LocalView::At(height);
        }
        let standing_on = tracker.tip().bitcoin_height;
        // Where this node's own burnchain ends, which is the bound on the walk. A
        // burnchain that cannot be read at all is not a burn view that is missing:
        // the walk is skipped and the round says so, rather than the chain being
        // abandoned in favour of a peer's answer.
        let burnchain_tip = match bitcoin.tip_height() {
            Ok(tip) => tip,
            Err(error) => {
                eprintln!("cannot ask Bitcoin where its chain ends: {error}");
                return LocalView::Unreached { standing_on };
            }
        };
        let Self {
            sortition: Some(tracker),
            bitcoin,
            ..
        } = self
        else {
            return LocalView::NoChain;
        };
        // Named before the walk, not after: every burn block costs a whole
        // Bitcoin block download, so a node closing a checkpoint's gap can be
        // busy for minutes, and a node that prints nothing for minutes teaches
        // an operator to guess at what it is doing. Once per burn view, because a
        // view already named returned above.
        if burnchain_tip > standing_on {
            println!(
                "walking the local sortition chain from burn {standing_on} to find burn view \
                 {view}, up to {} blocks toward Bitcoin's tip at {burnchain_tip}, one Bitcoin \
                 block download each",
                burnchain_tip - standing_on
            );
        }
        let located = tracker.locate_view(
            view,
            |height| bitcoin.block_at(height),
            burnchain_tip,
            payouts,
            crate::sortition::CATCH_UP_LIMIT,
        );
        let (found, walk) = match located {
            Ok(located) => located,
            // Reported and retried, not answered by a peer. The two ways this fails
            // are a burnchain that cannot be read — a network error, and next round
            // it may work — and a reward-cycle boundary a checkpoint-seeded chain
            // cannot cross, which is a real limit of such a chain and needs a
            // checkpoint past the boundary rather than a stranger's opinion.
            Err(error) => {
                eprintln!("deriving the burn view locally failed: {error}");
                return LocalView::Unreached { standing_on };
            }
        };
        // Priming counts as work worth reporting even when the chain was already
        // standing where it needed to be: it is six Bitcoin block downloads, seven
        // seconds on mainnet, and it is paid on every start — the largest single
        // item in this phase, and it used to print nothing at all because no
        // sortition came out of it.
        if walk.advanced > 0 || walk.primed > 0 {
            report_sortition_walk(&walk, tracker.tip().bitcoin_height);
        }
        // Written down as it advances rather than at shutdown, because a node
        // that is killed is exactly the one that must not start over — and only
        // as it advances: many Stacks blocks stand on one burn block, and
        // writing the whole derived history again for each of them cost a third
        // of a second per block on mainnet, where the history is 12 MB of JSON
        // that has not changed.
        if walk.advanced > 0 {
            self.save_sortitions();
        }
        if let Some(height) = found {
            self.sortition_gap = None;
            return LocalView::At(height);
        }
        let standing_on = self
            .sortition
            .as_ref()
            .map_or(standing_on, |tracker| tracker.tip().bitcoin_height);
        LocalView::Unreached { standing_on }
    }

    /// Write the derived chain down, so the next start resumes instead of
    /// re-deriving.
    fn save_sortitions(&self) {
        let written = std::time::Instant::now();
        match self.persist_sortitions_for_restart() {
            Ok(Some(_)) => println!(
                "the derived sortition chain is written down ({:.2}s)",
                written.elapsed().as_secs_f64()
            ),
            Ok(None) => {}
            Err(error) => {
                eprintln!("the derived sortition chain could not be written down: {error}");
            }
        }
    }

    /// Persist the burn view a Stacks fork can need immediately after retraction.
    fn persist_sortitions_for_restart(
        &self,
    ) -> Result<Option<u64>, crate::sortition::TrackerError> {
        let (Some(tracker), Some(state)) = (self.sortition.as_ref(), self.sortition_state.as_ref())
        else {
            return Ok(None);
        };
        // A branch can replace the first block of the current sortition without a
        // Bitcoin reorganization; its common parent then stands one burn block
        // lower. Saving exactly at execution made a restart unable to reach that
        // parent because a sortition chain only walks forward.
        tracker.save_standing_on(state, self.bitcoin_height().saturating_sub(1))?;
        Ok(Some(tracker.tip().bitcoin_height))
    }

    fn local_sortition(
        &mut self,
        pox: &PoxInfo,
        bitcoin_height: u64,
        bitcoin_spent: u64,
    ) -> Result<Option<LocalSortition>, CheckpointExecutionError> {
        if payout_schedule(pox).is_none() {
            return Ok(None);
        }
        let Some(tracker) = self.sortition.as_ref() else {
            return Ok(None);
        };
        // The snapshot for *this* burn view, not the chain's tip: the chain is walked
        // ahead until it names the view, so once a tenure's later blocks are being
        // executed the tip may already have moved on. Reading the tip was correct
        // only while the chain could go nowhere but where execution took it.
        let Some(snapshot) = tracker.snapshot_at(bitcoin_height) else {
            // Said once per gap rather than per block: a validation that never
            // runs looks exactly like one that always passes, so the condition
            // has to be named, but not at every block behind it.
            if self.sortition_gap != Some(bitcoin_height) {
                self.sortition_gap = Some(bitcoin_height);
                eprintln!(
                    "the local sortition chain holds no snapshot for burn {bitcoin_height}, \
                     which this block stands on: it ends at burn {} and keeps a bounded window \
                     behind that",
                    tracker.tip().bitcoin_height
                );
            }
            return Ok(None);
        };
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
        let local = LocalSortition::from_snapshot(snapshot);
        // Rejected rather than reported. A Nakamoto header's `bitcoin_spent` is
        // the running burn total of its view and carries threshold signer weight,
        // so this is the one field of a followed block that can be checked against
        // nano's own burnchain with nothing taken from a peer — and no state root
        // would catch it, because a node executing over the wrong chain of Bitcoin
        // blocks computes a perfectly consistent state for a chain nobody else is
        // on. It used to stop deriving and go back to the peer's sortitions, which
        // is exactly the wrong direction: it answered a disagreement about the
        // burnchain by trusting the peer more.
        if snapshot.total_burn != bitcoin_spent {
            return Err(CheckpointExecutionError::BitcoinSpent {
                bitcoin_height,
                header: bitcoin_spent,
                derived: snapshot.total_burn,
            });
        }
        Ok(Some(local))
    }

    /// Stand on a block this node executed before, after giving back the rest.
    ///
    /// A retraction rewinds the executed chain and the accounting, and this is the
    /// other half: the executor's own tip. Without it a node that retracted kept
    /// standing on the block it had just abandoned, so nothing staged was ever its
    /// child and no round after the switch executed anything — a stall that looks
    /// exactly like the one the fork switch was built to remove.
    ///
    /// The block comes back from the peer because a full block is not what a
    /// chainstate keeps, and its identity is checked rather than taken: a peer
    /// answering `/v3/blocks/:id` with something else must not be able to choose
    /// where this node stands.
    ///
    /// The three views derived from the tip are dropped rather than adjusted. The
    /// burn view and the cached sortition belonged to the branch just abandoned,
    /// and the Bitcoin height is re-read from the header this node wrote down for
    /// the block it now stands on.
    async fn stand_on_block(
        &mut self,
        node: &SyncClient,
        block_id: [u8; 32],
    ) -> Result<(), NodeExecutionError> {
        let block = node.block(StacksBlockId::from_bytes(block_id)).await?;
        if *block.block_id().as_bytes() != block_id {
            return Err(NodeExecutionError::Execution(
                CheckpointExecutionError::Link(format!(
                    "the peer answered for block {} with block {}, so this node cannot stand \
                     on the ancestor it retracted to",
                    hex::encode(block_id),
                    block.block_id()
                )),
            ));
        }
        self.stand_on_known_block(block);
        Ok(())
    }

    /// Update the in-memory and served tip after the durable chain was retracted.
    fn stand_on_known_block(&mut self, block: NakamotoBlock) {
        self.tip = block;
        self.bitcoin_view = None;
        self.bitcoin_height = 0;
        // And the blocks kept for serving, which are a claim about what this node
        // executed: everything above the block it now stands on it no longer did.
        if let Some(archive) = self.archive.as_ref()
            && let Err(error) = archive.retract_from(self.tip.header.chain_length + 1)
        {
            eprintln!("cannot give back the blocks this node retracted: {error}");
        }
    }

    /// Ask Bitcoin whether the burn blocks behind this node's sortitions still hold.
    ///
    /// One `block_hash_at` a round when nothing moved, which is the cheap half of
    /// `find_fork` done deliberately rather than as a side effect: the incremental
    /// read inside `BitcoinSource` notices a reorganization too, but only once it
    /// happens to read that height again, and what it did with the news was to
    /// stop deriving sortitions and go back to the peer's — answering a
    /// disagreement about the burnchain by trusting the peer more.
    ///
    /// A reorganization deeper than the chain's root is not survivable here and
    /// says so: nothing local can tell what replaced a checkpoint's own burn
    /// anchor.
    async fn check_burnchain(
        &mut self,
        node: &SyncClient,
        staging: &Staging,
    ) -> Result<Option<[u8; 32]>, NodeExecutionError> {
        let Self {
            sortition: Some(tracker),
            bitcoin,
            ..
        } = self
        else {
            return Ok(None);
        };
        let (height, hash) = {
            let tip = tracker.tip();
            (tip.bitcoin_height, *tip.bitcoin_header_hash.as_bytes())
        };
        match bitcoin.block_hash_at(height) {
            Ok(current) if current == hash => return Ok(None),
            Ok(_) => {}
            // A burnchain that cannot be read is not a burnchain that moved, and
            // treating the two alike would retract a chain over a network error.
            Err(error) => {
                eprintln!("cannot ask Bitcoin whether burn block {height} still holds: {error}");
                return Ok(None);
            }
        }
        println!(
            "Bitcoin no longer holds the block at height {height} this node snapshotted, \
             so the sortitions above the fork point are being given back"
        );
        let fork = tracker
            .find_fork(|height| bitcoin.block_hash_at(height))
            .map_err(|error| CheckpointExecutionError::Bitcoin(error.to_string()))?;
        let reorg = match fork {
            // The tip came back while the fork was being located, which is an
            // ordinary race on a chain that reorganized once and reorganized back.
            nano_sortition::Fork::Canonical => return Ok(None),
            nano_sortition::Fork::Above(height) => tracker
                .retract_above(height)
                .map_err(|error| CheckpointExecutionError::Bitcoin(error.to_string()))?,
            nano_sortition::Fork::BeyondChainRoot {
                root_bitcoin_height,
            } => {
                return Err(CheckpointExecutionError::Bitcoin(format!(
                    "the Bitcoin reorganization reaches below burn block {root_bitcoin_height}, \
                     which is where this node's sortition chain was seeded — nothing local can \
                     say what replaced it, so this state needs a checkpoint Bitcoin agrees with"
                ))
                .into());
            }
        };
        // The surviving chain's `PreStx` window only: an operation authorised by
        // an output in a block Bitcoin dropped was never an operation.
        bitcoin.invalidate_from(reorg.resume_bitcoin_height());
        let retraction = self.chainstate.retract(&reorg);
        // Written down before anything is executed on the replacement branch, so a
        // node killed here restarts on the retracted chain rather than on the one
        // Bitcoin abandoned.
        if let (Some(tracker), Some(state)) =
            (self.sortition.as_ref(), self.sortition_state.as_ref())
            && let Err(error) = tracker.save(state)
        {
            eprintln!("the retracted sortition chain could not be written down: {error}");
        }
        let Some(resume) = retraction
            .resume_from
            .filter(|_| !retraction.discarded.is_empty())
        else {
            println!(
                "{} sortitions were retracted and no block this node executed stood on any \
                 of them, so the executed chain is unchanged",
                reorg.depth()
            );
            return Ok(None);
        };
        println!(
            "a Bitcoin reorganization {} blocks deep took back {} Stacks blocks; standing on {} \
             and reading the burnchain again from {}",
            reorg.depth(),
            retraction.discarded.len(),
            hex::encode(resume),
            reorg.resume_bitcoin_height()
        );
        // Everything staged descends from the abandoned branch's tip or from
        // tenures that no longer exist, so it is refetched rather than sorted out.
        staging.clear()?;
        self.stand_on_block(node, resume).await?;
        Ok(Some(resume))
    }

    /// Stand on the block a branch this node already holds descends from.
    ///
    /// The fork a burn view cannot describe. A tenure can win a sortition, put a
    /// block out, and still be built around: the next sortition's miner commits
    /// to the tenure *before* it, and the chain carries on one block to the side
    /// of the one this node executed. Both branches then stand on the same
    /// unreorganized sortition chain, both tenures are canonical burn views, and
    /// there is no consensus hash whose presence on one side and absence on the
    /// other names where they parted — [`Self::switch_to_fork`] matches this
    /// node's own tip tenure, retracts nothing, and answers `None` forever. That
    /// is the mainnet stall at 8724697: 1509 blocks staged, none executable, for
    /// as long as the node ran
    /// ([[096-cross-a-stacks-fork-inside-one-sortition-chain]]).
    ///
    /// So the fork point is named as a block, and it is read off this node's own
    /// two records rather than asked for: the lowest block staging holds, and
    /// whether this node executed its parent. A peer supplies neither answer. It
    /// cannot name a block this node did not compute, cannot move the tip to a
    /// branch no higher than the one already executed, and cannot reach anything
    /// below the branch's own root — every block on it arrived carrying threshold
    /// signer weight and a burn view this node derived from Bitcoin itself.
    ///
    /// Staging is *kept*, unlike every other retraction here: the branch about to
    /// be executed is what it holds.
    ///
    /// Returns the block to resume from when a switch happened.
    async fn switch_to_staged_branch(
        &mut self,
        node: &SyncClient,
        staging: &Staging,
    ) -> Result<Option<[u8; 32]>, NodeExecutionError> {
        // Nothing to gain from a branch that reaches no further than the chain
        // already executed, which is what keeps a peer serving an old sibling
        // from rewinding this node round after round.
        let Some(highest) = staging.highest()? else {
            return Ok(None);
        };
        if highest.header.chain_length <= self.tip.header.chain_length {
            return Ok(None);
        }
        let Some((parent, _)) = staging.descent_resumes_at()? else {
            return Ok(None);
        };
        let parent = *parent.as_bytes();
        // The ordinary case, and the reason this is cheap: the branch descends
        // from the tip, so there is no fork and the executor was simply waiting
        // on a gap the descent above has now closed.
        if parent == *self.tip.block_id().as_bytes() || !self.chainstate.has_executed(parent) {
            return Ok(None);
        }
        // Fetch and validate the local ancestor before retracting durable state.
        // A peer or network failure must leave the persisted and in-memory tips
        // naming the same chain.
        let resume_block = node.block(StacksBlockId::from_bytes(parent)).await?;
        if *resume_block.block_id().as_bytes() != parent {
            return Err(NodeExecutionError::Execution(
                CheckpointExecutionError::Link(format!(
                    "the peer answered for block {} with block {}, so this node cannot stand on \
                     the ancestor its staged branch names",
                    hex::encode(parent),
                    resume_block.block_id()
                )),
            ));
        }
        // This belongs before the chainstate write, not merely to the periodic
        // burnchain walk. If the tracker was already current, that walk had
        // nothing to save; a kill after retraction would otherwise leave a sealed
        // parent below a sortition seed that can only walk forward.
        self.persist_sortitions_for_restart().map_err(|error| {
            NodeExecutionError::Execution(CheckpointExecutionError::Bitcoin(format!(
                "the sortition chain could not retain the burn view needed to restart this Stacks fork: {error}"
            )))
        })?;
        let retraction = self.chainstate.retract_to(parent);
        if retraction.discarded.is_empty() {
            return Ok(None);
        }
        println!(
            "a branch {} blocks longer parted at {}: giving back {} blocks this node executed \
             and standing on it again",
            highest
                .header
                .chain_length
                .saturating_sub(self.tip.header.chain_length),
            hex::encode(parent),
            retraction.discarded.len()
        );
        self.stand_on_known_block(resume_block);
        Ok(retraction.resume_from)
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
        // Nothing a peer answers here can fail the round. Its view of where two
        // chains parted is a hint that saves this node a search, never a
        // consensus input — a 400, a closed socket or a chain it does not
        // recognise all mean the same thing, which is that this peer cannot help.
        // Returning the error instead ended the round before execution: a peer
        // answering `NotInSameFork` stalled a mainnet follower for 728 rounds
        // ([[096-cross-a-stacks-fork-inside-one-sortition-chain]]).
        let theirs = match node.tenure_fork_info(theirs, oldest).await {
            Ok(view) => view,
            Err(error) => {
                eprintln!("a peer could not say where its burn view parted from this one: {error}");
                return Ok(None);
            }
        };
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
        // The executor's own tip, which the retraction does not move. Without this
        // the switch rewound the ledger and left the executor standing on the block
        // it had just abandoned, so no staged block was ever its child and every
        // round after the switch executed nothing — the stall this whole path
        // exists to remove, one step further along.
        self.stand_on_block(node, block).await?;
        Ok(retraction.resume_from)
    }

    /// Find the burn view a block inherits, by walking back through its tenure.
    ///
    /// Only a tenure change states one, so a block that carries none stands on
    /// the view of the last block before it that did. The walk stops when the
    /// tenure does, since a block cannot inherit a view across tenures.
    async fn bitcoin_view_of(
        peers: &mut TenureSource,
        block: &NakamotoBlock,
    ) -> Result<Option<nano_primitives::ConsensusHash>, NodeExecutionError> {
        let mut parent = block.header.parent_block_id;
        for _ in 0..TENURE_WALK_LIMIT {
            let ancestor = peers.block(parent).await?;
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

    /// Record the sortition that elected this block's tenure.
    ///
    /// Its validation inputs and burn height come from the tenure's own snapshot,
    /// while Clarity-visible fields remain those of the current burn view. They are
    /// the same block until an extend moves them apart. The local sortition history
    /// must place both; there is no peer fallback.
    fn record_tenure_sortition(
        &self,
        block: &NakamotoBlock,
        bitcoin_context: &mut BitcoinBlockContext,
    ) -> Result<(), CheckpointExecutionError> {
        let tracker = self.sortition.as_ref().ok_or_else(|| {
            CheckpointExecutionError::Link(format!(
                "the local sortition chain cannot place tenure {} for block {} at a burn height",
                block.header.consensus_hash, block.header.chain_length
            ))
        })?;
        let tenure = tracker
            .height_of_consensus_hash(block.header.consensus_hash)
            .ok_or_else(|| {
                CheckpointExecutionError::Link(format!(
                    "the local sortition chain cannot place tenure {} for block {} at a burn height",
                    block.header.consensus_hash, block.header.chain_length
                ))
            })?;
        let snapshot = tracker.snapshot_at(tenure).ok_or_else(|| {
            CheckpointExecutionError::Link(format!(
                "the local sortition chain no longer holds the authentication snapshot for tenure \
                 {} at burn {tenure}",
                block.header.consensus_hash
            ))
        })?;
        LocalSortition::from_snapshot(snapshot).record_authentication(bitcoin_context);
        let view_has_moved = self
            .bitcoin_view
            .is_some_and(|view| view != block.header.consensus_hash);
        if view_has_moved && tenure != bitcoin_context.height {
            let view = bitcoin_context.height;
            bitcoin_context.move_to_burn_block(tenure);
            bitcoin_context.extend_view_to(view);
        }
        Ok(())
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

    /// The Bitcoin context one staged block executes under, and the burn view it
    /// was executed on — or nothing when the chunk has to end here.
    ///
    /// Everything a chunk can be cut short by is in this one step, and the execution
    /// below it asks nobody anything. Three things can end it: a peer that started
    /// rate limiting while the burn view was being walked back to, a burn view this
    /// node's own sortition chain cannot name yet, and a tenure whose accumulated
    /// coinbase that chain cannot measure.
    ///
    /// Every consensus field below — the burn height itself, the burn header hash
    /// and time, the VRF seed, the two miner spends, the sortition hash, the winning
    /// leader key, and the coinbase a tenure accumulated — comes from this node's
    /// own burnchain. A peer may supply an ancestor block that states a view, but
    /// that view is only an identifier until the local sortition chain places and
    /// derives it.
    async fn context_for(
        &mut self,
        peers: &mut TenureSource,
        pox: &PoxInfo,
        block: &NakamotoBlock,
        timing: &mut ExecutionTiming,
        previous_view: &mut Option<nano_primitives::ConsensusHash>,
    ) -> Result<Option<(BitcoinBlockContext, u64)>, NodeExecutionError> {
        // The burn view, not the tenure. A tenure that outlives the burn
        // block that elected it is extended, and the extension moves the
        // view forward — so a block mid-tenure sees a later burn height
        // than its own sortition, and `burn-block-height` is what a
        // contract stores. Only a tenure change states the view, so it
        // carries forward to the blocks that follow.
        if let Some(view) = immediate_bitcoin_view(block, &self.tip) {
            self.bitcoin_view = Some(view);
        } else if self.bitcoin_view.is_none() {
            // A resumed node did not execute the tenure change that stated
            // the view, so it walks back to it. Blocks, not sortitions.
            let Some(walked) = ended_by_a_rate_limit(Self::bitcoin_view_of(peers, block).await)?
            else {
                return Ok(None);
            };
            self.bitcoin_view = walked;
        }
        let view = self.bitcoin_view.unwrap_or(block.header.consensus_hash);
        if previous_view.replace(view) != Some(view) {
            timing.views += 1;
        }
        let phase = std::time::Instant::now();
        let local_view = self.local_view(pox, view);
        timing.local += phase.elapsed();
        // No peer answer, in any branch. This was the last place a stranger could
        // choose the burn block a block executes under, and the four fields it chose
        // are read back by Clarity and by the tenure VRF rule.
        match local_view {
            LocalView::NoChain => {
                eprintln!(
                    "this node derives no sortitions of its own, so it cannot say which burn                      block {} stands on and will not take a peer's word for it: give the                      checkpoint a sortition history and a payout calendar",
                    block.header.chain_length
                );
                return Ok(None);
            }
            LocalView::At(_) => {}
            LocalView::Unreached { standing_on } => {
                eprintln!(
                    "the local sortition chain cannot name burn view {view}, standing on burn \
                     {standing_on}: this node will not execute block {} under a burn block a \
                     peer picked, and the next round walks again",
                    block.header.chain_length
                );
                return Ok(None);
            }
        }
        // The only remaining source, and it is this node's own arithmetic over
        // Bitcoin blocks it downloaded.
        let LocalView::At(bitcoin_height) = local_view else {
            return Ok(None);
        };
        let mut bitcoin_context = pox.bitcoin_context();
        bitcoin_context.move_to_burn_block(bitcoin_height);
        let phase = std::time::Instant::now();
        // Everything this node's own burnchain can answer, from there. The validation
        // inputs `check_tenure_vrf` reads, and the Clarity-visible ones that move a
        // state root: see `LocalSortition`.
        let local = self.local_sortition(pox, bitcoin_height, block.header.bitcoin_spent)?;
        timing.local += phase.elapsed();
        if let Some(local) = local {
            local.record(&mut bitcoin_context);
        }
        // After `record`, which moves the whole burn block: the tenure's own burn
        // height, which is the view's until this block extends its tenure past the
        // sortition that elected it. Derived like every other burn fact here -- the
        // tenure is named by the block's own `consensus_hash` and this node's
        // sortition chain says which burn block that is -- and left as the view where
        // the chain cannot name it, which is what it is wherever nothing was
        // extended.
        //
        // Exactly one rule reads it: the prepare-phase signer-set update, which
        // stacks-core drives from the tenure's sortition. Reading the view there is
        // what parted the roots at pox-5 height 931.
        self.record_tenure_sortition(block, &mut bitcoin_context)?;
        let phase = std::time::Instant::now();
        self.seed_burn_headers(bitcoin_height);
        timing.headers += phase.elapsed();
        let phase = std::time::Instant::now();
        let Some(bitcoin_context) = self.tenure_coinbase(block, bitcoin_context, bitcoin_height)
        else {
            return Ok(None);
        };
        timing.coinbase += phase.elapsed();
        Ok(Some((bitcoin_context, bitcoin_height)))
    }

    /// Fill in the coinbase a tenure-start block's tenure accumulated.
    ///
    /// A tenure collects the coinbase of every burn block since the last one that
    /// elected somebody, so the number turns on one fact: the height of that block.
    /// It used to cost two peer requests per tenure-start block, which is the one
    /// remaining peer answer that was **minted** rather than merely read — every
    /// other field a stranger could get wrong makes the block fail to seal, while
    /// this one makes it seal a different balance.
    ///
    /// It is pure local knowledge once the sortition chain runs ahead of execution:
    /// the snapshots hold `winner_txid` per burn block, so the walk is a walk over
    /// this node's own window. A window that cannot reach the answer ends the chunk
    /// rather than minting a zero.
    ///
    /// There is no peer branch left. This used to fall back to the peer's two-request
    /// walk whenever the local chain could not name the view, which made the one
    /// *minted* quantity in the whole context a stranger's answer: every other field
    /// a peer could get wrong makes the block fail to seal, while this one makes it
    /// seal a different balance
    /// ([[077-remove-peer-derived-consensus-execution-fallbacks]]).
    fn tenure_coinbase(
        &mut self,
        block: &NakamotoBlock,
        mut bitcoin_context: BitcoinBlockContext,
        bitcoin_height: u64,
    ) -> Option<BitcoinBlockContext> {
        let schedule = self.chainstate.accounting_mut().schedule();
        let Some(schedule) = schedule.filter(|_| nano_chainstate::starts_new_tenure(block)) else {
            return Some(bitcoin_context);
        };
        let Some(previous) = self
            .sortition
            .as_ref()
            .and_then(|tracker| tracker.previous_sortition_height(bitcoin_height))
        else {
            eprintln!(
                "the local sortition chain cannot say which burn block before {bitcoin_height} \
                 last elected somebody, and a tenure's accumulated coinbase is minted from \
                 that height — so block {} is not executed rather than minting a guess",
                block.header.chain_length
            );
            return None;
        };
        bitcoin_context.accumulated_coinbase =
            schedule.accumulated_at(bitcoin_height, Some(previous));
        Some(bitcoin_context)
    }

    /// Execute staged blocks forward from this node's tip, up to `budget`.
    ///
    /// `NANO_TIMING=1` makes each round say where its seconds went.
    ///
    /// A peer that starts rate limiting part-way through ends the chunk rather
    /// than the round: every block before it is sealed and committed, and what
    /// remains staged is executed by the next round without being fetched again.
    /// Raising it as an error instead lost nothing durable — the blocks were
    /// sealed — but it reported a round that had made progress as a failure, and
    /// sent the follow loop looking for another peer over a 429.
    async fn execute_staged(
        &mut self,
        peers: &mut TenureSource,
        pox: &PoxInfo,
        staging: &Staging,
        budget: usize,
    ) -> Result<ExecutedChunk, NodeExecutionError> {
        let mut executed = 0;
        let mut tenure_starts = 0;
        let mut rate_limited = false;
        let mut timing = ExecutionTiming::default();
        let mut previous_view = None;
        while executed < budget {
            let representations = staging.child_representations(self.tip.block_id())?;
            let Some(first) = representations.first() else {
                break;
            };
            let selected_id = first.block_id();
            let Some((bitcoin_context, executed_height)) = self
                .context_for(peers, pox, first, &mut timing, &mut previous_view)
                .await?
            else {
                rate_limited = true;
                break;
            };
            let phase = std::time::Instant::now();
            let mut rejected = None;
            let mut accepted = None;
            for block in representations {
                match self.authenticate(&block, bitcoin_context) {
                    Ok(authenticated) => {
                        accepted = Some((block, authenticated));
                        break;
                    }
                    Err(error) => rejected = Some(error),
                }
            }
            let Some((block, authenticated)) = accepted else {
                // Signer signatures are absent from the block ID. Keeping only
                // rejected bytes would make descent hide a later finalized form.
                staging.remove(selected_id)?;
                return Err(rejected
                    .expect("a non-empty set of representations was rejected")
                    .into());
            };
            staging.put(&authenticated)?;
            let applied = self.apply_authenticated(authenticated)?;
            if nano_chainstate::starts_new_tenure(&block) {
                tenure_starts += 1;
            }
            let block_execution = phase.elapsed();
            timing.execution += block_execution;
            if let Some(metrics) = self.metrics.as_ref() {
                let contract_calls = block
                    .transactions
                    .iter()
                    .filter(|transaction| {
                        matches!(
                            transaction.payload().data(),
                            nano_codec::TransactionPayloadData::ContractCall { .. }
                        )
                    })
                    .count();
                metrics.publish_block_execution(
                    &applied.execution_cost,
                    applied.receipts.len(),
                    contract_calls,
                    block_execution,
                );
            }
            // Executing a block is synchronous and takes as long as it takes, and a
            // round executes up to five hundred of them. Between two blocks standing
            // on the same burn view nothing above awaits, so the whole run was one
            // uninterrupted task: a worker thread pegged at 100% for twelve minutes
            // while the other fifteen sat idle, and the runtime never got the chance
            // to poll anything else on it.
            //
            // What that costs is the node's whole HTTP surface. A live mainnet
            // follower stopped answering `/v2/info` — its listening socket holding
            // seven connections it had never accepted — for as long as it was
            // catching up, which is exactly when a signer, a client or an operator
            // most wants to ask. Handing the scheduler a turn between blocks is the
            // whole fix; the work is unchanged.
            tokio::task::yield_now().await;
            trace_executed_block(&block, executed_height);
            let phase = std::time::Instant::now();
            self.announce_block(&block, &applied, bitcoin_context);
            timing.dispatch += phase.elapsed();
            let phase = std::time::Instant::now();
            // Kept where it is forgotten from: the two halves of "this block is
            // executed now" belong in one place. A store that will not take it is
            // said and stepped over — nothing here is consensus, and a node that
            // stopped executing over an archive write would be trading the chain
            // for a convenience.
            self.keep_executed_block(&block);
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
        Ok(ExecutedChunk {
            blocks: executed,
            tenure_starts,
            rate_limited,
        })
    }

    /// Execute a candidate block on the current tip without adopting it.
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
        Ok(self.chainstate.preview_nakamoto_block_selecting(
            bitcoin_context,
            &operations.operations,
            Some(*self.tip.block_id().as_bytes()),
            candidate,
            &[],
            miner_key,
        )?)
    }

    /// Execute a candidate block together with transactions it may drop, and
    /// derive the state root the admitted set produces without adopting it.
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
        Ok(self.chainstate.preview_nakamoto_block_selecting(
            bitcoin_context,
            &operations.operations,
            Some(*self.tip.block_id().as_bytes()),
            candidate,
            candidates,
            miner_key,
        )?)
    }

    /// Seal a threshold-signed block the network accepted and adopt its tip.
    pub fn accept_own_block(
        &mut self,
        block: &NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
    ) -> Result<AppliedBlock, CheckpointExecutionError> {
        let operations = self
            .bitcoin
            .block_at(bitcoin_context.height)
            .map_err(|error| CheckpointExecutionError::Bitcoin(error.to_string()))?;
        let applied = self
            .chainstate
            .append_nakamoto_block_with_bitcoin_operations_using_registry(
                bitcoin_context,
                &operations.operations,
                Some(*self.tip.block_id().as_bytes()),
                block,
                self.waterfall_registry.as_deref(),
            )?;
        self.remember_waterfall_payout(&applied, bitcoin_context);
        self.adopt_executed_tip(block.clone(), bitcoin_context);
        self.announce_block(block, &applied, bitcoin_context);
        self.keep_executed_block(block);
        Ok(applied)
    }

    /// Access the portable accounting ledger backing matured native rewards.
    pub const fn chainstate_mut(&mut self) -> &mut ChainState {
        &mut self.chainstate
    }

    /// Cache residency sampled while the executor is already exclusively owned.
    #[must_use]
    pub fn cache_usage(&mut self) -> nano_rpc::ExecutionCacheReport {
        let usage = self.chainstate.vm_mut().cache_usage();
        nano_rpc::ExecutionCacheReport {
            marf_node_entries: usage.marf_node_entries,
            marf_node_bytes: usage.marf_node_bytes,
            marf_auxiliary_bytes: usage.marf_auxiliary_bytes,
            clarity_value_entries: usage.clarity_value_entries,
            clarity_value_bytes: usage.clarity_value_bytes,
            wasm_module_entries: usage.wasm_module_entries,
            wasm_module_bytes: usage.wasm_module_bytes,
        }
    }

    /// The burn view this node derived for itself, for weighing a peer's tip.
    ///
    /// `None` when this node derives no sortitions — then a fork choice has
    /// nothing of its own to compare a candidate's burnchain against and says so,
    /// rather than falling back to the candidate's own answer about it.
    #[must_use]
    pub fn burn_view(&self) -> Option<&dyn nano_sync::BurnView> {
        self.sortition
            .as_ref()
            .map(|tracker| tracker as &dyn nano_sync::BurnView)
    }

    /// The signer set this node's own executed state records for the cycle its
    /// burn view sits in, for weighing a peer's tip.
    ///
    /// The same read `check_signer_signatures` makes before executing a block, so
    /// what selection weighs against and what execution enforces are one value
    /// from one place. A cycle with nothing recorded answers `None`, which leaves
    /// length to decide — the same policy execution takes, for the same reason.
    pub fn recorded_signer_set(
        &mut self,
        context: BitcoinBlockContext,
    ) -> Option<nano_chainstate::SignerWeights> {
        let mut context = context;
        context.move_to_burn_block(self.bitcoin_height());
        self.chainstate.recorded_signer_set(context).ok()
    }

    /// Return the most recently executed block.
    #[must_use]
    pub const fn tip(&self) -> &NakamotoBlock {
        &self.tip
    }

    /// The latest burn block this node derived, including a no-winner block.
    pub(crate) fn local_burn_tip(
        &self,
    ) -> Result<nano_sync::SortitionInfo, CheckpointExecutionError> {
        let tracker = self.sortition.as_ref().ok_or_else(|| {
            CheckpointExecutionError::Link(
                "the miner has no locally derived sortition chain".to_owned(),
            )
        })?;
        tracker
            .sortition_info_at(tracker.tip().bitcoin_height)
            .ok_or_else(|| {
                CheckpointExecutionError::Link(
                    "the local sortition tip has no retained snapshot".to_owned(),
                )
            })
    }

    /// The burn view the locally executed tip stands on.
    pub(crate) fn local_executed_burn_view(
        &self,
    ) -> Result<nano_sync::SortitionInfo, CheckpointExecutionError> {
        self.sortition
            .as_ref()
            .and_then(|tracker| tracker.sortition_info_at(self.bitcoin_height))
            .ok_or_else(|| {
                CheckpointExecutionError::Link(format!(
                    "the local sortition chain has no snapshot for executed burn {}",
                    self.bitcoin_height
                ))
            })
    }

    /// The latest locally elected tenure, which can precede a no-winner tip.
    pub(crate) fn latest_local_winner(
        &self,
    ) -> Result<Option<nano_sync::SortitionInfo>, CheckpointExecutionError> {
        let tip = self.local_burn_tip()?;
        if tip.was_sortition {
            return Ok(Some(tip));
        }
        let Some(height) = self
            .sortition
            .as_ref()
            .and_then(|tracker| tracker.previous_sortition_height(tip.bitcoin_height))
        else {
            return Ok(None);
        };
        Ok(self
            .sortition
            .as_ref()
            .and_then(|tracker| tracker.sortition_info_at(height)))
    }

    /// Every block-signing key registered on the locally derived burnchain.
    pub(crate) fn registered_local_miner_keys(&self) -> Vec<nano_primitives::Hash160> {
        self.sortition.as_ref().map_or_else(
            Vec::new,
            crate::sortition::SortitionTracker::registered_signing_key_hashes,
        )
    }

    /// The two locally elected `.miners` writers in stacks-core's slot order.
    pub(crate) fn local_miner_slot_writers(&self) -> Option<[nano_primitives::Hash160; 2]> {
        self.sortition
            .as_ref()
            .and_then(crate::sortition::SortitionTracker::miner_slot_writers)
    }

    /// One locally derived sortition, selected by its consensus hash.
    pub(crate) fn local_sortition_info(
        &self,
        consensus_hash: nano_primitives::ConsensusHash,
    ) -> Result<nano_sync::SortitionInfo, CheckpointExecutionError> {
        let tracker = self.sortition.as_ref().ok_or_else(|| {
            CheckpointExecutionError::Link(
                "the miner has no locally derived sortition chain".to_owned(),
            )
        })?;
        let height = tracker
            .height_of_consensus_hash(consensus_hash)
            .ok_or_else(|| {
                CheckpointExecutionError::Link(format!(
                    "the local sortition chain does not contain tenure {consensus_hash}"
                ))
            })?;
        tracker.sortition_info_at(height).ok_or_else(|| {
            CheckpointExecutionError::Link(format!(
                "the local sortition snapshot for tenure {consensus_hash} was not retained"
            ))
        })
    }

    /// Consensus fields a tenure-start proposal takes from local sortition state.
    pub(crate) fn local_tenure_view(
        &self,
        consensus_hash: nano_primitives::ConsensusHash,
    ) -> Result<BitcoinTenureView, CheckpointExecutionError> {
        let tracker = self.sortition.as_ref().ok_or_else(|| {
            CheckpointExecutionError::Link(
                "the miner has no locally derived sortition chain".to_owned(),
            )
        })?;
        let height = tracker
            .height_of_consensus_hash(consensus_hash)
            .ok_or_else(|| {
                CheckpointExecutionError::Link(format!(
                    "the local sortition chain does not contain tenure {consensus_hash}"
                ))
            })?;
        let snapshot = tracker.snapshot_at(height).ok_or_else(|| {
            CheckpointExecutionError::Link(format!(
                "the local sortition snapshot for tenure {consensus_hash} was not retained"
            ))
        })?;
        Ok(BitcoinTenureView {
            total_burn: snapshot.total_burn,
            sortition_hash: *snapshot.sortition_hash.as_bytes(),
        })
    }

    /// The parent tenure and miner nonce from this node's sealed chain.
    pub(crate) fn local_parent_tenure(
        &mut self,
        principal: &clarity::vm::types::PrincipalData,
    ) -> Result<ParentTenure, CheckpointExecutionError> {
        let consensus_hash = self.tip.header.consensus_hash;
        let (start_block_id, start_height) = self
            .chainstate
            .tenure_start(consensus_hash)
            .ok_or_else(|| {
                CheckpointExecutionError::Link(format!(
                    "the locally executed tenure {consensus_hash} has no authenticated start block"
                ))
            })?;
        let blocks = self
            .tip
            .header
            .chain_length
            .checked_sub(u64::from(start_height))
            .and_then(|blocks| blocks.checked_add(1))
            .and_then(|blocks| u32::try_from(blocks).ok())
            .ok_or_else(|| {
                CheckpointExecutionError::Link(format!(
                    "the locally executed tenure {consensus_hash} has an invalid start height"
                ))
            })?;
        let miner_nonce = self.chainstate.account_nonce(principal)?;
        Ok(ParentTenure {
            tip: TenureTip {
                consensus_hash,
                block_id: self.tip.block_id(),
                height: self.tip.header.chain_length,
                bitcoin_spent: self.tip.header.bitcoin_spent,
                timestamp: self.tip.header.timestamp,
            },
            start_block_id,
            blocks,
            miner_nonce,
        })
    }

    /// Build the execution context for a locally assembled proposal.
    pub(crate) fn local_mining_context(
        &mut self,
        pox: &PoxInfo,
        block: &NakamotoBlock,
        burn_view: nano_primitives::ConsensusHash,
    ) -> Result<BitcoinBlockContext, CheckpointExecutionError> {
        let bitcoin_height = self
            .sortition
            .as_ref()
            .and_then(|tracker| tracker.height_of_consensus_hash(burn_view))
            .ok_or_else(|| {
                CheckpointExecutionError::Link(format!(
                    "the local sortition chain cannot place proposal burn view {burn_view}"
                ))
            })?;
        let mut context = pox.bitcoin_context();
        context.move_to_burn_block(bitcoin_height);
        self.local_sortition(pox, bitcoin_height, block.header.bitcoin_spent)?
            .ok_or_else(|| {
                CheckpointExecutionError::Link(format!(
                    "the local sortition chain cannot authenticate proposal burn {bitcoin_height}"
                ))
            })?
            .record(&mut context);
        self.record_tenure_sortition(block, &mut context)?;
        self.seed_burn_headers(bitcoin_height);
        self.tenure_coinbase(block, context, bitcoin_height)
            .ok_or_else(|| {
                CheckpointExecutionError::Link(format!(
                    "the local sortition chain cannot derive proposal coinbase at burn {bitcoin_height}"
                ))
            })
    }

    /// Signer weights the locally executed `.signers` contract records.
    pub(crate) fn local_proposal_signers(
        &mut self,
        context: BitcoinBlockContext,
    ) -> Result<SignerWeights, CheckpointExecutionError> {
        Ok(self.chainstate.recorded_signer_set(context)?)
    }

    /// Account state used to order peer-supplied mempool candidates.
    pub(crate) fn local_mempool_accounts(
        &mut self,
        mempool: &nano_mempool::Mempool,
    ) -> Result<HashMap<nano_address::StacksAddress, nano_mempool::Account>, CheckpointExecutionError>
    {
        let mut accounts = HashMap::new();
        for address in mempool.addresses() {
            let principal = clarity::vm::types::PrincipalData::Standard(
                clarity::vm::types::StandardPrincipalData::new(
                    address.version(),
                    *address.hash160().as_bytes(),
                )
                .map_err(|error| CheckpointExecutionError::Link(error.to_string()))?,
            );
            accounts.insert(
                address,
                nano_mempool::Account {
                    nonce: self.chainstate.account_nonce(&principal)?,
                    balance: Some(self.chainstate.account_balance(&principal)?),
                },
            );
        }
        Ok(accounts)
    }

    /// Walk the derived sortition chain toward Bitcoin's tip, on Bitcoin's clock.
    ///
    /// Every other walk is driven by execution — the chain is advanced to name the
    /// burn view a staged block stands on — so a node at the chain tip with nothing
    /// staged never advanced at all. It then held no snapshot for its *own* tip's
    /// burn view, and answered `/v3/sortitions` with `503` while perfectly healthy
    /// and simply idle, which is the condition a signer spends most of its time in.
    ///
    /// Bounded and quiet: a walk that finds nothing new costs one `tip_height` call,
    /// and only a walk that actually moved is written down or reported.
    pub fn follow_burnchain(&mut self, pox: &PoxInfo) -> u64 {
        let (advanced, notifications) = self.follow_burnchain_deferred(pox);
        self.announce_burn_blocks(&notifications);
        advanced
    }

    pub(crate) fn follow_burnchain_deferred(
        &mut self,
        pox: &PoxInfo,
    ) -> (u64, Vec<sortition::BurnNotification>) {
        let Some(payouts) = payout_schedule(pox) else {
            return (0, Vec::new());
        };
        let executed = self.bitcoin_height();
        let Self {
            sortition: Some(tracker),
            bitcoin,
            ..
        } = self
        else {
            return (0, Vec::new());
        };
        if executed > 0 {
            tracker.keep_for_execution(executed);
        }
        let Ok(burnchain_tip) = bitcoin.tip_height() else {
            // Reported by the execution path already, and every round would repeat
            // it: a burnchain that cannot be read is not news twice a minute.
            return (0, Vec::new());
        };
        if burnchain_tip <= tracker.tip().bitcoin_height {
            return (0, Vec::new());
        }
        let standing_on = tracker.tip().bitcoin_height;
        match tracker.follow_burnchain(
            |height| bitcoin.block_at(height),
            burnchain_tip,
            payouts,
            crate::sortition::CATCH_UP_LIMIT,
        ) {
            Ok(walk) if walk.advanced > 0 => {
                let tip = tracker.tip().bitcoin_height;
                println!(
                    "derived {} sortitions locally as Bitcoin advanced, from burn {standing_on} \
                     to {tip}",
                    walk.advanced
                );
                let notifications = tracker.burn_notifications_after(standing_on);
                self.save_sortitions();
                (walk.advanced, notifications)
            }
            Ok(_) => (0, Vec::new()),
            Err(error) => {
                eprintln!("following the burnchain locally failed: {error}");
                (0, Vec::new())
            }
        }
    }

    /// Announce Bitcoin blocks only after their locally derived view is public.
    pub(crate) fn announce_burn_blocks(&self, notifications: &[sortition::BurnNotification]) {
        let Some(observers) = self.observers.as_ref() else {
            return;
        };
        for notification in notifications {
            let payload = nano_rpc::new_burn_block_payload(
                notification.bitcoin_block_hash,
                notification.bitcoin_height,
                notification.consensus_hash,
                notification.parent_bitcoin_block_hash,
                notification.burned,
            );
            observers.dispatch(nano_rpc::EventKind::NewBurnBlock, &payload);
        }
    }

    /// The current sortitions this node derived from its local Bitcoin source.
    ///
    /// What `/v3/sortitions` answers from. This signer-facing route advances with
    /// the locally derived Bitcoin chain even when the sealed Stacks tip has not
    /// advanced under it yet. `/v2/info` continues to report the executed view.
    ///
    /// The pair, because a signer reads its whole view of who may mine from one
    /// route and refuses to act on a first entry whose `last_sortition_ch` names an
    /// entry that is not there.
    ///
    /// Empty where this node derives no sortitions at all, which is the honest
    /// answer and the one the route turns into a 503: the alternative was serving
    /// the burn view a *peer* reported, which is the one input a follower must not
    /// take from a stranger.
    #[must_use]
    pub fn derived_sortitions(&self) -> Vec<nano_sync::SortitionInfo> {
        self.sortition.as_ref().map_or_else(
            Vec::new,
            crate::sortition::SortitionTracker::recent_sortitions,
        )
    }

    /// The newest burn block this node derived locally.
    #[must_use]
    pub fn derived_bitcoin_height(&self) -> u64 {
        self.sortition.as_ref().map_or_else(
            || self.bitcoin_height(),
            |tracker| tracker.tip().bitcoin_height,
        )
    }

    /// Place an ancestor's tenure on this node's burnchain for a partial header backfill.
    pub(crate) fn local_ancestor_burn_context(
        &self,
        tenure: nano_primitives::ConsensusHash,
    ) -> Result<(u64, [u8; 32]), CheckpointExecutionError> {
        let height = self
            .sortition
            .as_ref()
            .and_then(|tracker| tracker.height_of_consensus_hash(tenure))
            .ok_or_else(|| {
                CheckpointExecutionError::Link(format!(
                    "ancestor tenure {tenure} is absent from the local sortition history"
                ))
            })?;
        let hash = self
            .bitcoin
            .block_hash_at(height)
            .map_err(|error| CheckpointExecutionError::Bitcoin(error.to_string()))?;
        Ok((height, hash))
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

#[cfg(test)]
mod peer_boundary_tests {
    /// The execution path holds no route back to a peer's opinion of a burn view.
    ///
    /// A source check, and it says so. The behaviour it guards cannot be reached from
    /// a unit test — it needs a checkpoint, an executor and a burnchain — but what it
    /// guards against is a one-line reintroduction, which is exactly what happened
    /// before: `LocalView::NoChain` filled the Bitcoin height, the burn header hash,
    /// the timestamp and the VRF seed from `/v3/sortitions`, and `tenure_coinbase`
    /// asked a peer for the accumulated coinbase. Three of those are read back by
    /// Clarity and so move a state root; the fourth decides whether a tenure is the
    /// one the network elected; and the coinbase is *minted*, so a wrong answer seals
    /// a different balance rather than failing to seal at all.
    ///
    /// So the assertion is on the two functions that build a block's execution
    /// context, and it is deliberately about *names*: a peer's sortition and its
    /// coinbase walk have no business being mentioned in either of them.
    #[test]
    fn no_peer_answer_reaches_a_block_s_execution_context() {
        let source = include_str!("lib.rs");
        for (function, forbidden) in [
            ("async fn context_for", "peers.sortition"),
            ("async fn context_for", "sortition_for"),
            ("fn tenure_coinbase", "tenure_coinbase_context"),
            ("fn local_sortition", "SortitionInfo"),
        ] {
            let Some(start) = source.find(function) else {
                panic!("{function} is gone; this guard has to move with it");
            };
            // To the next item at the same indentation, which is where a method ends.
            let body = &source[start..];
            let end = body[1..].find("\n    /// ").map_or(body.len(), |at| at + 1);
            assert!(
                !body[..end].contains(forbidden),
                "{function} mentions {forbidden}: a peer cannot be allowed to choose the \
                 burn view a block executes under"
            );
        }
    }

    #[test]
    fn an_extended_view_reloads_the_tenure_s_authentication() {
        let source = include_str!("lib.rs");
        let start = source
            .find("fn record_tenure_sortition")
            .expect("the tenure context builder exists");
        let body = &source[start..];
        let end = body[1..].find("\n    /// ").map_or(body.len(), |at| at + 1);
        assert!(
            body[..end].contains("record_authentication(bitcoin_context)"),
            "moving an extended view back to its tenure must also restore the tenure winner"
        );
    }
}
