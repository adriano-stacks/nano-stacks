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
    sync::Arc,
};

use axum::{Router, extract::State, http::StatusCode, routing::get};
use nano_bitcoin::{BitcoinBlock, BitcoinSource};
use nano_chainstate::{ChainState, NakamotoBlock};
use nano_node::{CatchUpBudget, CheckpointExecutor, staging::Staging};
use nano_primitives::ConsensusHash;
use nano_sync::{PeerPool, PoxInfo, SyncClient, TenureSource};
use serde::Deserialize;

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
    sortition: u8,
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
fn captured_burnchain() -> BTreeMap<u64, BitcoinBlock> {
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
struct MovableBurnchain {
    blocks: Arc<std::sync::Mutex<BTreeMap<u64, BitcoinBlock>>>,
}

impl MovableBurnchain {
    fn new(blocks: BTreeMap<u64, BitcoinBlock>) -> Self {
        Self {
            blocks: Arc::new(std::sync::Mutex::new(blocks)),
        }
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
}

/// What one fake peer serves.
pub struct Served {
    /// The chain it offers, lowest first. Its last block is its tip.
    pub blocks: Vec<NakamotoBlock>,
    pub snapshots: Vec<Snapshot>,
}

impl Served {
    fn block(&self, id: &str) -> Option<Vec<u8>> {
        self.blocks
            .iter()
            .find(|block| hex::encode(block.block_id()) == id)
            .map(NakamotoBlock::encode)
    }

    /// Every block of the tenure the named block belongs to, back to back.
    ///
    /// This is what `/v3/tenures/:id` answers, and it is what makes a descent one
    /// request per tenure rather than one per block.
    fn tenure(&self, id: &str) -> Option<Vec<u8>> {
        let named = self
            .blocks
            .iter()
            .find(|block| hex::encode(block.block_id()) == id)?;
        Some(
            self.blocks
                .iter()
                .filter(|block| block.header.consensus_hash == named.header.consensus_hash)
                .flat_map(NakamotoBlock::encode)
                .collect(),
        )
    }

    fn tip(&self) -> &NakamotoBlock {
        self.blocks.last().expect("a served chain has a tip")
    }

    fn info(&self) -> serde_json::Value {
        let tip = self.tip();
        let start = self
            .blocks
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
            "contract_versions": [],
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
        serde_json::json!({
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
        })
    }

    /// The burn view between two consensus hashes, newest first.
    fn fork_info(&self, start: &str, stop: &str) -> serde_json::Value {
        let height = |hash: &str| {
            self.snapshots
                .iter()
                .find(|snapshot| snapshot.consensus_hash == hash)
                .map(|snapshot| snapshot.block_height)
        };
        let (Some(start), Some(stop)) = (height(start), height(stop)) else {
            return serde_json::Value::Array(Vec::new());
        };
        let mut entries: Vec<serde_json::Value> = self
            .snapshots
            .iter()
            .filter(|snapshot| snapshot.block_height >= stop && snapshot.block_height <= start)
            .map(|snapshot| {
                serde_json::json!({
                    "burn_block_height": snapshot.block_height,
                    "consensus_hash": snapshot.consensus_hash,
                    "was_sortition": snapshot.sortition == 1,
                    "first_block_mined": serde_json::Value::Null,
                })
            })
            .collect();
        entries.reverse();
        serde_json::Value::Array(entries)
    }
}

/// What a peer answers for bytes it either holds or does not.
fn found(body: Option<Vec<u8>>) -> (StatusCode, Vec<u8>) {
    body.map_or_else(
        || (StatusCode::NOT_FOUND, Vec::new()),
        |body| (StatusCode::OK, body),
    )
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
                axum::Json(state.fork_info(
                    &start.trim_start_matches("0x").to_lowercase(),
                    &stop.trim_start_matches("0x").to_lowercase(),
                ))
            }),
        )
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

/// A node standing on the captured checkpoint, with the anchor block applied.
///
/// Durable, in a directory of its own, because a retraction stands on the ledger
/// the surviving block committed and an in-memory state has none to read back.
fn node(
    directory: &Path,
    burnchain: MovableBurnchain,
) -> (CheckpointExecutor<MovableBurnchain>, Vec<NakamotoBlock>) {
    let fixtures = fixtures();
    let manifest = nano_node::CheckpointManifest::load(fixtures.join("chainstate/checkpoint-H"))
        .expect("the checkpoint manifest reads");
    let mut chainstate = ChainState::open_from_checkpoint(
        nano_conformance::captured_network(&fixtures),
        directory,
        fixtures.join("chainstate/checkpoint-H/marf.sqlite"),
        manifest.source_state_id,
        manifest.state_index_root,
    )
    .expect("the checkpoint opens");
    // The rewards the checkpoint still owes: a tenure this node executes pays out
    // one earned before the state was exported, which only the checkpoint knows.
    if let Some(accounting) = fs::read(fixtures.join("chainstate/checkpoint-H/native-effects.json"))
        .ok()
        .and_then(|contents| nano_chainstate::TenureAccounting::from_json(&contents).ok())
    {
        *chainstate.accounting_mut() = accounting;
    }
    let chain = captured_chain();
    let anchor = chain.first().expect("the capture has blocks").clone();
    // The anchor's own context comes from the capture, as a configured node's
    // does from its checkpoint: nothing has been executed yet, so there is
    // nowhere else for it to come from.
    let context = *nano_conformance::captured_bitcoin_snapshots(&fixtures)
        .expect("the captured snapshots read")
        .get(&anchor.header.consensus_hash.to_string())
        .expect("the anchor's own burn block");
    let executor = CheckpointExecutor::from_chainstate(chainstate, anchor, context, burnchain)
        .expect("the anchor block applies");
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
fn alternative_history(chain: &[NakamotoBlock], from: usize) -> Vec<NakamotoBlock> {
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
    let liar = alternative_history(&chain[..16], 1);
    assert!(
        liar.last().expect("a tip").header.chain_length
            > honest.last().expect("a tip").header.chain_length,
        "the wrong chain is not longer, so nothing is being tested"
    );

    let (honest_client, honest_task) = serve(Served {
        blocks: honest.clone(),
        snapshots: snapshots(),
    })
    .await;
    let (lying_client, lying_task) = serve(Served {
        blocks: liar,
        snapshots: snapshots(),
    })
    .await;

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
        .catch_up(&lying_client, &mut history, &pox(), &staging, budget)
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
        .catch_up(&honest_client, &mut history, &pox(), &staging, budget)
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
    let (honest_client, honest_task) = serve(Served {
        blocks: honest.clone(),
        snapshots: snapshots(),
    })
    .await;

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
        .catch_up(&honest_client, &mut history, &pox(), &staging, budget)
        .await
        .expect("the captured chain executes");
    let tenures = executor.chainstate_mut().executed_tenures();
    assert!(
        tenures.len() >= 2,
        "the executed chain spans one tenure, so no fork point below its tip exists"
    );
    let disputed = *tenures.first().expect("a newest tenure");
    let agreed = *tenures.get(1).expect("a tenure below it");
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
    let (parted_client, parted_task) = serve(Served {
        blocks: parted_view(&honest, disputed, replacement),
        snapshots: parted,
    })
    .await;

    let mut history = TenureSource::only(parted_client.clone());
    let round = executor
        .catch_up(&parted_client, &mut history, &pox(), &staging, budget)
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
    let (client, task) = serve(Served {
        blocks: chain[..12].to_vec(),
        snapshots: snapshots(),
    })
    .await;

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
        .catch_up(&client, &mut history, &pox(), &staging, budget)
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
    let tracker = derived_chain(seed, retracted_at, &burnchain, &directory.path().join("capture"));
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
        .catch_up(&client, &mut history, &pox(), &staging, budget)
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
fn derived_chain(
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
            let consensus = row["consensus_hash"].as_str().unwrap_or_default().to_owned();
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
