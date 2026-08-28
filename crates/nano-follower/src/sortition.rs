//! The sortitions this node derives for itself.
//!
//! Asking a peer what the sortition was lets that peer choose this node's
//! consensus hashes, its winners and its fork. The arithmetic belongs here, and
//! a captured window of mainnet proves it produces what the network produced:
//! the same operations, operations hash, consensus hash, sortition identifier,
//! sortition hash and running burn total, from the raw Bitcoin blocks and
//! nothing else.
//!
//! A chain that starts at a checkpoint cannot derive a consensus hash from its
//! own snapshots — the hash mixes the ones at power-of-two offsets behind it,
//! reaching back thousands of blocks — so the checkpoint carries those hashes.
//! They are twenty bytes a block: mainnet's whole history is twelve megabytes.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Display,
    fs,
    path::Path,
};

use nano_address::{PoxAddress, PoxAddressType32};
use nano_bitcoin::{BitcoinBlock, BitcoinOperationKind};
use nano_primitives::{BitcoinHeaderHash, ConsensusHash, Hash160};
use nano_sortition::{
    LeaderKeys, MINING_COMMITMENT_WINDOW, PayoutSchedule, PoxId, SortitionEngine, SortitionError,
    SortitionSnapshot, accepted_operation_txids, commitment_is_on_time, commitment_window_block,
    unbroken_pox_id_for,
};
use serde::{Deserialize, Serialize};

/// Reward cycles the seed's `PoX` history is searched for.
///
/// Mainnet has had 142 and gains one every fortnight, so this is decades of
/// slack over a search that costs one hash per cycle.
const POX_HISTORY_SEARCH_LIMIT: usize = 1024;

/// How many burn blocks one round of catching up may walk.
///
/// A catch-up exists to close two gaps and no others: the one between the
/// checkpoint's sortition seed and the first block the node executes — twelve
/// blocks on mainnet, because the seed is the last block the capture's hash
/// history reaches — and the run of burn blocks with no sortition between two
/// tenures, which is a handful. A burn height further off than a day of Bitcoin
/// is not a gap to walk but a tracker seeded on another chain or a peer on one,
/// and walking toward it costs a full Bitcoin block download per step: the
/// unbounded version of this walk once tried to cross 8.6 million of them
/// (commit 2ee576b8). Bounded per round, so a round that runs out still keeps
/// what it derived and the next one carries on.
pub const CATCH_UP_LIMIT: u64 = 144;

/// How far the derived chain may stand ahead of the burn view being executed.
///
/// Ahead is the design: a staged block names its burn view by hash, so the view
/// has to be derived before the block can run, and locating one view can take
/// more than a single [`CATCH_UP_LIMIT`] batch — two in a row without executing
/// anything between them is an ordinary lookahead.
///
/// Unbounded is the defect. The block a round is about to execute is the lowest
/// one staged, so its view sits just above the frontier; a walk that keeps going
/// to Bitcoin's tip every round while execution crawls ends up thousands of
/// blocks ahead, and a chain that derived a different hash back there then says
/// only that it cannot name the view — thousands of blocks after the walk that
/// caused it, and about a height it will never revisit.
///
/// Four batches, so a lookahead has twice the room the deepest one measured
/// needs and a runaway has none.
pub const LEAD_OVER_EXECUTION: u64 = CATCH_UP_LIMIT * 4;

fn execution_rollback_floor(bitcoin_height: u64) -> u64 {
    let rollback = u64::try_from(nano_sortition::MINING_COMMITMENT_WINDOW)
        .expect("the commitment window fits u64");
    bitcoin_height.saturating_sub(rollback)
}

/// What one round of catching up did, and where its time went.
///
/// It is worth counting because the cost of deriving a sortition was attributed
/// wrongly once already. A round's `local` phase looked like 0.11 s per *Stacks*
/// block on mainnet, which would have been a reason to make the arithmetic
/// cheaper; the node's own timing lines show the phase does not grow between one
/// Stacks block and the next at all — it grows only when the burn view moves,
/// which is once a tenure. Sortition is a fact about a Bitcoin block, and many
/// Stacks blocks stand on one, so per-block is the wrong denominator.
///
/// What is left after that division is `reading`: a full Bitcoin block fetched
/// from whatever burnchain the node is configured with, which for a node reading
/// a hosted Esplora is a megabyte or two over the internet. `deriving` is the
/// hashes, and it is the part this node could make cheaper — the numbers say
/// there is nothing there to win.
#[derive(Debug, Default)]
pub struct CatchUp {
    /// Burn blocks snapshotted.
    pub advanced: u64,
    /// Burn blocks read to fill the mining window behind the seed, once a start.
    pub primed: u64,
    /// Waiting on the burnchain for those blocks.
    pub reading: std::time::Duration,
    /// The sortition arithmetic over them.
    pub deriving: std::time::Duration,
}

/// Why a locally derived sortition chain could not be started or advanced.
#[derive(Debug)]
pub enum TrackerError {
    Seed(String),
    Bitcoin(String),
    Sortition(SortitionError),
    BoundarySnapshotMissing(ConsensusHash),
    BoundarySnapshotDuplicate(ConsensusHash),
    BoundaryHistoryMissing(ConsensusHash),
    BoundaryHistoryDuplicate(ConsensusHash),
    BoundaryNotWinner {
        consensus_hash: ConsensusHash,
        bitcoin_height: u64,
    },
    BoundaryBlockMismatch {
        bitcoin_height: u64,
    },
    BoundaryWinnerCommitmentMissing {
        bitcoin_height: u64,
        winner_txid: [u8; 32],
    },
    BoundaryWinnerKeyUnavailable {
        bitcoin_height: u64,
        key_block_height: u64,
        key_transaction_index: u32,
    },
    BoundaryWinnerMismatch {
        bitcoin_height: u64,
        field: &'static str,
    },
}

impl std::fmt::Display for TrackerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Seed(reason) => write!(formatter, "sortition seed: {reason}"),
            Self::Bitcoin(reason) => write!(formatter, "burnchain: {reason}"),
            Self::Sortition(error) => write!(formatter, "sortition: {error:?}"),
            Self::BoundarySnapshotMissing(hash) => {
                write!(
                    formatter,
                    "no captured snapshot names checkpoint boundary {hash}"
                )
            }
            Self::BoundarySnapshotDuplicate(hash) => write!(
                formatter,
                "more than one captured snapshot names checkpoint boundary {hash}"
            ),
            Self::BoundaryHistoryMissing(hash) => write!(
                formatter,
                "checkpoint boundary {hash} is absent from the consensus-hash history"
            ),
            Self::BoundaryHistoryDuplicate(hash) => write!(
                formatter,
                "checkpoint boundary {hash} occurs more than once in the consensus-hash history"
            ),
            Self::BoundaryNotWinner {
                consensus_hash,
                bitcoin_height,
            } => write!(
                formatter,
                "checkpoint boundary {consensus_hash} at burn {bitcoin_height} did not elect a winner"
            ),
            Self::BoundaryBlockMismatch { bitcoin_height } => write!(
                formatter,
                "checkpoint boundary snapshot and Bitcoin block disagree at burn {bitcoin_height}"
            ),
            Self::BoundaryWinnerCommitmentMissing {
                bitcoin_height,
                winner_txid,
            } => write!(
                formatter,
                "checkpoint boundary at burn {bitcoin_height} names winning commitment {}, but that eligible commitment is absent",
                hex::encode(winner_txid)
            ),
            Self::BoundaryWinnerKeyUnavailable {
                bitcoin_height,
                key_block_height,
                key_transaction_index,
            } => write!(
                formatter,
                "checkpoint boundary winner at burn {bitcoin_height} names absent leader key {key_block_height}:{key_transaction_index}"
            ),
            Self::BoundaryWinnerMismatch {
                bitcoin_height,
                field,
            } => write!(
                formatter,
                "checkpoint boundary snapshot and winning commitment disagree on {field} at burn {bitcoin_height}"
            ),
        }
    }
}

impl std::error::Error for TrackerError {}

impl From<SortitionError> for TrackerError {
    fn from(error: SortitionError) -> Self {
        Self::Sortition(error)
    }
}

/// The consensus-hash history a checkpoint carries.
#[derive(Debug, Deserialize, Serialize)]
struct History {
    hashes: Vec<String>,
}

/// A snapshot chain this node advances from its own burnchain.
#[derive(Debug)]
pub struct SortitionTracker {
    engine: SortitionEngine,
    pox_id: PoxId,
    /// Leader keys registered by the burn blocks this tracker has walked.
    ///
    /// A winning commitment names the registration that authorises its VRF
    /// proof, and the proof is what a tenure-start block is checked against. A
    /// node that started at a checkpoint has not seen the older registrations,
    /// so a lookup can miss — which is reported rather than treated as "no check
    /// needed".
    keys: LeaderKeys,
    /// Locally authenticated waterfall recipients, by the first burn height that
    /// carried each one. Captures take these from stacks-core's own `pox_payouts`;
    /// live execution adds the reward set it computes for the next cycle.
    waterfall_payouts: BTreeMap<u64, WaterfallPayout>,
    /// Whether the six burn blocks the distribution weighs over have been read.
    ///
    /// They come from behind the seed, so they are not blocks the chain takes a
    /// snapshot of. Without them the window is short and the winner is not the
    /// one the network picked.
    primed: bool,
    /// Accepted block commitments on the canonical Bitcoin branch since the
    /// beginning of the priming window, by burn position.
    commitments: BTreeSet<(u64, u32)>,
    commitment_floor: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WaterfallPayout {
    observed_at: u64,
    recipient: PoxAddress,
}

/// One newly derived Bitcoin block to publish to event observers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BurnNotification {
    pub bitcoin_block_hash: BitcoinHeaderHash,
    pub bitcoin_height: u64,
    pub consensus_hash: ConsensusHash,
    pub parent_bitcoin_block_hash: BitcoinHeaderHash,
    pub burned: u64,
}

impl SortitionTracker {
    /// Start from a seed snapshot and the consensus hashes behind it.
    pub fn new(seed: SortitionSnapshot, history: Vec<ConsensusHash>) -> Result<Self, TrackerError> {
        let pox_id = seed.pox_id.clone();
        let engine = SortitionEngine::with_history(seed, history).ok_or_else(|| {
            TrackerError::Seed("the history does not end at the snapshot it seeds".to_owned())
        })?;
        Ok(Self {
            engine,
            pox_id,
            keys: LeaderKeys::new(),
            waterfall_payouts: BTreeMap::new(),
            primed: false,
            commitments: BTreeSet::new(),
            commitment_floor: None,
        })
    }

    /// Record the payout address this node computed for a waterfall cycle.
    pub fn record_waterfall_payout(
        &mut self,
        bitcoin_height: u64,
        observed_at: u64,
        recipient: PoxAddress,
    ) {
        self.waterfall_payouts.insert(
            bitcoin_height,
            WaterfallPayout {
                observed_at,
                recipient,
            },
        );
    }

    /// The sBTC address this chain would pay a waterfall commitment at a height.
    ///
    /// The same lookup derivation makes, asked from outside so a caller learning a
    /// cycle's address late can tell whether the chain already derived that cycle
    /// under a different one.
    #[must_use]
    pub fn waterfall_recipient_at(&self, bitcoin_height: u64) -> Option<PoxAddress> {
        self.waterfall_payouts
            .range(..=bitcoin_height)
            .next_back()
            .map(|(_, payout)| payout.recipient)
    }

    fn payouts_at(
        &self,
        payouts: PayoutSchedule,
        bitcoin_height: u64,
    ) -> Result<PayoutSchedule, TrackerError> {
        if !payouts.is_waterfall_at(bitcoin_height) {
            return Ok(payouts);
        }
        let recipient = self
            .waterfall_payouts
            .range(..=bitcoin_height)
            .next_back()
            .map(|(_, payout)| payout.recipient)
            .ok_or_else(|| {
                TrackerError::Seed(format!(
                    "burn {bitcoin_height} uses the PoX waterfall, but the locally derived chain \
                     carries no sBTC payout address for it"
                ))
            })?;
        Ok(payouts.paying_waterfall_to(recipient))
    }

    /// Read the consensus hashes a capture carries, oldest first.
    pub fn history_from(directory: &Path) -> Result<Vec<ConsensusHash>, TrackerError> {
        let bytes = fs::read(directory.join("consensus-hashes.json"))
            .map_err(|error| TrackerError::Seed(error.to_string()))?;
        let history: History = serde_json::from_slice(&bytes)
            .map_err(|error| TrackerError::Seed(error.to_string()))?;
        history
            .hashes
            .iter()
            .map(|hash| {
                let bytes =
                    hex::decode(hash).map_err(|error| TrackerError::Seed(error.to_string()))?;
                <[u8; 20]>::try_from(bytes.as_slice())
                    .map(ConsensusHash::from_bytes)
                    .map_err(|_| TrackerError::Seed("a consensus hash is not 20 bytes".to_owned()))
            })
            .collect()
    }

    /// The snapshot this chain is standing on.
    #[must_use]
    pub fn tip(&self) -> &SortitionSnapshot {
        self.engine.snapshots().tip()
    }

    /// The consensus hash this chain derived for `bitcoin_height`.
    ///
    /// The history is one hash per burn block, ending at the tip, because
    /// `ConsensusHash::from_ops` mixes the hashes behind it at power-of-two offsets
    /// and none may be skipped. So a height maps to an index by subtraction, and
    /// nothing has to be searched or trusted: this is the hash *this node* derived,
    /// which is what makes it usable for naming a reward cycle to a peer.
    #[must_use]
    pub fn consensus_hash_at(&self, bitcoin_height: u64) -> Option<ConsensusHash> {
        let history = self.engine.snapshots().history();
        let back = usize::try_from(self.tip().bitcoin_height.checked_sub(bitcoin_height)?).ok()?;
        history.get(history.len().checked_sub(back + 1)?).copied()
    }

    /// Whether this chain's history holds a burn view, by consensus hash.
    ///
    /// A linear search over the hash history, which on mainnet is a quarter of a
    /// million entries and twelve megabytes — cheap enough for a question asked
    /// once per fork choice, and the reason the answer is not asked per block.
    ///
    /// The history reaches below the seed, where its entries came from the
    /// checkpoint rather than from this node's own arithmetic. That is still this
    /// node's own view and not the peer's, which is the distinction that matters
    /// here: the alternative is having nothing to compare a peer's burnchain
    /// against at all.
    #[must_use]
    pub fn holds_consensus_hash(&self, consensus_hash: ConsensusHash) -> bool {
        self.engine.snapshots().history().contains(&consensus_hash)
    }

    /// The Bitcoin height a burn view sits at, from this chain's own history.
    ///
    /// A consensus hash names a burn block, and the history is that naming in
    /// order, so this is the reverse of [`Self::consensus_hash_at`] and the answer
    /// a node otherwise had to ask a peer for. Searched from the tip backwards
    /// because a follower's views arrive in ascending order and the newest is
    /// almost always the one being asked about.
    ///
    /// Not bounded by [`CATCH_UP_LIMIT`], which it used to be. That bound exists
    /// because a *walk* costs one Bitcoin block download per step; this is a
    /// comparison against bytes already in memory, and the two share nothing but a
    /// direction. Bounded, it stopped naming burn views this node had already
    /// derived and walked through: the tracker runs its tip to Bitcoin's while
    /// execution lags behind it, so after one 500-block batch on mainnet the view
    /// execution stood on was 282 blocks back against a window of 144, and the
    /// follower never executed again — it re-walked the same ground every round
    /// while Bitcoin widened the gap. The whole history is a quarter of a million
    /// twenty-byte entries and the scan is a fraction of a millisecond, against a
    /// burn view that changes once a tenure.
    ///
    /// `None` where the history does not hold it at all: the view is ahead of this
    /// chain, which one round of catching up may close, or it belongs to a burnchain
    /// this node is not on, which no amount of walking will.
    #[must_use]
    pub fn height_of_consensus_hash(&self, consensus_hash: ConsensusHash) -> Option<u64> {
        let history = self.engine.snapshots().history();
        let tip = self.tip().bitcoin_height;
        history
            .iter()
            .rev()
            .position(|hash| *hash == consensus_hash)
            .and_then(|back| tip.checked_sub(u64::try_from(back).ok()?))
    }

    /// Say which burn view execution has reached, so no snapshot above it is
    /// dropped before it is read.
    ///
    /// See [`nano_sortition::SnapshotChain::keep_from`]. The tracker running ahead
    /// of execution is the design; running away from it is the defect.
    pub fn keep_from(&mut self, bitcoin_height: u64) {
        self.engine.snapshots_mut().keep_from(bitcoin_height);
    }

    /// Keep every burn view execution or an admitted Bitcoin reorganization can
    /// still need. A same-sortition Stacks fork needs the immediately preceding
    /// view; the deepest admitted Bitcoin reorganization reaches one commitment
    /// window further back.
    pub fn keep_for_execution(&mut self, bitcoin_height: u64) {
        self.keep_from(execution_rollback_floor(bitcoin_height));
    }

    /// Whether the six burn blocks behind the seed have been read.
    ///
    /// Asked by a caller that wants to answer from the history *without* touching the
    /// burnchain: the seed's own burn spends and the mining window every later winner
    /// is weighed over come out of those blocks, so a chain that has not read them
    /// yet has to, even for a view its history already names.
    #[must_use]
    pub const fn is_primed(&self) -> bool {
        self.primed
    }

    /// The snapshot this chain derived for a Bitcoin height.
    ///
    /// What a block being executed reads, and the reason the chain keeps a window
    /// rather than only its tip: the chain is walked ahead until it *names* the burn
    /// view a staged block stands on, so by the time that block executes the tip may
    /// already be several burn blocks further on. See
    /// [`nano_sortition::SnapshotChain::snapshot_at`].
    #[must_use]
    pub fn snapshot_at(&self, bitcoin_height: u64) -> Option<&SortitionSnapshot> {
        self.engine.snapshots().snapshot_at(bitcoin_height)
    }

    /// The height of a view this chain derived and then dropped from its window.
    ///
    /// Two refusals are indistinguishable from outside and mean opposite things.
    /// "I have not walked to that view yet" is ordinary and clears itself: the next
    /// round walks further. "I derived that view and threw the snapshot away" never
    /// clears, because the retained window closed above it and a chain only walks
    /// forward — the node then repeats one round forever while reporting itself
    /// healthy. Sixteen hours of a mainnet catch-up were lost to exactly that
    /// conflation, so the two are separated here rather than in a log message.
    ///
    /// The consensus-hash history is never front-pruned, so a view this chain
    /// derived is still named there long after its snapshot is gone. That is what
    /// makes the second case recognisable at all.
    #[must_use]
    pub fn window_closed_below(&self, view: ConsensusHash) -> Option<u64> {
        let height = self.height_of_consensus_hash(view)?;
        if self.snapshot_at(height).is_some() {
            return None;
        }
        Some(height)
    }

    /// Return one locally derived snapshot in the shape proposal authentication uses.
    #[must_use]
    pub fn sortition_info_at(&self, bitcoin_height: u64) -> Option<nano_sync::SortitionInfo> {
        let snapshot = self.snapshot_at(bitcoin_height)?;
        let hash_at =
            |height: Option<u64>| height.and_then(|height| self.consensus_hash_at(height));
        Some(nano_sync::SortitionInfo {
            bitcoin_block_hash: snapshot.bitcoin_header_hash,
            bitcoin_height: snapshot.bitcoin_height,
            bitcoin_timestamp: snapshot.bitcoin_timestamp,
            sortition_id: snapshot.sortition_id,
            parent_sortition_id: snapshot.parent_sortition_id,
            consensus_hash: snapshot.consensus_hash,
            was_sortition: snapshot.winner_txid.is_some(),
            miner_public_key_hash: snapshot
                .winner_signing_key_hash
                .map(nano_primitives::Hash160::from_bytes),
            stacks_parent_consensus_hash: hash_at(snapshot.parent_bitcoin_height),
            last_sortition_consensus_hash: hash_at(
                self.previous_sortition_height(snapshot.bitcoin_height),
            ),
            committed_block_hash: snapshot
                .committed_block_hash
                .map(nano_primitives::BlockHeaderHash::from_bytes),
            vrf_seed: snapshot.winner_vrf_seed,
            mining_competition: snapshot.mining_competition.clone(),
        })
    }

    /// The current locally derived burn views and recent winning elections.
    ///
    /// Signers need the current Bitcoin view even when Stacks execution has not
    /// advanced under it yet. Recent consecutive views let diagnostics compare
    /// every derived burn block even when the node catches up between polls. The
    /// previous elections are retained separately because the current view names
    /// one as `last_sortition_consensus_hash`.
    #[must_use]
    pub fn recent_sortitions(&self) -> Vec<nano_sync::SortitionInfo> {
        const KEPT_BURN_VIEWS: u64 = 64;
        const KEPT_ELECTIONS: usize = 12;
        let mut sortitions = Vec::new();

        for distance in 0..KEPT_BURN_VIEWS {
            let Some(height) = self.tip().bitcoin_height.checked_sub(distance) else {
                break;
            };
            let Some(sortition) = self.sortition_info_at(height) else {
                break;
            };
            sortitions.push(sortition);
        }

        let mut elections = sortitions
            .iter()
            .filter(|sortition| sortition.was_sortition)
            .count();
        let mut walk = Some(self.tip().bitcoin_height);
        while elections < KEPT_ELECTIONS {
            let Some(height) = walk.take() else {
                break;
            };
            let Some(sortition) = self.sortition_info_at(height) else {
                break;
            };
            if sortition.was_sortition
                && !sortitions
                    .iter()
                    .any(|retained| retained.bitcoin_height == height)
            {
                sortitions.push(sortition);
                elections += 1;
            }
            walk = self.previous_sortition_height(height);
        }
        sortitions
    }

    /// The two `.miners` writers in their canonical pair order.
    ///
    /// This is stacks-core's `make_miners_stackerdb_config` rule: the newest
    /// winning snapshot occupies pair zero when its cumulative sortition count
    /// is even and pair one when it is odd. No peer answer is involved.
    #[must_use]
    pub fn miner_slot_writers(&self) -> Option<[Hash160; 2]> {
        let newest_height = if self.tip().winner_txid.is_some() {
            self.tip().bitcoin_height
        } else {
            self.previous_sortition_height(self.tip().bitcoin_height)?
        };
        let newest = self.snapshot_at(newest_height)?;
        let previous = self.snapshot_at(self.previous_sortition_height(newest_height)?)?;
        ordered_miner_slot_writers(newest, previous)
    }

    /// Every locally derived Bitcoin block above a previously published height.
    #[must_use]
    pub fn burn_notifications_after(&self, bitcoin_height: u64) -> Vec<BurnNotification> {
        let mut notifications = Vec::new();
        for height in bitcoin_height.saturating_add(1)..=self.tip().bitcoin_height {
            let Some(parent) = self.snapshot_at(height.saturating_sub(1)) else {
                break;
            };
            let Some(snapshot) = self.snapshot_at(height) else {
                break;
            };
            notifications.push(BurnNotification {
                bitcoin_block_hash: snapshot.bitcoin_header_hash,
                bitcoin_height: height,
                consensus_hash: snapshot.consensus_hash,
                parent_bitcoin_block_hash: parent.bitcoin_header_hash,
                burned: snapshot.total_burn.saturating_sub(parent.total_burn),
            });
        }
        notifications
    }

    /// The last burn height below this one that elected somebody.
    ///
    /// A tenure collects the coinbase of every burn block since that height, so this
    /// is what makes a tenure-start block's reward derivable — and the answer is
    /// *minted*, which is why `None` means "this chain cannot say" and must not be
    /// read as "there was none": that would mint zero and seal a root nobody else
    /// computes. Two peer requests per tenure-start block before this existed.
    #[must_use]
    pub fn previous_sortition_height(&self, bitcoin_height: u64) -> Option<u64> {
        let parent = bitcoin_height.checked_sub(1)?;
        self.engine.snapshots().last_sortition_at_or_below(parent)
    }

    /// Where this chain and Bitcoin's own history part company, if they do.
    ///
    /// One lookup when nothing moved: the walk stops at the first agreement and
    /// starts at the tip. `nano_bitcoin::BitcoinSource::block_hash_at` is what a
    /// node passes.
    pub fn find_fork<E>(
        &self,
        canonical_hash: impl FnMut(u64) -> Result<[u8; 32], E>,
    ) -> Result<nano_sortition::Fork, E> {
        self.engine.snapshots().find_fork(canonical_hash)
    }

    /// Give back every sortition above a Bitcoin height.
    ///
    /// Refused when the reorganization is deeper than the commitment window this
    /// chain retained, because the replacement branch's first sortition would then
    /// be weighed over fewer blocks than the network weighed it over — a short
    /// window is a different answer, not a rougher one.
    pub fn retract_above(
        &mut self,
        bitcoin_height: u64,
    ) -> Result<nano_sortition::SortitionReorg, TrackerError> {
        let reorg = self.engine.retract_above(bitcoin_height)?;
        self.commitments
            .retain(|(height, _)| *height <= bitcoin_height);
        Ok(reorg)
    }

    /// Commitments the burn block at the tip put up for its sortition.
    ///
    /// Reporting, not a gate: the winner derives whether or not the block left a
    /// choice. See [`SortitionEngine::candidates`].
    #[must_use]
    pub fn candidates(&self) -> usize {
        self.engine.candidates()
    }

    /// Extend the chain with one Bitcoin block, deriving everything about it.
    ///
    /// The running burn total is derived here rather than taken from a Nakamoto
    /// header: it is the burn distribution's total over the six-block mining
    /// window, which is why the sum of what a block's commitments paid out is
    /// not it — a block whose sortition went to the null miner adds nothing at
    /// all, as mainnet's burn 960,222 does. [`Self::agrees_with_header`] is what
    /// checks the derivation against threshold signer weight.
    pub fn advance(
        &mut self,
        block: &BitcoinBlock,
        payouts: PayoutSchedule,
    ) -> Result<&SortitionSnapshot, TrackerError> {
        if self.engine.snapshots().effective_winner_seed().is_none() {
            return Err(TrackerError::Seed(format!(
                "the chain at burn {} has no effective winner seed, so burn {} cannot be \
                 sampled without substituting an all-zero seed",
                self.tip().bitcoin_height,
                block.height
            )));
        }
        let payouts = self.payouts_at(payouts, block.height)?;
        // A reward cycle opening adds a bit to the `PoX` history, and the consensus
        // hash mixes that history, so getting this wrong derives a wrong hash for
        // every block after it and reports nothing.
        //
        // In Nakamoto the bit is one. Not because every history seen so far happens
        // to be all ones -- mainnet's 142, hacknet's 21, this repository's capture
        // going 20, 21, 22, 23, 24, 25 across its five boundaries -- but because
        // epoch 3.0 onwards has no code path that writes a zero.
        // `load_nakamoto_reward_set` builds exactly one status,
        // `PoxAnchorBlockStatus::SelectedAndKnown`
        // (`nakamoto/coordinator/mod.rs:543`), so `is_reward_info_known` is
        // unconditionally true and `make_next_pox_id` unconditionally calls
        // `extend_with_present_block`. Its own comment says why: "In Nakamoto, every
        // reward cycle _must_ have a PoX anchor block; otherwise, the chain halts."
        // The alternative that does exist there is `Ok(None)` -- the anchor is not
        // processed *yet* -- and that is a wait, not a zero.
        //
        // `NotSelected` and `SelectedAndUnknown` are reachable only through the
        // epoch-2.x path and the first cycle of epoch 3.0, which a node that starts
        // at or after the 4.0 boundary can never be asked about.
        //
        // So this is the epoch-4.0 rule rather than a guess, and it is checked
        // rather than asserted: `pox_boundary` derives forward across the capture's
        // five boundaries and compares every sortition identifier and consensus hash
        // with what stacks-core wrote. A wrong bit changes the identifier at the
        // first boundary and every block after it.
        if payouts.starts_reward_cycle(block.height) {
            self.pox_id.extend_with_anchor(true);
        }
        self.register_keys(block);
        let (block, commitments) = self.accepted_block(block, payouts);
        let txids = accepted_operation_txids(&block, payouts);
        Ok(self.engine.append(
            &block,
            &txids,
            commitments,
            self.pox_id.clone(),
            payouts.mining_window_at(block.height),
        )?)
    }

    /// Walk the burnchain until the chain stands on `target`, or the bound runs
    /// out.
    ///
    /// Nothing is skipped: every burn block between here and there is read and
    /// snapshotted, because a consensus hash mixes the ones behind it and a
    /// height left out changes every hash from there on.
    ///
    /// The two costs are counted apart because they are not the same kind of
    /// thing and only one of them is this node's to make cheaper: reading a burn
    /// block is a Bitcoin block download, and the arithmetic over it is hashes.
    pub fn catch_up<E: Display>(
        &mut self,
        mut block_at: impl FnMut(u64) -> Result<BitcoinBlock, E>,
        target: u64,
        payouts: PayoutSchedule,
        limit: u64,
    ) -> Result<CatchUp, TrackerError> {
        let mut walk = CatchUp::default();
        if !self.primed {
            self.prime(&mut block_at, payouts, &mut walk)?;
        }
        self.walk(&mut block_at, payouts, limit, &mut walk, |tip| {
            tip.bitcoin_height >= target
        })?;
        Ok(walk)
    }

    /// The burn height a consensus hash names, walking the burnchain to find out.
    ///
    /// This is what breaks the circle the production path stood in. The height of a
    /// burn view used to come from a peer, because the only thing that advanced this
    /// chain was the block being executed — so the chain's tip was always *exactly*
    /// the view being executed, a view arriving for the first time was always at
    /// least one block ahead of the history, and [`Self::height_of_consensus_hash`]
    /// could never answer for it.
    ///
    /// So the chain is walked forward instead, one Bitcoin block at a time, until one
    /// of them derives the hash asked about. Nothing is skipped and nothing is
    /// searched: a consensus hash mixes the ones behind it, so the block that derives
    /// it is the block it names. Two bounds, and they mean different things —
    /// `burnchain_tip` is where this node's own Bitcoin source ends, so stopping
    /// there says "that view is not on my burnchain yet" rather than reading past the
    /// end of a chain; `limit` is what one round may spend, since each step is a full
    /// Bitcoin block download.
    ///
    /// `None` is therefore not "the peer is lying": it is one of three things, all of
    /// which the caller reports rather than works around — the burnchain has not
    /// reached that block, the round ran out of walk, or the view belongs to a
    /// different chain of Bitcoin blocks than the one this node reads.
    pub fn locate_view<E: Display>(
        &mut self,
        view: ConsensusHash,
        mut block_at: impl FnMut(u64) -> Result<BitcoinBlock, E>,
        burnchain_tip: u64,
        payouts: PayoutSchedule,
        limit: u64,
    ) -> Result<(Option<u64>, CatchUp), TrackerError> {
        let mut walk = CatchUp::default();
        if !self.primed {
            self.prime(&mut block_at, payouts, &mut walk)?;
        }
        if let Some(height) = self.height_of_consensus_hash(view) {
            return Ok((Some(height), walk));
        }
        let room = burnchain_tip.saturating_sub(self.tip().bitcoin_height);
        // Walked to where Bitcoin ends rather than stopped at the view asked for.
        // Every burn block above it elects a tenure this node is about to want, so
        // the downloads are not extra work brought forward but the same work done
        // in one round instead of one per round — and stopping at the first match
        // is what held a catching-up node to a single sortition a round, which
        // bounded it to a handful of executed blocks while hundreds sat staged.
        //
        // The walk cannot outrun the lookup behind it: it advances at most `limit`
        // blocks and `height_of_consensus_hash` looks back over the same window, so
        // a view found on the way is still addressable when the walk stops.
        self.walk(&mut block_at, payouts, limit.min(room), &mut walk, |_| {
            false
        })?;
        let found = self.height_of_consensus_hash(view);
        Ok((found, walk))
    }

    /// Walk toward Bitcoin's tip because Bitcoin moved, not because a block asked.
    ///
    /// Every other walk here is driven by execution: the chain is advanced to name
    /// the burn view a staged block stands on. A node at the chain tip with nothing
    /// staged therefore never advances at all — so it holds no snapshot for its own
    /// tip's burn view, and `/v3/sortitions` answers `503` on a node that is
    /// perfectly healthy and simply idle. That is the condition a signer spends most
    /// of its time in, and a burn block is news to one whether or not a Stacks block
    /// stands on it yet.
    pub fn follow_burnchain<E: Display>(
        &mut self,
        mut block_at: impl FnMut(u64) -> Result<BitcoinBlock, E>,
        burnchain_tip: u64,
        payouts: PayoutSchedule,
        limit: u64,
    ) -> Result<CatchUp, TrackerError> {
        let mut walk = CatchUp::default();
        if !self.primed {
            self.prime(&mut block_at, payouts, &mut walk)?;
        }
        let room = burnchain_tip.saturating_sub(self.tip().bitcoin_height);
        self.walk(&mut block_at, payouts, limit.min(room), &mut walk, |_| {
            false
        })?;
        Ok(walk)
    }

    /// Read burn blocks and derive their sortitions until `done`, or the bound runs
    /// out.
    ///
    /// `done` is asked about the tip before anything is read, so a chain already
    /// standing where it needs to be costs nothing — which is why deriving sortitions
    /// is not a per-Stacks-block cost: many Stacks blocks stand on one burn block and
    /// only the first of them walks.
    fn walk<E: Display>(
        &mut self,
        block_at: &mut impl FnMut(u64) -> Result<BitcoinBlock, E>,
        payouts: PayoutSchedule,
        limit: u64,
        walk: &mut CatchUp,
        mut done: impl FnMut(&SortitionSnapshot) -> bool,
    ) -> Result<(), TrackerError> {
        while !done(self.tip()) && walk.advanced < limit {
            let height = self
                .tip()
                .bitcoin_height
                .checked_add(1)
                .ok_or(SortitionError::HeightOverflow)?;
            let read = std::time::Instant::now();
            let block =
                block_at(height).map_err(|error| TrackerError::Bitcoin(error.to_string()))?;
            walk.reading += read.elapsed();
            let derive = std::time::Instant::now();
            self.advance(&block, payouts)?;
            walk.deriving += derive.elapsed();
            walk.advanced += 1;
        }
        Ok(())
    }

    /// Read the mining window behind the seed, which the seed itself is not in.
    ///
    /// A short window is not a smaller version of the right answer: it computes
    /// each candidate's median burn over fewer blocks than the network did, and
    /// so picks a different winner. Priming with seven blocks instead of six is
    /// what turned mainnet's sortition at burn 960,226 into no sortition at all.
    fn prime<E: Display>(
        &mut self,
        block_at: &mut impl FnMut(u64) -> Result<BitcoinBlock, E>,
        payouts: PayoutSchedule,
        walk: &mut CatchUp,
    ) -> Result<(), TrackerError> {
        let tip = self.tip().bitcoin_height;
        let behind = u64::try_from(MINING_COMMITMENT_WINDOW).expect("window fits u64") - 1;
        for height in tip.saturating_sub(behind)..=tip {
            let read = std::time::Instant::now();
            let block =
                block_at(height).map_err(|error| TrackerError::Bitcoin(error.to_string()))?;
            walk.reading += read.elapsed();
            walk.primed += 1;
            self.register_keys(&block);
            let payouts = self.payouts_at(payouts, height)?;
            let (block, commitments) = self.accepted_block(&block, payouts);
            if height == tip {
                self.recover_seed_from(&block)?;
            }
            self.engine.prime(height, commitments);
        }
        self.primed = true;
        Ok(())
    }

    /// Recover and adopt the checkpoint seed before the node starts following.
    ///
    /// A chain this node saved already carries the effective seed and winning key,
    /// and costs no Bitcoin read. A capture missing either reads its seed block and
    /// recovers the winning commitment. Every other case is refused here, before a
    /// caller can persist or execute against the tracker.
    pub fn recover_seed<E: Display>(
        &mut self,
        mut block_at: impl FnMut(u64) -> Result<BitcoinBlock, E>,
    ) -> Result<(), TrackerError> {
        let seed_is_complete = self.engine.snapshots().effective_winner_seed().is_some()
            && (self.tip().winner_txid.is_none() || self.tip().winner_vrf_public_key.is_some());
        if seed_is_complete {
            return Ok(());
        }
        let height = self.tip().bitcoin_height;
        let block = block_at(height).map_err(|error| TrackerError::Bitcoin(error.to_string()))?;
        self.recover_seed_from(&block)
    }

    /// Resolve the captured boundary winner through its Bitcoin commitment and
    /// the checkpoint's leader-key registry.
    ///
    /// A boundary is the root of this tracker, so its winner was stated by the
    /// capture rather than derived by the engine. Reading the named commitment
    /// again is what keeps the VRF key used for the boundary proof local.
    pub fn authenticate_boundary_winner(
        &self,
        block: &BitcoinBlock,
    ) -> Result<[u8; 32], TrackerError> {
        let boundary = self.tip();
        if block.height != boundary.bitcoin_height
            || block.hash != *boundary.bitcoin_header_hash.as_bytes()
        {
            return Err(TrackerError::BoundaryBlockMismatch {
                bitcoin_height: boundary.bitcoin_height,
            });
        }
        let winner_txid = boundary
            .winner_txid
            .ok_or(TrackerError::BoundaryNotWinner {
                consensus_hash: boundary.consensus_hash,
                bitcoin_height: boundary.bitcoin_height,
            })?;
        let operation = block.operations.iter().find(|operation| {
            operation.txid == winner_txid
                && matches!(
                    &operation.kind,
                    BitcoinOperationKind::LeaderBlockCommit { parent_modulus, .. }
                        if commitment_is_on_time(*parent_modulus, block.height)
                )
        });
        let Some(operation) = operation else {
            return Err(TrackerError::BoundaryWinnerCommitmentMissing {
                bitcoin_height: boundary.bitcoin_height,
                winner_txid,
            });
        };
        let BitcoinOperationKind::LeaderBlockCommit {
            block_header_hash,
            new_seed,
            key_block_height,
            key_transaction_index,
            ..
        } = &operation.kind
        else {
            unreachable!("the winner search selected a leader commitment");
        };
        if boundary.winner_vrf_seed != Some(*new_seed) {
            return Err(TrackerError::BoundaryWinnerMismatch {
                bitcoin_height: boundary.bitcoin_height,
                field: "the effective VRF seed",
            });
        }
        if boundary
            .committed_block_hash
            .is_some_and(|hash| hash != *block_header_hash)
        {
            return Err(TrackerError::BoundaryWinnerMismatch {
                bitcoin_height: boundary.bitcoin_height,
                field: "the committed Stacks block",
            });
        }
        let key_height = u64::from(*key_block_height);
        let key_index = u32::from(*key_transaction_index);
        let registration = self.keys.registration(key_height, key_index).ok_or(
            TrackerError::BoundaryWinnerKeyUnavailable {
                bitcoin_height: boundary.bitcoin_height,
                key_block_height: key_height,
                key_transaction_index: key_index,
            },
        )?;
        if boundary
            .winner_signing_key_hash
            .is_some_and(|hash| registration.signing_key_hash != Some(hash))
        {
            return Err(TrackerError::BoundaryWinnerMismatch {
                bitcoin_height: boundary.bitcoin_height,
                field: "the registered block-signing key",
            });
        }
        Ok(registration.vrf_public_key)
    }

    fn recover_seed_from(&mut self, block: &BitcoinBlock) -> Result<(), TrackerError> {
        let tip = self.tip();
        let seed_was_present = self.engine.snapshots().effective_winner_seed().is_some();
        let winner_key_is_missing =
            tip.winner_txid.is_some() && tip.winner_vrf_public_key.is_none();
        if seed_was_present && !winner_key_is_missing {
            return Ok(());
        }
        if block.height != tip.bitcoin_height || block.hash != *tip.bitcoin_header_hash.as_bytes() {
            return Err(TrackerError::Seed(format!(
                "the seed snapshot names burn {} with header {}, but its Bitcoin source \
                 returned burn {} with header {}",
                tip.bitcoin_height,
                tip.bitcoin_header_hash,
                block.height,
                hex::encode(block.hash)
            )));
        }
        if !seed_was_present {
            let winner = tip.winner_txid.ok_or_else(|| {
                TrackerError::Seed(format!(
                    "the sortition seed at burn {} carries no effective winner seed and names no \
                     winning commitment",
                    tip.bitcoin_height
                ))
            })?;
            // Refused rather than reported. Sampling the next sortition against a zero
            // seed names miners that did not win, and the only sign of it is their
            // tenures' coinbase proofs being refused hundreds of blocks later.
            let seed = winner_seed(block, winner).ok_or_else(|| {
                TrackerError::Seed(format!(
                    "the sortition seed at burn {} says commitment {} won, and neither that \
                     eligible commitment nor an agreement between the block's eligible ones \
                     says which VRF seed it carried -- so the seed the next sortition mixes \
                     cannot be recovered. A checkpoint has to carry `winner_vrf_seed` for a \
                     seed row that elected somebody.",
                    tip.bitcoin_height,
                    hex::encode(winner)
                ))
            })?;
            if !self.engine.adopt_root_winner_seed(seed) {
                return Err(TrackerError::Seed(
                    "the checkpoint seed can only be adopted before the chain advances".to_owned(),
                ));
            }
        }
        // Fresh checkpoint construction authenticates its boundary separately.
        // This repairs saved roots written before winner keys were persisted.
        if seed_was_present
            && self.tip().winner_txid.is_some()
            && self.tip().winner_vrf_public_key.is_none()
        {
            let key = self.authenticate_boundary_winner(block)?;
            if !self.engine.adopt_root_winner_vrf_public_key(key) {
                return Err(TrackerError::Seed(
                    "the checkpoint winner key can only be adopted before the chain advances"
                        .to_owned(),
                ));
            }
        }
        Ok(())
    }

    /// Record the leader keys a burn block registers.
    ///
    /// Before its commitments are read, because a commitment in this very block
    /// may name a key this block registers and a lookup that ran first would
    /// miss it.
    fn register_keys(&mut self, block: &BitcoinBlock) {
        for operation in &block.operations {
            if let BitcoinOperationKind::LeaderKeyRegistration {
                vrf_public_key,
                block_signing_key_hash,
                ..
            } = &operation.kind
            {
                self.keys.register(
                    block.height,
                    operation.transaction_index,
                    nano_sortition::LeaderKeyRegistration {
                        vrf_public_key: *vrf_public_key,
                        signing_key_hash: *block_signing_key_hash,
                    },
                );
            }
        }
    }

    /// Apply the stateful checks a decoded block commitment cannot answer alone.
    ///
    /// In particular, its parent burn position must name an accepted commitment
    /// on this Bitcoin fork. A one-block Bitcoin reorganization can leave miners
    /// pointing at an operation from the abandoned block; stacks-core rejects that
    /// operation with `BlockCommitNoParent`, so it contributes neither a candidate
    /// nor a transaction to the operations hash.
    fn accepted_block(
        &mut self,
        block: &BitcoinBlock,
        payouts: PayoutSchedule,
    ) -> (BitcoinBlock, nano_sortition::CommitmentWindowBlock) {
        let floor = *self.commitment_floor.get_or_insert(block.height);
        let mut accepted = block.clone();
        accepted.operations.retain(|operation| {
            let BitcoinOperationKind::LeaderBlockCommit {
                parent_block_height,
                parent_transaction_index,
                key_block_height,
                key_transaction_index,
                ..
            } = operation.kind
            else {
                return true;
            };
            let parent_height = u64::from(parent_block_height);
            let parent_index = u32::from(parent_transaction_index);
            let parent_exists = (parent_height < block.height || parent_height == 0)
                && parent_index == 0
                || parent_height < floor
                || self.commitments.contains(&(parent_height, parent_index));
            let key_exists = self
                .keys
                .registration(
                    u64::from(key_block_height),
                    u32::from(key_transaction_index),
                )
                .is_some();
            parent_exists && key_exists
        });

        let commitments = commitment_window_block(&accepted, payouts, &self.keys);
        let accepted_txids = commitments
            .commitments
            .iter()
            .map(|commitment| commitment.txid)
            .chain(
                commitments
                    .missed_commitments
                    .iter()
                    .map(|commitment| commitment.txid),
            )
            .collect::<BTreeSet<_>>();
        self.commitments.extend(
            accepted
                .operations
                .iter()
                .filter(|operation| accepted_txids.contains(&operation.txid))
                .map(|operation| (block.height, operation.transaction_index)),
        );
        (accepted, commitments)
    }

    /// Take on burn heights known to have elected somebody, from outside this chain.
    ///
    /// The executed ledger is the source: a tenure exists only because a sortition
    /// chose its miner, so every executed tenure's burn block elected one. That is
    /// how a chain resumed at its own tip regains the ability to answer for burn
    /// blocks below it, which its snapshots no longer reach and which staged blocks
    /// still stand on.
    pub fn remember_elected_heights(&mut self, heights: Vec<u64>) {
        let mut known = self.engine.snapshots().sortitions_below_window().to_vec();
        known.extend(heights);
        self.engine
            .snapshots_mut()
            .seed_sortitions_below_window(known);
    }

    /// How many leader-key registrations this chain can resolve a winner through.
    #[must_use]
    pub fn leader_keys(&self) -> usize {
        self.keys.available()
    }

    /// Block-signing key hashes registered by the locally derived burnchain.
    #[must_use]
    pub fn registered_signing_key_hashes(&self) -> Vec<nano_primitives::Hash160> {
        self.keys
            .entries()
            .filter_map(|(_, _, registration)| registration.signing_key_hash)
            .map(nano_primitives::Hash160::from_bytes)
            .collect()
    }

    /// Take on the leader-key registry a checkpoint carries.
    ///
    /// A winning commitment names the registration that authorises its VRF proof
    /// by burn position, and that position is tens of thousands of blocks below
    /// any window a follower holds — so without this the proof of every tenure is
    /// reported as uncheckable rather than checked. Answering it out of the
    /// registry is what makes the checkpoint, rather than the peer that supplied
    /// the block, the source of a validation input.
    ///
    /// An absent file is not an error here: it is a capture or a state directory
    /// written before the registry existed, and the caller says so out loud
    /// instead. A malformed one *is* — a registry that half-loaded would resolve
    /// some winners and silently not others.
    pub fn load_leader_keys(&mut self, directory: &Path) -> Result<usize, TrackerError> {
        let keys = read_leader_keys(directory)?;
        let loaded = keys.available();
        for (height, index, registration) in keys.entries() {
            self.keys.register(height, index, registration);
        }
        Ok(loaded)
    }
}

/// What this chain says about a tip a peer is offering, for the fork choice.
///
/// The placement rule is the header's own cumulative burn. It is a *running*
/// total over the burn view, so a header stating strictly less burn than this
/// chain's tip was built on a burn block below the ones this chain derived — and
/// every burn view below the tip is in the history, so its consensus hash has to
/// be there. Equality is deliberately not judged: a burn block that elects nobody
/// adds nothing to the total, so an honest peer one view *ahead* of this chain can
/// state exactly this chain's total, and refusing that would refuse the peer that
/// is right.
impl nano_sync::BurnView for SortitionTracker {
    fn derived(&self, consensus_hash: ConsensusHash, bitcoin_spent: u64) -> Option<bool> {
        (bitcoin_spent < self.tip().total_burn).then(|| self.holds_consensus_hash(consensus_hash))
    }

    fn height_of(&self, consensus_hash: ConsensusHash) -> Option<u64> {
        self.height_of_consensus_hash(consensus_hash)
    }
}

/// The seed every eligible commitment in a burn block carries, when they agree.
///
/// In Nakamoto a commitment's `new_seed` is the hash of the parent tenure's
/// coinbase proof, which every miner can compute, so all the candidates in one
/// burn block carry the same one — mainnet's burn 960,230 has five commitments
/// naming five different leader keys and one seed between them. That is what
/// lets a checkpoint's seed snapshot recover the winning seed it does not record,
/// which the sampling of the block after it reads. Candidates that disagree give
/// nothing: there is no telling which of them won.
/// The VRF seed the block's *winning* commitment carried.
///
/// A capture's seed row names the winner by txid, and that commitment's own
/// `new_seed` is the seed the next sortition mixes — exact, and needing no
/// agreement between candidates to read. The mainnet capture this repository
/// carries has a seed block whose eligible commitments disagree, so requiring
/// unanimity gave up on a question the row had already answered.
///
/// Unanimity stays as the fallback for the block whose winning transaction this
/// node did not decode as an on-time commitment: see [`unanimous_winner_seed`].
fn winner_seed(block: &BitcoinBlock, winner_txid: [u8; 32]) -> Option<[u8; 32]> {
    block
        .operations
        .iter()
        .find_map(|operation| match &operation.kind {
            BitcoinOperationKind::LeaderBlockCommit {
                new_seed,
                parent_modulus,
                ..
            } if operation.txid == winner_txid
                && commitment_is_on_time(*parent_modulus, block.height) =>
            {
                Some(*new_seed)
            }
            _ => None,
        })
        .or_else(|| unanimous_winner_seed(block))
}

fn unanimous_winner_seed(block: &BitcoinBlock) -> Option<[u8; 32]> {
    let mut seeds = block
        .operations
        .iter()
        .filter_map(|operation| match &operation.kind {
            BitcoinOperationKind::LeaderBlockCommit {
                new_seed,
                parent_modulus,
                ..
            } if commitment_is_on_time(*parent_modulus, block.height) => Some(*new_seed),
            _ => None,
        });
    let first = seeds.next()?;
    seeds.all(|seed| seed == first).then_some(first)
}

/// Where a checkpoint's leader-key registry is written down.
///
/// Beside the snapshots and the consensus hashes, because it answers the same
/// kind of question they do: what the burnchain below this node's window said.
pub const LEADER_KEY_FILE: &str = "leader-keys.json";
const WATERFALL_PAYOUT_FILE: &str = "waterfall-payouts.json";

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct CapturedWaterfallPayout {
    bitcoin_height: u64,
    observed_at: u64,
    mainnet: bool,
    bytes: [u8; 32],
}

/// Read the leader-key registry a checkpoint carries.
///
/// An absent registry is an empty one. A malformed registry is an error rather
/// than a partial answer.
pub fn read_leader_keys(directory: &Path) -> Result<LeaderKeys, TrackerError> {
    let path = directory.join(LEADER_KEY_FILE);
    let Ok(bytes) = fs::read(&path) else {
        return Ok(LeaderKeys::new());
    };
    let records: Vec<CapturedLeaderKey> = serde_json::from_slice(&bytes)
        .map_err(|error| TrackerError::Seed(format!("{}: {error}", path.display())))?;
    let mut keys = LeaderKeys::new();
    for record in records {
        keys.register(record.block_height, record.vtxindex, record.registration()?);
    }
    Ok(keys)
}

/// One leader-key registration, as a checkpoint carries it.
///
/// The column names are stacks-core's own (`leader_keys`: `block_height`,
/// `vtxindex`, `public_key`, `memo`), so an export is a copy of the archive's
/// rows rather than a translation of them — and a translation is where a wrong
/// field would hide.
#[derive(Debug, Deserialize, Serialize)]
struct CapturedLeaderKey {
    block_height: u64,
    vtxindex: u32,
    public_key: String,
    /// The 20-byte block-signing key hash, when the registration carries one.
    ///
    /// Empty for every registration from before Nakamoto, which is most of
    /// mainnet's: they authorise a VRF key and nothing else.
    #[serde(default)]
    memo: String,
}

impl CapturedLeaderKey {
    fn registration(&self) -> Result<nano_sortition::LeaderKeyRegistration, TrackerError> {
        let bytes = hex::decode(&self.public_key)
            .map_err(|_| TrackerError::Seed("a leader key is not hexadecimal".to_owned()))?;
        let vrf_public_key = <[u8; 32]>::try_from(bytes.as_slice())
            .map_err(|_| TrackerError::Seed("a leader key is not 32 bytes".to_owned()))?;
        let signing_key_hash = if self.memo.is_empty() {
            None
        } else {
            let bytes = hex::decode(&self.memo).map_err(|_| {
                TrackerError::Seed("a block-signing key hash is not hexadecimal".to_owned())
            })?;
            Some(<[u8; 20]>::try_from(bytes.as_slice()).map_err(|_| {
                TrackerError::Seed("a block-signing key hash is not 20 bytes".to_owned())
            })?)
        };
        Ok(nano_sortition::LeaderKeyRegistration {
            vrf_public_key,
            signing_key_hash,
        })
    }
}

/// A snapshot a capture holds, in the fields a seed needs.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct CapturedSnapshot {
    block_height: u64,
    burn_header_hash: String,
    /// The burn block's header time, as stacks-core's own column spells it.
    ///
    /// Clarity reads it back as `burn-block-time`, so a chain seeded here has to
    /// state it for its seed the way it states the seed's burn total: the tenure
    /// standing on the seed's own burn view is executed before this chain has
    /// advanced once. Absent in a chain saved before this field existed, and then
    /// the seed's own answer is unavailable rather than wrong — every block after
    /// it derives from the Bitcoin header.
    #[serde(default)]
    burn_header_timestamp: u64,
    sortition_id: String,
    consensus_hash: String,
    sortition_hash: String,
    total_burn: String,
    /// stacks-core's expected `PoX` recipients and amount per output.
    #[serde(default)]
    pox_payouts: Option<String>,
    /// Stacks-core's cumulative winning-sortition count.
    #[serde(default)]
    num_sortitions: Option<u64>,
    /// Whether this burn block elected anybody, as stacks-core's own column
    /// spells it. Absent in a chain saved before this field existed.
    #[serde(default)]
    sortition: Option<i64>,
    /// The txid of the commitment that won, as stacks-core's own column spells it.
    ///
    /// Read for one reason: the two Clarity-visible burn spends of a sortition are
    /// the eligible commitments' payout burn and the *winner's* share of it, so the
    /// seed has to name its winner or a node standing on the seed's own burn view —
    /// which is every node for the first tenure after a restart — has half the
    /// answer and offers a contract a winner's spend of zero.
    ///
    /// A capture writes all zeroes for a block that elected nobody, so the
    /// `sortition` column above is what discriminates rather than the value.
    #[serde(default)]
    winning_block_txid: Option<String>,
    /// The winning VRF seed the next sampling has to mix — the most recent
    /// winner's, not necessarily this block's.
    ///
    /// A chain saved at a burn block that elected nobody cannot recover it: the
    /// commitments of such a block carry the seed of the tenure they were bidding
    /// for, and adopting that samples the next sortition against a seed nobody
    /// won. See [`nano_sortition::SnapshotChain::effective_winner_seed`].
    #[serde(default)]
    winner_vrf_seed: Option<String>,
    /// The last burn height at or below this one that elected somebody.
    ///
    /// nano's own field rather than one of the archive's columns, and it exists for
    /// the same reason `winner_vrf_seed` does: a chain resumed at a burn block that
    /// elected nobody holds no snapshot with a winner in it, so it cannot walk back
    /// to the height a tenure's accumulated coinbase is measured from. That coinbase
    /// is minted, so the alternative to writing this down is minting a guess.
    ///
    /// Absent in a chain saved before this existed, and then the first tenure on the
    /// seed's own burn view cannot have its reward derived — which is refused rather
    /// than guessed, and the chain re-derived from the checkpoint.
    #[serde(default)]
    last_sortition_height: Option<u64>,
    /// Every burn height below the retained window that elected somebody,
    /// ascending.
    ///
    /// The single height above answers for everything at or above itself and for
    /// nothing below it, which is the case a resumed chain lands in: it is seeded at
    /// the burn block its history ends at and holds no snapshot lower, while
    /// execution is still working through staged blocks standing on earlier burn
    /// views. Measured on a live mainnet follower -- seeded at burn 961,342, asked
    /// about 961,320, and it could only say "this chain cannot say", which stops
    /// execution rather than minting a guess.
    ///
    /// Absent from a state written before this existed, which then falls back to the
    /// single height and is no worse than it was.
    #[serde(default)]
    sortitions_below_window: Vec<u64>,
    /// The archive's own three facts about the seed's winning commitment.
    ///
    /// A seed row states them and a derived snapshot takes them off the commitment
    /// it weighed, so only a chain standing on its *seed* needs these — which is
    /// every chain until it derives its first block, and every rig whose chain is
    /// not currently electing anybody. Left out, `/v3/sortitions` answers with the
    /// winner's key, the block it committed to and the parent identifier all null,
    /// and a stock signer will not initialise against that.
    #[serde(default)]
    parent_sortition_id: Option<String>,
    #[serde(default)]
    miner_pk_hash: Option<String>,
    #[serde(default)]
    winning_stacks_block_hash: Option<String>,
}

impl SortitionTracker {
    /// Start from a capture directory, at the snapshot for `burn_height`.
    ///
    /// The `PoX` history is taken from the seed's own sortition identifier,
    /// which is the burn header hash and that bit vector hashed together — so
    /// the capture states it rather than a node guessing.
    /// Resume from a state directory this tracker saved itself into, or start
    /// from the capture the checkpoint came with.
    ///
    /// Deriving a sortition costs a Bitcoin block fetch, and the run from a
    /// checkpoint's anchor to the burn tip grows for as long as the chain does —
    /// so a node that re-derived on every start spent minutes before executing
    /// anything, and would spend longer every week. The derivation is what has to
    /// be local; keeping the answer is free.
    ///
    /// The saved form is the capture's own, so this is the same loader either way
    /// and a saved chain cannot be read more loosely than a captured one.
    pub fn resume_or_capture(state: &Path, capture: &Path) -> Result<Self, TrackerError> {
        Self::resume_or_capture_below(state, capture, u64::MAX)
    }

    /// The same, refusing a saved chain seeded *above* the burn view execution needs.
    ///
    /// A chain only walks forward, so one seeded above the executed tip's burn view
    /// can never answer for it: every staged block standing lower finds no snapshot
    /// and the node stops. A live mainnet state was left exactly there -- executed
    /// tip needing burn 961,447, saved chain ending at 961,450 -- and no restart
    /// could get out of it, because the thing that could not be used was also the
    /// thing being resumed from.
    ///
    /// So the saved chain is checked against what execution needs before it is
    /// adopted, and the capture is used instead when it is too far ahead. That costs
    /// a re-derivation, which is slow and correct.
    pub fn resume_or_capture_below(
        state: &Path,
        capture: &Path,
        executed_burn_view: u64,
    ) -> Result<Self, TrackerError> {
        let saved = Self::from_capture(state).and_then(|tracker| {
            let seeded_at = tracker.tip().bitcoin_height;
            if seeded_at > executed_burn_view {
                return Err(TrackerError::Seed(format!(
                    "the saved chain is seeded at burn {seeded_at}, above the burn view \
                    {executed_burn_view} execution has reached, and a chain only walks forward"
                )));
            }
            if tracker.tip().num_sortitions.is_none() {
                return Err(TrackerError::Seed(
                    "the saved chain predates its cumulative sortition count".to_owned(),
                ));
            }
            Ok(tracker)
        });
        let mut tracker = match saved {
            Ok(tracker) => tracker,
            Err(saved) => {
                // Said out loud, because the fallback is not free: re-deriving
                // from the checkpoint's own anchor is one Bitcoin block download
                // per burn block, minutes of them on mainnet, and a node that
                // prints nothing while doing it looks stopped.
                eprintln!(
                    "the saved sortitions cannot seed a chain, so it is re-derived from the \
                     checkpoint: {saved}"
                );
                Self::from_capture_below(capture, executed_burn_view).map_err(|captured| {
                    TrackerError::Seed(format!(
                        "neither the saved sortitions ({saved}) nor the capture ({captured}) \
                         can seed a chain"
                    ))
                })?
            }
        };
        // A registry is checkpoint data, not derived data, so a state directory
        // written before it existed takes it from the capture rather than being
        // re-imported. The saved copy wins when there is one, because it holds
        // the registrations this chain walked past above the checkpoint as well.
        if tracker.leader_keys() == 0 {
            tracker.load_leader_keys(capture)?;
        }
        if tracker.leader_keys() == 0 {
            eprintln!(
                "this checkpoint carries no leader-key registry ({}), so no tenure's \
                 coinbase proof and no miner signature can be checked: the registration a \
                 winning commitment names sits tens of thousands of burn blocks below any \
                 window this node holds. `cargo xtask export-leader-keys` writes one.",
                capture.join(LEADER_KEY_FILE).display()
            );
        } else {
            println!(
                "{} leader-key registrations carried with the checkpoint",
                tracker.leader_keys()
            );
        }
        Ok(tracker)
    }

    /// Write the chain down, so the next start resumes instead of re-deriving.
    ///
    /// Only the tip is written: a chain is seeded by the snapshot its history ends
    /// at, and every snapshot before it has already done its work. The history
    /// goes whole, because `ConsensusHash::from_ops` mixes hashes at power-of-two
    /// offsets and a truncated one derives different hashes from there on.
    pub fn save(&self, directory: &Path) -> Result<(), TrackerError> {
        self.save_standing_on(directory, self.tip().bitcoin_height)
    }

    /// Write the chain down as it stood at a burn block, which is not always its tip.
    ///
    /// The tip runs *ahead* of execution: `locate_view` walks toward Bitcoin's end
    /// to find the burn block a block names, and it keeps what it derived. Saving
    /// that tip means a resumed chain is seeded above the burn view execution still
    /// needs — and a chain only walks forward, so every staged block standing lower
    /// finds no snapshot, the local derivation returns nothing, and the tenure VRF
    /// check falls back to a peer's answer and fails. Measured on the live mainnet
    /// follower: seeded at burn 961,342, stopped on burn 961,321 with `committed seed
    /// is not the hash of the parent tenure's VRF proof`.
    ///
    /// So the chain is written down as it stood on the burn view it has *executed*
    /// to. What is above that is a lookahead, and re-deriving it costs the same
    /// bounded walk that produced it.
    pub fn save_standing_on(
        &self,
        directory: &Path,
        bitcoin_height: u64,
    ) -> Result<(), TrackerError> {
        self.save_standing_on_with(directory, bitcoin_height, |name, bytes| {
            replace_file(directory, name, bytes)
        })
    }

    fn save_standing_on_with(
        &self,
        directory: &Path,
        bitcoin_height: u64,
        mut write: impl FnMut(&str, Vec<u8>) -> Result<(), TrackerError>,
    ) -> Result<(), TrackerError> {
        // A chain that has nowhere to be written down is re-derived from the
        // checkpoint on the next start, one Bitcoin block download per burn block,
        // and the only sign of it is a line in a log. Making the directory is
        // cheaper than that, and the failure is reported rather than swallowed.
        fs::create_dir_all(directory).map_err(|error| TrackerError::Seed(error.to_string()))?;
        // The tip when execution has caught up to it, and never above what the
        // history can be truncated to.
        //
        // `unwrap_or_else(tip)` used to be here, and it undid the whole point of this
        // function: where the retained window no longer reaches the executed burn
        // view -- which is exactly what a rewind or a long stall produces -- it saved
        // the *lookahead* tip instead, and the next start seeded above what execution
        // needed and could not walk back to it. A live mainnet state was left
        // unusable that way: executed tip needing burn 961,447, saved chain ending at
        // 961,450, and re-seeding from the checkpoint landed at 961,451.
        //
        // So a chain that cannot stand where it is asked to says so and writes
        // nothing. The previous file stays, and a start that finds it unusable falls
        // back to the capture, which is slow and correct -- where this was fast and
        // wrong.
        let asked = bitcoin_height.min(self.tip().bitcoin_height);
        let Some(tip) = self.snapshot_at(asked) else {
            return Err(TrackerError::Seed(format!(
                "this chain keeps no snapshot for burn {asked}, which is the burn view \
                 execution has reached, so writing it down would save a chain seeded above \
                 what a restart needs"
            )));
        };
        let pox_payouts = self
            .waterfall_payouts
            .range(..=tip.bitcoin_height)
            .next_back()
            .map(|(_, payout)| encoded_waterfall_payout(payout.recipient))
            .transpose()?;
        let snapshots = vec![CapturedSnapshot {
            block_height: tip.bitcoin_height,
            burn_header_hash: hex::encode(tip.bitcoin_header_hash.as_bytes()),
            sortition_id: hex::encode(tip.sortition_id.as_bytes()),
            consensus_hash: tip.consensus_hash.to_string(),
            burn_header_timestamp: tip.bitcoin_timestamp,
            sortition_hash: hex::encode(tip.sortition_hash.as_bytes()),
            total_burn: tip.total_burn.to_string(),
            pox_payouts,
            num_sortitions: tip.num_sortitions,
            sortition: Some(i64::from(tip.winner_txid.is_some())),
            winning_block_txid: tip.winner_txid.map(hex::encode),
            // The one field a resumed chain cannot derive and must not guess -- and
            // the row is the burn block *execution* reached, so the seed has to be
            // the one that had been won by then. Taken from the chain's tip, it was
            // the lookahead's: a row calling itself burn 961,448 carried the seed of
            // the commitments in 961,459, the resumed chain sampled 961,449 against
            // it, and the miner it named was not the one the network elected -- so
            // every block of that tenure was refused for a signature the winning key
            // could not have made. `None` where nothing at or below the row had won
            // yet, which the loader refuses and re-derives rather than guesses.
            winner_vrf_seed: self
                .engine
                .snapshots()
                .effective_winner_seed_at_or_below(tip.bitcoin_height)
                .map(hex::encode),
            // The other one. Where the tip itself elected somebody this is the tip's
            // own height and the resumed chain would find it anyway; where it did
            // not, this is the only thing that can name it.
            last_sortition_height: self
                .engine
                .snapshots()
                .last_sortition_at_or_below(tip.bitcoin_height),
            // Every burn height that elected somebody and has left the window, not
            // just the newest. A resumed chain is seeded at the burn block its
            // history ends at and holds no snapshot below it, while execution is
            // still working through staged blocks standing on earlier burn views --
            // so one height answers for everything at or above itself and for
            // nothing below, and the run is what makes the resumed chain able to
            // answer what an unrestarted one could.
            sortitions_below_window: self.engine.snapshots().sortitions_below_window().to_vec(),
            // Written down for the same reason the two above are: the next start is
            // seeded on this row, and a seed that cannot state its winner's key or
            // the block it committed to answers `/v3/sortitions` with nulls.
            parent_sortition_id: Some(hex::encode(tip.parent_sortition_id.as_bytes())),
            miner_pk_hash: tip.winner_signing_key_hash.map(hex::encode),
            winning_stacks_block_hash: tip.committed_block_hash.map(hex::encode),
        }];
        let history = History {
            // Truncated to end at the snapshot above, because `from_capture` seeds a
            // chain from the snapshot its history ends at and refuses a pair that
            // disagree. The hashes dropped are the lookahead's, and the next start
            // re-derives them by walking forward.
            hashes: {
                let history = self.engine.snapshots().history();
                let ahead =
                    usize::try_from(self.tip().bitcoin_height.saturating_sub(tip.bitcoin_height))
                        .unwrap_or(0);
                history[..history.len().saturating_sub(ahead)]
                    .iter()
                    .map(ToString::to_string)
                    .collect()
            },
        };
        // The registry goes with them, and it has to: a key registered in the
        // burn blocks this chain has already walked past would otherwise be lost
        // on the next start, because a resumed chain reads the blocks *after* its
        // saved tip and never those before it.
        let keys: Vec<CapturedLeaderKey> = self
            .keys
            .entries()
            .map(|(block_height, vtxindex, registration)| CapturedLeaderKey {
                block_height,
                vtxindex,
                public_key: hex::encode(registration.vrf_public_key),
                memo: registration
                    .signing_key_hash
                    .map(hex::encode)
                    .unwrap_or_default(),
            })
            .collect();
        write(
            "consensus-hashes.json",
            serde_json::to_vec(&history).map_err(|error| TrackerError::Seed(error.to_string()))?,
        )?;
        write(
            LEADER_KEY_FILE,
            serde_json::to_vec(&keys).map_err(|error| TrackerError::Seed(error.to_string()))?,
        )?;
        let waterfall_payouts = self
            .waterfall_payouts
            .iter()
            .filter(|(_, payout)| payout.observed_at <= tip.bitcoin_height)
            .map(|(bitcoin_height, payout)| captured_waterfall_payout(*bitcoin_height, *payout))
            .collect::<Result<Vec<_>, _>>()?;
        write(
            WATERFALL_PAYOUT_FILE,
            serde_json::to_vec(&waterfall_payouts)
                .map_err(|error| TrackerError::Seed(error.to_string()))?,
        )?;
        write(
            "snapshots.json",
            serde_json::to_vec(&snapshots)
                .map_err(|error| TrackerError::Seed(error.to_string()))?,
        )
    }

    pub fn from_capture(directory: &Path) -> Result<Self, TrackerError> {
        let snapshots = captured_snapshots(directory)?;
        // The one snapshot a history can seed is the one it ends at: the
        // consensus hash of every block after it has to be derived, not stated,
        // or the chain would be quoting the capture rather than checking it.
        let history = Self::history_from(directory)?;
        let anchor = history
            .last()
            .ok_or_else(|| TrackerError::Seed("the history is empty".to_owned()))?
            .to_string();
        let seed = snapshots
            .iter()
            .find(|snapshot| snapshot.consensus_hash == anchor)
            .ok_or_else(|| {
                TrackerError::Seed(format!(
                    "no snapshot for the hash the history ends at: {anchor}"
                ))
            })?;
        tracker_from_capture_seed(directory, seed, history)
    }

    /// Start from the latest winning captured snapshot below execution's admitted
    /// rollback window.
    fn from_capture_below(directory: &Path, executed_burn_view: u64) -> Result<Self, TrackerError> {
        let snapshots = captured_snapshots(directory)?;
        let mut history = Self::history_from(directory)?;
        let rollback_floor = execution_rollback_floor(executed_burn_view);
        let (index, seed) = history
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, hash)| {
                let hash = hash.to_string();
                snapshots
                    .iter()
                    .find(|snapshot| {
                        snapshot.consensus_hash == hash
                            && snapshot.block_height <= rollback_floor
                            && matches!(snapshot.sortition, Some(sortition) if sortition != 0)
                    })
                    .map(|snapshot| (index, snapshot))
            })
            .ok_or_else(|| {
                TrackerError::Seed(format!(
                    "the capture has no winning seed at or below rollback floor burn \
                     {rollback_floor} for executed burn {executed_burn_view}"
                ))
            })?;
        history.truncate(index + 1);
        tracker_from_capture_seed(directory, seed, history)
    }

    /// Start a fresh checkpoint no later than its attested parent-tenure boundary.
    ///
    /// A general capture is normally seeded at its latest winner. Authentication
    /// starts earlier: it must verify the first included tenure against the
    /// preceding tenure's proof, so seeding above that boundary makes the proof
    /// uncheckable. This selector takes the boundary from the artifact and
    /// truncates a carried history that reaches past it. When the capture's seed
    /// is below the boundary, it keeps that seed so the caller can derive the
    /// boundary from Bitcoin instead of trusting a captured snapshot above it.
    pub fn from_capture_at_consensus(
        directory: &Path,
        boundary: ConsensusHash,
    ) -> Result<Self, TrackerError> {
        let snapshots = captured_snapshots(directory)?;
        let boundary_text = boundary.to_string();
        let mut matching = snapshots
            .iter()
            .filter(|snapshot| snapshot.consensus_hash == boundary_text);
        let seed = matching
            .next()
            .ok_or(TrackerError::BoundarySnapshotMissing(boundary))?;
        if matching.next().is_some() {
            return Err(TrackerError::BoundarySnapshotDuplicate(boundary));
        }
        if !matches!(seed.sortition, Some(sortition) if sortition != 0) {
            return Err(TrackerError::BoundaryNotWinner {
                consensus_hash: boundary,
                bitcoin_height: seed.block_height,
            });
        }
        let mut history = Self::history_from(directory)?;
        let mut positions = history
            .iter()
            .enumerate()
            .filter_map(|(index, hash)| (*hash == boundary).then_some(index));
        let boundary_index = positions.next();
        if positions.next().is_some() {
            return Err(TrackerError::BoundaryHistoryDuplicate(boundary));
        }
        if let Some(boundary_index) = boundary_index {
            history.truncate(boundary_index + 1);
            return tracker_from_capture_seed(directory, seed, history);
        }

        let anchor = history
            .last()
            .ok_or_else(|| TrackerError::Seed("the history is empty".to_owned()))?
            .to_string();
        let mut anchors = snapshots
            .iter()
            .filter(|snapshot| snapshot.consensus_hash == anchor);
        let captured_seed = anchors.next().ok_or_else(|| {
            TrackerError::Seed(format!(
                "no snapshot for the hash the history ends at: {anchor}"
            ))
        })?;
        if anchors.next().is_some() {
            return Err(TrackerError::Seed(format!(
                "more than one snapshot names the hash the history ends at: {anchor}"
            )));
        }
        if captured_seed.block_height >= seed.block_height {
            return Err(TrackerError::BoundaryHistoryMissing(boundary));
        }
        tracker_from_capture_seed(directory, captured_seed, history)
    }
}

fn replace_file(directory: &Path, name: &str, bytes: Vec<u8>) -> Result<(), TrackerError> {
    let path = directory.join(name);
    let temporary = directory.join(format!("{name}.partial"));
    fs::write(&temporary, bytes).map_err(|error| TrackerError::Seed(error.to_string()))?;
    fs::rename(&temporary, &path).map_err(|error| TrackerError::Seed(error.to_string()))
}

fn captured_snapshots(directory: &Path) -> Result<Vec<CapturedSnapshot>, TrackerError> {
    let bytes = fs::read(directory.join("snapshots.json"))
        .map_err(|error| TrackerError::Seed(error.to_string()))?;
    serde_json::from_slice(&bytes).map_err(|error| TrackerError::Seed(error.to_string()))
}

fn tracker_from_capture_seed(
    directory: &Path,
    seed: &CapturedSnapshot,
    history: Vec<ConsensusHash>,
) -> Result<SortitionTracker, TrackerError> {
    let mut tracker = SortitionTracker::new(seed_snapshot(seed)?, history)?;
    // Where the last sortition at or below the seed was, for a seed that did not
    // itself elect anybody. A capture's rows do not carry it and do not need to:
    // a captured seed is refused above unless its own block had a sortition.
    tracker
        .engine
        .snapshots_mut()
        .seed_sortition_below_window(seed.last_sortition_height);
    // And the run behind it, where a saved chain carries one. Older states have
    // none and fall back to the single height above, which is what they had.
    if !seed.sortitions_below_window.is_empty() {
        tracker
            .engine
            .snapshots_mut()
            .seed_sortitions_below_window(seed.sortitions_below_window.clone());
    }
    tracker.waterfall_payouts = read_waterfall_payouts(directory, seed.block_height)?;
    tracker.load_leader_keys(directory)?;
    Ok(tracker)
}

fn read_waterfall_payouts(
    directory: &Path,
    through: u64,
) -> Result<BTreeMap<u64, WaterfallPayout>, TrackerError> {
    let mut payouts: BTreeMap<u64, WaterfallPayout> = BTreeMap::new();
    for snapshot in captured_snapshots(directory)?
        .into_iter()
        .filter(|snapshot| snapshot.block_height <= through)
    {
        let Some(encoded) = snapshot.pox_payouts.as_deref() else {
            continue;
        };
        if let Some(recipient) = parse_waterfall_payout(encoded)? {
            let payout = WaterfallPayout {
                observed_at: snapshot.block_height,
                recipient,
            };
            if payouts
                .last_key_value()
                .is_none_or(|(_, previous)| previous.recipient != recipient)
            {
                payouts.insert(snapshot.block_height, payout);
            }
        }
    }
    let path = directory.join(WATERFALL_PAYOUT_FILE);
    if let Ok(bytes) = fs::read(&path) {
        let recorded: Vec<CapturedWaterfallPayout> = serde_json::from_slice(&bytes)
            .map_err(|error| TrackerError::Seed(format!("{}: {error}", path.display())))?;
        for payout in recorded
            .into_iter()
            .filter(|payout| payout.observed_at <= through)
        {
            payouts.insert(payout.bitcoin_height, payout.payout());
        }
    }
    Ok(payouts)
}

fn parse_waterfall_payout(encoded: &str) -> Result<Option<PoxAddress>, TrackerError> {
    let (addresses, _): (Vec<serde_json::Value>, u64) = serde_json::from_str(encoded)
        .map_err(|error| TrackerError::Seed(format!("invalid captured PoX payouts: {error}")))?;
    let Some(value) = addresses.first().and_then(|address| address.get("Addr32")) else {
        return Ok(None);
    };
    let (mainnet, address_type, bytes): (bool, String, [u8; 32]) =
        serde_json::from_value(value.clone()).map_err(|error| {
            TrackerError::Seed(format!("invalid captured waterfall payout: {error}"))
        })?;
    if address_type != "P2TR" {
        return Err(TrackerError::Seed(format!(
            "captured waterfall payout uses {address_type}, not P2TR"
        )));
    }
    Ok(Some(PoxAddress::Addr32 {
        mainnet,
        address_type: PoxAddressType32::P2tr,
        bytes,
    }))
}

fn captured_waterfall_payout(
    bitcoin_height: u64,
    payout: WaterfallPayout,
) -> Result<CapturedWaterfallPayout, TrackerError> {
    let recipient = payout.recipient;
    let PoxAddress::Addr32 {
        mainnet,
        address_type: PoxAddressType32::P2tr,
        bytes,
    } = recipient
    else {
        return Err(TrackerError::Seed(
            "a waterfall payout is not a P2TR address".to_owned(),
        ));
    };
    Ok(CapturedWaterfallPayout {
        bitcoin_height,
        observed_at: payout.observed_at,
        mainnet,
        bytes,
    })
}

fn encoded_waterfall_payout(recipient: PoxAddress) -> Result<String, TrackerError> {
    let value = match recipient {
        PoxAddress::Addr32 {
            mainnet,
            address_type: PoxAddressType32::P2tr,
            bytes,
        } => serde_json::json!([[{"Addr32": [mainnet, "P2TR", bytes]}], 0]),
        _ => {
            return Err(TrackerError::Seed(
                "a waterfall payout is not a P2TR address".to_owned(),
            ));
        }
    };
    Ok(value.to_string())
}

impl CapturedWaterfallPayout {
    const fn payout(self) -> WaterfallPayout {
        WaterfallPayout {
            observed_at: self.observed_at,
            recipient: PoxAddress::Addr32 {
                mainnet: self.mainnet,
                address_type: PoxAddressType32::P2tr,
                bytes: self.bytes,
            },
        }
    }
}

/// The block-signing hash the seed's winning key was registered with.
///
/// `None` where the seed's burn block elected nobody, and where the export predates
/// the field -- in both cases the answer is unavailable rather than zero, because a
/// zero here is a miner nobody can be.
fn seed_signing_key_hash(seed: &CapturedSnapshot) -> Result<Option<[u8; 20]>, TrackerError> {
    let (Some(sortition), Some(hash)) = (seed.sortition, seed.miner_pk_hash.as_deref()) else {
        return Ok(None);
    };
    if sortition == 0 {
        return Ok(None);
    }
    let bytes = hex::decode(hash)
        .map_err(|_| TrackerError::Seed("the miner key hash is not hexadecimal".to_owned()))?;
    <[u8; 20]>::try_from(bytes.as_slice())
        .map(Some)
        .map_err(|_| TrackerError::Seed("the miner key hash is not 20 bytes".to_owned()))
}

/// The Stacks block the seed's winning commitment committed to.
fn seed_committed_block_hash(seed: &CapturedSnapshot) -> Result<Option<[u8; 32]>, TrackerError> {
    let (Some(sortition), Some(hash)) = (seed.sortition, seed.winning_stacks_block_hash.as_deref())
    else {
        return Ok(None);
    };
    if sortition == 0 {
        return Ok(None);
    }
    let bytes = hex::decode(hash).map_err(|_| {
        TrackerError::Seed("the winning stacks block hash is not hexadecimal".to_owned())
    })?;
    <[u8; 32]>::try_from(bytes.as_slice())
        .map(Some)
        .map_err(|_| TrackerError::Seed("the winning stacks block hash is not 32 bytes".to_owned()))
}

fn seed_snapshot(seed: &CapturedSnapshot) -> Result<SortitionSnapshot, TrackerError> {
    // The sampling of the block after the seed mixes the most recent winner's VRF
    // seed, and where that seed is not the seed block's own it cannot be found in
    // the seed block. Refusing here is what stops the alternative: adopting the
    // seed the block's commitments were *bidding* with, which names a different
    // winner and so a different leader key — a wrong answer that no consensus
    // hash, sortition hash or burn total disagrees with, and that surfaces only
    // as a valid tenure's coinbase proof being rejected. Mainnet does this once
    // every three or four burn blocks.
    match (seed.winner_vrf_seed.is_some(), seed.sortition) {
        // Stated by the chain that derived it: nothing to recover.
        (true, _) => {}
        // A capture whose seed elected somebody: `prime` recovers the seed from
        // that block's own commitments, which all carry it.
        (false, Some(sortition)) if sortition != 0 => {}
        (false, Some(_)) => {
            return Err(TrackerError::Seed(format!(
                "burn {} elected nobody, so the winning seed the next sortition mixes is an \
                 older block's and this snapshot does not carry it — seed the chain at a \
                 burn block that had a sortition, or resume from a chain that saved the seed",
                seed.block_height
            )));
        }
        (false, None) => {
            return Err(TrackerError::Seed(format!(
                "the snapshot at burn {} says neither whether that block elected anybody nor \
                 what winning seed the next sortition mixes, which is how a chain saved \
                 before those were written down looks. Guessing costs a wrongly named \
                 winner one restart in three, and the only thing that names is the leader \
                 key a coinbase proof is checked against — so this is re-derived instead.",
                seed.block_height
            )));
        }
    }
    let bytes = |value: &str, name: &str| -> Result<Vec<u8>, TrackerError> {
        hex::decode(value).map_err(|_| TrackerError::Seed(format!("{name} is not hexadecimal")))
    };
    let thirty_two = |value: &str, name: &str| -> Result<[u8; 32], TrackerError> {
        <[u8; 32]>::try_from(bytes(value, name)?.as_slice())
            .map_err(|_| TrackerError::Seed(format!("{name} is not 32 bytes")))
    };
    let bitcoin_header_hash = nano_primitives::BitcoinHeaderHash::from_bytes(thirty_two(
        &seed.burn_header_hash,
        "burn header hash",
    )?);
    let sortition_id =
        nano_primitives::SortitionId::from_bytes(thirty_two(&seed.sortition_id, "sortition id")?);
    // The capture's own identifier says which `PoX` bit vector produced it, so
    // the vector is read off the checkpoint rather than configured. Configured,
    // it was `PoxId::initial()` — one bit where mainnet has 142 — and since the
    // consensus hash mixes the vector, every hash this node derived was wrong for
    // that reason alone, however right the rest of the arithmetic was.
    let pox_id = unbroken_pox_id_for(bitcoin_header_hash, sortition_id, POX_HISTORY_SEARCH_LIMIT)
        .ok_or_else(|| {
        TrackerError::Seed(format!(
            "the seed's sortition identifier {} is not the burn header hash and an \
                 unbroken PoX history hashed together, so this node cannot tell which \
                 reward-cycle history the checkpoint stands on",
            seed.sortition_id
        ))
    })?;
    Ok(SortitionSnapshot {
        bitcoin_height: seed.block_height,
        bitcoin_header_hash,
        bitcoin_timestamp: seed.burn_header_timestamp,
        sortition_id,
        parent_sortition_id: seed
            .parent_sortition_id
            .as_deref()
            .map(|value| thirty_two(value, "parent sortition id"))
            .transpose()?
            .map_or_else(
                || nano_primitives::SortitionId::from_bytes([0; 32]),
                nano_primitives::SortitionId::from_bytes,
            ),
        // Never read: only the hash of a block *after* the seed is derived.
        operations_hash: nano_sortition::OpsHash::from_txids(&[]),
        consensus_hash: ConsensusHash::from_bytes(
            <[u8; 20]>::try_from(bytes(&seed.consensus_hash, "consensus hash")?.as_slice())
                .map_err(|_| TrackerError::Seed("consensus hash is not 20 bytes".to_owned()))?,
        ),
        total_burn: seed
            .total_burn
            .parse()
            .map_err(|_| TrackerError::Seed("total burn is not a number".to_owned()))?,
        sortition_hash: nano_sortition::SortitionHash::from_bytes(thirty_two(
            &seed.sortition_hash,
            "sortition hash",
        )?),
        num_sortitions: seed.num_sortitions,
        // Named where the seed's block elected somebody, because the winner's own
        // payout burn is a Clarity answer for every tenure standing on that burn
        // view. The `sortition` column decides it rather than the value, since a
        // capture writes all zeroes for a block that elected nobody.
        winner_txid: match (seed.sortition, seed.winning_block_txid.as_deref()) {
            (Some(sortition), Some(txid)) if sortition != 0 => {
                Some(thirty_two(txid, "winning block txid")?)
            }
            _ => None,
        },
        // The seed the sampling of the block after this one mixes. A chain this
        // node saved states it, exactly, because it derived it. A capture does
        // not, and then it is recovered from the seed's own burn block by
        // `prime` — which is only sound where the seed block *elected* somebody,
        // so a capture that says otherwise is refused above rather than seeded
        // against a seed nobody won.
        winner_vrf_seed: seed
            .winner_vrf_seed
            .as_deref()
            .map(|value| thirty_two(value, "winning VRF seed"))
            .transpose()?,
        // Resolved during priming from the seed's winning commitment and the
        // carried leader-key registry, before any block in that tenure executes.
        winner_vrf_public_key: None,
        // Stated by the archive for the seed, unlike the VRF key: `miner_pk_hash`
        // is the block-signing hash the winning key was registered with, which is
        // what `/v3/sortitions` reports and what a miner signs its headers under.
        winner_signing_key_hash: seed_signing_key_hash(seed)?,
        // The block the seed's winner committed to, where the archive states it.
        // Not the *parent* burn height, which no column holds: that one stays
        // unanswered for a seed rather than guessed at.
        committed_block_hash: seed_committed_block_hash(seed)?,
        parent_bitcoin_height: None,
        // Filled by `prime`, which reads the seed's own burn block: the two spends
        // come out of the commitment window, not out of a captured row.
        burn_spends: None,
        // Rebuilt by the same priming walk from the seed's commitment window.
        mining_competition: None,
        pox_id,
    })
}

fn ordered_miner_slot_writers(
    newest: &SortitionSnapshot,
    previous: &SortitionSnapshot,
) -> Option<[Hash160; 2]> {
    let newest_is_first = newest.num_sortitions? % 2 == 0;
    let newest = Hash160::from_bytes(newest.winner_signing_key_hash?);
    let previous = Hash160::from_bytes(previous.winner_signing_key_hash?);
    Some(if newest_is_first {
        [newest, previous]
    } else {
        [previous, newest]
    })
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use nano_address::{PoxAddress, PoxAddressType32};
    use nano_bitcoin::{
        BitcoinBlock, BitcoinInput, BitcoinOperation, BitcoinOperationKind, BitcoinOutput,
    };
    use nano_primitives::{ConsensusHash, sha512_256};
    use nano_sortition::{
        OpsHash, PayoutSchedule, PoxId, RewardCycleSchedule, SortitionHash, SortitionSnapshot,
    };
    use nano_sync::BurnView as _;

    use super::{
        CapturedSnapshot, History, LEADER_KEY_FILE, SortitionTracker, TrackerError,
        WATERFALL_PAYOUT_FILE, captured_snapshots, execution_rollback_floor,
        ordered_miner_slot_writers, replace_file, seed_snapshot,
    };

    fn captured_sortitions() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nano-conformance/fixtures/sortition")
    }

    fn captured_consensus_hash(snapshot: &CapturedSnapshot) -> ConsensusHash {
        let bytes = hex::decode(&snapshot.consensus_hash).expect("decode captured consensus hash");
        ConsensusHash::from_bytes(
            <[u8; 20]>::try_from(bytes).expect("a captured consensus hash is 20 bytes"),
        )
    }

    /// A chain standing on one snapshot, with one hash behind it.
    pub(super) fn a_chain() -> SortitionTracker {
        tracker(1_000)
    }

    fn tracker(total_burn: u64) -> SortitionTracker {
        tracker_with_seed(None, Some([9; 32]), total_burn)
    }

    pub(super) fn tracker_with_seed(
        winner_txid: Option<[u8; 32]>,
        winner_vrf_seed: Option<[u8; 32]>,
        total_burn: u64,
    ) -> SortitionTracker {
        let behind = ConsensusHash::from_bytes([0xbe; 20]);
        let bitcoin_header_hash = nano_primitives::BitcoinHeaderHash::from_bytes([1; 32]);
        let pox_id = PoxId::initial();
        let mut sortition_preimage = bitcoin_header_hash.as_bytes().to_vec();
        sortition_preimage.extend_from_slice(&pox_id.as_consensus_bytes());
        let seed = SortitionSnapshot {
            bitcoin_height: 100,
            bitcoin_header_hash,
            bitcoin_timestamp: 0,
            sortition_id: nano_primitives::SortitionId::from_bytes(
                *sha512_256(&sortition_preimage).as_bytes(),
            ),
            parent_sortition_id: nano_primitives::SortitionId::from_bytes([3; 32]),
            operations_hash: OpsHash::from_txids(&[]),
            consensus_hash: ConsensusHash::from_bytes([0x7f; 20]),
            total_burn,
            sortition_hash: SortitionHash::from_bytes([4; 32]),
            num_sortitions: Some(u64::from(winner_txid.is_some())),
            winner_txid,
            winner_vrf_seed,
            winner_vrf_public_key: None,
            winner_signing_key_hash: None,
            committed_block_hash: None,
            parent_bitcoin_height: None,
            burn_spends: None,
            mining_competition: None,
            pox_id,
        };
        let history = vec![behind, seed.consensus_hash];
        SortitionTracker::new(seed, history).expect("the history ends at the seed")
    }

    /// A view this chain dropped is not a view it has not reached.
    ///
    /// The stall those two being one answer produced ran sixteen hours on mainnet
    /// while `/health` said `ready: true`. A chain that has not walked far enough
    /// clears itself on the next round; a chain whose window closed above the view
    /// never does, because it only walks forward.
    #[test]
    fn a_dropped_view_is_told_apart_from_one_not_yet_walked_to() {
        let chain = a_chain();

        // The tip's own view is retained, so nothing closed below it.
        let tip = chain.tip().consensus_hash;
        assert_eq!(chain.height_of_consensus_hash(tip), Some(100));
        assert_eq!(chain.window_closed_below(tip), None);

        // The hash behind the seed: named by the history, which is never
        // front-pruned, and with no snapshot to answer from. This is the stall.
        let behind = ConsensusHash::from_bytes([0xbe; 20]);
        assert_eq!(chain.height_of_consensus_hash(behind), Some(99));
        assert_eq!(chain.snapshot_at(99), None);
        assert_eq!(
            chain.window_closed_below(behind),
            Some(99),
            "a view the chain derived and dropped has to be recognisable as that"
        );

        // A view from some other chain of Bitcoin blocks is named by nothing, so
        // it stays the ordinary case a further walk may still answer.
        assert_eq!(
            chain.window_closed_below(ConsensusHash::from_bytes([0x5c; 20])),
            None,
            "a view this chain never derived is not a dropped one"
        );
    }

    #[test]
    fn miner_slot_order_follows_the_cumulative_sortition_parity() {
        let mut newest =
            SortitionSnapshot::genesis(2, nano_primitives::BitcoinHeaderHash::from_bytes([2; 32]));
        newest.winner_signing_key_hash = Some([1; 20]);
        let mut previous =
            SortitionSnapshot::genesis(1, nano_primitives::BitcoinHeaderHash::from_bytes([1; 32]));
        previous.winner_signing_key_hash = Some([2; 20]);

        newest.num_sortitions = Some(8);
        assert_eq!(
            ordered_miner_slot_writers(&newest, &previous),
            Some([
                nano_primitives::Hash160::from_bytes([1; 20]),
                nano_primitives::Hash160::from_bytes([2; 20]),
            ])
        );
        newest.num_sortitions = Some(9);
        assert_eq!(
            ordered_miner_slot_writers(&newest, &previous),
            Some([
                nano_primitives::Hash160::from_bytes([2; 20]),
                nano_primitives::Hash160::from_bytes([1; 20]),
            ])
        );
        newest.num_sortitions = None;
        assert_eq!(ordered_miner_slot_writers(&newest, &previous), None);
    }

    fn block_with(height: u64, operations: Vec<BitcoinOperation>) -> BitcoinBlock {
        BitcoinBlock {
            height,
            hash: [1; 32],
            timestamp: 0,
            operations,
        }
    }

    fn commitment(txid: [u8; 32], seed: [u8; 32], height: u64, on_time: bool) -> BitcoinOperation {
        let timely = u8::try_from((height + 4) % 5).expect("modulo five fits u8");
        BitcoinOperation {
            txid,
            transaction_index: 0,
            inputs: Vec::new(),
            outputs: Vec::new(),
            kind: BitcoinOperationKind::LeaderBlockCommit {
                block_header_hash: [0; 32],
                new_seed: seed,
                parent_block_height: 0,
                parent_transaction_index: 0,
                key_block_height: 0,
                key_transaction_index: 0,
                memo: 0,
                parent_modulus: if on_time { timely } else { (timely + 1) % 5 },
            },
        }
    }

    #[test]
    fn a_resumed_winning_seed_recovers_its_vrf_key_before_execution() {
        let height = 100;
        let winner = [0x11; 32];
        let seed = [0xaa; 32];
        let public_key = [0x22; 32];
        let mut tracker = tracker_with_seed(Some(winner), Some(seed), 1_000);
        tracker.keys.register(
            0,
            0,
            nano_sortition::LeaderKeyRegistration {
                vrf_public_key: public_key,
                signing_key_hash: Some([0x33; 20]),
            },
        );
        let winner_block = block_with(height, vec![commitment(winner, seed, height, true)]);
        assert_eq!(tracker.tip().winner_vrf_public_key, None);
        tracker
            .recover_seed(|_| Ok::<_, String>(winner_block.clone()))
            .expect("recovery resolves the winner through the carried registry");
        assert_eq!(tracker.tip().winner_vrf_public_key, Some(public_key));
    }

    /// Where a candidate's burn total places it against this chain.
    ///
    /// The three answers, and the boundary between them is what matters: a header
    /// stating *strictly less* burn than this chain's tip was built below it and
    /// has to name a view this chain holds, while one stating exactly the tip's
    /// total may be a burn block ahead that elected nobody — the shape mainnet
    /// leaves in four burn blocks out of every fifteen — and is not judged.
    #[test]
    fn a_burn_total_below_the_tip_places_a_candidate_on_this_chain() {
        let tracker = tracker(1_000);
        let tip = ConsensusHash::from_bytes([0x7f; 20]);
        let behind = ConsensusHash::from_bytes([0xbe; 20]);
        let foreign = ConsensusHash::from_bytes([0xaa; 20]);

        assert_eq!(tracker.derived(tip, 999), Some(true));
        assert_eq!(tracker.derived(behind, 999), Some(true));
        assert_eq!(
            tracker.derived(foreign, 999),
            Some(false),
            "a view below this chain's tip that this chain never derived is another burnchain"
        );
        assert_eq!(
            tracker.derived(foreign, 1_000),
            None,
            "a candidate at this chain's own total may be a sortition-less block ahead of it"
        );
        assert_eq!(tracker.derived(foreign, 8_000_000), None);
    }

    #[test]
    fn a_checkpoint_boundary_selects_and_truncates_the_capture() {
        let directory = captured_sortitions();
        let snapshots = captured_snapshots(&directory).expect("read captured snapshots");
        let ordinary = SortitionTracker::from_capture(&directory).expect("load ordinary seed");
        let boundary = snapshots
            .iter()
            .rev()
            .find(|snapshot| {
                snapshot.block_height < ordinary.tip().bitcoin_height
                    && matches!(snapshot.sortition, Some(sortition) if sortition != 0)
            })
            .expect("the capture has an earlier winning boundary");
        let boundary_hash = captured_consensus_hash(boundary);

        let selected = SortitionTracker::from_capture_at_consensus(&directory, boundary_hash)
            .expect("select the attested boundary");

        assert_eq!(selected.tip().bitcoin_height, boundary.block_height);
        assert_eq!(selected.tip().consensus_hash, boundary_hash);
        assert_eq!(
            selected.engine.snapshots().history().last(),
            Some(&boundary_hash)
        );
        assert!(
            selected.engine.snapshots().history().len()
                < ordinary.engine.snapshots().history().len()
        );
        assert_eq!(selected.leader_keys(), ordinary.leader_keys());
    }

    #[test]
    fn a_checkpoint_boundary_above_the_capture_seed_is_left_to_local_derivation() {
        let directory = captured_sortitions();
        let mut snapshots = captured_snapshots(&directory).expect("read captured snapshots");
        let ordinary = SortitionTracker::from_capture(&directory).expect("load ordinary seed");
        let mut boundary = snapshots
            .iter()
            .max_by_key(|snapshot| snapshot.block_height)
            .expect("the capture has a snapshot")
            .clone();
        boundary.block_height = ordinary.tip().bitcoin_height + 1;
        boundary.consensus_hash = "42".repeat(20);
        boundary.sortition = Some(1);
        let boundary_hash = captured_consensus_hash(&boundary);
        snapshots.push(boundary);

        let extended = tempfile::tempdir().expect("an extended capture");
        fs::write(
            extended.path().join("snapshots.json"),
            serde_json::to_vec(&snapshots).expect("encode snapshots"),
        )
        .expect("write snapshots");
        for name in ["consensus-hashes.json", "leader-keys.json"] {
            fs::copy(directory.join(name), extended.path().join(name))
                .expect("copy the seed artifact");
        }

        let selected = SortitionTracker::from_capture_at_consensus(extended.path(), boundary_hash)
            .expect("retain the authenticated seed below the boundary");

        assert_eq!(selected.tip(), ordinary.tip());
        assert_eq!(
            selected.engine.snapshots().history(),
            ordinary.engine.snapshots().history()
        );
        assert_eq!(selected.leader_keys(), ordinary.leader_keys());
    }

    #[test]
    fn a_capture_seed_above_execution_is_truncated_to_its_latest_winner() {
        let capture = captured_sortitions();
        let snapshots = captured_snapshots(&capture).expect("read captured snapshots");
        let ordinary = SortitionTracker::from_capture(&capture).expect("load ordinary seed");
        let executed = snapshots
            .iter()
            .rev()
            .find(|snapshot| {
                snapshot.block_height < ordinary.tip().bitcoin_height
                    && matches!(snapshot.sortition, Some(sortition) if sortition != 0)
            })
            .expect("the capture has an earlier winning boundary");
        let rollback_floor = execution_rollback_floor(executed.block_height);
        let boundary = snapshots
            .iter()
            .rev()
            .find(|snapshot| {
                snapshot.block_height <= rollback_floor
                    && matches!(snapshot.sortition, Some(sortition) if sortition != 0)
            })
            .expect("the capture has a winner below the rollback floor");
        let state = tempfile::tempdir().expect("an empty role-specific state directory");

        let selected = SortitionTracker::resume_or_capture_below(
            state.path(),
            &capture,
            executed.block_height,
        )
        .expect("truncate the capture below execution's burn view");

        assert_eq!(selected.tip().bitcoin_height, boundary.block_height);
        assert_eq!(
            selected.tip().consensus_hash,
            captured_consensus_hash(boundary)
        );
        assert_eq!(
            selected.engine.snapshots().history().last(),
            Some(&selected.tip().consensus_hash)
        );
        assert_eq!(selected.leader_keys(), ordinary.leader_keys());
    }

    #[test]
    fn a_saved_chain_without_its_sortition_count_is_rederived() {
        let capture = captured_sortitions();
        let tracker = SortitionTracker::from_capture(&capture).expect("load the captured chain");
        let expected = tracker.tip().num_sortitions;
        assert!(expected.is_some(), "the capture states its sortition count");
        let state = tempfile::tempdir().expect("a role-specific state directory");
        tracker.save(state.path()).expect("save the chain");

        let path = state.path().join("snapshots.json");
        let mut snapshots: Vec<serde_json::Value> =
            serde_json::from_slice(&fs::read(&path).expect("read saved snapshots"))
                .expect("decode saved snapshots");
        snapshots[0]
            .as_object_mut()
            .expect("a snapshot object")
            .remove("num_sortitions");
        fs::write(
            &path,
            serde_json::to_vec(&snapshots).expect("encode old snapshots"),
        )
        .expect("write old snapshots");

        let resumed = SortitionTracker::resume_or_capture(state.path(), &capture)
            .expect("re-derive from the complete capture");
        assert_eq!(resumed.tip().num_sortitions, expected);
    }

    #[test]
    fn every_saved_capture_rename_failure_preserves_or_refuses_the_generation() {
        let mut tracker = tracker_with_seed(Some([8; 32]), Some([9; 32]), 1_000);
        let prior_height = tracker.tip().bitcoin_height;
        let payouts = PayoutSchedule::new(
            RewardCycleSchedule::new(0, 1_000, None).expect("a schedule"),
            2,
        )
        .expect("payouts");
        tracker
            .advance(&block_with(prior_height + 1, Vec::new()), payouts)
            .expect("derive the next generation");
        let prior_consensus = tracker
            .snapshot_at(prior_height)
            .expect("the prior burn view is retained")
            .consensus_hash;
        let capture = tempfile::tempdir().expect("a complete fallback capture");
        tracker
            .save_standing_on(capture.path(), prior_height)
            .expect("save the complete fallback generation");
        let files = [
            "consensus-hashes.json",
            LEADER_KEY_FILE,
            WATERFALL_PAYOUT_FILE,
            "snapshots.json",
        ];

        for (fail_at, failed_file) in files.into_iter().enumerate() {
            let state = tempfile::tempdir().expect("a saved-chain directory");
            tracker
                .save_standing_on(state.path(), prior_height)
                .expect("save the complete prior generation");
            SortitionTracker::from_capture(state.path())
                .expect("the complete prior generation reopens before injection");
            let mut writes = 0;
            let error = tracker
                .save_standing_on_with(state.path(), tracker.tip().bitcoin_height, |name, bytes| {
                    writes += 1;
                    if writes == fail_at + 1 {
                        assert_eq!(name, failed_file);
                        return Err(TrackerError::Seed("injected rename failure".to_owned()));
                    }
                    replace_file(state.path(), name, bytes)
                })
                .expect_err("the selected rename fails");
            assert!(error.to_string().contains("injected rename failure"));
            assert_eq!(writes, fail_at + 1);

            match SortitionTracker::from_capture(state.path()) {
                Ok(saved) => {
                    assert_eq!(
                        fail_at, 0,
                        "only a failure before the first rename is complete"
                    );
                    assert_eq!(saved.tip().consensus_hash, prior_consensus);
                }
                Err(_) => assert!(fail_at > 0, "the complete prior generation must reopen"),
            }
            let resumed = SortitionTracker::resume_or_capture_below(
                state.path(),
                capture.path(),
                tracker.tip().bitcoin_height + 10,
            )
            .expect("a torn saved generation falls back to the checkpoint capture");
            assert_eq!(resumed.tip().bitcoin_height, prior_height);
        }
    }

    #[test]
    fn a_late_address_is_visible_as_different_from_the_one_a_cycle_was_derived_under() {
        let mut tracker = tracker(1_000);
        let carried = PoxAddress::Addr32 {
            mainnet: true,
            address_type: PoxAddressType32::P2tr,
            bytes: [0x11; 32],
        };
        tracker.record_waterfall_payout(100, 100, carried);
        // What the chain would pay at a later cycle's first block before that
        // cycle's own address is known: the previous one, which admits no
        // commitment in it.
        assert_eq!(tracker.waterfall_recipient_at(200), Some(carried));

        let settled = PoxAddress::Addr32 {
            mainnet: true,
            address_type: PoxAddressType32::P2tr,
            bytes: [0x22; 32],
        };
        assert_ne!(
            tracker.waterfall_recipient_at(200),
            Some(settled),
            "a cycle derived under the carried address has to be distinguishable \
             from one derived under the address that cycle actually pays"
        );
        tracker.record_waterfall_payout(200, 150, settled);
        assert_eq!(tracker.waterfall_recipient_at(200), Some(settled));
        // Below it, nothing changed.
        assert_eq!(tracker.waterfall_recipient_at(199), Some(carried));
    }

    #[test]
    fn the_captured_waterfall_recipient_survives_a_restart() {
        let capture = captured_sortitions();
        let mut tracker =
            SortitionTracker::from_capture(&capture).expect("load the captured chain");
        let recipient = PoxAddress::Addr32 {
            mainnet: false,
            address_type: PoxAddressType32::P2tr,
            bytes: [
                188, 203, 26, 12, 216, 93, 168, 108, 78, 75, 115, 253, 39, 143, 98, 215, 34, 85,
                112, 39, 36, 119, 22, 206, 78, 69, 249, 48, 33, 116, 201, 145,
            ],
        };
        assert_eq!(
            tracker
                .waterfall_payouts
                .range(..=tracker.tip().bitcoin_height)
                .next_back()
                .map(|(_, payout)| payout.recipient),
            Some(recipient)
        );

        let next = PoxAddress::Addr32 {
            mainnet: false,
            address_type: PoxAddressType32::P2tr,
            bytes: [0xaa; 32],
        };
        tracker.record_waterfall_payout(360, tracker.tip().bitcoin_height, next);
        let not_yet_observed = PoxAddress::Addr32 {
            mainnet: false,
            address_type: PoxAddressType32::P2tr,
            bytes: [0xbb; 32],
        };
        tracker.record_waterfall_payout(380, tracker.tip().bitcoin_height + 1, not_yet_observed);

        let state = tempfile::tempdir().expect("a role-specific state directory");
        tracker.save(state.path()).expect("save the chain");
        let resumed = SortitionTracker::from_capture(state.path()).expect("resume saved chain");
        assert_eq!(
            resumed
                .waterfall_payouts
                .range(..=360)
                .next_back()
                .map(|(_, payout)| payout.recipient),
            Some(next),
            "a future payout learned in the prepare phase survives a restart"
        );
        assert_ne!(
            resumed
                .waterfall_payouts
                .range(..=380)
                .next_back()
                .map(|(_, payout)| payout.recipient),
            Some(not_yet_observed),
            "a payout learned above the saved standing height is not retained"
        );
    }

    #[test]
    fn an_absent_duplicate_or_nonwinning_boundary_is_typed() {
        let source = captured_sortitions();
        let snapshots = captured_snapshots(&source).expect("read captured snapshots");
        let winner = snapshots
            .iter()
            .find(|snapshot| matches!(snapshot.sortition, Some(sortition) if sortition != 0))
            .expect("the capture has a winner");
        let nonwinner = snapshots
            .iter()
            .find(|snapshot| snapshot.sortition == Some(0))
            .expect("the capture has a nonwinner");
        let winner_hash = captured_consensus_hash(winner);
        let nonwinner_hash = captured_consensus_hash(nonwinner);

        assert!(matches!(
            SortitionTracker::from_capture_at_consensus(
                &source,
                ConsensusHash::from_bytes([0xff; 20])
            ),
            Err(TrackerError::BoundarySnapshotMissing(_))
        ));
        assert!(matches!(
            SortitionTracker::from_capture_at_consensus(&source, nonwinner_hash),
            Err(TrackerError::BoundaryNotWinner { .. })
        ));

        let duplicate = tempfile::tempdir().expect("a duplicate capture directory");
        let mut duplicated = snapshots.clone();
        duplicated.push(winner.clone());
        fs::write(
            duplicate.path().join("snapshots.json"),
            serde_json::to_vec(&duplicated).expect("encode duplicate snapshots"),
        )
        .expect("write duplicate snapshots");
        assert!(matches!(
            SortitionTracker::from_capture_at_consensus(duplicate.path(), winner_hash),
            Err(TrackerError::BoundarySnapshotDuplicate(_))
        ));

        let missing_history = tempfile::tempdir().expect("a missing history directory");
        fs::write(
            missing_history.path().join("snapshots.json"),
            serde_json::to_vec(&snapshots).expect("encode snapshots"),
        )
        .expect("write snapshots");
        let mut history = SortitionTracker::history_from(&source).expect("read captured history");
        history.retain(|hash| *hash != winner_hash);
        fs::write(
            missing_history.path().join("consensus-hashes.json"),
            serde_json::to_vec(&History {
                hashes: history.iter().map(ToString::to_string).collect(),
            })
            .expect("encode missing history"),
        )
        .expect("write missing history");
        assert!(matches!(
            SortitionTracker::from_capture_at_consensus(missing_history.path(), winner_hash),
            Err(TrackerError::BoundaryHistoryMissing(_))
        ));

        let mut history = SortitionTracker::history_from(&source).expect("read captured history");
        let boundary_index = history
            .iter()
            .position(|hash| *hash == winner_hash)
            .expect("the history carries the winning snapshot");
        history.insert(boundary_index, winner_hash);
        fs::write(
            missing_history.path().join("consensus-hashes.json"),
            serde_json::to_vec(&History {
                hashes: history.iter().map(ToString::to_string).collect(),
            })
            .expect("encode duplicate history"),
        )
        .expect("write duplicate history");
        assert!(matches!(
            SortitionTracker::from_capture_at_consensus(missing_history.path(), winner_hash),
            Err(TrackerError::BoundaryHistoryDuplicate(_))
        ));
    }

    #[test]
    fn a_named_eligible_winner_recovers_its_seed_despite_disagreeing_candidates() {
        let height = 100;
        let winner = [0x11; 32];
        let mut tracker = tracker_with_seed(Some(winner), None, 1_000);
        let block = block_with(
            height,
            vec![
                commitment(winner, [0xaa; 32], height, true),
                commitment([0x22; 32], [0xbb; 32], height, true),
            ],
        );

        tracker
            .recover_seed(|_| Ok::<_, String>(block.clone()))
            .expect("the named winner makes disagreement irrelevant");
        assert_eq!(tracker.tip().winner_vrf_seed, Some([0xaa; 32]));
    }

    #[test]
    fn disagreeing_candidates_cannot_recover_an_absent_winner() {
        let height = 100;
        let mut tracker = tracker_with_seed(Some([0x33; 32]), None, 1_000);
        let block = block_with(
            height,
            vec![
                commitment([0x11; 32], [0xaa; 32], height, true),
                commitment([0x22; 32], [0xbb; 32], height, true),
            ],
        );

        assert!(matches!(
            tracker.recover_seed(|_| Ok::<_, String>(block.clone())),
            Err(TrackerError::Seed(_))
        ));
        assert_eq!(tracker.tip().winner_vrf_seed, None);
    }

    #[test]
    fn no_eligible_commitment_cannot_recover_a_seed() {
        let height = 100;
        let winner = [0x11; 32];
        let mut tracker = tracker_with_seed(Some(winner), None, 1_000);
        let block = block_with(height, vec![commitment(winner, [0xaa; 32], height, false)]);

        assert!(matches!(
            tracker.recover_seed(|_| Ok::<_, String>(block.clone())),
            Err(TrackerError::Seed(_))
        ));
    }

    #[test]
    fn unanimous_eligible_candidates_recover_an_undecoded_winner() {
        let height = 100;
        let mut tracker = tracker_with_seed(Some([0x33; 32]), None, 1_000);
        let block = block_with(
            height,
            vec![
                commitment([0x11; 32], [0xaa; 32], height, true),
                commitment([0x22; 32], [0xaa; 32], height, true),
            ],
        );

        tracker
            .recover_seed(|_| Ok::<_, String>(block.clone()))
            .expect("unanimity is an unambiguous fallback");
        assert_eq!(tracker.tip().winner_vrf_seed, Some([0xaa; 32]));
    }

    #[test]
    fn a_sortitionless_capture_without_its_predecessor_seed_is_refused() {
        let captured = CapturedSnapshot {
            block_height: 100,
            burn_header_hash: String::new(),
            burn_header_timestamp: 0,
            sortition_id: String::new(),
            consensus_hash: String::new(),
            sortition_hash: String::new(),
            total_burn: String::new(),
            pox_payouts: None,
            num_sortitions: None,
            sortition: Some(0),
            winning_block_txid: None,
            winner_vrf_seed: None,
            last_sortition_height: None,
            sortitions_below_window: Vec::new(),
            parent_sortition_id: None,
            miner_pk_hash: None,
            winning_stacks_block_hash: None,
        };

        assert!(matches!(
            seed_snapshot(&captured),
            Err(TrackerError::Seed(_))
        ));
    }

    #[test]
    fn a_tracker_with_no_effective_seed_never_advances_against_zero() {
        let mut tracker = tracker_with_seed(None, None, 1_000);
        let payouts = PayoutSchedule::new(
            RewardCycleSchedule::new(0, 10, None).expect("a schedule"),
            2,
        )
        .expect("payouts");

        assert!(matches!(
            tracker.advance(&block_with(101, Vec::new()), payouts),
            Err(TrackerError::Seed(_))
        ));
        assert_eq!(tracker.tip().bitcoin_height, 100);
    }

    #[test]
    fn a_commitment_parent_must_exist_on_the_canonical_burnchain() {
        let mut tracker = tracker(1_000);
        tracker.keys.register(
            90,
            1,
            nano_sortition::LeaderKeyRegistration {
                vrf_public_key: [0x11; 32],
                signing_key_hash: Some([0x22; 20]),
            },
        );
        let recipient = PoxAddress::Addr32 {
            mainnet: true,
            address_type: PoxAddressType32::P2tr,
            bytes: [0x33; 32],
        };
        let payouts = PayoutSchedule::new(
            RewardCycleSchedule::new(0, 20, Some(0)).expect("a schedule"),
            5,
        )
        .expect("payouts")
        .paying_waterfall_to(recipient);
        let operation = |height: u64, txid: u8, index: u32, parent_index: u16| BitcoinOperation {
            txid: [txid; 32],
            transaction_index: index,
            inputs: vec![BitcoinInput {
                txid: [txid.wrapping_sub(1); 32],
                output_index: 2,
            }],
            outputs: vec![BitcoinOutput {
                amount_sats: 40_000,
                recipient,
            }],
            kind: BitcoinOperationKind::LeaderBlockCommit {
                block_header_hash: [txid; 32],
                new_seed: [txid; 32],
                parent_block_height: u32::try_from(height - 1).expect("height fits u32"),
                parent_transaction_index: parent_index,
                key_block_height: 90,
                key_transaction_index: 1,
                memo: 0,
                parent_modulus: u8::try_from((height + 4) % 5).expect("modulus fits u8"),
            },
        };

        let parent = block_with(101, vec![operation(101, 1, 7, 1)]);
        let (_, parent_window) = tracker.accepted_block(&parent, payouts);
        assert_eq!(parent_window.commitments.len(), 1);

        let child = block_with(102, vec![operation(102, 2, 8, 7), operation(102, 3, 9, 42)]);
        let (accepted, window) = tracker.accepted_block(&child, payouts);
        assert_eq!(
            accepted
                .operations
                .iter()
                .map(|operation| operation.txid)
                .collect::<Vec<_>>(),
            vec![[2; 32]]
        );
        assert_eq!(window.commitments.len(), 1);

        tracker
            .retract_above(100)
            .expect("the abandoned commitment is forgotten");
        let (accepted, _) = tracker.accepted_block(&child, payouts);
        assert!(accepted.operations.is_empty());
    }

    #[test]
    fn a_saved_tracker_can_restart_one_burn_view_before_execution() {
        let mut tracker = a_chain();
        let payouts = PayoutSchedule::new(
            RewardCycleSchedule::new(0, 10, None).expect("a schedule"),
            2,
        )
        .expect("payouts");
        tracker
            .advance(&block_with(101, Vec::new()), payouts)
            .expect("derive the previous view");
        tracker
            .advance(&block_with(102, Vec::new()), payouts)
            .expect("derive the executed view");

        let directory = tempfile::tempdir().expect("saved tracker directory");
        tracker
            .save_standing_on(directory.path(), 101)
            .expect("save one view behind execution");
        let resumed = SortitionTracker::from_capture(directory.path()).expect("resume saved view");

        assert_eq!(resumed.tip().bitcoin_height, 101);
        assert_eq!(
            resumed.tip().consensus_hash,
            tracker
                .snapshot_at(101)
                .expect("the previous view is retained")
                .consensus_hash
        );
    }
}

#[cfg(test)]
mod anchor_tests {
    use nano_bitcoin::BitcoinBlock;
    use nano_sortition::{PayoutSchedule, PoxId, RewardCycleSchedule};

    use super::{CATCH_UP_LIMIT, tests::a_chain, tests::tracker_with_seed};

    /// A cycle length of ten from burn zero, so 111 and 121 both open one.
    fn payouts() -> PayoutSchedule {
        PayoutSchedule::new(
            RewardCycleSchedule::new(0, 10, None).expect("a schedule"),
            2,
        )
        .expect("a payout schedule")
    }

    fn empty_block(height: u64) -> BitcoinBlock {
        BitcoinBlock {
            height,
            hash: [u8::try_from(height % 251).unwrap_or(0); 32],
            timestamp: 0,
            operations: Vec::new(),
        }
    }

    /// A boundary adds one bit, and a block that opens no cycle adds none.
    ///
    /// The chain used to refuse a boundary outright, because whether a cycle
    /// selected an anchor block was a fact it could not derive. In epoch 4.0 there
    /// is nothing to derive: `load_nakamoto_reward_set` builds only
    /// `PoxAnchorBlockStatus::SelectedAndKnown`, so `make_next_pox_id` only ever
    /// extends with the present-anchor bit, and the alternative stacks-core has is
    /// to wait rather than to write a zero. See `advance`.
    ///
    /// What that is worth is checked where it can be: `pox_boundary` derives across
    /// the capture's five boundaries and compares every sortition identifier and
    /// consensus hash with what stacks-core wrote. This pins the arithmetic.
    #[test]
    fn a_boundary_adds_a_bit_and_nothing_else_does() {
        let mut chain = a_chain();
        let before = chain.tip().pox_id.as_consensus_bytes().len();
        assert!(payouts().starts_reward_cycle(111));
        assert!(payouts().starts_reward_cycle(121));

        // 111 and not 110: before the waterfall a cycle opens at offset 1.
        let mut opened = 0;
        for height in (chain.tip().bitcoin_height + 1)..=121 {
            if payouts().starts_reward_cycle(height) {
                opened += 1;
            }
            chain
                .advance(&empty_block(height), payouts())
                .expect("a boundary is crossed rather than refused");
        }
        assert!(opened >= 2, "the walk crosses more than one boundary");
        assert_eq!(
            chain.tip().pox_id,
            PoxId::from_bits(vec![true; before + opened]),
            "one bit per boundary crossed, no bit anywhere else, and every one of them set"
        );
    }

    #[test]
    fn two_bounded_batches_keep_the_view_execution_still_needs() {
        let mut chain = a_chain();
        let executed = chain.tip().bitcoin_height;
        chain.keep_from(executed);
        let target = executed + CATCH_UP_LIMIT * 2;

        let first = chain
            .catch_up(
                |height| Ok::<_, String>(empty_block(height)),
                target,
                payouts(),
                CATCH_UP_LIMIT,
            )
            .expect("the first bounded batch advances");
        let first_tip = chain.tip().consensus_hash;
        assert_eq!(first.advanced, CATCH_UP_LIMIT);
        assert_eq!(chain.tip().bitcoin_height, executed + CATCH_UP_LIMIT);

        let second = chain
            .catch_up(
                |height| Ok::<_, String>(empty_block(height)),
                target,
                payouts(),
                CATCH_UP_LIMIT,
            )
            .expect("the second bounded batch advances without a restart");
        assert_eq!(second.advanced, CATCH_UP_LIMIT);
        assert_eq!(chain.tip().bitcoin_height, target);
        assert_eq!(
            chain.height_of_consensus_hash(first_tip),
            Some(target - CATCH_UP_LIMIT)
        );
        assert!(
            chain.snapshot_at(executed).is_some(),
            "lookahead dropped the burn view execution still stands on"
        );
    }

    #[test]
    fn the_signer_view_advances_before_stacks_execution() {
        let mut chain = tracker_with_seed(Some([0x11; 32]), Some([9; 32]), 1_000);
        let executed = chain.tip().consensus_hash;
        chain
            .advance(&empty_block(101), payouts())
            .expect("derive a no-winner burn above execution");

        let sortitions = chain.recent_sortitions();
        assert!(sortitions.len() >= 2);
        assert_eq!(sortitions[0].bitcoin_height, 101);
        assert!(!sortitions[0].was_sortition);
        assert_eq!(sortitions[0].last_sortition_consensus_hash, Some(executed));
        assert_eq!(sortitions[1].bitcoin_height, 100);
        assert!(sortitions[1].was_sortition);

        let notifications = chain.burn_notifications_after(100);
        let [notification] = notifications.as_slice() else {
            panic!("the locally derived burn is announced before Stacks execution");
        };
        assert_eq!(notification.bitcoin_height, 101);
        assert_eq!(notification.bitcoin_block_hash.as_bytes(), &[101; 32]);
        assert_eq!(notification.parent_bitcoin_block_hash.as_bytes(), &[1; 32]);
        assert_eq!(notification.burned, 0);

        chain
            .advance(&empty_block(102), payouts())
            .expect("derive another no-winner burn");
        let sortitions = chain.recent_sortitions();
        assert_eq!(
            sortitions
                .iter()
                .take(3)
                .map(|sortition| sortition.bitcoin_height)
                .collect::<Vec<_>>(),
            vec![102, 101, 100]
        );
    }
}
