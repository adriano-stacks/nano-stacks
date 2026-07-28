#![forbid(unsafe_code)]

use std::{collections::BTreeMap, fmt};

use nano_chainstate::{BitcoinBlockContext, ChainState, NakamotoBlock};
use nano_crypto::StacksPrivateKey;
use nano_primitives::{ConsensusHash, Sha256Sum, hash160};
use nano_stackerdb::{
    BlockAcceptance, BlockProposal, Chunk, ChunkAck, SignerMessage, StackerDbClient,
    StackerDbClientError, StackerDbContract, StackerDbError,
};
use nano_sync::SortitionInfo;

/// Checks a miner proposal against the node's current chain and sortition view.
pub trait ProposalValidator {
    /// Return an explanation when a proposal must not be signed.
    fn validate(&mut self, proposal: &BlockProposal) -> Result<(), String>;
}

/// Validates that a proposal belongs to the expected Bitcoin sortition winner.
#[derive(Clone, Debug)]
pub struct SortitionProposalValidator {
    sortition: SortitionInfo,
    reward_cycle: u64,
}

impl SortitionProposalValidator {
    /// Construct a validator for one reward cycle and its active sortition.
    #[must_use]
    pub const fn new(sortition: SortitionInfo, reward_cycle: u64) -> Self {
        Self {
            sortition,
            reward_cycle,
        }
    }
}

impl ProposalValidator for SortitionProposalValidator {
    fn validate(&mut self, proposal: &BlockProposal) -> Result<(), String> {
        if proposal.reward_cycle != self.reward_cycle {
            return Err("proposal reward cycle does not match the active cycle".to_owned());
        }
        if proposal.bitcoin_height != self.sortition.bitcoin_height {
            return Err("proposal Bitcoin height does not match its sortition".to_owned());
        }
        if proposal.block.header.consensus_hash != self.sortition.consensus_hash {
            return Err("proposal consensus hash does not match its sortition".to_owned());
        }
        if !self.sortition.was_sortition {
            return Err("proposal consensus hash does not select a miner".to_owned());
        }
        let expected_key = self
            .sortition
            .miner_public_key_hash
            .ok_or_else(|| "sortition does not identify a miner key".to_owned())?;
        if !proposal.block.header.signer_signatures.is_empty() {
            return Err("proposal already includes signer signatures".to_owned());
        }
        let miner_key = proposal
            .block
            .header
            .miner_signature
            .recover(proposal.block.header.miner_signature_hash().as_bytes())
            .map_err(|error| format!("invalid miner signature: {error}"))?;
        if hash160(&miner_key.to_bytes_compressed()) != expected_key {
            return Err("proposal miner does not match the sortition winner".to_owned());
        }
        Ok(())
    }
}

/// Validates proposal execution from a trusted, checkpointed chain state.
#[derive(Debug)]
pub struct ChainstateProposalValidator {
    chainstate: ChainState,
    bitcoin_context: BitcoinBlockContext,
    trusted: BTreeMap<nano_primitives::StacksBlockId, nano_chainstate::NakamotoBlockHeader>,
    candidates: BTreeMap<nano_primitives::StacksBlockId, nano_chainstate::NakamotoBlockHeader>,
}

impl ChainstateProposalValidator {
    /// Start validating proposals from a block whose state is already present in `chainstate`.
    #[must_use]
    pub fn new(
        chainstate: ChainState,
        anchor: &NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
    ) -> Self {
        let mut trusted = BTreeMap::new();
        trusted.insert(anchor.block_id(), anchor.header.clone());
        Self {
            chainstate,
            bitcoin_context,
            trusted,
            candidates: BTreeMap::new(),
        }
    }

    /// Record an observed block after its state has been independently verified.
    pub fn observe(&mut self, block: &NakamotoBlock, bitcoin_height: u64) -> Result<(), String> {
        let block_id = block.block_id();
        if let Some(candidate) = self.candidates.remove(&block_id) {
            if candidate != block.header {
                return Err("observed block differs from the validated candidate".to_owned());
            }
        } else {
            self.validate_block(block, self.context_at(bitcoin_height))?;
        }
        self.trusted.insert(block_id, block.header.clone());
        Ok(())
    }

    const fn context_at(&self, bitcoin_height: u64) -> BitcoinBlockContext {
        BitcoinBlockContext {
            height: bitcoin_height,
            ..self.bitcoin_context
        }
    }

    fn validate_block(
        &mut self,
        block: &NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
    ) -> Result<(), String> {
        let parent = self
            .trusted
            .get(&block.header.parent_block_id)
            .ok_or_else(|| "proposal parent is not in the trusted chain view".to_owned())?;
        block
            .validate_successor(parent)
            .map_err(|error| format!("proposal does not extend its parent: {error}"))?;
        self.chainstate
            .append_nakamoto_block_with_bitcoin_context(
                bitcoin_context,
                Some(*block.header.parent_block_id.as_bytes()),
                block,
            )
            .map_err(|error| format!("proposal execution failed: {error}"))?;
        Ok(())
    }
}

impl ProposalValidator for ChainstateProposalValidator {
    fn validate(&mut self, proposal: &BlockProposal) -> Result<(), String> {
        let block_id = proposal.block.block_id();
        if self.candidates.get(&block_id) == Some(&proposal.block.header) {
            return Ok(());
        }
        self.validate_block(&proposal.block, self.context_at(proposal.bitcoin_height))?;
        self.candidates
            .insert(block_id, proposal.block.header.clone());
        Ok(())
    }
}

/// Applies two independent proposal validators in order.
pub struct ProposalValidators<A, B> {
    first: A,
    second: B,
}

impl<A, B> ProposalValidators<A, B> {
    /// Construct a validator which runs `first` before `second`.
    #[must_use]
    pub const fn new(first: A, second: B) -> Self {
        Self { first, second }
    }

    /// Return the two component validators.
    #[must_use]
    pub fn into_inner(self) -> (A, B) {
        (self.first, self.second)
    }
}

impl<A: ProposalValidator, B: ProposalValidator> ProposalValidator for ProposalValidators<A, B> {
    fn validate(&mut self, proposal: &BlockProposal) -> Result<(), String> {
        self.first.validate(proposal)?;
        self.second.validate(proposal)
    }
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
    signed: BTreeMap<(ConsensusHash, u64), SignedBlock>,
}

struct SignedBlock {
    signature_hash: Sha256Sum,
    chunk: Chunk,
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

/// Polls miner proposals and publishes accepted responses.
pub struct SignerService<V> {
    client: StackerDbClient,
    miner_contract: StackerDbContract,
    signer_contract: StackerDbContract,
    signer: EmbeddedSigner<V>,
    last_proposal: Option<Sha256Sum>,
}

/// Errors while transporting signer messages.
#[derive(Debug)]
pub enum SignerServiceError {
    Client(StackerDbClientError),
    Message(nano_stackerdb::SignerMessageError),
    Signer(SignerError),
    UnexpectedMessage,
    Rejected {
        reason: Option<String>,
        code: Option<u32>,
    },
}

impl fmt::Display for SignerServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(error) => write!(formatter, "StackerDB client error: {error}"),
            Self::Message(error) => write!(formatter, "signer message error: {error}"),
            Self::Signer(error) => write!(formatter, "signer error: {error}"),
            Self::UnexpectedMessage => formatter.write_str("miner slot did not contain a proposal"),
            Self::Rejected { reason, code } => {
                write!(formatter, "StackerDB rejected signer chunk")?;
                if let Some(code) = code {
                    write!(formatter, " (code {code})")?;
                }
                if let Some(reason) = reason {
                    write!(formatter, ": {reason}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for SignerServiceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Client(error) => Some(error),
            Self::Message(error) => Some(error),
            Self::Signer(error) => Some(error),
            Self::UnexpectedMessage | Self::Rejected { .. } => None,
        }
    }
}

impl From<StackerDbClientError> for SignerServiceError {
    fn from(error: StackerDbClientError) -> Self {
        Self::Client(error)
    }
}

impl From<nano_stackerdb::SignerMessageError> for SignerServiceError {
    fn from(error: nano_stackerdb::SignerMessageError) -> Self {
        Self::Message(error)
    }
}

impl From<SignerError> for SignerServiceError {
    fn from(error: SignerError) -> Self {
        Self::Signer(error)
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
        let position = (
            proposal.block.header.consensus_hash,
            proposal.block.header.chain_length,
        );
        let signature_hash = proposal.block.header.signer_signature_hash();
        if let Some(signed) = self.signed.get(&position) {
            return if signed.signature_hash == signature_hash {
                Ok(signed.chunk.clone())
            } else {
                Err(SignerError::Equivocation)
            };
        }
        self.validator
            .validate(proposal)
            .map_err(SignerError::Validation)?;
        let next_slot_version = self
            .next_slot_version
            .checked_add(1)
            .ok_or(SignerError::SlotVersionOverflow)?;
        let signature = self.private_key.sign(signature_hash.as_bytes());
        let message = SignerMessage::BlockResponse(BlockAcceptance::new(signature_hash, signature));
        let mut chunk = Chunk::new(self.writer_slot, self.next_slot_version, message.encode()?);
        chunk.sign(&self.private_key)?;
        self.signed.insert(
            position,
            SignedBlock {
                signature_hash,
                chunk: chunk.clone(),
            },
        );
        self.next_slot_version = next_slot_version;
        Ok(chunk)
    }

    /// Return the next version that will be used for a response chunk.
    #[must_use]
    pub const fn next_slot_version(&self) -> u32 {
        self.next_slot_version
    }
}

impl<V: ProposalValidator> SignerService<V> {
    /// Construct a service for the miner proposal and signer response contracts of one cycle.
    #[must_use]
    pub const fn new(
        client: StackerDbClient,
        miner_contract: StackerDbContract,
        signer_contract: StackerDbContract,
        signer: EmbeddedSigner<V>,
    ) -> Self {
        Self {
            client,
            miner_contract,
            signer_contract,
            signer,
            last_proposal: None,
        }
    }

    /// Process the latest miner proposal once and upload an acceptance response when needed.
    pub async fn poll(&mut self) -> Result<Option<ChunkAck>, SignerServiceError> {
        let Some(bytes) = self.client.latest_chunk(&self.miner_contract, 0).await? else {
            return Ok(None);
        };
        let proposal_hash = nano_primitives::sha512_256(&bytes);
        if self.last_proposal == Some(proposal_hash) {
            return Ok(None);
        }
        let SignerMessage::BlockProposal(proposal) = SignerMessage::decode(&bytes)? else {
            return Err(SignerServiceError::UnexpectedMessage);
        };
        let chunk = self.signer.sign(&proposal)?;
        let acknowledgement = self.client.put_chunk(&self.signer_contract, &chunk).await?;
        if !acknowledgement.accepted {
            return Err(SignerServiceError::Rejected {
                reason: acknowledgement.reason,
                code: acknowledgement.code,
            });
        }
        self.last_proposal = Some(proposal_hash);
        Ok(Some(acknowledgement))
    }
}

#[cfg(test)]
mod tests {
    use nano_chainstate::{NakamotoBlock, NakamotoBlockHeader};
    use nano_crypto::StacksPrivateKey;
    use nano_primitives::{
        BitVec, BitcoinHeaderHash, ConsensusHash, Hash160, Sha256Sum, SortitionId, StacksBlockId,
        TrieHash, hash160,
    };
    use nano_stackerdb::{BlockProposal, SignerMessage};
    use nano_sync::SortitionInfo;

    use super::{
        EmbeddedSigner, ProposalValidator, SignerConfig, SignerError, SortitionProposalValidator,
    };

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

    fn valid_sortition_proposal() -> (BlockProposal, SortitionInfo) {
        let miner = StacksPrivateKey::from_seed(b"miner");
        let mut proposal = proposal();
        proposal.block.header.miner_signature =
            miner.sign(proposal.block.header.miner_signature_hash().as_bytes());
        let sortition = SortitionInfo {
            bitcoin_block_hash: BitcoinHeaderHash::from_bytes([1; 32]),
            bitcoin_height: proposal.bitcoin_height,
            bitcoin_timestamp: 1,
            sortition_id: SortitionId::from_bytes([2; 32]),
            parent_sortition_id: SortitionId::from_bytes([3; 32]),
            consensus_hash: proposal.block.header.consensus_hash,
            was_sortition: true,
            miner_public_key_hash: Some(hash160(&miner.public_key().to_bytes_compressed())),
            stacks_parent_consensus_hash: None,
            last_sortition_consensus_hash: None,
            committed_block_hash: None,
        };
        (proposal, sortition)
    }

    #[test]
    fn sortition_validator_authenticates_the_winning_miner() {
        let (proposal, sortition) = valid_sortition_proposal();
        let mut validator = SortitionProposalValidator::new(sortition.clone(), 1);

        validator.validate(&proposal).expect("valid proposal");

        let mut unexpected_miner = sortition;
        unexpected_miner.miner_public_key_hash = Some(Hash160::from_bytes([0; 20]));
        let mut validator = SortitionProposalValidator::new(unexpected_miner, 1);
        assert!(validator.validate(&proposal).is_err());
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

    #[test]
    fn repeated_proposals_reuse_the_original_response() {
        let mut signer = EmbeddedSigner::new(
            SignerConfig {
                private_key: StacksPrivateKey::from_seed(b"signer"),
                writer_slot: 7,
                next_slot_version: 3,
            },
            Accept,
        );
        let proposal = proposal();
        let first = signer.sign(&proposal).expect("sign proposal");
        let repeated = signer.sign(&proposal).expect("repeat proposal");

        assert_eq!(repeated, first);
        assert_eq!(signer.next_slot_version(), 4);
    }
}
