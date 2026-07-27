#![forbid(unsafe_code)]

/// A state-root placeholder. M7 replaces this with the bit-exact MARF root.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StateRoot(pub [u8; 32]);

impl StateRoot {
    #[must_use]
    pub const fn empty() -> Self {
        Self([0; 32])
    }
}
