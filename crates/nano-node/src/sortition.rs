//! The sortitions this node derives for itself.
//!
//! Asking a peer what the sortition was lets that peer choose this node's
//! consensus hashes, its winners and its fork. The arithmetic belongs here, and
//! a captured window of mainnet proves it produces what the network produced:
//! the same operations, operations hash, consensus hash, sortition identifier
//! and sortition hash, from the raw Bitcoin blocks and nothing else.
//!
//! A chain that starts at a checkpoint cannot derive a consensus hash from its
//! own snapshots — the hash mixes the ones at power-of-two offsets behind it,
//! reaching back thousands of blocks — so the checkpoint carries those hashes.
//! They are twenty bytes a block: mainnet's whole history is twelve megabytes.

use std::{fs, path::Path};

use nano_bitcoin::{BitcoinBlock, BitcoinOperationKind};
use nano_primitives::ConsensusHash;
use nano_sortition::{
    PoxId, SnapshotChain, SortitionError, SortitionSnapshot, SortitionWinner,
    commit_lands_in_block,
};
use serde::Deserialize;

/// Why a locally derived sortition chain could not be started or advanced.
#[derive(Debug)]
pub enum TrackerError {
    Seed(String),
    Sortition(SortitionError),
}

impl std::fmt::Display for TrackerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Seed(reason) => write!(formatter, "sortition seed: {reason}"),
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
#[derive(Debug, Deserialize)]
struct History {
    hashes: Vec<String>,
}

/// A snapshot chain this node advances from its own burnchain.
#[derive(Debug)]
pub struct SortitionTracker {
    chain: SnapshotChain,
    pox_id: PoxId,
}

impl SortitionTracker {
    /// Start from a seed snapshot and the consensus hashes behind it.
    pub fn new(
        seed: SortitionSnapshot,
        history: Vec<ConsensusHash>,
    ) -> Result<Self, TrackerError> {
        let pox_id = seed.pox_id.clone();
        let chain = SnapshotChain::with_history(seed, history).ok_or_else(|| {
            TrackerError::Seed("the history does not end at the snapshot it seeds".to_owned())
        })?;
        Ok(Self { chain, pox_id })
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
                let bytes = hex::decode(hash)
                    .map_err(|error| TrackerError::Seed(error.to_string()))?;
                <[u8; 20]>::try_from(bytes.as_slice())
                    .map(ConsensusHash::from_bytes)
                    .map_err(|_| TrackerError::Seed("a consensus hash is not 20 bytes".to_owned()))
            })
            .collect()
    }

    /// The snapshot this chain is standing on.
    #[must_use]
    pub fn tip(&self) -> &SortitionSnapshot {
        self.chain.tip()
    }

    /// Extend the chain with one Bitcoin block.
    ///
    /// `total_burn` is the running total the network keeps, which a node
    /// accumulates from the burn each block's operations spent.
    pub fn advance(
        &mut self,
        block: &BitcoinBlock,
        total_burn: u64,
    ) -> Result<&SortitionSnapshot, TrackerError> {
        let txids = operation_txids(block);
        let winner = winner_of(block);
        Ok(self.chain.append_with_operations(
            block,
            &txids,
            total_burn,
            self.pox_id.clone(),
            winner,
        )?)
    }
}

/// The transactions of a burn block that are operations for its sortition.
///
/// A commitment that arrived after the block it was aiming at is a *missed*
/// commitment: still a transaction, still able to chain its UTXO so the mining
/// window survives a gap, but not an operation and not part of the hash.
fn operation_txids(block: &BitcoinBlock) -> Vec<[u8; 32]> {
    block
        .operations
        .iter()
        .filter(|operation| match &operation.kind {
            BitcoinOperationKind::LeaderBlockCommit { parent_modulus, .. } => {
                commit_lands_in_block(*parent_modulus, block.height)
            }
            _ => true,
        })
        .map(|operation| operation.txid)
        .collect()
}

/// The commitment that won this block, if one did.
///
/// Choosing between several is the burn distribution's business; a block with
/// one eligible commitment has no choice to make, which is the common case and
/// the one this answers.
fn winner_of(block: &BitcoinBlock) -> Option<SortitionWinner> {
    let mut eligible = block.operations.iter().filter_map(|operation| {
        match (&operation.kind, commit_lands_in_block_of(operation, block)) {
            (BitcoinOperationKind::LeaderBlockCommit { new_seed, .. }, true) => {
                Some(SortitionWinner {
                    txid: operation.txid,
                    vrf_seed: *new_seed,
                })
            }
            _ => None,
        }
    });
    let first = eligible.next()?;
    eligible.next().is_none().then_some(first)
}

const fn commit_lands_in_block_of(
    operation: &nano_bitcoin::BitcoinOperation,
    block: &BitcoinBlock,
) -> bool {
    match &operation.kind {
        BitcoinOperationKind::LeaderBlockCommit { parent_modulus, .. } => {
            commit_lands_in_block(*parent_modulus, block.height)
        }
        _ => false,
    }
}

/// A snapshot a capture holds, in the fields a seed needs.
#[derive(Debug, Deserialize)]
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
    pub fn from_capture(
        directory: &Path,
        burn_height: u64,
        pox_id: PoxId,
    ) -> Result<Self, TrackerError> {
        let bytes = fs::read(directory.join("snapshots.json"))
            .map_err(|error| TrackerError::Seed(error.to_string()))?;
        let snapshots: Vec<CapturedSnapshot> = serde_json::from_slice(&bytes)
            .map_err(|error| TrackerError::Seed(error.to_string()))?;
        let seed = snapshots
            .iter()
            .find(|snapshot| snapshot.block_height == burn_height)
            .ok_or_else(|| TrackerError::Seed(format!("no snapshot at burn {burn_height}")))?;
        let history = Self::history_from(directory)?;
        Self::new(seed_snapshot(seed, pox_id)?, history)
    }
}

fn seed_snapshot(seed: &CapturedSnapshot, pox_id: PoxId) -> Result<SortitionSnapshot, TrackerError> {
    let bytes = |value: &str, name: &str| -> Result<Vec<u8>, TrackerError> {
        hex::decode(value).map_err(|_| TrackerError::Seed(format!("{name} is not hexadecimal")))
    };
    let thirty_two = |value: &str, name: &str| -> Result<[u8; 32], TrackerError> {
        <[u8; 32]>::try_from(bytes(value, name)?.as_slice())
            .map_err(|_| TrackerError::Seed(format!("{name} is not 32 bytes")))
    };
    Ok(SortitionSnapshot {
        bitcoin_height: seed.block_height,
        bitcoin_header_hash: nano_primitives::BitcoinHeaderHash::from_bytes(thirty_two(
            &seed.burn_header_hash,
            "burn header hash",
        )?),
        sortition_id: nano_primitives::SortitionId::from_bytes(thirty_two(
            &seed.sortition_id,
            "sortition id",
        )?),
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
        winner_vrf_seed: None,
        pox_id,
    })
}
