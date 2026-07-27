#![forbid(unsafe_code)]

use nano_marf::StateRoot;

/// M0 execution output. M8/M10 replace the marker with Clarity receipts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionResult {
    pub state_root: StateRoot,
}

#[must_use]
pub const fn execute_stub() -> ExecutionResult {
    ExecutionResult {
        state_root: StateRoot::empty(),
    }
}
