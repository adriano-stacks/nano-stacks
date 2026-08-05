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
    /// out. Returns how many burn blocks it advanced.
    ///
    /// Nothing is skipped: every burn block between here and there is read and
    /// snapshotted, because a consensus hash mixes the ones behind it and a
    /// height left out changes every hash from there on.
    pub fn catch_up<E: Display>(
        &mut self,
        mut block_at: impl FnMut(u64) -> Result<BitcoinBlock, E>,
        target: u64,
        payouts: PayoutSchedule,
        limit: u64,
    ) -> Result<u64, TrackerError> {
        if !self.primed {
            self.prime(&mut block_at, payouts)?;
        }
        let mut advanced = 0;
        while self.tip().bitcoin_height < target && advanced < limit {
            let height = self
                .tip()
                .bitcoin_height
                .checked_add(1)
                .ok_or(SortitionError::HeightOverflow)?;
            let block = block_at(height).map_err(|error| TrackerError::Bitcoin(error.to_string()))?;
            self.advance(&block, payouts)?;
            advanced += 1;
        }
        Ok(advanced)
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
    ) -> Result<(), TrackerError> {
        let tip = self.tip().bitcoin_height;
        let behind = u64::try_from(MINING_COMMITMENT_WINDOW).expect("window fits u64") - 1;
        for height in tip.saturating_sub(behind)..=tip {
            let block = block_at(height).map_err(|error| TrackerError::Bitcoin(error.to_string()))?;
            self.register_keys(&block);
            if height == tip && let Some(seed) = unanimous_winner_seed(&block) {
                self.engine.adopt_root_winner_seed(seed);
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
            Err(saved) => Self::from_capture(capture).map_err(|captured| {
                TrackerError::Seed(format!(
                    "neither the saved sortitions ({saved}) nor the capture ({captured}) \
                     can seed a chain"
                ))
            })?,
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
        // The seed's own winner is unknown, and only the *next* block's sampling
        // reads it — so the first sortition after a checkpoint is derived from a
        // zero previous seed and its winner is not this node's to trust. It is
        // reported for that reason rather than published.
        winner_vrf_seed: None,
        winner_vrf_public_key: None,
        pox_id,
    })
}
