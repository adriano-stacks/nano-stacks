//! The queue between the peers that push data and the loop that can check it.
//!
//! Everything else in this crate deliberately has no opinion about whether what a
//! peer said is true, and a pushed block is the least-verified thing a node ever
//! sees. So relay cannot be done *here*: the checks live where the chainstate is,
//! and doing it from the discovery loop would be the one place in the crate that
//! trusted a peer.
//!
//! What this module is, then, is a hand-off with a bound on it. Peers offer blocks
//! and transactions in; the loop that owns a chainstate takes them, puts each one
//! through `ChainState::authenticate_block` or the mempool's own admission, and
//! announces back out only what passed. Two queues, one direction each, and no
//! judgement in between.
//!
//! ## What nano says about a message it forwards
//!
//! A relayed message is re-encoded and re-signed rather than forwarded verbatim,
//! because the relayer list is part of the frame the signature covers. Nano appends
//! **itself and nothing else**: the upstream list is a stranger's claim about which
//! other nodes have seen this item, nano has no way to check any of it, and passing
//! it on signed by nano would be republishing unverified claims as if they were
//! ours. What the list is actually load-bearing for is loop prevention, and for that
//! the only entry that matters is the sender's.
//!
//! [`relayed_by`] is the other half of that: an item whose relayer list already
//! names this node has been round the loop, and is dropped rather than checked
//! again.
//!
//! ## Why a rejected push is not a penalty
//!
//! Nothing here or in `nano-node` scores a peer for pushing a block that fails
//! authentication. A block can fail because *this* node cannot yet derive the reward
//! set, or has not executed the tenure it builds on, and isolating a peer for that
//! would repeat the bug the third slice fixed — nano isolating the peers that were
//! working hardest. A peer that pushes garbage costs one authentication, which is
//! cheap and bounded by the queue below.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use nano_chainstate::NakamotoBlock;
use nano_codec::Transaction;
use nano_primitives::Hash160;

use crate::wire::Message;

/// How many offers wait for the loop that checks them, in each direction.
///
/// The peers decide how much they push and this node decides how often it looks, so
/// the bound has to be nobody's choice but ours. Beyond it the oldest are dropped
/// and counted: a block or transaction nano missed will be pushed again by another
/// peer, while a queue that grows with the network's output is memory the network
/// gets to choose.
pub const MAX_QUEUED_OFFERS: usize = 1024;

/// How many accepted items to remember, so each is relayed once.
const MAX_REMEMBERED: usize = 4096;

/// One block or transaction, going in or out.
#[derive(Clone, Debug)]
pub enum Pushed {
    /// Boxed because a block is by far the larger of the two, and every
    /// transaction-shaped offer would otherwise be padded to a block's size.
    Block(Box<NakamotoBlock>),
    Transaction(Box<Transaction>),
}

impl Pushed {
    /// What identifies this item, so that nano relays it once however many peers
    /// push it.
    ///
    /// A block identifier and a txid are both 32 bytes and both derived from the
    /// item's own consensus bytes, so neither can collide with the other by accident
    /// and neither is something a peer chooses freely.
    #[must_use]
    pub fn id(&self) -> [u8; 32] {
        match self {
            Self::Block(block) => *block.block_id().as_bytes(),
            Self::Transaction(transaction) => *transaction.txid().as_bytes(),
        }
    }

    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Block(_) => "block",
            Self::Transaction(_) => "transaction",
        }
    }
}

/// An item and the peer it came from.
#[derive(Clone, Debug)]
pub struct Offer {
    /// The peer that pushed it, or `None` for something this node originated.
    ///
    /// Kept for exactly one reason: not sending an item back to the peer that sent
    /// it. It is *not* a reason to treat the item differently — a block from a peer
    /// nano likes goes through the same checks as one from a peer it does not.
    pub from: Option<Hash160>,
    pub data: Pushed,
}

impl Offer {
    #[must_use]
    pub fn block(from: Option<Hash160>, block: NakamotoBlock) -> Self {
        Self {
            from,
            data: Pushed::Block(Box::new(block)),
        }
    }

    #[must_use]
    pub fn transaction(from: Option<Hash160>, transaction: Box<Transaction>) -> Self {
        Self {
            from,
            data: Pushed::Transaction(transaction),
        }
    }
}

/// The hand-off itself: cheap to clone, safe to share, and bounded.
///
/// A `std` mutex rather than a `tokio` one because nothing holds it across an await:
/// the peer-facing loops put things in and take things out, and the loop with the
/// chainstate does the slow part outside the lock.
#[derive(Clone, Debug, Default)]
pub struct Relay {
    inner: Arc<Mutex<Queues>>,
}

#[derive(Debug, Default)]
struct Queues {
    /// What peers pushed, waiting to be checked.
    offered: VecDeque<Offer>,
    /// What passed the checks, waiting to go out.
    announcing: VecDeque<Offer>,
    /// Items already accepted and relayed, so each goes out once and a second peer
    /// pushing the same block does not cost a second authentication.
    accepted: HashSet<[u8; 32]>,
    /// Insertion order for `accepted`, so it can be bounded without a full LRU.
    remembered: VecDeque<[u8; 32]>,
    dropped: u64,
}

impl Relay {
    /// Offer something a peer pushed, and say whether it is new to this node.
    ///
    /// `false` means it has already been accepted and relayed, in which case there
    /// is nothing to check and nothing to forward.
    pub fn offer(&self, offer: Offer) -> bool {
        self.with(|queues| {
            if queues.accepted.contains(&offer.data.id()) {
                return false;
            }
            if queues.offered.len() >= MAX_QUEUED_OFFERS {
                queues.offered.pop_front();
                queues.dropped = queues.dropped.saturating_add(1);
            }
            queues.offered.push_back(offer);
            true
        })
        .unwrap_or(false)
    }

    /// Take everything waiting to be checked.
    #[must_use]
    pub fn take_offered(&self) -> Vec<Offer> {
        self.with(|queues| queues.offered.drain(..).collect())
            .unwrap_or_default()
    }

    /// Put something this node has accepted on its way out, and say whether it is
    /// the first time.
    ///
    /// `false` means nano has already relayed it, which is what keeps a block pushed
    /// by eight peers from being pushed back eight times.
    pub fn announce(&self, offer: Offer) -> bool {
        self.with(|queues| {
            let id = offer.data.id();
            if !queues.accepted.insert(id) {
                return false;
            }
            queues.remembered.push_back(id);
            while queues.remembered.len() > MAX_REMEMBERED {
                if let Some(oldest) = queues.remembered.pop_front() {
                    queues.accepted.remove(&oldest);
                }
            }
            if queues.announcing.len() >= MAX_QUEUED_OFFERS {
                queues.announcing.pop_front();
                queues.dropped = queues.dropped.saturating_add(1);
            }
            queues.announcing.push_back(offer);
            true
        })
        .unwrap_or(false)
    }

    /// Take everything waiting to go out.
    #[must_use]
    pub fn take_announcing(&self) -> Vec<Offer> {
        self.with(|queues| queues.announcing.drain(..).collect())
            .unwrap_or_default()
    }

    /// How many offers were shed because a queue was full. Non-zero means this node
    /// is dropping relayed data, not that any peer did anything wrong.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.with(|queues| queues.dropped).unwrap_or(0)
    }

    /// A poisoned lock means a panic while queueing a block, which is not a reason
    /// to bring the node down: dropping the item is the same outcome as the bound
    /// above, and the node carries on relaying.
    fn with<T>(&self, act: impl FnOnce(&mut Queues) -> T) -> Option<T> {
        self.inner.lock().ok().map(|mut queues| act(&mut queues))
    }
}

/// Whether a message has already been round the loop through this node.
///
/// Nano puts itself in the relayer list of everything it forwards and copies nothing
/// from upstream, so its own key hash appearing there means the item is one nano
/// already relayed. Checking it costs a comparison and saves re-authenticating what
/// this node published.
#[must_use]
pub fn relayed_by(message: &Message, us: Hash160) -> bool {
    message
        .relayers
        .iter()
        .any(|relayer| relayer.peer.public_key_hash == us)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nano_codec::{AnchorMode, TransactionPayloadData, TransactionVersion};
    use nano_primitives::Network;

    /// A distinct signed transaction per nonce, which is all these tests need of
    /// one: the identifier is the txid, and the txid covers the nonce.
    fn transaction(nonce: u64) -> Box<Transaction> {
        Box::new(
            Transaction::sign_standard(
                TransactionVersion::Testnet,
                Network::TESTNET.chain_id(),
                AnchorMode::OnChainOnly,
                &nano_crypto::StacksPrivateKey::from_seed(b"relay test"),
                nonce,
                1,
                TransactionPayloadData::SmartContract {
                    contract_name: "relayed".to_owned(),
                    source: "(define-read-only (f) true)".to_owned(),
                },
            )
            .expect("sign a deployment"),
        )
    }

    /// The hand-off carries an item one way and the verdict the other.
    #[test]
    fn an_offer_goes_in_and_an_acceptance_comes_out() {
        let relay = Relay::default();
        let from = Some(Hash160::from_bytes([7; 20]));
        assert!(relay.offer(Offer::transaction(from, transaction(0))));
        let taken = relay.take_offered();
        assert_eq!(taken.len(), 1);
        assert!(relay.take_offered().is_empty(), "taking drains");
        assert_eq!(taken[0].from, from);
        assert_eq!(taken[0].data.name(), "transaction");

        assert!(relay.announce(taken[0].clone()));
        assert_eq!(relay.take_announcing().len(), 1);
    }

    /// A block eight peers push is authenticated once and relayed once.
    #[test]
    fn an_accepted_item_is_neither_re_offered_nor_re_announced() {
        let relay = Relay::default();
        let offer = Offer::transaction(None, transaction(1));
        assert!(relay.offer(offer.clone()));
        drop(relay.take_offered());
        assert!(relay.announce(offer.clone()), "the first acceptance goes out");
        assert!(!relay.announce(offer.clone()), "the second does not");
        assert!(
            !relay.offer(offer),
            "and a peer pushing it again costs no second check"
        );
        assert!(relay.take_offered().is_empty());
    }

    /// Two different transactions are two items, so the identifier is doing work.
    #[test]
    fn different_items_are_told_apart() {
        let relay = Relay::default();
        assert!(relay.announce(Offer::transaction(None, transaction(2))));
        assert!(relay.announce(Offer::transaction(None, transaction(3))));
        assert_eq!(relay.take_announcing().len(), 2);
    }

    /// The queue is bounded by this node's choice, not by the network's output.
    #[test]
    fn a_full_queue_sheds_the_oldest_and_counts_it() {
        let relay = Relay::default();
        for nonce in 0..u64::try_from(MAX_QUEUED_OFFERS + 8).expect("small") {
            relay.offer(Offer::transaction(None, transaction(nonce)));
        }
        assert_eq!(relay.take_offered().len(), MAX_QUEUED_OFFERS);
        assert_eq!(relay.dropped(), 8);
    }

    /// The set of accepted items is bounded too, and forgetting is not a fault: the
    /// worst it costs is relaying one item a second time.
    #[test]
    fn the_memory_of_accepted_items_is_bounded() {
        let relay = Relay::default();
        let first = Offer::transaction(None, transaction(0));
        for nonce in 0..u64::try_from(MAX_REMEMBERED + 1).expect("small") {
            relay.announce(Offer::transaction(None, transaction(nonce)));
        }
        drop(relay.take_announcing());
        assert!(
            relay.announce(first),
            "the oldest acceptance has been forgotten"
        );
    }
}
