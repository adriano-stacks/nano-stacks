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

/// Serve the public RPC until the listener is stopped.
pub async fn serve(listener: tokio::net::TcpListener, state: RpcState) -> std::io::Result<()> {
    serve_with_limits(listener, router(state), ServerLimits::default()).await
}

async fn serve_with_limits(
    listener: tokio::net::TcpListener,
    app: Router,
    limits: ServerLimits,
) -> std::io::Result<()> {
    let slots = ConnectionSlots::new(limits);
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
}

impl ConnectionSlots {
    fn new(limits: ServerLimits) -> Self {
        Self {
            accounting: Arc::new(Mutex::new(ConnectionAccounting {
                limits,
                total: 0,
                addresses: HashMap::new(),
            })),
        }
    }

    fn try_acquire(&self, address: IpAddr) -> Option<ConnectionSlot> {
        let mut accounting = lock(&self.accounting);
        if accounting.total >= accounting.limits.connections
            || accounting.addresses.get(&address).copied().unwrap_or(0)
                >= accounting.limits.connections_per_address
        {
            return None;
        }
        accounting.total += 1;
        *accounting.addresses.entry(address).or_default() += 1;
        drop(accounting);
        Some(ConnectionSlot {
            accounting: self.accounting.clone(),
            address,
        })
    }
}

struct ConnectionSlot {
    accounting: Arc<Mutex<ConnectionAccounting>>,
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
    }
}

struct ConnectionAccounting {
    limits: ServerLimits,
    total: usize,
    addresses: HashMap<IpAddr, usize>,
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

    use super::{ConnectionSlots, ServerLimits, serve_with_limits};
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
        let slots = ConnectionSlots::new(limits());
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
        assert!(slots.try_acquire(first).is_some());
        drop(other);
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
}
