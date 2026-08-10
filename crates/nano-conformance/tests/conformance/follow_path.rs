//! Two peers and a burnchain that moves, through the production follow path.
//!
//! `peer_equivocation` weighs candidate tips, which is the *choice*; this drives
//! `CheckpointExecutor::catch_up` — the loop a running node runs — against real
//! HTTP peers over the captured chain, so what is exercised is the descent, the
//! staging store, execution, the state-root check and the fork switch together.
//!
//! Two cases, and neither is a malformed message:
//!
//! - **A peer serving a coherent but wrong chain.** Well-formed blocks that link
//!   to each other, carry real transactions, real Merkle roots and real state
//!   roots, and belong to no chain the reward set signed. That is the strongest
//!   lie an attacker can actually build: signer signatures cannot be forged, so
//!   any alternative history is one the signers of this cycle never put their
//!   names to, and there is nothing else about it to notice. [[027]] names it as
//!   the case `peer_equivocation` cannot reach.
//! - **A Bitcoin reorganization.** The burnchain the node reads gives back a
//!   block it had snapshotted, which invalidates a sortition and every Stacks
//!   block that stood on it. No signature and no state root says anything about
//!   this: a chain executed over a burnchain nobody else is on is perfectly
//!   self-consistent.
//!
//! Everything is offline: the captured 340-block chain, its Bitcoin blocks and
//! its snapshots, served from loopback. No capture, no environment variable, so
//! neither gate can skip itself.

use std::{
    collections::BTreeMap,
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use axum::{Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use nano_bitcoin::{BitcoinBlock, BitcoinSource};
use nano_chainstate::{BitcoinBlockContext, NakamotoBlock};
use nano_node::{CatchUpBudget, CheckpointExecutor, staging::Staging};
use nano_primitives::ConsensusHash;
use nano_sync::{PeerPool, PoxInfo, SyncClient, TenureSource};
use serde::Deserialize;

/// How many sortitions stacks-core will walk back in one `fork_info` answer,
/// from `stackslib/src/net/api/get_tenures_fork_info.rs:38`.
const DEPTH_LIMIT: usize = 10;

pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// One row of the captured `snapshots` table, in the fields a peer answers with.
#[derive(Clone, Debug, Deserialize)]
pub struct Snapshot {
    pub block_height: u64,
    pub burn_header_hash: String,
    sortition_id: String,
    parent_sortition_id: String,
    burn_header_timestamp: u64,
    pub consensus_hash: String,
    pub sortition: u8,
    #[serde(default)]
    miner_pk_hash: Option<String>,
}

pub fn snapshots() -> Vec<Snapshot> {
    serde_json::from_slice(&fs::read(fixtures().join("sortition/snapshots.json")).expect("read"))
        .expect("the captured snapshots parse")
}

/// The captured chain, lowest block first.
pub fn captured_chain() -> Vec<NakamotoBlock> {
    let mut blocks: Vec<NakamotoBlock> = nano_conformance::captured_block_paths(&fixtures())
        .into_iter()
        .map(|path| {
            NakamotoBlock::decode(&fs::read(&path).expect("read a captured block"))
                .expect("a captured block decodes")
        })
        .collect();
    blocks.sort_by_key(|block| block.header.chain_length);
    blocks
}

/// The Bitcoin blocks under the captured chain, decoded as the node decodes them.
///
/// Read through `decode_block_with_pre_stx` in height order, because a `PreStx`
/// output authorises an operation up to six blocks later and a block decoded on
/// its own would drop those pairings.
pub fn captured_burnchain() -> BTreeMap<u64, BitcoinBlock> {
    let mut rows = snapshots();
    rows.sort_by_key(|snapshot| snapshot.block_height);
    let mut cache = nano_bitcoin::PreStxCache::new();
    let mut blocks = BTreeMap::new();
    for snapshot in rows {
        let path = fixtures()
            .join("bitcoin/blocks")
            .join(format!("{}.hex", snapshot.burn_header_hash));
        let Ok(encoded) = fs::read_to_string(&path) else {
            continue;
        };
        let raw = hex::decode(encoded.trim()).expect("the captured block is hexadecimal");
        let block = nano_bitcoin::decode_block_with_pre_stx(
            snapshot.block_height,
            &raw,
            *b"T3",
            &mut cache,
        )
        .expect("a captured Bitcoin block decodes");
        blocks.insert(snapshot.block_height, block);
    }
    blocks
}

/// A burnchain a test can move underneath the node.
///
/// `block_hash_at` answers from the same map `block_at` reads, so replacing a
/// block is the whole of a reorganization from the node's point of view — which
/// is exactly what a node sees: Bitcoin does not announce a reorganization, it
/// answers differently at a height it answered before.
#[derive(Clone, Debug)]
pub struct MovableBurnchain {
    blocks: Arc<std::sync::Mutex<BTreeMap<u64, BitcoinBlock>>>,
}

impl MovableBurnchain {
    pub fn new(blocks: BTreeMap<u64, BitcoinBlock>) -> Self {
        Self {
            blocks: Arc::new(std::sync::Mutex::new(blocks)),
        }
    }

    /// Extend this burnchain with deterministic empty blocks.
    pub fn extend_empty(&self, count: u64) -> u64 {
        let mut blocks = self.blocks.lock().expect("the burnchain is not poisoned");
        let start = blocks.keys().next_back().copied().unwrap_or_default();
        for height in (start + 1)..=(start + count) {
            let mut hash = [0_u8; 32];
            hash[..8].copy_from_slice(&height.to_be_bytes());
            blocks.insert(
                height,
                BitcoinBlock {
                    height,
                    hash,
                    timestamp: height,
                    operations: Vec::new(),
                },
            );
        }
        drop(blocks);
        start + count
    }

    /// Replace the block at a height with one carrying a different hash.
    ///
    /// The operations are kept: a reorganization that also emptied the block
    /// would be two changes at once, and the one under test is that the *block*
    /// this node snapshotted is no longer the one Bitcoin holds.
    fn reorganize(&self, height: u64) {
        let mut blocks = self.blocks.lock().expect("the burnchain is not poisoned");
        if let Some(block) = blocks.get_mut(&height) {
            block.hash[0] ^= 0xff;
        }
    }
}

impl BitcoinSource for MovableBurnchain {
    type Error = String;

    fn block_at(&mut self, height: u64) -> Result<BitcoinBlock, Self::Error> {
        self.blocks
            .lock()
            .expect("the burnchain is not poisoned")
            .get(&height)
            .cloned()
            .ok_or_else(|| format!("no captured Bitcoin block at height {height}"))
    }

    fn block_hash_at(&self, height: u64) -> Result<[u8; 32], Self::Error> {
        self.blocks
            .lock()
            .expect("the burnchain is not poisoned")
            .get(&height)
            .map(|block| block.hash)
            .ok_or_else(|| format!("no captured Bitcoin block at height {height}"))
    }

    /// The highest captured block, which is where this burnchain ends.
    ///
    /// A fixture has a tip like any other burnchain, and the node's walk forward is
    /// bounded by it: without one the walk would read past the capture and read a
    /// missing block as a burnchain failure.
    fn tip_height(&self) -> Result<u64, Self::Error> {
        self.blocks
            .lock()
            .expect("the burnchain is not poisoned")
            .keys()
            .next_back()
            .copied()
            .ok_or_else(|| "the captured burnchain is empty".to_owned())
    }
}

/// How a peer behaves badly, in the three ways a real one does.
///
/// Every field is deterministic and counted from the requests the node actually
/// makes, so a scenario is reproducible without a clock, a sleep or a random
/// number: the same node makes the same requests in the same order and meets the
/// same refusals. Shared through `Arc` so a test keeps a handle on the policy it
/// gave a peer and can move the peer's tip or start refusing between rounds.
#[derive(Clone, Debug, Default)]
pub struct Policy {
    /// Answer 429 to every *n*th request. `Some(1)` refuses everything.
    refuse_every: Option<usize>,
    /// Answer at most this many requests, then rate-limit the rest.
    refuse_after: Option<usize>,
    /// Answer 429 to everything while this is set.
    pub refusing: Arc<AtomicBool>,
    /// Answer 429 to tenure requests only, while this is set.
    ///
    /// A peer that still says where its tip is and will not serve the history
    /// below it, which is what a rate limit keyed by cost looks like — and the
    /// only way to drive a descent into the throttle bookkeeping, since a peer
    /// that refuses everything is never asked for a tenure at all.
    pub refusing_tenures: Arc<AtomicBool>,
    /// At most this many blocks in one `/v3/tenures/:id` answer.
    page: Option<usize>,
    /// How many of the served blocks are visible. Zero means all of them.
    visible: Arc<AtomicUsize>,
    /// Make one more block visible every *n*th request, so the tip moves while a
    /// round is in flight rather than only between rounds.
    reveal_every: Option<usize>,
    /// Every path this peer was asked for, in order, and whether it answered.
    asked: Arc<Mutex<Vec<(String, bool)>>>,
}

impl Policy {
    /// Refuse every *n*th request with a 429.
    #[must_use]
    pub const fn refusing_every(mut self, requests: usize) -> Self {
        self.refuse_every = Some(requests);
        self
    }

    /// Answer `requests` times, then refuse every later request.
    #[must_use]
    pub const fn refusing_after(mut self, requests: usize) -> Self {
        self.refuse_after = Some(requests);
        self
    }

    /// Never answer a tenure with more than this many blocks.
    #[must_use]
    pub const fn paged(mut self, blocks: usize) -> Self {
        self.page = Some(blocks);
        self
    }

    /// Start with `visible` blocks and admit one more every *n*th request.
    #[must_use]
    pub fn revealing(mut self, visible: usize, every: usize) -> Self {
        self.visible = Arc::new(AtomicUsize::new(visible));
        self.reveal_every = Some(every);
        self
    }

    /// How many times this peer was asked for a sortition, at all.
    ///
    /// The counter [[049]]'s first acceptance criterion is about: a node with a
    /// burnchain of its own derives the burn view a block stands on, so this is zero
    /// however the peer would have answered. Counted over every request rather than
    /// the answered ones, because a refused request is still a request made.
    pub fn sortitions_asked(&self) -> usize {
        self.asked
            .lock()
            .expect("the log is not poisoned")
            .iter()
            .filter(|(path, _)| path.starts_with("/v3/sortitions"))
            .count()
    }

    /// The burn views whose tenures were asked of this peer, in order.
    ///
    /// The request only an inventory-driven forward schedule makes: `/v3/tenures/:id`
    /// is a backward walk's request and needs an identifier from the answer above,
    /// while a burn view is derived locally and names a tenure before anything above
    /// it is known. Recording which views were asked of *which* peer is how "the
    /// schedule honoured the inventory" becomes a measurement.
    pub fn tenures_asked_by_view(&self) -> Vec<String> {
        self.asked
            .lock()
            .expect("the log is not poisoned")
            .iter()
            .filter_map(|(path, _)| path.strip_prefix("/v3/tenures/fork_info/"))
            .filter_map(|range| range.split_once('/'))
            .filter(|(start, stop)| start == stop)
            .map(|(start, _)| start.trim_start_matches("0x").to_lowercase())
            .collect()
    }

    /// How many of them it answered with a 429.
    pub fn refusals(&self) -> usize {
        self.asked
            .lock()
            .expect("the log is not poisoned")
            .iter()
            .filter(|(_, admitted)| !admitted)
            .count()
    }

    /// The tenures this peer actually answered, in order, as the block asked for.
    ///
    /// Answered rather than asked: the client retries a 429 on its own, so a
    /// refused request appears several times over and would look like a caller
    /// that walked the same history twice.
    pub fn tenures_served(&self) -> Vec<String> {
        self.asked
            .lock()
            .expect("the log is not poisoned")
            .iter()
            .filter(|(_, admitted)| *admitted)
            .filter_map(|(path, _)| tenure_asked_for(path))
            .map(str::to_owned)
            .collect()
    }

    /// How many content-addressed block requests reached this peer.
    pub fn blocks_asked(&self) -> usize {
        self.asked
            .lock()
            .expect("the log is not poisoned")
            .iter()
            .filter(|(path, _)| path.starts_with("/v3/blocks/"))
            .count()
    }

    /// Record a request and say whether this one is refused.
    fn admits(&self, path: &str) -> bool {
        let mut asked = self.asked.lock().expect("the log is not poisoned");
        let count = asked.len() + 1;
        // The tip moves off the node's own requests rather than off a clock, so a
        // round is interrupted at the same point on every run.
        if self
            .reveal_every
            .is_some_and(|every| count.is_multiple_of(every))
        {
            self.visible.fetch_add(1, Ordering::SeqCst);
        }
        let refused = self.refusing.load(Ordering::SeqCst)
            || (self.refusing_tenures.load(Ordering::SeqCst) && tenure_asked_for(path).is_some())
            || self.refuse_after.is_some_and(|after| count > after)
            || self
                .refuse_every
                .is_some_and(|every| count.is_multiple_of(every));
        let admitted = !refused;
        asked.push((path.to_owned(), admitted));
        admitted
    }
}

/// The block a request asks for the tenure of, if that is what it asks.
///
/// `/v3/tenures/info` travels the same prefix and is a question about the peer
/// rather than about a tenure, so it is told apart by the block identifier's
/// length rather than by the route.
fn tenure_asked_for(path: &str) -> Option<&str> {
    path.strip_prefix("/v3/tenures/")
        .filter(|block| block.len() == 64)
}

/// What one fake peer serves.
pub struct Served {
    /// The chain it offers, lowest first. Its last block is its tip.
    pub blocks: Vec<NakamotoBlock>,
    pub snapshots: Vec<Snapshot>,
    /// How it misbehaves while serving them.
    pub policy: Policy,
    /// Whether every consensus field in a served sortition is adversarial.
    lying_sortitions: bool,
    /// Whether a tenure asked for by burn view is answered with another tenure's
    /// blocks.
    lying_tenures: bool,
    /// Whether a content-addressed block request returns another held block.
    lying_blocks: bool,
}

impl Served {
    /// A peer that answers everything it holds, as fast as it is asked.
    pub fn honest(blocks: Vec<NakamotoBlock>, snapshots: Vec<Snapshot>) -> Self {
        Self {
            blocks,
            snapshots,
            policy: Policy::default(),
            lying_sortitions: false,
            lying_tenures: false,
            lying_blocks: false,
        }
    }

    /// The same peer, answering every scheduled tenure with somebody else's blocks.
    #[must_use]
    pub const fn lying_about_tenures(mut self) -> Self {
        self.lying_tenures = true;
        self
    }

    /// The same peer, answering each block request with a different held block.
    #[must_use]
    pub const fn answering_the_wrong_block(mut self) -> Self {
        self.lying_blocks = true;
        self
    }

    /// The same peer, misbehaving.
    #[must_use]
    pub fn under(mut self, policy: Policy) -> Self {
        self.policy = policy;
        self
    }

    /// The same peer, lying about every execution-visible sortition field.
    #[must_use]
    pub const fn lying_about_sortitions(mut self) -> Self {
        self.lying_sortitions = true;
        self
    }

    /// The blocks this peer is currently willing to admit it has.
    ///
    /// A prefix rather than the whole chain, because that is what a tip moving
    /// looks like from outside: the peer answered for fewer blocks a moment ago
    /// and answers for more now, and nothing else about it changed.
    fn visible(&self) -> &[NakamotoBlock] {
        let visible = self.policy.visible.load(Ordering::SeqCst);
        let upto = if visible == 0 {
            self.blocks.len()
        } else {
            visible.min(self.blocks.len())
        };
        &self.blocks[..upto]
    }

    fn block(&self, id: &str) -> Option<Vec<u8>> {
        let blocks = self.visible();
        let wanted = |block: &&NakamotoBlock| hex::encode(block.block_id()) == id;
        if self.lying_blocks {
            blocks.iter().find(wanted)?;
            return blocks
                .iter()
                .find(|block| !wanted(block))
                .map(NakamotoBlock::encode);
        }
        blocks.iter().find(wanted).map(NakamotoBlock::encode)
    }

    /// Every block of the tenure the named block belongs to, back to back.
    ///
    /// This is what `/v3/tenures/:id` answers, and it is what makes a descent one
    /// request per tenure rather than one per block.
    ///
    /// A `page` cuts the answer to the named block and the ones just below it,
    /// which is the shape a bounded response has: a peer that will not serve a
    /// whole tenure in one body serves the top of it and leaves the rest to be
    /// asked for again from lower down.
    fn tenure(&self, id: &str) -> Option<Vec<u8>> {
        let blocks = self.visible();
        let named = blocks
            .iter()
            .find(|block| hex::encode(block.block_id()) == id)?;
        let tenure: Vec<&NakamotoBlock> = blocks
            .iter()
            .filter(|block| block.header.consensus_hash == named.header.consensus_hash)
            .filter(|block| block.header.chain_length <= named.header.chain_length)
            .collect();
        let page = self.policy.page.unwrap_or(tenure.len()).max(1);
        Some(
            tenure[tenure.len().saturating_sub(page)..]
                .iter()
                .flat_map(|block| block.encode())
                .collect(),
        )
    }

    fn tip(&self) -> &NakamotoBlock {
        self.visible().last().expect("a served chain has a tip")
    }

    fn info(&self) -> serde_json::Value {
        let tip = self.tip();
        let start = self
            .visible()
            .iter()
            .find(|block| block.header.consensus_hash == tip.header.consensus_hash)
            .unwrap_or(tip);
        serde_json::json!({
            "consensus_hash": hex::encode(tip.header.consensus_hash.as_bytes()),
            "tenure_start_block_id": hex::encode(start.block_id()),
            "parent_consensus_hash": hex::encode(tip.header.consensus_hash.as_bytes()),
            "parent_tenure_start_block_id": hex::encode(start.block_id()),
            "tip_block_id": hex::encode(tip.block_id()),
            "tip_height": tip.header.chain_length,
            "reward_cycle": 0,
        })
    }

    /// `/v2/info`, which is how a starting node learns the chain and how far
    /// ahead this peer is.
    fn node_info(&self) -> serde_json::Value {
        let tip = self.tip();
        serde_json::json!({
            "burn_block_height": self
                .snapshots
                .iter()
                .find(|snapshot| snapshot.consensus_hash == tip.header.consensus_hash.to_string())
                .map_or(0, |snapshot| snapshot.block_height),
            "stacks_tip_height": tip.header.chain_length,
            "stacks_tip": hex::encode(tip.header.block_hash().as_bytes()),
            "stacks_tip_consensus_hash": hex::encode(tip.header.consensus_hash.as_bytes()),
            "network_id": CHAIN_ID,
        })
    }

    /// `/v2/pox`, the stacking calendar every execution context is built from.
    fn pox_info(&self) -> serde_json::Value {
        let calendar = pox();
        serde_json::json!({
            "first_burnchain_block_height": calendar.first_bitcoin_height,
            "current_burnchain_block_height": self
                .snapshots
                .iter()
                .map(|snapshot| snapshot.block_height)
                .max()
                .unwrap_or_default(),
            "prepare_phase_block_length": calendar.prepare_phase_length,
            "reward_phase_block_length": calendar.reward_phase_length,
            "reward_slots": calendar.reward_slots,
            "rejection_fraction": serde_json::Value::Null,
            // Where pox-5 activates, which is how a node learns the waterfall
            // opens and so how many outputs a commitment pays. `pox()` states it
            // for the rigs that build a context in process; the ones that run the
            // shipped binary read it here and got `None`, which counts two payout
            // outputs on a chain that pays one — so every candidate's burn read as
            // its own change, nothing was elected, and every consensus hash derived
            // above the seed was wrong. A live `/v2/pox` states it, so serving it
            // is this fixture catching up with production rather than a
            // convenience.
            "contract_versions": [{
                "contract_id": "ST000000000000000000002AMW42H.pox-5",
                "activation_burnchain_block_height": POX_5_ACTIVATION_HEIGHT,
                "first_reward_cycle_id": 0,
            }],
            "epochs": [],
        })
    }

    fn sortition(&self, snapshot: &Snapshot) -> serde_json::Value {
        // The winning commitment's seed, which the tenure's `vrf-seed` reads
        // back: taken from the captured Bitcoin block rather than from the
        // snapshot row, which does not record it.
        let vrf_seed = nano_conformance::captured_bitcoin_snapshots(&fixtures())
            .and_then(|contexts| contexts.get(&snapshot.consensus_hash).map(|c| c.vrf_seed))
            .unwrap_or_default();
        let previous = self
            .snapshots
            .iter()
            .filter(|other| other.block_height < snapshot.block_height && other.sortition == 1)
            .max_by_key(|other| other.block_height);
        let mut answer = serde_json::json!({
            "burn_block_hash": format!("0x{}", snapshot.burn_header_hash),
            "burn_block_height": snapshot.block_height,
            "burn_header_timestamp": snapshot.burn_header_timestamp,
            "sortition_id": format!("0x{}", snapshot.sortition_id),
            "parent_sortition_id": format!("0x{}", snapshot.parent_sortition_id),
            "consensus_hash": format!("0x{}", snapshot.consensus_hash),
            "was_sortition": snapshot.sortition == 1,
            "miner_pk_hash160": snapshot.miner_pk_hash.as_ref().map(|hash| format!("0x{hash}")),
            "stacks_parent_ch": serde_json::Value::Null,
            "last_sortition_ch": previous.map_or(serde_json::Value::Null, |other| {
                serde_json::Value::String(format!("0x{}", other.consensus_hash))
            }),
            "committed_block_hash": serde_json::Value::Null,
            "vrf_seed": format!("0x{}", hex::encode(vrf_seed)),
        });
        if self.lying_sortitions {
            answer["burn_block_hash"] = serde_json::json!(format!("0x{}", "11".repeat(32)));
            answer["burn_block_height"] = serde_json::json!(snapshot.block_height + 1_000_000);
            answer["burn_header_timestamp"] = serde_json::json!(u64::MAX);
            answer["sortition_id"] = serde_json::json!(format!("0x{}", "22".repeat(32)));
            answer["parent_sortition_id"] = serde_json::json!(format!("0x{}", "33".repeat(32)));
            answer["was_sortition"] = serde_json::json!(snapshot.sortition != 1);
            answer["last_sortition_ch"] = serde_json::json!(format!("0x{}", "44".repeat(20)));
            answer["vrf_seed"] = serde_json::json!(format!("0x{}", "55".repeat(32)));
        }
        answer
    }

    /// The burn view between two consensus hashes, newest first.
    ///
    /// Each entry carries the whole tenure that burn block elected, hex-encoded, as
    /// stacks-core's `prefix_opt_hex_codec` states it and as nano's own RPC does. That
    /// is what makes a tenure addressable by the burn view that elected it rather than
    /// by a block identifier only the answer above it carries — the request an
    /// inventory-driven forward schedule makes.
    ///
    /// The consensus hash is stated `0x`-prefixed here, because that is what both real
    /// implementations state and a client that could not read it could not read either.
    fn fork_info(&self, recurse_end: &str, start_from: &str) -> Option<serde_json::Value> {
        let height = |hash: &str| {
            self.snapshots
                .iter()
                .find(|snapshot| snapshot.consensus_hash == hash)
                .map(|snapshot| snapshot.block_height)
        };
        let (Some(stop), Some(start)) = (height(recurse_end), height(start_from)) else {
            return Some(serde_json::Value::Array(Vec::new()));
        };
        // stacks-core refuses a walk whose first path element does not bound the
        // second from below. Equal elements are the valid single-tenure case.
        if recurse_end != start_from && stop >= start {
            return None;
        }
        let mut rows: Vec<_> = self
            .snapshots
            .iter()
            .filter(|snapshot| snapshot.block_height >= stop && snapshot.block_height <= start)
            .collect();
        rows.sort_by_key(|snapshot| std::cmp::Reverse(snapshot.block_height));
        // stacks-core walks back from the cursor and gives up after `DEPTH_LIMIT`
        // sortitions, answering 200 with a *truncated* body and saying nothing
        // about it (`get_tenures_fork_info.rs:38`). A harness that answered the
        // whole range let a client that asked once look correct, which is how a
        // fork point thirty-eight burn blocks down came to read as no fork at all
        // ([[096-cross-a-stacks-fork-inside-one-sortition-chain]]). The cursor
        // itself is pushed before the walk starts, so it is never counted.
        let mut depth = 0;
        let entries: Vec<serde_json::Value> = rows
            .into_iter()
            .take_while(|snapshot| {
                let room = depth <= DEPTH_LIMIT;
                if snapshot.sortition == 1 {
                    depth += 1;
                }
                room
            })
            .map(|snapshot| {
                let tenure = self.tenure_of(&snapshot.consensus_hash);
                serde_json::json!({
                    "burn_block_height": snapshot.block_height,
                    "consensus_hash": format!("0x{}", snapshot.consensus_hash),
                    "was_sortition": snapshot.sortition == 1,
                    "first_block_mined": tenure
                        .first()
                        .map(|block| format!("0x{}", hex::encode(block.block_id()))),
                    "nakamoto_blocks": format!("0x{}", hex::encode(encode_blocks(&tenure))),
                })
            })
            .collect();
        Some(serde_json::Value::Array(entries))
    }

    /// Every block this peer holds of the tenure a burn view elected.
    ///
    /// `lying_tenures` answers with the blocks of a *different* tenure instead, which
    /// is the one substitution a fetch addressed by burn view is open to: nothing in
    /// the answer is the identifier that was asked for, so only the view each block's
    /// own header states can refuse it.
    fn tenure_of(&self, consensus_hash: &str) -> Vec<NakamotoBlock> {
        let wanted = |block: &NakamotoBlock| block.header.consensus_hash.to_string();
        let answer = if self.lying_tenures {
            self.visible()
                .iter()
                .map(wanted)
                .find(|hash| hash != consensus_hash)
                .unwrap_or_else(|| consensus_hash.to_owned())
        } else {
            consensus_hash.to_owned()
        };
        self.visible()
            .iter()
            .filter(|block| wanted(block) == answer)
            .cloned()
            .collect()
    }
}

/// A block vector in the consensus encoding: a big-endian count, then the blocks.
fn encode_blocks(blocks: &[NakamotoBlock]) -> Vec<u8> {
    let count = u32::try_from(blocks.len()).unwrap_or(u32::MAX);
    let mut bytes = count.to_be_bytes().to_vec();
    for block in blocks {
        bytes.extend(block.encode());
    }
    bytes
}

/// What a peer answers for bytes it either holds or does not.
fn found(body: Option<Vec<u8>>) -> (StatusCode, Vec<u8>) {
    body.map_or_else(
        || (StatusCode::NOT_FOUND, Vec::new()),
        |body| (StatusCode::OK, body),
    )
}

/// One place every request passes through, so a policy applies to all of them.
///
/// A rate limit is not a property of an endpoint — a throttled peer refuses the
/// sortition a block needs as readily as the tenure above it — so refusing per
/// route would only exercise the descent and never execution.
///
/// `Retry-After: 0` is answered deliberately: `SyncClient` honours what it is
/// told as given, so this keeps the client's own three retries in the test
/// without making the suite wait seconds for each one. What that costs is that
/// the wait itself is not what is being pinned here; `nano-sync`'s own tests
/// cover the honouring.
async fn gate(
    State(state): State<Arc<Served>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if state.policy.admits(request.uri().path()) {
        return next.run(request).await;
    }
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(axum::http::header::RETRY_AFTER, "0")],
        Vec::new(),
    )
        .into_response()
}

/// Start a peer on loopback and hand back a client pointed at it.
pub async fn serve(served: Served) -> (SyncClient, tokio::task::JoinHandle<()>) {
    let state = Arc::new(served);
    let router = Router::new()
        .route(
            "/v2/info",
            get(|State(state): State<Arc<Served>>| async move { axum::Json(state.node_info()) }),
        )
        .route(
            "/v2/pox",
            get(|State(state): State<Arc<Served>>| async move { axum::Json(state.pox_info()) }),
        )
        .route(
            "/v3/tenures/info",
            get(|State(state): State<Arc<Served>>| async move { axum::Json(state.info()) }),
        )
        .route(
            "/v3/blocks/{id}",
            get(|State(state): State<Arc<Served>>, axum::extract::Path(id): axum::extract::Path<String>| async move {
                found(state.block(&id.trim_start_matches("0x").to_lowercase()))
            }),
        )
        .route(
            "/v3/tenures/{id}",
            get(|State(state): State<Arc<Served>>, axum::extract::Path(id): axum::extract::Path<String>| async move {
                found(state.tenure(&id.trim_start_matches("0x").to_lowercase()))
            }),
        )
        .route(
            "/v3/sortitions/consensus/{hash}",
            get(|State(state): State<Arc<Served>>, axum::extract::Path(hash): axum::extract::Path<String>| async move {
                let hash = hash.trim_start_matches("0x").to_lowercase();
                let found = state
                    .snapshots
                    .iter()
                    .find(|snapshot| snapshot.consensus_hash == hash)
                    .map(|snapshot| state.sortition(snapshot));
                found.map_or_else(
                    || (StatusCode::NOT_FOUND, axum::Json(serde_json::Value::Null)),
                    |sortition| (StatusCode::OK, axum::Json(serde_json::json!([sortition]))),
                )
            }),
        )
        .route(
            "/v3/sortitions/burn_height/{height}",
            get(|State(state): State<Arc<Served>>, axum::extract::Path(height): axum::extract::Path<u64>| async move {
                let found = state
                    .snapshots
                    .iter()
                    .find(|snapshot| snapshot.block_height == height)
                    .map(|snapshot| state.sortition(snapshot));
                found.map_or_else(
                    || (StatusCode::NOT_FOUND, axum::Json(serde_json::Value::Null)),
                    |sortition| (StatusCode::OK, axum::Json(serde_json::json!([sortition]))),
                )
            }),
        )
        .route(
            "/v3/tenures/fork_info/{start}/{stop}",
            get(|State(state): State<Arc<Served>>, axum::extract::Path((start, stop)): axum::extract::Path<(String, String)>| async move {
                state
                    .fork_info(
                        &start.trim_start_matches("0x").to_lowercase(),
                        &stop.trim_start_matches("0x").to_lowercase(),
                    )
                    .map_or_else(
                        || {
                            (
                                StatusCode::BAD_REQUEST,
                                axum::Json(serde_json::json!(
                                    "Supplied start and end sortitions are not in the same fork"
                                )),
                            )
                        },
                        |body| (StatusCode::OK, axum::Json(body)),
                    )
            }),
        )
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            gate,
        ))
        .with_state(state);
    // Port zero: several of these run inside one test binary and a fixed port
    // would make them collide.
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind a loopback port");
    let address = listener.local_addr().expect("the bound address");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let client =
        SyncClient::new(reqwest::Url::parse(&format!("http://{address}/")).expect("a base url"))
            .expect("a client");
    (client, handle)
}

/// The chain identifier the captured chain carries, from its own provenance.
pub const CHAIN_ID: u32 = 2_147_483_648;

/// The stacking calendar the captured chain was produced under.
///
/// `pox_5_activation_height` is the chain's own `pox_v4_unlock_height`, which every
/// captured `new_block` event states. It was `None` here, and that is not a
/// harmless omission: the waterfall opens with the reward cycle after the one
/// pox-5 activates in, and under the waterfall a commitment pays **one** output
/// where a classic reward phase pays two. Without it `payout_schedule` counted two
/// on a chain that pays one, so every candidate's burn read as its own change, no
/// sortition was elected at any block of this capture and its running burn total
/// never moved. `/v2/pox` states the field on a live chain, so this is a fixture
/// that was missing what production is given.
pub const fn pox() -> PoxInfo {
    PoxInfo {
        first_bitcoin_height: 0,
        bitcoin_height: 0,
        prepare_phase_length: 5,
        reward_phase_length: 15,
        reward_slots: 30,
        rejection_fraction: None,
        pox_5_activation_height: Some(POX_5_ACTIVATION_HEIGHT),
        v1_unlock_height: None,
        v2_unlock_height: None,
        v3_unlock_height: None,
    }
}

/// The captured chain's `pox_v4_unlock_height`, as its `new_block` events state it.
///
/// With a 20-block cycle starting at Bitcoin 0 this puts the waterfall at burn 280,
/// and the capture's snapshots agree without being asked: their `pox_payouts`
/// column switches from two classic reward addresses to one `Addr32` P2TR — the
/// sBTC taproot output — at exactly 280.
pub const POX_5_ACTIVATION_HEIGHT: u32 = 262;

/// The capture starts on the last block of an already-running tenure. Its first
/// tenure change therefore commits a nine-block parent tenure while only one of
/// those blocks is present. Replay that incomplete boundary explicitly through
/// the fixture-only API; every block followed after it has a complete local
/// tenure ledger and goes through production authentication.
const FIXTURE_BOUNDARY_BLOCKS: usize = 2;

/// A node standing on the first complete tenure boundary in the capture.
///
/// Durable, in a directory of its own, because a retraction stands on the ledger
/// the surviving block committed and an in-memory state has none to read back.
/// The opener is `restart::open`, which is the one that recovers the ledger a
/// sealed block committed — so a directory this has been pointed at twice
/// resumes rather than starting over, and a catch-up across a restart is the
/// same code path as a catch-up that was never interrupted.
pub fn node(
    directory: &Path,
    burnchain: MovableBurnchain,
) -> (CheckpointExecutor<MovableBurnchain>, Vec<NakamotoBlock>) {
    let fixtures = fixtures();
    let (mut chainstate, source) = crate::restart::open(directory);
    let chain = captured_chain();
    let replay = nano_conformance::replay_into(
        &mut chainstate,
        source,
        &fixtures,
        nano_conformance::FixtureManifest {
            mode: nano_conformance::FixtureMode::Captured,
            replay_blocks: u64::try_from(FIXTURE_BOUNDARY_BLOCKS).expect("the prefix fits"),
            receipts: true,
        },
        0,
        &mut |_, _| {},
    );
    assert_eq!(
        replay.completed,
        u64::try_from(FIXTURE_BOUNDARY_BLOCKS).expect("the prefix fits"),
        "the explicit fixture boundary did not replay: {replay:?}"
    );
    let anchor = chain
        .get(FIXTURE_BOUNDARY_BLOCKS - 1)
        .expect("the capture has its boundary blocks")
        .clone();
    assert_eq!(
        chainstate.tip().expect("the fixture tip reads"),
        Some(*anchor.block_id().as_bytes()),
        "the fixture prefix did not seal its boundary"
    );
    let mut executor = CheckpointExecutor::resume(chainstate, anchor, burnchain);
    nano_conformance::derive_sortitions(&mut executor, &fixtures, directory);
    (executor, chain)
}

/// A coherent alternative history: the captured blocks, re-timed and re-linked.
///
/// Every block is well-formed and every one links to the block before it, so a
/// descent walks the whole branch and a state root is the one the network
/// computed for that content. What cannot be reproduced is the signatures: they
/// are made over a preimage containing the timestamp, so each one recovers to a
/// key the reward set does not hold. That is not a weakness of the fixture — it
/// is the reason a wrong chain is refusable at all, and an attacker faces
/// exactly this.
pub fn alternative_history(chain: &[NakamotoBlock], from: usize) -> Vec<NakamotoBlock> {
    let mut branch = chain[..from].to_vec();
    let mut parent = branch.last().expect("a fork point").block_id();
    for block in &chain[from..] {
        let mut forged = block.clone();
        forged.header.parent_block_id = parent;
        forged.header.timestamp = block.header.timestamp.saturating_add(1);
        parent = forged.block_id();
        branch.push(forged);
    }
    branch
}

/// A peer serving a coherent wrong chain moves nothing, and loses the choice.
///
/// Three claims, because a refusal is only worth something with the control
/// beside it: the fork choice takes the honest peer although the liar's chain is
/// longer, the follow path executes none of what the liar serves, and the same
/// checkpoint follows the honest peer to its tip.
///
/// The liar forks at the anchor rather than further up, so that nothing it serves
/// is executable. A branch sharing a prefix would have nano execute that prefix —
/// correctly, since it is the canonical chain — and "nothing moved" would then be
/// a claim about the fixture rather than about the rule.
///
/// Two nodes on two state directories, because staging is keyed by parent block:
/// one store holding both branches would hand either round whichever child of the
/// anchor it found first, which is a property of a test double and not of a node.
#[tokio::test]
async fn a_peer_serving_a_coherent_wrong_chain_moves_nothing() {
    let chain = captured_chain();
    let honest: Vec<_> = chain[..8].to_vec();
    let liar = alternative_history(&chain[..16], FIXTURE_BOUNDARY_BLOCKS);
    assert!(
        liar.last().expect("a tip").header.chain_length
            > honest.last().expect("a tip").header.chain_length,
        "the wrong chain is not longer, so nothing is being tested"
    );

    let (honest_client, honest_task) = serve(Served::honest(honest.clone(), snapshots())).await;
    let (lying_client, lying_task) = serve(Served::honest(liar, snapshots())).await;

    let against_the_liar = tempfile::tempdir().expect("a directory");
    let burnchain = MovableBurnchain::new(captured_burnchain());
    let (mut executor, _) = node(against_the_liar.path(), burnchain.clone());
    // The set the fork choice weighs against is the one this node's own executed
    // state records for the cycle its burn view sits in, read the way the follow
    // path reads it — not a document either peer served.
    let signers = executor
        .recorded_signer_set(pox().bitcoin_context())
        .expect("the captured chain records a signer set for its cycle");

    let pool = PeerPool::new(vec![honest_client.clone(), lying_client.clone()]);
    let candidates = pool.candidate_tips().await;
    assert_eq!(candidates.len(), 2, "both peers answered");
    // Length alone takes the liar, which is the control for the rule below: it is
    // the weight that decides, rather than the fixture being unattractive.
    assert_eq!(
        nano_sync::choose_canonical_tip(&candidates, None, None).map(|tip| tip.peer),
        Some(1),
        "the wrong chain does not win on length, so this proves nothing about weight"
    );
    assert_eq!(
        nano_sync::choose_canonical_tip(&candidates, Some(&signers), None).map(|tip| tip.peer),
        Some(0),
        "the fork choice followed a chain the recorded signer set never approved"
    );

    // Now the follow path itself, against the liar alone: the half a fork choice
    // cannot answer, because a node whose only peer serves a wrong chain has to
    // execute none of it rather than the best of it.
    let staging =
        Staging::open(&against_the_liar.path().join("staging.sqlite")).expect("staging opens");
    let mut history = TenureSource::only(lying_client.clone());
    let before = executor.tip().block_id();
    let executed_before = executor.chainstate_mut().executed_blocks();
    let budget = CatchUpBudget {
        fetch: 64,
        execute: 64,
    };
    let outcome = executor
        .catch_up(&lying_client, &mut history, &pox(), &staging, budget, &[])
        .await;
    // The round *fails*, and that shape is deliberate rather than incidental: a
    // block that cannot be executed ends the round, which is what sets
    // `peer_failed` in the runtime and makes the next round weigh the pool again.
    // A round that quietly executed nothing would leave the node on the liar.
    match outcome {
        Ok(round) => assert_eq!(
            round.executed, 0,
            "blocks from a chain no signer approved were executed"
        ),
        // Which signature rule fires depends on where the branch parts, and both
        // say the same thing: nobody who could have signed this block did. A
        // tenure-start block is refused by the *miner* rule, because the header
        // signature no longer recovers to the key the tenure change names; a
        // mid-tenure block is refused by the reward set's weight. This branch
        // parts at a tenure start, so it is the miner rule that answers — the
        // weight rule is what refuses the same branch at the fork choice above,
        // and `signer_weight_enforcement` puts it against execution directly.
        Err(error) => {
            let refusal = error.to_string();
            assert!(
                refusal.contains("signer") || refusal.contains("miner"),
                "the refusal is not about a signature, so it may be about the fixture: {refusal}"
            );
        }
    }
    assert!(
        staging.len().expect("the staging store answers") > 0,
        "the round staged nothing, so the refusal is about the transport rather \
         than about the chain"
    );
    assert_eq!(executor.tip().block_id(), before, "the executed tip moved");
    assert_eq!(
        executor.chainstate_mut().executed_blocks(),
        executed_before,
        "the executed chain changed"
    );

    // And the same checkpoint follows the honest peer to its tip, which is what
    // makes the refusal above a judgement rather than an inability.
    let against_the_honest = tempfile::tempdir().expect("a directory");
    let (mut executor, _) = node(against_the_honest.path(), burnchain);
    let staging =
        Staging::open(&against_the_honest.path().join("staging.sqlite")).expect("staging opens");
    let mut history = TenureSource::only(honest_client.clone());
    let round = executor
        .catch_up(&honest_client, &mut history, &pox(), &staging, budget, &[])
        .await
        .expect("the captured chain executes");
    assert!(
        round.executed > 0,
        "the honest peer's chain executed nothing either, so the test says nothing"
    );
    assert_eq!(
        executor.tip().block_id(),
        honest.last().expect("a tip").block_id(),
        "the node did not reach the honest peer's tip"
    );

    honest_task.abort();
    lying_task.abort();
}

/// The blocks of a tenure a peer's burn view no longer holds, on a view of its own.
///
/// Every block under `disputed` is re-tenured to `replacement` and re-linked, so
/// the branch parts from this node's chain exactly where the two burn views part —
/// which is the only way a *Stacks* fork can be resolved by naming a tenure. Two
/// branches inside one tenure cannot be: a consensus hash is a fact about a burn
/// block, so both branches carry the same one, and the last block this node
/// executed under it is on the branch it is already standing on. That is why
/// `switch_to_fork` answers with a tenure and not with a block.
fn parted_view(
    chain: &[NakamotoBlock],
    disputed: ConsensusHash,
    replacement: ConsensusHash,
) -> Vec<NakamotoBlock> {
    let mut branch = Vec::new();
    let mut reparent = None;
    for block in chain {
        if block.header.consensus_hash != disputed {
            branch.push(block.clone());
            continue;
        }
        let mut moved = block.clone();
        moved.header.consensus_hash = replacement;
        if let Some(parent) = reparent {
            moved.header.parent_block_id = parent;
        }
        reparent = Some(moved.block_id());
        branch.push(moved);
    }
    branch
}

/// A burn view walk reaches past what one `fork_info` answer carries.
///
/// stacks-core stops after ten sortitions and answers 200 with a truncated body,
/// stating nothing about having stopped. So the bound a caller asks for and the
/// bound it gets back are different bounds, and the difference is silent: every
/// fork deeper than ten burn blocks read as no fork at all, which on mainnet was
/// a fork thirty-eight deep ([[096-cross-a-stacks-fork-inside-one-sortition-chain]]).
///
/// Asked of the client rather than through the node, because this is a claim
/// about one request being made into as many as the answer needs.
#[tokio::test]
async fn a_burn_view_walk_reaches_past_one_answer() {
    let (client, task) = serve(Served::honest(captured_chain(), snapshots())).await;
    let hash = |snapshot: &Snapshot| {
        ConsensusHash::from_bytes(
            hex::decode(&snapshot.consensus_hash)
                .expect("a captured consensus hash is hexadecimal")
                .try_into()
                .expect("a consensus hash is twenty bytes"),
        )
    };
    let mut elected: Vec<_> = snapshots()
        .into_iter()
        .filter(|snapshot| snapshot.sortition == 1)
        .collect();
    elected.sort_by_key(|snapshot| snapshot.block_height);
    assert!(
        elected.len() > DEPTH_LIMIT * 3,
        "the captured burnchain is too short to walk past one answer"
    );
    let newest = hash(elected.last().expect("a newest sortition"));
    let bound = &elected[elected.len() - 1 - DEPTH_LIMIT * 3];

    let walked = client
        .tenure_fork_info(newest, hash(bound))
        .await
        .expect("the peer answers");

    assert!(
        walked
            .iter()
            .any(|entry| entry.consensus_hash == hash(bound)),
        "the walk stopped short of the bound it was given: {} sortitions, down to burn {:?}",
        walked.len(),
        walked.last().map(|entry| entry.bitcoin_height)
    );
    assert!(
        walked.iter().filter(|entry| entry.was_sortition).count() > DEPTH_LIMIT,
        "the walk carried no more than a single answer's worth of sortitions"
    );

    task.abort();
}

/// A tenure that won its sortition and was built around is given back.
///
/// The mainnet stall of 2026-08-08, reduced: this node executes the block its
/// tenure produced, the next sortition's miner commits to the tenure *before* it,
/// and the chain carries on one block to the side. Both branches stand on the same
/// unreorganized sortition chain and both tenures are canonical burn views, so
/// there is no consensus hash present on one side and absent on the other —
/// `switch_to_fork` matches this node's own tip tenure, retracts nothing, and the
/// node holds fifteen hundred unexecutable blocks for as long as it runs
/// ([[096-cross-a-stacks-fork-inside-one-sortition-chain]]).
///
/// What the round does instead is read the answer off its own two records: the
/// lowest block staging holds, and whether this node executed that block's parent.
/// The last assertion is the point — no peer was asked where the chains parted,
/// because no peer could have been believed about it.
fn execute_fixture_orphan(
    directory: &Path,
    burnchain: MovableBurnchain,
    chain: &[NakamotoBlock],
    agreed: [u8; 32],
) -> (CheckpointExecutor<MovableBurnchain>, Vec<[u8; 32]>) {
    let mut orphan = chain[11].clone();
    orphan.header.timestamp = orphan.header.timestamp.saturating_add(1);
    let orphan_id = orphan.block_id();
    assert_ne!(
        orphan_id,
        chain[11].block_id(),
        "the orphan is not a sibling"
    );
    let orphan_bitcoin_height = snapshots()
        .into_iter()
        .find(|snapshot| snapshot.consensus_hash == orphan.header.consensus_hash.to_string())
        .map(|snapshot| snapshot.block_height)
        .expect("the captured burnchain names the orphan's canonical view");
    let (mut chainstate, _) = crate::restart::open(directory);
    chainstate
        .execute_unauthenticated_fixture_block_with_bitcoin_operations(
            BitcoinBlockContext::at_height(orphan_bitcoin_height),
            &[],
            Some(agreed),
            &orphan,
        )
        .expect("the fixture orphan executes");
    let mut executor = CheckpointExecutor::resume(chainstate, orphan, burnchain);
    nano_conformance::derive_sortitions(&mut executor, &fixtures(), directory);
    let executed = executor.chainstate_mut().executed_blocks();
    assert_eq!(
        executed.last().copied(),
        Some(*orphan_id.as_bytes()),
        "the executed chain does not end at the tip"
    );
    (executor, executed)
}

#[tokio::test]
async fn a_branch_that_parts_at_a_block_is_followed_onto_the_fork() {
    let chain = captured_chain();
    let honest: Vec<_> = chain[..11].to_vec();
    let (honest_client, honest_task) = serve(Served::honest(honest.clone(), snapshots())).await;

    let directory = tempfile::tempdir().expect("a directory");
    let burnchain = MovableBurnchain::new(captured_burnchain());
    let (mut executor, _) = node(directory.path(), burnchain.clone());
    let staging = Staging::open(&directory.path().join("staging.sqlite")).expect("staging opens");
    let budget = CatchUpBudget {
        fetch: 64,
        execute: 64,
    };
    let mut history = TenureSource::only(honest_client.clone());
    executor
        .catch_up(&honest_client, &mut history, &pox(), &staging, budget, &[])
        .await
        .expect("the captured chain executes");
    let agreed = *executor.tip().block_id().as_bytes();
    drop(executor);

    // Put one locally executed orphan above the common block. Only this setup
    // block uses the explicit fixture seam: changing its timestamp changes its
    // identity and invalidates its captured signature. The replacement branch
    // below is the byte-exact captured chain and goes through production signer,
    // tenure, VRF and state-root checks.
    let (mut executor, executed_before) =
        execute_fixture_orphan(directory.path(), burnchain.clone(), &chain, agreed);

    // The branch: the same height as this node's orphan, a different block on it,
    // and four blocks above — all byte-exact captured blocks under burn views this
    // node's own sortition chain holds.
    let branch = chain[..16].to_vec();
    let branch_tip = branch.last().expect("replacement tip").block_id();
    let branch_policy = Policy::default();
    let (branch_client, branch_task) =
        serve(Served::honest(branch, snapshots()).under(branch_policy.clone())).await;

    let mut history = TenureSource::only(branch_client.clone());
    let round = executor
        .catch_up(&branch_client, &mut history, &pox(), &staging, budget, &[])
        .await
        .expect("a branch parting at a block is not an error");
    assert_eq!(
        round.executed, 0,
        "a block whose parent this node did not execute was executed"
    );
    assert_eq!(
        round.reorganized,
        Some(agreed),
        "the round did not stand on the last block both branches descend from"
    );
    assert_eq!(
        *executor.tip().block_id().as_bytes(),
        agreed,
        "the executor kept standing on the block it had given back"
    );
    let executed_after = executor.chainstate_mut().executed_blocks();
    assert_eq!(
        executed_after.last().copied(),
        Some(agreed),
        "the chain does not end at the block the switch named"
    );
    assert!(
        executed_before.starts_with(&executed_after),
        "the surviving chain is not a prefix of the one that was executed"
    );
    assert!(
        !staging.is_empty().expect("staging answers"),
        "the switch threw away the branch it had just decided to execute"
    );

    drop(executor);
    let (chainstate, _) = crate::restart::open(directory.path());
    let agreed_block = honest
        .iter()
        .find(|block| *block.block_id().as_bytes() == agreed)
        .expect("the common block is in the served chain")
        .clone();
    let mut executor = CheckpointExecutor::resume(chainstate, agreed_block, burnchain);
    nano_conformance::derive_sortitions(&mut executor, &fixtures(), directory.path());

    let next = executor
        .catch_up(&branch_client, &mut history, &pox(), &staging, budget, &[])
        .await
        .expect("the retained replacement branch executes");
    assert!(next.executed > 0, "the replacement branch made no progress");
    assert_eq!(
        executor.tip().block_id(),
        branch_tip,
        "the executor did not reach the replacement branch's tip"
    );
    assert!(
        staging.is_empty().expect("staging answers"),
        "executed replacement blocks remain staged"
    );
    let asked = branch_policy.asked.lock().expect("the log is not poisoned");
    assert!(
        !asked.iter().any(|(path, _)| {
            path.strip_prefix("/v3/tenures/fork_info/")
                .and_then(|rest| rest.split_once('/'))
                .is_some_and(|(start, stop)| start != stop)
        }),
        "this node asked a peer where two chains parted instead of reading it off \
         the blocks it already held"
    );
    drop(asked);

    honest_task.abort();
    branch_task.abort();
}

/// A peer whose burn view parted from this node's is followed onto the fork.
///
/// The Stacks half of a reorganization, through the production loop: nothing about
/// the peer's blocks is malformed, they simply descend from a tenure this node did
/// not execute. `TenureFollower` used to answer `SyncError::Fork` and stop there;
/// what happens now is that the round fetches them, executes none of them, and
/// takes *that* as the question worth asking — `/v3/tenures/fork_info` back to the
/// oldest tenure this node executed, against the tenures it executed, and standing
/// on the last block of the one they agree about.
///
/// Both sides are checked and neither is taken on trust, which is what the last two
/// assertions are about: the block this node stands on is one it executed itself,
/// and everything kept below it is untouched.
#[tokio::test]
async fn a_peer_on_a_parted_burn_view_is_followed_onto_the_fork() {
    let chain = captured_chain();
    let honest: Vec<_> = chain[..12].to_vec();
    let (honest_client, honest_task) = serve(Served::honest(honest.clone(), snapshots())).await;

    let directory = tempfile::tempdir().expect("a directory");
    let burnchain = MovableBurnchain::new(captured_burnchain());
    let (mut executor, _) = node(directory.path(), burnchain);
    let staging = Staging::open(&directory.path().join("staging.sqlite")).expect("staging opens");
    let budget = CatchUpBudget {
        fetch: 64,
        execute: 64,
    };
    let mut history = TenureSource::only(honest_client.clone());
    executor
        .catch_up(&honest_client, &mut history, &pox(), &staging, budget, &[])
        .await
        .expect("the captured chain executes");
    let tenures = executor.chainstate_mut().executed_tenures();
    assert!(
        tenures.len() >= 2,
        "the executed chain spans one tenure, so no fork point below its tip exists"
    );
    let disputed = *tenures.first().expect("a newest tenure");
    let agreed = *tenures.get(1).expect("a tenure below it");
    let oldest = *tenures.last().expect("an oldest tenure");
    let executed_before = executor.chainstate_mut().executed_blocks();
    let stands_on = executor
        .chainstate_mut()
        .last_block_of_tenure(agreed)
        .expect("this node executed a block under the agreed tenure");

    // The peer's view: the disputed tenure replaced by one of its own, in its
    // blocks and in the burn view it answers `fork_info` from.
    let replacement = ConsensusHash::from_bytes([0x5f; 20]);
    let mut parted = snapshots();
    for row in &mut parted {
        if row.consensus_hash == disputed.to_string() {
            row.consensus_hash = replacement.to_string();
        }
    }
    let parted_policy = Policy::default();
    let (parted_client, parted_task) = serve(
        Served::honest(parted_view(&honest, disputed, replacement), parted)
            .under(parted_policy.clone()),
    )
    .await;

    let mut history = TenureSource::only(parted_client.clone());
    let round = executor
        .catch_up(&parted_client, &mut history, &pox(), &staging, budget, &[])
        .await
        .expect("a peer on another fork is not an error");
    assert_eq!(
        round.executed, 0,
        "blocks from a tenure this node did not execute were executed"
    );
    assert_eq!(
        round.reorganized,
        Some(stands_on),
        "the round did not stand on the last block of the tenure both chains hold"
    );
    assert_eq!(
        *executor.tip().block_id().as_bytes(),
        stands_on,
        "the executor kept standing on a block it had given back"
    );
    let executed_after = executor.chainstate_mut().executed_blocks();
    assert!(
        executed_after.len() < executed_before.len(),
        "the switch discarded nothing"
    );
    assert!(
        executed_before.starts_with(&executed_after),
        "the surviving chain is not a prefix of the one that was executed"
    );
    assert_eq!(
        executed_after.last().copied(),
        Some(stands_on),
        "the chain does not end at the block the switch named"
    );
    let expected_path = format!("/v3/tenures/fork_info/{oldest}/{replacement}");
    assert!(
        parted_policy
            .asked
            .lock()
            .expect("the log is not poisoned")
            .iter()
            .any(|(path, admitted)| path == &expected_path && *admitted),
        "the client did not put stacks-core's older bound before its newer cursor"
    );

    honest_task.abort();
    parted_task.abort();
}

/// A Bitcoin reorganization gives back the sortitions and the blocks on them.
///
/// The event nothing else in the suite reaches through the production loop: the
/// burnchain answers differently at a height this node snapshotted, which
/// invalidates that sortition and every Stacks block executed under it. Neither a
/// signature nor a state root says anything about it — a chain executed over an
/// abandoned burnchain is perfectly self-consistent — so the only thing that can
/// notice is the node's own snapshots, compared against Bitcoin.
///
/// Which is why the chain here is *derived* rather than quoted: `find_fork` walks
/// the snapshots this node took, and a node seeded from a checkpoint and no
/// further has one snapshot and nothing above it to give back.
#[tokio::test]
async fn a_bitcoin_reorganization_retracts_the_blocks_it_invalidated() {
    let chain = captured_chain();
    let (client, task) = serve(Served::honest(chain[..12].to_vec(), snapshots())).await;

    let directory = tempfile::tempdir().expect("a directory");
    let burnchain = MovableBurnchain::new(captured_burnchain());
    let (mut executor, _) = node(directory.path(), burnchain.clone());
    let staging = Staging::open(&directory.path().join("staging.sqlite")).expect("staging opens");
    let mut history = TenureSource::only(client.clone());
    let budget = CatchUpBudget {
        fetch: 64,
        execute: 64,
    };
    let round = executor
        .catch_up(&client, &mut history, &pox(), &staging, budget, &[])
        .await
        .expect("the captured chain executes");
    assert!(round.executed > 0, "nothing was executed to retract");

    // The burn blocks the executed tenures were elected in, oldest first.
    let tenures = executor.chainstate_mut().executed_tenures();
    let mut heights = burn_heights(&tenures);
    heights.sort_unstable();
    // Seeded at the *second* executed tenure rather than the first. The oldest is
    // the checkpoint's own, and a walk starting below it crosses the burn block
    // that opens the reward cycle — where a chain which has not derived that
    // cycle's anchor block cannot continue, since the consensus hash mixes one bit
    // per cycle. That is a real limit of a checkpoint-seeded chain and not
    // something to work around here.
    let (seed, retracted_at) = (
        *heights.get(1).expect("two executed tenures"),
        *heights.last().expect("an executed tenure"),
    );
    assert!(
        retracted_at > seed,
        "the executed chain spans one burn block above the seed, so nothing could \
         be derived or given back"
    );
    let tracker = derived_chain(
        seed,
        retracted_at,
        &burnchain,
        &directory.path().join("capture"),
    );
    // The chain this node derived is the chain it executed: without this the
    // retraction below could discard nothing and the test would still pass, because
    // a wrong consensus hash matches no tenure.
    assert_eq!(
        tracker.consensus_hash_at(retracted_at),
        tenures.first().copied(),
        "the derived consensus hash at burn {retracted_at} is not the one the \
         executed tenure carries"
    );
    let tip_before = executor.tip().block_id();
    let executed_before = executor.chainstate_mut().executed_blocks();
    executor.track_sortitions(tracker, directory.path().join("sortitions"));

    // Bitcoin gives back the block the last executed tenure was elected in.
    burnchain.reorganize(retracted_at);
    let round = executor
        .catch_up(&client, &mut history, &pox(), &staging, budget, &[])
        .await
        .expect("a reorganized burnchain is not an error");
    let resumed = round
        .reorganized
        .expect("the reorganization was not noticed");
    assert_ne!(
        resumed,
        *tip_before.as_bytes(),
        "the node stood on the same tip after a reorganization took it back"
    );
    assert_eq!(
        *executor.tip().block_id().as_bytes(),
        resumed,
        "the executor kept standing on a block it had given back, which is the \
         stall a retraction exists to avoid"
    );
    let executed_after = executor.chainstate_mut().executed_blocks();
    assert!(
        executed_after.len() < executed_before.len(),
        "the reorganization discarded no blocks: {} before, {} after",
        executed_before.len(),
        executed_after.len()
    );
    assert!(
        executed_before.starts_with(&executed_after),
        "the surviving chain is not a prefix of the one that was executed"
    );
    // Nothing of the abandoned branch is left staged to be executed again on the
    // burnchain that replaced it.
    assert!(
        staging.is_empty().expect("the staging store answers"),
        "blocks from the abandoned burn view are still staged"
    );

    task.abort();
}

/// Close a gap round by round until the tip reaches `target`, and say how much was
/// executed.
///
/// The loop a running node runs, bounded so a stall fails the test instead of hanging
/// it. A round that fetches without executing is ordinary — a descent walks the gap
/// from the peer's tip downwards and only the round that reaches this node's own tip
/// can execute anything — so the loop is not stopped by one.
pub async fn close_the_gap(
    executor: &mut CheckpointExecutor<MovableBurnchain>,
    client: &SyncClient,
    history: &mut TenureSource,
    staging: &Staging,
    budget: CatchUpBudget,
    target: u64,
) -> usize {
    let mut executed = 0;
    for _ in 0..ROUNDS {
        let round = executor
            .catch_up(client, history, &pox(), staging, budget, &[])
            .await
            .expect("a round commits what it executed");
        executed += round.executed;
        if executor.tip().header.chain_length >= target {
            break;
        }
    }
    executed
}

/// How many rounds a gap is given before it is called stuck.
const ROUNDS: usize = 64;

/// The reward cycle length the captured chain was produced under.
pub const CYCLE: u64 = 20;

/// The burn height a captured snapshot gives a block's tenure.
pub fn burn_height_of(rows: &[Snapshot], block: &NakamotoBlock) -> u64 {
    let Some(row) = rows
        .iter()
        .find(|row| row.consensus_hash == block.header.consensus_hash.to_string())
    else {
        panic!("no captured snapshot for {}", block.header.consensus_hash)
    };
    row.block_height
}

/// The second distinct burn view a chain stands on.
///
/// The first is the checkpoint's own, and a chain seeded below it would have to
/// derive across the block that opens the reward cycle.
pub fn second_burn_view(chain: &[NakamotoBlock], burn_of: &impl Fn(&NakamotoBlock) -> u64) -> u64 {
    let mut views: Vec<u64> = chain.iter().map(burn_of).collect();
    views.dedup();
    *views.get(1).expect("the capture holds two burn views")
}

/// was never asked for, which is the stronger half: a node that asked and quietly
/// carried on when refused would pass the first claim while still letting a
/// reachable peer choose its burn heights.
///
/// The chain is derived rather than quoted. The whole capture is served so the
/// execution path crosses every reward-cycle boundary that `pox_boundary`
/// independently checks at the sortition layer.
/// Close the same gap against a peer that *does* serve sortitions.
///
/// The control, and the whole test rests on it: without a run that reached the same
/// tip through the ordinary path there is no root to compare the blind run's against,
/// and "it executed something" would pass.
async fn reference_roots(
    served: &[NakamotoBlock],
    target: u64,
    budget: CatchUpBudget,
) -> (
    [u8; 32],
    nano_primitives::TrieHash,
    Option<nano_primitives::TrieHash>,
) {
    let directory = tempfile::tempdir().expect("a directory");
    let (honest, task) = serve(Served::honest(served.to_vec(), snapshots())).await;
    let burnchain = MovableBurnchain::new(captured_burnchain());
    let (mut executor, _) = node(directory.path(), burnchain);
    let staging = Staging::open(&directory.path().join("staging.sqlite")).expect("staging opens");
    let mut history = TenureSource::only(honest.clone());
    close_the_gap(
        &mut executor,
        &honest,
        &mut history,
        &staging,
        budget,
        target,
    )
    .await;
    let tip = executor.tip().clone();
    assert_eq!(
        tip.header.chain_length, target,
        "the reference run did not reach the peer's tip, so there is nothing to compare against"
    );
    let roots = (
        *tip.block_id().as_bytes(),
        tip.header.state_index_root,
        executor
            .chainstate_mut()
            .state_content_root(*tip.block_id().as_bytes())
            .expect("read the reference content root"),
    );
    task.abort();
    roots
}

/// Peer sortition lies cannot change any locally derived execution input.
///
/// [[049]]'s first acceptance criterion, and the one thing the rest of this file
/// could not show: every burn view a block executes under is named by this node's own
/// snapshot chain, walked forward from its own Bitcoin source, so a peer that does
/// not serve sortitions at all is a peer a follower can still follow.
///
/// Two claims and neither is enough alone. The gap closes — the node reaches the
/// peer's tip and seals the same roots as one that had the route — **and** the route

#[tokio::test]
async fn peer_sortition_lies_never_reach_execution() {
    let chain = captured_chain();
    let rows = snapshots();
    let burn_of = |block: &NakamotoBlock| burn_height_of(&rows, block);
    // The second burn view the chain holds. The first is the checkpoint's own, and a
    // chain seeded below it would have to walk the block that opens the cycle.
    let seed = second_burn_view(&chain, &burn_of);
    let served = chain.clone();
    let upto_seed = served
        .iter()
        .take_while(|block| burn_of(block) <= seed)
        .count();
    assert!(
        served.len() > upto_seed,
        "the served prefix ends at the seed's own burn view, so nothing above it would \
         have to be located locally"
    );
    let target = served.last().expect("a served tip").header.chain_length;
    let budget = CatchUpBudget {
        fetch: 64,
        execute: 8,
    };

    let reference_closed = reference_roots(&served, target, budget).await;

    // The node under test. It executes up to the seed's burn view against a peer that
    // does serve sortitions — a node has to reach the burn block its chain is seeded
    // at before it can be seeded there — and everything above that against one whose
    // height, burn hash, timestamp, sortition identifiers, winner flag, VRF seed and
    // previous-sortition pointer are all lies. The last two would also change the
    // peer-derived accumulated coinbase.
    let directory = tempfile::tempdir().expect("a directory");
    let staging_path = directory.path().join("staging.sqlite");
    let burnchain = MovableBurnchain::new(captured_burnchain());
    let (mut executor, _) = node(directory.path(), burnchain.clone());
    let staging = Staging::open(&staging_path).expect("staging opens");
    let (client, task) = serve(Served::honest(served[..upto_seed].to_vec(), snapshots())).await;
    let mut history = TenureSource::only(client.clone());
    close_the_gap(
        &mut executor,
        &client,
        &mut history,
        &staging,
        budget,
        served[..upto_seed]
            .last()
            .expect("a prefix tip")
            .header
            .chain_length,
    )
    .await;
    assert_eq!(
        executor.bitcoin_height(),
        seed,
        "the node did not reach the burn view its sortition chain is about to be seeded at"
    );
    task.abort();

    let tracker = derived_chain(seed, seed, &burnchain, &directory.path().join("capture"));
    executor.track_sortitions(tracker, directory.path().join("sortitions"));

    // The peer's own record of what it was asked, kept here: a `Policy` is shared
    // through `Arc`, so this is the same log the served peer writes into.
    let asked = Policy::default();
    let (blind, blind_task) = serve(
        Served::honest(served.clone(), snapshots())
            .under(asked.clone())
            .lying_about_sortitions(),
    )
    .await;
    let mut history = TenureSource::only(blind.clone());
    let executed = close_the_gap(
        &mut executor,
        &blind,
        &mut history,
        &staging,
        budget,
        target,
    )
    .await;
    assert!(
        executed > 0,
        "nothing was executed against the lying peer, so the claim is untested"
    );
    let tip = executor.tip().clone();
    assert_eq!(
        tip.header.chain_length, target,
        "the node stopped at height {} of the lying peer's {target}",
        tip.header.chain_length
    );
    assert_eq!(
        (
            *tip.block_id().as_bytes(),
            tip.header.state_index_root,
            executor
                .chainstate_mut()
                .state_content_root(*tip.block_id().as_bytes())
                .expect("read the derived content root"),
        ),
        reference_closed,
        "the peer's false burn height, hash, timestamp, VRF seed or accumulated coinbase \
         changed locally derived execution"
    );
    // The stronger half. A node that asked and carried on when refused would have
    // reached the tip too, and would still be one reachable peer away from having its
    // burn heights chosen for it.
    assert_eq!(
        asked.sortitions_asked(),
        0,
        "the node asked the peer for a sortition {} times while deriving its own",
        asked.sortitions_asked()
    );
    blind_task.abort();
}

/// The burn heights a set of tenures was elected at, in the order given.
fn burn_heights(tenures: &[ConsensusHash]) -> Vec<u64> {
    let rows = snapshots();
    tenures
        .iter()
        .filter_map(|tenure| {
            rows.iter()
                .find(|snapshot| snapshot.consensus_hash == tenure.to_string())
                .map(|snapshot| snapshot.block_height)
        })
        .collect()
}

/// A sortition chain seeded at one captured burn block and derived from there.
///
/// Through `SortitionTracker::from_capture`, which is the production seeding path,
/// rather than by building a snapshot by hand: the seed's `PoX` history comes from
/// its own sortition identifier and its running burn total from the captured row,
/// and a hand-built seed would have both wrong in ways every derived hash after it
/// inherits.
///
/// The capture it reads is written here from the fixture's own snapshots, cut at
/// the seed — a history may only seed the snapshot it *ends* at, because every hash
/// above that has to be derived rather than quoted.
pub fn derived_chain(
    seed: u64,
    upto: u64,
    burnchain: &MovableBurnchain,
    capture: &Path,
) -> nano_node::sortition::SortitionTracker {
    fs::create_dir_all(capture).expect("a capture directory");
    // The captured rows, with the seed's own winning VRF seed written in. A chain
    // that derived its snapshots states that seed; the `snapshots` table does not
    // record it, and recovering it from the seed block's commitments only works
    // where they agree about it — which they do not on a contested burn block, of
    // which this capture has one. It is read out of the winning commitment in the
    // captured Bitcoin block, which is where a chain would have got it.
    let seeds = nano_conformance::captured_bitcoin_snapshots(&fixtures())
        .expect("the captured snapshots read");
    let mut rows: Vec<serde_json::Value> = serde_json::from_slice(
        &fs::read(fixtures().join("sortition/snapshots.json")).expect("read the snapshots"),
    )
    .expect("the snapshots parse");
    for row in &mut rows {
        if row["block_height"].as_u64() == Some(seed) {
            let consensus = row["consensus_hash"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let vrf_seed = seeds
                .get(&consensus)
                .map(|context| context.vrf_seed)
                .expect("the seed's burn block names a winning commitment");
            row["winner_vrf_seed"] = serde_json::Value::String(hex::encode(vrf_seed));
        }
    }
    fs::write(
        capture.join("snapshots.json"),
        serde_json::to_vec(&rows).expect("the snapshots encode"),
    )
    .expect("the snapshots are written");
    let rows = snapshots();
    let hashes: Vec<String> = {
        let mut behind: Vec<&Snapshot> = rows
            .iter()
            .filter(|snapshot| snapshot.block_height <= seed)
            .collect();
        behind.sort_by_key(|snapshot| snapshot.block_height);
        behind
            .into_iter()
            .map(|snapshot| snapshot.consensus_hash.clone())
            .collect()
    };
    fs::write(
        capture.join("consensus-hashes.json"),
        serde_json::to_vec(&serde_json::json!({ "hashes": hashes })).expect("the history encodes"),
    )
    .expect("the history is written");
    fs::copy(
        fixtures()
            .join("sortition")
            .join(nano_node::sortition::LEADER_KEY_FILE),
        capture.join(nano_node::sortition::LEADER_KEY_FILE),
    )
    .expect("the checkpoint leader-key registry is copied");

    let mut tracker = nano_node::sortition::SortitionTracker::from_capture(capture)
        .expect("the synthesized capture seeds a chain");
    // The node's own derivation from the node's own constants, not a schedule
    // written out by hand here: the count of payout outputs is what decides every
    // candidate's weight, so a test that stated it separately would be checking the
    // tracker against a second opinion instead of against production. See `pox()`
    // for what this capture pays and why it used to derive nothing.
    let payouts = nano_node::payout_schedule(&pox()).expect("a payout schedule");
    let mut burnchain = burnchain.clone();
    tracker
        .catch_up(
            |height| burnchain.block_at(height),
            upto,
            payouts,
            nano_node::sortition::CATCH_UP_LIMIT,
        )
        .expect("the captured burn blocks derive");
    assert_eq!(
        tracker.tip().bitcoin_height,
        upto,
        "the derived chain did not reach the burn block under test"
    );
    tracker
}

/// Add the winning keys this node derives from the captured Bitcoin chain.
pub fn authenticated_context(
    mut context: BitcoinBlockContext,
    tenure: ConsensusHash,
) -> BitcoinBlockContext {
    let seed = nano_node::CheckpointManifest::load(fixtures().join("chainstate/checkpoint-H"))
        .expect("read the checkpoint manifest")
        .first_bitcoin_height;
    let burnchain = MovableBurnchain::new(captured_burnchain());
    let directory = tempfile::tempdir().expect("a local sortition capture");
    let tracker = derived_chain(
        seed,
        context.height,
        &burnchain,
        &directory.path().join("sortition"),
    );
    let snapshot = tracker
        .snapshot_at(context.height)
        .expect("the local sortition chain reaches the tenure");
    assert_eq!(
        snapshot.consensus_hash, tenure,
        "the locally derived burn view is the captured tenure's"
    );
    context.sortition_hash = *snapshot.sortition_hash.as_bytes();
    context.winner_vrf_public_key = Some(
        snapshot
            .winner_vrf_public_key
            .expect("the local sortition resolves the winning VRF key"),
    );
    context.winner_signing_key_hash = Some(
        snapshot
            .winner_signing_key_hash
            .expect("the local sortition resolves the winning signing key"),
    );
    context
}
