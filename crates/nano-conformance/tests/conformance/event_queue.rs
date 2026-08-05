//! A queued observer still sees every block, in order, and never holds up one.
//!
//! `event_observer` checks the *shape* of a `new_block` payload against the one
//! stacks-core published, and `event_delivery` checks that a payload reaches a
//! listener at all. Neither says anything about what happens when a whole replay
//! runs through the dispatcher: events are queued now rather than posted inline,
//! and a queue is exactly the place where a stream loses its order, loses a
//! member, or quietly stops.
//!
//! So this replays the captured blocks, hands every executed block's payload to
//! the dispatcher, and asserts the sink received precisely the payloads that were
//! dispatched, in the order they were dispatched, with contiguous sequence
//! numbers and nothing dropped. Ordering is not a nicety: the Hiro API and
//! `stacks-signer` both consume `new_block` as a chain, and a child that arrives
//! before its parent is not applicable.
//!
//! The sink is deliberately *slow* — 5 ms per request against a replay that
//! executes far faster — so the drain runs behind the executor throughout, which
//! is the condition under which order and completeness are actually at risk. The
//! same run measures what the replay loop pays for dispatching: an observer that
//! answers in its own time must cost execution nothing.
//!
//! Payload *fields* are not re-checked here; `event_observer` owns that. What is
//! checked is that the bytes handed to `dispatch` are the bytes that arrive.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Router,
    extract::{Path as UrlPath, State},
    http::HeaderMap,
    routing::post,
};
use nano_conformance::replay_captured_blocks;
use nano_rpc::{BlockEventContext, EventDispatcher, EventKind, new_block_payload};
use serde_json::Value;

/// How long the sink takes to answer one POST.
///
/// Enough that the drain cannot keep up with the replay — 340 blocks is 1.7 s of
/// sink time against a replay that dispatches in microseconds — so the queue is
/// never empty and the properties under test are under load.
const SINK_DELAY: Duration = Duration::from_millis(5);

/// What the replay may spend handing every block to the dispatcher.
///
/// The whole point of the queue: 340 events at the inline path's cost (five
/// attempts with 0/100/200/300/400 ms of backoff against an observer that does
/// not answer) would be minutes. Serializing them is all this may be.
const DISPATCH_BUDGET: Duration = Duration::from_secs(1);

/// How long the queue is given to empty after the replay ends.
const PATIENCE: Duration = Duration::from_mins(2);

/// One POST the sink received.
#[derive(Clone, Debug)]
struct Post {
    path: String,
    payload: Value,
    sequence: u64,
    dropped: u64,
}

type Received = Arc<Mutex<Vec<Post>>>;

/// An HTTP sink that records every event it is sent, `SINK_DELAY` per request.
async fn sink() -> (String, Received) {
    let received: Received = Arc::default();
    let app = Router::new()
        .route(
            "/{event}",
            post(
                |State(received): State<Received>,
                 UrlPath(event): UrlPath<String>,
                 headers: HeaderMap,
                 body: String| async move {
                    tokio::time::sleep(SINK_DELAY).await;
                    let number = |name: &str| {
                        headers
                            .get(name)
                            .and_then(|value| value.to_str().ok())
                            .and_then(|value| value.parse().ok())
                            .unwrap_or_else(|| panic!("every POST carries {name}"))
                    };
                    received.lock().expect("the record").push(Post {
                        path: event,
                        payload: serde_json::from_str(&body).expect("a JSON payload"),
                        sequence: number("x-nano-event-seq"),
                        dropped: number("x-nano-events-dropped"),
                    });
                },
            ),
        )
        .with_state(received.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the sink");
    let address = listener.local_addr().expect("the sink's address");
    tokio::spawn(async move { axum::serve(listener, app).await });
    (format!("http://{address}/"), received)
}

fn replay_blocks() -> (std::path::PathBuf, u64) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let blocks = nano_conformance::FixtureManifest::load(&root.join("manifest.toml"))
        .expect("the fixture manifest")
        .replay_blocks;
    (root, blocks)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_observer_receives_every_replayed_block_in_order() {
    let (url, received) = sink().await;
    let dispatcher = EventDispatcher::new(vec![url.parse().expect("the sink's URL")]);

    let (root, blocks) = replay_blocks();
    let mut sent: Vec<Value> = Vec::new();
    let mut dispatching = Duration::ZERO;
    let depth = replay_captured_blocks(&root, blocks, &mut |block, applied| {
        // The same call the executor makes for a block it has just sealed.
        let payload = new_block_payload(block, applied, &BlockEventContext::default());
        let started = Instant::now();
        dispatcher.dispatch(EventKind::NewBlock, &payload);
        dispatching += started.elapsed();
        sent.push(payload);
    });
    assert_eq!(
        depth.completed, blocks,
        "replay stopped early: {:?}",
        depth.first_divergence
    );

    // An observer answering in its own time costs the executor nothing. Asserted
    // before the drain is waited on, because after it the two are indistinguishable.
    assert!(
        dispatching < DISPATCH_BUDGET,
        "dispatching {} blocks took {dispatching:?}, so execution waited on the observer",
        sent.len()
    );
    assert!(
        dispatching * 10 < SINK_DELAY * u32::try_from(sent.len()).expect("a small count"),
        "dispatching cost the same order as the sink's own time: {dispatching:?}"
    );

    assert!(
        dispatcher.settle(PATIENCE).await,
        "the queue never emptied: {:?}",
        dispatcher.status()
    );
    let received = received.lock().expect("the record").clone();
    assert_eq!(
        received.len(),
        sent.len(),
        "the sink received {} of {} events",
        received.len(),
        sent.len()
    );
    for (index, (post, expected)) in received.iter().zip(&sent).enumerate() {
        assert_eq!(post.path, "new_block");
        // Equality of the payload *and* of its position: a set comparison would
        // pass on a stream delivered backwards.
        assert_eq!(
            &post.payload, expected,
            "event {index} is not the event dispatched at {index}"
        );
        assert_eq!(
            post.sequence,
            u64::try_from(index).expect("a small count"),
            "sequence numbers are contiguous while nothing is dropped"
        );
        assert_eq!(post.dropped, 0);
    }
    let [status] = dispatcher.status().try_into().expect("one observer");
    assert_eq!(
        (status.delivered, status.dropped),
        (u64::try_from(sent.len()).expect("a small count"), 0)
    );
    // A stream with nothing in it would satisfy everything above.
    assert!(sent.len() > 1, "no events were compared");
}
