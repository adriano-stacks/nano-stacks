#![forbid(unsafe_code)]

pub(crate) mod carryover;

use std::{collections::HashMap, fmt};

use nano_bitcoin::BitcoinBlock;
use nano_primitives::{
    BitcoinHeaderHash, ConsensusHash, SortitionId, Uint256, Uint512, hash160, sha256, sha512_256,
};

const SYSTEM_FORK_SET_VERSION: [u8; 4] = [23, 0, 0, 0];

/// The Bitcoin blocks a sortition weighs mining commitments over.
pub const MINING_COMMITMENT_WINDOW: usize = 6;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpsHash([u8; 32]);

impl OpsHash {
    #[must_use]
    pub fn from_txids(txids: &[[u8; 32]]) -> Self {
        let mut bytes = Vec::with_capacity(txids.len() * 32);
        for txid in txids {
            bytes.extend_from_slice(txid);
        }
        Self(*sha256(&bytes).as_bytes())
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SortitionHash([u8; 32]);

impl SortitionHash {
    #[must_use]
    pub const fn initial() -> Self {
        Self([0; 32])
    }

    #[must_use]
    pub fn mix_bitcoin_header(self, header: BitcoinHeaderHash) -> Self {
        let mut bytes = [0; 64];
        bytes[..32].copy_from_slice(&self.0);
        bytes[32..].copy_from_slice(header.as_bytes());
        Self(*sha256(&bytes).as_bytes())
    }

    #[must_use]
    pub fn mix_vrf_seed(self, seed: [u8; 32]) -> Self {
        let mut bytes = [0; 64];
        bytes[..32].copy_from_slice(&self.0);
        bytes[32..].copy_from_slice(&seed);
        Self(*sha256(&bytes).as_bytes())
    }

    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MiningCommitment {
    pub txid: [u8; 32],
    pub spent_txid: [u8; 32],
    pub spent_output: u32,
    pub burn_sats: u64,
    pub vrf_seed: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SortitionWinner {
    pub txid: [u8; 32],
    pub vrf_seed: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissedCommitment {
    pub txid: [u8; 32],
    pub spent_txid: [u8; 32],
    pub spent_output: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitmentWindowBlock {
    pub commitments: Vec<MiningCommitment>,
    pub missed_commitments: Vec<MissedCommitment>,
    pub requires_single_commit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BurnSample {
    pub candidate: MiningCommitment,
    pub burn_sats: u64,
    pub median_burn_sats: u64,
    pub frequency: u8,
    pub range_start: Uint256,
    pub range_end: Uint256,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommitmentBurnStatistics {
    pub block_burn: u64,
    pub window_median_burn: u64,
}

pub fn commitment_burn_statistics(
    window: &[CommitmentWindowBlock],
) -> Result<CommitmentBurnStatistics, SortitionError> {
    let Some(latest) = window.last() else {
        return Err(SortitionError::EmptyCommitmentWindow);
    };
    let mut block_burns = window
        .iter()
        .map(|block| {
            block
                .commitments
                .iter()
                .try_fold(0_u64, |total, commitment| {
                    total
                        .checked_add(commitment.burn_sats)
                        .ok_or(SortitionError::BurnOverflow)
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    block_burns.sort_unstable();
    let middle = block_burns.len() / 2;
    let window_median_burn = if block_burns.len() % 2 == 0 {
        u64::try_from(u128::midpoint(
            u128::from(block_burns[middle - 1]),
            u128::from(block_burns[middle]),
        ))
        .expect("median of two u64 values fits u64")
    } else {
        block_burns[middle]
    };
    let block_burn = latest
        .commitments
        .iter()
        .try_fold(0_u64, |total, commitment| {
            total
                .checked_add(commitment.burn_sats)
                .ok_or(SortitionError::BurnOverflow)
        })?;
    Ok(CommitmentBurnStatistics {
        block_burn,
        window_median_burn,
    })
}

pub fn commitment_distribution(
    window: &[CommitmentWindowBlock],
) -> Result<Vec<BurnSample>, SortitionError> {
    let Some((latest, earlier)) = window.split_last() else {
        return Err(SortitionError::EmptyCommitmentWindow);
    };
    let window_len = u8::try_from(window.len()).map_err(|_| SortitionError::WindowTooLong)?;
    let mut linked = latest
        .commitments
        .iter()
        .cloned()
        .map(|commitment| vec![Some(Link::Commitment(commitment))])
        .collect::<Vec<_>>();

    for block in earlier.iter().rev() {
        let expected_output = if block.requires_single_commit { 2 } else { 3 };
        let mut commitments = block
            .commitments
            .iter()
            .cloned()
            .map(|commitment| (commitment.txid, commitment))
            .collect::<HashMap<_, _>>();
        let mut missed = block
            .missed_commitments
            .iter()
            .cloned()
            .map(|commitment| (commitment.txid, commitment))
            .collect::<HashMap<_, _>>();
        for chain in &mut linked {
            let Some(last) = chain.iter().rev().find_map(Option::as_ref) else {
                return Err(SortitionError::InvalidCommitmentWindow);
            };
            let (spent_txid, spent_output) = last.spent();
            if spent_output != expected_output {
                chain.push(None);
            } else if let Some(commitment) = commitments.remove(&spent_txid) {
                chain.push(Some(Link::Commitment(commitment)));
            } else if let Some(commitment) = missed.remove(&spent_txid) {
                chain.push(Some(Link::Missed(commitment)));
            } else {
                chain.push(None);
            }
        }
    }

    let mut samples = linked
        .into_iter()
        .map(|chain| make_burn_sample(&chain, window_len))
        .collect::<Result<Vec<_>, _>>()?;
    assign_ranges(&mut samples)?;
    Ok(samples)
}

#[must_use]
pub fn select_winner(
    distribution: &[BurnSample],
    sortition_hash: SortitionHash,
    previous_vrf_seed: [u8; 32],
) -> Option<usize> {
    if distribution.is_empty() {
        return None;
    }
    if distribution.len() == 1 {
        return Some(0);
    }
    let point =
        Uint256::from_little_endian(sortition_hash.mix_vrf_seed(previous_vrf_seed).as_bytes());
    distribution
        .iter()
        .position(|sample| sample.range_start <= point && point < sample.range_end)
}

#[must_use]
pub fn select_epoch4_winner(
    distribution: &[BurnSample],
    sampled_window_len: usize,
    sortition_hash: SortitionHash,
    previous_vrf_seed: [u8; 32],
    block_burn: u64,
    window_median_burn: u64,
) -> Option<usize> {
    let candidate = select_winner(distribution, sortition_hash, previous_vrf_seed)?;
    let minimum_frequency = 3_usize.min(sampled_window_len);
    if usize::from(distribution[candidate].frequency) < minimum_frequency {
        return None;
    }
    let Some(null_range_end) = carryover::null_miner_probability(block_burn, window_median_burn)
    else {
        return Some(candidate);
    };
    let point =
        Uint256::from_little_endian(sortition_hash.mix_vrf_seed(previous_vrf_seed).as_bytes());
    (point >= null_range_end).then_some(candidate)
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Link {
    Commitment(MiningCommitment),
    Missed(MissedCommitment),
}

impl Link {
    const fn burn_sats(&self) -> u64 {
        match self {
            Self::Commitment(commitment) => commitment.burn_sats,
            Self::Missed(_) => 1,
        }
    }

    const fn spent(&self) -> ([u8; 32], u32) {
        match self {
            Self::Commitment(commitment) => (commitment.spent_txid, commitment.spent_output),
            Self::Missed(commitment) => (commitment.spent_txid, commitment.spent_output),
        }
    }
}

fn make_burn_sample(chain: &[Option<Link>], window_len: u8) -> Result<BurnSample, SortitionError> {
    let Some(Some(Link::Commitment(candidate))) = chain.first() else {
        return Err(SortitionError::InvalidCommitmentWindow);
    };
    if chain.len() != usize::from(window_len) {
        return Err(SortitionError::InvalidCommitmentWindow);
    }
    let burns = chain
        .iter()
        .map(|link| link.as_ref().map_or(1, Link::burn_sats))
        .collect::<Vec<_>>();
    let mut sorted = burns.clone();
    sorted.sort_unstable();
    let middle = sorted.len() / 2;
    let median_burn_sats = if sorted.len() % 2 == 0 {
        u64::try_from(u128::midpoint(
            u128::from(sorted[middle - 1]),
            u128::from(sorted[middle]),
        ))
        .expect("median of two u64 values fits u64")
    } else {
        sorted[middle]
    };
    Ok(BurnSample {
        candidate: candidate.clone(),
        burn_sats: burns[0].min(median_burn_sats),
        median_burn_sats,
        frequency: u8::try_from(chain.iter().filter(|link| link.is_some()).count())
            .expect("commitment window fits u8"),
        range_start: Uint256::zero(),
        range_end: Uint256::zero(),
    })
}

fn assign_ranges(samples: &mut [BurnSample]) -> Result<(), SortitionError> {
    if samples.is_empty() {
        return Ok(());
    }
    if samples.len() == 1 {
        samples[0].range_end = Uint256::MAX;
        return Ok(());
    }
    let total = samples.iter().try_fold(0_u64, |total, sample| {
        total
            .checked_add(sample.burn_sats)
            .ok_or(SortitionError::BurnOverflow)
    })?;
    if total == 0 {
        return Err(SortitionError::ZeroBurnDistribution);
    }
    let mut accumulated = 0_u64;
    let mut range_end = Uint256::zero();
    for sample in samples {
        sample.range_start = range_end;
        accumulated = accumulated
            .checked_add(sample.burn_sats)
            .ok_or(SortitionError::BurnOverflow)?;
        let scaled = Uint512::from(Uint256::MAX) * Uint512::from(accumulated);
        range_end = Uint256::try_from(scaled / Uint512::from(total))
            .expect("scaled sortition range fits Uint256");
        sample.range_end = range_end;
    }
    Ok(())
}

/// The reward-cycle fork history committed to by a consensus hash.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PoxId(Vec<bool>);

impl PoxId {
    #[must_use]
    pub fn initial() -> Self {
        Self(vec![true])
    }

    #[must_use]
    pub const fn from_bits(bits: Vec<bool>) -> Self {
        Self(bits)
    }

    pub fn extend_with_anchor(&mut self, present: bool) {
        self.0.push(present);
    }

    #[must_use]
    pub fn bits(&self) -> &[bool] {
        &self.0
    }

    #[must_use]
    pub fn as_consensus_bytes(&self) -> Vec<u8> {
        self.0
            .iter()
            .map(|present| if *present { b'1' } else { b'0' })
            .collect()
    }
}

/// Bitcoin reward-cycle rules that determine when the `PoX` history advances.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RewardCycleSchedule {
    first_bitcoin_height: u64,
    reward_cycle_length: u64,
    first_waterfall_height: Option<u64>,
}

impl RewardCycleSchedule {
    pub const fn new(
        first_bitcoin_height: u64,
        reward_cycle_length: u64,
        first_waterfall_height: Option<u64>,
    ) -> Result<Self, SortitionError> {
        if reward_cycle_length == 0 {
            return Err(SortitionError::ZeroRewardCycleLength);
        }
        Ok(Self {
            first_bitcoin_height,
            reward_cycle_length,
            first_waterfall_height,
        })
    }

    fn starts_at(&self, bitcoin_height: u64) -> bool {
        let relative_height = bitcoin_height.saturating_sub(self.first_bitcoin_height);
        if self
            .first_waterfall_height
            .is_some_and(|height| bitcoin_height >= height)
        {
            relative_height % self.reward_cycle_length == 0
        } else {
            relative_height % self.reward_cycle_length == 1
        }
    }
}

/// Tracks the `PoX` history committed to by each Bitcoin snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PoxIdTracker {
    schedule: RewardCycleSchedule,
    pox_id: PoxId,
    bitcoin_height: u64,
}

impl PoxIdTracker {
    #[must_use]
    pub fn new(schedule: RewardCycleSchedule) -> Self {
        Self {
            bitcoin_height: schedule.first_bitcoin_height,
            schedule,
            pox_id: PoxId::initial(),
        }
    }

    #[must_use]
    pub const fn pox_id(&self) -> &PoxId {
        &self.pox_id
    }

    pub fn advance(
        &mut self,
        bitcoin_height: u64,
        anchor_known: bool,
    ) -> Result<&PoxId, SortitionError> {
        let expected = self
            .bitcoin_height
            .checked_add(1)
            .ok_or(SortitionError::HeightOverflow)?;
        if bitcoin_height != expected {
            return Err(SortitionError::UnexpectedHeight {
                expected,
                actual: bitcoin_height,
            });
        }
        if self.schedule.starts_at(bitcoin_height) {
            self.pox_id.extend_with_anchor(anchor_known);
        }
        self.bitcoin_height = bitcoin_height;
        Ok(&self.pox_id)
    }

    /// Rewind to a Bitcoin height, dropping the anchor bits the blocks above it
    /// contributed.
    pub fn retract_to(&mut self, bitcoin_height: u64) -> Result<&PoxId, SortitionError> {
        if bitcoin_height > self.bitcoin_height {
            return Err(SortitionError::UnexpectedHeight {
                expected: self.bitcoin_height,
                actual: bitcoin_height,
            });
        }
        let retracted_starts = (bitcoin_height + 1..=self.bitcoin_height)
            .filter(|height| self.schedule.starts_at(*height))
            .count();
        self.pox_id
            .0
            .truncate(self.pox_id.0.len().saturating_sub(retracted_starts));
        self.bitcoin_height = bitcoin_height;
        Ok(&self.pox_id)
    }
}

impl fmt::Display for PoxId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .write_str(std::str::from_utf8(&self.as_consensus_bytes()).expect("PoX ID is ASCII"))
    }
}

/// The consensus context derived from a Bitcoin block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SortitionSnapshot {
    pub bitcoin_height: u64,
    pub bitcoin_header_hash: BitcoinHeaderHash,
    pub sortition_id: SortitionId,
    pub parent_sortition_id: SortitionId,
    pub operations_hash: OpsHash,
    pub consensus_hash: ConsensusHash,
    pub total_burn: u64,
    pub sortition_hash: SortitionHash,
    pub winner_txid: Option<[u8; 32]>,
    pub winner_vrf_seed: Option<[u8; 32]>,
    pub pox_id: PoxId,
}

impl SortitionSnapshot {
    #[must_use]
    pub fn genesis(bitcoin_height: u64, bitcoin_header_hash: BitcoinHeaderHash) -> Self {
        Self {
            bitcoin_height,
            bitcoin_header_hash,
            sortition_id: SortitionId::from_bytes(*bitcoin_header_hash.as_bytes()),
            parent_sortition_id: SortitionId::from_bytes(*bitcoin_header_hash.as_bytes()),
            operations_hash: OpsHash([0; 32]),
            consensus_hash: ConsensusHash::from_bytes([0; 20]),
            total_burn: 0,
            sortition_hash: SortitionHash::initial(),
            winner_txid: None,
            winner_vrf_seed: None,
            pox_id: PoxId::initial(),
        }
    }
}

/// Where Bitcoin's chain and a snapshot chain part company.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fork {
    /// Bitcoin still holds every block the chain took a snapshot of.
    Canonical,
    /// The snapshots above this Bitcoin height are no longer canonical.
    Above(u64),
    /// The reorganization reaches the chain's root, which cannot be retracted.
    ///
    /// The root is where the chain was started from — a checkpoint, or the
    /// first Bitcoin block a node ever saw — so nothing local can tell what
    /// replaces it. The node has to be restarted from a checkpoint Bitcoin
    /// agrees with.
    BeyondChainRoot { root_bitcoin_height: u64 },
}

/// The sortitions a Bitcoin reorganization retracted, and where to resume.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SortitionReorg {
    /// The deepest snapshot Bitcoin still agrees with.
    pub valid_ancestor: SortitionSnapshot,
    /// The retracted snapshots, oldest first.
    pub retracted: Vec<SortitionSnapshot>,
}

impl SortitionReorg {
    /// The number of snapshots the reorganization retracted.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.retracted.len()
    }

    /// Whether the reorganization retracted nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.retracted.is_empty()
    }

    /// The first Bitcoin height to read again.
    ///
    /// This is also the height to invalidate the `PreStx` window from, with
    /// [`nano_bitcoin::PreStxCache::invalidate_from`].
    #[must_use]
    pub const fn resume_bitcoin_height(&self) -> u64 {
        self.valid_ancestor.bitcoin_height.saturating_add(1)
    }

    /// The consensus hashes whose Stacks blocks left the canonical chain.
    ///
    /// A retracted sortition takes its tenure with it. Unwinding the Clarity
    /// state those tenures wrote is not this crate's to do — `nano-chainstate`
    /// owns it — so this is the signal it acts on: discard every Stacks block
    /// whose `consensus_hash` appears here, and re-execute from the last block
    /// under [`Self::valid_ancestor`]'s consensus hash.
    #[must_use]
    pub fn invalidated_consensus_hashes(&self) -> Vec<ConsensusHash> {
        self.retracted
            .iter()
            .map(|snapshot| snapshot.consensus_hash)
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotChain {
    snapshots: Vec<SortitionSnapshot>,
}

impl SnapshotChain {
    #[must_use]
    pub fn new(genesis: SortitionSnapshot) -> Self {
        Self {
            snapshots: vec![genesis],
        }
    }

    #[must_use]
    pub fn tip(&self) -> &SortitionSnapshot {
        self.snapshots.last().expect("snapshot chain has genesis")
    }

    #[must_use]
    pub fn snapshots(&self) -> &[SortitionSnapshot] {
        &self.snapshots
    }

    /// Find the deepest snapshot Bitcoin still agrees with.
    ///
    /// `canonical_hash` reports the header hash Bitcoin holds at a height now;
    /// `nano_bitcoin::BitcoinRpcSource::block_hash_at` is what a node passes.
    /// The walk stops at the first agreement, so a chain that did not
    /// reorganize costs one lookup.
    pub fn find_fork<E>(
        &self,
        mut canonical_hash: impl FnMut(u64) -> Result<[u8; 32], E>,
    ) -> Result<Fork, E> {
        for snapshot in self.snapshots.iter().rev() {
            if canonical_hash(snapshot.bitcoin_height)? == *snapshot.bitcoin_header_hash.as_bytes()
            {
                return Ok(if snapshot.bitcoin_height == self.tip().bitcoin_height {
                    Fork::Canonical
                } else {
                    Fork::Above(snapshot.bitcoin_height)
                });
            }
        }
        Ok(Fork::BeyondChainRoot {
            root_bitcoin_height: self.root().bitcoin_height,
        })
    }

    /// Retract every snapshot above a Bitcoin height.
    ///
    /// Snapshots are contiguous in Bitcoin height, so the height alone fixes
    /// the split. Retracting the chain's root is refused: see
    /// [`Fork::BeyondChainRoot`].
    pub fn retract_above(&mut self, bitcoin_height: u64) -> Result<SortitionReorg, SortitionError> {
        let root_bitcoin_height = self.root().bitcoin_height;
        let above_root = bitcoin_height.checked_sub(root_bitcoin_height).ok_or(
            SortitionError::ReorgBeyondChainRoot {
                root_bitcoin_height,
            },
        )?;
        let kept = usize::try_from(above_root)
            .unwrap_or(usize::MAX)
            .saturating_add(1);
        let retracted = if kept < self.snapshots.len() {
            self.snapshots.split_off(kept)
        } else {
            Vec::new()
        };
        Ok(SortitionReorg {
            valid_ancestor: self.tip().clone(),
            retracted,
        })
    }

    fn root(&self) -> &SortitionSnapshot {
        self.snapshots.first().expect("snapshot chain has genesis")
    }

    pub fn append(
        &mut self,
        block: &BitcoinBlock,
        total_burn: u64,
        pox_id: PoxId,
    ) -> Result<&SortitionSnapshot, SortitionError> {
        self.append_with_winner(block, total_burn, pox_id, None)
    }

    pub fn append_with_winner(
        &mut self,
        block: &BitcoinBlock,
        total_burn: u64,
        pox_id: PoxId,
        winner: Option<SortitionWinner>,
    ) -> Result<&SortitionSnapshot, SortitionError> {
        let operation_txids = block
            .operations
            .iter()
            .map(|operation| operation.txid)
            .collect::<Vec<_>>();
        self.append_with_operations(block, &operation_txids, total_burn, pox_id, winner)
    }

    pub fn append_with_operations(
        &mut self,
        block: &BitcoinBlock,
        operation_txids: &[[u8; 32]],
        total_burn: u64,
        pox_id: PoxId,
        winner: Option<SortitionWinner>,
    ) -> Result<&SortitionSnapshot, SortitionError> {
        let parent = self.tip();
        let expected_height = parent
            .bitcoin_height
            .checked_add(1)
            .ok_or(SortitionError::HeightOverflow)?;
        if block.height != expected_height {
            return Err(SortitionError::UnexpectedHeight {
                expected: expected_height,
                actual: block.height,
            });
        }

        let operations_hash = OpsHash::from_txids(operation_txids);
        let bitcoin_header_hash = BitcoinHeaderHash::from_bytes(block.hash);
        let consensus_hash = consensus_hash(
            bitcoin_header_hash,
            operations_hash,
            total_burn,
            &self.previous_consensus_hashes(),
            &pox_id,
        );
        let sortition_hash = parent
            .sortition_hash
            .mix_bitcoin_header(bitcoin_header_hash);
        let snapshot = SortitionSnapshot {
            bitcoin_height: block.height,
            bitcoin_header_hash,
            sortition_id: sortition_id(bitcoin_header_hash, &pox_id),
            parent_sortition_id: parent.sortition_id,
            operations_hash,
            consensus_hash,
            total_burn,
            sortition_hash: winner.map_or(sortition_hash, |winner| {
                sortition_hash.mix_vrf_seed(winner.vrf_seed)
            }),
            winner_txid: winner.map(|winner| winner.txid),
            winner_vrf_seed: winner.map(|winner| winner.vrf_seed),
            pox_id,
        };
        self.snapshots.push(snapshot);
        Ok(self.tip())
    }

    fn previous_consensus_hashes(&self) -> Vec<ConsensusHash> {
        let parent_index = self.snapshots.len() - 1;
        let mut hashes = Vec::new();
        let mut exponent = 0_u32;
        while exponent < 64 {
            let offset = (1_usize << exponent).saturating_sub(1);
            let Some(index) = parent_index.checked_sub(offset) else {
                break;
            };
            hashes.push(self.snapshots[index].consensus_hash);
            exponent += 1;
        }
        hashes
    }
}

fn sortition_id(bitcoin_header_hash: BitcoinHeaderHash, pox_id: &PoxId) -> SortitionId {
    let mut bytes = Vec::with_capacity(bitcoin_header_hash.as_bytes().len() + pox_id.0.len());
    bytes.extend_from_slice(bitcoin_header_hash.as_bytes());
    bytes.extend_from_slice(&pox_id.as_consensus_bytes());
    SortitionId::from_bytes(*sha512_256(&bytes).as_bytes())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SortitionEngine {
    snapshots: SnapshotChain,
    commitment_window: Vec<CommitmentWindowBlock>,
}

impl SortitionEngine {
    #[must_use]
    pub fn new(genesis: SortitionSnapshot) -> Self {
        Self {
            snapshots: SnapshotChain::new(genesis),
            commitment_window: Vec::new(),
        }
    }

    #[must_use]
    pub const fn snapshots(&self) -> &SnapshotChain {
        &self.snapshots
    }

    #[must_use]
    pub fn commitment_window(&self) -> &[CommitmentWindowBlock] {
        &self.commitment_window
    }

    /// Retract every sortition above a Bitcoin height, and the commitment
    /// window entries the retracted blocks contributed.
    ///
    /// The window holds one entry per snapshot appended, so a retraction drops
    /// that many entries from its end and the entries below the fork point
    /// survive to weigh the replacement branch. A reorganization deeper than
    /// the window would empty it, and the blocks that would refill it are below
    /// the fork point and never read again — the sortitions that followed could
    /// not be recomputed, so the retraction is refused rather than performed
    /// against a short window.
    pub fn retract_above(&mut self, bitcoin_height: u64) -> Result<SortitionReorg, SortitionError> {
        let depth = usize::try_from(
            self.snapshots
                .tip()
                .bitcoin_height
                .saturating_sub(bitcoin_height),
        )
        .unwrap_or(usize::MAX);
        if depth > MINING_COMMITMENT_WINDOW {
            return Err(SortitionError::ReorgTooDeep {
                depth,
                limit: MINING_COMMITMENT_WINDOW,
            });
        }
        let reorg = self.snapshots.retract_above(bitcoin_height)?;
        self.commitment_window
            .truncate(self.commitment_window.len().saturating_sub(reorg.depth()));
        Ok(reorg)
    }

    pub fn append(
        &mut self,
        block: &BitcoinBlock,
        accepted_operation_txids: &[[u8; 32]],
        commitments: CommitmentWindowBlock,
        pox_id: PoxId,
    ) -> Result<&SortitionSnapshot, SortitionError> {
        let mut window = self.commitment_window.clone();
        window.push(commitments);
        if window.len() > MINING_COMMITMENT_WINDOW {
            window.remove(0);
        }
        let statistics = commitment_burn_statistics(&window)?;
        let distribution = commitment_distribution(&window)?;
        let next_sortition_hash = self
            .snapshots
            .tip()
            .sortition_hash
            .mix_bitcoin_header(BitcoinHeaderHash::from_bytes(block.hash));
        let previous_vrf_seed = self
            .snapshots
            .snapshots()
            .iter()
            .rev()
            .find_map(|snapshot| snapshot.winner_vrf_seed)
            .unwrap_or([0; 32]);
        let winner = (statistics.block_burn != 0)
            .then(|| {
                select_epoch4_winner(
                    &distribution,
                    window.len(),
                    next_sortition_hash,
                    previous_vrf_seed,
                    statistics.block_burn,
                    statistics.window_median_burn,
                )
            })
            .flatten();
        let winner = winner.and_then(|index| {
            self.snapshots
                .tip()
                .total_burn
                .checked_add(statistics.block_burn)
                .map(|total_burn| {
                    (
                        total_burn,
                        SortitionWinner {
                            txid: distribution[index].candidate.txid,
                            vrf_seed: distribution[index].candidate.vrf_seed,
                        },
                    )
                })
        });
        let (total_burn, winner) = winner.map_or_else(
            || (self.snapshots.tip().total_burn, None),
            |(total_burn, winner)| (total_burn, Some(winner)),
        );
        self.commitment_window = window;
        self.snapshots.append_with_operations(
            block,
            accepted_operation_txids,
            total_burn,
            pox_id,
            winner,
        )
    }
}

fn consensus_hash(
    bitcoin_header_hash: BitcoinHeaderHash,
    operations_hash: OpsHash,
    total_burn: u64,
    previous_hashes: &[ConsensusHash],
    pox_id: &PoxId,
) -> ConsensusHash {
    let mut bytes = Vec::with_capacity(
        SYSTEM_FORK_SET_VERSION.len()
            + bitcoin_header_hash.as_bytes().len()
            + operations_hash.as_bytes().len()
            + std::mem::size_of::<u64>()
            + pox_id.0.len()
            + previous_hashes.len() * 20,
    );
    bytes.extend_from_slice(&SYSTEM_FORK_SET_VERSION);
    bytes.extend_from_slice(bitcoin_header_hash.as_bytes());
    bytes.extend_from_slice(operations_hash.as_bytes());
    bytes.extend_from_slice(&total_burn.to_be_bytes());
    bytes.extend_from_slice(&pox_id.as_consensus_bytes());
    for hash in previous_hashes {
        bytes.extend_from_slice(hash.as_bytes());
    }
    ConsensusHash::from_bytes(*hash160(&bytes).as_bytes())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SortitionError {
    EmptyCommitmentWindow,
    InvalidCommitmentWindow,
    WindowTooLong,
    BurnOverflow,
    ZeroBurnDistribution,
    ZeroRewardCycleLength,
    HeightOverflow,
    UnexpectedHeight { expected: u64, actual: u64 },
    ReorgBeyondChainRoot { root_bitcoin_height: u64 },
    ReorgTooDeep { depth: usize, limit: usize },
}

impl fmt::Display for SortitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyCommitmentWindow => formatter.write_str("commitment window is empty"),
            Self::InvalidCommitmentWindow => formatter.write_str("commitment window is invalid"),
            Self::WindowTooLong => formatter.write_str("commitment window exceeds 255 blocks"),
            Self::BurnOverflow => formatter.write_str("commitment burn amount overflow"),
            Self::ZeroBurnDistribution => {
                formatter.write_str("commitment distribution has no burn")
            }
            Self::ZeroRewardCycleLength => {
                formatter.write_str("reward cycle length cannot be zero")
            }
            Self::HeightOverflow => formatter.write_str("Bitcoin height overflow"),
            Self::UnexpectedHeight { expected, actual } => {
                write!(
                    formatter,
                    "expected Bitcoin height {expected}, got {actual}"
                )
            }
            Self::ReorgBeyondChainRoot {
                root_bitcoin_height,
            } => write!(
                formatter,
                "Bitcoin reorganization reaches the chain root at height {root_bitcoin_height}"
            ),
            Self::ReorgTooDeep { depth, limit } => write!(
                formatter,
                "Bitcoin reorganization of {depth} blocks exceeds the {limit}-block commitment window"
            ),
        }
    }
}

impl std::error::Error for SortitionError {}

/// Build the first snapshot for a Bitcoin source without prior context.
#[must_use]
pub fn snapshot_for(block: &BitcoinBlock) -> SortitionSnapshot {
    let bitcoin_header_hash = BitcoinHeaderHash::from_bytes(block.hash);
    SortitionSnapshot::genesis(block.height, bitcoin_header_hash)
}

#[cfg(test)]
mod tests {
    use super::{
        CommitmentWindowBlock, MiningCommitment, PoxIdTracker, RewardCycleSchedule,
        SortitionEngine, SortitionHash, SortitionSnapshot, commitment_burn_statistics,
        commitment_distribution, select_epoch4_winner, select_winner,
    };

    #[test]
    fn commitment_distribution_uses_minimum_median_burns() {
        let prior = commitment(1, 0, 5);
        let linked = commitment(2, 1, 9);
        let unlinked = commitment(3, 9, 8);
        let distribution = commitment_distribution(&[
            CommitmentWindowBlock {
                commitments: vec![prior],
                missed_commitments: Vec::new(),
                requires_single_commit: false,
            },
            CommitmentWindowBlock {
                commitments: vec![linked, unlinked],
                missed_commitments: Vec::new(),
                requires_single_commit: false,
            },
        ])
        .expect("valid commitment window");

        assert_eq!(distribution[0].burn_sats, 7);
        assert_eq!(distribution[0].median_burn_sats, 7);
        assert_eq!(distribution[0].frequency, 2);
        assert_eq!(distribution[1].burn_sats, 4);
        assert_eq!(distribution[1].frequency, 1);
        assert_eq!(distribution[0].range_start, super::Uint256::zero());
        assert_eq!(distribution[1].range_end, super::Uint256::MAX);
        assert!(select_winner(&distribution, SortitionHash::initial(), [0; 32]).is_some());
    }

    #[test]
    fn carryover_uses_block_totals_not_weighted_samples() {
        let statistics = commitment_burn_statistics(&[
            CommitmentWindowBlock {
                commitments: vec![commitment(1, 0, 2), commitment(2, 0, 8)],
                missed_commitments: Vec::new(),
                requires_single_commit: false,
            },
            CommitmentWindowBlock {
                commitments: vec![commitment(3, 0, 6)],
                missed_commitments: Vec::new(),
                requires_single_commit: false,
            },
        ])
        .expect("commitment window statistics");
        assert_eq!(statistics.block_burn, 6);
        assert_eq!(statistics.window_median_burn, 8);
    }

    #[test]
    fn epoch4_rejects_inactive_or_under_carried_winners() {
        let sample = super::BurnSample {
            candidate: commitment(1, 0, 1),
            burn_sats: 1,
            median_burn_sats: 1,
            frequency: 1,
            range_start: super::Uint256::zero(),
            range_end: super::Uint256::MAX,
        };
        let distribution = vec![sample.clone(), sample.clone(), sample];
        assert_eq!(
            select_epoch4_winner(&distribution, 6, SortitionHash::initial(), [0; 32], 10, 10),
            None
        );

        let active = super::BurnSample {
            frequency: 3,
            ..distribution[0].clone()
        };
        let distribution = vec![active.clone(), active.clone(), active];
        assert_eq!(
            select_epoch4_winner(&distribution, 6, SortitionHash::initial(), [0; 32], 0, 10),
            None
        );
    }

    #[test]
    fn engine_keeps_winner_and_total_across_bitcoin_blocks() {
        let genesis = SortitionSnapshot::genesis(0, super::BitcoinHeaderHash::from_bytes([0; 32]));
        let mut engine = SortitionEngine::new(genesis);
        let first = commitment(1, 0, 10);
        let first_block = bitcoin_block(1, 1);
        let snapshot = engine
            .append(
                &first_block,
                &[first.txid],
                CommitmentWindowBlock {
                    commitments: vec![first.clone()],
                    missed_commitments: Vec::new(),
                    requires_single_commit: false,
                },
                super::PoxId::initial(),
            )
            .expect("first sortition snapshot");
        assert_eq!(snapshot.total_burn, 10);
        assert_eq!(snapshot.winner_txid, Some(first.txid));
        assert_eq!(snapshot.winner_vrf_seed, Some(first.vrf_seed));
        assert_eq!(
            snapshot.operations_hash,
            super::OpsHash::from_txids(&[first.txid])
        );

        let second = commitment(2, 1, 10);
        let snapshot = engine
            .append(
                &bitcoin_block(2, 2),
                &[second.txid],
                CommitmentWindowBlock {
                    commitments: vec![second.clone()],
                    missed_commitments: Vec::new(),
                    requires_single_commit: false,
                },
                super::PoxId::initial(),
            )
            .expect("second sortition snapshot");
        assert_eq!(snapshot.total_burn, 20);
        assert_eq!(snapshot.winner_txid, Some(second.txid));
        assert_eq!(snapshot.winner_vrf_seed, Some(second.vrf_seed));
        assert_eq!(engine.commitment_window().len(), 2);
    }

    #[test]
    fn pox_tracker_uses_classic_and_waterfall_cycle_starts() {
        let schedule = RewardCycleSchedule::new(0, 20, Some(280)).expect("valid schedule");
        let mut tracker = PoxIdTracker::new(schedule);
        for height in 1..=300 {
            tracker.advance(height, true).expect("contiguous height");
            let expected_length = if height < 280 {
                (height - 1) / 20 + 2
            } else {
                (height - 280) / 20 + 16
            };
            assert_eq!(
                tracker.pox_id().bits().len(),
                usize::try_from(expected_length).expect("test length fits usize"),
                "{height}"
            );
            assert!(tracker.pox_id().bits().iter().all(|bit| *bit));
        }
    }

    #[test]
    fn a_reorganized_branch_is_retracted_and_replaced() {
        let mut engine = engine_over(&[1, 2, 3, 4]);
        let chain = engine.snapshots().clone();

        let fork = chain
            .find_fork(|height| Ok::<_, ()>(canonical_hash(height, 2)))
            .expect("fork lookup succeeds");
        assert_eq!(fork, super::Fork::Above(2));
        let super::Fork::Above(fork_height) = fork else {
            panic!("expected a fork above height 2");
        };

        let reorg = engine.retract_above(fork_height).expect("retract branch");
        assert_eq!(reorg.depth(), 2);
        assert_eq!(reorg.resume_bitcoin_height(), 3);
        assert_eq!(reorg.valid_ancestor.bitcoin_height, 2);
        assert_eq!(
            reorg.invalidated_consensus_hashes(),
            chain.snapshots()[3..]
                .iter()
                .map(|snapshot| snapshot.consensus_hash)
                .collect::<Vec<_>>()
        );
        assert_eq!(engine.commitment_window().len(), 2);
        assert_eq!(engine.snapshots().tip().bitcoin_height, 2);

        // The replacement branch reproduces the snapshots of a chain that only
        // ever saw it.
        append(&mut engine, 3, 0x33);
        append(&mut engine, 4, 0x44);
        let mut replayed = engine_over(&[1, 2]);
        append(&mut replayed, 3, 0x33);
        append(&mut replayed, 4, 0x44);
        assert_eq!(engine.snapshots(), replayed.snapshots());
    }

    #[test]
    fn retracting_nothing_leaves_the_chain_alone() {
        let mut engine = engine_over(&[1, 2]);
        let reorg = engine.retract_above(2).expect("retract nothing");

        assert!(reorg.is_empty());
        assert_eq!(reorg.valid_ancestor.bitcoin_height, 2);
        assert_eq!(engine.commitment_window().len(), 2);
    }

    #[test]
    fn a_reorganization_deeper_than_the_commitment_window_is_refused() {
        let mut engine = engine_over(&[1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(
            engine.retract_above(0),
            Err(super::SortitionError::ReorgTooDeep {
                depth: 7,
                limit: super::MINING_COMMITMENT_WINDOW,
            })
        );
        assert_eq!(engine.snapshots().tip().bitcoin_height, 7);

        let mut chain = engine_over(&[1, 2]).snapshots().clone();
        assert_eq!(
            chain.find_fork(|_| Ok::<_, ()>([0xff; 32])),
            Ok(super::Fork::BeyondChainRoot {
                root_bitcoin_height: 0
            })
        );
        assert_eq!(
            chain.retract_above(0).map(|reorg| reorg.depth()),
            Ok(2),
            "the root itself survives a retraction down to it"
        );
        assert!(matches!(
            chain.retract_above(u64::MAX),
            Ok(reorg) if reorg.is_empty()
        ));
    }

    #[test]
    fn pox_tracker_rewinds_the_anchors_of_a_retracted_branch() {
        let schedule = RewardCycleSchedule::new(0, 20, None).expect("valid schedule");
        let mut tracker = PoxIdTracker::new(schedule);
        for height in 1..=25 {
            tracker.advance(height, true).expect("contiguous height");
        }
        let bits = tracker.pox_id().bits().len();

        tracker.retract_to(20).expect("rewind past a cycle start");
        assert_eq!(tracker.pox_id().bits().len(), bits - 1);
        for height in 21..=25 {
            tracker.advance(height, true).expect("replayed height");
        }
        assert_eq!(tracker.pox_id().bits().len(), bits);
    }

    #[test]
    fn pox_tracker_records_an_unknown_anchor() {
        let schedule = RewardCycleSchedule::new(0, 20, None).expect("valid schedule");
        let mut tracker = PoxIdTracker::new(schedule);
        tracker.advance(1, false).expect("first cycle start");
        assert_eq!(tracker.pox_id().bits(), &[true, false]);
    }

    /// An engine over Bitcoin blocks whose header hash and commitment are named
    /// by one byte, each commitment spending the one before it.
    fn engine_over(hashes: &[u8]) -> SortitionEngine {
        let mut engine = SortitionEngine::new(SortitionSnapshot::genesis(
            0,
            super::BitcoinHeaderHash::from_bytes([0; 32]),
        ));
        for (index, hash) in hashes.iter().enumerate() {
            append(
                &mut engine,
                u64::try_from(index).expect("test index fits u64") + 1,
                *hash,
            );
        }
        engine
    }

    fn append(engine: &mut SortitionEngine, height: u64, hash: u8) {
        let spent_txid = engine
            .commitment_window()
            .last()
            .and_then(|block| block.commitments.first())
            .map_or([0; 32], |commitment| commitment.txid);
        let mining = MiningCommitment {
            txid: [hash; 32],
            spent_txid,
            spent_output: 3,
            burn_sats: 10,
            vrf_seed: [hash; 32],
        };
        engine
            .append(
                &bitcoin_block(height, hash),
                &[mining.txid],
                CommitmentWindowBlock {
                    commitments: vec![mining],
                    missed_commitments: Vec::new(),
                    requires_single_commit: false,
                },
                super::PoxId::initial(),
            )
            .expect("contiguous Bitcoin block");
    }

    /// The header hash Bitcoin reports once it reorganized above `fork_height`.
    fn canonical_hash(height: u64, fork_height: u64) -> [u8; 32] {
        let byte = u8::try_from(height).expect("test height fits u8");
        if height <= fork_height {
            [byte; 32]
        } else {
            [byte ^ 0x80; 32]
        }
    }

    fn commitment(txid: u8, spent_txid: u8, burn_sats: u64) -> MiningCommitment {
        MiningCommitment {
            txid: [txid; 32],
            spent_txid: [spent_txid; 32],
            spent_output: 3,
            burn_sats,
            vrf_seed: [0; 32],
        }
    }

    fn bitcoin_block(height: u64, hash: u8) -> nano_bitcoin::BitcoinBlock {
        nano_bitcoin::BitcoinBlock {
            height,
            hash: [hash; 32],
            operations: Vec::new(),
        }
    }
}
