//! Catching up while the peers misbehave, restarting after every chunk.
//!
//! [[047]]'s claim is that executed height only ever goes up, and that stopping a
//! node at any committed block and starting it again reaches the state an
//! uninterrupted run reaches. The live mainnet run exercises that constantly —
//! public peers rate-limit, bound their response bodies and gain blocks while a
//! descent is walking them — and nothing offline pinned any of it. That is the last
//! open item on the task, and this is it.
//!
//! `follow_path` already stands the whole environment up in about a second: the
//! captured chain as loopback Stacks peers, its Bitcoin blocks as a burnchain the
//! test holds, a state directory imported from `chainstate/checkpoint-H`. What is
//! added here is a peer that
//!
//! - **answers 429 with `Retry-After`**, a bounded number of times, wherever in the
//!   round the budget happens to run out — tenure info, a tenure, a block, a
//!   sortition or a burn view. nano honours the peer's own answer
//!   ([[048]] records why: capping a peer's minute at two seconds earned another
//!   429 indefinitely), so a refused request costs the round time and not progress;
//! - **bounds its pages**, ending a descent step inside a tenure rather than at its
//!   first block, so a round finishes short of the tip and the next one resumes;
//! - **gains blocks as it is asked**, so the tip moves while a round is in flight.
//!   Deterministically, which a background task moving it would not be: the peer
//!   grows *because* it was asked for a tenure, which is also what a real one does.
//!
//! And the node is restarted between **every** execution chunk: the state directory
//! is closed and reopened through the same two branches `runtime::open_chainstate`
//! takes, so each chunk boundary is a real recovery from what the last chunk
//! committed, not a continuation in memory.
//!
//! Then the assertion is [[047]]'s acceptance criteria and nothing softer: the
//! executed height never goes backwards, and the final root, the accounting and the
//! executed suffix are the ones a single uninterrupted run reaches.

use std::{
    fs,
    path::Path,
    sync::{Arc, atomic::Ordering},
};

use nano_chainstate::{ChainState, NakamotoBlock};
use nano_node::{CatchUpBudget, CheckpointExecutor, staging::Staging};
use nano_sync::{SyncClient, TenureSource};

use crate::follow_path::{
    MovableBurnchain, Pressure, Served, captured_burnchain, captured_chain, fixtures, pox,
    serve_shared, snapshots,
};

/// How much of the captured chain the peers eventually hold.
///
/// Enough to span several tenures and at least one long one, which the test
/// asserts rather than assumes — the gap [[047]] is about is a gap of *tenures*,
/// and a run inside one tenure would say nothing about the tenure-change path a
/// resumed node has to walk back to.
const BLOCKS: usize = 40;

/// How much of it they hold to begin with, before they start gaining blocks.
const FIRST: usize = 6;

/// How many 429s each peer will hand out before it starts answering.
///
/// Bounded because a peer that refuses forever is a peer that is down, which is a
/// different test; and spent one at a time rather than by a modulus over a request
/// counter, because two peers and a retrying client make the request count depend
/// on timing.
const RATE_LIMITS: usize = 4;

/// A ceiling on the rounds, so a stalled follower fails instead of hanging.
const ROUNDS: usize = 200;

/// Open the state in `directory`, importing the checkpoint the first time and
/// resuming what is on disk after that.
///
/// The two branches are `runtime::open_chainstate`'s, deliberately: a first start
/// takes the checkpoint's accounting because only the checkpoint knows what the
/// first tenures still owe, and every later start recovers the ledger committed
/// with the sealed tip and is handed nothing. Getting that wrong is how a restart
/// silently forks a node, so the resume branch asserts a ledger *was* recovered
/// rather than falling back to a file.
fn open(
    directory: &Path,
    chain: &[NakamotoBlock],
    burnchain: MovableBurnchain,
) -> CheckpointExecutor<MovableBurnchain> {
    let fixtures = fixtures();
    let checkpoint = fixtures.join("chainstate/checkpoint-H");
    let manifest =
        nano_node::CheckpointManifest::load(&checkpoint).expect("the checkpoint manifest reads");
    let mut chainstate = ChainState::open_from_checkpoint(
        nano_conformance::captured_network(&fixtures),
        directory,
        checkpoint.join("marf.sqlite"),
        manifest.source_state_id,
        manifest.state_index_root,
    )
    .expect("the checkpoint opens");

    if let Some(tip) = chainstate
        .tip()
        .filter(|tip| *tip != manifest.source_state_id)
    {
        assert!(
            chainstate
                .recover_ledger_at(tip)
                .expect("the ledger reads back"),
            "the block this state is sealed at committed no ledger, so a resumed \
             run would owe whatever the last round happened to write"
        );
        let sealed = chain
            .iter()
            .find(|block| *block.block_id().as_bytes() == tip)
            .expect("the sealed tip is a block of the captured chain")
            .clone();
        return CheckpointExecutor::resume(chainstate, sealed, burnchain);
    }

    let accounting = fs::read(checkpoint.join("native-effects.json"))
        .ok()
        .and_then(|contents| nano_chainstate::TenureAccounting::from_json(&contents).ok())
        .expect("the checkpoint carries accounting");
    *chainstate.accounting_mut() = accounting;
    let anchor = chain.first().expect("the capture has blocks").clone();
    let context = *nano_conformance::captured_bitcoin_snapshots(&fixtures)
        .expect("the captured snapshots read")
        .get(&anchor.header.consensus_hash.to_string())
        .expect("the anchor's own burn block");
    CheckpointExecutor::from_chainstate(chainstate, anchor, context, burnchain)
        .expect("the anchor block applies")
}

/// What a run ends holding, as one comparable value.
///
/// Field by field, because which one differs names what went wrong: a differing
/// root is the MARF or the write order, a differing accounting is a fee or a
/// maturity that a restart lost, and a differing executed suffix is a node that
/// cannot walk a reorganization back as far as the run beside it could.
#[derive(Debug, Eq, PartialEq)]
struct Reached {
    tip: [u8; 32],
    root: Option<nano_marf::StateRoot>,
    accounting: Vec<u8>,
    executed: Vec<[u8; 32]>,
}

fn reached(executor: &mut CheckpointExecutor<MovableBurnchain>) -> Reached {
    let tip = *executor.tip().block_id().as_bytes();
    let chainstate = executor.chainstate_mut();
    Reached {
        tip,
        root: chainstate.vm_mut().root(tip),
        accounting: chainstate
            .accounting_mut()
            .to_json()
            .expect("encode the accounting"),
        executed: chainstate.executed_blocks(),
    }
}

/// What one run of bounded rounds did, beside where it ended.
struct Progress {
    reached: Reached,
    /// The executed height after every round, which is the monotonic sequence.
    heights: Vec<u64>,
    rounds: usize,
    /// Rounds that ended in an error rather than a result. A rate limit ends a
    /// round *successfully*, so these are counted separately and expected to be
    /// few — a run that errored on most rounds would be making progress for some
    /// other reason.
    failed: usize,
    /// Rounds the peers asked this node to slow down in.
    limited: usize,
}

/// Run bounded catch-up rounds until the peers' tip is reached.
///
/// `restart` closes and reopens the state between every round, which is what makes
/// each chunk boundary a recovery. The executed height is read from the executor's
/// own tip after each round — never from a peer, never from staging — and asserted
/// non-decreasing on the spot, so a failure names the round it happened in rather
/// than showing up as a wrong final answer.
async fn follow(
    directory: &Path,
    clients: &[SyncClient],
    chain: &[NakamotoBlock],
    budget: CatchUpBudget,
    restart: bool,
) -> Progress {
    let staging = Staging::open(&directory.join("staging.sqlite")).expect("staging opens");
    let burnchain = MovableBurnchain::new(captured_burnchain());
    let mut history = TenureSource::new(clients.to_vec());
    let node = clients.first().expect("a peer").clone();
    let target = *chain
        .last()
        .expect("the chain has a tip")
        .block_id()
        .as_bytes();

    let mut held = None;
    let mut heights = Vec::new();
    let mut progress = Progress {
        reached: Reached {
            tip: [0; 32],
            root: None,
            accounting: Vec::new(),
            executed: Vec::new(),
        },
        heights: Vec::new(),
        rounds: 0,
        failed: 0,
        limited: 0,
    };
    for round in 0..ROUNDS {
        let mut executor =
            held.take()
                .unwrap_or_else(|| open(directory, chain, burnchain.clone()));
        let before = executor.tip().header.chain_length;
        match executor
            .catch_up(&node, &mut history, &pox(), &staging, budget)
            .await
        {
            Ok(round) if round.rate_limited => progress.limited += 1,
            Ok(_) => {}
            // Not a fatal outcome: a peer that refused mid-execution ends the
            // round, and the blocks sealed before it are committed. What the run
            // has to keep is monotonicity, which the assertion below checks
            // whichever way the round ended.
            Err(error) => {
                progress.failed += 1;
                assert!(
                    round + 1 < ROUNDS,
                    "the last round failed with {error}, so the run did not finish"
                );
            }
        }
        let after = executor.tip().header.chain_length;
        assert!(
            after >= before,
            "round {round}: the executed height went from {before} back to {after}"
        );
        heights.push(after);
        progress.rounds = round + 1;
        if *executor.tip().block_id().as_bytes() == target {
            progress.reached = reached(&mut executor);
            progress.heights = heights;
            return progress;
        }
        if !restart {
            held = Some(executor);
        }
    }
    panic!(
        "the follower did not reach the peers' tip in {ROUNDS} rounds; it got to height {:?}",
        heights.last()
    );
}

/// Two peers over the same chain, one under pressure and one not.
///
/// Both are needed, and which one is which matters: `TenureSource` moves a tenure
/// to another peer when one throttles, so a pool where every member refuses at once
/// tests the waiting and a pool where one is willing tests the spreading. Here both
/// carry a 429 budget, so the round meets each in turn.
async fn peers(
    chain: &[NakamotoBlock],
    pressure: bool,
) -> (Vec<Arc<Served>>, Vec<SyncClient>, Vec<tokio::task::JoinHandle<()>>) {
    let mut served = Vec::new();
    let mut clients = Vec::new();
    let mut tasks = Vec::new();
    for peer in 0..2 {
        let state = Arc::new(Served {
            blocks: chain.to_vec(),
            snapshots: snapshots(),
            pressure: if pressure {
                Pressure {
                    rate_limits: RATE_LIMITS.into(),
                    // Honoured as given, so it has to be a second a test can
                    // afford. What is under test is that the wait happens and the
                    // round survives it, not how long it is.
                    retry_after: Some(1),
                    // Two blocks a response, so a tenure of more than two ends a
                    // descent step inside itself. The peers differ by one so the
                    // round meets both a page that divides a tenure evenly and one
                    // that does not.
                    page: Some(2 + peer),
                    visible: std::sync::Mutex::new(Some(FIRST)),
                    growth: 2,
                    ..Pressure::default()
                }
            } else {
                Pressure::default()
            },
        });
        let (client, task) = serve_shared(Arc::clone(&state)).await;
        served.push(state);
        clients.push(client);
        tasks.push(task);
    }
    (served, clients, tasks)
}

/// Peers that refuse everything until told to stop, and answer nothing meanwhile.
///
/// No `Retry-After`, so the client's own bounded backoff applies and a sweep of the
/// pool costs seconds rather than the minutes a header may legitimately ask for.
/// Disarmed to begin with, so the caller decides which round meets them.
async fn refusing(
    chain: &[NakamotoBlock],
) -> (Vec<Arc<Served>>, Vec<SyncClient>, Vec<tokio::task::JoinHandle<()>>) {
    let mut served = Vec::new();
    let mut clients = Vec::new();
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let state = Arc::new(Served::honest(chain.to_vec(), snapshots()));
        let (client, task) = serve_shared(Arc::clone(&state)).await;
        served.push(state);
        clients.push(client);
        tasks.push(task);
    }
    (served, clients, tasks)
}

/// A round every peer rate-limits keeps what it staged, and the next one asks again.
///
/// The shape [[047]] names — "rate limits and bounded peer pages end a round
/// successfully after all available progress is committed" — at the one point it is
/// hardest to reach: a descent that has *not* finished, so the round has no choice
/// but to ask a peer, and every peer refuses.
///
/// The followed peer answers everything, which is not a convenience: in a running
/// node `node` is the peer the fork choice took and `history` is the pool discovery
/// found, and they are frequently disjoint. Refusing on both at once would only
/// prove that a round whose *first* request fails returns, which is uninteresting
/// because nothing has been done yet to lose.
#[tokio::test]
async fn a_round_every_peer_rate_limits_keeps_what_it_staged() {
    let chain: Vec<NakamotoBlock> = captured_chain().into_iter().take(BLOCKS).collect();
    let (_, followed, followed_tasks) = peers(&chain, false).await;
    let (pool, clients, tasks) = refusing(&chain).await;
    let node = followed.first().expect("a followed peer").clone();

    let directory = tempfile::tempdir().expect("a directory");
    let staging = Staging::open(&directory.path().join("staging.sqlite")).expect("staging opens");
    let burnchain = MovableBurnchain::new(captured_burnchain());
    let mut executor = open(directory.path(), &chain, burnchain);
    let mut history = TenureSource::new(clients);
    // A fetch budget far smaller than the gap, so the descent cannot finish and the
    // round after it has to ask somebody.
    let budget = CatchUpBudget {
        fetch: 8,
        execute: 8,
    };

    let opened = executor
        .catch_up(&node, &mut history, &pox(), &staging, budget)
        .await
        .expect("a round over willing peers");
    let staged = opened.staged;
    assert!(
        staged > 0 && opened.executed == 0,
        "the first round staged {staged} blocks and executed {}; this test needs a \
         descent that has started and not reached this node's tip",
        opened.executed
    );

    for peer in &pool {
        peer.pressure.rate_limits.store(64, Ordering::SeqCst);
    }
    let tip = *executor.tip().block_id().as_bytes();
    let refused = executor
        .catch_up(&node, &mut history, &pox(), &staging, budget)
        .await
        .expect("a round in which every peer rate limits is not a failed round");
    assert!(
        refused.rate_limited,
        "the round did not report being rate limited, so it did not reach the pool"
    );
    assert_eq!(
        refused.staged, staged,
        "the round discarded the descent it had already staged"
    );
    assert_eq!(
        *executor.tip().block_id().as_bytes(),
        tip,
        "the executed tip moved in a round that fetched nothing"
    );

    // And the pool is asked again. This is the half that used to fail: every peer
    // had been set aside and nothing in a running node ever puts one back, so the
    // descent stopped here for as long as the process lived.
    for peer in &pool {
        peer.pressure.rate_limits.store(0, Ordering::SeqCst);
    }
    let resumed = executor
        .catch_up(&node, &mut history, &pox(), &staging, budget)
        .await
        .expect("a round after the limits lift");
    assert!(
        resumed.staged > staged,
        "the descent staged {} blocks after the limits lifted, against {staged} before: \
         the pool never asked its peers again",
        resumed.staged
    );

    for task in followed_tasks.into_iter().chain(tasks) {
        task.abort();
    }
}

/// The tenures the gap being closed spans, and the longest of them.
fn tenures(chain: &[NakamotoBlock]) -> (usize, usize) {
    let mut spans: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for block in chain {
        *spans
            .entry(block.header.consensus_hash.to_string())
            .or_default() += 1;
    }
    (spans.len(), spans.values().copied().max().unwrap_or_default())
}

/// Executed height only goes up, and a restart at every chunk reaches the same state.
#[tokio::test]
async fn a_pressured_catch_up_advances_monotonically_and_resumes_to_the_same_state() {
    let chain: Vec<NakamotoBlock> = captured_chain().into_iter().take(BLOCKS).collect();
    assert_eq!(chain.len(), BLOCKS, "the capture is shorter than the gap");
    let (spanned, longest) = tenures(&chain);
    assert!(
        spanned >= 3 && longest >= 5,
        "the gap spans {spanned} tenures, the longest {longest} blocks — [[047]] is about \
         a gap of tenures, and a run inside one says nothing about resuming across a \
         tenure change"
    );

    // The control: one process, one executor, the whole gap, peers that answer
    // everything at once. Without it "the same state" is unanchored.
    let uninterrupted = tempfile::tempdir().expect("a directory");
    let (_, honest, honest_tasks) = peers(&chain, false).await;
    let whole = follow(
        uninterrupted.path(),
        &honest,
        &chain,
        CatchUpBudget {
            fetch: 256,
            execute: 256,
        },
        false,
    )
    .await;
    assert_eq!(
        whole.failed, 0,
        "the uninterrupted run over honest peers failed a round, so it is not a control"
    );

    // The same gap, over peers that rate-limit, page short and grow, with the
    // state closed and reopened between every chunk.
    let pressured = tempfile::tempdir().expect("a directory");
    let (under, clients, tasks) = peers(&chain, true).await;
    let broken = follow(
        pressured.path(),
        &clients,
        &chain,
        // Three blocks a chunk, so the run crosses tenure boundaries mid-chunk and
        // lands on them between chunks; and a fetch budget smaller than the gap, so
        // the descent itself is spread over rounds rather than completed in one.
        CatchUpBudget {
            fetch: 8,
            execute: 3,
        },
        true,
    )
    .await;

    // The pressure was applied, not merely configured. Each of these has been zero
    // at some point while the test still passed, which is why they are asserted.
    let handed_out: usize = under
        .iter()
        .map(|peer| peer.pressure.limited.load(Ordering::SeqCst))
        .sum();
    let shortened: usize = under
        .iter()
        .map(|peer| peer.pressure.shortened.load(Ordering::SeqCst))
        .sum();
    let grew: usize = under
        .iter()
        .map(|peer| peer.pressure.grew.load(Ordering::SeqCst))
        .sum();
    assert_eq!(
        handed_out,
        RATE_LIMITS * under.len(),
        "the peers did not hand out the 429s they were given to hand out"
    );
    assert!(shortened > 0, "no response was ever bounded");
    assert!(grew > 0, "the peers' tip never moved");
    assert!(
        broken.rounds > whole.rounds,
        "the pressured run took {} rounds against the control's {}, so it was not \
         chunked and nothing was restarted between chunks",
        broken.rounds,
        whole.rounds
    );

    // Monotonic, and asserted over the whole recorded sequence rather than only at
    // the ends: a run that went backwards and recovered would pass a comparison of
    // first and last.
    assert!(
        broken.heights.windows(2).all(|pair| pair[1] >= pair[0]),
        "the executed height went backwards: {:?}",
        broken.heights
    );
    assert!(
        broken.heights.iter().rev().take(2).all(|height| *height > 0),
        "the run reported no executed height at all: {:?}",
        broken.heights
    );

    // And it ends where the run that was never interrupted ended.
    assert_eq!(
        broken.reached, whole.reached,
        "a catch-up restarted at every chunk boundary under rate limits and bounded \
         pages reached a different state from an uninterrupted one"
    );
    assert!(
        broken.reached.root.is_some(),
        "the run sealed no root, so the comparison above compared two absences"
    );
    println!(
        "{BLOCKS} blocks over {spanned} tenures: {} chunks with a restart after each, \
         {handed_out} rate limits ({} rounds ended in one), {shortened} bounded pages, \
         {grew} tip movements, {} rounds ended in an error",
        broken.rounds, broken.limited, broken.failed
    );

    for task in honest_tasks.into_iter().chain(tasks) {
        task.abort();
    }
}
