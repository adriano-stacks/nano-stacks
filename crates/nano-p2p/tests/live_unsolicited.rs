//! What a real mainnet peer sends when nobody asked it anything.
//!
//! This test exists because the running mainnet node reported `4 isolated` out of
//! seven peers and hundreds of "unsolicited messages dropped", and neither number
//! could be explained from the code alone: a session used to isolate a peer that
//! interleaved more than thirty-two messages before a reply, and whether that is
//! misbehaviour or a handler gap depends entirely on *what* those messages are.
//!
//! So this counts them, by payload identifier — which is the part that matters,
//! because every unmodelled message shares the name `Unhandled` and identifier 25
//! (`StackerDBPushChunk`, an announcement) is a very different thing from identifier
//! 5 (`GetBlocksInv`, a request). Off by default, because it needs the internet:
//!
//! ```text
//! NANO_P2P_PEER=seed.mainnet.hiro.so:20444 NANO_P2P_LISTEN_SECS=120 \
//!   cargo test --release -p nano-p2p --test live_unsolicited -- --nocapture
//! ```
//!
//! The answer, recorded in `tasks/054-join-and-synchronize-over-the-stacks-p2p-network.md`:
//! across three mainnet seeds, *every* unprompted message was an announcement —
//! `StackerDBPushChunk` (signer traffic, the overwhelming majority), `Transaction`
//! and `NakamotoBlocks` — arriving at between 0.2 and 0.8 a second. So the peers
//! were behaving perfectly and nano was isolating them for it, worst offender first,
//! because the swarm reads a session once every fifty seconds and a fixed count is
//! crossed by any rate given enough time.
//!
//! The test now asserts that too: a session that has read a mainnet peer for a
//! minute must still be usable, which is what fails if volume is ever a fault again.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use nano_crypto::StacksPrivateKey;
use nano_p2p::wire::ChainView;
use nano_p2p::{LocalPeer, Protocol, Session};
use nano_primitives::BitcoinHeaderHash;

const TIMEOUT: Duration = Duration::from_secs(20);

/// A view no peer can contradict, which is enough to get in without a burnchain.
fn stale_view() -> ChainView {
    ChainView::new(
        100_000,
        BitcoinHeaderHash::from_bytes([0; 32]),
        BitcoinHeaderHash::from_bytes([0; 32]),
    )
    .expect("a tip above the confirmation window")
}

#[tokio::test]
async fn count_what_a_mainnet_peer_sends_unprompted() {
    let Ok(peer) = std::env::var("NANO_P2P_PEER") else {
        eprintln!("skipped: set NANO_P2P_PEER=<host>:<port> to dial a real peer");
        return;
    };
    let listen = std::env::var("NANO_P2P_LISTEN_SECS")
        .ok()
        .and_then(|secs| secs.parse::<u64>().ok())
        .unwrap_or(60);
    let address = tokio::net::lookup_host(&peer)
        .await
        .expect("resolve the peer")
        .next()
        .expect("the peer resolves to an address");
    let local = LocalPeer::quiet(
        StacksPrivateKey::from_seed(b"nano-p2p unsolicited census"),
        20444,
    );
    let mut session = Session::open(address, &local, Protocol::mainnet(), stale_view(), TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("{peer} refused the handshake: {error}"));
    eprintln!("{peer}: handshook, listening for {listen}s");

    // Sit silent for the interval the production swarm uses before saying anything.
    // The gap is the point: it is what turns an ordinary relay rate into a burst of
    // queued messages, and a session that cannot survive the gap cannot survive
    // mainnet.
    let deadline = Instant::now() + Duration::from_secs(listen);
    let mut census: BTreeMap<String, usize> = BTreeMap::new();
    let mut failure = None;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(50).min(deadline - Instant::now())).await;
        // The ping first, then the collect the swarm does after it, so anything that
        // arrived during the round trip is not left for the next round.
        if let Err(error) = session.ping().await {
            failure = Some(error);
            break;
        }
        if let Err(error) = session.collect().await {
            failure = Some(error);
            break;
        }
        for message in session.take_pushed() {
            // The identifier matters as much as the name: every unmodelled message
            // shares the name `Unhandled`, and the difference between identifier 25
            // (`StackerDBPushChunk`, an announcement) and identifier 5
            // (`GetBlocksInv`, a *request*) is the difference between something safe
            // to drop and something a peer is waiting on.
            let key = format!("{} (id {})", message.payload.name(), message.payload.id());
            *census.entry(key).or_default() += 1;
        }
    }
    let total: usize = census.values().sum();
    eprintln!("  {total} unsolicited messages in {listen}s:");
    for (name, count) in &census {
        eprintln!("    {count:>5}  {name}");
    }
    assert!(
        failure.is_none(),
        "a peer relaying at mainnet's ordinary rate broke the session: {}",
        failure.expect("checked just above"),
    );
    assert_eq!(
        session.dropped_pushes(),
        0,
        "the push buffer overflowed inside one round"
    );
    // And the session is still good for work, which is the whole claim: reading a
    // minute of a peer's relay traffic must leave a peer nano can still ask things of.
    session
        .neighbors()
        .await
        .expect("the peer still answers after a round of its own relay traffic");
    assert!(total > 0, "a mainnet peer sends something unprompted");
}
