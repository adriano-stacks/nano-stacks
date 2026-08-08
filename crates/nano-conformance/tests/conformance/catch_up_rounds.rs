//! Closing a gap through rounds, against a peer that behaves like a real one.
//!
//! `follow_path` drives `CheckpointExecutor::catch_up` against peers that are
//! either honest or lying. Neither is what stalled the live mainnet run: that
//! peer was honest and *slow* — it answered 429, it answered fewer blocks than
//! it was asked for, and its tip moved while a round was in flight. [[047]]'s
//! last unchecked item is a deterministic harness for exactly that.
//!
//! What is pinned here, and each one is its own claim:
//!
//! - a gap inside **one long tenure**, and a gap across **many tenures**, closed
//!   by repeated rounds against the same staging store;
//! - **429s**, both sprinkled deterministically through a run and for a whole
//!   round at a time: the round has to end *successfully*, having committed the
//!   progress it made, and the round after it has to carry on from the last
//!   block it sealed rather than walking the gap again;
//! - **short pages**: a peer whose tenure answers are cut to fewer blocks than
//!   the tenure holds;
//! - a **tip that moves mid-round**, revealed per request rather than between
//!   rounds, so the tip a round started from is stale before it finishes;
//! - a **restart at every chunk boundary**, reaching the same sealed root,
//!   executed tip and accounting as a run that was never interrupted;
//! - **monotonicity**, explicitly: the executed height never decreases, and the
//!   number of blocks executed across every round equals the height the chain
//!   advanced — so a block that was sealed and executed again would be counted.
//!
//! Nothing here waits on a clock. A refusal is decided by counting the requests
//! the node makes, so the same node meets the same refusals in the same places
//! on every run, and the peers answer `Retry-After: 0` so the client's own
//! retries cost the suite nothing.
//!
//! What this cannot pin is scale. The capture's longest tenure is 12 blocks and
//! its whole chain is 340, where the mainnet gap this task came from was 20,000
//! blocks and its tenures are bounded only by Bitcoin. A bug that needs a gap
//! deeper than a fetch budget of thousands, or a tenure longer than one
//! response, is not reachable from this fixture.

use std::{collections::BTreeMap, path::Path, sync::atomic::Ordering};

use nano_chainstate::NakamotoBlock;
use nano_node::{CatchUpBudget, CatchUpRound, CheckpointExecutor, staging::Staging};
use nano_primitives::TrieHash;
use nano_sync::{SyncClient, TenureSource};

use crate::follow_path::{
    MovableBurnchain, Policy, Served, captured_burnchain, captured_chain, node, pox, serve,
    snapshots,
};

/// The captured blocks a peer holds for the single-tenure gap.
///
/// Heights 461 to 470: the anchor, and then the whole nine-block tenure above
/// it. The gap is inside one tenure, which is the case a descent closes in a
/// single answer and execution then has to chunk through.
const ONE_TENURE: usize = 10;

/// The captured blocks a peer holds for the many-tenure gap.
///
/// Heights 461 to 504, which is seven tenures and includes the capture's longest
/// — twelve blocks, 493 to 504. A descent over this crosses tenure boundaries in
/// both directions: walking back to the executed tip, and executing forward
/// through the tenure changes that state each new burn view.
const MANY_TENURES: usize = 44;

/// How many rounds a scenario is given before it is called stuck.
///
/// Generous: the point of the bound is that a stalled catch-up fails the test
/// instead of hanging it, and the assertion prints the whole per-round history.
const ROUND_LIMIT: usize = 120;

/// How often the same tenure may be answered before the descent is re-walking.
///
/// Once for the page the descent passes through, and one more for the round that
/// meets the peer's tip inside a tenure that is still growing. A third would mean
/// a round asked for history it already held — the mainnet symptom.
const TENURE_ANSWERS: usize = 2;

/// What a run of rounds did, in the numbers monotonicity is a claim about.
#[derive(Debug, Default)]
struct Progress {
    rounds: usize,
    /// The executed height after each round, in order.
    heights: Vec<u64>,
    executed: usize,
    fetched: usize,
    /// Rounds the peer cut short, which have to be successful rounds.
    rate_limited: usize,
}

impl Progress {
    /// Take one round, refusing any round that went backwards.
    fn record(&mut self, round: &CatchUpRound, height: u64) {
        if let Some(previous) = self.heights.last() {
            assert!(
                height >= *previous,
                "round {} took the executed height from {previous} back to {height}",
                self.rounds
            );
        }
        self.rounds += 1;
        self.heights.push(height);
        self.executed += round.executed;
        self.fetched += round.fetched;
        self.rate_limited += usize::from(round.rate_limited);
    }
}

/// Everything a finished run can be compared by, on both sides of a restart.
///
/// Two roots, because they are two different facts about the same block. The
/// header's `state_index_root` is the one the network signed and the one the seal
/// refused the block for differing from; the MARF's *content* root for that block
/// is what this node's trie actually holds under it, with no skip-list over the
/// ancestry mixed in. A run that agreed about the signed root and disagreed about
/// the content it was computed over would be writing different state and reaching
/// the same header — which is what a comparison of tips alone would miss.
#[derive(Debug, Eq, PartialEq)]
struct Closed {
    tip: [u8; 32],
    height: u64,
    header_root: TrieHash,
    content_root: Option<TrieHash>,
    canonical: crate::restart::Canonical,
}

/// One run of rounds: the gap, the budget, the peer's manners.
struct Run<'a> {
    /// How many captured blocks the peer holds.
    served: usize,
    budget: CatchUpBudget,
    policy: &'a Policy,
    /// Close and reopen the chainstate after every round.
    restarting: bool,
    /// Refuse everything for every other round.
    refusing_alternate_rounds: bool,
}

impl<'a> Run<'a> {
    /// A run against a peer with these manners, uninterrupted.
    const fn new(served: usize, budget: CatchUpBudget, policy: &'a Policy) -> Self {
        Self {
            served,
            budget,
            policy,
            restarting: false,
            refusing_alternate_rounds: false,
        }
    }

    const fn restarting(mut self) -> Self {
        self.restarting = true;
        self
    }

    const fn refusing_alternate_rounds(mut self) -> Self {
        self.refusing_alternate_rounds = true;
        self
    }
}

/// Reopen a state directory the way a restarted node does.
///
/// `restart::open` recovers the ledger the sealed block committed, and the tip's
/// own block comes back from the chain — a running node asks a peer for it
/// (`resume_from`), and the fixture is the same bytes that peer serves.
fn resumed(
    directory: &Path,
    chain: &[NakamotoBlock],
    burnchain: MovableBurnchain,
) -> CheckpointExecutor<MovableBurnchain> {
    let (chainstate, _) = crate::restart::open(directory);
    let tip = chainstate
        .tip()
        .expect("read the sealed tip")
        .expect("the state is sealed at a block");
    let block = chain
        .iter()
        .find(|block| *block.block_id().as_bytes() == tip)
        .unwrap_or_else(|| {
            panic!(
                "the sealed tip {} is not a captured block",
                hex::encode(tip)
            )
        })
        .clone();
    let mut executor = CheckpointExecutor::resume(chainstate, block, burnchain);
    // The burn views this rig executes under are derived here, from the capture,
    // exactly as a node derives them from its checkpoint. Before 077 they came from
    // the peer, which is the path that no longer exists.
    nano_conformance::derive_sortitions(&mut executor, &crate::follow_path::fixtures(), directory);
    executor
}

/// The tenures a chain executed, as the heights `restart::canonical` compares by.
fn tenure_heights(
    executor: &mut CheckpointExecutor<MovableBurnchain>,
    blocks: &[NakamotoBlock],
) -> Vec<u32> {
    let chainstate = executor.chainstate_mut();
    let mut tenures: Vec<u32> = blocks
        .iter()
        .filter_map(|block| {
            chainstate
                .recorded_header(*block.block_id().as_bytes())
                .map(|header| header.tenure_height)
        })
        .collect();
    tenures.sort_unstable();
    tenures.dedup();
    tenures
}

/// What every finished run has to have done, whatever the peer did to it.
///
/// Three claims, and the last two are the item's counters. The chain is at the
/// peer's tip with nothing executable left behind; exactly as many blocks were
/// executed as the chain advanced, so a block that was sealed and then executed
/// again is counted rather than sampled; and no tenure was answered more than
/// `TENURE_ANSWERS` times, so a round that resumed from the peer's tip instead of
/// from the block it last sealed is counted too.
fn closed_the_gap(
    progress: &Progress,
    policy: &Policy,
    staging: &Staging,
    (anchor, target): (u64, u64),
    tip: &NakamotoBlock,
) {
    assert_eq!(
        tip.header.chain_length, target,
        "the node stopped at height {} of the peer's {target} after {} rounds: {progress:?}",
        tip.header.chain_length, progress.rounds
    );
    assert!(
        staging
            .child_of(tip.block_id())
            .expect("the staging store answers")
            .is_none(),
        "the node stopped with a child of its tip still staged, so it stopped short of \
         what it already held"
    );
    assert_eq!(
        progress.executed,
        usize::try_from(target - anchor).expect("the gap fits"),
        "{} blocks were executed to advance {} heights, so a sealed block was executed again",
        progress.executed,
        target - anchor
    );
    let mut answered: BTreeMap<String, usize> = BTreeMap::new();
    for tenure in policy.tenures_served() {
        *answered.entry(tenure).or_default() += 1;
    }
    for (tenure, answers) in &answered {
        assert!(
            *answers <= TENURE_ANSWERS,
            "tenure {tenure} was answered {answers} times over {} rounds, so the descent is \
             re-walking history the node already holds",
            progress.rounds
        );
    }
}

/// Close a gap of `served` captured blocks against one peer, round by round.
///
/// `restarting` closes and reopens the chainstate, the staging store and the peer
/// client after **every** round — which is after every committed chunk, since a
/// round seals at most `budget.execute` blocks and commits each one as it goes.
/// The client is rebuilt rather than kept so that its block cache cannot answer
/// for a peer that was never asked: what the peer was asked is the peer's own
/// record.
async fn close_the_gap(run: Run<'_>) -> (Progress, Closed) {
    let Run {
        served,
        budget,
        policy,
        restarting,
        refusing_alternate_rounds,
    } = run;
    let chain = captured_chain();
    let blocks: Vec<NakamotoBlock> = chain[..served].to_vec();
    let target = blocks
        .last()
        .expect("the peer serves a tip")
        .header
        .chain_length;
    let (mut client, task) =
        serve(Served::honest(blocks.clone(), snapshots()).under(policy.clone())).await;

    let directory = tempfile::tempdir().expect("a directory");
    let staging_path = directory.path().join("staging.sqlite");
    let burnchain = MovableBurnchain::new(captured_burnchain());
    let (mut executor, _) = node(directory.path(), burnchain.clone());
    let anchor = executor.tip().header.chain_length;
    assert!(
        target > anchor,
        "the peer serves nothing above the anchor, so there is no gap to close"
    );
    let mut staging = Staging::open(&staging_path).expect("staging opens");
    let mut history = TenureSource::only(client.clone());

    let mut progress = Progress::default();
    for round in 0..ROUND_LIMIT {
        // A whole round refused, then a whole round served. The alternation is
        // what makes a restart land on a round that ended early: a round the peer
        // cut short has to be followed by one that resumes from what it sealed.
        if refusing_alternate_rounds {
            policy
                .refusing
                .store(!round.is_multiple_of(2), Ordering::SeqCst);
        }
        let before = executor.tip().header.chain_length;
        let outcome = executor
            .catch_up(&client, &mut history, &pox(), &staging, budget, &[])
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "round {round} ended in an error at height {before} instead of committing \
                     what it had: {error}"
                )
            });
        let height = executor.tip().header.chain_length;
        progress.record(&outcome, height);
        if restarting {
            drop(executor);
            drop(staging);
            client = SyncClient::new(client.base_url().clone()).expect("a client");
            history = TenureSource::only(client.clone());
            executor = resumed(directory.path(), &chain, burnchain.clone());
            staging = Staging::open(&staging_path).expect("staging reopens");
            assert_eq!(
                executor.tip().header.chain_length,
                height,
                "round {round} sealed height {height} and the restart after it came back \
                 somewhere else"
            );
        }
        if height >= target {
            break;
        }
    }

    let tip = executor.tip().clone();
    closed_the_gap(&progress, policy, &staging, (anchor, target), &tip);

    let tenures = tenure_heights(&mut executor, &blocks);
    let closed = Closed {
        tip: *tip.block_id().as_bytes(),
        height: tip.header.chain_length,
        header_root: tip.header.state_index_root,
        content_root: executor
            .chainstate_mut()
            .state_content_root(*tip.block_id().as_bytes())
            .expect("read the closed content root"),
        canonical: crate::restart::canonical(executor.chainstate_mut(), &tenures),
    };
    task.abort();
    (progress, closed)
}

/// A gap inside one long tenure closes across rounds, monotonically.
///
/// The control for everything below it: an honest peer, and an execution budget
/// smaller than the tenure, so the gap is closed by several rounds against one
/// staging store rather than in a single pass. The tenure arrives whole in one
/// answer, which is what makes this the *execution* chunking case.
#[tokio::test]
async fn a_gap_inside_one_tenure_closes_across_rounds() {
    let budget = CatchUpBudget {
        fetch: 8,
        execute: 3,
    };
    let honest = Policy::default();
    let (progress, _) = close_the_gap(Run::new(ONE_TENURE, budget, &honest)).await;
    assert!(
        progress.rounds >= 3,
        "the gap closed in {} rounds, so the execution budget was not the bound and this \
         says nothing about chunking",
        progress.rounds
    );
    assert_eq!(
        progress.rate_limited, 0,
        "the honest peer refused something"
    );
}

/// A gap across many tenures closes under deterministic 429s and short pages.
///
/// Three of the item's conditions at once, on purpose: they interact. A short
/// page makes the descent ask more often, which makes it meet more refusals,
/// which makes more rounds end early — and the property has to hold through all
/// of it. Every fourth request is refused and no tenure answer carries more than
/// three blocks.
#[tokio::test]
async fn a_gap_across_many_tenures_closes_under_rate_limits_and_short_pages() {
    let policy = Policy::default().refusing_every(4).paged(3);
    let budget = CatchUpBudget {
        fetch: 8,
        execute: 5,
    };
    let (progress, _) = close_the_gap(Run::new(MANY_TENURES, budget, &policy)).await;
    assert!(
        policy.refusals() > 0,
        "the peer refused nothing, so the rate limit was not exercised"
    );
    assert!(
        progress.rounds > 1,
        "the gap closed in one round, so no round resumed from another's progress"
    );
}

/// A whole round of 429s ends successfully, and the next round carries on.
///
/// The mainnet failure, reduced: a peer that refuses everything for a round.
/// Driven a round at a time rather than through the driver above, because what
/// is being asserted is the *shape of one round* — that it returns `Ok`, that it
/// says it was rate limited, that it moved nothing, and that the state it leaves
/// is the one the round before it committed.
///
/// The refusal is placed after a round that executed, so what the next round has
/// to resume from is a block this node sealed rather than its anchor.
#[tokio::test]
async fn a_round_of_refusals_keeps_what_it_had_and_the_next_one_resumes() {
    let policy = Policy::default();
    let chain = captured_chain();
    let blocks: Vec<NakamotoBlock> = chain[..ONE_TENURE].to_vec();
    let target = blocks.last().expect("a tip").header.chain_length;
    let (client, task) = serve(Served::honest(blocks, snapshots()).under(policy.clone())).await;

    let directory = tempfile::tempdir().expect("a directory");
    let burnchain = MovableBurnchain::new(captured_burnchain());
    let (mut executor, _) = node(directory.path(), burnchain);
    let staging = Staging::open(&directory.path().join("staging.sqlite")).expect("staging opens");
    let mut history = TenureSource::only(client.clone());
    let budget = CatchUpBudget {
        fetch: 8,
        execute: 3,
    };

    let first = executor
        .catch_up(&client, &mut history, &pox(), &staging, budget, &[])
        .await
        .expect("the first round executes");
    assert!(
        first.executed > 0,
        "the first round executed nothing, so there is no progress for the refusal to keep"
    );
    let committed = executor.tip().block_id();
    let height = executor.tip().header.chain_length;
    let staged = staging.len().expect("the staging store answers");
    assert!(
        staged > 0,
        "nothing is staged for the next round to resume from"
    );

    // Now the peer refuses everything, for one whole round.
    policy.refusing.store(true, Ordering::SeqCst);
    let refused_before = policy.refusals();
    let refused_round = executor
        .catch_up(&client, &mut history, &pox(), &staging, budget, &[])
        .await
        .expect(
            "a round a peer refused outright came back as an error, which is the mainnet \
             stall: the round is discarded and the next one starts from the peer's tip again",
        );
    assert!(
        policy.refusals() > refused_before,
        "the peer refused nothing during the round it was told to refuse everything"
    );
    assert!(
        refused_round.rate_limited,
        "the round did not report the rate limit, so the loop around it cannot tell a \
         throttled peer from an idle one"
    );
    assert_eq!(
        refused_round.fetched, 0,
        "the refused round fetched blocks from a peer that was refusing everything"
    );
    // It does execute, and that is the property rather than an accident: the
    // blocks were already on disk and the context for the burn view they stand on
    // was already in hand, so a peer that has stopped answering does not stop a
    // node from sealing what it holds. What it must not do is go backwards or
    // throw the descent away.
    assert!(
        executor.tip().header.chain_length >= height,
        "the refused round took the executed tip below the block the round before it \
         committed"
    );
    assert_eq!(
        u64::from(u32::try_from(refused_round.executed).expect("a chunk fits")),
        executor.tip().header.chain_length - height,
        "the refused round's own count does not match the heights it moved"
    );
    assert!(
        executor.tip().header.chain_length > height || executor.tip().block_id() == committed,
        "the tip changed without the round executing anything"
    );
    assert_eq!(
        staging.len().expect("the staging store answers")
            + u64::try_from(refused_round.executed).expect("a chunk fits"),
        staged,
        "the refused round threw away the descent the round before it paid for"
    );

    // And the peer relents. The next round resumes from the block above the one
    // this node sealed — not from the peer's tip, and not from the anchor.
    policy.refusing.store(false, Ordering::SeqCst);
    let mut executed = first.executed + refused_round.executed;
    for round in 0..ROUND_LIMIT {
        let outcome = executor
            .catch_up(&client, &mut history, &pox(), &staging, budget, &[])
            .await
            .unwrap_or_else(|error| panic!("round {round} after the refusal failed: {error}"));
        executed += outcome.executed;
        assert!(
            executor.tip().header.chain_length >= height,
            "a round after the refusal went below the height the refused round left"
        );
        if executor.tip().header.chain_length >= target {
            break;
        }
    }
    assert_eq!(
        executor.tip().header.chain_length,
        target,
        "the node never reached the peer's tip after the refusal"
    );
    assert_eq!(
        executed,
        usize::try_from(target - chain[0].header.chain_length).expect("the gap fits"),
        "the rounds after the refusal executed blocks the refused round had already sealed"
    );

    task.abort();
}

/// A peer that throttles the descent is asked again on the next round.
///
/// The other half of a rate limit, and the one that cost the mainnet run
/// everything: a peer that still answers where its tip is and refuses the history
/// below it. That is the only way a descent reaches the throttle bookkeeping at
/// all — a peer refusing *everything* is never asked for a tenure — and the
/// bookkeeping is per pool and outlives the round that learned it.
///
/// Two claims, and the first is what makes the second mean anything: the round
/// really did set the peer aside, and the round after it asked that peer again. A
/// throttle kept across rounds left `TenureSource` with nobody to ask for the rest
/// of the process, and every later round answered "no peer left to ask" before
/// executing a single block of the twenty thousand it had on disk.
#[tokio::test]
async fn a_peer_that_throttles_the_descent_is_asked_again_next_round() {
    let policy = Policy::default();
    let chain = captured_chain();
    let blocks: Vec<NakamotoBlock> = chain[..ONE_TENURE].to_vec();
    let target = blocks.last().expect("a tip").header.chain_length;
    let (client, task) = serve(Served::honest(blocks, snapshots()).under(policy.clone())).await;

    let directory = tempfile::tempdir().expect("a directory");
    let burnchain = MovableBurnchain::new(captured_burnchain());
    let (mut executor, _) = node(directory.path(), burnchain);
    let staging = Staging::open(&directory.path().join("staging.sqlite")).expect("staging opens");
    let mut history = TenureSource::only(client.clone());
    let budget = CatchUpBudget {
        fetch: 8,
        execute: 3,
    };

    policy.refusing_tenures.store(true, Ordering::SeqCst);
    let throttled = executor
        .catch_up(&client, &mut history, &pox(), &staging, budget, &[])
        .await
        .expect("a peer that will not serve history is not a failed round");
    assert!(
        throttled.rate_limited,
        "the round did not notice the descent being refused"
    );
    assert_eq!(throttled.fetched, 0, "the refused descent fetched blocks");
    assert_eq!(
        history.throttled(),
        1,
        "the round did not set the peer aside, so nothing here is about forgiving it"
    );
    assert_eq!(
        executor.tip().header.chain_length,
        chain[0].header.chain_length,
        "the round executed a block it could not have fetched"
    );

    policy.refusing_tenures.store(false, Ordering::SeqCst);
    let mut executed = 0;
    for round in 0..ROUND_LIMIT {
        let outcome = executor
            .catch_up(&client, &mut history, &pox(), &staging, budget, &[])
            .await
            .unwrap_or_else(|error| panic!("round {round} after the throttle failed: {error}"));
        if round == 0 {
            assert!(
                outcome.fetched > 0,
                "the round after the throttle fetched nothing from a peer that had started \
                 answering again, so the pool was set aside for good"
            );
        }
        executed += outcome.executed;
        if executor.tip().header.chain_length >= target {
            break;
        }
    }
    assert_eq!(
        executor.tip().header.chain_length,
        target,
        "the node never reached the peer's tip after the throttle"
    );
    assert_eq!(
        executed,
        usize::try_from(target - chain[0].header.chain_length).expect("the gap fits"),
        "the rounds after the throttle executed a block twice"
    );

    task.abort();
}

/// A tip that moves while a round is in flight is followed, not re-walked.
///
/// The peer reveals one more block every third request, so the tip a round read
/// at its start is already stale by the time that round descends — and it moves
/// again while the round executes. Nothing about this is timed: it is the node's
/// own request count that moves the peer.
#[tokio::test]
async fn a_tip_that_moves_mid_round_is_followed() {
    // Two blocks visible to begin with: the anchor, and one above it for the
    // first round to have something to do.
    let policy = Policy::default().revealing(2, 3);
    let budget = CatchUpBudget {
        fetch: 8,
        execute: 4,
    };
    let (progress, _) = close_the_gap(Run::new(MANY_TENURES, budget, &policy)).await;
    assert!(
        progress.rounds > 1,
        "the whole chain was visible at once, so the tip never moved"
    );
}

/// A restart after every committed chunk reaches the state an unbroken run does.
///
/// The two runs differ in one thing: the second one closes the chainstate, the
/// staging store and the peer client after every round and opens them again. A
/// round commits each block it seals, so "after every round" is after every chunk
/// boundary — and with an execution budget of three over a nine-block gap there
/// are several of them.
///
/// Everything is compared, not just the tip: the root the header commits to, the
/// root the MARF answers for that block, the executed suffix a reorganization
/// would walk, the accounting, the parent tenure proof, and both copies of every
/// tenure's start height. A restart that recovered any one of them differently
/// would seal a different root at the next block, and this is where that shows.
#[tokio::test]
async fn a_restart_at_every_chunk_boundary_reaches_the_same_state() {
    let budget = CatchUpBudget {
        fetch: 8,
        execute: 3,
    };
    let honest = Policy::default();
    let (straight, unbroken) = close_the_gap(Run::new(ONE_TENURE, budget, &honest)).await;
    let honest = Policy::default();
    let (across, restarted) =
        close_the_gap(Run::new(ONE_TENURE, budget, &honest).restarting()).await;
    assert!(
        straight.rounds >= 3 && across.rounds >= 3,
        "one of the runs closed the gap in {} and {} rounds, so there were no chunk \
         boundaries to restart at",
        straight.rounds,
        across.rounds
    );
    assert_eq!(
        restarted, unbroken,
        "a run restarted at every chunk boundary reached a different state from the one \
         that was never interrupted"
    );
}

/// The same, with the peer refusing and paging as well.
///
/// Kept separate from the run above because it answers a different question. That
/// one asks whether a restart is lossless; this asks whether a restart is lossless
/// *while* rounds are ending early — which is the combination a mainnet catch-up
/// is in for hours, and the one where a round that ends on a 429 and a round that
/// ends on a process stop can lose the same progress twice.
#[tokio::test]
async fn a_restart_at_every_chunk_boundary_survives_rate_limits_and_short_pages() {
    let budget = CatchUpBudget {
        fetch: 6,
        execute: 4,
    };
    let policy = || Policy::default().paged(2);
    let straight = policy();
    let (_, unbroken) =
        close_the_gap(Run::new(ONE_TENURE, budget, &straight).refusing_alternate_rounds()).await;
    let across = policy();
    let (progress, restarted) = close_the_gap(
        Run::new(ONE_TENURE, budget, &across)
            .restarting()
            .refusing_alternate_rounds(),
    )
    .await;
    assert!(
        straight.refusals() > 0 && across.refusals() > 0,
        "neither peer refused anything, so this is the previous test again"
    );
    assert!(
        progress.rate_limited > 0,
        "no round ended on the rate limit, so the restart never landed on one"
    );
    assert_eq!(
        restarted, unbroken,
        "a restart during a rate-limited catch-up reached a different state"
    );
}
