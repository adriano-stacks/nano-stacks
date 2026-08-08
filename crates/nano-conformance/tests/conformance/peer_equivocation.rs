//! Losing a peer and being lied to by one, over real HTTP.
//!
//! `nano-p2p`'s `loopback` tests isolate a peer that signs its handshake with
//! the wrong key and one that claims the wrong network. Both are refused at the
//! wire. The case those cannot reach — and the one [[027]] names — is a peer that
//! completes a perfectly correct handshake and then **serves a plausible but
//! wrong chain**: well-formed blocks, decodable, consistent with themselves, and
//! belonging to no chain the network signed.
//!
//! That peer is refused by weight, not by shape, and weight is checked in the
//! fork choice — `choose_canonical_tip` — before a single block is downloaded.
//! So these tests stand up real HTTP peers on loopback, serve real mainnet blocks
//! through one and mutations of them through another, and ask the pool what it
//! chooses.
//!
//! Everything here is offline. The blocks and the reward set are
//! `fixtures/mainnet`, the same five blocks and cycle-140 set `mainnet_envelope`
//! uses, so no capture and no environment variable is needed and the gate cannot
//! skip itself.
//!
//! ## What this proves and what it does not
//!
//! Proved: removing a peer does not change what the node follows, and a lying
//! peer cannot make it follow anything — including when the liar is the *only*
//! peer left, where the honest answer is to follow nothing rather than to follow
//! the lie.
//!
//! Not proved here: the same thing happening to a running node over a Bitcoin
//! reorganization, which needs the live run tasks/053 describes.

use std::{collections::BTreeMap, fs, net::SocketAddr, path::Path, sync::Arc};

use axum::{Router, extract::State, http::StatusCode, routing::get};
use nano_chainstate::{NakamotoBlock, Signer, SignerSet, SignerWeights};
use nano_crypto::StacksPublicKey;
use nano_sync::{PeerPool, SyncClient, choose_canonical_tip, weigh_tip};

fn mainnet() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("mainnet")
}

/// The reward set mainnet published for the cycle these blocks fall in.
fn reward_set() -> SignerWeights {
    let document: serde_json::Value =
        serde_json::from_slice(&fs::read(mainnet().join("stacker_set-140.json")).expect("read it"))
            .expect("parse the reward set");
    let signers = document["stacker_set"]["signers"]
        .as_array()
        .expect("the reward set has signers")
        .iter()
        .map(|entry| Signer {
            public_key: StacksPublicKey::from_bytes(
                &hex::decode(
                    entry["signing_key"]
                        .as_str()
                        .expect("a signing key")
                        .trim_start_matches("0x"),
                )
                .expect("hexadecimal"),
            )
            .expect("a public key"),
            weight: u32::try_from(entry["weight"].as_u64().expect("a weight")).expect("it fits"),
        })
        .collect();
    // By signing-key hash: that is the shape `.signers` holds and the one both the
    // fork choice and execution weigh against, so a test that weighed a different
    // shape would be exercising a rule the node does not apply.
    SignerSet::new(signers)
        .expect("the reward set is not empty")
        .signing_weights()
        .expect("the reward set is well formed")
}

/// The captured mainnet blocks, lowest first.
fn blocks() -> Vec<NakamotoBlock> {
    let mut blocks: Vec<NakamotoBlock> = fs::read_dir(mainnet().join("blocks"))
        .expect("read the block directory")
        .flatten()
        .filter_map(|entry| NakamotoBlock::decode(&fs::read(entry.path()).ok()?).ok())
        .collect();
    blocks.sort_by_key(|block| block.header.chain_length);
    assert!(blocks.len() >= 2, "these tests need at least two blocks");
    blocks
}

/// What one fake peer will answer.
struct Served {
    /// `/v3/blocks/:id`, keyed by the identifier a client will ask for, as bare
    /// hexadecimal — which is how `StacksBlockId` renders itself into a path. The
    /// key need not be the block's own identifier: a peer that answers with
    /// something else is one of the lies under test.
    blocks: BTreeMap<String, Vec<u8>>,
    /// `/v3/tenures/info`.
    info: serde_json::Value,
}

/// The tenure-info document a peer offering `tip` would publish.
fn tenure_info(tip: &NakamotoBlock) -> serde_json::Value {
    // Bare hexadecimal, no `0x`: stacks-core's `/v3/tenures/info` writes these
    // unprefixed and `SyncClient` reads them that way.
    let id = hex::encode(tip.block_id());
    let consensus = hex::encode(tip.header.consensus_hash.as_bytes());
    serde_json::json!({
        "consensus_hash": consensus,
        "tenure_start_block_id": id,
        "parent_consensus_hash": consensus,
        "parent_tenure_start_block_id": id,
        "tip_block_id": id,
        "tip_height": tip.header.chain_length,
        "reward_cycle": 140,
    })
}

/// An honest peer: it offers `tip` and answers for it with `tip`.
fn honest(tip: &NakamotoBlock) -> Served {
    Served {
        blocks: std::iter::once((hex::encode(tip.block_id()), tip.encode())).collect(),
        info: tenure_info(tip),
    }
}

/// Start a peer on loopback and return a client pointed at it.
///
/// Port zero, because two of these run at once inside one test binary and a
/// fixed port would make them collide.
async fn serve(served: Served) -> (SyncClient, tokio::task::JoinHandle<()>) {
    let state = Arc::new(served);
    let router = Router::new()
        .route(
            "/v3/tenures/info",
            get(|State(state): State<Arc<Served>>| async move { axum::Json(state.info.clone()) }),
        )
        .route(
            "/v3/blocks/{id}",
            get(
                |State(state): State<Arc<Served>>,
                 axum::extract::Path(id): axum::extract::Path<String>| async move {
                    let id = id.trim_start_matches("0x").to_lowercase();
                    state.blocks.get(&id).map_or_else(
                        || (StatusCode::NOT_FOUND, Vec::new()),
                        |bytes| (StatusCode::OK, bytes.clone()),
                    )
                },
            ),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind a loopback port");
    let address = listener.local_addr().expect("the bound address");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let client = SyncClient::new(
        reqwest::Url::parse(&format!("http://{address}/")).expect("a valid base url"),
    )
    .expect("a client");
    (client, handle)
}

/// The tip the pool chooses among its peers, and which peer offered it.
async fn chosen(
    peers: Vec<SyncClient>,
    distrust: &[usize],
) -> Option<(usize, nano_primitives::StacksBlockId)> {
    let mut pool = PeerPool::new(peers);
    for peer in distrust {
        pool.distrust(*peer);
    }
    let candidates = pool.candidate_tips().await;
    let signers = reward_set();
    choose_canonical_tip(&candidates, Some(&signers), None)
        .map(|candidate| (candidate.peer, candidate.header.block_id()))
}

/// A block that claims to be far ahead of the chain it came from.
///
/// The cheapest plausible lie and the one that matters: length is what a naive
/// fork choice compares, so a peer that adds a thousand to it wins on length
/// alone. Nothing else about the block is touched — every transaction is real,
/// the Merkle root matches them, the state root is one the network computed.
/// What it cannot fix is the signatures: they were made over a preimage that
/// includes `chain_length`, so bumping it leaves them recovering to keys the
/// reward set does not contain.
fn claims_to_be_ahead(block: &NakamotoBlock) -> NakamotoBlock {
    let mut lie = block.clone();
    lie.header.chain_length = block.header.chain_length.saturating_add(1_000);
    lie
}

/// A lying peer cannot win the fork choice from an honest one.
#[tokio::test]
async fn a_peer_offering_a_longer_chain_nobody_signed_does_not_win() {
    let blocks = blocks();
    let truth = blocks.last().expect("a tip");
    let lie = claims_to_be_ahead(truth);

    // The lie is genuinely more attractive on length, so this is not passing
    // because the liar offered something worse.
    assert!(
        lie.header.chain_length > truth.header.chain_length,
        "the lie is not longer, so nothing is being tested"
    );
    let signers = reward_set();
    assert!(
        weigh_tip(&truth.header, &signers).is_ok(),
        "the honest block does not weigh, so this fixture cannot be the control"
    );
    assert!(
        weigh_tip(&lie.header, &signers).is_err(),
        "the lengthened block still weighs, which would mean the signatures do \
         not cover chain_length"
    );

    let (honest_client, honest_task) = serve(honest(truth)).await;
    let (lying_client, lying_task) = serve(honest(&lie)).await;

    let (peer, chosen_id) = chosen(vec![honest_client, lying_client], &[])
        .await
        .expect("an honest peer is present, so a tip is chosen");
    assert_eq!(peer, 0, "the pool followed the lying peer");
    assert_eq!(
        chosen_id.to_string(),
        truth.block_id().to_string(),
        "the chosen tip is not the block the network signed"
    );

    honest_task.abort();
    lying_task.abort();
}

/// With only the liar left, the node follows nothing.
///
/// The half that a "the honest peer wins" test cannot reach, and the one that
/// matters for the acceptance criterion: a node whose last honest peer went away
/// must stall rather than adopt the only chain on offer. Stalling is visible and
/// recoverable; following is a fork.
#[tokio::test]
async fn a_node_left_with_only_a_lying_peer_follows_nothing() {
    let blocks = blocks();
    let lie = claims_to_be_ahead(blocks.last().expect("a tip"));
    let (lying_client, task) = serve(honest(&lie)).await;

    assert!(
        chosen(vec![lying_client], &[]).await.is_none(),
        "the pool adopted a tip no signer approved because it was the only one \
         offered"
    );
    task.abort();
}

/// Removing a peer does not change what the node follows.
///
/// Two honest peers offering the same tip, then each one distrusted in turn. The
/// canonical tip has to be the same block all three times — a fork choice that
/// depended on which peer answered would be a node that reorganizes when a peer
/// restarts.
#[tokio::test]
async fn removing_a_peer_does_not_change_the_chosen_tip() {
    let blocks = blocks();
    let truth = blocks.last().expect("a tip");
    let (first, first_task) = serve(honest(truth)).await;
    let (second, second_task) = serve(honest(truth)).await;
    let peers = vec![first, second];

    let both = chosen(peers.clone(), &[]).await.expect("a tip with both");
    let without_second = chosen(peers.clone(), &[1])
        .await
        .expect("a tip with the second peer gone");
    let without_first = chosen(peers, &[0])
        .await
        .expect("a tip with the first peer gone");

    for (label, (_, id)) in [
        ("both", both),
        ("without the second", without_second),
        ("without the first", without_first),
    ] {
        assert_eq!(
            id.to_string(),
            truth.block_id().to_string(),
            "{label}: the chosen tip moved when the set of peers changed"
        );
    }

    // And a peer that is gone is not asked again, which is what makes losing one
    // cost nothing rather than cost a timeout every round.
    let mut pool = PeerPool::new(vec![]);
    pool.distrust(0);
    assert!(!pool.is_trusted(0));

    first_task.abort();
    second_task.abort();
}

/// A peer that answers a block request with a different block changes nothing.
///
/// `SyncClient::block` does not check that what came back is what was asked for
/// — recorded here as a fact rather than assumed away, because it means a peer
/// can put another block into the client's cache under an identifier of its
/// choosing. It is not exploitable for a fork, and this is why: whatever comes
/// back is still weighed against the reward set and still compared on length, so
/// a substitute can only be a block the network *did* sign, and offering a real
/// block from lower down the same chain makes the liar less attractive rather
/// than more.
///
/// The peer that substitutes is honest about its `tip_height`; only the bytes it
/// serves are wrong. That is the shape a downgrade attack would take.
#[tokio::test]
async fn a_peer_substituting_a_block_for_the_one_asked_for_changes_nothing() {
    let blocks = blocks();
    let truth = blocks.last().expect("a tip");
    let older = blocks.first().expect("an older block");
    assert!(
        older.header.chain_length < truth.header.chain_length,
        "the substitute is not lower, so this proves nothing"
    );

    // It advertises the real tip and answers for that identifier with the older
    // block.
    let substituting = Served {
        blocks: std::iter::once((hex::encode(truth.block_id()), older.encode())).collect(),
        info: tenure_info(truth),
    };
    let (honest_client, honest_task) = serve(honest(truth)).await;
    let (substituting_client, substituting_task) = serve(substituting).await;

    let (peer, chosen_id) = chosen(vec![honest_client, substituting_client], &[])
        .await
        .expect("a tip is chosen");
    assert_eq!(peer, 0, "the substituting peer won the fork choice");
    assert_eq!(
        chosen_id.to_string(),
        truth.block_id().to_string(),
        "the chosen tip is the substituted block"
    );

    honest_task.abort();
    substituting_task.abort();
}
