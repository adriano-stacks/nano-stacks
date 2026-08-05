//! Reaching mainnet with no hosted API configured.
//!
//! This is task 054's acceptance criterion taken literally: *neither catch-up nor
//! steady-state operation may require a hosted Stacks API*. What makes that true is
//! not a different protocol — stacks-core fetches Nakamoto blocks over HTTP too —
//! but where the endpoints come from. Nano is given four p2p bootstrap addresses,
//! walks the network from them, and ends up with a list of ordinary nodes' own RPC
//! endpoints, none of them a service whose rate limit is anybody's liveness.
//!
//! Off by default, because it needs the internet:
//!
//! ```text
//! NANO_P2P_MAINNET=1 cargo test --release -p nano-conformance --test conformance \
//!   p2p_discovery -- --nocapture
//! ```

use std::time::Duration;

use nano_crypto::StacksPrivateKey;
use nano_p2p::wire::ChainView;
use nano_p2p::{LocalPeer, MAINNET_SEEDS, PeerDb, Protocol, Swarm, SwarmLimits};
use nano_primitives::BitcoinHeaderHash;
use nano_sync::PeerPool;

/// What a node with no chain yet advertises.
///
/// A peer refuses a message whose *stable* header hash contradicts its own, and it
/// only keeps about 288 blocks below its stable height — so a view this old cannot
/// be contradicted, and stacks-core reads not-contradictable as merely stale. That
/// is exactly the position a node starting from a checkpoint is in before it has
/// executed anything, so it is the view worth testing with.
fn fresh_node_view() -> ChainView {
    ChainView::new(
        100_000,
        BitcoinHeaderHash::from_bytes([0; 32]),
        BitcoinHeaderHash::from_bytes([0; 32]),
    )
    .expect("a height above the confirmation window")
}

/// What a list of places to fetch from has to be, beyond non-empty.
fn check_endpoints(endpoints: &[String]) {
    println!("{} endpoints to fetch from:", endpoints.len());
    for endpoint in endpoints {
        println!("  {endpoint}");
    }
    assert!(
        !endpoints.is_empty(),
        "no connected peer advertised an HTTP endpoint, so there is nowhere to fetch"
    );
    for endpoint in endpoints {
        // Asserted rather than eyeballed, because the whole criterion is about *what*
        // nano depends on, and a configuration that quietly reintroduced a hosted API
        // would still pass every other check here.
        assert!(
            !endpoint.contains("hiro.so"),
            "a discovered endpoint is a hosted API: {endpoint}"
        );
        // Mainnet advertises `http://10.0.1.37:20443` — a load-balanced node naming
        // the address it sees itself at. Fetching from that would be dialling this
        // machine's own network.
        assert!(
            !endpoint.contains("//10.")
                && !endpoint.contains("//127.")
                && !endpoint.contains("//192.168."),
            "a discovered endpoint is on somebody else's private network: {endpoint}"
        );
    }
    let mut distinct = endpoints.to_vec();
    distinct.sort();
    distinct.dedup();
    assert_eq!(
        distinct.len(),
        endpoints.len(),
        "the same endpoint is listed twice, so a count of peers is not a count of \
         places to fetch from: {endpoints:?}"
    );
}

#[tokio::test]
async fn nano_finds_mainnet_peers_to_fetch_from_without_a_hosted_api() {
    if std::env::var_os("NANO_P2P_MAINNET").is_none() {
        nano_conformance::skip_gate("NANO_P2P_MAINNET must be set to dial mainnet");
        return;
    }

    let mut swarm = Swarm::new(
        PeerDb::in_memory().expect("a peer table"),
        // A fresh identity, so the peers' own backoff tables are not part of the
        // test, and no services: this node serves nothing yet, and saying otherwise
        // would have peers spend connection slots on it.
        LocalPeer::quiet(StacksPrivateKey::from_seed(b"nano-conformance discovery"), 20444),
        Protocol::mainnet(),
        SwarmLimits {
            outbound: 8,
            dials_per_round: 8,
            timeout: Duration::from_secs(20),
        },
    );
    for seed in MAINNET_SEEDS {
        swarm.seed(seed).await.expect("record the bootstrap peer");
    }
    let discovered = swarm.discovered();

    // Two rounds: the first reaches the bootstrap peers and learns addresses from
    // one of them, the second dials some of what it learned. A node that could only
    // ever talk to its seed list would still have four services as a dependency.
    let mut learned_beyond_the_seeds = false;
    for round in 1..=2 {
        // No cycle to ask inventories about: a fresh table has no locally derived
        // sortitions, so there is no consensus hash this node could name a cycle by
        // without quoting a peer for it.
        let outcome = swarm.maintain(fresh_node_view(), None).await;
        println!(
            "round {round}: {} connected, {} dialled, {} isolated, {} addresses learned, \
             {} known, {} unprompted messages",
            outcome.connected,
            outcome.dialled,
            outcome.isolated,
            outcome.learned,
            discovered.known(),
            outcome.collected,
        );
        learned_beyond_the_seeds |= outcome.learned > 0;
    }

    assert!(
        discovered.connected() >= 2,
        "only {} peers connected, so no single one of them is optional",
        discovered.connected()
    );
    assert!(
        learned_beyond_the_seeds,
        "the peer table never grew past its seed list"
    );
    assert!(
        discovered.known() > MAINNET_SEEDS.len(),
        "the peer table holds only its {} seeds",
        MAINNET_SEEDS.len()
    );

    let endpoints = discovered.endpoints();
    check_endpoints(&endpoints);

    // And they are peers that can actually be caught up from: a tip, weighed by the
    // same `PeerPool` the production follow loop weighs, over headers this node
    // fetched itself.
    let pool = PeerPool::from_endpoints(&endpoints);
    assert_eq!(pool.len(), endpoints.len());
    let Some((peer, client)) = pool.choose_source(None).await else {
        panic!(
            "none of the {} discovered endpoints served a tip; they were {endpoints:?}",
            endpoints.len()
        );
    };
    let info = client
        .node_info()
        .await
        .expect("the chosen peer answers /v2/info");
    println!(
        "chose peer {peer} at {}: stacks height {}, bitcoin height {}",
        client.base_url(),
        info.stacks_height,
        info.bitcoin_height,
    );
    assert_eq!(
        info.network_id,
        Protocol::mainnet().network_id,
        "a discovered peer is not on mainnet"
    );
    // Mainnet passed Bitcoin height 800,000 in 2023; anything below it is not a
    // node following mainnet, whatever it says about itself.
    assert!(info.bitcoin_height > 800_000);
    assert!(info.stacks_height > 0);

    // How many of them could be caught up from, not just the chosen one: the
    // criterion is that removing one changes nothing.
    let mut answered = 0;
    for (_, candidate) in pool.trusted() {
        if candidate.node_info().await.is_ok() {
            answered += 1;
        }
    }
    println!("{answered} of {} discovered peers answer HTTP", pool.len());
    assert!(
        answered >= 2,
        "only {answered} discovered peers answer, so one of them is load bearing"
    );
}

/// Bulk history really is fetched from many peers, and not from one.
///
/// The other half of the same criterion, and the half that was still a claim about
/// the transport rather than about catching up: `catch_up`'s descent walks a tenure
/// at a time, and from the mainnet checkpoint that is tens of thousands of blocks. As
/// long as they all went down one connection, one service's rate limit *was* nano's
/// catch-up speed, whatever the peer table held.
///
/// So this walks a real descent — the tip's tenure, then its parent's, and so on —
/// through `TenureSource` over the peers discovery found, and asserts that the
/// requests actually landed on several of them.
#[tokio::test]
async fn bulk_history_comes_from_several_mainnet_peers() {
    // Enough tenures to tell a pool from a favourite, and few enough that this stays a
    // test rather than a load generator against strangers' nodes.
    const TENURES: usize = 10;
    if std::env::var_os("NANO_P2P_MAINNET").is_none() {
        println!("skipped: set NANO_P2P_MAINNET=1 to reach mainnet");
        return;
    }
    let mut swarm = Swarm::new(
        PeerDb::in_memory().expect("a peer table"),
        LocalPeer::quiet(StacksPrivateKey::from_seed(b"nano-p2p bulk history"), 20444),
        Protocol::mainnet(),
        SwarmLimits {
            outbound: 8,
            dials_per_round: 8,
            timeout: Duration::from_secs(20),
        },
    );
    for seed in MAINNET_SEEDS {
        swarm.seed(seed).await.expect("record the bootstrap peer");
    }
    let discovered = swarm.discovered();
    swarm.maintain(fresh_node_view(), None).await;
    let endpoints = discovered.endpoints();
    check_endpoints(&endpoints);

    // Where to start: whichever peer the same fork choice the production loop uses
    // picks, and its tip. Nothing below trusts that peer for anything — every tenure
    // is fetched from whoever `TenureSource` picks next.
    let pool = PeerPool::from_endpoints(&endpoints);
    let (_, chosen) = pool
        .choose_source(None)
        .await
        .expect("a discovered peer serves a tip");
    let mut cursor = chosen
        .tenure_info()
        .await
        .expect("the chosen peer names its tip")
        .tip_block_id;

    let mut history = nano_sync::TenureSource::new(pool.into_clients());
    let mut fetched = 0;
    let mut blocks = 0;
    for _ in 0..TENURES {
        let tenure = match history.blocks_of_tenure(cursor).await {
            Ok(tenure) => tenure,
            Err(error) => {
                println!("the descent stopped after {fetched} tenures: {error}");
                break;
            }
        };
        let lowest = tenure
            .iter()
            .min_by_key(|block| block.header.chain_length)
            .expect("a tenure has blocks");
        let next = lowest.header.parent_block_id;
        blocks += tenure.len();
        fetched += 1;
        if next == cursor {
            break;
        }
        cursor = next;
    }
    println!("{fetched} tenures, {blocks} blocks, over {} peers", history.len());
    assert!(
        fetched >= 3,
        "only {fetched} tenures came back, so nothing was measured"
    );
    // The point of the whole thing: a descent that used one peer would report one.
    assert!(
        history.served_by() >= 2,
        "every tenure came from {} peer(s), so the descent is still single-peer",
        history.served_by()
    );
    println!("{} distinct peers served history", history.served_by());
}
