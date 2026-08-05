//! Handshake with a real Stacks peer.
//!
//! A wire format that passes a differential test against `stackslib`'s codec is
//! still only a claim about bytes. This is the claim about the *protocol*: that a
//! stock mainnet node accepts nano's handshake, signs a reply nano authenticates,
//! and then answers `Ping` and `GetNeighbors` on the same connection.
//!
//! It is off by default, because a test that needs the internet is a test that
//! fails for reasons that are nobody's fault. Turn it on with a peer to dial:
//!
//! ```text
//! NANO_P2P_PEER=seed.mainnet.hiro.so:20444 \
//!   cargo test --release -p nano-p2p --test live_peer -- --nocapture
//! ```
//!
//! ## The Bitcoin view, and why a fabricated one gets in
//!
//! Every message carries the sender's Bitcoin view, and a peer refuses one whose
//! *stable* header hash contradicts its own at that height. It only holds the
//! roughly 288 blocks below its stable height, though, so it cannot contradict a
//! view older than that — a claim about an ancient height is not checkable, and
//! stacks-core treats "not checkable" as "this peer is merely stale".
//!
//! That is what this test relies on when no real view is supplied, and it is
//! worth being explicit that it is a *test* affordance rather than how a node
//! should behave: a stale view puts nano in the bucket a peer will not walk
//! toward, so the production path has to advertise the view from nano's own
//! sortition database. Supply one here to check that path too:
//!
//! ```text
//! HEIGHT=$(curl -s https://mempool.space/api/blocks/tip/height)
//! NANO_P2P_PEER=seed.mainnet.hiro.so:20444 \
//! NANO_P2P_BITCOIN_HEIGHT=$HEIGHT \
//! NANO_P2P_BITCOIN_HASH=$(curl -s https://mempool.space/api/block-height/$HEIGHT) \
//! NANO_P2P_BITCOIN_STABLE_HASH=$(curl -s https://mempool.space/api/block-height/$((HEIGHT-7))) \
//!   cargo test --release -p nano-p2p --test live_peer -- --nocapture
//! ```

use std::time::Duration;

use nano_crypto::StacksPrivateKey;
use nano_p2p::wire::ChainView;
use nano_p2p::{LocalPeer, PeerDb, Protocol, Session};
use nano_primitives::BitcoinHeaderHash;

const TIMEOUT: Duration = Duration::from_secs(20);

/// The Bitcoin view to advertise: the real one if the environment names it, or an
/// unfalsifiable ancient one otherwise.
fn view() -> ChainView {
    let real = (
        std::env::var("NANO_P2P_BITCOIN_HEIGHT").ok(),
        std::env::var("NANO_P2P_BITCOIN_HASH").ok(),
        std::env::var("NANO_P2P_BITCOIN_STABLE_HASH").ok(),
    );
    if let (Some(height), Some(tip), Some(stable)) = real
        && let Ok(height) = height.trim().parse::<u64>()
        && let (Some(tip), Some(stable)) = (parse_hash(&tip), parse_hash(&stable))
    {
        eprintln!("advertising the real Bitcoin tip {height}");
        return ChainView::new(height, tip, stable).expect("a tip above the confirmation window");
    }
    eprintln!("advertising a stale Bitcoin view, which no peer can contradict");
    ChainView::new(
        100_000,
        BitcoinHeaderHash::from_bytes([0; 32]),
        BitcoinHeaderHash::from_bytes([0; 32]),
    )
    .expect("a tip above the confirmation window")
}

/// A Bitcoin block hash as an explorer prints it, which is the byte order
/// `BitcoinHeaderHash` uses.
fn parse_hash(text: &str) -> Option<BitcoinHeaderHash> {
    let text = text.trim();
    if text.len() != 64 {
        return None;
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(BitcoinHeaderHash::from_bytes(bytes))
}

#[tokio::test]
async fn a_real_peer_completes_the_handshake() {
    let Ok(peer) = std::env::var("NANO_P2P_PEER") else {
        eprintln!("skipped: set NANO_P2P_PEER=<host>:<port> to dial a real peer");
        return;
    };
    let address = tokio::net::lookup_host(&peer)
        .await
        .expect("resolve the peer")
        .next()
        .expect("the peer resolves to an address");
    // A fresh key each run: nano has no identity to defend here, and reusing one
    // across runs would make the peer's rate limiting part of the test.
    let local = LocalPeer::quiet(StacksPrivateKey::from_seed(&address.to_string().into_bytes()), 20444);
    let mut session = Session::open(address, &local, Protocol::mainnet(), view(), TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("{peer} refused the handshake: {error}"));

    let handshake = session.handshake().clone();
    eprintln!(
        "{peer} at {address}: key {}, services {:#06x}, heartbeat {}s, data url {:?}",
        session.public_key_hash(),
        handshake.services,
        session.heartbeat_interval(),
        handshake.data_url,
    );
    eprintln!(
        "  its Bitcoin view: tip {} ({}), stable {}",
        session.remote_view().height,
        session.remote_view().hash,
        session.remote_view().stable_height,
    );

    // The peer's tip has to be a real one. Bitcoin passed 800,000 in 2023, so
    // anything below that is a peer that is not following mainnet.
    assert!(
        session.remote_view().height > 800_000,
        "the peer's Bitcoin tip is {}",
        session.remote_view().height
    );

    // Liveness, on the connection the handshake authenticated. Every reply here is
    // verified against the key the handshake announced, so a `Pong` arriving at
    // all is proof the peer is both alive and the same peer.
    session.ping().await.expect("the peer answers a ping");

    let neighbors = session.neighbors().await.expect("the peer names its neighbors");
    eprintln!("  it knows {} neighbors", neighbors.len());

    // The peer table is the point of asking: a node that has run before should
    // start from what it learned rather than from its seed list.
    let peers = PeerDb::in_memory().expect("a peer table");
    peers
        .record_handshake(
            nano_p2p::PeerAddress::from_ip(address.ip()),
            address.port(),
            &handshake,
            Protocol::mainnet().peer_version,
            Protocol::mainnet().network_id,
        )
        .expect("record the handshake");
    let learned = peers.learn(&neighbors).expect("record the neighbors");
    assert_eq!(peers.count().expect("count"), learned + 1);
    eprintln!("  learned {learned} new addresses from it");

    // Nothing above went through a payload nano cannot model, which is the point
    // of counting them: a peer that only sends epoch-2.x messages is not one this
    // node can sync from.
    eprintln!(
        "  {} unmodelled messages, {} unsolicited",
        session.unhandled_messages(),
        session.take_pushed().len(),
    );
}
