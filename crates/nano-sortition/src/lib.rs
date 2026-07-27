#![forbid(unsafe_code)]

use nano_bitcoin::BitcoinBlock;
use nano_primitives::{BitcoinHeaderHash, sha256};

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

/// The consensus context derived from a burn block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SortitionSnapshot {
    pub bitcoin_height: u64,
    pub operations_hash: OpsHash,
    pub sortition_hash: SortitionHash,
}

/// M0 placeholder. M6 replaces this with consensus sortition.
#[must_use]
pub fn snapshot_for(block: &BitcoinBlock) -> SortitionSnapshot {
    SortitionSnapshot {
        bitcoin_height: block.height,
        operations_hash: OpsHash::from_txids(
            &block
                .operations
                .iter()
                .map(|operation| operation.txid)
                .collect::<Vec<_>>(),
        ),
        sortition_hash: SortitionHash::initial()
            .mix_bitcoin_header(BitcoinHeaderHash::from_bytes(block.hash)),
    }
}
