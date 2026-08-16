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
use nano_crypto::{MessageSignature, StacksPrivateKey};
use nano_p2p::wire::{ChainView, Message, PREAMBLE_LEN, Payload, Preamble, services};
use nano_p2p::{
    FrameBudget, FrameLimits, InboundLimits, Listener, LocalPeer, PeerDb, Protocol, Service,
    Session, SessionError, Swarm, SwarmLimits, serve_peer,
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

fn view_with_confirmations(height: u64, confirmations: u64) -> ChainView {
    ChainView::with_stable_confirmations(
        height,
        BitcoinHeaderHash::from_bytes([0x5a; 32]),
        BitcoinHeaderHash::from_bytes([0xc3; 32]),
        confirmations,
    )
    .expect("a tip above the confirmation window")
}

/// A node that knows one reward cycle and three neighbours.
#[derive(Default)]
struct TestService {
    height: u64,
    stable_confirmations: Option<u64>,
    offered_blocks: Mutex<Vec<(Hash160, usize)>>,
    offered_transactions: Mutex<Vec<Hash160>>,
}

impl Service for TestService {
    fn chain_view(&self) -> ChainView {
        self.stable_confirmations.map_or_else(
            || view(self.height),
            |confirmations| view_with_confirmations(self.height, confirmations),
        )
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
    listening_with_protocol(service, data_url, Protocol::testnet()).await
}

async fn listening_with_protocol(
    service: Arc<TestService>,
    data_url: Option<&str>,
    protocol: Protocol,
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
                    protocol,
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

/// Regtest's one-confirmation view is protocol-valid on both sides.
#[tokio::test]
async fn a_peer_uses_the_configured_stable_confirmation_window() {
    let protocol = Protocol::testnet()
        .with_stable_confirmations(1)
        .expect("one confirmation is valid");
    let service = Arc::new(TestService {
        height: 900_000,
        stable_confirmations: Some(1),
        ..TestService::default()
    });
    let (address, _, _task) = listening_with_protocol(service, None, protocol).await;
    let dialler = LocalPeer::quiet(StacksPrivateKey::from_seed(b"regtest dialler"), 20444);
    let mut session = Session::open(
        address,
        &dialler,
        protocol,
        view_with_confirmations(900_001, 1),
        TIMEOUT,
    )
    .await
    .expect("the one-confirmation handshake completes");

    assert_eq!(session.remote_view().stable_height, 899_999);
    session.ping().await.expect("the session remains valid");
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

#[tokio::test]
async fn an_advertised_frame_reserves_the_shared_budget_before_its_payload() {
    let protocol = Protocol::testnet();
    let (address, _task) = hostile(move |seq| {
        Preamble {
            peer_version: protocol.peer_version,
            network_id: protocol.network_id,
            seq,
            bitcoin_height: 900_000,
            bitcoin_hash: BitcoinHeaderHash::from_bytes([0x5a; 32]),
            stable_bitcoin_height: 899_993,
            stable_bitcoin_hash: BitcoinHeaderHash::from_bytes([0xc3; 32]),
            additional_data: 0,
            signature: MessageSignature::from_bytes([0; 65]),
            payload_len: 1024,
        }
        .encode()
    })
    .await;
    let budget = FrameBudget::new(FrameLimits::new(512, 512));
    let dialler = LocalPeer::quiet(StacksPrivateKey::from_seed(b"bounded dialler"), 20444);
    let result = Session::open_with_budget(
        address,
        &dialler,
        protocol,
        view(900_000),
        TIMEOUT,
        budget.clone(),
    )
    .await;
    let Err(error) = result else {
        panic!("a frame larger than the shared budget was admitted");
    };
    assert!(matches!(error, SessionError::Overloaded));
    assert_eq!(budget.status().bytes, 0);
    assert_eq!(budget.status().saturations, 1);
}

/// A peer that completes a real handshake and then keeps talking.
///
/// `hostile` answers one message with fixed bytes, which is enough to test a
/// handshake and nothing after it. These two tests are about what a *live* session
/// does with what a peer says in the middle of it, so the peer has to survive past
/// its own handshake.
async fn chatty(
    key: StacksPrivateKey,
    after_handshake: impl FnOnce(Conversation) -> tokio::task::JoinHandle<()> + Send + 'static,
) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let request = read_message(&mut stream)
            .await
            .expect("a handshake arrives");
        let accept = nano_p2p::wire::HandshakeAccept {
            handshake: nano_p2p::Handshake {
                address: nano_p2p::PeerAddress::from_bytes([0; 16]),
                port: address.port(),
                services: 0,
                public_key: key.public_key().to_bytes_compressed(),
                expire_bitcoin_height: u64::MAX,
                data_url: String::new(),
            },
            heartbeat_interval: 60,
        };
        let mut conversation = Conversation { stream, key };
        conversation
            .say(request.preamble.seq, Payload::HandshakeAccept(accept))
            .await;
        let _ = after_handshake(conversation).await;
    });
    address
}

/// One end of a conversation, with the key to sign what it says.
struct Conversation {
    stream: tokio::net::TcpStream,
    key: StacksPrivateKey,
}

impl Conversation {
    async fn say(&mut self, seq: u32, payload: Payload) {
        let message = Message::sign(
            Protocol::testnet().peer_version,
            Protocol::testnet().network_id,
            &view(900_000),
            seq,
            payload,
            &self.key,
        )
        .expect("sign");
        let _ = self.stream.write_all(&message.encode()).await;
    }

    async fn hear(&mut self) -> Option<Message> {
        read_message(&mut self.stream).await
    }
}

/// Read one whole message from a raw stream.
async fn read_message(stream: &mut tokio::net::TcpStream) -> Option<Message> {
    let mut header = [0; PREAMBLE_LEN];
    stream.read_exact(&mut header).await.ok()?;
    let preamble = Preamble::decode(&header).ok()?;
    let mut frame = vec![0; preamble.payload_len as usize];
    stream.read_exact(&mut frame).await.ok()?;
    Message::decode(preamble, frame).ok()
}

/// A peer relaying at mainnet's rate is not a peer that misbehaved.
///
/// This is the regression test for the finding that mattered most in this slice: the
/// running mainnet node was isolating four of seven peers, and the reason was that a
/// session refused more than thirty-two unsolicited messages before a reply. Mainnet
/// pushes signer chunks, blocks and transactions at between 0.2 and 0.8 a second per
/// peer (`tests/live_unsolicited.rs` counted it), and the swarm reads a session once
/// every fifty seconds — so a *fixed count* is crossed by any rate at all given
/// enough silence, and the busiest and most useful peers crossed it first.
#[tokio::test]
async fn a_peer_that_pushes_a_lot_between_rounds_is_not_isolated() {
    // Comfortably past the old limit of thirty-two, and past the buffer bound too, so
    // that both the "this is fine" and the "we are dropping data" halves are checked.
    const PUSHES: u32 = 300;

    let key = StacksPrivateKey::from_seed(b"a busy honest peer");
    let address = chatty(key, |mut conversation| {
        tokio::spawn(async move {
            while let Some(request) = conversation.hear().await {
                // Everything the peer has queued arrives before the answer, which is
                // exactly what fifty seconds of silence produces on mainnet.
                for nonce in 0..PUSHES {
                    conversation
                        .say(
                            0x8000_0000 + nonce,
                            Payload::NatPunchReply(nano_p2p::wire::NatPunch {
                                address: nano_p2p::PeerAddress::from_bytes([0; 16]),
                                port: 20444,
                                nonce,
                            }),
                        )
                        .await;
                }
                let reply = match request.payload {
                    Payload::Ping(nonce) => Payload::Pong(nonce),
                    Payload::GetNeighbors => Payload::Neighbors(Vec::new()),
                    _ => continue,
                };
                conversation.say(request.preamble.seq, reply).await;
            }
        })
    })
    .await;

    let (round, known) = swarm_against(address).await;
    assert_eq!(round.dialled, 1);
    assert_eq!(round.connected, 1);
    assert_eq!(round.isolated, 0, "a peer was isolated for relaying data");
    assert_eq!(round.dropped, 0);
    assert_eq!(known.consecutive_failures, 0);
    // The pushes are collected rather than merely tolerated: a caller that never sees
    // them cannot relay or validate them, which is the next item in task 054. Two
    // requests are made in a round — the ping and the neighbour walk — so the count is
    // what the peer sent, less whatever the bounded buffer had to shed.
    assert!(
        round.collected >= usize::try_from(PUSHES).expect("fits"),
        "collected only {} of {PUSHES} pushes",
        round.collected
    );
}

/// A peer nano dialled can ask nano things, and gets answers.
///
/// The other half of the same finding. `Ping`, `Handshake` and `GetNeighbors` are
/// things a stock node sends on any conversation regardless of who opened it — its
/// heartbeat and its neighbour walk do not care — and nano used to read them, count
/// them as "unsolicited" and never reply. A peer blocked on an answer that never
/// comes is a peer that sets this node aside.
#[tokio::test]
async fn a_peer_we_dialled_gets_its_own_requests_answered() {
    let key = StacksPrivateKey::from_seed(b"a curious peer");
    let announced = key.public_key().to_bytes_compressed();
    let (heard, recorder) = std::sync::mpsc::channel();
    let address = chatty(key.clone(), move |mut conversation| {
        tokio::spawn(async move {
            // Three requests of the peer's own, on its own sequence numbers, with
            // nothing of nano's outstanding.
            conversation.say(0x0101, Payload::Ping(0xfeed)).await;
            conversation.say(0x0202, Payload::GetNeighbors).await;
            conversation
                .say(
                    0x0303,
                    Payload::Handshake(nano_p2p::Handshake {
                        address: nano_p2p::PeerAddress::from_bytes([0; 16]),
                        port: 20444,
                        services: 0,
                        public_key: announced,
                        expire_bitcoin_height: u64::MAX,
                        data_url: String::new(),
                    }),
                )
                .await;
            while let Some(message) = conversation.hear().await {
                if heard
                    .send((message.preamble.seq, message.payload.name().to_owned()))
                    .is_err()
                {
                    return;
                }
                // Answer nano's own liveness check too, so that the assertion at the
                // end — that the session is still nano's to use — is about nano's
                // behaviour rather than about this stub's.
                if let Payload::Ping(nonce) = message.payload {
                    conversation
                        .say(message.preamble.seq, Payload::Pong(nonce))
                        .await;
                }
            }
        })
    })
    .await;

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
    // Without a service there is nothing to answer `GetNeighbors` with, which is
    // correct for a node that serves nothing and is not what this test is about.
    session.serving(Arc::new(TestService {
        height: 900_000,
        ..TestService::default()
    }));

    // Collecting is what the swarm does every round, and it is where a peer's own
    // requests get answered: nothing here is a request of nano's.
    let mut answers = Vec::new();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while answers.len() < 3 && tokio::time::Instant::now() < deadline {
        session.collect().await.expect("collect what the peer sent");
        while let Ok(answer) = recorder.try_recv() {
            answers.push(answer);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    answers.sort();
    assert_eq!(
        answers,
        vec![
            (0x0101, "Pong".to_owned()),
            (0x0202, "Neighbors".to_owned()),
            (0x0303, "HandshakeAccept".to_owned()),
        ],
        "a peer's own requests went unanswered"
    );
    // None of them counted as pushed data: a request answered is not a message the
    // caller has to deal with.
    assert!(session.take_pushed().is_empty());
    // And the session is still nano's to use.
    session.ping().await.expect("the session still works");
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
    let round = swarm.maintain(view(900_001), None).await;
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
    let (first, _, first_task) = listening(service.clone(), Some("http://127.0.0.1:20443")).await;
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

    let round = swarm.maintain(view(900_001), Some(KNOWN_CYCLE)).await;
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
    // All three answered the inventory; the two with an HTTP endpoint are the ones
    // worth fetching from, because the third has nothing at the other end to ask.
    assert_eq!(round.claiming, 2);
    let mut claiming = discovered.claiming();
    claiming.sort();
    assert_eq!(claiming, endpoints);
    // The claims themselves, not only who made them. This is what a forward download
    // schedules from: an endpoint shortlist says which peers to ask, and only the bit
    // vectors say which peer for which tenure. All three peers are here, including the
    // one with no HTTP endpoint — `assign_tenures` drops that one because there is
    // nowhere to fetch from, and dropping it here would make "how many peers claimed
    // this tenure" unanswerable.
    let published = discovered.claims();
    assert_eq!(published.len(), 3);
    assert!(
        published
            .iter()
            .all(|claim| claim.tenures.get(0) == Some(true))
    );
    assert_eq!(
        nano_p2p::assign_tenures(&published, &[0]).len(),
        1,
        "one wanted tenure is one assignment, spread over the peers that claim it"
    );

    // Every peer answers the same inventory question, because one peer's inventory
    // is one peer's claim.
    let claims = swarm
        .tenure_claims(KNOWN_CYCLE, &mut nano_p2p::Round::default())
        .await;
    assert_eq!(claims.len(), 3);
    assert!(
        claims
            .iter()
            .all(|claim| claim.tenures.get(0) == Some(true))
    );
    assert_eq!(
        claims
            .iter()
            .filter(|claim| claim.endpoint.is_some())
            .count(),
        2
    );
    // A nack is an answer and not a fault, so nobody is dropped for it.
    assert!(
        swarm
            .tenure_claims(UNKNOWN_CYCLE, &mut nano_p2p::Round::default())
            .await
            .is_empty()
    );
    assert_eq!(swarm.discovered().connected(), 3);

    // One peer goes away. It is dropped, not isolated: not answering is a restart,
    // and a peer punished for restarting is a peer a small network cannot afford.
    first_task.abort();
    let round = swarm.maintain(view(900_002), Some(KNOWN_CYCLE)).await;
    assert_eq!(round.connected, 2);
    assert_eq!(round.isolated, 0);
    // The one that went away is reported, which is not free: `retire` used to
    // penalise into a `Round` it threw away, so a peer lost during an inventory
    // exchange left the round claiming it still had it.
    assert_eq!(round.dropped, 1);
    // And the two that stayed still answer, which is what the inbound idle budget
    // buys: the round above spent five seconds waiting on the peer that had gone, and
    // a conversation closed at its read deadline would have taken the other two with
    // it.
    assert_eq!(round.claiming, 1);
    assert_eq!(discovered.connected(), 2);
    assert!(
        !discovered
            .endpoints()
            .contains(&"http://127.0.0.1:20443".to_owned())
    );
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
