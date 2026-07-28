#![forbid(unsafe_code)]

use nano_sync::{FollowedTenure, SyncClient, SyncError, TenureFollower, TenureInfo};

/// A node's validated view of a remote Nakamoto tenure stream.
#[derive(Clone, Debug)]
pub struct Node {
    follower: TenureFollower,
}

impl Node {
    /// Construct a node that follows the supplied HTTP peer.
    #[must_use]
    pub const fn new(client: SyncClient) -> Self {
        Self {
            follower: TenureFollower::new(client),
        }
    }

    /// Return the latest validated peer tenure.
    #[must_use]
    pub const fn latest_tenure(&self) -> Option<&TenureInfo> {
        self.follower.latest()
    }

    /// Fetch and validate the peer's next tenure update.
    pub async fn poll(&mut self) -> Result<Option<FollowedTenure>, SyncError> {
        self.follower.poll().await
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
