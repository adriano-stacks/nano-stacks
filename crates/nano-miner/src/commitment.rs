//! Derivation of the leader commitment a miner must publish for the next Bitcoin block.

use std::fmt;

use nano_address::PoxAddress;
use nano_bitcoin::{
    BitcoinOperation, BitcoinOperationKind, BitcoinRpcSourceError, BitcoinSource,
    LeaderBlockCommitment,
};
use nano_chainstate::NakamotoBlock;
use nano_codec::TransactionPayloadData;
use nano_primitives::{Hash160, sha512_256};
use nano_sync::{SortitionInfo, SyncClient, SyncError};

/// The epoch marker every epoch-4 commitment must carry.
pub const EPOCH_4_MARKER: u8 = 0x11;

/// Bitcoin block heights are taken modulo this value to pin a commitment to one block.
const BITCOIN_BLOCK_MINED_AT_MODULUS: u64 = 5;

/// The confirmed Bitcoin position of a miner's leader-key registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegisteredLeaderKey {
    pub bitcoin_height: u32,
    pub transaction_index: u16,
}

/// A commitment together with the payout and Bitcoin height it is only valid at.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitmentPlan {
    pub commitment: LeaderBlockCommitment,
    pub sbtc_address: PoxAddress,
    pub target_bitcoin_height: u64,
    pub reward_cycle: u64,
}

#[derive(Debug)]
pub enum CommitmentPlanError {
    Sync(SyncError),
    Bitcoin(BitcoinRpcSourceError),
    NoParentSortition,
    MissingVrfProof,
    ParentCommitmentNotFound,
    AmbiguousParentCommitment,
    HeightOverflow,
}

impl fmt::Display for CommitmentPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sync(error) => write!(formatter, "node query failed: {error}"),
            Self::Bitcoin(error) => write!(formatter, "Bitcoin query failed: {error}"),
            Self::NoParentSortition => {
                formatter.write_str("the tip tenure has no winning Bitcoin sortition")
            }
            Self::MissingVrfProof => {
                formatter.write_str("the tip tenure's start block carries no VRF proof")
            }
            Self::ParentCommitmentNotFound => {
                formatter.write_str("the parent tenure's winning commitment is not on Bitcoin")
            }
            Self::AmbiguousParentCommitment => {
                formatter.write_str("the parent tenure's winning commitment is ambiguous")
            }
            Self::HeightOverflow => formatter.write_str("Bitcoin height overflow"),
        }
    }
}

impl std::error::Error for CommitmentPlanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sync(error) => Some(error),
            Self::Bitcoin(error) => Some(error),
            _ => None,
        }
    }
}

impl From<SyncError> for CommitmentPlanError {
    fn from(error: SyncError) -> Self {
        Self::Sync(error)
    }
}

impl From<BitcoinRpcSourceError> for CommitmentPlanError {
    fn from(error: BitcoinRpcSourceError) -> Self {
        Self::Bitcoin(error)
    }
}

/// Derive the commitment that extends the peer's canonical tenure at the next Bitcoin block.
pub async fn plan_commitment<S>(
    node: &SyncClient,
    bitcoin: &mut S,
    key: RegisteredLeaderKey,
) -> Result<CommitmentPlan, CommitmentPlanError>
where
    S: BitcoinSource,
    CommitmentPlanError: From<S::Error>,
{
    let bitcoin_tip = node.sortition_tip().await?;
    let tenure = node.tenure_info().await?;
    let tenure_start = node.block(tenure.tenure_start_block_id).await?;
    let parent = node.sortition(tenure.consensus_hash).await?;
    let target_bitcoin_height = bitcoin_tip
        .bitcoin_height
        .checked_add(1)
        .ok_or(CommitmentPlanError::HeightOverflow)?;
    let reward_cycle = node.pox_info().await?.reward_cycle(target_bitcoin_height);

    Ok(CommitmentPlan {
        commitment: LeaderBlockCommitment {
            block_header_hash: *tenure.tenure_start_block_id.as_bytes(),
            new_seed: tenure_seed(&tenure_start)?,
            parent_block_height: u32::try_from(parent.bitcoin_height)
                .map_err(|_| CommitmentPlanError::HeightOverflow)?,
            parent_transaction_index: parent_transaction_index(bitcoin, &parent)?,
            key_block_height: key.bitcoin_height,
            key_transaction_index: key.transaction_index,
            memo: EPOCH_4_MARKER,
            parent_modulus: u8::try_from(bitcoin_tip.bitcoin_height % BITCOIN_BLOCK_MINED_AT_MODULUS)
                .expect("a Bitcoin modulus below five fits in a byte"),
        },
        sbtc_address: node.stacker_set(reward_cycle).await?.sbtc_address,
        target_bitcoin_height,
        reward_cycle,
    })
}

/// The seed a commitment must carry is the hash of the tenure it builds upon.
fn tenure_seed(tenure_start: &NakamotoBlock) -> Result<[u8; 32], CommitmentPlanError> {
    tenure_start
        .transactions
        .iter()
        .find_map(|transaction| match transaction.payload().data() {
            TransactionPayloadData::NakamotoCoinbase { vrf_proof, .. } => Some(*vrf_proof),
            _ => None,
        })
        .map(|proof| *sha512_256(&proof).as_bytes())
        .ok_or(CommitmentPlanError::MissingVrfProof)
}

/// Locate the winning commitment of a sortition among the Bitcoin operations that produced it.
fn parent_transaction_index<S>(
    bitcoin: &mut S,
    parent: &SortitionInfo,
) -> Result<u16, CommitmentPlanError>
where
    S: BitcoinSource,
    CommitmentPlanError: From<S::Error>,
{
    let (Some(committed_block_hash), Some(vrf_seed), Some(miner_key_hash)) = (
        parent.committed_block_hash,
        parent.vrf_seed,
        parent.miner_public_key_hash,
    ) else {
        return Err(CommitmentPlanError::NoParentSortition);
    };
    let operations = bitcoin.block_at(parent.bitcoin_height)?.operations;
    let mut candidates = operations.iter().filter(|operation| {
        matches!(
            operation.kind,
            BitcoinOperationKind::LeaderBlockCommit {
                block_header_hash,
                new_seed,
                ..
            } if block_header_hash == *committed_block_hash.as_bytes() && new_seed == vrf_seed
        )
    });
    let Some(first) = candidates.next() else {
        return Err(CommitmentPlanError::ParentCommitmentNotFound);
    };
    if candidates.next().is_none() {
        return transaction_index(first);
    }

    // Competing miners commit to the same tenure with the same seed, so only the
    // registered block-signing key the node reported tells them apart.
    let mut matching = operations
        .iter()
        .filter(|operation| signing_key_hash(bitcoin, operation) == Some(miner_key_hash));
    let winner = matching
        .next()
        .ok_or(CommitmentPlanError::ParentCommitmentNotFound)?;
    if matching.next().is_some() {
        return Err(CommitmentPlanError::AmbiguousParentCommitment);
    }
    transaction_index(winner)
}

/// Resolve the block-signing key a commitment inherits from its leader-key registration.
fn signing_key_hash<S: BitcoinSource>(
    bitcoin: &mut S,
    operation: &BitcoinOperation,
) -> Option<Hash160> {
    let BitcoinOperationKind::LeaderBlockCommit {
        key_block_height,
        key_transaction_index,
        ..
    } = operation.kind
    else {
        return None;
    };
    bitcoin
        .block_at(u64::from(key_block_height))
        .ok()?
        .operations
        .iter()
        .find(|candidate| candidate.transaction_index == u32::from(key_transaction_index))
        .and_then(|candidate| match candidate.kind {
            BitcoinOperationKind::LeaderKeyRegistration {
                block_signing_key_hash,
                ..
            } => block_signing_key_hash.map(Hash160::from_bytes),
            _ => None,
        })
}

fn transaction_index(operation: &BitcoinOperation) -> Result<u16, CommitmentPlanError> {
    u16::try_from(operation.transaction_index).map_err(|_| CommitmentPlanError::HeightOverflow)
}

#[cfg(test)]
mod tests {
    use nano_bitcoin::{BitcoinBlock, BitcoinOperation, BitcoinOperationKind, BitcoinSource};
    use nano_primitives::{BlockHeaderHash, ConsensusHash, Hash160, SortitionId};
    use nano_sync::SortitionInfo;

    use super::{CommitmentPlanError, parent_transaction_index};

    struct FixedBitcoin(Vec<BitcoinBlock>);

    impl BitcoinSource for FixedBitcoin {
        type Error = CommitmentPlanError;

        fn block_at(&mut self, height: u64) -> Result<BitcoinBlock, Self::Error> {
            self.0
                .iter()
                .find(|block| block.height == height)
                .cloned()
                .ok_or(CommitmentPlanError::ParentCommitmentNotFound)
        }
    }

    fn commitment(transaction_index: u32, key_transaction_index: u16) -> BitcoinOperation {
        BitcoinOperation {
            txid: [transaction_index.to_be_bytes()[3]; 32],
            transaction_index,
            inputs: Vec::new(),
            outputs: Vec::new(),
            kind: BitcoinOperationKind::LeaderBlockCommit {
                block_header_hash: [7; 32],
                new_seed: [8; 32],
                parent_block_height: 1,
                parent_transaction_index: 0,
                key_block_height: 1,
                key_transaction_index,
                memo: 0x11,
                parent_modulus: 0,
            },
        }
    }

    fn registration(transaction_index: u32, key_hash: [u8; 20]) -> BitcoinOperation {
        BitcoinOperation {
            txid: [transaction_index.to_be_bytes()[3]; 32],
            transaction_index,
            inputs: Vec::new(),
            outputs: Vec::new(),
            kind: BitcoinOperationKind::LeaderKeyRegistration {
                consensus_hash: [0; 20],
                vrf_public_key: [0; 32],
                block_signing_key_hash: Some(key_hash),
                memo: Vec::new(),
            },
        }
    }

    fn sortition(height: u64) -> SortitionInfo {
        SortitionInfo {
            bitcoin_block_hash: nano_primitives::BitcoinHeaderHash::from_bytes([0; 32]),
            bitcoin_height: height,
            bitcoin_timestamp: 0,
            sortition_id: SortitionId::from_bytes([0; 32]),
            parent_sortition_id: SortitionId::from_bytes([0; 32]),
            consensus_hash: ConsensusHash::from_bytes([0; 20]),
            was_sortition: true,
            miner_public_key_hash: Some(Hash160::from_bytes([9; 20])),
            stacks_parent_consensus_hash: None,
            last_sortition_consensus_hash: None,
            committed_block_hash: Some(BlockHeaderHash::from_bytes([7; 32])),
            vrf_seed: Some([8; 32]),
        }
    }

    #[test]
    fn a_single_matching_commitment_identifies_the_winner() {
        let mut bitcoin = FixedBitcoin(vec![BitcoinBlock {
            height: 2,
            hash: [0; 32],
            operations: vec![commitment(3, 0)],
        }]);

        assert_eq!(
            parent_transaction_index(&mut bitcoin, &sortition(2)).expect("winning commitment"),
            3
        );
    }

    #[test]
    fn competing_commitments_are_resolved_by_the_registered_signing_key() {
        let mut bitcoin = FixedBitcoin(vec![
            BitcoinBlock {
                height: 1,
                hash: [0; 32],
                operations: vec![registration(0, [1; 20]), registration(1, [9; 20])],
            },
            BitcoinBlock {
                height: 2,
                hash: [0; 32],
                operations: vec![commitment(4, 0), commitment(5, 1)],
            },
        ]);

        assert_eq!(
            parent_transaction_index(&mut bitcoin, &sortition(2)).expect("winning commitment"),
            5
        );
    }
}
