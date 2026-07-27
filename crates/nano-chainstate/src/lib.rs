#![forbid(unsafe_code)]

mod nakamoto;

pub use nakamoto::{
    NakamotoBlock, NakamotoBlockHeader, NakamotoCodecError, Signer, SignerSet, SignerSetError,
    TenureError,
};

use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::errors::ClarityEvalError;
use nano_sortition::SortitionSnapshot;
use nano_vm::{ExecutionResult, MarfStoreError, Vm};

/// M0 boundary that makes the final validation stage explicit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppliedBlock {
    pub bitcoin_height: u64,
    pub execution: ExecutionResult,
}

/// A chainstate execution context backed by versioned VM state.
#[derive(Debug)]
pub struct ChainState {
    vm: Vm,
}

#[derive(Debug)]
pub enum ChainStateError {
    Storage(MarfStoreError),
    Evaluation(ClarityEvalError),
}

impl std::fmt::Display for ChainStateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "state storage error: {error}"),
            Self::Evaluation(error) => write!(formatter, "Clarity evaluation error: {error}"),
        }
    }
}

impl std::error::Error for ChainStateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            Self::Evaluation(_) => None,
        }
    }
}

impl From<MarfStoreError> for ChainStateError {
    fn from(error: MarfStoreError) -> Self {
        Self::Storage(error)
    }
}

impl From<ClarityEvalError> for ChainStateError {
    fn from(error: ClarityEvalError) -> Self {
        Self::Evaluation(error)
    }
}

impl ChainState {
    /// Create an empty chainstate.
    pub fn new() -> Result<Self, ChainStateError> {
        Ok(Self { vm: Vm::new()? })
    }

    /// Execute a Clarity program for a block and seal its consensus state root.
    pub fn append_program(
        &mut self,
        snapshot: &SortitionSnapshot,
        parent: Option<[u8; 32]>,
        block: [u8; 32],
        source: &str,
    ) -> Result<AppliedBlock, ChainStateError> {
        self.vm.begin_block(parent, block)?;
        self.vm.execute(source, LimitedCostTracker::new_free())?;
        let state_root = self.vm.seal_block()?;
        Ok(AppliedBlock {
            bitcoin_height: snapshot.bitcoin_height,
            execution: ExecutionResult { state_root },
        })
    }
}

#[must_use]
pub const fn append_stub(snapshot: &SortitionSnapshot) -> AppliedBlock {
    AppliedBlock {
        bitcoin_height: snapshot.bitcoin_height,
        execution: ExecutionResult {
            state_root: nano_marf::StateRoot::empty(),
        },
    }
}

#[cfg(test)]
mod tests {
    use nano_primitives::BitcoinHeaderHash;
    use nano_sortition::SortitionSnapshot;

    use super::ChainState;

    #[test]
    fn append_program_seals_the_vm_state_root() {
        let snapshot = SortitionSnapshot::genesis(42, BitcoinHeaderHash::from_bytes([0; 32]));
        let mut chainstate = ChainState::new().expect("create chainstate");

        let applied = chainstate
            .append_program(
                &snapshot,
                None,
                [1; 32],
                "(define-data-var counter uint u1) (var-set counter u2) (var-get counter)",
            )
            .expect("append program");

        assert_eq!(applied.bitcoin_height, 42);
        assert_ne!(applied.execution.state_root, nano_marf::StateRoot::empty());
    }
}
