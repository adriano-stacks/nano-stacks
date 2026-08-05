//! Nano on both ends of the wire.
//!
//! The live test proves nano can talk to stacks-core. This proves the other
//! direction of the same protocol — that nano *answers* — without needing anything
//! outside the process, which is what makes it a gate rather than a diagnostic. It
//! is the closest deterministic stand-in for the acceptance criterion that a stock
//! node can handshake with nano and exchange inventory in both directions; the
//! reference implementation's own codec checks the bytes in
//! `nano-conformance/tests/conformance/p2p_wire.rs`, and what is left to check is
//! the conversation, which is what this does.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nano_chainstate::NakamotoBlock;
use nano_crypto::StacksPrivateKey;
use nano_p2p::wire::{ChainView, Message, PREAMBLE_LEN, Payload, Preamble, services};
use nano_p2p::{
    InboundLimits, Listener, LocalPeer, PeerDb, Protocol, Service, Session, Swarm, SwarmLimits,
    serve_peer,
};
use nano_primitives::{BitVec, BitcoinHeaderHash, ConsensusHash, Hash160};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TIMEOUT: Duration = Duration::from_secs(5);

/// The cycle this node claims to know about, so that the one it does not can be
/// told apart from a bug.
const KNOWN_CYCLE: ConsensusHash = ConsensusHash::from_bytes([0x11; 20]);
const UNKNOWN_CYCLE: ConsensusHash = ConsensusHash::from_bytes([0x99; 20]);

fn view(height: u64) -> ChainView {
    ChainView::new(
        height,
        BitcoinHeaderHash::from_bytes([0x5a; 32]),
        BitcoinHeaderHash::from_bytes([0xc3; 32]),
    )
    .expect("a tip above the confirmation window")
}

/// A node that knows one reward cycle and three neighbours.
#[derive(Default)]
struct TestService {
    height: u64,
    offered_blocks: Mutex<Vec<(Hash160, usize)>>,
    offered_transactions: Mutex<Vec<Hash160>>,
}

impl Service for TestService {
    fn chain_view(&self) -> ChainView {
        view(self.height)
    }

    fn neighbors(&self) -> Vec<nano_p2p::NeighborAddress> {
        (1..=3_u8)
            .map(|index| nano_p2p::NeighborAddress {
                address: nano_p2p::PeerAddress::from_ip(std::net::IpAddr::V4(
                    std::net::Ipv4Addr::new(203, 0, 113, index),
                )),
                port: 20444,
                public_key_hash: Hash160::from_bytes([index; 20]),
            })
            .collect()
    }

    fn tenure_inventory(&self, cycle_start: ConsensusHash) -> Option<BitVec<2100>> {
        (cycle_start == KNOWN_CYCLE).then(|| {
            let mut tenures = BitVec::<2100>::zeros(2100).expect("a cycle-length bit vector");
            tenures.set(0, true).expect("in bounds");
            tenures.set(2099, true).expect("in bounds");
            tenures
        })
    }

    fn offer_blocks(&self, from: Hash160, blocks: Vec<NakamotoBlock>) {
        if let Ok(mut offered) = self.offered_blocks.lock() {
            offered.push((from, blocks.len()));
        }
    }

    fn offer_transaction(&self, from: Hash160, _transaction: Box<nano_codec::Transaction>) {
        if let Ok(mut offered) = self.offered_transactions.lock() {
            offered.push(from);
        }
    }
}

/// Bring up a nano node that answers peers, and return where to reach it.
async fn listening(
    service: Arc<TestService>,
    data_url: Option<&str>,
) -> (SocketAddr, LocalPeer, tokio::task::JoinHandle<()>) {
    let listener = Listener::bind("127.0.0.1:0".parse().expect("a loopback address"))
        .await
        .expect("bind");
    let address = listener.local_addr().expect("the bound address");
    let mut local = LocalPeer::quiet(
        StacksPrivateKey::from_seed(&address.port().to_be_bytes()),
        address.port(),
    );
    local.address = nano_p2p::PeerAddress::from_ip(address.ip());
    if let Some(url) = data_url {
        url.clone_into(&mut local.data_url);
        // The RPC bit is what tells a peer the endpoint will answer; a `data_url`
        // without it is not something to dial.
        local.services = services::RPC | services::RELAY;
    }
    let served = local.clone();
    let task = tokio::spawn(async move {
        // The conversations live in a `JoinSet` owned by the accept loop, so that
        // aborting the loop takes them with it. Spawned loose, an aborted listener
        // left its existing connections answering pings, and a peer this test had
        // "removed" was still in the swarm.
        let mut conversations = tokio::task::JoinSet::new();
        loop {
            let Ok((stream, from)) = listener.accept().await else {
                return;
            };
            let service = service.clone();
            let local = served.clone();
            conversations.spawn(async move {
                let _ = serve_peer(
                    stream,
                    from,
                    &local,
                    Protocol::testnet(),
                    service.as_ref(),
                    InboundLimits {
                        timeout: TIMEOUT,
                        ..InboundLimits::default()
                    },
                )
                .await;
            });
        }
    });
    (address, local, task)
}

/// A peer that answers with a well-formed message that breaks a protocol rule.
///
/// Needed because a peer that simply hangs up — which is what the wrong-network
/// case looks like from the *other* side — is indistinguishable from a restart, and
/// nano deliberately treats it as one. Isolation is for a peer that answers and is
/// wrong, so testing it needs a peer that answers.
async fn hostile(
    reply: impl Fn(u32) -> Vec<u8> + Send + 'static,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("the bound address");
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut header = [0; PREAMBLE_LEN];
            if stream.read_exact(&mut header).await.is_err() {
                continue;
            }
            let Ok(preamble) = Preamble::decode(&header) else {
                continue;
            };
            let mut frame = vec![0; preamble.payload_len as usize];
            if stream.read_exact(&mut frame).await.is_err() {
                continue;
            }
            let _ = stream.write_all(&reply(preamble.seq)).await;
            let _ = stream.flush().await;
        }
    });
    (address, task)
}

/// Dial one hostile peer and report what the swarm made of it.
async fn swarm_against(address: SocketAddr) -> (nano_p2p::Round, nano_p2p::KnownPeer) {
    let mut swarm = Swarm::new(
        PeerDb::in_memory().expect("a peer table"),
        LocalPeer::quiet(StacksPrivateKey::from_seed(b"honest"), 20444),
        Protocol::testnet(),
        SwarmLimits {
            outbound: 4,
            dials_per_round: 4,
            timeout: TIMEOUT,
        },
    );
    swarm
        .seed(&address.to_string())
        .await
        .expect("record the seed");
    let round = swarm.maintain(view(900_001)).await;
    let known = swarm
        .peer_table()
        .get(nano_p2p::PeerAddress::from_ip(address.ip()), address.port())
        .expect("read the table")
        .expect("the peer is still known");
    (round, known)
}

/// A handshake, a ping and a neighbour walk, nano to nano.
#[tokio::test]
async fn nano_answers_a_peer_that_dialled_it() {
    let service = Arc::new(TestService {
        height: 900_000,
        ..TestService::default()
    });
    let (address, served, _task) = listening(service.clone(), Some("http://127.0.0.1:20443")).await;

    let dialler = LocalPeer::quiet(StacksPrivateKey::from_seed(b"dialler"), 20444);
    let mut session = Session::open(
        address,
        &dialler,
        Protocol::testnet(),
        view(900_001),
        TIMEOUT,
    )
    .await
    .expect("the handshake completes");

    // The key the listener announced is the key it signed with, which is the whole
    // of what a handshake establishes.
    assert_eq!(
        session.public_key_hash(),
        nano_primitives::hash160(&served.private_key.public_key().to_bytes_compressed())
    );
    assert_eq!(session.handshake().data_url, "http://127.0.0.1:20443");
    assert_eq!(session.handshake().port, address.port());
    // Its advertised view is the one its service reports, not the one we sent.
    assert_eq!(session.remote_view().height, 900_000);

    session.ping().await.expect("it answers a ping");

    let neighbors = session.neighbors().await.expect("it names its neighbours");
    assert_eq!(neighbors.len(), 3);
    assert_eq!(neighbors[0].public_key_hash, Hash160::from_bytes([1; 20]));

    // An inventory for a cycle it knows, and a refusal for one it does not. The
    // refusal matters as much: a node that answered every cycle with zeroes would
    // look like it was withholding every tenure it has.
    let tenures = session
        .nakamoto_inventory(KNOWN_CYCLE)
        .await
        .expect("it knows this cycle");
    assert_eq!(tenures.len(), 2100);
    assert_eq!(tenures.get(0), Some(true));
    assert_eq!(tenures.get(2099), Some(true));
    assert_eq!(tenures.get(1), Some(false));

    match session.nakamoto_inventory(UNKNOWN_CYCLE).await {
        Err(nano_p2p::SessionError::Nack(code)) => {
            assert_eq!(code, nano_p2p::wire::nack::NO_SUCH_BITCOIN_BLOCK);
        }
        other => panic!("expected a nack for an unknown cycle, got {other:?}"),
    }
}

/// Nothing but a handshake gets served before a handshake.
///
/// A node that answered `GetNeighbors` to an unauthenticated caller would hand its
/// whole peer table to anyone who opened a socket, and would do work on demand for
/// a peer that never identified itself.
#[tokio::test]
async fn an_unauthenticated_request_is_nacked() {
    let service = Arc::new(TestService {
        height: 900_000,
        ..TestService::default()
    });
    let (address, _served, _task) = listening(service, None).await;

    let key = StacksPrivateKey::from_seed(b"rude");
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect");
    let request = Message::sign(
        Protocol::testnet().peer_version,
        Protocol::testnet().network_id,
        &view(900_001),
        0x4242,
        Payload::GetNeighbors,
        &key,
    )
    .expect("sign");
    stream
        .write_all(&request.encode())
        .await
        .expect("send the request");

    let mut header = [0; PREAMBLE_LEN];
    stream.read_exact(&mut header).await.expect("read a reply");
    let preamble = Preamble::decode(&header).expect("the reply's preamble decodes");
    // The reply carries the request's sequence number, which is how a peer pairs it.
    assert_eq!(preamble.seq, 0x4242);
    let mut frame = vec![0; preamble.payload_len as usize];
    stream.read_exact(&mut frame).await.expect("read the frame");
    match Message::decode(preamble, frame).expect("decode").payload {
        Payload::Nack(code) => assert_eq!(code, nano_p2p::wire::nack::HANDSHAKE_REQUIRED),
        other => panic!("expected a handshake-required nack, got a {}", other.name()),
    }
}

/// The swarm holds several peers, publishes their endpoints, and drops the ones
/// that go away.
#[tokio::test]
async fn a_swarm_holds_several_peers_and_notices_one_leaving() {
    let service = Arc::new(TestService {
        height: 900_000,
        ..TestService::default()
    });
    let (first, _, first_task) =
        listening(service.clone(), Some("http://127.0.0.1:20443")).await;
    let (second, _, _second_task) =
        listening(service.clone(), Some("http://127.0.0.1:20543")).await;
    // A third peer that serves no HTTP: it should be a session, and not an
    // endpoint, because there is nothing at the other end to fetch from.
    let (third, _, _third_task) = listening(service, None).await;

    let mut swarm = Swarm::new(
        PeerDb::in_memory().expect("a peer table"),
        LocalPeer::quiet(StacksPrivateKey::from_seed(b"swarm"), 20444),
        Protocol::testnet(),
        SwarmLimits {
            outbound: 4,
            dials_per_round: 4,
            timeout: TIMEOUT,
        },
    );
    for address in [first, second, third] {
        swarm
            .seed(&address.to_string())
            .await
            .expect("record the seed");
    }
    let discovered = swarm.discovered();

    let round = swarm.maintain(view(900_001)).await;
    assert_eq!(round.dialled, 3);
    assert_eq!(round.connected, 3);
    assert_eq!(round.isolated, 0);
    // A neighbour walk asks one peer per round, and each of these names three.
    assert_eq!(round.learned, 3);
    assert_eq!(discovered.connected(), 3);
    let mut endpoints = discovered.endpoints();
    endpoints.sort();
    assert_eq!(
        endpoints,
        vec!["http://127.0.0.1:20443", "http://127.0.0.1:20543"]
    );
    // Three dialled peers plus the three addresses one of them gossiped.
    assert_eq!(discovered.known(), 6);

    // Every peer answers the same inventory question, because one peer's inventory
    // is one peer's claim.
    let claims = swarm.tenure_claims(KNOWN_CYCLE).await;
    assert_eq!(claims.len(), 3);
    assert!(claims.iter().all(|claim| claim.tenures.get(0) == Some(true)));
    assert_eq!(
        claims.iter().filter(|claim| claim.endpoint.is_some()).count(),
        2
    );
    // A nack is an answer and not a fault, so nobody is dropped for it.
    assert!(swarm.tenure_claims(UNKNOWN_CYCLE).await.is_empty());
    assert_eq!(swarm.discovered().connected(), 3);

    // One peer goes away. It is dropped, not isolated: not answering is a restart,
    // and a peer punished for restarting is a peer a small network cannot afford.
    first_task.abort();
    let round = swarm.maintain(view(900_002)).await;
    assert_eq!(round.connected, 2);
    assert_eq!(round.isolated, 0);
    assert_eq!(discovered.connected(), 2);
    assert!(!discovered.endpoints().contains(&"http://127.0.0.1:20443".to_owned()));
    // And it is still known, with a failure against it rather than forgotten.
    let known = swarm
        .peer_table()
        .get(nano_p2p::PeerAddress::from_ip(first.ip()), first.port())
        .expect("read the table")
        .expect("the peer is still known");
    assert_eq!(known.consecutive_failures, 1);
}

/// A peer that answers with somebody else's key is isolated.
///
/// This is the attack the handshake is shaped around: announce a key, sign with
/// another, and every later message gets judged against a key that never signed
/// anything. Getting it wrong is not a transient failure, so the peer table
/// remembers it across a restart.
#[tokio::test]
async fn a_peer_that_signs_with_another_key_is_isolated() {
    let announced = StacksPrivateKey::from_seed(b"announced");
    let signer = StacksPrivateKey::from_seed(b"somebody else");
    let (address, _task) = hostile(move |seq| {
        let accept = nano_p2p::wire::HandshakeAccept {
            handshake: nano_p2p::Handshake {
                address: nano_p2p::PeerAddress::from_bytes([0; 16]),
                port: 20444,
                services: 0,
                public_key: announced.public_key().to_bytes_compressed(),
                expire_bitcoin_height: u64::MAX,
                data_url: String::new(),
            },
            heartbeat_interval: 60,
        };
        Message::sign(
            Protocol::testnet().peer_version,
            Protocol::testnet().network_id,
            &view(900_000),
            seq,
            Payload::HandshakeAccept(accept),
            &signer,
        )
        .expect("sign")
        .encode()
    })
    .await;

    let (round, known) = swarm_against(address).await;
    assert_eq!(round.dialled, 0);
    assert_eq!(round.isolated, 1);
    // Isolated peers stay in the table on the longest backoff it can express,
    // rather than being banned: a node that permanently bans on protocol errors
    // bans the network one deployment at a time.
    assert!(known.consecutive_failures > 4);
    assert!(!known.is_due(Some(0), 3599));
}

/// A peer answering on another network is isolated rather than retried.
#[tokio::test]
async fn a_peer_answering_on_another_network_is_isolated() {
    let key = StacksPrivateKey::from_seed(b"wrong network");
    let (address, _task) = hostile(move |seq| {
        Message::sign(
            // A well-formed message, correctly signed, on mainnet — to a swarm that
            // speaks testnet.
            Protocol::mainnet().peer_version,
            Protocol::mainnet().network_id,
            &view(900_000),
            seq,
            Payload::Pong(1),
            &key,
        )
        .expect("sign")
        .encode()
    })
    .await;

    let (round, known) = swarm_against(address).await;
    assert_eq!(round.dialled, 0);
    assert_eq!(round.isolated, 1);
    assert!(known.consecutive_failures > 4);
}
