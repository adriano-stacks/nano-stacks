//! The event endpoint a stock node posts to when nano replaces its signer.
//!
//! nano signs from its own checkpoint and reads proposals over HTTP, so the
//! node's event stream is not an input to anything it validates. Serving the
//! endpoint anyway keeps the node healthy: its dispatcher retries a failed
//! POST forever, so a replaced signer that stops answering stalls the tenure
//! of the node that fed it.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use axum::{Router, extract::State, http::StatusCode, routing::any};

/// Counts of the event payloads a node has delivered, keyed by endpoint path.
#[derive(Clone, Debug, Default)]
pub struct EventSink {
    delivered: Arc<Mutex<BTreeMap<String, u64>>>,
}

impl EventSink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The payload count each path has received so far.
    #[must_use]
    pub fn delivered(&self) -> BTreeMap<String, u64> {
        self.lock().clone()
    }

    fn record(&self, path: &str) -> u64 {
        let mut delivered = self.lock();
        let count = delivered.entry(path.to_owned()).or_default();
        *count = count.saturating_add(1);
        let total = *count;
        drop(delivered);
        total
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, u64>> {
        self.delivered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Accept every event path a node may be configured to deliver.
pub fn router(sink: EventSink) -> Router {
    Router::new()
        .fallback(any(accept))
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(sink)
}

/// Serve the event endpoint until the listener is stopped.
pub async fn serve(listener: tokio::net::TcpListener, sink: EventSink) -> std::io::Result<()> {
    axum::serve(listener, router(sink)).await
}

async fn accept(
    State(sink): State<EventSink>,
    uri: axum::http::Uri,
    // Reading the payload to the end lets the node reuse the connection it
    // opened, even though nano derives nothing from the event itself.
    _payload: axum::body::Bytes,
) -> StatusCode {
    let count = sink.record(uri.path());
    if count == 1 {
        eprintln!("serving the node's {} event stream", uri.path());
    }
    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::{EventSink, router};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// A node's dispatcher retries until it sees a 200, whatever the path.
    #[tokio::test]
    async fn every_event_path_is_acknowledged_and_counted() {
        let sink = EventSink::new();
        for path in ["/new_burn_block", "/stackerdb_chunks", "/proposal_response"] {
            let response = router(sink.clone())
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .body(Body::from("{}"))
                        .expect("build the event request"),
                )
                .await
                .expect("serve the event request");
            assert_eq!(response.status(), StatusCode::OK);
        }

        assert_eq!(
            sink.delivered().into_iter().collect::<Vec<_>>(),
            vec![
                ("/new_burn_block".to_owned(), 1),
                ("/proposal_response".to_owned(), 1),
                ("/stackerdb_chunks".to_owned(), 1),
            ]
        );
    }
}
