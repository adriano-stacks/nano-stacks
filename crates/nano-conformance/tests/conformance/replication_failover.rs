//! Losing the peer a hosted signer's chunks were going through, over real HTTP.
//!
//! The mainnet log said `replicating StackerDB chunks with
//! https://api.mainnet.hiro.so/` while seven P2P-discovered peers stood idle:
//! replication cloned the one `SyncClient` the runtime picked at startup and
//! looped on it forever. Chain synchronization could survive losing that peer and
//! the signer this node hosts could not, which makes the hosted API a liveness
//! dependency of exactly the role the P2P work was meant to free.
//!
//! So these tests stand up two real `StackerDB` peers on loopback, break the first
//! one in each of the ways a peer actually breaks, and ask whose turn it is
//! afterwards. Every case is offline and deterministic: no fixture, no capture,
//! no environment variable, and nothing that waits on a clock.
//!
//! ## What this proves and what it does not
//!
//! Proved: a peer that refuses connections, rate-limits, errors, answers rubbish
//! or names a chunk it will not serve costs one round and the next round goes
//! elsewhere; a peer that serves a *forged* chunk keeps its turn, because refusing
//! that chunk is this node's own verdict and not the peer failing to answer — the
//! alternative would let one liar walk a node off every honest peer it has. And
//! the chunk this node's signer wrote survives the round that failed, rather than
//! being dropped with the peer that was going to carry it.
//!
//! Not proved here: the live run tasks/071 asks for, where a configured peer is
//! removed from a running node.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use nano_crypto::StacksPrivateKey;
use nano_node::hosting::{Replicas, identifier, round};
use nano_primitives::{Hash160, Network, hash160};
use nano_rpc::RpcState;
use nano_stackerdb::Chunk;

/// How the peer under test behaves when replication reaches it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Behaviour {
    /// Answers everything, and holds the chunk in `slot`.
    Honest,
    /// Rate-limits the metadata listing, which is a peer working and asking to be
    /// asked less — but asking *this* node, so the round goes to another peer.
    Throttling,
    Erroring,
    /// Answers the metadata listing with something that is not a listing.
    Rubbish,
    /// Names a newer slot version and then will not serve the chunk, which is what
    /// a replica half-way through catching up looks like from outside.
    Stale,
    /// Serves a chunk signed by a key that owns no slot.
    Forging,
}

#[derive(Clone)]
struct Peer {
    behaviour: Behaviour,
    /// The chunk this peer holds, already signed by the slot's writer.
    chunk: Chunk,
    /// How many chunks this peer has been handed, so a push can be counted.
    pushed: Arc<AtomicUsize>,
}

/// The reward cycle every peer here reports, so `replicated()` names one contract
/// set and the test does not depend on the calendar.
const CYCLE: u64 = 140;

fn tenure_info() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "consensus_hash": "00".repeat(20),
        "tenure_start_block_id": "00".repeat(32),
        "parent_consensus_hash": "00".repeat(20),
        "parent_tenure_start_block_id": "00".repeat(32),
        "tip_block_id": "00".repeat(32),
        "tip_height": 1,
        "reward_cycle": CYCLE,
    }))
}

async fn serve_tenure_info() -> impl IntoResponse {
    tenure_info()
}

async fn serve_metadata(State(peer): State<Peer>) -> axum::response::Response {
    match peer.behaviour {
        Behaviour::Throttling => StatusCode::TOO_MANY_REQUESTS.into_response(),
        Behaviour::Erroring => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        Behaviour::Rubbish => Json(serde_json::json!({"not": "a listing"})).into_response(),
        Behaviour::Honest | Behaviour::Stale | Behaviour::Forging => {
            let metadata = peer.chunk.metadata();
            Json(serde_json::json!([{
                "slot_id": metadata.slot_id,
                "slot_version": metadata.slot_version,
                "data_hash": hex::encode(metadata.data_hash.as_bytes()),
                "signature": hex::encode(metadata.signature.as_bytes()),
            }]))
            .into_response()
        }
    }
}

async fn serve_chunk(State(peer): State<Peer>) -> axum::response::Response {
    match peer.behaviour {
        Behaviour::Stale => StatusCode::NOT_FOUND.into_response(),
        _ => peer.chunk.data.into_response(),
    }
}

async fn take_chunk(State(peer): State<Peer>, body: String) -> axum::response::Response {
    drop(body);
    peer.pushed.fetch_add(1, Ordering::SeqCst);
    Json(serde_json::json!({"accepted": true})).into_response()
}

/// Serve one peer on a loopback port and answer with its endpoint.
async fn serve(peer: Peer) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a peer");
    let bound: SocketAddr = listener.local_addr().expect("an address");
    let app = Router::new()
        .route("/v3/tenures/info", get(serve_tenure_info))
        .route("/v2/stackerdb/{address}/{name}", get(serve_metadata))
        .route("/v2/stackerdb/{address}/{name}/chunks", post(take_chunk))
        .route(
            "/v2/stackerdb/{address}/{name}/{slot}/{version}",
            get(serve_chunk),
        )
        .route("/v2/stackerdb/{address}/{name}/{slot}", get(serve_chunk))
        .with_state(peer);
    tokio::spawn(async move {
        drop(axum::serve(listener, app).await);
    });
    format!("http://{bound}/")
}

/// A port nothing is listening on, which is what a peer that went away looks like.
async fn refused() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind to find a free port");
    let address: SocketAddr = listener.local_addr().expect("an address");
    drop(listener);
    format!("http://{address}/")
}

/// The chunk a peer holds, signed by `key`, in the slot `key` owns.
fn signed(key: &StacksPrivateKey, version: u32) -> Chunk {
    let mut chunk = Chunk::new(0, version, b"a signer's answer".to_vec());
    chunk.sign(key).expect("sign the chunk");
    chunk
}

fn writer(key: &StacksPrivateKey) -> Hash160 {
    hash160(&key.public_key().to_bytes_compressed())
}

/// A node whose replicas are configured for the contracts this cycle replicates,
/// so `pull` has somewhere to put a chunk and something to check it against.
async fn node(writers: Vec<Hash160>) -> RpcState {
    let state = RpcState::new(Network::MAINNET);
    let store = state.stackerdb();
    let mut store = store.write().await;
    for contract in nano_node::hosting::replicated(Network::MAINNET, CYCLE) {
        store.configure(&identifier(&contract), writers.clone());
    }
    drop(store);
    state
}

/// The first peer breaks, the second one is honest: whose turn is it next?
async fn failover(behaviour: Behaviour) -> (Replicas, RpcState, usize) {
    let key = StacksPrivateKey::from_bytes([7; 32]).expect("a key");
    let chunk = signed(&key, 3);
    let pushed = Arc::new(AtomicUsize::new(0));
    let broken = if behaviour == Behaviour::Honest {
        refused().await
    } else {
        serve(Peer {
            behaviour,
            chunk: chunk.clone(),
            pushed: Arc::clone(&pushed),
        })
        .await
    };
    let honest = serve(Peer {
        behaviour: Behaviour::Honest,
        chunk,
        pushed: Arc::clone(&pushed),
    })
    .await;
    let state = node(vec![writer(&key)]).await;
    let mut replicas = Replicas::from_endpoints(&[broken.clone(), honest.clone()]);
    assert_eq!(
        replicas.serving(),
        Some(broken.as_str()),
        "the round under test has to start at the peer that is about to break"
    );

    let outcome = round(&mut replicas, Network::MAINNET, &state, &[]).await;
    assert!(
        outcome.is_err(),
        "a peer that does not answer is a round that failed: {outcome:?}"
    );
    assert_eq!(
        replicas.serving(),
        Some(honest.as_str()),
        "the next round has to go somewhere else"
    );

    // And the honest peer's round completes, which is what makes the failover a
    // recovery rather than a different way of failing.
    round(&mut replicas, Network::MAINNET, &state, &[])
        .await
        .expect("the honest peer serves the round");
    let (served, failures) = replicas.distribution();
    assert_eq!(served, 1, "exactly one peer has served a round");
    assert_eq!(failures, 1, "and exactly one round failed");
    (replicas, state, pushed.load(Ordering::SeqCst))
}

#[tokio::test]
async fn a_peer_that_went_away_costs_one_round() {
    failover(Behaviour::Honest).await;
}

#[tokio::test]
async fn a_rate_limiting_peer_costs_one_round() {
    failover(Behaviour::Throttling).await;
}

#[tokio::test]
async fn an_erroring_peer_costs_one_round() {
    failover(Behaviour::Erroring).await;
}

#[tokio::test]
async fn a_peer_answering_rubbish_costs_one_round() {
    failover(Behaviour::Rubbish).await;
}

#[tokio::test]
async fn a_replica_that_names_a_chunk_it_will_not_serve_costs_one_round() {
    failover(Behaviour::Stale).await;
}

/// The one case that must *not* rotate.
///
/// A forged chunk is refused by `StackerDbStore::put`, which is this node's verdict
/// on the chunk and not the peer failing to answer. Rotating on it would let a
/// single peer serving rubbish walk a node off every honest peer it has, one round
/// each, and the pool would be a liability rather than a defence.
#[tokio::test]
async fn a_peer_serving_a_forged_chunk_keeps_its_turn() {
    let owner = StacksPrivateKey::from_bytes([7; 32]).expect("a key");
    let stranger = StacksPrivateKey::from_bytes([9; 32]).expect("another key");
    let forging = serve(Peer {
        behaviour: Behaviour::Forging,
        chunk: signed(&stranger, 3),
        pushed: Arc::new(AtomicUsize::new(0)),
    })
    .await;
    let honest = serve(Peer {
        behaviour: Behaviour::Honest,
        chunk: signed(&owner, 3),
        pushed: Arc::new(AtomicUsize::new(0)),
    })
    .await;
    let state = node(vec![writer(&owner)]).await;
    let mut replicas = Replicas::from_endpoints(&[forging.clone(), honest]);

    round(&mut replicas, Network::MAINNET, &state, &[])
        .await
        .expect("the peer answered every request it was asked");
    assert_eq!(
        replicas.serving(),
        Some(forging.as_str()),
        "a chunk this node refuses is not a peer that failed to answer"
    );
    assert_eq!(replicas.distribution(), (1, 0));
    assert!(
        state
            .stackerdb()
            .read()
            .await
            .chunk(
                &identifier(&nano_node::hosting::replicated(Network::MAINNET, CYCLE)[0]),
                0,
                None,
            )
            .is_none(),
        "and the forgery is still not in the replica"
    );
}

/// A chunk the hosted signer wrote, handed to the round that then failed.
///
/// It has to survive: it is a signature the network is counting, and the peer whose
/// turn it happened to be going away is no reason to lose it. The honest peer gets
/// handed it on the next round.
#[tokio::test]
async fn a_chunk_this_node_wrote_survives_the_peer_that_was_carrying_it() {
    let key = StacksPrivateKey::from_bytes([7; 32]).expect("a key");
    let chunk = signed(&key, 3);
    let pushed = Arc::new(AtomicUsize::new(0));
    let gone = refused().await;
    let honest = serve(Peer {
        behaviour: Behaviour::Honest,
        chunk: chunk.clone(),
        pushed: Arc::clone(&pushed),
    })
    .await;
    let state = node(vec![writer(&key)]).await;
    let mut replicas = Replicas::from_endpoints(&[gone, honest]);
    let outbound_chunk = (
        identifier(&nano_node::hosting::replicated(Network::MAINNET, CYCLE)[0]),
        chunk,
    );
    let outbound_bytes = outbound_chunk.0.len() + outbound_chunk.1.data.len() + 73;
    let (sender, mut receiver) = nano_queue::channel(nano_queue::Limits::new(1, outbound_bytes));
    sender
        .try_send(outbound_chunk, outbound_bytes)
        .expect("the signer chunk fits its production-shaped queue");
    let outbound = vec![
        receiver
            .recv_lease()
            .await
            .expect("the retry keeps its byte lease"),
    ];

    assert!(
        round(&mut replicas, Network::MAINNET, &state, &outbound)
            .await
            .is_err(),
        "the peer whose turn it was is not there"
    );
    assert_eq!(
        pushed.load(Ordering::SeqCst),
        0,
        "nothing reached the peer that was not there"
    );

    round(&mut replicas, Network::MAINNET, &state, &outbound)
        .await
        .expect("the honest peer takes it");
    assert_eq!(
        pushed.load(Ordering::SeqCst),
        1,
        "the chunk the signer wrote reached a peer on the next round"
    );
}

/// Peer discovery finding more peers must not send the next round back to the front.
#[tokio::test]
async fn a_pool_that_grew_keeps_whose_turn_it_was() {
    let (first, second, third) = (
        "http://127.0.0.1:1/".to_owned(),
        "http://127.0.0.1:2/".to_owned(),
        "http://127.0.0.1:3/".to_owned(),
    );
    let mut replicas = Replicas::from_endpoints(&[first.clone(), second.clone()]);
    replicas.rotate();
    assert_eq!(replicas.serving(), Some(second.as_str()));

    // Discovery puts a new peer at the front, which shifts every index by one.
    replicas.refresh(&[third, first, second.clone()]);
    assert_eq!(
        replicas.serving(),
        Some(second.as_str()),
        "the turn follows the endpoint it was pointing at, not its index"
    );
}
