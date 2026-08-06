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
            .iter()
            .any(|hash| *hash == consensus_hash)
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
        while self.tip().bitcoin_height < target && walk.advanced < limit {
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
        Ok(walk)
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
            self.engine.prime(commitments);
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

    /// Whether the derived burn total is the one a signed header states.
    ///
    /// A Nakamoto header's `bitcoin_spent` is the burn view's running total and
    /// carries threshold signer weight, so this is the one check that puts the
    /// locally derived distribution against something the network signed. A
    /// disagreement means every consensus hash from here on is derived from a
    /// wrong total, so it is not a difference to log and continue past.
    #[must_use]
    pub fn agrees_with_header(&self, bitcoin_spent: u64) -> bool {
        self.tip().total_burn == bitcoin_spent
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
    sortition_id: String,
    consensus_hash: String,
    sortition_hash: String,
    total_burn: String,
    /// Whether this burn block elected anybody, as stacks-core's own column
    /// spells it. Absent in a chain saved before this field existed.
    #[serde(default)]
    sortition: Option<i64>,
    /// The winning VRF seed the next sampling has to mix — the most recent
    /// winner's, not necessarily this block's.
    ///
    /// A chain saved at a burn block that elected nobody cannot recover it: the
    /// commitments of such a block carry the seed of the tenure they were bidding
    /// for, and adopting that samples the next sortition against a seed nobody
    /// won. See [`nano_sortition::SnapshotChain::effective_winner_seed`].
    #[serde(default)]
    winner_vrf_seed: Option<String>,
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
        let write = |name: &str, bytes: Vec<u8>| -> Result<(), TrackerError> {
            let path = directory.join(name);
            let temporary = directory.join(format!("{name}.partial"));
            // Two files that have to agree, so neither is left half-written: a
            // torn history and a tip that does not end it seed nothing, and the
            // node would fall back to the capture and re-derive in silence.
            fs::write(&temporary, bytes).map_err(|error| TrackerError::Seed(error.to_string()))?;
            fs::rename(&temporary, &path).map_err(|error| TrackerError::Seed(error.to_string()))
        };
        let tip = self.tip();
        let snapshots = vec![CapturedSnapshot {
            block_height: tip.bitcoin_height,
            burn_header_hash: hex::encode(tip.bitcoin_header_hash.as_bytes()),
            sortition_id: hex::encode(tip.sortition_id.as_bytes()),
            consensus_hash: tip.consensus_hash.to_string(),
            sortition_hash: hex::encode(tip.sortition_hash.as_bytes()),
            total_burn: tip.total_burn.to_string(),
            sortition: Some(i64::from(tip.winner_txid.is_some())),
            // The one field a resumed chain cannot derive and must not guess.
            winner_vrf_seed: self
                .engine
                .snapshots()
                .effective_winner_seed()
                .map(hex::encode),
        }];
        let history = History {
            hashes: self
                .engine
                .snapshots()
                .history()
                .iter()
                .map(ToString::to_string)
                .collect(),
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
        tracker.load_leader_keys(directory)?;
        Ok(tracker)
    }
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
        sortition_id,
        parent_sortition_id: nano_primitives::SortitionId::from_bytes([0; 32]),
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
        winner_txid: None,
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
        winner_signing_key_hash: None,
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
