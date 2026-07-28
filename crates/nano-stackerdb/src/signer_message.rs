use std::fmt;

use nano_chainstate::NakamotoBlock;
use nano_crypto::MessageSignature;
use nano_primitives::Sha256Sum;

use crate::MAX_CHUNK_SIZE;

const BLOCK_RESPONSE_DATA_VERSION: u8 = 5;
const NOT_REJECTED: u8 = u8::MAX;

/// A proposal written by a miner to its `StackerDB` slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockProposal {
    pub block: NakamotoBlock,
    pub bitcoin_height: u64,
    pub reward_cycle: u64,
    /// The versioned proposal extension, retained byte-for-byte for forward compatibility.
    pub data: Vec<u8>,
}

/// A signer's acceptance of a proposed block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockAcceptance {
    pub signer_signature_hash: Sha256Sum,
    pub signature: MessageSignature,
    pub server_version: String,
    pub full_extend_timestamp: u64,
    pub read_count_extend_timestamp: u64,
}

impl BlockAcceptance {
    /// Construct the current-version acceptance message.
    #[must_use]
    pub fn new(signer_signature_hash: Sha256Sum, signature: MessageSignature) -> Self {
        Self {
            signer_signature_hash,
            signature,
            server_version: format!("nano-stacks/{}", env!("CARGO_PKG_VERSION")),
            full_extend_timestamp: u64::MAX,
            read_count_extend_timestamp: u64::MAX,
        }
    }
}

/// Signer messages used by the epoch-4 `StackerDB` protocol.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SignerMessage {
    BlockProposal(BlockProposal),
    BlockResponse(BlockAcceptance),
    BlockPushed(NakamotoBlock),
}

/// Wire prefixes for the signer messages this node consumes and produces.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum SignerMessageType {
    BlockProposal = 0,
    BlockResponse = 1,
    BlockPushed = 2,
}

/// Errors while decoding or encoding signer messages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SignerMessageError {
    Truncated,
    TrailingBytes,
    Oversized,
    InvalidMessageType(u8),
    InvalidResponseType(u8),
    InvalidText,
    Block(String),
}

impl fmt::Display for SignerMessageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => formatter.write_str("truncated signer message"),
            Self::TrailingBytes => formatter.write_str("signer message has trailing bytes"),
            Self::Oversized => formatter.write_str("signer message exceeds the protocol limit"),
            Self::InvalidMessageType(kind) => {
                write!(formatter, "unsupported signer message type {kind}")
            }
            Self::InvalidResponseType(kind) => {
                write!(formatter, "unsupported block response type {kind}")
            }
            Self::InvalidText => formatter.write_str("signer message contains invalid UTF-8"),
            Self::Block(error) => write!(formatter, "invalid proposed block: {error}"),
        }
    }
}

impl std::error::Error for SignerMessageError {}

impl SignerMessage {
    /// Decode a complete signer message from a `StackerDB` chunk payload.
    pub fn decode(bytes: &[u8]) -> Result<Self, SignerMessageError> {
        if bytes.len() > MAX_CHUNK_SIZE {
            return Err(SignerMessageError::Oversized);
        }
        let mut reader = Reader::new(bytes);
        let message = match reader.byte()? {
            0 => Self::BlockProposal(BlockProposal {
                block: reader.block()?,
                bitcoin_height: reader.u64()?,
                reward_cycle: reader.u64()?,
                data: reader.remaining().to_vec(),
            }),
            1 => {
                let response_type = reader.byte()?;
                if response_type != 0 {
                    return Err(SignerMessageError::InvalidResponseType(response_type));
                }
                let signer_signature_hash = Sha256Sum::from_bytes(reader.array()?);
                let signature = MessageSignature::from_bytes(reader.array()?);
                let server_version = reader.text()?;
                let (full_extend_timestamp, read_count_extend_timestamp) =
                    reader.response_data()?;
                Self::BlockResponse(BlockAcceptance {
                    signer_signature_hash,
                    signature,
                    server_version,
                    full_extend_timestamp,
                    read_count_extend_timestamp,
                })
            }
            2 => Self::BlockPushed(reader.block()?),
            kind => return Err(SignerMessageError::InvalidMessageType(kind)),
        };
        if !reader.is_empty() {
            return Err(SignerMessageError::TrailingBytes);
        }
        Ok(message)
    }

    /// Encode a signer message in the canonical epoch-4 wire format.
    pub fn encode(&self) -> Result<Vec<u8>, SignerMessageError> {
        let mut writer = Writer::default();
        match self {
            Self::BlockProposal(proposal) => {
                writer.byte(SignerMessageType::BlockProposal as u8);
                writer.raw(&proposal.block.encode());
                writer.u64(proposal.bitcoin_height);
                writer.u64(proposal.reward_cycle);
                writer.raw(&proposal.data);
            }
            Self::BlockResponse(response) => {
                writer.byte(SignerMessageType::BlockResponse as u8);
                writer.byte(0);
                writer.raw(response.signer_signature_hash.as_bytes());
                writer.raw(response.signature.as_bytes());
                writer.bytes(response.server_version.as_bytes())?;
                writer.response_data(response)?;
            }
            Self::BlockPushed(block) => {
                writer.byte(SignerMessageType::BlockPushed as u8);
                writer.raw(&block.encode());
            }
        }
        let bytes = writer.finish();
        if bytes.len() > MAX_CHUNK_SIZE {
            return Err(SignerMessageError::Oversized);
        }
        Ok(bytes)
    }
}

#[derive(Default)]
struct Writer(Vec<u8>);

impl Writer {
    fn byte(&mut self, value: u8) {
        self.0.push(value);
    }

    fn raw(&mut self, value: &[u8]) {
        self.0.extend_from_slice(value);
    }

    fn u32(&mut self, value: u32) {
        self.raw(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.raw(&value.to_be_bytes());
    }

    fn bytes(&mut self, value: &[u8]) -> Result<(), SignerMessageError> {
        let length = u32::try_from(value.len()).map_err(|_| SignerMessageError::Oversized)?;
        self.u32(length);
        self.raw(value);
        Ok(())
    }

    fn response_data(&mut self, response: &BlockAcceptance) -> Result<(), SignerMessageError> {
        let mut inner = Self::default();
        inner.u64(response.full_extend_timestamp);
        inner.byte(NOT_REJECTED);
        inner.u64(response.read_count_extend_timestamp);
        inner.byte(0);
        self.byte(BLOCK_RESPONSE_DATA_VERSION);
        self.bytes(&inner.finish())
    }

    fn finish(self) -> Vec<u8> {
        self.0
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn byte(&mut self) -> Result<u8, SignerMessageError> {
        Ok(self.take(1)?[0])
    }

    fn array<const LENGTH: usize>(&mut self) -> Result<[u8; LENGTH], SignerMessageError> {
        self.take(LENGTH)?
            .try_into()
            .map_err(|_| SignerMessageError::Truncated)
    }

    fn u32(&mut self) -> Result<u32, SignerMessageError> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, SignerMessageError> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    fn bytes(&mut self) -> Result<&'a [u8], SignerMessageError> {
        let length = usize::try_from(self.u32()?).map_err(|_| SignerMessageError::Oversized)?;
        if length > MAX_CHUNK_SIZE {
            return Err(SignerMessageError::Oversized);
        }
        self.take(length)
    }

    fn text(&mut self) -> Result<String, SignerMessageError> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| SignerMessageError::InvalidText)
    }

    fn block(&mut self) -> Result<NakamotoBlock, SignerMessageError> {
        let (block, consumed) = NakamotoBlock::decode_prefix(self.remaining())
            .map_err(|error| SignerMessageError::Block(error.to_string()))?;
        self.offset = self
            .offset
            .checked_add(consumed)
            .ok_or(SignerMessageError::Truncated)?;
        Ok(block)
    }

    fn response_data(&mut self) -> Result<(u64, u64), SignerMessageError> {
        let version = self.byte()?;
        let bytes = self.bytes()?;
        let mut data = Self::new(bytes);
        let timestamp = data.u64()?;
        if data.byte()? != NOT_REJECTED {
            return Err(SignerMessageError::InvalidResponseType(1));
        }
        if version < 4 {
            return Ok((timestamp, u64::MAX));
        }
        let read_count_timestamp = data.u64()?;
        if version >= 5 && data.byte()? != 0 {
            data.take(32)?;
        }
        Ok((timestamp, read_count_timestamp))
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], SignerMessageError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(SignerMessageError::Truncated)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(SignerMessageError::Truncated)?;
        self.offset = end;
        Ok(value)
    }

    fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.offset..]
    }

    const fn is_empty(&self) -> bool {
        self.offset == self.bytes.len()
    }
}
