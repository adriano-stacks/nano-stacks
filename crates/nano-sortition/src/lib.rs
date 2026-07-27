#![forbid(unsafe_code)]

use std::fmt;

use nano_bitcoin::BitcoinBlock;
use nano_primitives::{BitcoinHeaderHash, ConsensusHash, hash160, sha256};

const SYSTEM_FORK_SET_VERSION: [u8; 4] = [23, 0, 0, 0];

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
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
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
    pub fn as_consensus_bytes(&self) -> Vec<u8> {
        self.0
            .iter()
            .map(|present| if *present { b'1' } else { b'0' })
            .collect()
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
    pub operations_hash: OpsHash,
    pub consensus_hash: ConsensusHash,
    pub total_burn: u64,
    pub sortition_hash: SortitionHash,
    pub pox_id: PoxId,
}

impl SortitionSnapshot {
    #[must_use]
    pub fn genesis(bitcoin_height: u64, bitcoin_header_hash: BitcoinHeaderHash) -> Self {
        Self {
            bitcoin_height,
            bitcoin_header_hash,
            operations_hash: OpsHash([0; 32]),
            consensus_hash: ConsensusHash::from_bytes([0; 20]),
            total_burn: 0,
            sortition_hash: SortitionHash::initial(),
            pox_id: PoxId::initial(),
        }
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

    pub fn append(
        &mut self,
        block: &BitcoinBlock,
        total_burn: u64,
        pox_id: PoxId,
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

        let operations_hash = OpsHash::from_txids(
            &block
                .operations
                .iter()
                .map(|operation| operation.txid)
                .collect::<Vec<_>>(),
        );
        let bitcoin_header_hash = BitcoinHeaderHash::from_bytes(block.hash);
        let consensus_hash = consensus_hash(
            bitcoin_header_hash,
            operations_hash,
            total_burn,
            &self.previous_consensus_hashes(),
            &pox_id,
        );
        let snapshot = SortitionSnapshot {
            bitcoin_height: block.height,
            bitcoin_header_hash,
            operations_hash,
            consensus_hash,
            total_burn,
            sortition_hash: parent
                .sortition_hash
                .mix_bitcoin_header(bitcoin_header_hash),
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
    HeightOverflow,
    UnexpectedHeight { expected: u64, actual: u64 },
}

impl fmt::Display for SortitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeightOverflow => formatter.write_str("Bitcoin height overflow"),
            Self::UnexpectedHeight { expected, actual } => {
                write!(
                    formatter,
                    "expected Bitcoin height {expected}, got {actual}"
                )
            }
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
