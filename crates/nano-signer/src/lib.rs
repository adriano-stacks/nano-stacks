#![forbid(unsafe_code)]

use std::{collections::BTreeMap, fmt};

use nano_crypto::StacksPrivateKey;
use nano_primitives::{ConsensusHash, Sha256Sum};
use nano_stackerdb::{BlockAcceptance, BlockProposal, Chunk, SignerMessage, StackerDbError};

/// Checks a miner proposal against the node's current chain and sortition view.
pub trait ProposalValidator {
    /// Return an explanation when a proposal must not be signed.
    fn validate(&mut self, proposal: &BlockProposal) -> Result<(), String>;
}

/// Configuration for one signer writer slot.
#[derive(Clone)]
pub struct SignerConfig {
    pub private_key: StacksPrivateKey,
    pub writer_slot: u32,
    pub next_slot_version: u32,
}

/// A stateful signer that emits authenticated `StackerDB` acceptance chunks.
pub struct EmbeddedSigner<V> {
    private_key: StacksPrivateKey,
    validator: V,
    writer_slot: u32,
    next_slot_version: u32,
    signed: BTreeMap<(ConsensusHash, u64), Sha256Sum>,
}

/// Errors while validating or signing a proposal.
#[derive(Debug)]
pub enum SignerError {
    Validation(String),
    Equivocation,
    SlotVersionOverflow,
    Message(nano_stackerdb::SignerMessageError),
    Chunk(StackerDbError),
}

impl fmt::Display for SignerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation(error) => write!(formatter, "proposal validation failed: {error}"),
            Self::Equivocation => formatter.write_str("refusing to sign a conflicting block"),
            Self::SlotVersionOverflow => formatter.write_str("StackerDB slot version overflow"),
            Self::Message(error) => write!(formatter, "signer message error: {error}"),
            Self::Chunk(error) => write!(formatter, "StackerDB chunk error: {error}"),
        }
    }
}

impl std::error::Error for SignerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Message(error) => Some(error),
            Self::Chunk(error) => Some(error),
            Self::Validation(_) | Self::Equivocation | Self::SlotVersionOverflow => None,
        }
    }
}

impl From<nano_stackerdb::SignerMessageError> for SignerError {
    fn from(error: nano_stackerdb::SignerMessageError) -> Self {
        Self::Message(error)
    }
}

impl From<StackerDbError> for SignerError {
    fn from(error: StackerDbError) -> Self {
        Self::Chunk(error)
    }
}

impl<V: ProposalValidator> EmbeddedSigner<V> {
    /// Construct a signer from a persistent writer-slot configuration.
    #[must_use]
    pub const fn new(config: SignerConfig, validator: V) -> Self {
        Self {
            private_key: config.private_key,
            validator,
            writer_slot: config.writer_slot,
            next_slot_version: config.next_slot_version,
            signed: BTreeMap::new(),
        }
    }

    /// Validate a proposed block and return the signed response chunk to upload.
    pub fn sign(&mut self, proposal: &BlockProposal) -> Result<Chunk, SignerError> {
        self.validator
            .validate(proposal)
            .map_err(SignerError::Validation)?;
        let position = (
            proposal.block.header.consensus_hash,
            proposal.block.header.chain_length,
        );
        let signature_hash = proposal.block.header.signer_signature_hash();
        if self
            .signed
            .get(&position)
            .is_some_and(|signed| *signed != signature_hash)
        {
            return Err(SignerError::Equivocation);
        }
        if self.signed.contains_key(&position) {
            return Err(SignerError::Equivocation);
        }
        let next_slot_version = self
            .next_slot_version
            .checked_add(1)
            .ok_or(SignerError::SlotVersionOverflow)?;
        let signature = self.private_key.sign(signature_hash.as_bytes());
        let message = SignerMessage::BlockResponse(BlockAcceptance::new(signature_hash, signature));
        let mut chunk = Chunk::new(self.writer_slot, self.next_slot_version, message.encode()?);
        chunk.sign(&self.private_key)?;
        self.signed.insert(position, signature_hash);
        self.next_slot_version = next_slot_version;
        Ok(chunk)
    }

    /// Return the next version that will be used for a response chunk.
    #[must_use]
    pub const fn next_slot_version(&self) -> u32 {
        self.next_slot_version
    }
}

#[cfg(test)]
mod tests {
    use nano_chainstate::{NakamotoBlock, NakamotoBlockHeader};
    use nano_crypto::StacksPrivateKey;
    use nano_primitives::{BitVec, ConsensusHash, Sha256Sum, StacksBlockId, TrieHash};
    use nano_stackerdb::{BlockProposal, SignerMessage};

    use super::{EmbeddedSigner, ProposalValidator, SignerConfig, SignerError};

    struct Accept;

    impl ProposalValidator for Accept {
        fn validate(&mut self, _proposal: &BlockProposal) -> Result<(), String> {
            Ok(())
        }
    }

    struct Reject;

    impl ProposalValidator for Reject {
        fn validate(&mut self, _proposal: &BlockProposal) -> Result<(), String> {
            Err("state root mismatch".to_owned())
        }
    }

    fn proposal() -> BlockProposal {
        let key = StacksPrivateKey::from_seed(b"miner");
        let header = NakamotoBlockHeader {
            version: 0,
            chain_length: 1,
            bitcoin_spent: 0,
            consensus_hash: ConsensusHash::from_bytes([1; 20]),
            parent_block_id: StacksBlockId::from_bytes([2; 32]),
            transaction_merkle_root: Sha256Sum::from_bytes([3; 32]),
            state_index_root: TrieHash::from_bytes([4; 32]),
            timestamp: 1,
            miner_signature: key.sign(&[5; 32]),
            signer_signatures: Vec::new(),
            pox_treatment: BitVec::zeros(1).expect("one bit is valid"),
        };
        BlockProposal {
            block: NakamotoBlock {
                header,
                transactions: Vec::new(),
            },
            bitcoin_height: 10,
            reward_cycle: 1,
            data: Vec::new(),
        }
    }

    #[test]
    fn accepted_proposals_produce_authenticated_responses() {
        let key = StacksPrivateKey::from_seed(b"signer");
        let mut signer = EmbeddedSigner::new(
            SignerConfig {
                private_key: key.clone(),
                writer_slot: 7,
                next_slot_version: 3,
            },
            Accept,
        );
        let proposal = proposal();
        let chunk = signer.sign(&proposal).expect("sign proposal");

        assert_eq!(chunk.slot_id, 7);
        assert_eq!(chunk.slot_version, 3);
        assert!(
            chunk
                .verify(nano_primitives::hash160(
                    &key.public_key().to_bytes_compressed()
                ))
                .expect("verify chunk")
        );
        assert!(matches!(
            SignerMessage::decode(&chunk.data),
            Ok(SignerMessage::BlockResponse(_))
        ));
        assert_eq!(signer.next_slot_version(), 4);
    }

    #[test]
    fn rejected_proposals_never_advance_the_writer_version() {
        let mut signer = EmbeddedSigner::new(
            SignerConfig {
                private_key: StacksPrivateKey::from_seed(b"signer"),
                writer_slot: 7,
                next_slot_version: 3,
            },
            Reject,
        );

        assert!(matches!(
            signer.sign(&proposal()),
            Err(SignerError::Validation(_))
        ));
        assert_eq!(signer.next_slot_version(), 3);
    }
}
