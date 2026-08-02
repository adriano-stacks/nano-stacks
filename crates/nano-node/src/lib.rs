pub mod config;
pub mod miner;
pub mod runtime;
pub mod signer;
pub mod staging;

use std::{fmt, path::Path};

use nano_bitcoin::BitcoinSource;
use nano_chainstate::{
    AppliedBlock, BitcoinBlockContext, ChainState, ChainStateError, NakamotoBlock,
    NakamotoBlockHeader, SignerSet, SignerSetError, TenureAccounting,
};
pub use nano_marf::{CheckpointAttestation, CheckpointManifest, CheckpointProvenance};
use nano_primitives::{Network, StacksBlockId, TrieHash};
use nano_sync::{FollowedTenure, Node, NodeView, PoxInfo, SyncClient, SyncError};

use crate::staging::{Staging, StagingError};

/// Executes a validated tenure stream from an imported checkpoint state.
#[derive(Debug)]
pub struct CheckpointExecutor<S> {
    chainstate: ChainState,
    tip: NakamotoBlock,
    /// The Bitcoin height the sealed tip was executed under.
    ///
    /// A block header carries the burn it spent, not the height it landed at,
    /// and nothing else records it per block — so the executor keeps it, since
    /// it is what a caller asking how far this node has come actually means.
    bitcoin_height: u64,
    bitcoin: S,
}

/// Where a descent stops: the block this node has executed, by identity and by
/// height, because a batch can step over the one without reaching the other.
#[derive(Clone, Copy, Debug)]
struct Stop {
    block_id: StacksBlockId,
    height: u64,
}

/// How much one round of catching up is allowed to do.
#[derive(Clone, Copy, Debug)]
pub struct CatchUpBudget {
    /// Blocks this round will fetch from the peer.
    pub fetch: usize,
    /// Blocks this round will execute and seal.
    pub execute: usize,
}

/// What one round of catching up actually did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CatchUpRound {
    pub fetched: usize,
    pub executed: usize,
    /// Blocks fetched but not yet executed.
    pub staged: u64,
    /// Whether the peer asked this node to slow down, which ends a round
    /// successfully rather than discarding it.
    pub rate_limited: bool,
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
    /// A tenure the descent asked for could not be used, and which one.
    Descent {
        tenure: StacksBlockId,
        error: SyncError,
    },
    Execution(CheckpointExecutionError),
    Staging(StagingError),
    MissingView,
}

impl From<StagingError> for NodeExecutionError {
    fn from(error: StagingError) -> Self {
        Self::Staging(error)
    }
}

impl fmt::Display for NodeExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sync(error) => write!(formatter, "node synchronization failed: {error}"),
            Self::Descent { tenure, error } => {
                write!(formatter, "descending through tenure {tenure} failed: {error}")
            }
            Self::Execution(error) => write!(formatter, "node execution failed: {error}"),
            Self::Staging(error) => write!(formatter, "node staging failed: {error}"),
            Self::MissingView => formatter.write_str("node has no complete validated view"),
        }
    }
}

impl std::error::Error for NodeExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sync(error) | Self::Descent { error, .. } => Some(error),
            Self::Execution(error) => Some(error),
            Self::Staging(error) => Some(error),
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
            bitcoin_height: bitcoin_context.height,
            bitcoin,
        })
    }

    /// Continue from a chainstate that already holds the blocks up to `tip`.
    ///
    /// A durable chainstate outlives the process, so a restart adopts the block
    /// its state was sealed at instead of importing a checkpoint again.
    /// The Bitcoin height is not on disk, so a resumed executor reports zero
    /// until it applies a block of its own — an honest unknown rather than a
    /// height it made up.
    pub const fn resume(chainstate: ChainState, tip: NakamotoBlock, bitcoin: S) -> Self {
        Self {
            chainstate,
            tip,
            bitcoin_height: 0,
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
        bitcoin_context.burn_header_hash = *tenure.sortition.bitcoin_block_hash.as_bytes();
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
        self.bitcoin_height = bitcoin_context.height;
        Ok(applied)
    }

    /// The Bitcoin height the sealed tip was executed under.
    #[must_use]
    pub const fn bitcoin_height(&self) -> u64 {
        self.bitcoin_height
    }

    /// Write down the headers of blocks this node executed before it kept any.
    ///
    /// A contract reading the block it is standing on gets `none` otherwise,
    /// and the transaction carrying it fails against a network that answered.
    /// Refetching the blocks is far cheaper than executing them again.
    pub async fn backfill_headers(
        &mut self,
        node: &SyncClient,
        pox: &PoxInfo,
        from: [u8; 32],
    ) -> Result<usize, NodeExecutionError> {
        if self.chainstate.has_recorded_headers() {
            return Ok(0);
        }
        let mut walk = Vec::new();
        let mut cursor = self.tip.block_id();
        while *cursor.as_bytes() != from {
            let block = node.block(cursor).await?;
            cursor = block.header.parent_block_id;
            walk.push(block);
        }
        let recorded = walk.len();
        for block in walk.iter().rev() {
            let sortition = node.sortition(block.header.consensus_hash).await?;
            let mut bitcoin_context = pox.bitcoin_context();
            bitcoin_context.height = sortition.bitcoin_height;
            bitcoin_context.burn_header_hash = *sortition.bitcoin_block_hash.as_bytes();
            bitcoin_context.burn_block_time = sortition.bitcoin_timestamp;
            self.chainstate
                .backfill_block_header(block, bitcoin_context)
                .map_err(CheckpointExecutionError::from)?;
        }
        Ok(recorded)
    }

    /// Extend the staged descent toward this node's tip, then execute what it
    /// can, committing as it goes.
    ///
    /// Replaces a walk that buffered the whole gap in memory and executed only
    /// once it reached this node's tip. Against mainnet that gap was twenty
    /// thousand blocks and the walk never once completed: a single rate limit
    /// anywhere in it discarded every block fetched, and the next round began
    /// again from a tip that had since moved. Here the descent is on disk, so a
    /// round that ends early is progress kept, and execution runs in bounded
    /// chunks from whatever staging already holds.
    pub async fn catch_up(
        &mut self,
        node: &SyncClient,
        pox: &PoxInfo,
        staging: &Staging,
        budget: CatchUpBudget,
    ) -> Result<CatchUpRound, NodeExecutionError> {
        let mut round = CatchUpRound::default();
        let peer_tip = node.tenure_info().await?.tip_block_id;

        // What the peer added since the last round sits above everything
        // staged, so this stops as soon as it meets a block already held.
        let executed_tip = self.tip.block_id();
        let executed_height = self.tip.header.chain_length;
        // A descent that overshot leaves blocks below the executed tip, which
        // no round will ever execute and every round would otherwise resume
        // from.
        staging.remove_to(executed_height)?;
        let mut fetched = Self::descend(
            node,
            staging,
            peer_tip,
            Stop {
                block_id: executed_tip,
                height: executed_height,
            },
            budget.fetch,
            &mut round,
        )
        .await?;
        // The descent itself continues from the furthest it has reached, which
        // is what makes a rate-limited round cost nothing but time.
        if let Some(resume) = staging.descent_resumes_at()? {
            fetched += Self::descend(
                node,
                staging,
                resume,
                Stop {
                    block_id: executed_tip,
                    height: executed_height,
                },
                budget.fetch.saturating_sub(fetched),
                &mut round,
            )
            .await?;
        }
        round.fetched = fetched;
        round.executed = self.execute_staged(node, pox, staging, budget.execute).await?;
        round.staged = staging.len()?;
        Ok(round)
    }

    /// Walk back from `from`, staging each block, until this node's tip or a
    /// block already staged is reached, or the budget runs out.
    async fn descend(
        node: &SyncClient,
        staging: &Staging,
        from: StacksBlockId,
        until: Stop,
        budget: usize,
        round: &mut CatchUpRound,
    ) -> Result<usize, NodeExecutionError> {
        let mut cursor = from;
        let mut fetched = 0;
        while cursor != until.block_id && fetched < budget {
            if staging.holds(cursor)? {
                break;
            }
            // A whole tenure per request rather than a block per request: over
            // a gap of tens of thousands of blocks that is the difference
            // between catching up and being rate limited forever.
            let blocks = match node.blocks_of_tenure(cursor).await {
                Ok(blocks) => blocks,
                // A peer that is rate limiting has not failed, and neither has
                // the round: everything staged so far still stands.
                Err(error) if error.is_rate_limited() => {
                    round.rate_limited = true;
                    break;
                }
                // Naming the tenure makes the next undecodable block one curl
                // and one offline decode away, instead of a node restart.
                Err(error) => {
                    return Err(NodeExecutionError::Descent {
                        tenure: cursor,
                        error,
                    });
                }
            };
            let lowest = blocks
                .iter()
                .min_by_key(|block| block.header.chain_length)
                .ok_or(NodeExecutionError::Sync(SyncError::EmptyTenure))?;
            let next = lowest.header.parent_block_id;
            for block in &blocks {
                staging.put(block)?;
            }
            fetched += blocks.len();
            // A tenure arrives whole, so a batch straddling the executed tip
            // never lands on it exactly — the cursor would step over it and
            // descend forever into history this node already has.
            if lowest.header.chain_length <= until.height {
                break;
            }
            // A peer that answers with only the block asked for still moves the
            // descent along, because that block's parent is the next cursor.
            if next == cursor {
                break;
            }
            cursor = next;
        }
        Ok(fetched)
    }

    /// Execute staged blocks forward from this node's tip, up to `budget`.
    async fn execute_staged(
        &mut self,
        node: &SyncClient,
        pox: &PoxInfo,
        staging: &Staging,
        budget: usize,
    ) -> Result<usize, NodeExecutionError> {
        let mut executed = 0;
        while executed < budget {
            let Some(block) = staging.child_of(self.tip.block_id())? else {
                break;
            };
            let sortition = node.sortition(block.header.consensus_hash).await?;
            let mut bitcoin_context = pox.bitcoin_context();
            bitcoin_context.height = sortition.bitcoin_height;
            // Clarity reads this back through `get-burn-block-info?`, and sBTC
            // compares it against the hash a withdrawal was signed for. A
            // context that leaves it zero makes every such call fail.
            bitcoin_context.burn_header_hash = *sortition.bitcoin_block_hash.as_bytes();
            let bitcoin_context = node
                .tenure_coinbase_context(
                    &block,
                    self.chainstate.accounting_mut().schedule(),
                    bitcoin_context,
                )
                .await?;
            self.apply(&block, bitcoin_context)?;
            staging.remove(block.block_id())?;
            executed += 1;
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
