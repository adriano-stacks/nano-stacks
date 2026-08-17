//! Bounded HTTP connections for the public RPC.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use axum::{Router, body::Body};
use hyper::{Request, body::Incoming, server::conn::http1::Builder, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::{net::TcpStream, task::JoinSet};
use tower::ServiceExt as _;

use crate::{RpcState, router};

const MAX_CONNECTIONS: usize = 256;
const MAX_CONNECTIONS_PER_ADDRESS: usize = 16;
const MAX_METRICS_CONNECTIONS: usize = 16;
const MAX_METRICS_CONNECTIONS_PER_ADDRESS: usize = 4;
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECTION_LIFETIME: Duration = Duration::from_mins(15);
const HTTP1_MAX_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug)]
struct ServerLimits {
    connections: usize,
    connections_per_address: usize,
    header_timeout: Duration,
    connection_lifetime: Duration,
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            connections: MAX_CONNECTIONS,
            connections_per_address: MAX_CONNECTIONS_PER_ADDRESS,
            header_timeout: HEADER_READ_TIMEOUT,
            connection_lifetime: CONNECTION_LIFETIME,
        }
    }
}

impl ServerLimits {
    const fn metrics() -> Self {
        Self {
            connections: MAX_METRICS_CONNECTIONS,
            connections_per_address: MAX_METRICS_CONNECTIONS_PER_ADDRESS,
            header_timeout: HEADER_READ_TIMEOUT,
            connection_lifetime: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Copy)]
enum ConnectionSurface {
    Rpc,
    Metrics,
}

/// Serve the public RPC until the listener is stopped.
pub async fn serve(listener: tokio::net::TcpListener, state: RpcState) -> std::io::Result<()> {
    let metrics = state.metrics();
    serve_with_limits(
        listener,
        router(state),
        ServerLimits::default(),
        metrics,
        ConnectionSurface::Rpc,
    )
    .await
}

pub async fn serve_metrics(
    listener: tokio::net::TcpListener,
    app: Router,
    metrics: crate::NodeMetrics,
) -> std::io::Result<()> {
    serve_with_limits(
        listener,
        app,
        ServerLimits::metrics(),
        metrics,
        ConnectionSurface::Metrics,
    )
    .await
}

async fn serve_with_limits(
    listener: tokio::net::TcpListener,
    app: Router,
    limits: ServerLimits,
    metrics: crate::NodeMetrics,
    surface: ConnectionSurface,
) -> std::io::Result<()> {
    let slots = ConnectionSlots::new(limits, metrics, surface);
    let mut connections = JoinSet::new();
    loop {
        while connections.try_join_next().is_some() {}
        let (stream, remote) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!("accepting an RPC connection failed: {error}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let Some(slot) = slots.try_acquire(remote.ip()) else {
            continue;
        };
        if let Err(error) = stream.set_nodelay(true) {
            eprintln!("configuring RPC connection {remote} failed: {error}");
            continue;
        }
        let app = app.clone();
        connections.spawn(async move {
            let _slot = slot;
            serve_connection(stream, app, limits).await;
        });
    }
}

async fn serve_connection(stream: TcpStream, app: Router, limits: ServerLimits) {
    let service =
        service_fn(move |request: Request<Incoming>| app.clone().oneshot(request.map(Body::new)));
    let mut builder = Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_timeout)
        .max_buf_size(HTTP1_MAX_BUFFER_BYTES);
    let connection = builder
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades();
    let _ = tokio::time::timeout(limits.connection_lifetime, connection).await;
}

#[derive(Clone)]
struct ConnectionSlots {
    accounting: Arc<Mutex<ConnectionAccounting>>,
    metrics: crate::NodeMetrics,
    surface: ConnectionSurface,
}

impl ConnectionSlots {
    fn new(limits: ServerLimits, metrics: crate::NodeMetrics, surface: ConnectionSurface) -> Self {
        let slots = Self {
            accounting: Arc::new(Mutex::new(ConnectionAccounting {
                limits,
                total: 0,
                addresses: HashMap::new(),
                saturations: 0,
            })),
            metrics,
            surface,
        };
        slots.publish(&lock(&slots.accounting));
        slots
    }

    fn try_acquire(&self, address: IpAddr) -> Option<ConnectionSlot> {
        let mut accounting = lock(&self.accounting);
        if accounting.total >= accounting.limits.connections
            || accounting.addresses.get(&address).copied().unwrap_or(0)
                >= accounting.limits.connections_per_address
        {
            accounting.saturations = accounting.saturations.saturating_add(1);
            self.publish(&accounting);
            return None;
        }
        accounting.total += 1;
        *accounting.addresses.entry(address).or_default() += 1;
        self.publish(&accounting);
        drop(accounting);
        Some(ConnectionSlot {
            accounting: self.accounting.clone(),
            metrics: self.metrics.clone(),
            surface: self.surface,
            address,
        })
    }

    fn publish(&self, accounting: &ConnectionAccounting) {
        match self.surface {
            ConnectionSurface::Rpc => self.metrics.publish_rpc_connections(accounting.status()),
            ConnectionSurface::Metrics => self
                .metrics
                .publish_metrics_connections(accounting.status()),
        }
    }
}

struct ConnectionSlot {
    accounting: Arc<Mutex<ConnectionAccounting>>,
    metrics: crate::NodeMetrics,
    surface: ConnectionSurface,
    address: IpAddr,
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        let mut accounting = lock(&self.accounting);
        accounting.total -= 1;
        let count = accounting
            .addresses
            .get_mut(&self.address)
            .expect("an RPC connection slot has an address");
        *count -= 1;
        if *count == 0 {
            accounting.addresses.remove(&self.address);
        }
        match self.surface {
            ConnectionSurface::Rpc => self.metrics.publish_rpc_connections(accounting.status()),
            ConnectionSurface::Metrics => self
                .metrics
                .publish_metrics_connections(accounting.status()),
        }
    }
}

struct ConnectionAccounting {
    limits: ServerLimits,
    total: usize,
    addresses: HashMap<IpAddr, usize>,
    saturations: u64,
}

impl ConnectionAccounting {
    fn status(&self) -> crate::AdmissionStatus {
        crate::AdmissionStatus {
            used: self.total,
            subjects: self.addresses.len(),
            limit: self.limits.connections,
            per_subject_limit: self.limits.connections_per_address,
            saturations: self.saturations,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr},
        time::Duration,
    };

    use nano_primitives::Network;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::{ConnectionSlots, ConnectionSurface, ServerLimits, serve_with_limits};
    use crate::{RpcState, router};

    fn limits() -> ServerLimits {
        ServerLimits {
            connections: 2,
            connections_per_address: 1,
            header_timeout: Duration::from_millis(30),
            connection_lifetime: Duration::from_secs(1),
        }
    }

    #[test]
    fn connection_slots_are_bounded_globally_and_per_address() {
        let metrics = crate::NodeMetrics::default();
        let slots = ConnectionSlots::new(limits(), metrics.clone(), ConnectionSurface::Rpc);
        let first = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let second = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2));
        let held = slots.try_acquire(first).expect("first address");
        assert!(slots.try_acquire(first).is_none());
        let other = slots.try_acquire(second).expect("second address");
        assert!(
            slots
                .try_acquire(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 3)))
                .is_none()
        );
        drop(held);
        let recovered = slots.try_acquire(first).expect("the slot recovers");
        drop(other);
        let body = metrics.encode().expect("metrics encode");
        assert!(body.contains("nano_rpc_connections_active 1"));
        assert!(body.contains("nano_rpc_connection_addresses 1"));
        assert!(body.contains("nano_rpc_connection_limit 2"));
        assert!(body.contains("nano_rpc_connection_per_address_limit 1"));
        assert!(body.contains("nano_rpc_connection_saturations 2"));
        drop(recovered);
    }

    #[test]
    fn metrics_connection_slots_are_bounded_and_reported_separately() {
        let metrics = crate::NodeMetrics::default();
        let slots = ConnectionSlots::new(limits(), metrics.clone(), ConnectionSurface::Metrics);
        let held = slots
            .try_acquire(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .expect("first connection");
        assert!(slots.try_acquire(IpAddr::V4(Ipv4Addr::LOCALHOST)).is_none());
        let body = metrics.encode().expect("metrics encode");
        assert!(body.contains("nano_metrics_connections_active 1"));
        assert!(body.contains("nano_metrics_connection_addresses 1"));
        assert!(body.contains("nano_metrics_connection_limit 2"));
        assert!(body.contains("nano_metrics_connection_per_address_limit 1"));
        assert!(body.contains("nano_metrics_connection_saturations 1"));
        assert!(body.contains("nano_rpc_connections_active 0"));
        drop(held);
    }

    #[tokio::test]
    async fn a_slow_header_is_closed_and_the_server_recovers() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let served = tokio::spawn(serve_with_limits(
            listener,
            router(RpcState::new(Network::TESTNET)),
            limits(),
            crate::NodeMetrics::default(),
            ConnectionSurface::Rpc,
        ));
        let mut slow = tokio::net::TcpStream::connect(address)
            .await
            .expect("slow connection");
        slow.write_all(b"G").await.expect("partial HTTP request");
        let mut byte = [0];
        assert_eq!(slow.read(&mut byte).await.expect("closed connection"), 0);

        let response = reqwest::get(format!("http://{address}/v2/info"))
            .await
            .expect("recovered request");
        assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        served.abort();
    }

    #[tokio::test]
    async fn a_slow_metrics_client_is_shed_and_the_listener_recovers() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let metrics = crate::NodeMetrics::default();
        let served = tokio::spawn(serve_with_limits(
            listener,
            crate::metrics::router(metrics.clone()),
            limits(),
            metrics.clone(),
            ConnectionSurface::Metrics,
        ));
        let mut slow = tokio::net::TcpStream::connect(address)
            .await
            .expect("slow connection");
        slow.write_all(b"G").await.expect("partial HTTP request");
        tokio::time::timeout(Duration::from_secs(1), async {
            while !metrics
                .encode()
                .expect("metrics encode")
                .contains("nano_metrics_connections_active 1")
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the slow connection is admitted");

        let mut refused = tokio::net::TcpStream::connect(address)
            .await
            .expect("refused connection");
        refused
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("request");
        let mut byte = [0];
        match refused.read(&mut byte).await {
            Ok(0) => {}
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            result => panic!("refused metrics connection stayed open: {result:?}"),
        }
        assert_eq!(slow.read(&mut byte).await.expect("timed out header"), 0);

        let response = reqwest::get(format!("http://{address}/metrics"))
            .await
            .expect("recovered request");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body = response.text().await.expect("metrics body");
        assert!(body.contains("nano_metrics_connection_saturations 1"));
        served.abort();
    }
}
