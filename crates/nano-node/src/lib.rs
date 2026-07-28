#![forbid(unsafe_code)]

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
