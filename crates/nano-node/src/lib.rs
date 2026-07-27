#![forbid(unsafe_code)]

use nano_bitcoin::{BitcoinBlock, BitcoinSource};
use nano_chainstate::append_stub;
use nano_sortition::snapshot_for;

/// Result used by the offline replay harness to name the first divergence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplayFailure {
    BitcoinInput,
    StateRoot,
}

/// M0 replay boundary. The expected block cannot be processed until fixtures
/// and consensus components are implemented, so block one is the baseline.
pub fn replay_one<S: BitcoinSource>(source: &S, height: u64) -> Result<(), ReplayFailure> {
    let block = source
        .block_at(height)
        .map_err(|_| ReplayFailure::BitcoinInput)?;
    let snapshot = snapshot_for(&block);
    let _applied = append_stub(&snapshot);
    Err(ReplayFailure::StateRoot)
}

/// A deterministic fixture source used only by the M0 baseline test.
pub struct BaselineSource;

impl BitcoinSource for BaselineSource {
    type Error = core::convert::Infallible;

    fn block_at(&self, height: u64) -> Result<BitcoinBlock, Self::Error> {
        Ok(BitcoinBlock { height })
    }
}

#[cfg(test)]
mod tests {
    use super::{BaselineSource, ReplayFailure, replay_one};

    #[test]
    fn baseline_replay_diverges_at_the_state_root() {
        assert_eq!(
            replay_one(&BaselineSource, 1),
            Err(ReplayFailure::StateRoot)
        );
    }
}
