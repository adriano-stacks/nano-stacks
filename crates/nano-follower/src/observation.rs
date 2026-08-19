//! The follower's two loopback-only observation routes.

use std::{
    io,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    sync::RwLock,
};

const MAX_REQUEST_BYTES: usize = 1_024;

/// Every HTTP route served by the follower artifact.
pub const LOOPBACK_ROUTES: [&str; 2] = ["/health", "/metrics"];

/// The follower artifact deliberately has no public HTTP surface.
pub const PUBLIC_ROUTES: [&str; 0] = [];

/// What an operator can observe without gaining a chainstate capability.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Snapshot {
    pub ready: bool,
    pub stacks_height: Option<u64>,
    pub bitcoin_height: Option<u64>,
    pub state_root: Option<String>,
    pub p2p_connected: usize,
    pub p2p_known: usize,
    pub last_error: Option<String>,
}

/// A read-only snapshot shared with the two loopback listeners.
#[derive(Clone, Debug, Default)]
pub struct Observation {
    snapshot: Arc<RwLock<Snapshot>>,
    /// The executed tip as the executor stores it, block by block.
    ///
    /// A snapshot is published once per round, and a round's execute budget can
    /// be a whole checkpoint catch-up — hours during which health and metrics
    /// would otherwise keep naming the height the round started from, which
    /// reads exactly like a stalled node. Zero means no block was executed
    /// since the last snapshot, so the snapshot's own height stands.
    executed: Arc<AtomicU64>,
}

impl Observation {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn publish(&self, snapshot: Snapshot) {
        // The round's snapshot is authoritative — it can also move a tip
        // backwards across a retraction, which the per-block signal never does.
        self.executed
            .store(snapshot.stacks_height.unwrap_or(0), Ordering::Relaxed);
        *self.snapshot.write().await = snapshot;
    }

    /// Where the executor reports each executed block's height, synchronously.
    #[must_use]
    pub fn executed_height_sink(&self) -> Arc<AtomicU64> {
        self.executed.clone()
    }

    #[must_use]
    pub fn status(&self) -> Arc<RwLock<Snapshot>> {
        self.snapshot.clone()
    }

    /// Serve health and metrics until either loopback listener fails.
    pub async fn serve(&self, health: SocketAddr, metrics: SocketAddr) -> io::Result<()> {
        let health = TcpListener::bind(health).await?;
        let metrics = TcpListener::bind(metrics).await?;
        tokio::select! {
            result = serve(health, Route::Health, self.clone()) => result,
            result = serve(metrics, Route::Metrics, self.clone()) => result,
        }
    }
}

#[derive(Clone, Copy)]
enum Route {
    Health,
    Metrics,
}

impl Route {
    const fn path(self) -> &'static str {
        match self {
            Self::Health => LOOPBACK_ROUTES[0],
            Self::Metrics => LOOPBACK_ROUTES[1],
        }
    }
}

async fn serve(listener: TcpListener, route: Route, observation: Observation) -> io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let observation = observation.clone();
        tokio::spawn(async move {
            if let Err(error) = answer(stream, route, observation).await {
                eprintln!("follower observation request failed: {error}");
            }
        });
    }
}

async fn answer(mut stream: TcpStream, route: Route, observation: Observation) -> io::Result<()> {
    let mut request = [0; MAX_REQUEST_BYTES];
    let read = stream.read(&mut request).await?;
    let request = String::from_utf8_lossy(&request[..read]);
    let mut words = request
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace();
    let matches = words.next() == Some("GET") && words.next() == Some(route.path());
    if !matches {
        return write_response(&mut stream, 404, "text/plain", b"not found\n").await;
    }
    let mut snapshot = observation.snapshot.read().await.clone();
    let executed = observation.executed.load(Ordering::Relaxed);
    if executed != 0 {
        snapshot.stacks_height = Some(executed);
    }
    match route {
        Route::Health => {
            let code = if snapshot.ready { 200 } else { 503 };
            let body = serde_json::to_vec(&serde_json::json!({
                "ready": snapshot.ready,
                "stacks_height": snapshot.stacks_height,
                "bitcoin_height": snapshot.bitcoin_height,
                "state_root": snapshot.state_root,
                "p2p_connected": snapshot.p2p_connected,
                "p2p_known": snapshot.p2p_known,
                "last_error": snapshot.last_error,
            }))
            .map_err(io::Error::other)?;
            write_response(&mut stream, code, "application/json", &body).await
        }
        Route::Metrics => {
            let body = metrics(&snapshot);
            write_response(
                &mut stream,
                200,
                "text/plain; version=0.0.4",
                body.as_bytes(),
            )
            .await
        }
    }
}

fn metrics(snapshot: &Snapshot) -> String {
    format!(
        "nano_follower_ready {}\n\
         nano_follower_stacks_height {}\n\
         nano_follower_bitcoin_height {}\n\
         nano_follower_p2p_connected {}\n\
         nano_follower_p2p_known {}\n",
        u8::from(snapshot.ready),
        snapshot.stacks_height.unwrap_or_default(),
        snapshot.bitcoin_height.unwrap_or_default(),
        snapshot.p2p_connected,
        snapshot.p2p_known,
    )
}

async fn write_response(
    stream: &mut TcpStream,
    code: u16,
    content_type: &str,
    body: &[u8],
) -> io::Result<()> {
    let reason = match code {
        200 => "OK",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let headers = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(headers.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use tokio::{io::AsyncReadExt as _, io::AsyncWriteExt as _, net::TcpStream};

    use super::{Observation, Route, Snapshot, serve};

    async fn request(address: std::net::SocketAddr, path: &str) -> String {
        let mut stream = TcpStream::connect(address).await.expect("connect");
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .expect("request");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("response");
        response
    }

    async fn listener(route: Route, observation: Observation) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        tokio::spawn(serve(listener, route, observation));
        address
    }

    #[tokio::test]
    async fn health_exposes_only_readiness_and_execution_position() {
        let observation = Observation::new();
        observation
            .publish(Snapshot {
                ready: true,
                stacks_height: Some(8_724_890),
                bitcoin_height: Some(961_700),
                state_root: Some("ab".repeat(32)),
                p2p_connected: 4,
                p2p_known: 81,
                last_error: None,
            })
            .await;
        let address = listener(Route::Health, observation).await;
        let response = request(address, "/health").await;
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("\"stacks_height\":8724890"));
        assert!(response.contains("\"p2p_connected\":4"));
        assert!(
            request(address, "/metrics")
                .await
                .starts_with("HTTP/1.1 404")
        );
    }

    #[tokio::test]
    async fn metrics_are_read_only_and_unready_health_is_explicit() {
        let observation = Observation::new();
        let health = listener(Route::Health, observation.clone()).await;
        let metrics = listener(Route::Metrics, observation).await;
        assert!(request(health, "/health").await.starts_with("HTTP/1.1 503"));
        let response = request(metrics, "/metrics").await;
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("nano_follower_ready 0"));
        assert!(
            request(metrics, "/v2/transactions")
                .await
                .starts_with("HTTP/1.1 404")
        );
    }

    /// A round's snapshot is published when the round ends, and one round can
    /// be a whole checkpoint catch-up — the per-block height must show through
    /// in between, and the next snapshot must remain authoritative over it,
    /// including across a retraction that moves the tip backwards.
    #[tokio::test]
    async fn executed_blocks_move_health_between_snapshots() {
        let observation = Observation::new();
        let sink = observation.executed_height_sink();
        observation
            .publish(Snapshot {
                ready: true,
                stacks_height: Some(8_665_601),
                bitcoin_height: Some(960_231),
                state_root: Some("ab".repeat(32)),
                p2p_connected: 4,
                p2p_known: 81,
                last_error: None,
            })
            .await;
        let health = listener(Route::Health, observation.clone()).await;
        let metrics = listener(Route::Metrics, observation.clone()).await;
        assert!(
            request(health, "/health")
                .await
                .contains("\"stacks_height\":8665601")
        );

        sink.store(8_700_000, std::sync::atomic::Ordering::Relaxed);
        assert!(
            request(health, "/health")
                .await
                .contains("\"stacks_height\":8700000")
        );
        assert!(
            request(metrics, "/metrics")
                .await
                .contains("nano_follower_stacks_height 8700000")
        );

        observation
            .publish(Snapshot {
                ready: true,
                stacks_height: Some(8_699_998),
                bitcoin_height: Some(960_400),
                state_root: Some("cd".repeat(32)),
                p2p_connected: 4,
                p2p_known: 81,
                last_error: None,
            })
            .await;
        assert!(
            request(health, "/health")
                .await
                .contains("\"stacks_height\":8699998")
        );
    }
}
