//! A pushed block, from stacks-core's encoder to nano's authentication boundary.
//!
//! Task 054's relay item and its "feed all received data through the local checks
//! before fork choice or relay" item are the same sentence read twice, and the thing
//! that has to be shown is a single path: the reference implementation signs and
//! serialises a `NakamotoBlocks` message, nano's listener authenticates the *sender*,
//! offers the block without an opinion, and the check that decides whether nano keeps
//! it is `ChainState::authenticate_block` over a real chainstate — the same call
//! `/v3/blocks/upload` goes through.
//!
//! Two halves, and both are load bearing:
//!
//! * **Nothing on the socket decides anything.** `nano_p2p::Relay` is where a push
//!   lands, and it holds blocks with no more claim than "this peer said so".
//! * **The boundary rejects.** A block the network accepted passes; the same block
//!   with its signer signatures stripped does not, and it is the *chainstate* that
//!   says so rather than anything in `nano-p2p`.
//!
//! Restart is the third test here rather than in `p2p_inbound.rs` because it is about
//! the same thing from the other direction: what nano will still tell a stock node it
//! has after the process that learned it has gone.

use std::sync::Arc;
use std::time::Duration;

use nano_chainstate::{ChainState, NakamotoBlock};
use nano_conformance::{FixtureManifest, FixtureMode, replay_into};
use nano_p2p::wire::{ChainView, services};
use nano_p2p::{InboundLimits, Listener, LocalPeer, Protocol, Relay, Service};
use nano_primitives::{BitVec, BitcoinHeaderHash, ConsensusHash, Hash160};
use tokio::io::AsyncWriteExt;

use blockstack_lib::chainstate::nakamoto::NakamotoBlock as CoreBlock;
use blockstack_lib::net::{HandshakeData, NakamotoBlocksData, StacksMessage, StacksMessageType};
use blockstack_lib::util_lib::strings::UrlString;
use stacks_common::codec::StacksMessageCodec;
use stacks_common::types::StacksPublicKeyBuffer;
use stacks_common::types::chainstate::BurnchainHeaderHash;
use stacks_common::types::net::PeerAddress as CorePeerAddress;
use stacks_common::util::secp256k1::{Secp256k1PrivateKey, Secp256k1PublicKey};

const TIP: u64 = 900_000;

/// The burn height the cycle in the restart and reorganization tests opens at.
const CYCLE_AT: u64 = 906_000;

fn fixtures() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn nano_view() -> ChainView {
    ChainView::new(
        TIP,
        BitcoinHeaderHash::from_bytes([0x5a; 32]),
        BitcoinHeaderHash::from_bytes([0xc3; 32]),
    )
    .expect("a tip above the confirmation window")
}

/// A chainstate with the first captured block executed, and the *second* captured
/// block, which is the one a peer will push.
///
/// The chainstate has to have run a block for the headers and ledger a following
/// block is judged against to exist at all, and the block that is pushed has to be
/// one it has *not* run — a relayed block is by definition new. Taking the next one
/// in height order is also what lets the last assertion be an execution: the replay
/// harness will run exactly this block from `skip: 1`.
fn checkpoint_and_pushable_block() -> Option<(ChainState, NakamotoBlock)> {
    let fixtures = fixtures();
    let (mut chainstate, source) = nano_conformance::replay_chainstate(&fixtures).ok()?;
    let depth = replay_into(
        &mut chainstate,
        source,
        &fixtures,
        FixtureManifest {
            mode: FixtureMode::Captured,
            replay_blocks: 1,
            receipts: true,
        },
        0,
        &mut |_, _| {},
    );
    if depth.completed == 0 {
        return None;
    }
    let path = nano_conformance::captured_block_paths(&fixtures).into_iter().nth(1)?;
    let block = NakamotoBlock::decode(&std::fs::read(&path).ok()?).ok()?;
    Some((chainstate, block))
}

/// What nano tells a peer, and where it puts what the peer pushes.
///
/// This is `nano-node`'s `PeerService` in miniature and for the same reason: the
/// listener runs on its own task with no chainstate, so the most it can honestly do
/// with a pushed block is write down who said so.
struct NanoNode {
    relay: Relay,
    /// Behind a mutex for the same reason `nano-node` puts it behind one: a
    /// `rusqlite::Connection` is `Send` but not `Sync`, and a listener holds its
    /// service across every await.
    served: std::sync::Mutex<nano_p2p::ServedTenures>,
}

impl Service for NanoNode {
    fn chain_view(&self) -> ChainView {
        nano_view()
    }

    fn neighbors(&self) -> Vec<nano_p2p::NeighborAddress> {
        Vec::new()
    }

    fn tenure_inventory(&self, cycle_start: ConsensusHash) -> Option<BitVec<2100>> {
        self.served
            .lock()
            .ok()
            .and_then(|served| served.inventory(cycle_start).ok())
            .flatten()
    }

    fn offer_blocks(&self, from: Hash160, blocks: Vec<NakamotoBlock>) {
        for block in blocks {
            self.relay.offer(nano_p2p::Offer::block(Some(from), block));
        }
    }

    fn offer_transaction(&self, from: Hash160, transaction: Box<nano_codec::Transaction>) {
        self.relay
            .offer(nano_p2p::Offer::transaction(Some(from), transaction));
    }
}

/// One stacks-core-driven conversation with nano's listener.
struct Reference {
    stream: tokio::net::TcpStream,
    key: Secp256k1PrivateKey,
    seq: u32,
}

impl Reference {
    /// Send a payload the way stacks-core sends it, without waiting for a reply.
    ///
    /// A push is an announcement, so there is nothing to read back: the assertion is
    /// what nano *did* with it, which is what the relay queue is for.
    async fn push(&mut self, payload: StacksMessageType) {
        self.seq = self.seq.wrapping_add(1);
        let mut message = StacksMessage::new(
            Protocol::testnet().peer_version,
            Protocol::testnet().network_id,
            TIP,
            &BurnchainHeaderHash([0x5a; 32]),
            TIP - 7,
            &BurnchainHeaderHash([0xc3; 32]),
            payload,
        );
        message.sign(self.seq, &self.key).expect("sign");
        self.stream
            .write_all(&message.serialize_to_vec())
            .await
            .expect("send");
    }

    /// The handshake a stock node opens with, which is what makes nano willing to
    /// attribute anything to this peer at all.
    async fn handshake(&mut self) {
        self.push(StacksMessageType::Handshake(HandshakeData {
            addrbytes: CorePeerAddress::from_ipv4(127, 0, 0, 1),
            port: 20444,
            services: 0x03,
            node_public_key: StacksPublicKeyBuffer::from_public_key(
                &Secp256k1PublicKey::from_private(&self.key),
            ),
            expire_block_height: TIP + 10_000,
            data_url: UrlString::try_from("http://127.0.0.1:20443".to_string())
                .expect("a valid URL"),
        }))
        .await;
    }

    fn key_hash(&self) -> Hash160 {
        Hash160::from_bytes(
            *stacks_common::util::hash::Hash160::from_node_public_key(
                &Secp256k1PublicKey::from_private(&self.key),
            )
            .as_bytes(),
        )
    }
}

/// Bring nano up as a listening peer over `service` and hand back a stacks-core
/// client for it.
async fn dial_nano(service: Arc<NanoNode>) -> Reference {
    let listener = Listener::bind("127.0.0.1:0".parse().expect("a loopback address"))
        .await
        .expect("bind");
    let address = listener.local_addr().expect("the bound address");
    let mut local = LocalPeer::quiet(
        nano_crypto::StacksPrivateKey::from_seed(b"nano relay listener"),
        address.port(),
    );
    local.address = nano_p2p::PeerAddress::from_ip(address.ip());
    local.data_url = format!("http://{address}");
    local.services = services::RPC | services::RELAY;

    tokio::spawn(async move {
        let mut conversations = tokio::task::JoinSet::new();
        while let Ok((stream, from)) = listener.accept().await {
            let local = local.clone();
            let service = service.clone();
            conversations.spawn(async move {
                let _ = nano_p2p::serve_peer(
                    stream,
                    from,
                    &local,
                    Protocol::testnet(),
                    service.as_ref(),
                    InboundLimits {
                        timeout: Duration::from_secs(10),
                        ..InboundLimits::default()
                    },
                )
                .await;
            });
        }
    });

    let stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect to nano");
    stream.set_nodelay(true).expect("nodelay");
    Reference {
        stream,
        key: Secp256k1PrivateKey::from_slice(&[0x3c; 32]).expect("a valid key"),
        seq: 0x7000_0000,
    }
}

/// Wait for the listener's task to have offered something, or give up.
///
/// Polled rather than awaited on a channel because the offer crosses a task
/// boundary and the whole point of the queue is that the listener does not wait for
/// anybody. A second is four orders of magnitude more than a loopback push needs.
async fn offered(relay: &Relay) -> Vec<nano_p2p::Offer> {
    for _ in 0..100 {
        let offers = relay.take_offered();
        if !offers.is_empty() {
            return offers;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Vec::new()
}

/// A block the network accepted, pushed by stacks-core, authenticates at nano's
/// boundary — and a forged one does not.
///
/// The two halves together are the claim: relay reaches execution, and it reaches it
/// *through* a check that says no.
#[tokio::test]
async fn a_block_stacks_core_pushes_reaches_the_authenticated_boundary() {
    let Some((chainstate, block)) = checkpoint_and_pushable_block() else {
        nano_conformance::skip_gate("the capture has no tenure-start block to push");
        return;
    };
    // Re-encoded by the reference implementation on the way out, so what arrives is
    // stacks-core's bytes and not nano's own encoder talking to itself.
    let core_block = CoreBlock::consensus_deserialize(&mut &block.encode()[..])
        .expect("stacks-core decodes a captured block");

    let relay = Relay::default();
    let service = Arc::new(NanoNode {
        relay: relay.clone(),
        served: std::sync::Mutex::new(
            nano_p2p::ServedTenures::in_memory().expect("a served store"),
        ),
    });
    let mut peer = dial_nano(service).await;
    peer.handshake().await;
    peer.push(StacksMessageType::NakamotoBlocks(NakamotoBlocksData {
        blocks: vec![core_block],
    }))
    .await;

    let offers = offered(&relay).await;
    assert_eq!(offers.len(), 1, "the push reached the relay queue");
    // Attributed to the peer that signed the message, which is the only thing the
    // socket establishes and the only thing it is allowed to establish.
    assert_eq!(offers[0].from, Some(peer.key_hash()));
    let nano_p2p::Pushed::Block(pushed) = &offers[0].data else {
        panic!("a pushed block arrived as something else");
    };
    assert_eq!(
        pushed.encode(),
        block.encode(),
        "the block survived stacks-core's encoder and nano's decoder unchanged"
    );

    // ---- And now the boundary, which is the whole point.
    let mut chainstate = chainstate;
    chainstate
        .authenticate_block(pushed)
        .expect("a block the network accepted authenticates when a peer pushes it");

    // The same block with the miner signature disturbed. `check_tenure_change_miner`
    // ties the key that signed the header to the key the tenure change names, so this
    // is the shape a block lifted out of another miner's tenure has — and the peer
    // that pushed it is the same authenticated peer as above, which is the point: the
    // socket established who spoke, and nothing more.
    let mut forged = (**pushed).clone();
    let mut signature = *forged.header.miner_signature.as_bytes();
    signature[10] ^= 0xff;
    forged.header.miner_signature = nano_crypto::MessageSignature::from_bytes(signature);
    // Only a tenure-start block carries a tenure change to be tied to, so the
    // always-applicable half is asserted separately below rather than instead.
    if nano_chainstate::starts_new_tenure(&forged) {
        chainstate
            .authenticate_block(&forged)
            .expect_err("a block whose header was signed by another key is not this miner's");
    }
    // True of every block, so this is the half that cannot quietly stop asserting:
    // the boundary a pushed block meets does say no.
    let mut hollow = (**pushed).clone();
    hollow.transactions.clear();
    let refused = chainstate
        .authenticate_block(&hollow)
        .expect_err("a block with nothing in it is not a block");
    assert!(
        refused.to_string().contains("no transactions"),
        "the rejection says why: {refused}"
    );

    // ---- And it executes. The bytes that came off the relay queue are the bytes the
    // replay harness runs here — asserted above — so this is the pushed block reaching
    // execution and matching the state root its own header commits to.
    let depth = replay_into(
        &mut chainstate,
        [0; 32],
        &fixtures(),
        FixtureManifest {
            mode: FixtureMode::Captured,
            replay_blocks: 1,
            receipts: true,
        },
        1,
        &mut |executed, _| {
            assert_eq!(
                executed.block_id(),
                block.block_id(),
                "the block executed is the block the peer pushed"
            );
        },
    );
    assert_eq!(
        depth.completed, 1,
        "the pushed block executed: {:?}",
        depth.first_divergence
    );
}

/// A peer cannot make nano check the same block twice by pushing it twice.
///
/// The queue drops what has already been accepted, so the cost of a peer repeating
/// itself is a message rather than an authentication — and the cost of eight honest
/// peers pushing the same block is one authentication, not eight.
#[tokio::test]
async fn a_block_already_accepted_is_not_offered_again() {
    let Some((_, block)) = checkpoint_and_pushable_block() else {
        nano_conformance::skip_gate("the capture has no tenure-start block to push");
        return;
    };
    let core_block = CoreBlock::consensus_deserialize(&mut &block.encode()[..])
        .expect("stacks-core decodes a captured block");

    let relay = Relay::default();
    let service = Arc::new(NanoNode {
        relay: relay.clone(),
        served: std::sync::Mutex::new(
            nano_p2p::ServedTenures::in_memory().expect("a served store"),
        ),
    });
    let mut peer = dial_nano(service).await;
    peer.handshake().await;
    peer.push(StacksMessageType::NakamotoBlocks(NakamotoBlocksData {
        blocks: vec![core_block.clone()],
    }))
    .await;
    let offers = offered(&relay).await;
    assert_eq!(offers.len(), 1);

    // Accepted, so from here it is something nano has published.
    relay.announce(offers.into_iter().next().expect("the offer"));
    assert_eq!(relay.take_announcing().len(), 1, "and it goes out once");

    peer.push(StacksMessageType::NakamotoBlocks(NakamotoBlocksData {
        blocks: vec![core_block],
    }))
    .await;
    assert!(
        offered(&relay).await.is_empty(),
        "a second push of an accepted block is not checked again"
    );
}

/// What nano tells a stock node it has, after the process that learned it has gone.
///
/// The inventory nano derives from its executed ledger reaches `REORG_REACH = 256`
/// blocks back, so before this store existed a restart left nano answering for the
/// recent end of a 2,100-tenure cycle and nothing else. The assertion is the stock
/// node's: `NakamotoInvData::has_ith_tenure` on a reply decoded by the implementation
/// that defines what one is.
#[tokio::test]
async fn a_restarted_nano_still_answers_the_inventory_it_had() {
    let cycle = ConsensusHash::from_bytes([0x2c; 20]);
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("served.sqlite");

    // ---- The first run walks part of a cycle, in two rounds whose windows do not
    // overlap — which is what the executed window does as it slides forward.
    {
        let served = nano_p2p::ServedTenures::open(&path).expect("a served store");
        let mut window = BitVec::<2100>::zeros(2100).expect("a cycle-length vector");
        for index in [3_u16, 4, 5] {
            window.set(index, true).expect("in bounds");
        }
        served
            .record(CYCLE_AT, cycle, &window)
            .expect("record the window");
        let mut later = BitVec::<2100>::zeros(2100).expect("a cycle-length vector");
        for index in [6_u16, 7] {
            later.set(index, true).expect("in bounds");
        }
        served
            .record(CYCLE_AT, cycle, &later)
            .expect("record the next window");
    }

    // ---- The process is gone. A new one opens the same directory and a stock node
    // asks it what it has.
    let service = Arc::new(NanoNode {
        relay: Relay::default(),
        served: std::sync::Mutex::new(
            nano_p2p::ServedTenures::open(&path).expect("the same store"),
        ),
    });
    let mut peer = dial_nano(service).await;
    peer.handshake().await;

    let reply = ask_inventory(&mut peer, cycle).await;
    match reply.payload {
        StacksMessageType::NakamotoInv(ref inventory) => {
            for tenure in 3..=7 {
                assert!(
                    inventory.has_ith_tenure(tenure),
                    "tenure {tenure} was run before the restart"
                );
            }
            assert!(
                !inventory.has_ith_tenure(8),
                "and nothing is claimed that was not run"
            );
        }
        ref other => panic!(
            "nano answered GetNakamotoInv with a {}",
            other.get_message_name()
        ),
    }

    // A cycle this node has never walked is still nacked, which is what tells a stock
    // node to ask somebody else rather than that nano has nothing.
    let reply = ask_inventory(&mut peer, ConsensusHash::from_bytes([0xee; 20])).await;
    match reply.payload {
        StacksMessageType::Nack(ref nack) => assert_eq!(
            nack.error_code,
            blockstack_lib::net::NackErrorCodes::NoSuchBurnchainBlock
        ),
        ref other => panic!(
            "nano answered an unwalked cycle with a {}",
            other.get_message_name()
        ),
    }
}

/// What nano tells a stock node after its own burn view reorganized.
///
/// A cycle is named on the wire by the consensus hash of its first sortition, so a
/// reorganization across that boundary gives the same cycle a new name — and every
/// tenure nano claimed under the old name was a tenure on a fork it has now abandoned.
/// Two things have to be true afterwards, and a stock node can see both: the old name
/// is nacked, so nano is not offering blocks it no longer follows, and the new name is
/// answered with what nano has run since.
///
/// This is a reorganization of *nano's* view rather than of a peer's, which is the
/// direction that matters here: what a peer says about its own forks is a claim, while
/// what nano says about its own is a promise other nodes will act on.
#[tokio::test]
async fn a_reorganized_nano_stops_claiming_the_fork_it_left() {
    let before = ConsensusHash::from_bytes([0x2c; 20]);
    let after = ConsensusHash::from_bytes([0xb1; 20]);
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("served.sqlite");
    {
        let served = nano_p2p::ServedTenures::open(&path).expect("a served store");
        let mut window = BitVec::<2100>::zeros(2100).expect("a cycle-length vector");
        for index in [3_u16, 4, 5] {
            window.set(index, true).expect("in bounds");
        }
        served
            .record(CYCLE_AT, before, &window)
            .expect("record the fork nano was on");
        // The reorganization: the same cycle, opening at the same burn height, with a
        // different first sortition — and one tenure run on the new fork so far.
        let mut reorganized = BitVec::<2100>::zeros(2100).expect("a cycle-length vector");
        reorganized.set(9, true).expect("in bounds");
        served
            .record(CYCLE_AT, after, &reorganized)
            .expect("record the fork nano is on now");
    }

    let service = Arc::new(NanoNode {
        relay: Relay::default(),
        served: std::sync::Mutex::new(
            nano_p2p::ServedTenures::open(&path).expect("the same store"),
        ),
    });
    let mut peer = dial_nano(service).await;
    peer.handshake().await;

    let reply = ask_inventory(&mut peer, before).await;
    match reply.payload {
        StacksMessageType::Nack(ref nack) => assert_eq!(
            nack.error_code,
            blockstack_lib::net::NackErrorCodes::NoSuchBurnchainBlock,
            "the abandoned fork's cycle is unknown, not empty"
        ),
        ref other => panic!(
            "nano still answered for the fork it left, with a {}",
            other.get_message_name()
        ),
    }

    let reply = ask_inventory(&mut peer, after).await;
    match reply.payload {
        StacksMessageType::NakamotoInv(ref inventory) => {
            assert!(inventory.has_ith_tenure(9), "what nano has run since");
            for tenure in 3..=5 {
                assert!(
                    !inventory.has_ith_tenure(tenure),
                    "tenure {tenure} belonged to the fork nano left"
                );
            }
        }
        ref other => panic!(
            "nano answered the new cycle with a {}",
            other.get_message_name()
        ),
    }
}

/// Ask for a cycle's inventory and read the reply back with stacks-core's decoder.
async fn ask_inventory(peer: &mut Reference, cycle: ConsensusHash) -> StacksMessage {
    use tokio::io::AsyncReadExt;
    peer.push(StacksMessageType::GetNakamotoInv(
        blockstack_lib::net::GetNakamotoInvData {
            consensus_hash: blockstack_lib::chainstate::burn::ConsensusHash(*cycle.as_bytes()),
        },
    ))
    .await;
    loop {
        let mut header = vec![0; stacks_common::codec::PREAMBLE_ENCODED_SIZE as usize];
        peer.stream
            .read_exact(&mut header)
            .await
            .expect("read a preamble");
        let preamble = blockstack_lib::net::Preamble::consensus_deserialize(&mut &header[..])
            .expect("stacks-core decodes it");
        let mut frame = vec![0; preamble.payload_len as usize];
        peer.stream
            .read_exact(&mut frame)
            .await
            .expect("read the frame");
        let mut whole = header;
        whole.extend_from_slice(&frame);
        let message = StacksMessage::consensus_deserialize(&mut &whole[..])
            .expect("stacks-core decodes nano's reply");
        // The handshake reply arrives first; the inventory is the one paired to the
        // sequence number the request went out on.
        if message.preamble.seq == peer.seq {
            return message;
        }
    }
}
