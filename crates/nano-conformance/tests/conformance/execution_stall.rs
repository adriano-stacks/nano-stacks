//! Following reaches the peer's tip; execution reaches nothing; the RPC says so.
//!
//! This is the failure [[046-distinguish-followed-and-executed-chain-tips]] was
//! opened for, and it is the one shape the suite could not produce. On
//! 2026-08-02 a node reported height 8,688,023 while its durable MARF ended at
//! 8,665,601, and every endpoint it served agreed with the wrong number — so
//! 18,290 blocks were counted as verified state roots that were never executed.
//!
//! The pieces existed separately. `follow_path` proves the follow path executes
//! none of a chain nobody signed, in process, reading the executor's own tip.
//! `binary_restart` proves the binary's `/v2/info` reports the executed tip, with
//! honest peers, where the executed tip is also the peer's. Neither puts the two
//! together, and the together is the bug: an RPC is only trustworthy about a
//! *disagreement*.
//!
//! So: the shipped binary, a peer serving a coherent chain nobody signed, and the
//! three heights read back over HTTP. The peer's chain is well-formed and links
//! block to block, so the node follows it and stages it; the signatures are over
//! a preimage containing the timestamp, so no block of it executes. The node is
//! therefore at its checkpoint with a peer twelve blocks ahead, and has to say
//! both.

use std::{fs, time::Instant};

use crate::binary_restart::{PATIENCE, Running, free_port, serve_burnchain, write_config};
use crate::follow_path::{Served, alternative_history, captured_chain, serve, snapshots};

/// Blocks the peer serves above the anchor.
const SERVED_BLOCKS: usize = 12;

/// What the node says about itself, as `/nano/sync_status` reports it.
async fn sync_status(rpc: u16) -> Option<serde_json::Value> {
    let body = reqwest::get(format!("http://127.0.0.1:{rpc}/nano/sync_status"))
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    serde_json::from_str(&body).ok()
}

/// Wait until the node reports having heard a peer at this height, or give up.
///
/// Waiting on the *followed* height rather than on a timer, because that is the
/// precondition of the whole test: a node that had not yet reached the peer would
/// pass every assertion below for the boring reason.
async fn followed_height(node: &Running, at_least: u64) -> u64 {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(status) = sync_status(node.rpc).await
            && let Some(height) = status["followed_stacks_height"].as_u64()
            && height >= at_least
        {
            return height;
        }
        assert!(
            Instant::now() < deadline,
            "the node never reported hearing the peer at {at_least}: {}",
            fs::read_to_string(&node.log).unwrap_or_default()
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn a_node_that_follows_to_the_tip_and_executes_nothing_says_both() {
    let chain = captured_chain();
    let anchor = chain.first().expect("the capture has a block").clone();
    // Forked at the anchor, so nothing the peer serves is executable: a branch
    // sharing a prefix would have the node execute that prefix — correctly, it
    // being the canonical chain — and "executed nothing" would then be a claim
    // about the fixture.
    let served = alternative_history(&chain[..=SERVED_BLOCKS], 1);
    let peer_tip = served.last().expect("a tip").header.chain_length;
    assert!(
        peer_tip > anchor.header.chain_length,
        "the peer is not ahead of the anchor, so there is nothing to be behind"
    );

    let (burnchain, burnchain_task) = serve_burnchain().await;
    let (peer, peer_task) = serve(Served::honest(served, snapshots())).await;
    let directory = tempfile::tempdir().expect("a directory");
    let rpc = free_port().await;
    let config = write_config(
        directory.path(),
        &[peer.base_url().to_string()],
        &burnchain,
        rpc,
        &anchor,
        snapshots()
            .iter()
            .find(|row| row.consensus_hash == anchor.header.consensus_hash.to_string())
            .map_or(0, |row| row.block_height),
    );
    let node = Running::start(&config, rpc, directory.path().join("node.log"));

    let followed = followed_height(&node, peer_tip).await;
    // Give the node rounds to fail in: the assertion is that it *stays* where it
    // is, and a single sample taken before it tried anything says nothing.
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;

    let executed = node
        .executed_height()
        .await
        .expect("the node answers /v2/info from its executed tip");
    let status = sync_status(rpc)
        .await
        .expect("the node answers its own status");
    let log = fs::read_to_string(&node.log).unwrap_or_default();
    node.kill();
    peer_task.abort();
    burnchain_task.abort();

    // The three claims, in the order they were confused in.
    assert_eq!(
        executed, anchor.header.chain_length,
        "/v2/info reported a height this node never executed; the log said:\n{log}"
    );
    assert_eq!(
        status["executed_stacks_height"].as_u64(),
        Some(anchor.header.chain_length),
        "the executed height moved on a chain nobody signed:\n{log}"
    );
    assert!(
        followed >= peer_tip,
        "the peer was not followed to its tip, so nothing here is about a disagreement"
    );
    assert_eq!(
        status["followed_stacks_height"]
            .as_u64()
            .map(|followed| followed
                - status["executed_stacks_height"]
                    .as_u64()
                    .unwrap_or_default()),
        status["blocks_behind"].as_u64(),
        "the gap is not the difference between the two heights it is derived from"
    );
    assert!(
        status["blocks_behind"]
            .as_u64()
            .is_some_and(|behind| behind > 0),
        "a node at its checkpoint with a peer {peer_tip} blocks up reported no gap: {status}"
    );

    // And the log says which of the two it is, in the sentence a round that
    // executed nothing prints and no other round does.
    assert!(
        log.contains("executed nothing:"),
        "no round said it executed nothing:\n{log}"
    );
}
