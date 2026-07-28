#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

use atomicwrites::{AllowOverwrite, AtomicFile};
use fs2::FileExt;
use nano_bitcoin::BitcoinSource;
use nano_chainstate::{BitcoinBlockContext, ChainState, NakamotoBlock};
use nano_crypto::StacksPrivateKey;
use nano_primitives::{ConsensusHash, Sha256Sum, hash160};
use nano_stackerdb::{
    BlockAcceptance, BlockProposal, BlockResponse, Chunk, ChunkAck, CurrentMiner,
    LATEST_SIGNER_PROTOCOL_VERSION, SignerMessage, StackerDbClient, StackerDbClientError,
    StackerDbContract, StackerDbError, StateMachineUpdate,
};
use nano_sync::{SortitionInfo, SyncClient, SyncError};
use serde::{Deserialize, Serialize};

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

/// Adds refreshed sortition authentication to another proposal validator.
pub struct ActiveSortitionValidator<V> {
    context: Option<SortitionProposalValidator>,
    validator: V,
}

impl<V> ActiveSortitionValidator<V> {
    /// Construct a validator that refuses proposals until its Bitcoin context is refreshed.
    #[must_use]
    pub const fn new(validator: V) -> Self {
        Self {
            context: None,
            validator,
        }
    }

    /// Replace the active Bitcoin sortition and reward-cycle context.
    pub const fn set_context(&mut self, sortition: SortitionInfo, reward_cycle: u64) {
        self.context = Some(SortitionProposalValidator::new(sortition, reward_cycle));
    }

    /// Return the wrapped validator.
    #[must_use]
    pub fn into_inner(self) -> V {
        self.validator
    }

    /// Return the wrapped proposal validator.
    #[must_use]
    pub const fn validator_mut(&mut self) -> &mut V {
        &mut self.validator
    }
}

impl<V: ProposalValidator> ProposalValidator for ActiveSortitionValidator<V> {
    fn validate(&mut self, proposal: &BlockProposal) -> Result<(), String> {
        self.context
            .as_mut()
            .ok_or_else(|| "proposal sortition context has not been refreshed".to_owned())?
            .validate(proposal)?;
        self.validator.validate(proposal)
    }
}

/// Validates proposal execution from a trusted, checkpointed chain state.
#[derive(Debug)]
pub struct ChainstateProposalValidator<S> {
    chainstate: ChainState,
    bitcoin_context: BitcoinBlockContext,
    bitcoin: S,
    trusted: BTreeMap<nano_primitives::StacksBlockId, nano_chainstate::NakamotoBlockHeader>,
    candidates: BTreeMap<nano_primitives::StacksBlockId, nano_chainstate::NakamotoBlockHeader>,
}

impl<S> ChainstateProposalValidator<S>
where
    S: BitcoinSource,
    S::Error: fmt::Display,
{
    /// Start validating proposals using the authoritative Bitcoin operation source.
    #[must_use]
    pub fn new(
        chainstate: ChainState,
        anchor: &NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
        bitcoin: S,
    ) -> Self {
        let mut trusted = BTreeMap::new();
        trusted.insert(anchor.block_id(), anchor.header.clone());
        Self {
            chainstate,
            bitcoin_context,
            bitcoin,
            trusted,
            candidates: BTreeMap::new(),
        }
    }

    /// Record an observed block after its state has been independently verified.
    pub fn observe(&mut self, block: &NakamotoBlock, bitcoin_height: u64) -> Result<(), String> {
        let block_id = block.block_id();
        if let Some(candidate) = self.candidates.remove(&block_id) {
            // A proposal carries no signer signatures, so only the miner-signed
            // content can be compared against the block that was accepted.
            if candidate.miner_signature_hash() != block.header.miner_signature_hash() {
                return Err("observed block differs from the validated candidate".to_owned());
            }
        } else {
            self.validate_block(block, self.context_at(bitcoin_height))?;
        }
        self.trusted.insert(block_id, block.header.clone());
        Ok(())
    }

    /// Return whether a block is already in the independently verified chain view.
    #[must_use]
    pub fn has_trusted_block(&self, block_id: &nano_primitives::StacksBlockId) -> bool {
        self.trusted.contains_key(block_id)
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
        let operations = self
            .bitcoin
            .block_at(bitcoin_context.height)
            .map_err(|error| format!("could not load Bitcoin operations: {error}"))?;
        self.chainstate
            .append_nakamoto_block_with_bitcoin_operations(
                bitcoin_context,
                &operations.operations,
                Some(*block.header.parent_block_id.as_bytes()),
                block,
            )
            .map_err(|error| format!("proposal execution failed: {error}"))?;
        Ok(())
    }
}

impl<S> ProposalValidator for ChainstateProposalValidator<S>
where
    S: BitcoinSource,
    S::Error: fmt::Display,
{
    fn validate(&mut self, proposal: &BlockProposal) -> Result<(), String> {
        let block_id = proposal.block.block_id();
        // A block already executed into the trusted view cannot be executed twice.
        if self.trusted.contains_key(&block_id)
            || self.candidates.get(&block_id) == Some(&proposal.block.header)
        {
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

const SIGNER_STATE_VERSION: u8 = 1;

#[derive(Debug)]
pub enum SignerStateError {
    Io(io::Error),
    Decode(serde_json::Error),
    Invalid(String),
    Chunk(StackerDbError),
    Message(nano_stackerdb::SignerMessageError),
}

impl fmt::Display for SignerStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "signer state I/O failed: {error}"),
            Self::Decode(error) => write!(formatter, "invalid signer state JSON: {error}"),
            Self::Invalid(error) => write!(formatter, "invalid signer state: {error}"),
            Self::Chunk(error) => write!(formatter, "invalid signer state chunk: {error}"),
            Self::Message(error) => write!(formatter, "invalid signer state message: {error}"),
        }
    }
}

impl std::error::Error for SignerStateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Decode(error) => Some(error),
            Self::Chunk(error) => Some(error),
            Self::Message(error) => Some(error),
            Self::Invalid(_) => None,
        }
    }
}

impl From<io::Error> for SignerStateError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for SignerStateError {
    fn from(error: serde_json::Error) -> Self {
        Self::Decode(error)
    }
}

impl From<StackerDbError> for SignerStateError {
    fn from(error: StackerDbError) -> Self {
        Self::Chunk(error)
    }
}

impl From<nano_stackerdb::SignerMessageError> for SignerStateError {
    fn from(error: nano_stackerdb::SignerMessageError) -> Self {
        Self::Message(error)
    }
}

#[derive(Deserialize, Serialize)]
struct PersistedSignerState {
    version: u8,
    writer_public_key_hash: String,
    writer_slot: u32,
    next_slot_version: u32,
    signed: Vec<PersistedSignedBlock>,
}

#[derive(Deserialize, Serialize)]
struct PersistedSignedBlock {
    consensus_hash: String,
    chain_length: u64,
    signature_hash: String,
    chunk: String,
}

type SignedBlocks = BTreeMap<(ConsensusHash, u64), SignedBlock>;
type LoadedSignerState = (u32, SignedBlocks);

struct SignerStateStore {
    path: PathBuf,
    _lock: File,
}

impl SignerStateStore {
    fn open(
        path: impl AsRef<Path>,
        config: &SignerConfig,
    ) -> Result<(Self, u32, SignedBlocks), SignerStateError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(PathBuf::from(lock_path))?;
        lock.try_lock_exclusive()?;
        let store = Self { path, _lock: lock };

        if !store.path.exists() {
            return Ok((store, config.next_slot_version, BTreeMap::new()));
        }

        let persisted = serde_json::from_slice(&fs::read(&store.path)?)?;
        let signed = Self::validate(persisted, config)?;
        Ok((store, signed.0, signed.1))
    }

    fn validate(
        persisted: PersistedSignerState,
        config: &SignerConfig,
    ) -> Result<LoadedSignerState, SignerStateError> {
        if persisted.version != SIGNER_STATE_VERSION {
            return Err(SignerStateError::Invalid(format!(
                "unsupported format version {}",
                persisted.version
            )));
        }
        let writer = writer_identity(&config.private_key);
        if persisted.writer_public_key_hash != hex::encode(writer.as_bytes()) {
            return Err(SignerStateError::Invalid(
                "writer key does not match this signer".to_owned(),
            ));
        }
        if persisted.writer_slot != config.writer_slot {
            return Err(SignerStateError::Invalid(
                "writer slot does not match this signer".to_owned(),
            ));
        }

        let mut signed = BTreeMap::new();
        for entry in persisted.signed {
            let consensus_hash = ConsensusHash::from_bytes(decode_hex(&entry.consensus_hash)?);
            let signature_hash = Sha256Sum::from_bytes(decode_hex(&entry.signature_hash)?);
            let chunk = Chunk::decode(&hex::decode(&entry.chunk).map_err(|error| {
                SignerStateError::Invalid(format!("invalid chunk hex: {error}"))
            })?)?;
            if chunk.slot_id != persisted.writer_slot {
                return Err(SignerStateError::Invalid(
                    "stored chunk has the wrong writer slot".to_owned(),
                ));
            }
            if chunk.slot_version >= persisted.next_slot_version {
                return Err(SignerStateError::Invalid(
                    "stored chunk version has not been reserved".to_owned(),
                ));
            }
            if !chunk.verify(writer)? {
                return Err(SignerStateError::Invalid(
                    "stored chunk was not signed by this signer".to_owned(),
                ));
            }
            let SignerMessage::BlockResponse(BlockResponse::Accepted(response)) =
                SignerMessage::decode(&chunk.data)?
            else {
                return Err(SignerStateError::Invalid(
                    "stored chunk is not a block response".to_owned(),
                ));
            };
            if response.signer_signature_hash != signature_hash {
                return Err(SignerStateError::Invalid(
                    "stored response hashes a different block".to_owned(),
                ));
            }
            let response_key = response
                .signature
                .recover(signature_hash.as_bytes())
                .map_err(|error| {
                    SignerStateError::Invalid(format!("invalid response signature: {error}"))
                })?;
            if hash160(&response_key.to_bytes_compressed()) != writer {
                return Err(SignerStateError::Invalid(
                    "stored response was not signed by this signer".to_owned(),
                ));
            }
            let position = (consensus_hash, entry.chain_length);
            if signed
                .insert(
                    position,
                    SignedBlock {
                        signature_hash,
                        chunk,
                    },
                )
                .is_some()
            {
                return Err(SignerStateError::Invalid(
                    "duplicate signed block position".to_owned(),
                ));
            }
        }
        Ok((persisted.next_slot_version, signed))
    }

    fn persist(
        &self,
        private_key: &StacksPrivateKey,
        writer_slot: u32,
        next_slot_version: u32,
        signed: &SignedBlocks,
    ) -> Result<(), SignerStateError> {
        let signed = signed
            .iter()
            .map(|((consensus_hash, chain_length), block)| {
                Ok(PersistedSignedBlock {
                    consensus_hash: hex::encode(consensus_hash.as_bytes()),
                    chain_length: *chain_length,
                    signature_hash: hex::encode(block.signature_hash.as_bytes()),
                    chunk: hex::encode(block.chunk.encode()?),
                })
            })
            .collect::<Result<Vec<_>, StackerDbError>>()?;
        let bytes = serde_json::to_vec_pretty(&PersistedSignerState {
            version: SIGNER_STATE_VERSION,
            writer_public_key_hash: hex::encode(writer_identity(private_key).as_bytes()),
            writer_slot,
            next_slot_version,
            signed,
        })
        .map_err(|error| {
            SignerStateError::Invalid(format!("could not encode signer state: {error}"))
        })?;
        AtomicFile::new(&self.path, AllowOverwrite)
            .write(|file| io::Write::write_all(file, &bytes))
            .map_err(|error| SignerStateError::Io(error.into()))
    }
}

fn decode_hex<const N: usize>(value: &str) -> Result<[u8; N], SignerStateError> {
    let bytes = hex::decode(value).map_err(|error| {
        SignerStateError::Invalid(format!("invalid hexadecimal value: {error}"))
    })?;
    let length = bytes.len();
    bytes
        .try_into()
        .map_err(|_| SignerStateError::Invalid(format!("expected {N} bytes, found {length}")))
}

fn writer_identity(private_key: &StacksPrivateKey) -> nano_primitives::Hash160 {
    hash160(&private_key.public_key().to_bytes_compressed())
}

/// A stateful signer that emits authenticated `StackerDB` acceptance chunks.
pub struct EmbeddedSigner<V> {
    private_key: StacksPrivateKey,
    validator: V,
    writer_slot: u32,
    next_slot_version: u32,
    signed: SignedBlocks,
    state: Option<SignerStateStore>,
}

#[derive(Clone)]
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
    State(SignerStateError),
}

impl fmt::Display for SignerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation(error) => write!(formatter, "proposal validation failed: {error}"),
            Self::Equivocation => formatter.write_str("refusing to sign a conflicting block"),
            Self::SlotVersionOverflow => formatter.write_str("StackerDB slot version overflow"),
            Self::Message(error) => write!(formatter, "signer message error: {error}"),
            Self::Chunk(error) => write!(formatter, "StackerDB chunk error: {error}"),
            Self::State(error) => write!(formatter, "signer state error: {error}"),
        }
    }
}

impl std::error::Error for SignerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Message(error) => Some(error),
            Self::Chunk(error) => Some(error),
            Self::State(error) => Some(error),
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

impl From<SignerStateError> for SignerError {
    fn from(error: SignerStateError) -> Self {
        Self::State(error)
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

/// A decoded miner proposal that has not yet been answered by this signer.
#[derive(Clone, Debug)]
pub struct PendingProposal {
    pub hash: Sha256Sum,
    pub proposal: BlockProposal,
}

/// Couples a signer service to live, authenticated Bitcoin sortition data.
pub struct LiveSigner<V> {
    client: SyncClient,
    service: SignerService<ActiveSortitionValidator<V>>,
}

/// Errors raised while refreshing and responding to a live miner proposal.
#[derive(Debug)]
pub enum LiveSignerError {
    Sync(SyncError),
    Service(SignerServiceError),
    RewardCycle { expected: u64, actual: u64 },
}

impl fmt::Display for LiveSignerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sync(error) => write!(formatter, "live signer synchronization failed: {error}"),
            Self::Service(error) => write!(formatter, "live signer service failed: {error}"),
            Self::RewardCycle { expected, actual } => write!(
                formatter,
                "proposal reward cycle {actual} does not match the active cycle {expected}"
            ),
        }
    }
}

impl std::error::Error for LiveSignerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sync(error) => Some(error),
            Self::Service(error) => Some(error),
            Self::RewardCycle { .. } => None,
        }
    }
}

impl From<SyncError> for LiveSignerError {
    fn from(error: SyncError) -> Self {
        Self::Sync(error)
    }
}

impl From<SignerServiceError> for LiveSignerError {
    fn from(error: SignerServiceError) -> Self {
        Self::Service(error)
    }
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
            state: None,
        }
    }

    /// Reopen a signer journal and retain its lock for this signer's lifetime.
    pub fn from_state_file(
        config: SignerConfig,
        validator: V,
        path: impl AsRef<Path>,
    ) -> Result<Self, SignerStateError> {
        let (state, next_slot_version, signed) = SignerStateStore::open(path, &config)?;
        Ok(Self {
            private_key: config.private_key,
            validator,
            writer_slot: config.writer_slot,
            next_slot_version,
            signed,
            state: Some(state),
        })
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
        let next_slot_version = self.next_slot_version;
        let signature = self.private_key.sign(signature_hash.as_bytes());
        let message = SignerMessage::BlockResponse(BlockResponse::Accepted(BlockAcceptance::new(
            signature_hash,
            signature,
        )));
        self.record(
            position,
            signature_hash,
            message.encode()?,
            next_slot_version,
        )
    }

    /// Reissue an existing acceptance only when `StackerDB` has consumed its slot version.
    pub fn sign_after_slot_version(
        &mut self,
        proposal: &BlockProposal,
        remote_slot_version: u32,
    ) -> Result<Chunk, SignerError> {
        let position = (
            proposal.block.header.consensus_hash,
            proposal.block.header.chain_length,
        );
        let signature_hash = proposal.block.header.signer_signature_hash();
        let Some(signed) = self.signed.get(&position) else {
            return self.sign(proposal);
        };
        if signed.signature_hash != signature_hash {
            return Err(SignerError::Equivocation);
        }
        if signed.chunk.slot_version > remote_slot_version {
            return Ok(signed.chunk.clone());
        }
        self.record(
            position,
            signature_hash,
            signed.chunk.data.clone(),
            self.next_slot_version,
        )
    }

    fn record(
        &mut self,
        position: (ConsensusHash, u64),
        signature_hash: Sha256Sum,
        data: Vec<u8>,
        slot_version: u32,
    ) -> Result<Chunk, SignerError> {
        let next_slot_version = slot_version
            .checked_add(1)
            .ok_or(SignerError::SlotVersionOverflow)?;
        let mut chunk = Chunk::new(self.writer_slot, slot_version, data);
        chunk.sign(&self.private_key)?;
        let signed = SignedBlock {
            signature_hash,
            chunk: chunk.clone(),
        };
        let mut next_signed = self.signed.clone();
        next_signed.insert(position, signed);
        if let Some(state) = &self.state {
            state.persist(
                &self.private_key,
                self.writer_slot,
                next_slot_version,
                &next_signed,
            )?;
        }
        self.signed = next_signed;
        self.next_slot_version = next_slot_version;
        Ok(chunk)
    }

    /// Return the next version that will be used for a response chunk.
    #[must_use]
    pub const fn next_slot_version(&self) -> u32 {
        self.next_slot_version
    }

    /// Return this signer's `StackerDB` writer slot.
    #[must_use]
    pub const fn writer_slot(&self) -> u32 {
        self.writer_slot
    }

    /// Persistently advance the writer version when the remote slot is newer.
    pub fn advance_next_slot_version(&mut self, next_slot_version: u32) -> Result<(), SignerError> {
        if next_slot_version <= self.next_slot_version {
            return Ok(());
        }
        if let Some(state) = &self.state {
            state.persist(
                &self.private_key,
                self.writer_slot,
                next_slot_version,
                &self.signed,
            )?;
        }
        self.next_slot_version = next_slot_version;
        Ok(())
    }

    /// Return the proposal validator used before signing.
    #[must_use]
    pub const fn validator_mut(&mut self) -> &mut V {
        &mut self.validator
    }
}

impl<V: ProposalValidator + Send> SignerService<V> {
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
        let Some(pending) = self.next_proposal().await? else {
            return Ok(None);
        };
        self.respond(pending).await.map(Some)
    }

    /// Fetch and decode a miner proposal that has not already been answered.
    pub async fn next_proposal(&mut self) -> Result<Option<PendingProposal>, SignerServiceError> {
        let client = self.client.clone();
        let miner_contract = self.miner_contract.clone();
        let last_proposal = self.last_proposal;
        let Some(bytes) = client.latest_chunk(&miner_contract, 0).await? else {
            return Ok(None);
        };
        let proposal_hash = nano_primitives::sha512_256(&bytes);
        if last_proposal == Some(proposal_hash) {
            return Ok(None);
        }
        let SignerMessage::BlockProposal(proposal) = SignerMessage::decode(&bytes)? else {
            return Err(SignerServiceError::UnexpectedMessage);
        };
        Ok(Some(PendingProposal {
            hash: proposal_hash,
            proposal,
        }))
    }

    /// Validate and publish a response for a previously fetched miner proposal.
    pub async fn respond(
        &mut self,
        pending: PendingProposal,
    ) -> Result<ChunkAck, SignerServiceError> {
        let remote_slot_version = self.reconcile_writer_slot().await?;
        let chunk = self
            .signer
            .sign_after_slot_version(&pending.proposal, remote_slot_version)?;
        let client = self.client.clone();
        let signer_contract = self.signer_contract.clone();
        let acknowledgement = client.put_chunk(&signer_contract, &chunk).await?;
        if !acknowledgement.accepted {
            return Err(SignerServiceError::Rejected {
                reason: acknowledgement.reason,
                code: acknowledgement.code,
            });
        }
        self.last_proposal = Some(pending.hash);
        Ok(acknowledgement)
    }

    /// Advance the local writer version to the latest version accepted by `StackerDB`.
    pub async fn reconcile_writer_slot(&mut self) -> Result<u32, SignerServiceError> {
        let client = self.client.clone();
        let signer_contract = self.signer_contract.clone();
        let writer_slot = self.signer.writer_slot();
        let version = client
            .slot_versions(&signer_contract)
            .await?
            .into_iter()
            .find(|slot| slot.slot_id == writer_slot)
            .map_or(0, |slot| slot.slot_version);
        let next = version
            .checked_add(1)
            .ok_or(SignerError::SlotVersionOverflow)?;
        self.signer.advance_next_slot_version(next)?;
        Ok(version)
    }

    /// Return the embedded signer so its Bitcoin context can be refreshed before a response.
    #[must_use]
    pub const fn signer_mut(&mut self) -> &mut EmbeddedSigner<V> {
        &mut self.signer
    }
}

/// Publishes this signer's protocol version and miner view to the reward set.
///
/// Stock signers will not validate a block until a weighted majority of the
/// reward set has announced a protocol version, so a signer that only responds
/// to proposals blocks the network instead of abstaining from it.
pub struct StateAnnouncer {
    client: StackerDbClient,
    contract: StackerDbContract,
    writer_slot: u32,
    private_key: StacksPrivateKey,
    announced: Option<StateMachineUpdate>,
}

impl StateAnnouncer {
    #[must_use]
    pub const fn new(
        client: StackerDbClient,
        contract: StackerDbContract,
        writer_slot: u32,
        private_key: StacksPrivateKey,
    ) -> Self {
        Self {
            client,
            contract,
            writer_slot,
            private_key,
            announced: None,
        }
    }

    /// Publish this signer's view of the peer's Bitcoin tip, unless it is unchanged.
    pub async fn announce(
        &mut self,
        node: &SyncClient,
    ) -> Result<Option<ChunkAck>, LiveSignerError> {
        let update = Self::derive(node).await?;
        if self.announced.as_ref() == Some(&update) {
            return Ok(None);
        }
        let slot_version = self
            .client
            .slot_versions(&self.contract)
            .await
            .map_err(SignerServiceError::from)?
            .into_iter()
            .find(|slot| slot.slot_id == self.writer_slot)
            .map_or(0, |slot| slot.slot_version)
            .checked_add(1)
            .ok_or(SignerServiceError::Signer(SignerError::SlotVersionOverflow))?;
        let mut chunk = Chunk::new(
            self.writer_slot,
            slot_version,
            SignerMessage::StateMachineUpdate(update.clone())
                .encode()
                .map_err(SignerServiceError::from)?,
        );
        chunk
            .sign(&self.private_key)
            .map_err(|error| SignerServiceError::Signer(SignerError::Chunk(error)))?;
        let acknowledgement = self
            .client
            .put_chunk(&self.contract, &chunk)
            .await
            .map_err(SignerServiceError::from)?;
        if !acknowledgement.accepted {
            return Err(SignerServiceError::Rejected {
                reason: acknowledgement.reason,
                code: acknowledgement.code,
            }
            .into());
        }
        self.announced = Some(update);
        Ok(Some(acknowledgement))
    }

    async fn derive(node: &SyncClient) -> Result<StateMachineUpdate, LiveSignerError> {
        let bitcoin_tip = node.sortition_tip().await?;
        let current_miner = Self::current_miner(node, &bitcoin_tip).await?;
        Ok(StateMachineUpdate {
            active_protocol_version: LATEST_SIGNER_PROTOCOL_VERSION,
            local_supported_protocol_version: LATEST_SIGNER_PROTOCOL_VERSION,
            bitcoin_consensus_hash: bitcoin_tip.consensus_hash,
            bitcoin_height: bitcoin_tip.bitcoin_height,
            current_miner,
            replay_transactions: Vec::new(),
        })
    }

    async fn current_miner(
        node: &SyncClient,
        bitcoin_tip: &SortitionInfo,
    ) -> Result<CurrentMiner, LiveSignerError> {
        let (Some(public_key_hash), Some(parent_tenure_consensus_hash)) = (
            bitcoin_tip.miner_public_key_hash,
            bitcoin_tip.stacks_parent_consensus_hash,
        ) else {
            return Ok(CurrentMiner::None);
        };
        let tenure = node.tenure_info().await?;
        // The winner starts its tenure from the last block of the parent tenure,
        // which is this tenure's start block's parent once that block exists.
        let (parent_tenure_last_block, parent_tenure_last_block_height) =
            if tenure.consensus_hash == bitcoin_tip.consensus_hash {
                let start = node.block(tenure.tenure_start_block_id).await?;
                (
                    start.header.parent_block_id,
                    start.header.chain_length.saturating_sub(1),
                )
            } else {
                (tenure.tip_block_id, tenure.tip_height)
            };
        Ok(CurrentMiner::Active {
            public_key_hash,
            tenure_consensus_hash: bitcoin_tip.consensus_hash,
            parent_tenure_consensus_hash,
            parent_tenure_last_block,
            parent_tenure_last_block_height,
        })
    }
}

impl<V: ProposalValidator + Send> LiveSigner<V> {
    /// Construct a live signer from an HTTP peer and its `StackerDB` service.
    #[must_use]
    pub const fn new(
        client: SyncClient,
        service: SignerService<ActiveSortitionValidator<V>>,
    ) -> Self {
        Self { client, service }
    }

    /// Return the active validator so its independently verified chain view can advance.
    #[must_use]
    pub const fn validator_mut(&mut self) -> &mut ActiveSortitionValidator<V> {
        self.service.signer_mut().validator_mut()
    }

    /// Fetch, authenticate, validate, and answer the latest miner proposal once.
    pub async fn poll(&mut self) -> Result<Option<ChunkAck>, LiveSignerError> {
        let Some(pending) = self.service.next_proposal().await? else {
            return Ok(None);
        };
        let tenure = self.client.tenure_info().await?;
        if pending.proposal.reward_cycle != tenure.reward_cycle {
            return Err(LiveSignerError::RewardCycle {
                expected: tenure.reward_cycle,
                actual: pending.proposal.reward_cycle,
            });
        }
        let sortition = self
            .client
            .sortition(pending.proposal.block.header.consensus_hash)
            .await?;
        self.service
            .signer_mut()
            .validator_mut()
            .set_context(sortition, tenure.reward_cycle);
        self.service
            .respond(pending)
            .await
            .map(Some)
            .map_err(Into::into)
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
    use tempfile::tempdir;

    use super::{
        ActiveSortitionValidator, EmbeddedSigner, ProposalValidator, SignerConfig, SignerError,
        SortitionProposalValidator,
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
            data: BlockProposal::empty_data(),
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
            vrf_seed: None,
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
    fn active_sortition_validator_requires_a_refreshed_context() {
        let (proposal, sortition) = valid_sortition_proposal();
        let mut validator = ActiveSortitionValidator::new(Accept);
        assert!(validator.validate(&proposal).is_err());

        validator.set_context(sortition, proposal.reward_cycle);
        validator.validate(&proposal).expect("refreshed context");
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

    #[test]
    fn consumed_slot_reissues_the_same_acceptance_at_the_next_version() {
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
        let repeated = signer
            .sign_after_slot_version(&proposal, first.slot_version)
            .expect("reissue accepted proposal");

        assert_eq!(repeated.slot_version, 4);
        assert_eq!(repeated.data, first.data);
        assert_eq!(signer.next_slot_version(), 5);
    }

    #[test]
    fn signer_journal_restores_signed_responses_after_restart() {
        let directory = tempdir().expect("create temporary signer directory");
        let path = directory.path().join("signer-state.json");
        let config = SignerConfig {
            private_key: StacksPrivateKey::from_seed(b"signer"),
            writer_slot: 7,
            next_slot_version: 3,
        };
        let proposal = proposal();
        let first = {
            let mut signer = EmbeddedSigner::from_state_file(config.clone(), Accept, &path)
                .expect("open signer journal");
            signer.sign(&proposal).expect("sign proposal")
        };

        let mut signer =
            EmbeddedSigner::from_state_file(config, Accept, &path).expect("reopen signer journal");
        assert_eq!(signer.sign(&proposal).expect("reuse response"), first);
        assert_eq!(signer.next_slot_version(), 4);

        let mut conflicting = proposal;
        conflicting.block.header.transaction_merkle_root = Sha256Sum::from_bytes([9; 32]);
        assert!(matches!(
            signer.sign(&conflicting),
            Err(SignerError::Equivocation)
        ));
    }

    #[test]
    fn signer_journal_persists_remote_writer_version_advances() {
        let directory = tempdir().expect("create temporary signer directory");
        let path = directory.path().join("signer-state.json");
        let config = SignerConfig {
            private_key: StacksPrivateKey::from_seed(b"signer"),
            writer_slot: 7,
            next_slot_version: 3,
        };
        {
            let mut signer = EmbeddedSigner::from_state_file(config.clone(), Accept, &path)
                .expect("open signer journal");
            signer
                .advance_next_slot_version(9)
                .expect("advance writer version");
        }

        let signer =
            EmbeddedSigner::from_state_file(config, Accept, &path).expect("reopen signer journal");
        assert_eq!(signer.next_slot_version(), 9);
    }
}
