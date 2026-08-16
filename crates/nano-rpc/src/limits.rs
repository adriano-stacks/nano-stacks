//! Admission limits applied before public RPC handlers run.

use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use axum::{
    extract::{DefaultBodyLimit, Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::MethodRouter,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tower_http::limit::RequestBodyLimitLayer;

use crate::{RpcError, RpcState};

const GLOBAL_CONCURRENCY: usize = 128;
const RATE_WINDOW: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Policy {
    pub body_bytes: usize,
    concurrent: usize,
    per_second: u64,
}

impl Policy {
    const fn new(body_bytes: usize, concurrent: usize, per_second: u64) -> Self {
        assert!(concurrent > 0, "a route needs a concurrency slot");
        assert!(per_second > 0, "a route needs a request-rate slot");
        Self {
            body_bytes,
            concurrent,
            per_second,
        }
    }
}

pub const CHEAP_READ: Policy = Policy::new(0, 64, 512);
pub const STATE_READ: Policy = Policy::new(0, 16, 128);
pub const ARCHIVE_READ: Policy = Policy::new(0, 16, 64);
pub const EVENT_STREAM: Policy = Policy::new(0, 64, 64);
pub const CALL_READ: Policy = Policy::new(4 * 1024 * 1024 + 4096, 4, 16);
pub const TRANSACTION: Policy = Policy::new(2 * 1024 * 1024, 16, 64);
pub const MEMPOOL_QUERY: Policy = Policy::new(1024 * 1024, 8, 32);
pub const STACKERDB_UPLOAD: Policy = Policy::new(4 * 1024 * 1024 + 1024, 8, 32);
pub const BLOCK_UPLOAD: Policy = Policy::new(4 * 1024 * 1024, 4, 16);
pub const BLOCK_PROPOSAL: Policy = Policy::new(8 * 1024 * 1024 + 4096, 4, 16);

#[derive(Clone, Debug)]
pub struct Registry {
    global: Arc<Semaphore>,
    routes: Arc<Mutex<HashMap<&'static str, Budget>>>,
}

impl Registry {
    fn new(global_concurrency: usize) -> Self {
        Self {
            global: Arc::new(Semaphore::new(global_concurrency)),
            routes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn budget(&self, name: &'static str, policy: Policy) -> Budget {
        let mut routes = lock(&self.routes);
        routes
            .entry(name)
            .or_insert_with(|| Budget::new(name, policy, self.global.clone()))
            .clone()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new(GLOBAL_CONCURRENCY)
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
}

impl Budget {
    fn new(name: &'static str, policy: Policy, global: Arc<Semaphore>) -> Self {
        Self {
            name,
            policy,
            global,
            route: Arc::new(Semaphore::new(policy.concurrent)),
            rate: Arc::new(Mutex::new(RateWindow::new())),
        }
    }

    fn try_enter(&self) -> Result<Permits, Admission> {
        if !lock(&self.rate).admit(self.policy.per_second) {
            return Err(Admission::Rate);
        }
        let global = self
            .global
            .clone()
            .try_acquire_owned()
            .map_err(|_| Admission::Concurrency)?;
        let route = self
            .route
            .clone()
            .try_acquire_owned()
            .map_err(|_| Admission::Concurrency)?;
        Ok(Permits {
            _global: global,
            _route: route,
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
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode, header},
        routing::get,
    };
    use nano_primitives::Network;
    use tokio::sync::Notify;
    use tower::ServiceExt as _;

    use super::{Admission, Policy, Registry};
    use crate::RpcState;

    #[test]
    fn concurrency_and_rate_refusals_recover() {
        let registry = Registry::new(2);
        let concurrency = registry.budget("concurrency", Policy::new(0, 1, 10));
        let held = concurrency.try_enter().expect("first request");
        assert!(matches!(
            concurrency.try_enter(),
            Err(Admission::Concurrency)
        ));
        drop(held);
        assert!(concurrency.try_enter().is_ok());

        let rate = registry.budget("rate", Policy::new(0, 2, 1));
        drop(rate.try_enter().expect("first request"));
        assert!(matches!(rate.try_enter(), Err(Admission::Rate)));
    }

    fn request() -> Request<Body> {
        Request::builder()
            .uri("/")
            .body(Body::empty())
            .expect("a request")
    }

    #[tokio::test]
    async fn middleware_returns_explicit_overload_and_recovers() {
        let registry = Registry::new(2);
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
                super::guard(&registry, "held", get(handler), Policy::new(0, 1, 10)),
            )
            .with_state(RpcState::new(Network::TESTNET));
        let first = tokio::spawn(app.clone().oneshot(request()));
        started.notified().await;

        let overloaded = app.clone().oneshot(request()).await.expect("response");
        assert_eq!(overloaded.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(overloaded.headers()[header::RETRY_AFTER], "1");
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
    }

    #[tokio::test]
    async fn middleware_returns_explicit_rate_limit() {
        let registry = Registry::new(2);
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
                super::guard(&registry, "rate", get(handler), Policy::new(0, 1, 1)),
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
    }
}
