#![forbid(unsafe_code)]

use std::{fmt, path::Path};

use nano_chainstate::{
    AppliedBlock, BitcoinBlockContext, ChainState, ChainStateError, NakamotoBlock,
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
pub struct CheckpointExecutor {
    chainstate: ChainState,
    tip: NakamotoBlock,
}

/// A follower that executes each accepted tenure update from a checkpointed state.
#[derive(Debug)]
pub struct ExecutingNode {
    node: Node,
    executor: CheckpointExecutor,
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
    Link(String),
}

impl fmt::Display for CheckpointExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChainState(error) => write!(formatter, "checkpoint execution failed: {error}"),
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
            Self::Link(_) => None,
        }
    }
}

impl From<ChainStateError> for CheckpointExecutionError {
    fn from(error: ChainStateError) -> Self {
        Self::ChainState(error)
    }
}

impl CheckpointExecutor {
    /// Import a checkpoint and apply its first known descendant as the execution anchor.
    pub fn from_checkpoint(
        path: impl AsRef<Path>,
        source: [u8; 32],
        state_root: TrieHash,
        anchor: NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
    ) -> Result<Self, CheckpointExecutionError> {
        let mut chainstate = ChainState::from_checkpoint(path, source, state_root)?;
        chainstate.append_nakamoto_block_with_bitcoin_context(
            bitcoin_context,
            Some(source),
            &anchor,
        )?;
        Ok(Self {
            chainstate,
            tip: anchor,
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
        for block in blocks {
            applied.push(self.apply(block, bitcoin_context)?);
        }
        Ok(applied)
    }

    /// Validate and execute one direct descendant of the current execution tip.
    pub fn apply(
        &mut self,
        block: &NakamotoBlock,
        bitcoin_context: BitcoinBlockContext,
    ) -> Result<AppliedBlock, CheckpointExecutionError> {
        block
            .validate_successor(&self.tip.header)
            .map_err(|error| CheckpointExecutionError::Link(error.to_string()))?;
        let applied = self.chainstate.append_nakamoto_block_with_bitcoin_context(
            bitcoin_context,
            Some(*self.tip.block_id().as_bytes()),
            block,
        )?;
        self.tip = block.clone();
        Ok(applied)
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

impl ExecutingNode {
    /// Couple a peer follower to a checkpoint executor.
    #[must_use]
    pub const fn new(node: Node, executor: CheckpointExecutor) -> Self {
        Self { node, executor }
    }

    /// Follow, validate, and execute the peer's next tenure update.
    pub async fn poll(&mut self) -> Result<Option<FollowedTenure>, NodeExecutionError> {
        let followed = self.node.poll().await?;
        if let Some(tenure) = &followed {
            let view = self.node.view().ok_or(NodeExecutionError::MissingView)?;
            self.executor
                .apply_followed_tenure(tenure, &view.pox_info)?;
        }
        Ok(followed)
    }

    /// Return the executed chain tip.
    #[must_use]
    pub const fn executed_tip(&self) -> &NakamotoBlock {
        self.executor.tip()
    }

    /// Return the current validated node view.
    #[must_use]
    pub fn view(&self) -> Option<NodeView> {
        self.node.view()
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
