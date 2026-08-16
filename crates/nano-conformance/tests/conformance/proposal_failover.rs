//! Proposal validation survives the first history peer going away or lying.
//!
//! These drive the public hosting role rather than `TenureSource` alone: the role
//! catches its private validator up, derives each burn view from the captured
//! Bitcoin chain, and executes the candidate before returning its verdict.

use std::{net::SocketAddr, path::Path};

use nano_node::{config::Config, hosting, signer};
use nano_rpc::ProposalRequest;
use nano_sync::{SyncClient, TenureSource};

use crate::{
    binary_restart::{
        PATIENCE, authenticated_anchor_index, free_port, serve_burnchain, write_config,
    },
    follow_path::{Policy, Served, captured_chain, fixtures, pox, serve, snapshots},
};

/// Canonical blocks the proposal validator recovers above its authenticated anchor.
const RECOVERED: usize = 2;

/// A client aimed at a port the kernel allocated and nobody retained.
async fn dead_peer() -> SyncClient {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind a loopback port");
    let address = listener.local_addr().expect("the bound address");
    drop(listener);
    SyncClient::new(reqwest::Url::parse(&format!("http://{address}/")).expect("a base URL"))
        .expect("a client")
}

/// Open the production proposal role and ask it to validate one candidate.
async fn verdict(
    directory: &Path,
    burnchain: &str,
    bootstrap: SyncClient,
    peers: Vec<SyncClient>,
    chain: &[nano_chainstate::NakamotoBlock],
    mut candidate: nano_chainstate::NakamotoBlock,
) {
    let endpoints = peers
        .iter()
        .map(|peer| peer.base_url().to_string())
        .collect::<Vec<_>>();
    let config_path = write_config(directory, &endpoints, burnchain, free_port().await, chain);
    let mut config = Config::load(config_path).expect("the proposal-role configuration loads");
    config.node.max_sync_blocks = 16;
    let calendar = pox();
    let network = nano_conformance::captured_network(&fixtures());
    let mut opening = TenureSource::only(bootstrap);
    let validator = signer::open(
        &config,
        network,
        &calendar,
        &mut opening,
        &config.chainstate_dir("signer-chainstate"),
    )
    .await
    .expect("the authenticated proposal validator opens");

    // A proposal has not collected signer signatures yet. Everything else stays
    // byte-for-byte canonical, including the state root the role must reproduce.
    candidate.header.signer_signatures.clear();
    let (send, receive) = nano_queue::channel(nano_rpc::PROPOSAL_QUEUE_LIMITS);
    let state = nano_rpc::RpcState::new(network);
    let role = tokio::spawn(hosting::validate_proposals(
        config,
        calendar,
        None,
        TenureSource::new(peers),
        validator,
        state,
        receive,
    ));
    let (answer, answered) = tokio::sync::oneshot::channel();
    let bytes = candidate.encode().len();
    send.try_send(
        ProposalRequest {
            block: candidate,
            verdict: answer,
        },
        bytes,
    )
    .map_err(|error| error.reason)
    .expect("the proposal role is listening");
    let answer = match tokio::time::timeout(PATIENCE, answered).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(_)) => match role.await {
            Ok(Ok(())) => panic!("the proposal role stopped without an error"),
            Ok(Err(error)) => panic!("the proposal role stopped: {error}"),
            Err(error) => panic!("the proposal role task stopped: {error}"),
        },
        Err(error) => panic!("the proposal role did not answer within the test bound: {error}"),
    };
    role.abort();
    answer.unwrap_or_else(|(reason, code)| panic!("proposal refused as {code:?}: {reason}"));
}

// Opening the production role reads the burnchain synchronously. Keep the
// loopback server on another runtime worker so that read can be answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proposal_recovery_survives_the_peer_it_started_on() {
    let chain = captured_chain();
    let anchor = authenticated_anchor_index(&chain);
    let target = anchor + RECOVERED;
    let honest_policy = Policy::default();
    let (honest, honest_task) =
        serve(Served::honest(chain[..=target].to_vec(), snapshots()).under(honest_policy.clone()))
            .await;
    let gone = dead_peer().await;
    let (burnchain, burnchain_task) = serve_burnchain();
    let directory = tempfile::tempdir().expect("a state directory");

    verdict(
        directory.path(),
        &burnchain,
        honest.clone(),
        vec![gone, honest],
        &chain,
        chain[target + 1].clone(),
    )
    .await;
    assert!(
        honest_policy.blocks_asked() >= RECOVERED,
        "the surviving peer did not serve the recovered suffix"
    );

    honest_task.abort();
    drop(burnchain_task);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proposal_recovery_survives_a_first_peer_that_lies() {
    let chain = captured_chain();
    let anchor = authenticated_anchor_index(&chain);
    let target = anchor + RECOVERED;
    let served = chain[..=target].to_vec();
    let (honest, honest_task) = serve(Served::honest(served.clone(), snapshots())).await;
    let lying_policy = Policy::default();
    let (liar, liar_task) = serve(
        Served::honest(served, snapshots())
            .answering_the_wrong_block()
            .under(lying_policy.clone()),
    )
    .await;
    let (burnchain, burnchain_task) = serve_burnchain();
    let directory = tempfile::tempdir().expect("a state directory");

    verdict(
        directory.path(),
        &burnchain,
        honest.clone(),
        vec![liar, honest],
        &chain,
        chain[target + 1].clone(),
    )
    .await;
    assert!(
        lying_policy.blocks_asked() > 0,
        "the first peer never answered a block request wrongly"
    );

    honest_task.abort();
    liar_task.abort();
    drop(burnchain_task);
}
