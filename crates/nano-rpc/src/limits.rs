//! Admission limits applied before public RPC handlers run.

use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use axum::{
    extract::{DefaultBodyLimit, Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    routing::MethodRouter,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tower_http::{
    limit::RequestBodyLimitLayer,
    timeout::{RequestBodyTimeoutLayer, TimeoutLayer},
};

use crate::{READ_ONLY_WORKERS, RpcError, RpcState};

const GLOBAL_CONCURRENCY: usize = 128;
const RATE_WINDOW: Duration = Duration::from_secs(1);
const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Policy {
    pub body_bytes: usize,
    concurrent: usize,
    per_second: u64,
    body_idle_timeout: Duration,
    request_timeout: Duration,
}

impl Policy {
    const fn new(
        body_bytes: usize,
        concurrent: usize,
        per_second: u64,
        body_idle_timeout: Duration,
        request_timeout: Duration,
    ) -> Self {
        assert!(concurrent > 0, "a route needs a concurrency slot");
        assert!(per_second > 0, "a route needs a request-rate slot");
        Self {
            body_bytes,
            concurrent,
            per_second,
            body_idle_timeout,
            request_timeout,
        }
    }
}

const fn policy(body_bytes: usize, concurrent: usize, per_second: u64, seconds: u64) -> Policy {
    Policy::new(
        body_bytes,
        concurrent,
        per_second,
        BODY_IDLE_TIMEOUT,
        Duration::from_secs(seconds),
    )
}

pub const CHEAP_READ: Policy = policy(0, 64, 512, 5);
pub const STATE_READ: Policy = policy(0, 16, 128, 15);
pub const ARCHIVE_READ: Policy = policy(0, 16, 64, 30);
pub const LARGE_RESPONSE_READ: Policy = policy(0, 2, 8, 60);
pub const EVENT_STREAM: Policy = policy(0, 64, 64, 5);
pub const CALL_READ: Policy = policy(4 * 1024 * 1024 + 4096, READ_ONLY_WORKERS, 16, 60);
pub const TRANSACTION: Policy = policy(2 * 1024 * 1024, 16, 64, 30);
pub const MEMPOOL_QUERY: Policy = policy(128 * 1024, 8, 32, 30);
pub const STACKERDB_UPLOAD: Policy = policy(4 * 1024 * 1024 + 1024, 8, 32, 30);
pub const BLOCK_UPLOAD: Policy = policy(4 * 1024 * 1024, 4, 16, 60);
pub const BLOCK_PROPOSAL: Policy = policy(8 * 1024 * 1024 + 4096, 4, 16, 60);

#[derive(Clone, Debug)]
pub struct Registry {
    global: Arc<Semaphore>,
    global_limit: usize,
    routes: Arc<Mutex<HashMap<&'static str, Budget>>>,
    metrics: crate::NodeMetrics,
}

impl Registry {
    pub(crate) fn new(metrics: crate::NodeMetrics) -> Self {
        Self::with_global_limit(GLOBAL_CONCURRENCY, metrics)
    }

    fn with_global_limit(global_concurrency: usize, metrics: crate::NodeMetrics) -> Self {
        Self {
            global: Arc::new(Semaphore::new(global_concurrency)),
            global_limit: global_concurrency,
            routes: Arc::new(Mutex::new(HashMap::new())),
            metrics,
        }
    }

    fn budget(&self, name: &'static str, policy: Policy) -> Budget {
        let mut routes = lock(&self.routes);
        routes
            .entry(name)
            .or_insert_with(|| {
                Budget::new(
                    name,
                    policy,
                    self.global.clone(),
                    self.metrics.rpc_route(
                        name,
                        policy.body_bytes,
                        policy.concurrent,
                        policy.per_second,
                        self.global_limit,
                    ),
                )
            })
            .clone()
    }
}

pub fn guard(
    registry: &Registry,
    name: &'static str,
    route: MethodRouter<RpcState>,
    policy: Policy,
) -> MethodRouter<RpcState> {
    let budget = registry.budget(name, policy);
    route
        .layer::<_, Infallible>(DefaultBodyLimit::disable())
        .layer::<_, Infallible>(RequestBodyLimitLayer::new(policy.body_bytes))
        .layer::<_, Infallible>(RequestBodyTimeoutLayer::new(policy.body_idle_timeout))
        .layer::<_, Infallible>(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            policy.request_timeout,
        ))
        .layer::<_, Infallible>(axum::middleware::from_fn_with_state(budget, admit))
}

async fn admit(State(budget): State<Budget>, request: Request, next: Next) -> Response {
    let permits = match budget.try_enter() {
        Ok(permits) => permits,
        Err(Admission::Concurrency) => {
            return RpcError::Overloaded(format!("{} is at its concurrency limit", budget.name))
                .into_response();
        }
        Err(Admission::Rate) => {
            return RpcError::RateLimited(format!("{} is at its request-rate limit", budget.name))
                .into_response();
        }
    };
    let response = next.run(request).await;
    drop(permits);
    response
}

#[derive(Clone, Debug)]
struct Budget {
    name: &'static str,
    policy: Policy,
    global: Arc<Semaphore>,
    route: Arc<Semaphore>,
    rate: Arc<Mutex<RateWindow>>,
    metrics: crate::metrics::RpcRouteMetrics,
}

impl Budget {
    fn new(
        name: &'static str,
        policy: Policy,
        global: Arc<Semaphore>,
        metrics: crate::metrics::RpcRouteMetrics,
    ) -> Self {
        Self {
            name,
            policy,
            global,
            route: Arc::new(Semaphore::new(policy.concurrent)),
            rate: Arc::new(Mutex::new(RateWindow::new())),
            metrics,
        }
    }

    fn try_enter(&self) -> Result<Permits, Admission> {
        if !lock(&self.rate).admit(self.policy.per_second) {
            self.metrics.refuse_rate();
            return Err(Admission::Rate);
        }
        let global = self.global.clone().try_acquire_owned().map_err(|_| {
            self.metrics.refuse_concurrency(true);
            Admission::Concurrency
        })?;
        let route = self.route.clone().try_acquire_owned().map_err(|_| {
            self.metrics.refuse_concurrency(false);
            Admission::Concurrency
        })?;
        self.metrics.enter();
        Ok(Permits {
            _global: global,
            _route: route,
            metrics: self.metrics.clone(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Admission {
    Concurrency,
    Rate,
}

struct Permits {
    _global: OwnedSemaphorePermit,
    _route: OwnedSemaphorePermit,
    metrics: crate::metrics::RpcRouteMetrics,
}

impl Drop for Permits {
    fn drop(&mut self) {
        self.metrics.leave();
    }
}

#[derive(Debug)]
struct RateWindow {
    started: Instant,
    admitted: u64,
}

impl RateWindow {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            admitted: 0,
        }
    }

    fn admit(&mut self, limit: u64) -> bool {
        if self.started.elapsed() >= RATE_WINDOW {
            self.started = Instant::now();
            self.admitted = 0;
        }
        if self.admitted >= limit {
            return false;
        }
        self.admitted += 1;
        true
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use axum::{
        Router,
        body::{Body, Bytes},
        http::{Request, StatusCode, header},
        routing::{get, post},
    };
    use nano_primitives::Network;
    use tokio::sync::{Notify, mpsc};
    use tokio_stream::wrappers::ReceiverStream;
    use tower::ServiceExt as _;

    use super::{Admission, Policy, Registry};
    use crate::RpcState;

    fn registry(global_limit: usize) -> Registry {
        Registry::with_global_limit(global_limit, crate::NodeMetrics::default())
    }

    fn policy(concurrent: usize, per_second: u64) -> Policy {
        Policy::new(
            0,
            concurrent,
            per_second,
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
    }

    #[test]
    fn concurrency_and_rate_refusals_recover() {
        let registry = registry(2);
        let concurrency = registry.budget("concurrency", policy(1, 10));
        let held = concurrency.try_enter().expect("first request");
        assert!(matches!(
            concurrency.try_enter(),
            Err(Admission::Concurrency)
        ));
        drop(held);
        assert!(concurrency.try_enter().is_ok());

        let rate = registry.budget("rate", policy(2, 1));
        drop(rate.try_enter().expect("first request"));
        assert!(matches!(rate.try_enter(), Err(Admission::Rate)));
    }

    fn request() -> Request<Body> {
        Request::builder()
            .uri("/")
            .body(Body::empty())
            .expect("a request")
    }

    async fn scrape(metrics: crate::NodeMetrics) -> String {
        let response = crate::metrics::router(metrics)
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .expect("metrics request"),
            )
            .await
            .expect("metrics response");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("metrics body");
        String::from_utf8(body.to_vec()).expect("UTF-8 metrics")
    }

    #[tokio::test]
    async fn middleware_returns_explicit_overload_and_recovers() {
        let metrics = crate::NodeMetrics::default();
        let registry = Registry::with_global_limit(2, metrics.clone());
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let handler = {
            let started = started.clone();
            let release = release.clone();
            move || {
                let started = started.clone();
                let release = release.clone();
                async move {
                    started.notify_one();
                    release.notified().await;
                    StatusCode::OK
                }
            }
        };
        let app = Router::new()
            .route(
                "/",
                super::guard(&registry, "held", get(handler), policy(1, 10)),
            )
            .with_state(RpcState::new(Network::TESTNET));
        let first = tokio::spawn(app.clone().oneshot(request()));
        started.notified().await;

        let overloaded = app.clone().oneshot(request()).await.expect("response");
        assert_eq!(overloaded.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(overloaded.headers()[header::RETRY_AFTER], "1");
        let body = scrape(metrics.clone()).await;
        assert!(body.contains("nano_rpc_route_active{route=\"held\"} 1"));
        assert!(body.contains("nano_rpc_route_body_byte_limit{route=\"held\"} 0"));
        assert!(body.contains("nano_rpc_route_concurrency_limit{route=\"held\"} 1"));
        assert!(body.contains("nano_rpc_route_rate_limit{route=\"held\"} 10"));
        assert!(
            body.contains("nano_rpc_route_refusals_total{route=\"held\",reason=\"concurrency\"} 1")
        );
        assert!(body.contains("nano_rpc_requests_active 1"));
        assert!(body.contains("nano_rpc_request_concurrency_limit 2"));
        release.notify_one();
        assert_eq!(
            first
                .await
                .expect("request task")
                .expect("response")
                .status(),
            StatusCode::OK
        );

        let recovered = tokio::spawn(app.oneshot(request()));
        started.notified().await;
        release.notify_one();
        assert_eq!(
            recovered
                .await
                .expect("request task")
                .expect("response")
                .status(),
            StatusCode::OK
        );
        assert!(
            scrape(metrics)
                .await
                .contains("nano_rpc_route_active{route=\"held\"} 0")
        );
    }

    #[tokio::test]
    async fn middleware_returns_explicit_rate_limit() {
        let metrics = crate::NodeMetrics::default();
        let registry = Registry::with_global_limit(2, metrics.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let handler = {
            let calls = calls.clone();
            move || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::Relaxed);
                    StatusCode::OK
                }
            }
        };
        let app = Router::new()
            .route(
                "/",
                super::guard(&registry, "rate", get(handler), policy(1, 1)),
            )
            .with_state(RpcState::new(Network::TESTNET));
        assert_eq!(
            app.clone()
                .oneshot(request())
                .await
                .expect("response")
                .status(),
            StatusCode::OK
        );
        let limited = app.oneshot(request()).await.expect("response");
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(limited.headers()[header::RETRY_AFTER], "1");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(
            scrape(metrics)
                .await
                .contains("nano_rpc_route_refusals_total{route=\"rate\",reason=\"rate\"} 1")
        );
    }

    #[tokio::test]
    async fn a_timed_out_handler_releases_its_slot() {
        let registry = registry(1);
        let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let handler = {
            let first = first.clone();
            move || {
                let first = first.clone();
                async move {
                    if first.swap(false, Ordering::Relaxed) {
                        std::future::pending::<()>().await;
                    }
                    StatusCode::OK
                }
            }
        };
        let timeout = Policy::new(0, 1, 10, Duration::from_secs(1), Duration::from_millis(20));
        let app = Router::new()
            .route(
                "/",
                super::guard(&registry, "timeout", get(handler), timeout),
            )
            .with_state(RpcState::new(Network::TESTNET));

        let timed_out = app.clone().oneshot(request()).await.expect("response");
        assert_eq!(timed_out.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(
            app.oneshot(request()).await.expect("response").status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn a_stalled_body_is_refused_and_the_route_recovers() {
        let registry = registry(1);
        let timeout = Policy::new(
            1024,
            1,
            10,
            Duration::from_millis(20),
            Duration::from_secs(1),
        );
        let app = Router::new()
            .route(
                "/",
                super::guard(
                    &registry,
                    "body-timeout",
                    post(|_: Bytes| async { StatusCode::OK }),
                    timeout,
                ),
            )
            .with_state(RpcState::new(Network::TESTNET));
        let (_body_sender, body) = mpsc::channel::<Result<Bytes, Infallible>>(1);
        let stalled = Request::builder()
            .method("POST")
            .uri("/")
            .body(Body::from_stream(ReceiverStream::new(body)))
            .expect("request");

        assert_eq!(
            app.clone()
                .oneshot(stalled)
                .await
                .expect("response")
                .status(),
            StatusCode::BAD_REQUEST
        );
        let recovered = Request::builder()
            .method("POST")
            .uri("/")
            .body(Body::from("ok"))
            .expect("request");
        assert_eq!(
            app.oneshot(recovered).await.expect("response").status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn fragmented_bodies_share_one_byte_limit() {
        let registry = registry(1);
        let bounded = Policy::new(3, 1, 10, Duration::from_secs(1), Duration::from_secs(1));
        let app = Router::new()
            .route(
                "/",
                super::guard(
                    &registry,
                    "fragmented",
                    post(|_: Bytes| async { StatusCode::OK }),
                    bounded,
                ),
            )
            .with_state(RpcState::new(Network::TESTNET));
        let body = Body::from_stream(tokio_stream::iter([
            Ok::<_, Infallible>(Bytes::from_static(b"ab")),
            Ok(Bytes::from_static(b"cd")),
        ]));
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .body(body)
            .expect("request");

        assert_eq!(
            app.oneshot(request).await.expect("response").status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }
}
