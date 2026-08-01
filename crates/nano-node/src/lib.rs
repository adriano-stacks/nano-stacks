pub mod config;
pub mod miner;
pub mod runtime;
pub mod signer;

use std::{fmt, path::Path};

use nano_bitcoin::BitcoinSource;
use nano_chainstate::{
    AppliedBlock, BitcoinBlockContext, ChainState, ChainStateError, NakamotoBlock,
    NakamotoBlockHeader, SignerSet, SignerSetError, TenureAccounting,
};
pub use nano_marf::{CheckpointAttestation, CheckpointManifest, CheckpointProvenance};
use nano_primitives::{Network, TrieHash};
use nano_sync::{FollowedTenure, Node, NodeView, PoxInfo, SyncClient, SyncError};

/// Executes a validated tenure stream from an imported checkpoint state.
#[derive(Debug)]
pub struct CheckpointExecutor<S> {
    chainstate: ChainState,
    tip: NakamotoBlock,
    bitcoin: S,
}

/// A follower that executes each accepted tenure update from a checkpointed state.
#[derive(Debug)]
pub struct ExecutingNode<S> {
    node: Node,
    executor: CheckpointExecutor<S>,
    executed_view: Option<NodeView>,
}

#[derive(Debug)]
pub enum NodeExecutionError {
    Sync(SyncError),
    Execution(CheckpointExecutionError),
    MissingView,
}

impl fmt::Display for NodeExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sync(error) => write!(formatter, "node synchronization failed: {error}"),
            Self::Execution(error) => write!(formatter, "node execution failed: {error}"),
            Self::MissingView => formatter.write_str("node has no complete validated view"),
        }
    }
}

impl std::error::Error for NodeExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sync(error) => Some(error),
            Self::Execution(error) => Some(error),
            Self::MissingView => None,
        }
    }
}

impl From<SyncError> for NodeExecutionError {
    fn from(error: SyncError) -> Self {
        Self::Sync(error)
    }
}

impl From<CheckpointExecutionError> for NodeExecutionError {
    fn from(error: CheckpointExecutionError) -> Self {
        Self::Execution(error)
    }
}

#[derive(Debug)]
pub enum CheckpointExecutionError {
    ChainState(ChainStateError),
    Bitcoin(String),
    Link(String),
}

impl fmt::Display for CheckpointExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChainState(error) => write!(formatter, "checkpoint execution failed: {error}"),
            Self::Bitcoin(error) => write!(formatter, "Bitcoin operation loading failed: {error}"),
            Self::Link(error) => {
                write!(formatter, "checkpoint execution chain link failed: {error}")
            }
        }
    }
}

impl std::error::Error for CheckpointExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ChainState(error) => Some(error),
            Self::Bitcoin(_) | Self::Link(_) => None,
        }
    }
}

impl From<ChainStateError> for CheckpointExecutionError {
    fn from(error: ChainStateError) -> Self {
        Self::ChainState(error)
    }
}

/// A checkpointed Clarity state, and what is needed to execute from it.
#[derive(Clone, Debug)]
pub struct Checkpoint<P> {
    /// The chain this state belongs to, which fixes how it is executed.
    pub network: Network,
    /// Path to the checkpoint's MARF.
    pub path: P,
    /// The MARF state the checkpoint was taken at.
    pub source: [u8; 32],
    /// The state root that state is published under.
    pub state_root: TrieHash,
    /// The matured native rewards the checkpoint still owes, if it records them.
    pub accounting: Option<TenureAccounting>,
}

impl<P> Checkpoint<P> {
    /// Take the state and root to import from what the checkpoint publishes.
    ///
    /// Attest the manifest first: `Checkpoint` carries no evidence, so whatever
    /// this is built from is what the node ends up believing.
    #[must_use]
    pub const fn from_manifest(network: Network, path: P, manifest: &CheckpointManifest) -> Self {
        Self {
            network,
            path,
            source: manifest.source_state_id,
            state_root: manifest.state_index_root,
            accounting: None,
        }
    }
}

/// Why a checkpoint is not the state the signed header at its height published.
#[derive(Debug)]
pub enum CheckpointTrustError {
    Height { claimed: u64, header: u64 },
    StateId { claimed: [u8; 32], header: [u8; 32] },
    StateRoot { claimed: TrieHash, header: TrieHash },
    Signers(SignerSetError),
    Provenance(nano_marf::CheckpointError),
}

impl fmt::Display for CheckpointTrustError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Height { claimed, header } => write!(
                formatter,
                "checkpoint claims Stacks height {claimed}, attesting header is at {header}"
            ),
            Self::StateId { claimed, header } => write!(
                formatter,
                "checkpoint claims state {claimed:02x?}, attesting header sealed {header:02x?}"
            ),
            Self::StateRoot { claimed, header } => write!(
                formatter,
                "checkpoint claims state root {claimed}, attesting header published {header}"
            ),
            Self::Signers(error) => {
                write!(formatter, "no reward set attested the checkpoint: {error}")
            }
            Self::Provenance(error) => {
                write!(formatter, "checkpoint provenance was refused: {error}")
            }
        }
    }
}

impl std::error::Error for CheckpointTrustError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Signers(error) => Some(error),
            Self::Provenance(error) => Some(error),
            Self::Height { .. } | Self::StateId { .. } | Self::StateRoot { .. } => None,
        }
    }
}

impl From<SignerSetError> for CheckpointTrustError {
    fn from(error: SignerSetError) -> Self {
        Self::Signers(error)
    }
}

impl From<nano_marf::CheckpointError> for CheckpointTrustError {
    fn from(error: nano_marf::CheckpointError) -> Self {
        Self::Provenance(error)
    }
}

/// Check a checkpoint against the signed Nakamoto header that published its state.
///
/// `signer_signature_hash` is taken over a preimage that contains
/// `state_index_root`, so a reward set that signed the header signed the root
/// too, and `block_id` binds both to one state. That moves the checkpoint's
/// root from something the operator asserts to something the chain's own
/// signers put threshold weight behind — as long as `signers` is the reward set
/// of that cycle obtained independently of the checkpoint. See
/// `docs/checkpoint-trust.md`.
pub fn attest_checkpoint(
    manifest: &CheckpointManifest,
    header: &NakamotoBlockHeader,
    signers: &SignerSet,
) -> Result<CheckpointAttestation, CheckpointTrustError> {
    if header.chain_length != manifest.stacks_height {
        return Err(CheckpointTrustError::Height {
            claimed: manifest.stacks_height,
            header: header.chain_length,
        });
    }
    if *header.block_id().as_bytes() != manifest.source_state_id {
        return Err(CheckpointTrustError::StateId {
            claimed: manifest.source_state_id,
            header: *header.block_id().as_bytes(),
        });
    }
    if header.state_index_root != manifest.state_index_root {
        return Err(CheckpointTrustError::StateRoot {
            claimed: manifest.state_index_root,
            header: header.state_index_root,
        });
    }
    Ok(CheckpointAttestation {
        attesting_block_id: *header.block_id().as_bytes(),
        signer_weight: signers.verify(header)?,
        approval_threshold: signers.approval_threshold()?,
    })
}

/// Attest a checkpoint and record it as the origin of a state directory.
///
/// This is the one place where what the operator configured, what the
/// checkpoint publishes and what the reward set signed are made to agree, and
/// where that agreement is written down — so a restart does not have to take
/// the configuration's word for it a second time.
pub fn adopt_checkpoint(
    state_directory: impl AsRef<Path>,
    manifest: &CheckpointManifest,
    header: &NakamotoBlockHeader,
    signers: &SignerSet,
) -> Result<CheckpointAttestation, CheckpointTrustError> {
    let attestation = attest_checkpoint(manifest, header, signers)?;
    CheckpointProvenance {
        checkpoint: manifest.clone(),
        attestation: Some(attestation),
    }
    .record(state_directory)?;
    Ok(attestation)
}

impl<S> CheckpointExecutor<S>
where
    S: BitcoinSource,
    S::Error: fmt::Display,
{
    /// Import a checkpoint and apply its first known descendant as the execution anchor.
    pub fn from_checkpoint(
        checkpoint: Checkpoint<impl AsRef<Path>>,
        anchor: NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
        bitcoin: S,
    ) -> Result<Self, CheckpointExecutionError> {
        let Checkpoint {
            network,
            path,
            source,
            state_root,
            accounting,
        } = checkpoint;
        let mut chainstate = ChainState::from_checkpoint(network, path, source, state_root)?;
        if let Some(accounting) = accounting {
            *chainstate.accounting_mut() = accounting;
        }
        Self::from_chainstate(chainstate, anchor, bitcoin_context, bitcoin)
    }

    /// Apply the checkpoint's first known descendant to an already-open state.
    pub fn from_chainstate(
        mut chainstate: ChainState,
        anchor: NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
        mut bitcoin: S,
    ) -> Result<Self, CheckpointExecutionError> {
        let operations = bitcoin
            .block_at(bitcoin_context.height)
            .map_err(|error| CheckpointExecutionError::Bitcoin(error.to_string()))?;
        let parent = chainstate.tip();
        chainstate.append_nakamoto_block_with_bitcoin_operations(
            bitcoin_context,
            &operations.operations,
            parent,
            &anchor,
        )?;
        Ok(Self {
            chainstate,
            tip: anchor,
            bitcoin,
        })
    }

    /// Continue from a chainstate that already holds the blocks up to `tip`.
    ///
    /// A durable chainstate outlives the process, so a restart adopts the block
    /// its state was sealed at instead of importing a checkpoint again.
    pub const fn resume(chainstate: ChainState, tip: NakamotoBlock, bitcoin: S) -> Self {
        Self {
            chainstate,
            tip,
            bitcoin,
        }
    }

    /// Apply the blocks in a followed tenure that extend the current execution tip.
    pub fn apply_followed_tenure(
        &mut self,
        tenure: &FollowedTenure,
        pox: &PoxInfo,
    ) -> Result<Vec<AppliedBlock>, CheckpointExecutionError> {
        let mut bitcoin_context = pox.bitcoin_context();
        bitcoin_context.height = tenure.sortition.bitcoin_height;
        let current_tip = self.tip.block_id();
        let blocks = tenure
            .blocks
            .iter()
            .skip_while(|block| block.block_id() != current_tip);
        let mut blocks = blocks.peekable();
        if blocks.peek().is_some() {
            blocks.next();
        }
        let mut applied = Vec::new();
        let operations = self
            .bitcoin
            .block_at(bitcoin_context.height)
            .map_err(|error| CheckpointExecutionError::Bitcoin(error.to_string()))?;
        for block in blocks {
            applied.push(self.apply_with_operations(
                block,
                bitcoin_context,
                &operations.operations,
            )?);
        }
        Ok(applied)
    }

    /// Validate and execute one direct descendant of the current execution tip.
    pub fn apply(
        &mut self,
        block: &NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
    ) -> Result<AppliedBlock, CheckpointExecutionError> {
        let operations = self
            .bitcoin
            .block_at(bitcoin_context.height)
            .map_err(|error| CheckpointExecutionError::Bitcoin(error.to_string()))?;
        self.apply_with_operations(block, bitcoin_context, &operations.operations)
    }

    /// Validate and execute one direct descendant with decoded Bitcoin operations.
    pub fn apply_with_operations(
        &mut self,
        block: &NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
        operations: &[nano_bitcoin::BitcoinOperation],
    ) -> Result<AppliedBlock, CheckpointExecutionError> {
        block.validate_successor(&self.tip.header).map_err(|error| {
            CheckpointExecutionError::Link(format!(
                "{error}: block {} at height {} names parent {}, but this node is at {} of height {}",
                block.block_id(),
                block.header.chain_length,
                block.header.parent_block_id,
                self.tip.block_id(),
                self.tip.header.chain_length,
            ))
        })?;
        let applied = self
            .chainstate
            .append_nakamoto_block_with_bitcoin_operations(
                bitcoin_context,
                operations,
                Some(*self.tip.block_id().as_bytes()),
                block,
            )?;
        self.tip = block.clone();
        Ok(applied)
    }

    /// Execute every canonical block between this tip and the peer's, oldest first.
    ///
    /// Walking the peer's ancestry backwards from its tip keeps the executed
    /// chain on the canonical fork even when the peer reorganized.
    pub async fn follow_to_tip(
        &mut self,
        node: &SyncClient,
        pox: &PoxInfo,
        max_blocks: usize,
    ) -> Result<usize, NodeExecutionError> {
        let mut pending = Vec::new();
        let mut block_id = node.tenure_info().await?.tip_block_id;
        while block_id != self.tip.block_id() {
            if pending.len() == max_blocks {
                return Err(CheckpointExecutionError::Link(
                    "checkpoint is farther from the peer tip than the block limit".to_owned(),
                )
                .into());
            }
            let block = node.block(block_id).await?;
            block_id = block.header.parent_block_id;
            pending.push(block);
        }
        let executed = pending.len();
        for block in pending.iter().rev() {
            let mut bitcoin_context = pox.bitcoin_context();
            bitcoin_context.height = node
                .sortition(block.header.consensus_hash)
                .await?
                .bitcoin_height;
            let bitcoin_context = node
                .tenure_coinbase_context(
                    block,
                    self.chainstate.accounting_mut().schedule(),
                    bitcoin_context,
                )
                .await?;
            self.apply(block, bitcoin_context)?;
        }
        Ok(executed)
    }

    /// Execute a candidate block on the current tip and seal its committed state root.
    pub fn assemble(
        &mut self,
        candidate: NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
        miner_key: &nano_crypto::StacksPrivateKey,
    ) -> Result<(NakamotoBlock, AppliedBlock), CheckpointExecutionError> {
        let operations = self
            .bitcoin
            .block_at(bitcoin_context.height)
            .map_err(|error| CheckpointExecutionError::Bitcoin(error.to_string()))?;
        Ok(self
            .chainstate
            .assemble_nakamoto_block_with_bitcoin_operations(
                bitcoin_context,
                &operations.operations,
                Some(*self.tip.block_id().as_bytes()),
                candidate,
                miner_key,
            )?)
    }

    /// Execute a candidate block together with transactions it may drop, and
    /// seal the state root the admitted set produces.
    pub fn assemble_selecting(
        &mut self,
        candidate: NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
        candidates: &[nano_codec::Transaction],
        miner_key: &nano_crypto::StacksPrivateKey,
    ) -> Result<(NakamotoBlock, AppliedBlock), CheckpointExecutionError> {
        let operations = self
            .bitcoin
            .block_at(bitcoin_context.height)
            .map_err(|error| CheckpointExecutionError::Bitcoin(error.to_string()))?;
        Ok(self.chainstate.assemble_nakamoto_block_selecting(
            bitcoin_context,
            &operations.operations,
            Some(*self.tip.block_id().as_bytes()),
            candidate,
            candidates,
            miner_key,
        )?)
    }

    /// Adopt a block this node produced as the new execution tip.
    pub fn accept_own_block(&mut self, block: NakamotoBlock) {
        self.tip = block;
    }

    /// Access the portable accounting ledger backing matured native rewards.
    pub const fn chainstate_mut(&mut self) -> &mut ChainState {
        &mut self.chainstate
    }

    /// Return the most recently executed block.
    #[must_use]
    pub const fn tip(&self) -> &NakamotoBlock {
        &self.tip
    }
}

/// The public RPC answers from the state this node executed, and from nothing
/// else: an account or a read-only call is read at the tip it sealed.
impl<S: Send> nano_rpc::ChainAccess for CheckpointExecutor<S> {
    fn account(
        &mut self,
        principal: &clarity::vm::types::PrincipalData,
    ) -> Result<nano_rpc::AccountEntry, nano_rpc::ChainAccessError> {
        nano_rpc::ChainAccess::account(&mut self.chainstate, principal)
    }

    fn call_read_only(
        &mut self,
        call: &nano_rpc::ReadOnlyCall,
    ) -> Result<clarity::vm::Value, nano_rpc::ChainAccessError> {
        nano_rpc::ChainAccess::call_read_only(&mut self.chainstate, call)
    }
}

impl<S> ExecutingNode<S>
where
    S: BitcoinSource,
    S::Error: fmt::Display,
{
    /// Couple a peer follower to a checkpoint executor.
    #[must_use]
    pub const fn new(node: Node, executor: CheckpointExecutor<S>) -> Self {
        Self {
            node,
            executor,
            executed_view: None,
        }
    }

    /// Follow, validate, and execute the peer's next tenure update.
    pub async fn poll(&mut self) -> Result<Option<FollowedTenure>, NodeExecutionError> {
        let followed = self.node.poll().await?;
        let view = self.node.view().ok_or(NodeExecutionError::MissingView)?;
        let current_tip = self.executor.tip().block_id();
        let first_tenure = view
            .tenures
            .iter()
            .position(|tenure| {
                tenure
                    .blocks
                    .iter()
                    .any(|block| block.block_id() == current_tip)
            })
            .unwrap_or_else(|| view.tenures.len().saturating_sub(1));
        for tenure in &view.tenures[first_tenure..] {
            self.executor
                .apply_followed_tenure(tenure, &view.pox_info)?;
        }
        self.executed_view = Some(view);
        Ok(followed)
    }

    /// Return the executed chain tip.
    #[must_use]
    pub const fn executed_tip(&self) -> &NakamotoBlock {
        self.executor.tip()
    }

    /// Return the latest completely executed node view.
    #[must_use]
    pub fn view(&self) -> Option<NodeView> {
        self.executed_view.clone()
    }
}
