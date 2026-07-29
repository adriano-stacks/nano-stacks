use std::collections::{BTreeMap, BTreeSet};

use nano_codec::{CodecError, Transaction, TransactionPayloadType, transaction_merkle_root};
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
    pub problematic_transactions: Vec<ProblematicTransaction>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProblematicTransaction {
    pub index: u32,
    pub category: u8,
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
        self.encode_problematic_transactions(writer);
    }

    fn signing_bytes(&self, include_miner_signature: bool) -> Vec<u8> {
        let mut writer = Writer::default();
        self.encode_without_signatures(&mut writer);
        if include_miner_signature {
            writer.signature(self.miner_signature);
        }
        writer.bit_vec(&self.pox_treatment);
        self.encode_problematic_transactions(&mut writer);
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

    fn encode_problematic_transactions(&self, writer: &mut Writer) {
        if self.version & 0x7f >= 1 {
            writer.u32(
                u32::try_from(self.problematic_transactions.len())
                    .expect("problematic transaction count fits u32"),
            );
            for transaction in &self.problematic_transactions {
                writer.u32(transaction.index);
                writer.byte(transaction.category);
            }
        }
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

    /// Derive signer voting weights from stacked amounts and the stacking threshold.
    pub fn from_stacked_amounts(
        signers: Vec<(StacksPublicKey, u128)>,
        threshold: u128,
    ) -> Result<Self, SignerSetError> {
        if threshold == 0 {
            return Err(SignerSetError::ZeroThreshold);
        }
        let signers = signers
            .into_iter()
            .map(|(public_key, stacked_amount)| {
                let weight = u32::try_from(stacked_amount / threshold)
                    .map_err(|_| SignerSetError::WeightOverflow)?;
                Ok(Signer { public_key, weight })
            })
            .collect::<Result<Vec<_>, SignerSetError>>()?;
        Self::new(signers)
    }

    /// Derive signer weights from stacked amounts and the available reward slots.
    pub fn from_reward_slots(
        signers: Vec<(StacksPublicKey, u128)>,
        reward_slots: u32,
    ) -> Result<(Self, u128), SignerSetError> {
        if reward_slots == 0 {
            return Err(SignerSetError::ZeroRewardSlots);
        }

        let mut stacked = BTreeMap::new();
        for (public_key, amount) in signers {
            if amount == 0 {
                continue;
            }
            let key = public_key.to_bytes_compressed();
            let entry = stacked.entry(key).or_insert((public_key, 0_u128));
            entry.1 = entry
                .1
                .checked_add(amount)
                .ok_or(SignerSetError::StackedAmountOverflow)?;
        }
        let total = stacked.values().try_fold(0_u128, |total, (_, amount)| {
            total
                .checked_add(*amount)
                .ok_or(SignerSetError::StackedAmountOverflow)
        })?;
        let threshold = total.div_ceil(u128::from(reward_slots)).max(1);
        let mut apportioned = stacked
            .into_iter()
            .map(|(key, (public_key, amount))| {
                (key, public_key, amount / threshold, amount % threshold)
            })
            .collect::<Vec<_>>();
        let assigned = apportioned
            .iter()
            .try_fold(0_u128, |total, (_, _, weight, _)| {
                total
                    .checked_add(*weight)
                    .ok_or(SignerSetError::WeightOverflow)
            })?;
        let mut remaining = u128::from(reward_slots).saturating_sub(assigned);
        apportioned.sort_by(
            |(left_key, _, _, left_remainder), (right_key, _, _, right_remainder)| {
                right_remainder
                    .cmp(left_remainder)
                    .then_with(|| left_key.cmp(right_key))
            },
        );
        for (_, _, weight, _) in &mut apportioned {
            if remaining == 0 {
                break;
            }
            *weight = weight
                .checked_add(1)
                .ok_or(SignerSetError::WeightOverflow)?;
            remaining -= 1;
        }
        apportioned.sort_by_key(|(key, _, _, _)| *key);
        let signers = apportioned
            .into_iter()
            .filter(|(_, _, weight, _)| *weight != 0)
            .map(|(_, public_key, weight, _)| {
                Ok(Signer {
                    public_key,
                    weight: u32::try_from(weight).map_err(|_| SignerSetError::WeightOverflow)?,
                })
            })
            .collect::<Result<Vec<_>, SignerSetError>>()?;
        Ok((Self::new(signers)?, threshold))
    }

    /// Return the signer entries in consensus order.
    #[must_use]
    pub fn signers(&self) -> &[Signer] {
        &self.signers
    }

    /// Return the signer weights in consensus order.
    #[must_use]
    pub fn weights(&self) -> Vec<u32> {
        self.signers.iter().map(|signer| signer.weight).collect()
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

    /// Order valid signer responses by reward-set index and require threshold weight.
    pub fn order_responses(
        &self,
        header: &NakamotoBlockHeader,
        responses: impl IntoIterator<Item = MessageSignature>,
    ) -> Result<Vec<MessageSignature>, SignerSetError> {
        let digest = *header.signer_signature_hash().as_bytes();
        let mut ordered = BTreeMap::new();
        for signature in responses {
            let public_key = signature
                .recover(&digest)
                .map_err(SignerSetError::Signature)?;
            let index = self
                .signers
                .iter()
                .position(|signer| signer.public_key == public_key)
                .ok_or(SignerSetError::UnknownOrUnorderedSigner)?;
            if ordered.insert(index, signature).is_some() {
                return Err(SignerSetError::UnknownOrUnorderedSigner);
            }
        }
        let signatures = ordered.into_values().collect::<Vec<_>>();
        let mut candidate = header.clone();
        candidate.signer_signatures.clone_from(&signatures);
        self.verify(&candidate)?;
        Ok(signatures)
    }
}

/// Errors raised while constructing or applying a signer reward set.
#[derive(Debug)]
pub enum SignerSetError {
    Empty,
    DuplicateSigner,
    ZeroThreshold,
    ZeroRewardSlots,
    StackedAmountOverflow,
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
            Self::ZeroThreshold => formatter.write_str("signer threshold cannot be zero"),
            Self::ZeroRewardSlots => formatter.write_str("reward slot count cannot be zero"),
            Self::StackedAmountOverflow => formatter.write_str("stacked amount overflows"),
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
            | Self::ZeroThreshold
            | Self::ZeroRewardSlots
            | Self::StackedAmountOverflow
            | Self::WeightOverflow
            | Self::UnknownOrUnorderedSigner
            | Self::InsufficientWeight => None,
        }
    }
}

impl NakamotoBlock {
    /// The Bitcoin block a tenure change moves the Clarity burn view to.
    ///
    /// A tenure extend advances the burn view without starting a tenure, so a
    /// block's Clarity burn height is not always its own tenure's sortition.
    #[must_use]
    pub fn bitcoin_view_consensus_hash(&self) -> Option<ConsensusHash> {
        self.transactions
            .iter()
            .find_map(|transaction| match transaction.payload().data() {
                nano_codec::TransactionPayloadData::TenureChange(payload) => {
                    Some(payload.bitcoin_view_consensus_hash)
                }
                _ => None,
            })
    }

    /// Decode one complete Nakamoto block and validate transaction uniqueness and its Merkle root.
    pub fn decode(bytes: &[u8]) -> Result<Self, NakamotoCodecError> {
        let (block, consumed) = Self::decode_prefix(bytes)?;
        if consumed != bytes.len() {
            return Err(NakamotoCodecError::TrailingBytes);
        }
        Ok(block)
    }

    /// Decode one Nakamoto block from the front of a concatenated block stream.
    pub fn decode_prefix(bytes: &[u8]) -> Result<(Self, usize), NakamotoCodecError> {
        let mut reader = Reader::new(bytes);
        let version = reader.byte()?;
        let header = NakamotoBlockHeader {
            version,
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
            problematic_transactions: if version & 0x7f >= 1 {
                reader.problematic_transactions()?
            } else {
                Vec::new()
            },
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
        let consumed = reader.offset;
        Ok((validate_block(header, transactions)?, consumed))
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

    /// Validate this block's immediate chain and tenure linkage against its parent.
    pub fn validate_successor(&self, parent: &NakamotoBlockHeader) -> Result<(), TenureError> {
        if self.header.parent_block_id != parent.block_id() {
            return Err(TenureError::ParentBlockId);
        }
        if self.header.chain_length != parent.chain_length.saturating_add(1) {
            return Err(TenureError::ChainLength);
        }
        if self.header.timestamp <= parent.timestamp {
            return Err(TenureError::Timestamp);
        }
        if self.header.consensus_hash != parent.consensus_hash
            && !self.transactions.iter().any(|transaction| {
                transaction.payload_type() == TransactionPayloadType::TenureChange
            })
        {
            return Err(TenureError::MissingTenureChange);
        }
        Ok(())
    }
}

/// Errors raised when a block does not link correctly to its immediate predecessor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TenureError {
    ParentBlockId,
    ChainLength,
    Timestamp,
    MissingTenureChange,
}

impl std::fmt::Display for TenureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ParentBlockId => "Nakamoto block does not reference its parent",
            Self::ChainLength => "Nakamoto block chain length does not advance by one",
            Self::Timestamp => "Nakamoto block timestamp does not advance",
            Self::MissingTenureChange => "new tenure has no tenure-change transaction",
        })
    }
}

impl std::error::Error for TenureError {}

#[cfg(test)]
mod tests {
    use super::{
        NakamotoBlock, NakamotoBlockHeader, ProblematicTransaction, Signer, SignerSet,
        SignerSetError,
    };
    use nano_crypto::StacksPrivateKey;
    use nano_primitives::{BitVec, ConsensusHash, Sha256Sum, StacksBlockId, TrieHash};

    #[test]
    fn reward_set_rejects_zero_threshold() {
        assert!(matches!(
            SignerSet::from_stacked_amounts(Vec::new(), 0),
            Err(SignerSetError::ZeroThreshold)
        ));
    }

    #[test]
    fn reward_set_rejects_empty_signers() {
        assert!(matches!(
            SignerSet::from_stacked_amounts(Vec::new(), 1),
            Err(SignerSetError::Empty)
        ));
    }

    #[test]
    fn reward_slots_use_largest_remainders() {
        let signers = (1_u8..=5)
            .map(|seed| (StacksPrivateKey::from_seed(&[seed]).public_key(), 1_u128))
            .collect();
        let (signer_set, threshold) =
            SignerSet::from_reward_slots(signers, 4).expect("derive signer set");

        assert_eq!(threshold, 2);
        assert_eq!(signer_set.weights(), vec![1, 1, 1, 1]);
    }

    #[test]
    fn reward_slots_aggregate_duplicate_signers() {
        let signer = StacksPrivateKey::from_seed(b"signer").public_key();
        let (signer_set, threshold) =
            SignerSet::from_reward_slots(vec![(signer.clone(), 3), (signer, 2)], 4)
                .expect("derive signer set");

        assert_eq!(threshold, 2);
        assert_eq!(signer_set.weights(), vec![3]);
    }

    #[test]
    fn signer_responses_are_ordered_and_threshold_checked() {
        let first = StacksPrivateKey::from_seed(b"first signer");
        let second = StacksPrivateKey::from_seed(b"second signer");
        let third = StacksPrivateKey::from_seed(b"third signer");
        let set = SignerSet::new(vec![
            Signer {
                public_key: first.public_key(),
                weight: 3,
            },
            Signer {
                public_key: second.public_key(),
                weight: 4,
            },
            Signer {
                public_key: third.public_key(),
                weight: 3,
            },
        ])
        .expect("valid signer set");
        let header = NakamotoBlockHeader {
            version: 1,
            chain_length: 1,
            bitcoin_spent: 0,
            consensus_hash: ConsensusHash::from_bytes([1; 20]),
            parent_block_id: StacksBlockId::from_bytes([2; 32]),
            transaction_merkle_root: Sha256Sum::from_bytes([3; 32]),
            state_index_root: TrieHash::from_bytes([4; 32]),
            timestamp: 5,
            miner_signature: first.sign(&[5; 32]),
            signer_signatures: Vec::new(),
            pox_treatment: BitVec::zeros(1).expect("valid bit vector"),
            problematic_transactions: Vec::new(),
        };
        let digest = header.signer_signature_hash();
        let first_response = first.sign(digest.as_bytes());
        let second_response = second.sign(digest.as_bytes());
        let third_response = third.sign(digest.as_bytes());
        let ordered = set
            .order_responses(
                &header,
                vec![third_response, first_response, second_response],
            )
            .expect("threshold response set");
        assert_eq!(
            ordered,
            vec![first_response, second_response, third_response]
        );
    }

    #[test]
    fn epoch_four_headers_round_trip_problematic_transactions() {
        let block = NakamotoBlock {
            header: NakamotoBlockHeader {
                version: 1,
                chain_length: 1,
                bitcoin_spent: 0,
                consensus_hash: ConsensusHash::from_bytes([1; 20]),
                parent_block_id: StacksBlockId::from_bytes([2; 32]),
                transaction_merkle_root: Sha256Sum::default(),
                state_index_root: TrieHash::from_bytes([4; 32]),
                timestamp: 5,
                miner_signature: StacksPrivateKey::from_seed(b"miner").sign(&[5; 32]),
                signer_signatures: Vec::new(),
                pox_treatment: BitVec::zeros(1).expect("valid bit vector"),
                problematic_transactions: vec![ProblematicTransaction {
                    index: 3,
                    category: 1,
                }],
            },
            transactions: Vec::new(),
        };

        let decoded = NakamotoBlock::decode(&block.encode()).expect("decode block");
        assert_eq!(decoded, block);
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

    fn problematic_transactions(
        &mut self,
    ) -> Result<Vec<ProblematicTransaction>, NakamotoCodecError> {
        let count = usize::try_from(self.u32()?).map_err(|_| NakamotoCodecError::Length)?;
        if count > self.remaining() / 5 {
            return Err(NakamotoCodecError::Length);
        }
        (0..count)
            .map(|_| {
                Ok(ProblematicTransaction {
                    index: self.u32()?,
                    category: self.byte()?,
                })
            })
            .collect()
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
