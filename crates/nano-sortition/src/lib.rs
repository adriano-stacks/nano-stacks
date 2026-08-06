pub(crate) mod carryover;

use std::collections::{BTreeMap, BTreeSet};
use std::{collections::HashMap, fmt};

use nano_bitcoin::{BitcoinBlock, BitcoinOperationKind};
use nano_primitives::{
    BitcoinHeaderHash, ConsensusHash, SortitionId, Uint256, Uint512, hash160, sha256, sha512_256,
};

const SYSTEM_FORK_SET_VERSION: [u8; 4] = [23, 0, 0, 0];

/// The Bitcoin blocks a sortition weighs mining commitments over.
pub const MINING_COMMITMENT_WINDOW: usize = 6;

/// Blocks of commitment history a [`SortitionEngine`] keeps.
///
/// More than the window, because a Bitcoin reorganization takes the top of that
/// history with it and the replacement branch has to be weighed over a full
/// window from its first block. Keeping only the window meant a reorganization
/// two blocks deep left five, and the replayed sortition was weighed over five
/// blocks where the network used six — the same failure as a short window
/// anywhere else, which is a different answer rather than a rougher one. Twice
/// the window covers every retraction [`SortitionEngine::retract_above`] admits.
const RETAINED_COMMITMENT_BLOCKS: usize = MINING_COMMITMENT_WINDOW * 2;

/// How many snapshots a chain keeps behind its tip.
///
/// The chain runs ahead of the blocks being executed under it, so a snapshot has to
/// outlive the moment it was derived — and keeping all of them is a leak that grows
/// with the burnchain. This is the deepest reader plus margin, which is how
/// `nano_chainstate`'s `EARNINGS_KEPT` is chosen too: what a node can still be
/// asked about.
///
/// The deepest readers are the burn view of a block about to be executed, which is
/// at or just below the tip; the fork point of a Bitcoin reorganization, refused
/// beyond [`MINING_COMMITMENT_WINDOW`]; and the last burn block that elected
/// somebody, a handful back on any live chain. 144 burn blocks is a day of Bitcoin
/// and the same bound a catch-up round walks, so nothing a round can produce falls
/// outside it. Twenty-odd kilobytes.
const SNAPSHOTS_KEPT: usize = 144;

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
    /// The leader-key VRF public key this commitment named, when the burn block
    /// that registered it is one this node has seen.
    pub vrf_public_key: Option<[u8; 32]>,
    /// The block-signing `Hash160` the same registration carried, if it had one.
    pub signing_key_hash: Option<[u8; 20]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SortitionWinner {
    pub txid: [u8; 32],
    pub vrf_seed: [u8; 32],
    /// The VRF public key of the leader-key registration this commitment named,
    /// when the registering burn block is one this node has seen.
    ///
    /// `None` is not "no key" — every winning commitment names one. It means the
    /// registration predates the burnchain window this node holds, which is the
    /// ordinary case for the first tenures after a checkpoint. A validator that
    /// treated it as "no check needed" would be accepting an unverified proof;
    /// it has to say so instead.
    pub vrf_public_key: Option<[u8; 32]>,
    /// The block-signing `Hash160` that registration carried, which is what the
    /// tenure's miner signs its headers under. Optional twice over: the
    /// registration may be below this node's window, and only some registrations
    /// carry one at all.
    pub signing_key_hash: Option<[u8; 20]>,
}

/// Bitcoin blocks a miner may name when it says which block it built on.
///
/// A commitment carries the height of its parent modulo this
/// (`burn/operations/leader_block_commit.rs`, `BURN_BLOCK_MINED_AT_MODULUS`).
pub const BURN_BLOCK_MINED_AT_MODULUS: u64 = 5;

/// How many Bitcoin blocks late a commitment arrived; zero if it was on time.
///
/// A miner names the block it built on by its height modulo five, so the block it
/// means to land in is the one after that (`leader_block_commit.rs`, `check`).
/// Because the modulus wraps at five, "how late" is only knowable modulo five,
/// which is why stacks-core refuses a distance above one outright — see
/// [`commitment_window_block`].
#[must_use]
pub const fn commitment_miss_distance(parent_modulus: u8, bitcoin_height: u64) -> u64 {
    let intended =
        (parent_modulus as u64 % BURN_BLOCK_MINED_AT_MODULUS + 1) % BURN_BLOCK_MINED_AT_MODULUS;
    let actual = bitcoin_height % BURN_BLOCK_MINED_AT_MODULUS;
    if actual >= intended {
        actual - intended
    } else {
        BURN_BLOCK_MINED_AT_MODULUS + actual - intended
    }
}

/// Whether a commitment landed in the Bitcoin block it aimed at.
///
/// A commitment that arrives anywhere else missed: stacks-core keeps it only so
/// its UTXO can chain through the mining window, and it is neither a candidate
/// for the sortition nor one of the operations the block's `ops_hash` covers.
#[must_use]
pub const fn commitment_is_on_time(parent_modulus: u8, bitcoin_height: u64) -> bool {
    commitment_miss_distance(parent_modulus, bitcoin_height) == 0
}

/// The operations a Bitcoin block contributes to its own consensus hash.
///
/// Everything the block carries except the commitments that missed their
/// target, which the network parses but does not accept.
#[must_use]
pub fn accepted_operation_txids(block: &BitcoinBlock) -> Vec<[u8; 32]> {
    block
        .operations
        .iter()
        .filter(|operation| match operation.kind {
            BitcoinOperationKind::LeaderBlockCommit { parent_modulus, .. } => {
                commitment_is_on_time(parent_modulus, block.height)
            }
            _ => true,
        })
        .map(|operation| operation.txid)
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissedCommitment {
    pub txid: [u8; 32],
    pub spent_txid: [u8; 32],
    pub spent_output: u32,
}

/// How many of a mining commitment's outputs are payouts.
///
/// A commitment's burn is the sum of the outputs it pays `PoX` recipients with,
/// and everything after them is the miner's change — which is the output the
/// next commitment spends to chain through the mining window. So this one number
/// decides both a commitment's weight in the distribution and whether the chain
/// links at all, and getting it wrong makes every candidate's burn a change
/// amount tens of thousands of times too large.
///
/// A reward phase pays `OUTPUTS_PER_COMMIT` outputs; a prepare phase burns to one
/// address; the waterfall pays the one sBTC address. See
/// [`PayoutSchedule::outputs_at`] for where that is written down in stacks-core
/// and for the rule it is *not*.
///
/// It also answers how many blocks the burn distribution is weighed over, which
/// is not always six — see [`PayoutSchedule::mining_window_at`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PayoutSchedule {
    cycles: RewardCycleSchedule,
    prepare_phase_length: u64,
    /// The Bitcoin height epoch 4.0 activates at, when the node knows it.
    ///
    /// This is pox-5's activation height: `validate_epochs` requires the two to be
    /// equal, so `/v2/pox` states it and nothing new has to be configured.
    ///
    /// One boundary is enough because nano is a 4.0-only node that starts at or
    /// after that boundary, so it is the only epoch transition its mining window
    /// can ever contain. On mainnet the one below it, epoch 3.4 at burn 943,333, is
    /// seventeen thousand blocks back.
    epoch_four_activation: Option<u64>,
}

/// Recipients a commitment pays in a reward phase (`OUTPUTS_PER_COMMIT`).
pub const OUTPUTS_PER_COMMIT: usize = 2;

impl PayoutSchedule {
    pub const fn new(
        cycles: RewardCycleSchedule,
        prepare_phase_length: u64,
    ) -> Result<Self, SortitionError> {
        if prepare_phase_length >= cycles.reward_cycle_length {
            return Err(SortitionError::ZeroRewardCycleLength);
        }
        Ok(Self {
            cycles,
            prepare_phase_length,
            epoch_four_activation: None,
        })
    }

    /// Say where epoch 4.0 begins, which shortens the mining window around it.
    ///
    /// Additive rather than a constructor argument so a caller that does not know
    /// the boundary keeps the behaviour it had. See
    /// [`PayoutSchedule::mining_window_at`] for what knowing it buys.
    #[must_use]
    pub const fn activating_epoch_four_at(mut self, bitcoin_height: u64) -> Self {
        self.epoch_four_activation = Some(bitcoin_height);
        self
    }

    /// Whether this Bitcoin block is in a prepare phase.
    ///
    /// stacks-core's *classic* predicate (`PoxConstants::static_is_in_prepare_phase`),
    /// which is the one both the commitment parser and the burn distribution use:
    /// the first block of a cycle is at offset 1, so offset 0 is the last block of
    /// the previous cycle's prepare phase and counts as prepare, while the block at
    /// offset `length - prepare` does *not*. nano had `offset >= length - prepare`,
    /// which is that window shifted down by one — invisible in every capture,
    /// because they all sit deep in a reward phase, and wrong at both ends of every
    /// prepare phase mainnet has.
    #[must_use]
    pub const fn is_in_prepare_phase(&self, bitcoin_height: u64) -> bool {
        if bitcoin_height <= self.cycles.first_bitcoin_height {
            return false;
        }
        let offset = self.cycles.offset_in_cycle(bitcoin_height);
        offset == 0 || offset > self.cycles.reward_cycle_length - self.prepare_phase_length
    }

    /// How many payout outputs a commitment in this Bitcoin block carries.
    ///
    /// stacks-core spells this out once, in `SortitionHandleTx::get_num_pox_payouts`
    /// (`chainstate/burn/db/sortdb.rs`), and it is a function of the *height* alone:
    /// one output under the waterfall, one in a prepare phase, `OUTPUTS_PER_COMMIT`
    /// otherwise. Two other places have to agree with it and do —
    /// `parse_pox_waterfall_commits` and `parse_pre_pox_waterfall_commits` read that
    /// many outputs off the transaction, and `check_pox_pre_waterfall` requires the
    /// commitment to carry exactly that many.
    ///
    /// **It is not the size of the reward set.** That was the standing suspicion,
    /// because a small chain's cycle can hold fewer recipients than there are
    /// outputs, and it is wrong in a way stacks-core is explicit about: a reward set
    /// with one recipient is *padded with a burn address* to reach the full count —
    /// `RewardSetInfo::into_commit_outs` pads what a miner pays, and
    /// `check_pox_pre_waterfall` pads what a validator expects ("If the number of
    /// recipients in the set was odd, we need to pad with a burn address"). So a
    /// one-stacker cycle still pays two outputs, the second of them a burn, and the
    /// count never moves with the recipients.
    ///
    /// The archive says the same thing without being asked to interpret anything.
    /// A snapshot's `pox_payouts` column is `(addresses, amount-per-output)` where
    /// the address list is padded to exactly this count, so
    /// `amount × addresses.len()` is the block's whole payout burn — and it equals
    /// the running `total_burn`'s own step at every captured block, on mainnet's
    /// pre-waterfall reward phase (×2) and on the hacknet capture's waterfall (×1)
    /// alike. `conformance/burn_spends.rs` asserts both directions.
    #[must_use]
    pub fn outputs_at(&self, bitcoin_height: u64) -> usize {
        if self.cycles.is_waterfall_at(bitcoin_height) {
            return 1;
        }
        if self.is_in_prepare_phase(bitcoin_height) {
            1
        } else {
            OUTPUTS_PER_COMMIT
        }
    }

    /// How many Bitcoin blocks the burn distribution for this block is weighed over.
    ///
    /// Six is the ordinary answer and not the only one. stacks-core windows a
    /// sortition over `MINING_COMMITMENT_WINDOW` blocks *only* when the block is in
    /// a reward phase and the epoch at the bottom of the window is the epoch at the
    /// top (`Burnchain::from_block_ops`); otherwise it weighs the block alone.
    ///
    /// Both exceptions are real on mainnet. A prepare phase runs for a hundred
    /// blocks of every twenty-one hundred, and a one-block window changes more than
    /// a candidate's weight: the windowed median becomes the block's own total, so
    /// the assumed-total-commit carryover is always 1 and the null miner can never
    /// win. And epoch 4.0 activates at burn 960,230 — the mainnet checkpoint's own
    /// neighbourhood — so the seven blocks from there have the epoch 3.4 boundary
    /// inside their window and are weighed alone. That is why nano named a
    /// different winner at burn 960,230 and 960,233 while agreeing on every other
    /// field: its window was six blocks where the network's was one.
    #[must_use]
    pub const fn mining_window_at(&self, bitcoin_height: u64) -> usize {
        if self.is_in_prepare_phase(bitcoin_height) {
            return 1;
        }
        // The epoch at the bottom of the window is read at `bitcoin_height - 1 -
        // MINING_COMMITMENT_WINDOW`, so a boundary is "inside the window" for the
        // window's length plus one blocks after it.
        if let Some(activation) = self.epoch_four_activation
            && bitcoin_height >= activation
            && bitcoin_height - activation <= MINING_COMMITMENT_WINDOW as u64
        {
            return 1;
        }
        MINING_COMMITMENT_WINDOW
    }

    /// Whether this Bitcoin block opens a reward cycle, and so adds a bit to the
    /// `PoX` history the consensus hash mixes.
    #[must_use]
    pub fn starts_reward_cycle(&self, bitcoin_height: u64) -> bool {
        self.cycles.starts_at(bitcoin_height)
    }
}

/// The commitments a Bitcoin block contributes to the mining window.
///
/// A commitment that missed the block it aimed at by one is kept apart: it is not
/// a candidate and not part of the operations hash, but its UTXO still chains, so
/// dropping it entirely would break the window of every miner behind it.
///
/// A commitment that missed by *more* than one is dropped outright, and its UTXO
/// chains nothing. stacks-core refuses it with `BlockCommitMissDistanceTooBig`
/// (`check_intended_sortition`) and says why: a miner who could file late by any
/// amount could bunch a whole window's commitments into one Bitcoin block and mine
/// when it suited them, skipping the six-block warm-up the window exists to
/// impose. It is also what makes the filing rule below unambiguous.
#[must_use]
pub fn commitment_window_block(
    block: &BitcoinBlock,
    payouts: usize,
    keys: &LeaderKeys,
) -> CommitmentWindowBlock {
    let mut commitments = Vec::new();
    let mut missed_commitments = Vec::new();
    for operation in &block.operations {
        let BitcoinOperationKind::LeaderBlockCommit {
            new_seed,
            parent_modulus,
            key_block_height,
            key_transaction_index,
            ..
        } = &operation.kind
        else {
            continue;
        };
        // The first input is the UTXO the commitment chained from, which is what
        // links it to the miner's previous commitment in the window.
        let (spent_txid, spent_output) = operation
            .inputs
            .first()
            .map_or(([0; 32], 0), |input| (input.txid, input.output_index));
        let miss_distance = commitment_miss_distance(*parent_modulus, block.height);
        if miss_distance > 1 {
            continue;
        }
        if miss_distance == 0 {
            commitments.push(MiningCommitment {
                txid: operation.txid,
                spent_txid,
                spent_output,
                burn_sats: operation
                    .outputs
                    .iter()
                    .take(payouts)
                    .map(|output| output.amount_sats)
                    .sum(),
                vrf_seed: *new_seed,
                vrf_public_key: keys
                    .registration(
                        u64::from(*key_block_height),
                        u32::from(*key_transaction_index),
                    )
                    .map(|registration| registration.vrf_public_key),
                signing_key_hash: keys
                    .registration(
                        u64::from(*key_block_height),
                        u32::from(*key_transaction_index),
                    )
                    .and_then(|registration| registration.signing_key_hash),
            });
        } else {
            missed_commitments.push(MissedCommitment {
                txid: operation.txid,
                spent_txid,
                spent_output,
            });
        }
    }
    CommitmentWindowBlock {
        commitments,
        missed_commitments,
        requires_single_commit: payouts == 1,
    }
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

/// What the miners of one burn block spent on its sortition.
///
/// Both numbers are Clarity-visible — `get-block-info?`/`get-tenure-info?` answer
/// `miner-spend-total` and `miner-spend-winner` out of them — and the language
/// documentation promises the winner's is no larger than the total. They are
/// therefore reported together or not at all: a total without its winner is a
/// broken invariant offered to a contract, which is worse than an absence.
///
/// stacks-core keeps them per tenure in the `payments` table's
/// `burnchain_sortition_burn` and `burnchain_commit_burn` columns, filled at the
/// tenure-start block from `SortitionDB::get_block_burn_amount` (every accepted
/// commitment's `burn_fee` in the electing burn block) and from the winning
/// commitment's own `burn_fee`. A `burn_fee` counts payout outputs only, so both
/// depend on [`PayoutSchedule::outputs_at`] and on nothing else.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BurnSpends {
    /// Every eligible commitment's payout burn in the burn block.
    pub total: u64,
    /// The winning commitment's own.
    pub winner: u64,
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

    for (slot, block) in earlier.iter().enumerate().rev() {
        let expected_output = if block.requires_single_commit { 2 } else { 3 };
        let mut commitments = block
            .commitments
            .iter()
            .cloned()
            .map(|commitment| (commitment.txid, commitment))
            .collect::<HashMap<_, _>>();
        // A missed commitment belongs to the sortition it *intended* to land in,
        // not the one it arrived in: stacks-core files it under
        // `intended_sortition`, which is one block back, and refuses any larger
        // distance. So the misses a chain can link to in this slot are the ones
        // that arrived in the block above it — and the oldest slot's own misses
        // belong below the window and are never read.
        let mut missed = window
            .get(slot + 1)
            .into_iter()
            .flat_map(|above| above.missed_commitments.iter().cloned())
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

    /// How far into its reward cycle a Bitcoin block sits.
    #[must_use]
    pub const fn offset_in_cycle(&self, bitcoin_height: u64) -> u64 {
        bitcoin_height.saturating_sub(self.first_bitcoin_height) % self.reward_cycle_length
    }

    /// Whether the waterfall pays this Bitcoin block's commitments.
    #[must_use]
    pub fn is_waterfall_at(&self, bitcoin_height: u64) -> bool {
        self.first_waterfall_height
            .is_some_and(|height| bitcoin_height >= height)
    }

    /// Whether a Bitcoin block opens a reward cycle.
    #[must_use]
    pub fn starts_at(&self, bitcoin_height: u64) -> bool {
        let relative_height = bitcoin_height.saturating_sub(self.first_bitcoin_height);
        if self.is_waterfall_at(bitcoin_height) {
            relative_height.is_multiple_of(self.reward_cycle_length)
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
    /// The burn block's header time, which Clarity reads as `burn-block-time`.
    ///
    /// Carried on the snapshot rather than read from the burnchain again at
    /// execution time, because a chain resumed from a capture has to be able to
    /// state it for its own seed, and stacks-core's `snapshots` table records it
    /// beside every other field here as `burn_header_timestamp`.
    pub bitcoin_timestamp: u64,
    pub sortition_id: SortitionId,
    pub parent_sortition_id: SortitionId,
    pub operations_hash: OpsHash,
    pub consensus_hash: ConsensusHash,
    pub total_burn: u64,
    pub sortition_hash: SortitionHash,
    pub winner_txid: Option<[u8; 32]>,
    pub winner_vrf_seed: Option<[u8; 32]>,
    /// The winning commitment's leader-key VRF public key, if this node saw the
    /// burn block that registered it. See `SortitionWinner::vrf_public_key`.
    pub winner_vrf_public_key: Option<[u8; 32]>,
    /// The `Hash160` the winning leader key was registered with, which is what a
    /// miner signs its tenure's blocks under.
    ///
    /// Carried for the same reason as the VRF key and from the same place: the
    /// registration is far below any burnchain window a checkpointed node holds,
    /// so the tenure's own burn block cannot answer it. Not every registration
    /// has one — only 101 of mainnet's 2,477 do — so its absence is ordinary and
    /// says the rule cannot run rather than that it failed.
    pub winner_signing_key_hash: Option<[u8; 20]>,
    /// What this burn block's miners spent on its sortition, and the winner's
    /// share, which Clarity reads back as `miner-spend-total`/`miner-spend-winner`.
    ///
    /// Carried here for the same reason [`Self::bitcoin_timestamp`] is: it is a fact
    /// about *this* burn block, and a chain that has walked past it still has to
    /// answer for the blocks a follower is executing under. Reading it off the
    /// engine's commitment window instead answered only for the window's last
    /// block, which is the chain's tip and not the view being executed.
    ///
    /// `None` at a burn block that elected nobody, and on a chain whose mining
    /// window has not been primed. See [`BurnSpends`] for why the pair never
    /// splits.
    pub burn_spends: Option<BurnSpends>,
    pub pox_id: PoxId,
}

impl SortitionSnapshot {
    #[must_use]
    pub fn genesis(bitcoin_height: u64, bitcoin_header_hash: BitcoinHeaderHash) -> Self {
        Self {
            bitcoin_height,
            bitcoin_header_hash,
            bitcoin_timestamp: 0,
            sortition_id: SortitionId::from_bytes(*bitcoin_header_hash.as_bytes()),
            parent_sortition_id: SortitionId::from_bytes(*bitcoin_header_hash.as_bytes()),
            operations_hash: OpsHash([0; 32]),
            consensus_hash: ConsensusHash::from_bytes([0; 20]),
            total_burn: 0,
            sortition_hash: SortitionHash::initial(),
            winner_txid: None,
            winner_vrf_seed: None,
            winner_vrf_public_key: None,
            winner_signing_key_hash: None,
            burn_spends: None,
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
    pub const fn depth(&self) -> usize {
        self.retracted.len()
    }

    /// Whether the reorganization retracted nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
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
    /// Every consensus hash up to the tip, oldest first.
    ///
    /// A consensus hash mixes the ones at power-of-two offsets behind it,
    /// reaching back thousands of blocks, so a chain that starts at a
    /// checkpoint cannot derive one from its own snapshots alone. Carrying the
    /// hashes rather than the snapshots keeps that history to twenty bytes a
    /// block — six megabytes for the whole of mainnet.
    consensus_hashes: Vec<ConsensusHash>,
    /// The last burn height at or below the oldest snapshot still held that
    /// elected somebody. See [`Self::last_sortition_at_or_below`].
    sortition_below_window: Option<u64>,
}

impl SnapshotChain {
    #[must_use]
    pub fn new(genesis: SortitionSnapshot) -> Self {
        Self {
            consensus_hashes: vec![genesis.consensus_hash],
            snapshots: vec![genesis],
            sortition_below_window: None,
        }
    }

    /// Start from a checkpoint, carrying the consensus hashes behind it.
    ///
    /// `history` runs oldest first and ends with the genesis snapshot's own
    /// hash, which is what makes the skip-list reach past the checkpoint.
    #[must_use]
    pub fn with_history(genesis: SortitionSnapshot, history: Vec<ConsensusHash>) -> Option<Self> {
        if history.last() != Some(&genesis.consensus_hash) {
            return None;
        }
        Some(Self {
            snapshots: vec![genesis],
            consensus_hashes: history,
            sortition_below_window: None,
        })
    }

    /// Every consensus hash behind the tip, oldest first.
    ///
    /// This is what seeds a chain: `with_history` needs the whole run, because
    /// `ConsensusHash::from_ops` mixes hashes at power-of-two offsets and a
    /// truncated history derives a different one from there on.
    #[must_use]
    pub fn history(&self) -> &[ConsensusHash] {
        &self.consensus_hashes
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
        // The hash history must shrink with the chain, or a rebuilt branch
        // mixes hashes from the one it replaced.
        let dropped = self.snapshots.len().saturating_sub(kept);
        self.consensus_hashes
            .truncate(self.consensus_hashes.len().saturating_sub(dropped));
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

    /// The winning VRF seed the next sortition's sampling mixes.
    ///
    /// The most recent winner's, which is **not** the tip's: a burn block with no
    /// sortition mixes no seed, so the sampling of the block after it reaches
    /// back past it to the last block that elected somebody. Mainnet's burn
    /// 960,222, 960,224, 960,227 and 960,229 are four such blocks in fifteen.
    ///
    /// It is one field rather than a walk for a chain being resumed, which is
    /// exactly the case where a walk cannot answer: a chain seeded at a
    /// sortition-less burn block holds no snapshot with a seed at all, and the
    /// commitments of its own burn block carry the seed of the tenure they were
    /// *bidding* for rather than the one last won.
    #[must_use]
    pub fn effective_winner_seed(&self) -> Option<[u8; 32]> {
        self.snapshots
            .iter()
            .rev()
            .find_map(|snapshot| snapshot.winner_vrf_seed)
    }

    /// Adopt the winning seed of the snapshot the chain was seeded at.
    ///
    /// Only the sampling of the block *after* the root reads it, and a captured
    /// checkpoint does not record it — but every eligible commitment in a
    /// Nakamoto burn block carries the same `new_seed`, so the root's own burn
    /// block states it. Left unset, the first sortition after a checkpoint is
    /// sampled against a zero seed and names a miner that did not win.
    ///
    /// Refused once the chain has moved past its root, where it would be
    /// rewriting a seed the snapshots above it were already derived from.
    pub fn adopt_root_winner_seed(&mut self, seed: [u8; 32]) -> bool {
        if self.snapshots.len() != 1 {
            return false;
        }
        let root = self
            .snapshots
            .first_mut()
            .expect("snapshot chain has genesis");
        root.winner_vrf_seed = Some(seed);
        true
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
        let operation_txids = accepted_operation_txids(block);
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
            bitcoin_timestamp: block.timestamp,
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
            winner_vrf_public_key: winner.and_then(|winner| winner.vrf_public_key),
            winner_signing_key_hash: winner.and_then(|winner| winner.signing_key_hash),
            // Filled by whoever holds the commitment window this sortition was
            // weighed over, which is `SortitionEngine::append`: this chain is given
            // a total burn and a winner and never sees a commitment.
            burn_spends: None,
            pox_id,
        };
        self.consensus_hashes.push(snapshot.consensus_hash);
        self.snapshots.push(snapshot);
        self.forget_snapshots_nothing_can_ask_for();
        Ok(self.tip())
    }

    /// Record what the tip's burn block spent, once its window is known.
    ///
    /// Separate from the append because the commitment window belongs to
    /// [`SortitionEngine`] and this chain is handed only the answers derived from
    /// it. Both halves or neither: see [`BurnSpends`].
    pub fn record_tip_burn_spends(&mut self, spends: Option<BurnSpends>) {
        if let Some(tip) = self.snapshots.last_mut() {
            tip.burn_spends = spends;
        }
    }

    /// Drop snapshots no execution, no retraction and no walk can still read.
    ///
    /// The chain runs ahead of execution — it is walked forward from this node's own
    /// Bitcoin source until it names the burn view a staged block stands on — so it
    /// has to hold the snapshot for a view *behind* its tip, and holding all of them
    /// is a leak that grows with the burnchain forever.
    ///
    /// [`SNAPSHOTS_KEPT`] is what the deepest reader needs plus margin, in the same
    /// terms `nano_chainstate`'s `EARNINGS_KEPT` is: what a node can still be asked
    /// about. Three readers reach below the tip — the burn view of a block about to
    /// be executed, the fork point of a Bitcoin reorganization (refused beyond
    /// [`MINING_COMMITMENT_WINDOW`]), and the walk back to the last burn block that
    /// elected somebody, which the accumulated coinbase is computed over. The
    /// consensus-hash history is *not* bounded and cannot be: the skip-list mixes
    /// hashes at power-of-two offsets, and a truncated history derives a different
    /// hash from there on.
    fn forget_snapshots_nothing_can_ask_for(&mut self) {
        while self.snapshots.len() > SNAPSHOTS_KEPT {
            let dropped = self.snapshots.remove(0);
            // The one fact a dropped snapshot still has to answer for. A tenure
            // collects the coinbase of every burn block since the last sortition,
            // and that height can fall below the window on a chain resumed at a
            // burn block that elected nobody — which mainnet leaves four of in
            // every fifteen. Monotonic, because the walk only ever asks for the
            // *last* one.
            if dropped.winner_txid.is_some() {
                self.sortition_below_window = Some(dropped.bitcoin_height);
            }
        }
    }

    /// The snapshot this chain derived for a Bitcoin height.
    ///
    /// Snapshots are contiguous in height, so the index is a subtraction and nothing
    /// is searched. `None` is a height above the tip — a view this chain has not
    /// walked to yet — or one below the retained window, and the two are different
    /// answers to the caller: the first closes with one more Bitcoin block, the
    /// second never will.
    #[must_use]
    pub fn snapshot_at(&self, bitcoin_height: u64) -> Option<&SortitionSnapshot> {
        let back = usize::try_from(self.tip().bitcoin_height.checked_sub(bitcoin_height)?).ok()?;
        self.snapshots.get(self.snapshots.len().checked_sub(back + 1)?)
    }

    /// The last burn height at or below this one that elected somebody.
    ///
    /// What a tenure's accumulated coinbase is measured from: every burn block since
    /// then contributes its emission to the tenure that finally wins one. That makes
    /// this consensus-visible in the strongest sense — the answer is *minted* — so a
    /// height the window cannot reach is reported as unknown rather than as "none",
    /// which would mint zero and seal a root nobody else computes.
    ///
    /// `None` is "this chain cannot say", never "there was none": a chain seeded at
    /// a checkpoint has thousands of sortitions behind its root and can only see the
    /// ones it walked or was handed. The caller must treat it as a missing answer.
    #[must_use]
    pub fn last_sortition_at_or_below(&self, bitcoin_height: u64) -> Option<u64> {
        self.snapshots
            .iter()
            .rev()
            .skip_while(|snapshot| snapshot.bitcoin_height > bitcoin_height)
            .find_map(|snapshot| snapshot.winner_txid.map(|_| snapshot.bitcoin_height))
            // The walk reached the oldest snapshot held without finding one. The
            // hint left behind by whatever fell out of the window — or recorded
            // when the chain was seeded — is then the only thing that can answer.
            .or_else(|| self.sortition_below_window.filter(|height| *height <= bitcoin_height))
    }

    /// The last sortition at or below this chain's oldest retained snapshot.
    ///
    /// Written down when a chain is saved, because a chain resumed at a burn block
    /// that elected nobody has no snapshot with a winner in it at all and cannot
    /// walk further back. See [`Self::last_sortition_at_or_below`].
    #[must_use]
    pub const fn sortition_below_window(&self) -> Option<u64> {
        self.sortition_below_window
    }

    /// Adopt that hint, for a chain being seeded from what a previous run saved.
    pub const fn seed_sortition_below_window(&mut self, bitcoin_height: Option<u64>) {
        self.sortition_below_window = bitcoin_height;
    }

    fn previous_consensus_hashes(&self) -> Vec<ConsensusHash> {
        let parent_index = self.consensus_hashes.len() - 1;
        let mut hashes = Vec::new();
        let mut exponent = 0_u32;
        while exponent < 64 {
            let offset = (1_usize << exponent).saturating_sub(1);
            let Some(index) = parent_index.checked_sub(offset) else {
                break;
            };
            hashes.push(self.consensus_hashes[index]);
            exponent += 1;
        }
        hashes
    }
}

/// The `PoX` history a captured sortition identifier was produced from.
///
/// A sortition identifier is the burn header hash and the `PoX` bit vector
/// hashed together, so a capture that records the identifier *states* the vector
/// rather than leaving a node to configure one — and the consensus hash mixes
/// that vector, so a node standing on `PoxId::initial()` derives a different
/// hash for every block however right the rest of its arithmetic is.
///
/// Only unbroken histories are searched, one bit per reward cycle with every bit
/// set, because that is what a chain that has never missed an anchor block has —
/// mainnet's is 142 such bits at the epoch 4.0 boundary. A chain that did miss
/// one does not resolve here, which is a report rather than a guess: the search
/// space of arbitrary vectors is exponential and picking one that happens to
/// hash right is not evidence of anything.
#[must_use]
pub fn unbroken_pox_id_for(
    bitcoin_header_hash: BitcoinHeaderHash,
    identifier: SortitionId,
    max_cycles: usize,
) -> Option<PoxId> {
    (1..=max_cycles)
        .map(|cycles| PoxId::from_bits(vec![true; cycles]))
        .find(|pox_id| sortition_id(bitcoin_header_hash, pox_id) == identifier)
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

    /// Start from a checkpoint, carrying the consensus hashes behind it.
    ///
    /// See [`SnapshotChain::with_history`]: the hashes are what let the
    /// skip-list reach past the checkpoint.
    #[must_use]
    pub fn with_history(genesis: SortitionSnapshot, history: Vec<ConsensusHash>) -> Option<Self> {
        Some(Self {
            snapshots: SnapshotChain::with_history(genesis, history)?,
            commitment_window: Vec::new(),
        })
    }

    /// Add a burn block to the mining window without taking a snapshot of it.
    ///
    /// The distribution weighs a candidate over the six blocks behind it, so an
    /// engine starting at a checkpoint has to be given those blocks before its
    /// first snapshot. Without them the window is short, every candidate's
    /// median burn is computed over fewer blocks than the network used, and the
    /// winner it picks is not the one the network picked — which is how a
    /// seven-block window turned mainnet's sortition at burn 960,226 into no
    /// sortition at all.
    ///
    /// Feed the blocks oldest first, ending with the snapshot's own burn block.
    ///
    /// The height is what lets the last of those blocks fill in the seed's own burn
    /// spends: a chain resumed from a checkpoint executes the tenure standing on its
    /// seed's burn view before it advances once, and the two spends are Clarity's to
    /// read at that block like any other.
    pub fn prime(&mut self, bitcoin_height: u64, commitments: CommitmentWindowBlock) {
        self.commitment_window.push(commitments);
        if self.commitment_window.len() > RETAINED_COMMITMENT_BLOCKS {
            self.commitment_window.remove(0);
        }
        if bitcoin_height == self.snapshots.tip().bitcoin_height {
            let spends = self.spends_at_tip();
            self.snapshots.record_tip_burn_spends(spends);
        }
    }

    /// See [`SnapshotChain::adopt_root_winner_seed`].
    pub fn adopt_root_winner_seed(&mut self, seed: [u8; 32]) -> bool {
        self.snapshots.adopt_root_winner_seed(seed)
    }

    /// Commitments the most recent burn block put up for its sortition.
    ///
    /// A diagnostic now rather than a gate. It existed because the winner among
    /// several competing commitments did not derive exactly, so a caller that
    /// would *reject* a block on the strength of the winner's identity had to
    /// know whether the block left it a choice. The winner derives for every
    /// block of the captured mainnet window, and the distribution itself is
    /// checked against stacks-core's own `make_min_median_distribution`, so the
    /// count is only worth reporting.
    #[must_use]
    pub fn candidates(&self) -> usize {
        self.commitment_window
            .last()
            .map_or(0, |block| block.commitments.len())
    }

    #[must_use]
    pub const fn snapshots(&self) -> &SnapshotChain {
        &self.snapshots
    }

    /// The chain, for the two facts a resumed one is *told* rather than derives.
    pub const fn snapshots_mut(&mut self) -> &mut SnapshotChain {
        &mut self.snapshots
    }

    #[must_use]
    pub fn commitment_window(&self) -> &[CommitmentWindowBlock] {
        &self.commitment_window
    }

    /// Derive the tip's spends from the commitment window that weighed it.
    ///
    /// The tip's own burn block is the last entry of the commitment window —
    /// [`Self::append`] puts it there, and [`Self::prime`] ends with it for a chain
    /// seeded at a checkpoint — so this is the same data the distribution was
    /// weighed over, read again rather than recomputed from the burnchain.
    ///
    /// `None` where the winning commitment cannot be found among that block's
    /// eligible ones: a chain whose window has not been primed, or a burn block
    /// that elected nobody. The second is ordinary — mainnet leaves four such
    /// blocks in every fifteen — and no tenure stands on one, so nothing that could
    /// have an answer is denied one. See [`BurnSpends`] for why the pair is never
    /// split.
    fn spends_at_tip(&self) -> Option<BurnSpends> {
        let block = self.commitment_window.last()?;
        let winner = self.snapshots.tip().winner_txid?;
        let winner = block
            .commitments
            .iter()
            .find(|commitment| commitment.txid == winner)?;
        let total = block
            .commitments
            .iter()
            .try_fold(0_u64, |total, commitment| {
                total.checked_add(commitment.burn_sats)
            })?;
        Some(BurnSpends {
            total,
            winner: winner.burn_sats,
        })
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

    /// Extend the chain with a Bitcoin block, sampling over `window_len` blocks.
    ///
    /// `window_len` is [`PayoutSchedule::mining_window_at`] for this block's
    /// height, and it is not always [`MINING_COMMITMENT_WINDOW`]: a prepare phase
    /// and the blocks just after an epoch boundary weigh the block alone. The
    /// engine keeps the full six regardless, because the block after a one-block
    /// window needs them again.
    pub fn append(
        &mut self,
        block: &BitcoinBlock,
        accepted_operation_txids: &[[u8; 32]],
        commitments: CommitmentWindowBlock,
        pox_id: PoxId,
        window_len: usize,
    ) -> Result<&SortitionSnapshot, SortitionError> {
        let mut retained = self.commitment_window.clone();
        retained.push(commitments);
        if retained.len() > RETAINED_COMMITMENT_BLOCKS {
            retained.remove(0);
        }
        let window = &retained[retained.len().saturating_sub(window_len.max(1))..];
        let statistics = commitment_burn_statistics(window)?;
        let distribution = commitment_distribution(window)?;
        let next_sortition_hash = self
            .snapshots
            .tip()
            .sortition_hash
            .mix_bitcoin_header(BitcoinHeaderHash::from_bytes(block.hash));
        let previous_vrf_seed = self.snapshots.effective_winner_seed().unwrap_or([0; 32]);
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
                            vrf_public_key: distribution[index].candidate.vrf_public_key,
                            signing_key_hash: distribution[index].candidate.signing_key_hash,
                        },
                    )
                })
        });
        let (total_burn, winner) = winner.map_or_else(
            || (self.snapshots.tip().total_burn, None),
            |(total_burn, winner)| (total_burn, Some(winner)),
        );
        self.commitment_window = retained;
        self.snapshots.append_with_operations(
            block,
            accepted_operation_txids,
            total_burn,
            pox_id,
            winner,
        )?;
        // Derived here, where the window that weighed this block is the window's
        // last entry, and kept on the snapshot: by the time a follower executes
        // under this burn view the chain may stand several blocks further on.
        let spends = self.spends_at_tip();
        self.snapshots.record_tip_burn_spends(spends);
        Ok(self.snapshots.tip())
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
    SortitionSnapshot {
        bitcoin_timestamp: block.timestamp,
        ..SortitionSnapshot::genesis(block.height, bitcoin_header_hash)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CommitmentWindowBlock, MINING_COMMITMENT_WINDOW, MiningCommitment, PoxIdTracker,
        RewardCycleSchedule, SortitionEngine, SortitionHash, SortitionSnapshot,
        commitment_burn_statistics, commitment_distribution, select_epoch4_winner, select_winner,
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
                MINING_COMMITMENT_WINDOW,
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
                MINING_COMMITMENT_WINDOW,
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

    /// A reorganization must not leave the replacement branch weighed short.
    ///
    /// The commitment history is what refills the window, and retracting the top of
    /// it is the one moment the window can go short without any block being
    /// missing — which is a different sortition, not a rougher one. Keeping only
    /// the window itself meant the deepest admitted retraction emptied it.
    #[test]
    fn the_retained_history_refills_the_window_after_the_deepest_retraction() {
        let heights: Vec<u8> = (1..=20).collect();
        let mut engine = engine_over(&heights);
        assert_eq!(
            engine.commitment_window().len(),
            super::RETAINED_COMMITMENT_BLOCKS
        );

        let depth = u64::try_from(MINING_COMMITMENT_WINDOW).expect("window fits u64");
        let reorg = engine.retract_above(20 - depth).expect("retract the branch");
        assert_eq!(reorg.depth(), MINING_COMMITMENT_WINDOW);
        assert!(
            engine.commitment_window().len() + 1 >= MINING_COMMITMENT_WINDOW,
            "one replayed block refills the window, so it is weighed over all six"
        );
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

    /// A commitment's burn is what it paid out, never what it kept.
    ///
    /// Mainnet miners chain a change output tens of thousands of times larger
    /// than the commitment itself, so counting every output makes each
    /// candidate's weight the size of its wallet and the total burn absurd. The
    /// count of payout outputs is what separates the two, and it moves twice: in
    /// a prepare phase, where a commitment burns to one address, and under the
    /// waterfall, where it pays the one sBTC address.
    #[test]
    fn payout_outputs_follow_the_reward_cycle_and_the_waterfall() {
        // Mainnet: cycle 140 runs 960,050..962,149, its prepare phase the last
        // hundred of those. The first block of a cycle sits at offset 1, so the
        // prepare phase is offsets 2001..2099 plus the *next* cycle's offset 0 —
        // the "mod 0" block, which stacks-core's classic predicate counts as
        // prepare and which nano's earlier `offset >= 2000` rule got wrong at
        // both ends.
        let cycles =
            RewardCycleSchedule::new(666_050, 2100, Some(962_150)).expect("valid cycle schedule");
        let schedule = super::PayoutSchedule::new(cycles, 100).expect("valid schedule");
        assert_eq!(schedule.outputs_at(960_230), super::OUTPUTS_PER_COMMIT);
        assert_eq!(
            schedule.outputs_at(962_050),
            super::OUTPUTS_PER_COMMIT,
            "offset 2000 is the last reward-paying block, not the first prepare one"
        );
        assert_eq!(schedule.outputs_at(962_051), 1, "the prepare phase burns");
        assert_eq!(schedule.outputs_at(962_149), 1);
        assert_eq!(schedule.outputs_at(962_150), 1, "the waterfall pays one");
        // Past the waterfall the cycle phase no longer decides it.
        assert_eq!(schedule.outputs_at(963_000), 1);
        assert!(schedule.starts_reward_cycle(962_150), "cycle 141 opens here");
        assert!(!schedule.starts_reward_cycle(962_151));
        assert!(super::PayoutSchedule::new(cycles, 2100).is_err());
    }

    /// The mining window is six blocks except where stacks-core says otherwise.
    ///
    /// Both exceptions matter on mainnet, and getting either wrong changes which
    /// miner won: a one-block window is not a rougher answer than a six-block one.
    #[test]
    fn the_mining_window_collapses_in_a_prepare_phase_and_at_an_epoch_boundary() {
        let cycles =
            RewardCycleSchedule::new(666_050, 2100, Some(962_150)).expect("valid cycle schedule");
        let schedule = super::PayoutSchedule::new(cycles, 100).expect("valid schedule");
        // Without an epoch boundary to know about, only the prepare phase shortens
        // it — offsets 2001 upward, and the following cycle's mod 0 block.
        assert_eq!(schedule.mining_window_at(962_050), MINING_COMMITMENT_WINDOW);
        assert_eq!(schedule.mining_window_at(962_051), 1);
        assert_eq!(schedule.mining_window_at(962_149), 1);
        assert_eq!(schedule.mining_window_at(962_150), 1, "the mod 0 block");
        assert_eq!(schedule.mining_window_at(962_151), MINING_COMMITMENT_WINDOW);

        // Epoch 4.0 activates at mainnet burn 960,230. The epoch at the bottom of
        // the window is read seven blocks back, so the boundary is inside the
        // window for 960,230 through 960,236 and those sortitions are weighed on
        // their own block alone.
        let schedule = schedule.activating_epoch_four_at(960_230);
        assert_eq!(schedule.mining_window_at(960_229), MINING_COMMITMENT_WINDOW);
        for height in 960_230..=960_236 {
            assert_eq!(schedule.mining_window_at(height), 1, "{height}");
        }
        assert_eq!(schedule.mining_window_at(960_237), MINING_COMMITMENT_WINDOW);
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
            vrf_public_key: None,
            signing_key_hash: None,
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
                MINING_COMMITMENT_WINDOW,
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
            vrf_public_key: None,
            signing_key_hash: None,
            txid: [txid; 32],
            spent_txid: [spent_txid; 32],
            spent_output: 3,
            burn_sats,
            vrf_seed: [0; 32],
        }
    }

    fn bitcoin_block(height: u64, hash: u8) -> nano_bitcoin::BitcoinBlock {
        nano_bitcoin::BitcoinBlock {
            timestamp: 0,
            height,
            hash: [hash; 32],
            operations: Vec::new(),
        }
    }
}

/// The leader keys a burnchain has registered, and which have been spent.
///
/// A commitment is only an operation if it names a key that was registered and
/// has not been used before: stacks-core resolves `(key_block_height,
/// key_transaction_index)` against its burn database and drops a commitment
/// that does not. A node that keeps every commitment it can decode hashes a
/// different operation set and derives a different consensus hash — which is
/// how nano and mainnet parted at burn 960,230, on one commitment out of five.
///
/// The registry is also the *only* place a node standing on a checkpoint can
/// learn these from. A leader key is registered once on Bitcoin and named by
/// tens of thousands of commitments afterwards — the five keys mainnet's miners
/// used across the epoch 4.0 boundary were registered at burn 867,772 through
/// 939,759, twenty to ninety thousand blocks below it — so no burnchain window a
/// follower holds contains them. Carrying them is small: mainnet has 2,477 in
/// total.
#[derive(Clone, Debug, Default)]
pub struct LeaderKeys {
    registered: BTreeMap<(u64, u32), LeaderKeyRegistration>,
    spent: BTreeSet<(u64, u32)>,
}

/// What one leader-key registration binds, in the two fields consensus reads.
///
/// A registration authorises two different things and both are checked against
/// it: the VRF key that may produce the tenure's coinbase proof, and the
/// block-signing key hash that may sign the tenure's blocks. They come out of
/// the same Bitcoin transaction, so a node that can resolve one can resolve the
/// other, and a registry that carried only the first would have to be exported
/// again to check the second.
///
/// The signing hash is optional because the registrations from before Nakamoto
/// have no memo at all: of mainnet's 2,477 keys, 101 carry one — and every key
/// its 4.0 miners actually use is among them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LeaderKeyRegistration {
    pub vrf_public_key: [u8; 32],
    pub signing_key_hash: Option<[u8; 20]>,
}

impl LeaderKeys {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a registration a burn block made.
    pub fn register(
        &mut self,
        block_height: u64,
        transaction_index: u32,
        registration: LeaderKeyRegistration,
    ) {
        self.registered
            .insert((block_height, transaction_index), registration);
    }

    /// The VRF public key a commitment names, if it may still be used.
    #[must_use]
    pub fn usable(&self, block_height: u64, transaction_index: u32) -> Option<[u8; 32]> {
        self.registration(block_height, transaction_index)
            .map(|registration| registration.vrf_public_key)
    }

    /// The whole registration a commitment names, if it may still be used.
    #[must_use]
    pub fn registration(
        &self,
        block_height: u64,
        transaction_index: u32,
    ) -> Option<LeaderKeyRegistration> {
        let at = (block_height, transaction_index);
        if self.spent.contains(&at) {
            return None;
        }
        self.registered.get(&at).copied()
    }

    /// Every registration held, in burn order — which is how one is written down.
    pub fn entries(&self) -> impl Iterator<Item = (u64, u32, LeaderKeyRegistration)> + '_ {
        self.registered
            .iter()
            .map(|(&(height, index), registration)| (height, index, *registration))
    }

    /// Consume a key, so a later commitment naming it is not an operation.
    pub fn spend(&mut self, block_height: u64, transaction_index: u32) {
        self.spent.insert((block_height, transaction_index));
    }

    /// How many keys are registered and unspent.
    #[must_use]
    pub fn available(&self) -> usize {
        self.registered
            .keys()
            .filter(|at| !self.spent.contains(at))
            .count()
    }
}

#[cfg(test)]
mod leader_key_tests {
    use super::{LeaderKeyRegistration, LeaderKeys};

    /// A commitment may only name a key that was registered and not yet spent.
    ///
    /// mainnet drops one commitment of five at burn 960,230 for want of this,
    /// and a node that keeps it hashes a different operation set and derives a
    /// different consensus hash from there on.
    #[test]
    fn a_key_is_usable_once_and_only_after_it_is_registered() {
        let mut keys = LeaderKeys::new();
        assert_eq!(keys.usable(100, 7), None, "an unregistered key is unusable");

        keys.register(
            100,
            7,
            LeaderKeyRegistration {
                vrf_public_key: [0xab; 32],
                signing_key_hash: Some([0xcd; 20]),
            },
        );
        assert_eq!(keys.usable(100, 7), Some([0xab; 32]));
        assert_eq!(keys.available(), 1);
        // A different position is a different key, not the same one.
        assert_eq!(keys.usable(100, 8), None);
        // Both halves of the registration come back, because both are checked
        // against it: the VRF key produces the coinbase proof and the signing
        // hash signs the tenure's blocks.
        assert_eq!(
            keys.registration(100, 7).and_then(|key| key.signing_key_hash),
            Some([0xcd; 20])
        );

        keys.spend(100, 7);
        assert_eq!(keys.usable(100, 7), None, "a spent key is not usable again");
        assert_eq!(keys.available(), 0);
    }

    /// A registry is written down and read back whole, both halves.
    #[test]
    fn the_registry_is_walked_in_burn_order() {
        let mut keys = LeaderKeys::new();
        let registration = |byte: u8| LeaderKeyRegistration {
            vrf_public_key: [byte; 32],
            signing_key_hash: (byte != 0).then_some([byte; 20]),
        };
        keys.register(900, 4, registration(2));
        keys.register(100, 7, registration(1));
        keys.register(100, 2, registration(0));
        assert_eq!(
            keys.entries()
                .map(|(height, index, key)| (height, index, key.signing_key_hash.is_some()))
                .collect::<Vec<_>>(),
            vec![(100, 2, false), (100, 7, true), (900, 4, true)]
        );
    }
}

#[cfg(test)]
mod pox_id_tests {
    use super::{PoxId, sortition_id};
    use nano_primitives::BitcoinHeaderHash;

    /// The `PoX` history mainnet held at the epoch 4.0 boundary.
    ///
    /// A sortition identifier is the burn header hash and the `PoxId` hashed
    /// together, so the identifier a capture carries says which bit vector
    /// produced it — and mainnet's, at burn 960,219, is a hundred and
    /// forty-two bits, every one set. Every reward cycle mainnet has had chose
    /// an anchor block.
    ///
    /// Pinned because the consensus hash mixes it: without it every other
    /// field of a snapshot derives and this one does not.
    #[test]
    fn mainnet_pox_history_is_unbroken_at_the_epoch_four_boundary() {
        let header = BitcoinHeaderHash::from_bytes(
            <[u8; 32]>::try_from(
                hex::decode("00000000000000000000fbd11b102b2b1b9c85645d5b0dd8812d618e7a6ffd81")
                    .expect("hexadecimal")
                    .as_slice(),
            )
            .expect("32 bytes"),
        );

        assert_eq!(
            hex::encode(sortition_id(header, &PoxId::from_bits(vec![true; 142])).as_bytes()),
            "f49a1a55f7fa56cb1f5a27992ec2fec6545e94e1f37d82a3eb5485c3ec0c2f0c"
        );
        // One bit fewer or one unset is a different chain.
        assert_ne!(
            hex::encode(sortition_id(header, &PoxId::from_bits(vec![true; 141])).as_bytes()),
            "f49a1a55f7fa56cb1f5a27992ec2fec6545e94e1f37d82a3eb5485c3ec0c2f0c"
        );
    }

    /// A capture's own identifier says which bit vector produced it.
    ///
    /// This is how a node standing on a checkpoint learns the vector instead of
    /// configuring one, and getting it wrong is not subtle: the consensus hash
    /// mixes it, so `PoxId::initial()` derives a different hash at every block.
    #[test]
    fn a_captured_sortition_identifier_names_its_pox_history() {
        let header = BitcoinHeaderHash::from_bytes(
            <[u8; 32]>::try_from(
                hex::decode("00000000000000000000fbd11b102b2b1b9c85645d5b0dd8812d618e7a6ffd81")
                    .expect("hexadecimal")
                    .as_slice(),
            )
            .expect("32 bytes"),
        );
        let identifier = nano_primitives::SortitionId::from_bytes(
            <[u8; 32]>::try_from(
                hex::decode("f49a1a55f7fa56cb1f5a27992ec2fec6545e94e1f37d82a3eb5485c3ec0c2f0c")
                    .expect("hexadecimal")
                    .as_slice(),
            )
            .expect("32 bytes"),
        );
        assert_eq!(
            super::unbroken_pox_id_for(header, identifier, 256),
            Some(PoxId::from_bits(vec![true; 142]))
        );
        // A search too short to reach it finds nothing rather than something
        // close, and an identifier from another chain resolves to nothing.
        assert_eq!(super::unbroken_pox_id_for(header, identifier, 141), None);
        assert_eq!(
            super::unbroken_pox_id_for(
                header,
                nano_primitives::SortitionId::from_bytes([0; 32]),
                256
            ),
            None
        );
    }
}

#[cfg(test)]
mod missed_commit_tests {
    use super::{BURN_BLOCK_MINED_AT_MODULUS, commitment_is_on_time, commitment_miss_distance};

    /// A commitment is an operation only in the block after the one it names.
    #[test]
    fn a_commitment_lands_in_the_block_after_the_one_it_names() {
        // Burn 960,230 is 0 modulo five, so only a commitment built against
        // the block before it — modulus four — belongs there. That is exactly
        // the split mainnet made: four commitments accepted, one missed.
        assert!(commitment_is_on_time(4, 960_230));
        for late in [0, 1, 2, 3] {
            assert!(
                !commitment_is_on_time(late, 960_230),
                "modulus {late} is not aiming at burn 960,230"
            );
        }

        // The rule wraps, so the modulus before zero is the last one.
        assert!(commitment_is_on_time(
            u8::try_from(BURN_BLOCK_MINED_AT_MODULUS).expect("small") - 1,
            0
        ));
    }

    /// How late a commitment is, which decides whether it counts at all.
    ///
    /// One block late is a missed commitment whose UTXO still chains; more than
    /// one is refused outright, because a miner able to file arbitrarily late
    /// could bunch a whole window into one Bitcoin block.
    #[test]
    fn a_late_commitment_is_measured_in_blocks_and_refused_past_one() {
        // Burn 960,230 wants modulus 4. Modulus 3 aimed one block earlier, so it
        // is one late; modulus 2 is two late; and the count wraps at five.
        assert_eq!(commitment_miss_distance(4, 960_230), 0);
        assert_eq!(commitment_miss_distance(3, 960_230), 1);
        assert_eq!(commitment_miss_distance(2, 960_230), 2);
        assert_eq!(commitment_miss_distance(0, 960_230), 4);
        // Wrapping the other way: burn 960,231 wants modulus 0, and modulus 4
        // aimed at 960,230, one block earlier.
        assert_eq!(commitment_miss_distance(4, 960_231), 1);
    }
}
