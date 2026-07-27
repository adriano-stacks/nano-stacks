#![forbid(unsafe_code)]

use nano_sortition::SortitionSnapshot;
use nano_vm::{ExecutionResult, execute_stub};

/// M0 boundary that makes the final validation stage explicit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppliedBlock {
    pub bitcoin_height: u64,
    pub execution: ExecutionResult,
}

#[must_use]
pub const fn append_stub(snapshot: &SortitionSnapshot) -> AppliedBlock {
    AppliedBlock {
        bitcoin_height: snapshot.bitcoin_height,
        execution: execute_stub(),
    }
}
