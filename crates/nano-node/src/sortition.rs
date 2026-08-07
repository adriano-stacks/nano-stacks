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

use std::{fmt::Display, fs, path::Path};

use nano_bitcoin::{BitcoinBlock, BitcoinOperationKind};
use nano_primitives::ConsensusHash;
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
}

impl std::fmt::Display for TrackerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Seed(reason) => write!(formatter, "sortition seed: {reason}"),
            Self::Bitcoin(reason) => write!(formatter, "burnchain: {reason}"),
            Self::Sortition(error) => write!(formatter, "sortition: {error:?}"),
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
    /// Whether the six burn blocks the distribution weighs over have been read.
    ///
    /// They come from behind the seed, so they are not blocks the chain takes a
    /// snapshot of. Without them the window is short and the winner is not the
    /// one the network picked.
    primed: bool,
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
            primed: false,
        })
    }

    /// Read the consensus hashes a capture carries, oldest first.
    pub fn history_from(directory: &Path) -> Result<Vec<ConsensusHash>, TrackerError> {
        let bytes = fs::read(directory.join("consensus-hashes.json"))
            .map_err(|error| TrackerError::Seed(error.to_string()))?;
        let history: History =
            serde_json::from_slice(&bytes).map_err(|error| TrackerError::Seed(error.to_string()))?;
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
        self.engine
            .snapshots()
            .history()
            .contains(&consensus_hash)
    }

    /// The Bitcoin height a burn view sits at, from this chain's own history.
    ///
    /// A consensus hash names a burn block, and the history is that naming in
    /// order, so this is the reverse of [`Self::consensus_hash_at`] and the answer
    /// a node otherwise had to ask a peer for. Searched from the tip backwards
    /// because a follower's views arrive in ascending order and the newest is
    /// almost always the one being asked about; the walk is bounded by the same
    /// window a catch-up is, since a view further back than that belongs to a chain
    /// this node is not executing.
    ///
    /// `None` where the history does not hold it: the view is ahead of this chain,
    /// which one round of catching up may close, or it belongs to a burnchain this
    /// node is not on, which no amount of walking will.
    #[must_use]
    pub fn height_of_consensus_hash(&self, consensus_hash: ConsensusHash) -> Option<u64> {
        let history = self.engine.snapshots().history();
        let tip = self.tip().bitcoin_height;
        history
            .iter()
            .rev()
            .take(usize::try_from(CATCH_UP_LIMIT).unwrap_or(usize::MAX))
            .position(|hash| *hash == consensus_hash)
            .and_then(|back| tip.checked_sub(u64::try_from(back).ok()?))
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
        self.engine
            .snapshots()
            .last_sortition_at_or_below(parent)
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
        Ok(self.engine.retract_above(bitcoin_height)?)
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
        // A reward cycle opening adds a bit to the `PoX` history, and the
        // consensus hash mixes that history — so a chain that carried the seed's
        // vector across a boundary would derive a wrong hash for every block
        // after it. Whether the new cycle chose an anchor block is not something
        // this node can answer yet, and assuming it did is a guess the consensus
        // hash would silently encode.
        if payouts.starts_reward_cycle(block.height) {
            return Err(TrackerError::Seed(format!(
                "burn {} opens a reward cycle, which adds a bit to the PoX history \
                 the consensus hash mixes, and this node cannot yet say whether \
                 that cycle chose an anchor block",
                block.height
            )));
        }
        self.register_keys(block);
        let commitments =
            commitment_window_block(block, payouts.outputs_at(block.height), &self.keys);
        let txids = accepted_operation_txids(block);
        Ok(self.engine.append(
            block,
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
        self.walk(&mut block_at, payouts, limit.min(room), &mut walk, |_| false)?;
        let found = self.height_of_consensus_hash(view);
        Ok((found, walk))
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
            let block = block_at(height).map_err(|error| TrackerError::Bitcoin(error.to_string()))?;
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
            let block = block_at(height).map_err(|error| TrackerError::Bitcoin(error.to_string()))?;
            walk.reading += read.elapsed();
            walk.primed += 1;
            self.register_keys(&block);
            // Only where the seed does not already carry one: a chain this node
            // saved states the seed exactly, and the recovery below is a
            // capture's fallback that holds only because a capture whose seed
            // elected nobody is refused when it is loaded.
            if height == tip && self.tip().winner_vrf_seed.is_none() {
                match unanimous_winner_seed(&block) {
                    Some(seed) => {
                        self.engine.adopt_root_winner_seed(seed);
                    }
                    // The seed said this block elected somebody, so its
                    // commitments should have agreed on the seed that winner
                    // carried. When they do not there is no telling which of them
                    // won, and every sortition after this one is sampled against a
                    // zero seed — which names miners that did not win, and only
                    // shows up as their tenures' proofs being refused.
                    None => eprintln!(
                        "the sortition seed at burn {height} says its block elected somebody, \
                         but its commitments do not agree on the seed that winner carried, so \
                         this node cannot recover the seed the next sortition mixes and will \
                         sample against zero"
                    ),
                }
            }
            let commitments =
                commitment_window_block(&block, payouts.outputs_at(height), &self.keys);
            self.engine.prime(height, commitments);
        }
        self.primed = true;
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
        let path = directory.join(LEADER_KEY_FILE);
        let Ok(bytes) = fs::read(&path) else {
            return Ok(0);
        };
        let records: Vec<CapturedLeaderKey> =
            serde_json::from_slice(&bytes).map_err(|error| {
                TrackerError::Seed(format!("{}: {error}", path.display()))
            })?;
        let loaded = records.len();
        for record in records {
            self.keys
                .register(record.block_height, record.vtxindex, record.registration()?);
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
fn unanimous_winner_seed(block: &BitcoinBlock) -> Option<[u8; 32]> {
    let mut seeds = block.operations.iter().filter_map(|operation| {
        match &operation.kind {
            BitcoinOperationKind::LeaderBlockCommit {
                new_seed,
                parent_modulus,
                ..
            } if commitment_is_on_time(*parent_modulus, block.height) => Some(*new_seed),
            _ => None,
        }
    });
    let first = seeds.next()?;
    seeds.all(|seed| seed == first).then_some(first)
}

/// Where a checkpoint's leader-key registry is written down.
///
/// Beside the snapshots and the consensus hashes, because it answers the same
/// kind of question they do: what the burnchain below this node's window said.
pub const LEADER_KEY_FILE: &str = "leader-keys.json";

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
#[derive(Debug, Deserialize, Serialize)]
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
        let mut tracker = match Self::from_capture(state) {
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
                Self::from_capture(capture).map_err(|captured| {
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
        // A chain that has nowhere to be written down is re-derived from the
        // checkpoint on the next start, one Bitcoin block download per burn block,
        // and the only sign of it is a line in a log. Making the directory is
        // cheaper than that, and the failure is reported rather than swallowed.
        fs::create_dir_all(directory).map_err(|error| TrackerError::Seed(error.to_string()))?;
        let write = |name: &str, bytes: Vec<u8>| -> Result<(), TrackerError> {
            let path = directory.join(name);
            let temporary = directory.join(format!("{name}.partial"));
            // Two files that have to agree, so neither is left half-written: a
            // torn history and a tip that does not end it seed nothing, and the
            // node would fall back to the capture and re-derive in silence.
            fs::write(&temporary, bytes).map_err(|error| TrackerError::Seed(error.to_string()))?;
            fs::rename(&temporary, &path).map_err(|error| TrackerError::Seed(error.to_string()))
        };
        // The tip when execution has caught up to it, and never above what the
        // history can be truncated to.
        let tip = self
            .snapshot_at(bitcoin_height.min(self.tip().bitcoin_height))
            .unwrap_or_else(|| self.tip());
        let snapshots = vec![CapturedSnapshot {
            block_height: tip.bitcoin_height,
            burn_header_hash: hex::encode(tip.bitcoin_header_hash.as_bytes()),
            sortition_id: hex::encode(tip.sortition_id.as_bytes()),
            consensus_hash: tip.consensus_hash.to_string(),
            burn_header_timestamp: tip.bitcoin_timestamp,
            sortition_hash: hex::encode(tip.sortition_hash.as_bytes()),
            total_burn: tip.total_burn.to_string(),
            sortition: Some(i64::from(tip.winner_txid.is_some())),
            winning_block_txid: tip.winner_txid.map(hex::encode),
            // The one field a resumed chain cannot derive and must not guess.
            winner_vrf_seed: self
                .engine
                .snapshots()
                .effective_winner_seed()
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
            sortitions_below_window: self
                .engine
                .snapshots()
                .sortitions_below_window()
                .to_vec(),
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
                let ahead = usize::try_from(
                    self.tip().bitcoin_height.saturating_sub(tip.bitcoin_height),
                )
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
                memo: registration.signing_key_hash.map(hex::encode).unwrap_or_default(),
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
        write(
            "snapshots.json",
            serde_json::to_vec(&snapshots).map_err(|error| TrackerError::Seed(error.to_string()))?,
        )
    }

    pub fn from_capture(directory: &Path) -> Result<Self, TrackerError> {
        let bytes = fs::read(directory.join("snapshots.json"))
            .map_err(|error| TrackerError::Seed(error.to_string()))?;
        let snapshots: Vec<CapturedSnapshot> =
            serde_json::from_slice(&bytes).map_err(|error| TrackerError::Seed(error.to_string()))?;
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
        let mut tracker = Self::new(seed_snapshot(seed)?, history)?;
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
        tracker.load_leader_keys(directory)?;
        Ok(tracker)
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
    <[u8; 32]>::try_from(bytes.as_slice()).map(Some).map_err(|_| {
        TrackerError::Seed("the winning stacks block hash is not 32 bytes".to_owned())
    })
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
        // Both `None` for the seed itself, and neither is ever read: a seed is
        // taken as given and no tenure is validated against it. Every snapshot
        // after it resolves them from the carried registry.
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
        pox_id,
    })
}

#[cfg(test)]
mod tests {
    use nano_primitives::ConsensusHash;
    use nano_sortition::{OpsHash, PoxId, SortitionHash, SortitionSnapshot};
    use nano_sync::BurnView as _;

    use super::SortitionTracker;

    /// A chain standing on one snapshot, with one hash behind it.
    fn tracker(total_burn: u64) -> SortitionTracker {
        let behind = ConsensusHash::from_bytes([0xbe; 20]);
        let seed = SortitionSnapshot {
            bitcoin_height: 100,
            bitcoin_header_hash: nano_primitives::BitcoinHeaderHash::from_bytes([1; 32]),
            bitcoin_timestamp: 0,
            sortition_id: nano_primitives::SortitionId::from_bytes([2; 32]),
            parent_sortition_id: nano_primitives::SortitionId::from_bytes([3; 32]),
            operations_hash: OpsHash::from_txids(&[]),
            consensus_hash: ConsensusHash::from_bytes([0x7f; 20]),
            total_burn,
            sortition_hash: SortitionHash::from_bytes([4; 32]),
            winner_txid: None,
            winner_vrf_seed: None,
            winner_vrf_public_key: None,
            winner_signing_key_hash: None,
            committed_block_hash: None,
            parent_bitcoin_height: None,
            burn_spends: None,
            pox_id: PoxId::initial(),
        };
        let history = vec![behind, seed.consensus_hash];
        SortitionTracker::new(seed, history).expect("the history ends at the seed")
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
}
