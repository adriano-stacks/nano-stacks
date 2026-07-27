#![forbid(unsafe_code)]

use nano_bitcoin::BitcoinBlock;

/// The consensus context derived from a burn block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SortitionSnapshot {
    pub bitcoin_height: u64,
}

/// M0 placeholder. M6 replaces this with consensus sortition.
#[must_use]
pub const fn snapshot_for(block: &BitcoinBlock) -> SortitionSnapshot {
    SortitionSnapshot {
        bitcoin_height: block.height,
    }
}
