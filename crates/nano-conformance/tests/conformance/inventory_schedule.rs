//! An inventory decides what to download next, not merely who to ask.
//!
//! The last code item of [[054-join-and-synchronize-over-the-stacks-p2p-network]].
//! Three slices exchanged Nakamoto inventories and used them only to *shortlist
//! peers*: the downloader walked parent links backwards from one peer's tip, so the
//! single tenure it wanted next was always the parent of the last answer, and there
//! was no set of wanted tenures for `assign_tenures` to spread. What that costs is
//! not a round trip here and there — it is that **nothing can execute until the whole
//! gap has been downloaded**, because the chain below a descent is not contiguous with
//! this node's tip until the descent reaches it. From the mainnet checkpoint that is
//! twenty thousand blocks of downloading before the first one runs.
//!
//! A schedule keyed on burn views has no such ordering. Every burn block this node has
//! walked is named by its own sortition chain, so the tenure directly above the
//! executed tip can be asked for by name before anything above it is known.
//!
//! What is pinned here, and each is its own claim:
//!
//! - the schedule **executes on the first round** where the backward descent under the
//!   same budget executes nothing, which is the whole gain and the only thing that
//!   distinguishes forward from backward;
//! - both paths reach the **same tip, the same signed root and the same content root**,
//!   so the forward one is not a different chain;
//! - only the peer whose inventory **claimed** a tenure is asked for it, counted from
//!   the requests each peer actually received;
//! - a peer that answers a scheduled tenure with **another tenure's blocks** is
//!   refused, which is the one substitution a fetch addressed by burn view is open to;
//! - a node with **no claims** behaves exactly as it did, which is what keeps an
//!   HTTP-only configuration and the miner on the path they were on.
//!
//! Offline: the captured chain, its Bitcoin blocks and its snapshots over loopback.

use nano_chainstate::NakamotoBlock;
use nano_node::{CatchUpBudget, CatchUpRound, CheckpointExecutor, staging::Staging};
use nano_p2p::TenureClaim;
use nano_primitives::{BitVec, Hash160};
use nano_sync::{SyncClient, TenureSource};

use crate::follow_path::{
    MovableBurnchain, Policy, Served, burn_height_of, captured_burnchain, captured_chain,
    derived_chain, node, pox, second_burn_view, serve, snapshots,
};

/// How much one round may fetch and execute.
///
/// Deliberately smaller than the gap. The contrast this file is about only exists
/// under a budget a backward descent cannot spend its way across in one round — give
/// it enough to reach the executed tip and it executes on the first round too, and the
/// difference between forward and backward stops being visible at all.
const BUDGET: CatchUpBudget = CatchUpBudget {
    fetch: 12,
    execute: 64,
};

/// A claim that a peer holds every tenure of the cycle being walked.
///
/// Every bit rather than the ones the peer really has, and that is the honest shape
/// for this fixture: nothing here is testing whether nano believes an inventory, and a
/// bit set for a burn block that elected nobody is answered by the peer with an empty
/// tenure and dropped. What the vector has to do is name tenures, which is what makes
/// `assign_tenures` produce a schedule at all.
fn claims_everything(peer: u8, endpoint: &str) -> TenureClaim {
    let mut tenures = BitVec::<2100>::zeros(2100).expect("a cycle-length bit vector");
    for bit in 0..tenures.len() {
        tenures.set(bit, true).expect("in bounds");
    }
    TenureClaim {
        peer: Hash160::from_bytes([peer; 20]),
        // Without the trailing slash a `Url` normalises onto, because that is the shape
        // a `data_url` arrives in from a handshake — and comparing the two as *strings*
        // is what made `TenureSource::prefer` a no-op on the live mainnet node.
        endpoint: Some(endpoint.trim_end_matches('/').to_owned()),
        tenures,
    }
}

/// What a finished run can be compared by, on both sides of a change of downloader.
///
/// Three facts and not one. The tip says where it stopped; the header's
/// `state_index_root` is what the network signed; the MARF's content root for that
/// block is what this node's trie actually holds under it. A run that agreed about the
/// signed root and disagreed about the content would be writing different state and
/// reaching the same header, which comparing tips alone would miss.
#[derive(Debug, Eq, PartialEq)]
struct Closed {
    tip: [u8; 32],
    height: u64,
    signed_root: [u8; 32],
    content_root: Option<nano_primitives::TrieHash>,
}

/// A node standing on the captured checkpoint with a sortition chain of its own.
///
/// The chain is what makes a schedule possible: a bit index into a reward cycle is
/// only a tenure once some local answer says which burn view that offset is, and this
/// is that answer. It is derived from the captured Bitcoin blocks through the
/// production seeding path, so nothing about the schedule's keys comes from a peer.
async fn ready_node(
    directory: &std::path::Path,
    burnchain: &MovableBurnchain,
    served: &[NakamotoBlock],
) -> (CheckpointExecutor<MovableBurnchain>, Staging, u64) {
    let rows = snapshots();
    let burn_of = |block: &NakamotoBlock| burn_height_of(&rows, block);
    let seed = second_burn_view(served, &burn_of);
    let upto_seed = served
        .iter()
        .take_while(|block| burn_of(block) <= seed)
        .count();
    let (mut executor, _) = node(directory, burnchain.clone());
    let staging = Staging::open(&directory.join("staging.sqlite")).expect("staging opens");
    // Up to the seed against an ordinary peer, because a chain cannot be seeded at a
    // burn block this node has not reached. Everything above it is what the test drives.
    let (client, task) = serve(Served::honest(served[..upto_seed].to_vec(), snapshots())).await;
    let mut history = TenureSource::only(client.clone());
    crate::follow_path::close_the_gap(
        &mut executor,
        &client,
        &mut history,
        &staging,
        BUDGET,
        served[..upto_seed]
            .last()
            .expect("a prefix tip")
            .header
            .chain_length,
    )
    .await;
    task.abort();
    assert_eq!(
        executor.bitcoin_height(),
        seed,
        "the node did not reach the burn view its sortition chain is seeded at"
    );
    let top = served.iter().map(burn_of).max().expect("a served burn view");
    let tracker = derived_chain(seed, top, burnchain, &directory.join("capture"));
    executor.track_sortitions(tracker, directory.join("sortitions"));
    (executor, staging, top)
}

/// The blocks a run of this fixture closes a gap over, and the tip it closes on.
///
/// Cut at the reward-cycle boundary above the seed, for the reason [[049]] records: a
/// checkpoint-seeded sortition chain cannot derive across a cycle boundary, because
/// the consensus hash mixes one bit per cycle and whether a new cycle chose an anchor
/// block is not knowable from the burnchain alone.
fn served_chain() -> Vec<NakamotoBlock> {
    let chain = captured_chain();
    let rows = snapshots();
    let burn_of = |block: &NakamotoBlock| burn_height_of(&rows, block);
    let seed = second_burn_view(&chain, &burn_of);
    let boundary = (seed / crate::follow_path::CYCLE + 1) * crate::follow_path::CYCLE;
    chain
        .iter()
        .take_while(|block| burn_of(block) < boundary)
        .cloned()
        .collect()
}

/// Take rounds until the tip stops moving or the target is reached.
async fn run(
    executor: &mut CheckpointExecutor<MovableBurnchain>,
    peer: &SyncClient,
    history: &mut TenureSource,
    staging: &Staging,
    claims: &[TenureClaim],
    target: u64,
) -> Vec<CatchUpRound> {
    let mut rounds = Vec::new();
    for _ in 0..64 {
        let round = executor
            .catch_up(peer, history, &pox(), staging, BUDGET, claims)
            .await
            .expect("a round commits what it executed");
        rounds.push(round);
        if executor.tip().header.chain_length >= target {
            break;
        }
    }
    rounds
}

fn closed(executor: &mut CheckpointExecutor<MovableBurnchain>) -> Closed {
    let tip = executor.tip().clone();
    Closed {
        tip: *tip.block_id().as_bytes(),
        height: tip.header.chain_length,
        signed_root: *tip.header.state_index_root.as_bytes(),
        content_root: executor
            .chainstate_mut()
            .state_content_root(*tip.block_id().as_bytes()),
    }
}

/// The inventory schedules the tenure above the tip, and the round executes it.
///
/// The claim in one number: **the first round executes blocks**. The same node, the
/// same peer and the same budget with no claims fetches exactly as much and executes
/// nothing, because a backward descent that has not reached this node's tip has staged
/// nothing that extends it. Both then close on the same chain, so the forward path is
/// a different order and not a different answer.
#[tokio::test]
async fn an_inventory_drives_a_forward_download_the_first_round_executes() {
    let served = served_chain();
    let target = served.last().expect("a served tip").header.chain_length;

    // Backward only, which is what every slice before this one did.
    let backward = tempfile::tempdir().expect("a directory");
    let burnchain = MovableBurnchain::new(captured_burnchain());
    let (mut executor, staging, _) = ready_node(backward.path(), &burnchain, &served).await;
    let (peer, task) = serve(Served::honest(served.clone(), snapshots())).await;
    let mut history = TenureSource::only(peer.clone());
    let descent = run(
        &mut executor,
        &peer,
        &mut history,
        &staging,
        &[],
        target,
    )
    .await;
    let descended = closed(&mut executor);
    task.abort();

    // Forward, off the same claims a swarm publishes.
    let forward = tempfile::tempdir().expect("a directory");
    let burnchain = MovableBurnchain::new(captured_burnchain());
    let (mut executor, staging, _) = ready_node(forward.path(), &burnchain, &served).await;
    let asked = Policy::default();
    let (peer, task) = serve(Served::honest(served.clone(), snapshots()).under(asked.clone())).await;
    let mut history = TenureSource::only(peer.clone());
    let claims = vec![claims_everything(1, peer.base_url().as_str())];
    let scheduled = run(
        &mut executor,
        &peer,
        &mut history,
        &staging,
        &claims,
        target,
    )
    .await;
    let scheduled_closed = closed(&mut executor);

    let first = scheduled.first().expect("a first round");
    assert!(
        first.scheduled > 0,
        "the first round took no tenure from the inventory, so nothing is being tested: {first:?}"
    );
    assert!(
        first.executed > 0,
        "the first inventory-driven round executed nothing: {first:?}"
    );
    assert_eq!(
        descent.first().expect("a first round").executed,
        0,
        "the backward descent executed on its first round under this budget, so the \
         forward gain is not what this test measures: {:?}",
        descent.first()
    );
    assert!(
        !asked.tenures_asked_by_view().is_empty(),
        "no tenure was asked for by the burn view that elected it, so the schedule made \
         no request of its own"
    );
    // The schedule anchors above the furthest block *staged*, not the furthest
    // executed, and this is what that is for. Anchored at the executed tip it
    // re-derived nearly the same window every round — a tip advances by a tenure while
    // the window is dozens of tenures long — so it asked again for tenures it had
    // already paid for. Twice is the honest allowance: once for the tenure straddling
    // the furthest staged block, which is held only in part, and once more for a round
    // that met it again after execution consumed it.
    let mut asked_for = std::collections::BTreeMap::<String, usize>::new();
    for view in asked.tenures_asked_by_view() {
        *asked_for.entry(view).or_default() += 1;
    }
    let repeated: Vec<(&String, &usize)> = asked_for
        .iter()
        .filter(|(_, times)| **times > 2)
        .collect();
    assert!(
        repeated.is_empty(),
        "the schedule asked for the same tenure over and over, so it is re-downloading \
         what it has already staged: {repeated:?}"
    );
    assert_eq!(
        scheduled_closed, descended,
        "the inventory-driven descent closed on a different chain from the backward one"
    );
    assert_eq!(
        scheduled_closed.height, target,
        "the inventory-driven descent stopped at {} of the peer's {target}",
        scheduled_closed.height
    );
    task.abort();
}

/// A peer that claimed nothing is asked only for tenures that do not exist.
///
/// Two peers holding the same chain, one of them claiming it, and the exact shape of
/// the answer is worth stating because a stronger one would be false. Every tenure that
/// *exists* goes to the claiming peer, which is what an inventory is for. The silent
/// peer is still asked sometimes — for the offsets whose burn block elected nobody,
/// where the claiming peer answers with an empty tenure and `TenureSource` does what it
/// does with any peer that cannot serve a request: asks somebody else. Removing that
/// fallback would mean a peer's inventory could stop nano fetching a tenure, which is
/// the one thing a claim must never be able to do.
///
/// So the measurement is a partition: every burn view the silent peer was asked about
/// is one the capture's own snapshots record as having no sortition.
#[tokio::test]
async fn a_peer_that_claimed_nothing_is_asked_only_for_absent_tenures() {
    let served = served_chain();
    let target = served.last().expect("a served tip").header.chain_length;
    let directory = tempfile::tempdir().expect("a directory");
    let burnchain = MovableBurnchain::new(captured_burnchain());
    let (mut executor, staging, _) = ready_node(directory.path(), &burnchain, &served).await;

    let claiming_log = Policy::default();
    let (claiming, claiming_task) =
        serve(Served::honest(served.clone(), snapshots()).under(claiming_log.clone())).await;
    let silent_log = Policy::default();
    let (silent, silent_task) =
        serve(Served::honest(served.clone(), snapshots()).under(silent_log.clone())).await;

    let claims = vec![claims_everything(2, claiming.base_url().as_str())];
    let mut history = TenureSource::new(vec![silent.clone(), claiming.clone()]);
    let rounds = run(
        &mut executor,
        &silent,
        &mut history,
        &staging,
        &claims,
        target,
    )
    .await;

    assert!(
        rounds.iter().any(|round| round.scheduled > 0),
        "no round scheduled a tenure, so nothing is being tested"
    );
    let claimed = claiming_log.tenures_asked_by_view();
    let fallback = silent_log.tenures_asked_by_view();
    assert!(
        !claimed.is_empty(),
        "the peer that claimed the cycle was never asked for a tenure by burn view"
    );
    // The views these peers really can serve: a tenure exists for them and its blocks
    // are in what both peers hold. Every other view the schedule names — a burn block
    // that elected nobody, or one above the served prefix — is one neither peer can
    // answer, so a request going to the second is the pool working and not the
    // inventory being ignored.
    let servable: Vec<String> = served
        .iter()
        .map(|block| block.header.consensus_hash.to_string())
        .collect();
    let wrongly_asked: Vec<&String> = fallback
        .iter()
        .filter(|view| servable.contains(view))
        .collect();
    assert!(
        wrongly_asked.is_empty(),
        "the peer that claimed nothing was asked for tenures the claiming peer holds: \
         {wrongly_asked:?}"
    );
    assert!(
        claimed.iter().any(|view| servable.contains(view)),
        "the claiming peer was asked only for tenures nobody holds, so the schedule did \
         not use its claim"
    );
    claiming_task.abort();
    silent_task.abort();
}

/// A peer answering a scheduled tenure with another tenure's blocks is refused.
///
/// The one substitution a fetch addressed by burn view is open to, and the reason it
/// needs its own check: a backward walk asks for a block by its own hash, so the
/// answer is self-verifying, while a tenure named by the view that elected it is not.
/// What refuses it is the view every Nakamoto block header states — this node's own
/// derivation on one side, the block's own header on the other, and no peer between
/// them.
#[tokio::test]
async fn a_tenure_answered_for_another_burn_view_is_refused() {
    let served = served_chain();
    let rows = snapshots();
    let (peer, task) = serve(
        Served::honest(served.clone(), snapshots()).lying_about_tenures(),
    )
    .await;
    // A burn view whose tenure this peer really does hold, so the refusal is about the
    // answer and not about the peer having nothing.
    let block = served.last().expect("a served tip");
    let view = block.header.consensus_hash;
    assert!(
        rows.iter()
            .any(|row| row.consensus_hash == view.to_string()),
        "the burn view under test is not in the capture"
    );

    let honest = serve(Served::honest(served, snapshots())).await;
    let good = honest
        .0
        .tenure_at(view)
        .await
        .expect("an honest peer answers the tenure it was asked for");
    assert!(
        good.iter()
            .all(|block| block.header.consensus_hash == view),
        "the honest answer carries another view's blocks, so the fixture is wrong"
    );
    honest.1.abort();

    let error = peer
        .tenure_at(view)
        .await
        .expect_err("a substituted tenure is refused");
    assert!(
        matches!(error, nano_sync::SyncError::UnexpectedTenure { asked, .. } if asked == view),
        "a tenure answered for another burn view was reported as {error}"
    );
    task.abort();
}
