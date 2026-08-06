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
    let Some((peer, client)) = pool.choose_source(None, None).await else {
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
        .choose_source(None, None)
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

/// Real inventories drive a real forward download over real peers.
///
/// The offline gate in `inventory_schedule` pins the *rule* — a schedule that executes
/// on the first round, only claiming peers asked, an answer for another burn view
/// refused — against a fixture peer. This is the same code against mainnet, and it
/// exists because two things about the schedule are only true if stacks-core really
/// behaves as its source says:
///
/// * `GET /v3/tenures/fork_info/:ch/:ch` — the same view twice — has to answer with
///   that one sortition. `get_tenures_fork_info` pushes the stop snapshot and then
///   walks parents *while the cursor is not the start*, so a request naming one view
///   twice never enters the walk; that is a reading of the source until a peer agrees.
/// * its `nakamoto_blocks` has to carry the whole tenure, since that is what makes a
///   tenure addressable by the burn view that elected it at no cost beyond the request
///   the schedule was going to make anyway.
///
/// One thing here is *not* what production does, and it matters: the burn view of each
/// scheduled offset is taken from a peer's `/v3/sortitions`. Production derives it from
/// its own `SortitionTracker`, because a cycle identifier taken from a peer would make
/// that peer's burnchain the thing nano's own requests are keyed on. What is under test
/// is the download, and this test has no derived sortition chain to key it from.
#[tokio::test]
async fn mainnet_inventories_schedule_a_forward_download() {
    /// Enough tenures to tell a schedule from a favourite, few enough to stay a test.
    const TENURES: usize = 6;
    /// How many of the newest claimed tenures to leave alone. See the comment below.
    const NEWEST_UNSERVED: usize = 4;
    if std::env::var_os("NANO_P2P_MAINNET").is_none() {
        println!("skipped: set NANO_P2P_MAINNET=1 to reach mainnet");
        return;
    }
    let mut swarm = Swarm::new(
        PeerDb::in_memory().expect("a peer table"),
        LocalPeer::quiet(StacksPrivateKey::from_seed(b"nano-p2p schedule"), 20444),
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

    let pool = PeerPool::from_endpoints(&endpoints);
    let (_, chosen) = pool
        .choose_source(None, None)
        .await
        .expect("a discovered peer serves a tip");
    let (cycle_start, length, naming) = name_the_cycle(&chosen).await;
    println!("cycle opens at burn {cycle_start}, named {naming}");

    let claims = swarm
        .tenure_claims(naming, &mut nano_p2p::Round::default())
        .await;
    let claiming = claims
        .iter()
        .filter(|claim| {
            claim.endpoint.is_some()
                && (0..claim.tenures.len()).any(|bit| claim.tenures.get(bit) == Some(true))
        })
        .count();
    println!("{} peers answered, {claiming} claiming tenures of it", claims.len());
    assert!(
        claiming >= 2,
        "only {claiming} peer(s) claimed any of the cycle, so no schedule can be spread"
    );

    // The tenures nano would want next: the recent end of the cycle, which is where a
    // node that has just caught up to it sits — but not the *newest* few, and that is a
    // property of the endpoint worth recording. `TenureForkingInfo::from_snapshot` reads
    // a tenure's blocks against the serving node's own Stacks tip, so a sortition whose
    // tenure that node has not processed yet answers `was_sortition: true` with no
    // blocks at all. Measured: asked for the six highest offsets every peer claimed,
    // three came back empty, and their burn heights were the top of the burnchain. A
    // catching-up node wants the older end and its follow path covers the tip, so this
    // costs nothing in production; it would have made this test flaky.
    let mut wanted: Vec<u16> = (0..u16::try_from(length).expect("a cycle fits in u16"))
        .filter(|bit| {
            claims
                .iter()
                .any(|claim| claim.tenures.get(*bit) == Some(true))
        })
        .collect();
    wanted.reverse();
    wanted.drain(..NEWEST_UNSERVED.min(wanted.len()));
    wanted.truncate(TENURES);
    let schedule = nano_p2p::assign_tenures(&claims, &wanted);
    assert_eq!(
        schedule.len(),
        wanted.len(),
        "the scheduler dropped tenures that peers claim"
    );

    let mut history = nano_sync::TenureSource::new(pool.into_clients());
    let (fetched, blocks) = fetch_the_schedule(&chosen, &mut history, cycle_start, &schedule).await;
    println!(
        "{fetched} of {} scheduled tenures fetched, {blocks} blocks, over {} peers",
        schedule.len(),
        history.served_by()
    );
    assert!(
        fetched >= 3,
        "only {fetched} scheduled tenures came back, so nothing was measured"
    );
    assert!(
        history.served_by() >= 2,
        "every scheduled tenure came from {} peer(s), so the schedule is not spread",
        history.served_by()
    );
}

/// The reward cycle a node following this peer would be walking, and its name.
///
/// The boundary comes from `payout_schedule`, which is the production rule and is
/// waterfall-aware: a cycle opens at offset 0 once the waterfall is on and at offset 1
/// before it, so a node that decided from where its tip happened to sit would move the
/// boundary part-way through a prepare phase and name a cycle no peer recognises.
///
/// The consensus hash naming it comes from the peer, which production would not do —
/// see the caller. What is under test here is the download.
async fn name_the_cycle(peer: &nano_sync::SyncClient) -> (u64, u64, nano_primitives::ConsensusHash) {
    let pox = peer.pox_info().await.expect("a peer states the calendar");
    let payouts = nano_node::payout_schedule(&pox).expect("a payout schedule");
    let length = u64::from(pox.prepare_phase_length) + u64::from(pox.reward_phase_length);
    let cycle_start = (0..=length)
        .filter_map(|back| pox.bitcoin_height.checked_sub(back))
        .find(|height| payouts.starts_reward_cycle(*height))
        .expect("the cycle the peer's burn tip sits in opens somewhere");
    let naming = peer
        .sortition_at_height(cycle_start)
        .await
        .expect("the cycle's first sortition")
        .consensus_hash;
    (cycle_start, length, naming)
}

/// Fetch every scheduled tenure from the peer the inventory named for it.
///
/// Returns how many came back and how many blocks they carried. A tenure that does not
/// is printed and skipped rather than failing: the wanted list is bit indices, and an
/// offset whose burn block elected nobody has no tenure for anybody to hold.
async fn fetch_the_schedule(
    naming: &nano_sync::SyncClient,
    history: &mut nano_sync::TenureSource,
    cycle_start: u64,
    schedule: &[(u16, String)],
) -> (usize, usize) {
    let mut fetched = 0;
    let mut blocks = 0;
    for (offset, endpoint) in schedule {
        let view = match naming
            .sortition_at_height(cycle_start + u64::from(*offset))
            .await
        {
            Ok(sortition) => sortition.consensus_hash,
            Err(error) => {
                println!("offset {offset} has no sortition to name: {error}");
                continue;
            }
        };
        match history.tenure_at(Some(endpoint), view).await {
            Ok(tenure) => {
                // The check that makes a fetch by burn view safe over strangers,
                // asserted here as well as inside the client: an answer is only this
                // tenure if every block's own header says so.
                assert!(
                    tenure
                        .iter()
                        .all(|block| block.header.consensus_hash == view),
                    "a mainnet peer answered the tenure of {view} with another view's blocks"
                );
                blocks += tenure.len();
                fetched += 1;
            }
            Err(error) => println!("offset {offset} at {view} came back empty: {error}"),
        }
    }
    (fetched, blocks)
}
