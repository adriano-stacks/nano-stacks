#![forbid(unsafe_code)]

use std::{fmt, path::Path};

use nano_bitcoin::BitcoinSource;
use nano_chainstate::{
    AppliedBlock, BitcoinBlockContext, ChainState, ChainStateError, NakamotoBlock, TenureAccounting,
};
use nano_primitives::TrieHash;
use nano_sync::{
    FollowedTenure, NodeInfo, PoxInfo, SyncClient, SyncError, TenureFollower, TenureInfo,
};

/// A node's validated view of a remote Nakamoto tenure stream.
#[derive(Clone, Debug)]
pub struct Node {
    client: SyncClient,
    follower: TenureFollower,
    peer_info: Option<NodeInfo>,
    pox_info: Option<PoxInfo>,
}

/// A consistent read-only snapshot that can be served by the public RPC.
#[derive(Clone, Debug)]
pub struct NodeView {
    pub node_info: NodeInfo,
    pub pox_info: PoxInfo,
    pub tenures: Vec<FollowedTenure>,
}

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

impl<S> CheckpointExecutor<S>
where
    S: BitcoinSource,
    S::Error: fmt::Display,
{
    /// Import a checkpoint and apply its first known descendant as the execution anchor.
    pub fn from_checkpoint(
        path: impl AsRef<Path>,
        source: [u8; 32],
        state_root: TrieHash,
        anchor: NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
        bitcoin: S,
    ) -> Result<Self, CheckpointExecutionError> {
        Self::from_checkpoint_with_accounting(
            path,
            source,
            state_root,
            anchor,
            bitcoin_context,
            bitcoin,
            None,
        )
    }

    /// Import a checkpoint together with the matured native rewards it owes.
    pub fn from_checkpoint_with_accounting(
        path: impl AsRef<Path>,
        source: [u8; 32],
        state_root: TrieHash,
        anchor: NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
        mut bitcoin: S,
        accounting: Option<TenureAccounting>,
    ) -> Result<Self, CheckpointExecutionError> {
        let mut chainstate = ChainState::from_checkpoint(path, source, state_root)?;
        if let Some(accounting) = accounting {
            *chainstate.accounting_mut() = accounting;
        }
        let operations = bitcoin
            .block_at(bitcoin_context.height)
            .map_err(|error| CheckpointExecutionError::Bitcoin(error.to_string()))?;
        chainstate.append_nakamoto_block_with_bitcoin_operations(
            bitcoin_context,
            &operations.operations,
            Some(source),
            &anchor,
        )?;
        Ok(Self {
            chainstate,
            tip: anchor,
            bitcoin,
        })
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
        block
            .validate_successor(&self.tip.header)
            .map_err(|error| CheckpointExecutionError::Link(error.to_string()))?;
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

impl Node {
    /// Construct a node that follows the supplied HTTP peer.
    #[must_use]
    pub fn new(client: SyncClient) -> Self {
        Self {
            follower: TenureFollower::new(client.clone()),
            client,
            peer_info: None,
            pox_info: None,
        }
    }

    /// Return the latest validated peer tenure.
    #[must_use]
    pub const fn latest_tenure(&self) -> Option<&TenureInfo> {
        self.follower.latest()
    }

    /// Fetch and validate the peer's next tenure update.
    pub async fn poll(&mut self) -> Result<Option<FollowedTenure>, SyncError> {
        let followed = self.follower.poll().await?;
        self.peer_info = Some(self.client.node_info().await?);
        self.pox_info = Some(self.client.pox_info().await?);
        Ok(followed)
    }

    /// Return the latest complete local view after at least one successful poll.
    #[must_use]
    pub fn view(&self) -> Option<NodeView> {
        Some(NodeView {
            node_info: self.peer_info.clone()?,
            pox_info: self.pox_info.clone()?,
            tenures: self.follower.history().to_vec(),
        })
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

#[cfg(test)]
mod tests {
    use nano_sync::SyncClient;
    use reqwest::Url;

    use super::Node;

    #[test]
    fn node_starts_without_a_followed_tenure() {
        let client = SyncClient::new(Url::parse("http://127.0.0.1:20443/").expect("valid URL"))
            .expect("create sync client");
        let node = Node::new(client);

        assert!(node.latest_tenure().is_none());
    }
}
