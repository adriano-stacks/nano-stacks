use std::fmt;

use nano_chainstate::NakamotoBlock;
use nano_codec::Transaction;
use nano_crypto::MessageSignature;
use nano_primitives::{ConsensusHash, Hash160, Sha256Sum, StacksBlockId};

use crate::MAX_CHUNK_SIZE;

/// The newest signer protocol version this node speaks.
pub const LATEST_SIGNER_PROTOCOL_VERSION: u64 = 2;

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

impl BlockProposal {
    /// Return the canonical version-1 empty proposal extension accepted by stock signers.
    #[must_use]
    pub fn empty_data() -> Vec<u8> {
        vec![1, 0, 0, 0, 4, 0, 0, 0, 0]
    }
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
    /// Construct an acceptance that offers no opinion on tenure extends.
    ///
    /// A miner takes the weighted timestamp at which threshold signing power
    /// would accept an extension, so a signer that answers with the maximum
    /// keeps every miner on the network from ever extending a tenure. Only use
    /// this where no tenure is being answered for.
    #[must_use]
    pub fn new(signer_signature_hash: Sha256Sum, signature: MessageSignature) -> Self {
        Self::with_extend_timestamp(signer_signature_hash, signature, u64::MAX)
    }

    /// Construct an acceptance that also says when this signer would accept a
    /// time-based tenure extension.
    #[must_use]
    pub fn with_extend_timestamp(
        signer_signature_hash: Sha256Sum,
        signature: MessageSignature,
        extend_timestamp: u64,
    ) -> Self {
        Self {
            signer_signature_hash,
            signature,
            server_version: format!("nano-stacks/{}", env!("CARGO_PKG_VERSION")),
            full_extend_timestamp: extend_timestamp,
            read_count_extend_timestamp: extend_timestamp,
        }
    }
}

/// A signed signer rejection, preserving its versioned response data verbatim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockRejection {
    pub reason: String,
    pub reason_code: Vec<u8>,
    pub signer_signature_hash: Sha256Sum,
    pub chain_id: u32,
    pub signature: MessageSignature,
    pub server_version: String,
    pub data: Vec<u8>,
}

/// The signer state a peer publishes so the reward set can agree on a protocol
/// version and on who the current miner is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateMachineUpdate {
    pub active_protocol_version: u64,
    pub local_supported_protocol_version: u64,
    pub bitcoin_consensus_hash: ConsensusHash,
    pub bitcoin_height: u64,
    pub current_miner: CurrentMiner,
    /// Transactions the reward set agreed to replay; empty outside a replay.
    pub replay_transactions: Vec<Transaction>,
}

/// The miner a signer believes owns the current tenure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CurrentMiner {
    None,
    Active {
        public_key_hash: Hash160,
        tenure_consensus_hash: ConsensusHash,
        parent_tenure_consensus_hash: ConsensusHash,
        parent_tenure_last_block: StacksBlockId,
        parent_tenure_last_block_height: u64,
    },
}

/// Signer messages used by the epoch-4 `StackerDB` protocol.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SignerMessage {
    BlockProposal(BlockProposal),
    BlockResponse(BlockResponse),
    BlockPushed(NakamotoBlock),
    StateMachineUpdate(StateMachineUpdate),
    /// A signer's promise to sign a block, published before its signature.
    BlockPreCommit(Sha256Sum),
}

/// A signer's response to a proposed block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlockResponse {
    Accepted(BlockAcceptance),
    Rejected(BlockRejection),
}

/// Wire prefixes for the signer messages this node consumes and produces.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum SignerMessageType {
    BlockProposal = 0,
    BlockResponse = 1,
    BlockPushed = 2,
    StateMachineUpdate = 6,
    BlockPreCommit = 7,
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
    InvalidProposalData,
    InvalidMinerState(u8),
    UnsupportedProtocolVersion(u64),
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
            Self::InvalidProposalData => formatter.write_str("invalid block proposal data"),
            Self::InvalidMinerState(variant) => {
                write!(formatter, "unsupported signer miner state {variant}")
            }
            Self::UnsupportedProtocolVersion(version) => {
                write!(formatter, "unsupported signer protocol version {version}")
            }
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
            0 => {
                let block = reader.block()?;
                let bitcoin_height = reader.u64()?;
                let reward_cycle = reader.u64()?;
                let remaining = reader.remaining().len();
                let data = reader.take(remaining)?.to_vec();
                Self::BlockProposal(BlockProposal {
                    block,
                    bitcoin_height,
                    reward_cycle,
                    data,
                })
            }
            1 => {
                let response_type = reader.byte()?;
                Self::BlockResponse(match response_type {
                    0 => {
                        let signer_signature_hash = Sha256Sum::from_bytes(reader.array()?);
                        let signature = MessageSignature::from_bytes(reader.array()?);
                        let server_version = reader.text()?;
                        let (full_extend_timestamp, read_count_extend_timestamp) =
                            reader.response_data()?;
                        BlockResponse::Accepted(BlockAcceptance {
                            signer_signature_hash,
                            signature,
                            server_version,
                            full_extend_timestamp,
                            read_count_extend_timestamp,
                        })
                    }
                    1 => BlockResponse::Rejected(BlockRejection {
                        reason: reader.text()?,
                        reason_code: reader.rejection_code()?,
                        signer_signature_hash: Sha256Sum::from_bytes(reader.array()?),
                        chain_id: reader.u32()?,
                        signature: MessageSignature::from_bytes(reader.array()?),
                        server_version: reader.text()?,
                        data: reader.rejection_data()?,
                    }),
                    kind => return Err(SignerMessageError::InvalidResponseType(kind)),
                })
            }
            2 => Self::BlockPushed(reader.block()?),
            6 => Self::StateMachineUpdate(reader.state_machine_update()?),
            7 => Self::BlockPreCommit(Sha256Sum::from_bytes(reader.array()?)),
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
                if proposal.data.is_empty() {
                    return Err(SignerMessageError::InvalidProposalData);
                }
                writer.byte(SignerMessageType::BlockProposal as u8);
                writer.raw(&proposal.block.encode());
                writer.u64(proposal.bitcoin_height);
                writer.u64(proposal.reward_cycle);
                writer.raw(&proposal.data);
            }
            Self::BlockResponse(BlockResponse::Accepted(response)) => {
                writer.byte(SignerMessageType::BlockResponse as u8);
                writer.byte(0);
                writer.raw(response.signer_signature_hash.as_bytes());
                writer.raw(response.signature.as_bytes());
                writer.bytes(response.server_version.as_bytes())?;
                writer.response_data(response)?;
            }
            Self::BlockResponse(BlockResponse::Rejected(response)) => {
                writer.byte(SignerMessageType::BlockResponse as u8);
                writer.byte(1);
                writer.bytes(response.reason.as_bytes())?;
                writer.raw(&response.reason_code);
                writer.raw(response.signer_signature_hash.as_bytes());
                writer.u32(response.chain_id);
                writer.raw(response.signature.as_bytes());
                writer.bytes(response.server_version.as_bytes())?;
                writer.raw(&response.data);
            }
            Self::BlockPushed(block) => {
                writer.byte(SignerMessageType::BlockPushed as u8);
                writer.raw(&block.encode());
            }
            Self::BlockPreCommit(signer_signature_hash) => {
                writer.byte(SignerMessageType::BlockPreCommit as u8);
                writer.raw(signer_signature_hash.as_bytes());
            }
            Self::StateMachineUpdate(update) => {
                writer.byte(SignerMessageType::StateMachineUpdate as u8);
                writer.state_machine_update(update)?;
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

    fn state_machine_update(
        &mut self,
        update: &StateMachineUpdate,
    ) -> Result<(), SignerMessageError> {
        let version = update
            .active_protocol_version
            .min(update.local_supported_protocol_version);
        if version > LATEST_SIGNER_PROTOCOL_VERSION {
            return Err(SignerMessageError::UnsupportedProtocolVersion(version));
        }
        self.u64(update.active_protocol_version);
        self.u64(update.local_supported_protocol_version);

        let mut content = Self::default();
        content.raw(update.bitcoin_consensus_hash.as_bytes());
        content.u64(update.bitcoin_height);
        match &update.current_miner {
            CurrentMiner::None => content.byte(0),
            CurrentMiner::Active {
                public_key_hash,
                tenure_consensus_hash,
                parent_tenure_consensus_hash,
                parent_tenure_last_block,
                parent_tenure_last_block_height,
            } => {
                content.byte(1);
                content.raw(public_key_hash.as_bytes());
                content.raw(tenure_consensus_hash.as_bytes());
                content.raw(parent_tenure_consensus_hash.as_bytes());
                content.raw(parent_tenure_last_block.as_bytes());
                content.u64(*parent_tenure_last_block_height);
            }
        }
        if version >= 1 {
            let count = u32::try_from(update.replay_transactions.len())
                .map_err(|_| SignerMessageError::Oversized)?;
            content.u32(count);
            for transaction in &update.replay_transactions {
                content.raw(transaction.as_bytes());
            }
        }
        self.bytes(&content.finish())
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
            if !data.is_empty() {
                return Err(SignerMessageError::TrailingBytes);
            }
            return Ok((timestamp, u64::MAX));
        }
        let read_count_timestamp = data.u64()?;
        if version >= 5 && data.byte()? != 0 {
            data.take(32)?;
        }
        if !data.is_empty() {
            return Err(SignerMessageError::TrailingBytes);
        }
        Ok((timestamp, read_count_timestamp))
    }

    fn rejection_code(&mut self) -> Result<Vec<u8>, SignerMessageError> {
        let code = self.byte()?;
        if code > 5 {
            return Err(SignerMessageError::InvalidResponseType(code));
        }
        let mut bytes = vec![code];
        if code == 0 {
            bytes.push(self.byte()?);
        }
        Ok(bytes)
    }

    fn rejection_data(&mut self) -> Result<Vec<u8>, SignerMessageError> {
        let start = self.offset;
        self.byte()?;
        self.bytes()?;
        Ok(self.bytes[start..self.offset].to_vec())
    }

    fn state_machine_update(&mut self) -> Result<StateMachineUpdate, SignerMessageError> {
        let active_protocol_version = self.u64()?;
        let local_supported_protocol_version = self.u64()?;
        let version = active_protocol_version.min(local_supported_protocol_version);
        if version > LATEST_SIGNER_PROTOCOL_VERSION {
            return Err(SignerMessageError::UnsupportedProtocolVersion(version));
        }
        let length = usize::try_from(self.u32()?).map_err(|_| SignerMessageError::Oversized)?;
        let mut content = Self::new(self.take(length)?);
        let bitcoin_consensus_hash = ConsensusHash::from_bytes(content.array()?);
        let bitcoin_height = content.u64()?;
        let current_miner = match content.byte()? {
            0 => CurrentMiner::None,
            1 => CurrentMiner::Active {
                public_key_hash: Hash160::from_bytes(content.array()?),
                tenure_consensus_hash: ConsensusHash::from_bytes(content.array()?),
                parent_tenure_consensus_hash: ConsensusHash::from_bytes(content.array()?),
                parent_tenure_last_block: StacksBlockId::from_bytes(content.array()?),
                parent_tenure_last_block_height: content.u64()?,
            },
            variant => return Err(SignerMessageError::InvalidMinerState(variant)),
        };
        let mut replay_transactions = Vec::new();
        if version >= 1 {
            let count = content.u32()?;
            for _ in 0..count {
                let (transaction, consumed) = Transaction::decode(content.remaining())
                    .map_err(|error| SignerMessageError::Block(error.to_string()))?;
                content.take(consumed)?;
                replay_transactions.push(transaction);
            }
        }
        if !content.is_empty() {
            return Err(SignerMessageError::TrailingBytes);
        }
        Ok(StateMachineUpdate {
            active_protocol_version,
            local_supported_protocol_version,
            bitcoin_consensus_hash,
            bitcoin_height,
            current_miner,
            replay_transactions,
        })
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
