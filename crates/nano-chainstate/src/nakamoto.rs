use std::collections::BTreeSet;

use nano_codec::{CodecError, Transaction, transaction_merkle_root};
use nano_crypto::{CryptoError, MessageSignature, StacksPublicKey};
use nano_primitives::{
    BitVec, BitVecError, BlockHeaderHash, ConsensusHash, Sha256Sum, StacksBlockId, TrieHash,
    sha512_256,
};

/// A consensus-encoded Nakamoto block header.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NakamotoBlockHeader {
    pub version: u8,
    pub chain_length: u64,
    pub bitcoin_spent: u64,
    pub consensus_hash: ConsensusHash,
    pub parent_block_id: StacksBlockId,
    pub transaction_merkle_root: Sha256Sum,
    pub state_index_root: TrieHash,
    pub timestamp: u64,
    pub miner_signature: MessageSignature,
    pub signer_signatures: Vec<MessageSignature>,
    pub pox_treatment: BitVec<4000>,
}

impl NakamotoBlockHeader {
    /// Hash the header fields signed by the miner.
    #[must_use]
    pub fn miner_signature_hash(&self) -> Sha256Sum {
        sha512_256(&self.signing_bytes(false))
    }

    /// Hash the header fields signed by reward-set signers.
    #[must_use]
    pub fn signer_signature_hash(&self) -> Sha256Sum {
        sha512_256(&self.signing_bytes(true))
    }

    /// Return the canonical block hash, which excludes signer signatures.
    #[must_use]
    pub fn block_hash(&self) -> BlockHeaderHash {
        BlockHeaderHash::from_bytes(*self.signer_signature_hash().as_bytes())
    }

    /// Return the globally unique block identifier for this tenure.
    #[must_use]
    pub fn block_id(&self) -> StacksBlockId {
        let block_hash = self.block_hash();
        let mut bytes = Vec::with_capacity(52);
        bytes.extend_from_slice(block_hash.as_bytes());
        bytes.extend_from_slice(self.consensus_hash.as_bytes());
        StacksBlockId::from_bytes(*sha512_256(&bytes).as_bytes())
    }

    fn encode(&self, writer: &mut Writer) {
        self.encode_without_signatures(writer);
        writer.signature(self.miner_signature);
        writer.signatures(&self.signer_signatures);
        writer.bit_vec(&self.pox_treatment);
    }

    fn signing_bytes(&self, include_miner_signature: bool) -> Vec<u8> {
        let mut writer = Writer::default();
        self.encode_without_signatures(&mut writer);
        if include_miner_signature {
            writer.signature(self.miner_signature);
        }
        writer.bit_vec(&self.pox_treatment);
        writer.finish()
    }

    fn encode_without_signatures(&self, writer: &mut Writer) {
        writer.byte(self.version);
        writer.u64(self.chain_length);
        writer.u64(self.bitcoin_spent);
        writer.raw(self.consensus_hash.as_bytes());
        writer.raw(self.parent_block_id.as_bytes());
        writer.raw(self.transaction_merkle_root.as_bytes());
        writer.raw(self.state_index_root.as_bytes());
        writer.u64(self.timestamp);
    }
}

/// A Nakamoto block with canonical transactions and a validated Merkle root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NakamotoBlock {
    pub header: NakamotoBlockHeader,
    pub transactions: Vec<Transaction>,
}

/// A weighted signer from the reward set active for a tenure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Signer {
    pub public_key: StacksPublicKey,
    pub weight: u32,
}

/// The ordered reward-set signers authorized to approve a Nakamoto block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignerSet {
    signers: Vec<Signer>,
}

impl SignerSet {
    /// Construct a non-empty, uniquely keyed signer set.
    pub fn new(signers: Vec<Signer>) -> Result<Self, SignerSetError> {
        if signers.is_empty() {
            return Err(SignerSetError::Empty);
        }
        let keys = signers
            .iter()
            .map(|signer| signer.public_key.to_bytes_compressed())
            .collect::<BTreeSet<_>>();
        if keys.len() != signers.len() {
            return Err(SignerSetError::DuplicateSigner);
        }
        Ok(Self { signers })
    }

    /// Return the minimum signing weight required to approve a block.
    pub fn approval_threshold(&self) -> Result<u32, SignerSetError> {
        let total_weight = self.signers.iter().try_fold(0_u32, |total, signer| {
            total
                .checked_add(signer.weight)
                .ok_or(SignerSetError::WeightOverflow)
        })?;
        u32::try_from((u64::from(total_weight) * 7).div_ceil(10))
            .map_err(|_| SignerSetError::WeightOverflow)
    }

    /// Verify recovered signatures are unique, reward-set ordered, and sufficiently weighted.
    pub fn verify(&self, header: &NakamotoBlockHeader) -> Result<u32, SignerSetError> {
        let digest = *header.signer_signature_hash().as_bytes();
        let mut next_index = 0;
        let mut signed_weight = 0_u32;
        for signature in &header.signer_signatures {
            let public_key = signature
                .recover(&digest)
                .map_err(SignerSetError::Signature)?;
            let Some((index, signer)) = self
                .signers
                .iter()
                .enumerate()
                .skip(next_index)
                .find(|(_, signer)| signer.public_key == public_key)
            else {
                return Err(SignerSetError::UnknownOrUnorderedSigner);
            };
            signed_weight = signed_weight
                .checked_add(signer.weight)
                .ok_or(SignerSetError::WeightOverflow)?;
            next_index = index + 1;
        }
        if signed_weight < self.approval_threshold()? {
            return Err(SignerSetError::InsufficientWeight);
        }
        Ok(signed_weight)
    }
}

/// Errors raised while constructing or applying a signer reward set.
#[derive(Debug)]
pub enum SignerSetError {
    Empty,
    DuplicateSigner,
    WeightOverflow,
    Signature(CryptoError),
    UnknownOrUnorderedSigner,
    InsufficientWeight,
}

impl std::fmt::Display for SignerSetError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => formatter.write_str("signer set is empty"),
            Self::DuplicateSigner => formatter.write_str("signer set has duplicate public keys"),
            Self::WeightOverflow => formatter.write_str("signer weight overflows"),
            Self::Signature(error) => write!(formatter, "invalid signer signature: {error}"),
            Self::UnknownOrUnorderedSigner => {
                formatter.write_str("signer is unknown or signatures are out of reward-set order")
            }
            Self::InsufficientWeight => {
                formatter.write_str("signer weight is below approval threshold")
            }
        }
    }
}

impl std::error::Error for SignerSetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Signature(error) => Some(error),
            Self::Empty
            | Self::DuplicateSigner
            | Self::WeightOverflow
            | Self::UnknownOrUnorderedSigner
            | Self::InsufficientWeight => None,
        }
    }
}

impl NakamotoBlock {
    /// Decode one complete Nakamoto block and validate transaction uniqueness and its Merkle root.
    pub fn decode(bytes: &[u8]) -> Result<Self, NakamotoCodecError> {
        let mut reader = Reader::new(bytes);
        let header = NakamotoBlockHeader {
            version: reader.byte()?,
            chain_length: reader.u64()?,
            bitcoin_spent: reader.u64()?,
            consensus_hash: ConsensusHash::from_bytes(reader.array()?),
            parent_block_id: StacksBlockId::from_bytes(reader.array()?),
            transaction_merkle_root: Sha256Sum::from_bytes(reader.array()?),
            state_index_root: TrieHash::from_bytes(reader.array()?),
            timestamp: reader.u64()?,
            miner_signature: reader.signature()?,
            signer_signatures: reader.signatures()?,
            pox_treatment: reader.bit_vec()?,
        };
        let transaction_count = reader.u32()?;
        let count = usize::try_from(transaction_count).map_err(|_| NakamotoCodecError::Length)?;
        if count > reader.remaining() {
            return Err(NakamotoCodecError::Length);
        }
        let mut transactions = Vec::with_capacity(count);
        for _ in 0..count {
            let (transaction, consumed) = Transaction::decode(reader.remaining_bytes())
                .map_err(NakamotoCodecError::Transaction)?;
            reader.advance(consumed)?;
            transactions.push(transaction);
        }
        if !reader.is_empty() {
            return Err(NakamotoCodecError::TrailingBytes);
        }
        validate_block(header, transactions)
    }

    /// Encode this block in its canonical consensus representation.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::default();
        self.header.encode(&mut writer);
        writer.u32(u32::try_from(self.transactions.len()).expect("transaction count fits u32"));
        for transaction in &self.transactions {
            writer.raw(&transaction.encode());
        }
        writer.finish()
    }

    /// Return this block's globally unique identifier.
    #[must_use]
    pub fn block_id(&self) -> StacksBlockId {
        self.header.block_id()
    }
}

fn validate_block(
    header: NakamotoBlockHeader,
    transactions: Vec<Transaction>,
) -> Result<NakamotoBlock, NakamotoCodecError> {
    let transaction_ids = transactions
        .iter()
        .map(|transaction| *transaction.txid().as_bytes())
        .collect::<BTreeSet<_>>();
    if transaction_ids.len() != transactions.len() {
        return Err(NakamotoCodecError::DuplicateTransaction);
    }
    if transaction_merkle_root(&transactions) != header.transaction_merkle_root {
        return Err(NakamotoCodecError::TransactionMerkleRoot);
    }
    Ok(NakamotoBlock {
        header,
        transactions,
    })
}

/// Errors raised while decoding or validating a Nakamoto block envelope.
#[derive(Debug)]
pub enum NakamotoCodecError {
    EndOfInput,
    Length,
    TrailingBytes,
    BitVec(BitVecError),
    Transaction(CodecError),
    DuplicateTransaction,
    TransactionMerkleRoot,
}

impl std::fmt::Display for NakamotoCodecError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EndOfInput => formatter.write_str("unexpected end of Nakamoto block"),
            Self::Length => formatter.write_str("invalid Nakamoto block length"),
            Self::TrailingBytes => formatter.write_str("Nakamoto block has trailing bytes"),
            Self::BitVec(error) => write!(formatter, "invalid PoX treatment bit vector: {error}"),
            Self::Transaction(error) => write!(formatter, "invalid block transaction: {error}"),
            Self::DuplicateTransaction => {
                formatter.write_str("Nakamoto block has duplicate transactions")
            }
            Self::TransactionMerkleRoot => {
                formatter.write_str("Nakamoto block transaction Merkle root mismatches")
            }
        }
    }
}

impl std::error::Error for NakamotoCodecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BitVec(error) => Some(error),
            Self::Transaction(error) => Some(error),
            Self::EndOfInput
            | Self::Length
            | Self::TrailingBytes
            | Self::DuplicateTransaction
            | Self::TransactionMerkleRoot => None,
        }
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

    const fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }

    const fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    fn remaining_bytes(&self) -> &'a [u8] {
        &self.bytes[self.offset..]
    }

    fn advance(&mut self, length: usize) -> Result<(), NakamotoCodecError> {
        self.take(length).map(|_| ())
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], NakamotoCodecError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(NakamotoCodecError::Length)?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or(NakamotoCodecError::EndOfInput)?;
        self.offset = end;
        Ok(bytes)
    }

    fn array<const LENGTH: usize>(&mut self) -> Result<[u8; LENGTH], NakamotoCodecError> {
        Ok(self.take(LENGTH)?.try_into().expect("fixed-length slice"))
    }

    fn byte(&mut self) -> Result<u8, NakamotoCodecError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, NakamotoCodecError> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, NakamotoCodecError> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, NakamotoCodecError> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    fn signature(&mut self) -> Result<MessageSignature, NakamotoCodecError> {
        Ok(MessageSignature::from_bytes(self.array()?))
    }

    fn signatures(&mut self) -> Result<Vec<MessageSignature>, NakamotoCodecError> {
        let count = usize::try_from(self.u32()?).map_err(|_| NakamotoCodecError::Length)?;
        if count > self.remaining() / 65 {
            return Err(NakamotoCodecError::Length);
        }
        (0..count).map(|_| self.signature()).collect()
    }

    fn bit_vec(&mut self) -> Result<BitVec<4000>, NakamotoCodecError> {
        let length = self.u16()?;
        let data_length = usize::from(length).div_ceil(8);
        if self.u32()? != u32::try_from(data_length).expect("bit vector length fits u32") {
            return Err(NakamotoCodecError::Length);
        }
        let data = self.take(data_length)?;
        let mut wire = Vec::with_capacity(6 + data.len());
        wire.extend_from_slice(&length.to_be_bytes());
        wire.extend_from_slice(
            &u32::try_from(data.len())
                .expect("bit vector length fits u32")
                .to_be_bytes(),
        );
        wire.extend_from_slice(data);
        BitVec::from_wire_bytes(&wire).map_err(NakamotoCodecError::BitVec)
    }
}

#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn byte(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn raw(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    fn u32(&mut self, value: u32) {
        self.raw(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.raw(&value.to_be_bytes());
    }

    fn signature(&mut self, signature: MessageSignature) {
        self.raw(signature.as_bytes());
    }

    fn signatures(&mut self, signatures: &[MessageSignature]) {
        self.u32(u32::try_from(signatures.len()).expect("signature count fits u32"));
        for signature in signatures {
            self.signature(*signature);
        }
    }

    fn bit_vec(&mut self, bit_vec: &BitVec<4000>) {
        self.raw(&bit_vec.wire_bytes());
    }
}
