#![forbid(unsafe_code)]

/// A Bitcoin block accepted by the HTTP/RPC ingest boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BitcoinBlock {
    pub height: u64,
}

/// The source boundary for Bitcoin input.
pub trait BitcoinSource {
    type Error;

    fn block_at(&self, height: u64) -> Result<BitcoinBlock, Self::Error>;
}
