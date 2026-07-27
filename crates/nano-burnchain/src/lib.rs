#![forbid(unsafe_code)]

/// A burn block accepted by the HTTP/RPC ingest boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BurnBlock {
    pub height: u64,
}

/// The source boundary for burnchain input.
pub trait BurnchainSource {
    type Error;

    fn block_at(&self, height: u64) -> Result<BurnBlock, Self::Error>;
}
