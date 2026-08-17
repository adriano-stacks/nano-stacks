//! Starting a node: open the state, pick a peer, run the configured roles.
//!
//! Everything a role needs is derived here, once, so that following, signing
//! and mining are three tasks over one configuration rather than three
//! programs over three command lines.

use std::{
    collections::{BTreeSet, HashMap},
    error::Error,
    fs::{self, File, OpenOptions},
    future::Future,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use fs2::FileExt as _;

use nano_bitcoin::{BitcoinRestSource, BitcoinRpcSource, BitcoinSource as _};
use nano_chainstate::{
    BitcoinBlockContext, CHECKPOINT_HISTORY_LIMIT, ChainState, ChainStateError,
    CheckpointBoundaryProof, CheckpointHistoryBlock, MINER_REWARD_MATURITY, NakamotoBlock, Signer,
    SignerSet, TenureAccounting,
};
use nano_crypto::StacksPublicKey;
use nano_p2p::Discovered;
use nano_primitives::{Network, StacksBlockId};
use nano_rpc::{ChainAccess, EventDispatcher, RpcState, SealedTip, serve};
use nano_sync::{Node, PeerPool, PoxInfo, SyncClient, SyncError, TenureSource};
use tokio::{net::TcpListener, signal::unix::SignalKind, sync::Mutex, task::JoinSet, time::sleep};

use crate::{
    CatchUpBudget, CatchUpRound, CheckpointExecutor, CheckpointManifest, CheckpointProvenance,
    config::Config, miner, signer, sortition::SortitionTracker, staging::Staging,
};

/// How many blocks one round of catching up will fetch before executing.
pub(crate) const ROUND_FETCH: usize = 4_000;

/// How close a node has to be before it is worth following the peer's tenure
/// rather than spending every request catching up.
const FOLLOW_WHEN_WITHIN: u64 = 1_000;

/// How many rounds to stay with one peer before asking whether a better one exists.
const RESELECT_ROUNDS: u32 = 60;

/// How long a startup step waits out a rate-limited peer before giving up.
const STARTUP_PATIENCE: Duration = Duration::from_secs(64);

/// The state directory the node executes the canonical chain in.
pub(crate) const NODE_CHAINSTATE: &str = "chainstate";
/// The state directory the signer validates proposals in.
pub(crate) const SIGNER_CHAINSTATE: &str = "signer-chainstate";
/// The accounting a role used to rewrite as it executed.
///
/// Read only, now: the ledger is committed with the seal, which is what makes it
/// as of the tip rather than as of the last catch-up round. This is still read so
/// that a state directory written before that keeps opening.
const ACCOUNTING_FILE: &str = "accounting.json";

/// The shared executed chain the node follows along and answers reads from.
pub type SharedExecutor = Arc<Mutex<CheckpointExecutor<BurnchainSource>>>;

/// The optional miner owns catch-up while it runs.
///
/// Dropping the lease hands execution back to the follower. A miner that cannot
/// open its wallet is allowed to stop without taking the signer or RPC down, so
/// its executor cannot disappear with it.
struct MinerExecutionLease(Arc<AtomicBool>);

impl MinerExecutionLease {
    fn claim(active: Arc<AtomicBool>) -> Self {
        active.store(true, Ordering::Release);
        Self(active)
    }
}

impl Drop for MinerExecutionLease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn follower_owns_execution(miner_owns_execution: &AtomicBool) -> bool {
    !miner_owns_execution.load(Ordering::Acquire)
}

/// What a role reports when it stops, which is always the end of the node.
pub type Role = Result<(), String>;

/// Polls a peer is given to produce the block a resumed state is sealed at,
/// before the state is declared to have left the chain.
const RESUME_ATTEMPTS: u32 = 30;

/// The `StackerDB` message contracts a reward cycle's signers write to:
/// block responses, state machine updates and pre-commits, in that order
/// (`MessageSlotID`). A cycle's signer set owns one slot in each.
pub(crate) const SIGNER_MESSAGE_IDS: [u32; 3] = [1, 2, 3];

/// A job the node runs, and what its stopping means for the rest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Job {
    Rpc,
    Metrics,
    Follower,
    Signer,
    Miner,
    /// The p2p transport: peer discovery, and the listener that answers peers.
    Peers,
    /// Executing the proposals this node is asked to vouch for.
    Proposals,
    /// Keeping this node's `StackerDB` replicas and its peer's in step.
    Replication,
}

impl Job {
    /// Whether the node must stop when this job does.
    ///
    /// A network's liveness rests on its signers, and a node that has stopped
    /// validating must not keep an operator believing it still signs. A miner
    /// that cannot commit, or a closed RPC port, costs this node work and the
    /// chain nothing — so they must not take the signer down with them, which
    /// is how one stale leader-key transaction stalled a whole Hacknet.
    const fn is_fatal(self) -> bool {
        match self {
            Self::Signer | Self::Follower => true,
            // Losing peer discovery leaves whatever HTTP peers the operator
            // configured, and losing the listener only costs this node its place in
            // other nodes' peer tables. Neither is worth stopping a node that is
            // still executing the chain.
            // A node hosting a signer stops being useful to it when either of
            // these stops, but the chain it follows is unaffected, and the signer
            // says so far louder than this node could.
            Self::Rpc
            | Self::Metrics
            | Self::Miner
            | Self::Peers
            | Self::Proposals
            | Self::Replication => false,
        }
    }
}

impl std::fmt::Display for Job {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Rpc => "RPC server",
            Self::Metrics => "metrics server",
            Self::Follower => "follower",
            Self::Signer => "signer",
            Self::Miner => "miner",
            Self::Peers => "peer network",
            Self::Proposals => "proposal validator",
            Self::Replication => "StackerDB replication",
        })
    }
}

/// Say how long a startup phase took, so a slow start names itself.
///
/// A node that prints nothing for six minutes on a mainnet state teaches an
/// operator — and whoever is chasing a divergence — to guess. Every guess made
/// about this so far has been wrong: the sortition derivation cannot advance
/// there, the header backfill prints per ancestor and printed nothing, and the
/// process turned out to be at 30% CPU rather than blocked on a peer. The cost of
/// measuring it is one line per phase.
struct Phase {
    name: &'static str,
    started: std::time::Instant,
}

impl Phase {
    fn start(name: &'static str) -> Self {
        Self {
            name,
            started: std::time::Instant::now(),
        }
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        // Only the slow ones: a list of sub-second phases is noise an operator
        // learns to skip, and skipping it is how the slow one stays hidden.
        if elapsed.as_millis() > 500 {
            println!("startup: {} took {:.1}s", self.name, elapsed.as_secs_f64());
        }
    }
}

/// Hold a state directory for this process alone, for as long as it runs.
///
/// Two nodes on one directory is not a configuration, it is a corruption: each
/// reads the sealed tip once at startup and then executes forward from its own
/// idea of it, so the second one writes MARF versions the first has already
/// written and every round after that fails with `MARF version already exists`.
/// It happened here, by an operator restarting a node before the running one had
/// finished stopping, and the ledger recovered intact only because a commit is one
/// transaction. Nothing about that was luck to be relied on again.
///
/// An advisory lock and not a pid file, so a killed node leaves nothing to clean
/// up: the kernel drops the lock with the file descriptor.
fn hold_state_directory(working_dir: &Path) -> Result<File, Box<dyn Error>> {
    fs::create_dir_all(working_dir)?;
    let path = working_dir.join("node.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    lock.try_lock_exclusive().map_err(|error| {
        format!(
            "another nano-stacks node is already running on {}: {error}.              One state directory holds one node -- stop that one first, and check              that it has exited rather than assuming a signal was enough.",
            working_dir.display()
        )
    })?;
    Ok(lock)
}

/// Run a node until it is asked to stop or a role gives up.
pub async fn run(config: Config) -> Result<(), Box<dyn Error>> {
    // The kernel releases this lifetime lock even after a kill.
    let _state = hold_state_directory(&config.node.working_dir)?;
    let mut roles: JoinSet<(Job, Role)> = JoinSet::new();
    // Written by the follow loop once there is a chain to describe, and read by the
    // discovery loop that starts before there is one.
    let advertised = Advertised::open(&config.node.working_dir);
    let metrics = nano_rpc::NodeMetrics::default();
    // Where a peer's pushed blocks and transactions wait for the loop that can check
    // them. Created here rather than inside the transport because the follow loop is
    // the other end of it, and neither half is the owner.
    let relay = nano_p2p::Relay::default();
    // The p2p transport comes up first, because its whole point is to be a way in
    // that does not depend on a configured HTTP peer. It needs the chain identifier
    // up front — on this protocol the network id *is* the chain id and it is the
    // second field of the first message — so a configuration that leaves the chain
    // to be discovered gets no transport, and falls back to what it always did.
    let phase = Phase::start("joining the peer network");
    let discovered =
        start_configured_transport(&config, &advertised, &relay, metrics.clone(), &mut roles).await;
    drop(phase);
    let phase = Phase::start("reaching a peer");
    let peer = awaited_peer(&config, discovered.as_ref()).await?;
    drop(phase);
    let network = match config.network() {
        Some(network) => network,
        // A private network's chain identifier is only knowable from the chain,
        // so a configuration that does not fix it takes what the peer reports.
        None => Network::from_chain_id(patiently(|| peer.node_info()).await?.network_id),
    };
    let pox = patiently(|| peer.pox_info()).await?;
    let source = config.checkpoint.source_state_id()?;
    println!(
        "nano-stacks starting on chain {:#010x}, state under {}",
        network.chain_id(),
        config.node.working_dir.display()
    );

    // The chain is only executed when something reads the executed state: a
    // signer-only node validates proposals in its own store and would be
    // executing every block twice for nobody.
    let executor = open_executed_state(&config, network, &pox, discovered.as_ref()).await?;
    let dispatcher = EventDispatcher::new(config.node.event_observers()?);
    let phase = Phase::start("announcing the blocks already executed");
    announce_node_events(executor.as_ref(), &dispatcher).await;
    drop(phase);
    // One mempool, shared: a node whose RPC admits transactions into a pool the
    // miner cannot see accepts them and never mines them, which is worse than
    // refusing them.
    let mempool = Arc::new(Mutex::new(nano_mempool::Mempool::new(network)));
    // The blocks this node executes, kept so it can serve them. Opened here
    // because both halves need it: the executor writes what it seals and the RPC
    // reads what a caller asks for, and a node that keeps blocks nothing serves
    // is only using disk.
    let archive = keep_executed_blocks(&config, executor.as_ref()).await;
    let (wiring, api_to_loop, hosted) = ApiWiring::new(executor.clone(), mempool.clone(), archive);
    let rpc_enabled = config.node.rpc_bind.is_some();
    let state = start_rpc(&config, network, wiring, &dispatcher, metrics, &mut roles).await?;
    if let Some(executor) = executor.as_ref() {
        executor.lock().await.publish_execution_to(state.metrics());
    }
    publish_sealed_tip(Some(&state), executor.as_ref(), &pox).await;
    // The miner executes the chain itself while it runs, because it has to build
    // on its own blocks the moment it makes them. If the optional miner stops,
    // its lease hands execution back to the follower.
    let miner_owns_execution = Arc::new(AtomicBool::new(false));
    start_miner(
        &config,
        network,
        &pox,
        &peer,
        (
            executor.clone(),
            mempool.clone(),
            miner_owns_execution.clone(),
        ),
        (dispatcher, relay.clone(), state.metrics(), state.clone()),
        &mut roles,
    );
    start_signer(&config, network, &pox, discovered.as_ref(), &mut roles).await?;
    start_hosting(
        &config,
        network,
        &pox,
        discovered.as_ref(),
        rpc_enabled.then_some(&state),
        hosted,
        &mut roles,
    )
    .await?;
    // Following is only worth a task when someone reads what it produces: a
    // signer-only node validates from its own store and needs no second view.
    if rpc_enabled || executor.is_some() {
        let follower = Follower {
            config,
            network,
            peer,
            discovered,
            advertised,
            relay,
            mempool,
            pox,
            source,
            state: Some(state),
            executor,
            miner_owns_execution,
            offered: api_to_loop.offered,
            submitted: api_to_loop.submitted,
        };
        roles.spawn(async move { (Job::Follower, follow(follower).await) });
    }
    if roles.is_empty() {
        return Err("this configuration switches on no roles".into());
    }

    let outcome = supervise(&mut roles).await;
    // Aborting the roles drops their chainstates, which closes the stores they
    // hold; anything they had not sealed was never a tip.
    roles.abort_all();
    outcome
}

/// Run until a job the node depends on stops, or until it is asked to.
///
/// A job that is not fatal is reported and left behind; the node is only done
/// when a fatal one fails or nothing is left running.
async fn supervise(roles: &mut JoinSet<(Job, Role)>) -> Result<(), Box<dyn Error>> {
    loop {
        let joined = tokio::select! {
            joined = roles.join_next() => joined,
            () = terminated() => {
                println!("stopping: every sealed block is already on disk");
                return Ok(());
            }
        };
        match joined {
            None => return Ok(()),
            Some(Err(error)) => return Err(error.into()),
            Some(Ok((job, Err(error)))) if job.is_fatal() => return Err(error.into()),
            Some(Ok((job, result))) => {
                match result {
                    Err(error) => eprintln!("the {job} stopped: {error}"),
                    Ok(()) => eprintln!("the {job} finished"),
                }
                if roles.is_empty() {
                    return Ok(());
                }
                eprintln!("the node carries on without it");
            }
        }
    }
}

/// Open the store the executed blocks are kept in, and tell the executor to use
/// it.
///
/// A node with no chain of its own — a signer-only or RPC-only configuration —
/// executes nothing, so it has nothing to keep and nothing to serve. A store that
/// will not open is reported and left out: the blocks are the one thing a node can
/// always fetch again, so this is a served route lost and not a chain.
async fn keep_executed_blocks(
    config: &Config,
    executor: Option<&SharedExecutor>,
) -> Option<Arc<crate::archive::Archive>> {
    let executor = executor?;
    let directory = config.chainstate_dir(NODE_CHAINSTATE);
    if let Err(error) = fs::create_dir_all(&directory) {
        eprintln!("cannot make room for the executed blocks: {error}");
        return None;
    }
    match crate::archive::Archive::open(&directory.join("archive.sqlite")) {
        Ok(archive) => {
            let archive = Arc::new(archive);
            executor.lock().await.keep_executed_blocks(archive.clone());
            Some(archive)
        }
        Err(error) => {
            eprintln!(
                "cannot keep the blocks this node executes, so /v3/blocks and /v3/tenures \
                 will answer only for what a peer has recently told it about: {error}"
            );
            None
        }
    }
}

/// Publish what this node is sealed at before it follows anything, so a node
/// that never manages to execute reports the height it is really on rather than
/// nothing at all.
async fn publish_sealed_tip(
    state: Option<&RpcState>,
    executor: Option<&SharedExecutor>,
    pox: &PoxInfo,
) {
    if let (Some(state), Some(executor)) = (state, executor) {
        let (sealed, sortitions, cache_usage) = {
            let mut executor = executor.lock().await;
            (
                sealed_tip(executor.tip(), executor.bitcoin_height()),
                executor.derived_sortitions(),
                executor.cache_usage(),
            )
        };
        state.metrics().publish_execution_caches(cache_usage);
        state
            .publish_executed(sealed, sortitions, pox.clone())
            .await;
    }
}

/// A block offered over the public API passes the same self-contained envelope
/// check as a peer push before it can be retained as a download.
///
/// Full local burn, miner, VRF and signer-set authentication requires the
/// block's execution context. It runs later, through the same typed constructor
/// as every peer-fetched direct child, before executable staging.
impl<S: Send> nano_rpc::BlockAdmission for CheckpointExecutor<S> {
    fn authenticate(&mut self, block: &NakamotoBlock) -> Result<(), String> {
        self.chainstate
            .authenticate_block(block)
            .map_err(|error| error.to_string())
    }
}

/// What this node has sealed, for the RPC to answer from.
pub(crate) fn sealed_tip(tip: &NakamotoBlock, bitcoin_height: u64) -> SealedTip {
    SealedTip {
        stacks_height: tip.header.chain_length,
        stacks_tip: tip.block_id(),
        stacks_block_hash: tip.header.block_hash(),
        consensus_hash: tip.header.consensus_hash,
        bitcoin_height,
        state_index_root: tip.header.state_index_root,
    }
}

/// Say what a round of catching up actually did.
///
/// A round that executed nothing reads exactly like one that executed a
/// thousand blocks unless it says so, which is how a node that had never
/// executed a single block past its checkpoint looked healthy for hours. So the
/// two are different sentences and not the same sentence with a zero in it: a
/// batch that executed something names where it started, where it ended, how
/// many blocks that was and the state root it sealed, and a batch that executed
/// nothing says so first and has no root to name.
fn round_report(from: u64, round: &CatchUpRound, tip: &NakamotoBlock) -> String {
    let limited = if round.rate_limited {
        ", peer rate limiting"
    } else {
        ""
    };
    // Named rather than counted into `fetched`, because the two answer different
    // questions: how much history a round acquired, and how much of it the peers'
    // inventories chose rather than a walk back from one peer's tip.
    let scheduled = if round.scheduled == 0 {
        String::new()
    } else {
        format!(", {} tenures the inventory scheduled", round.scheduled)
    };
    let authentication = if round.executed == 0 {
        String::new()
    } else {
        format!(
            ", authentication passed: {} block envelope/miner-signature/winner-sortition/signer-threshold/tenure-continuity checks and {} tenure-start coinbase-vrf/parent-seed checks",
            round.executed, round.authenticated_tenure_starts
        )
    };
    batch_report(
        from,
        round.executed,
        tip,
        &format!(
            ", {} staged, {} fetched{scheduled}{authentication}{limited}",
            round.staged, round.fetched
        ),
    )
}

/// Say where a round that failed got to, which its error does not.
///
/// A round that stops partway has still sealed everything up to where it
/// stopped. Reporting only the successful ones left a node that had executed
/// eighty-three blocks claiming twenty-two — and left a node executing *nothing*,
/// round after round, saying so only in an error whose wording is about the peer.
fn failed_round_report(from: u64, tip: &NakamotoBlock) -> String {
    let executed =
        usize::try_from(tip.header.chain_length.saturating_sub(from)).unwrap_or(usize::MAX);
    batch_report(from, executed, tip, ", then the round failed")
}

/// The one sentence an execution batch is reported in.
///
/// A batch that executed something names where it started, where it ended, how
/// many blocks that was and the root it sealed; a batch that executed nothing says
/// *that* first and has no root to name, because there is no new one. Two shapes
/// rather than one with a zero in it: they are read by a person deciding whether a
/// node is moving.
fn batch_report(from: u64, executed: usize, tip: &NakamotoBlock, detail: &str) -> String {
    if executed == 0 {
        format!("executed nothing: sealed at {from}{detail}")
    } else {
        format!(
            "executed {executed} blocks, {from} to {}, state root {}{detail}",
            tip.header.chain_length, tip.header.state_index_root
        )
    }
}

/// Write down the ancestor headers this state is missing, once, at startup.
///
/// A state built before headers were kept has none, so the first block it executes
/// cannot read the one it stands on.
async fn backfill_ancestors(
    executor: &SharedExecutor,
    _peer: &SyncClient,
    _pox: &PoxInfo,
    _source: [u8; 32],
) -> Result<(), String> {
    let _phase = Phase::start("backfilling ancestor headers");
    let executor = executor.lock().await;
    let result = executor.backfill_headers();
    drop(executor);
    match result {
        Ok(0) => {}
        Ok(recorded) => println!("wrote down {recorded} headers this state was missing"),
        Err(error) => return Err(format!("writing down the missing headers failed: {error}")),
    }
    Ok(())
}

/// What one round of execution left behind for the loop around it.
struct ExecutedRound {
    sealed: nano_rpc::SealedTip,
    executed_height: u64,
    peer_failed: bool,
}

/// What one catch-up round reads, as one value for the same reason `Follower` is
/// one: a round now takes the peer, the pool, the calendar, the store, the budget,
/// the handle it publishes through and what the peers claimed, and seven positional
/// arguments hide which is which.
struct RoundInputs<'a> {
    /// The peer this round follows, which is a fork choice and lands on one answer.
    peer: &'a SyncClient,
    /// Where bulk history comes from, which is work to be spread and does not.
    history: &'a mut TenureSource,
    pox: &'a PoxInfo,
    staging: &'a Staging,
    budget: CatchUpBudget,
    advertised: &'a Advertised,
    /// What the peers said about the cycle being walked, tenure by tenure. Empty
    /// when the transport is off, which leaves the round the backward descent it was.
    claims: Vec<nano_p2p::TenureClaim>,
    metrics: Option<nano_rpc::NodeMetrics>,
}

/// Run one catch-up round, and publish what it makes this node able to say.
///
/// Extracted from the follow loop because it is the only part that holds the
/// executor lock, and holding a lock is worth being able to see the boundary of.
async fn execute_round(executor: &SharedExecutor, inputs: RoundInputs<'_>) -> ExecutedRound {
    let RoundInputs {
        peer,
        history,
        pox,
        staging,
        budget,
        advertised,
        claims,
        metrics,
    } = inputs;
    let mut executor = executor.lock().await;
    let mut peer_failed = false;
    let from = executor.tip().header.chain_length;
    match executor
        .catch_up(peer, history, pox, staging, budget, &claims)
        .await
    {
        Ok(round) => println!("{}", round_report(from, &round, executor.tip())),
        // A round that stops partway has still sealed everything up to where it
        // stopped, and that is what has to be recorded: reporting only successful
        // rounds left a node that had executed eighty-three blocks claiming
        // twenty-two, and left its accounting behind its own chain.
        Err(error) => {
            eprintln!("executing the peer's chain failed: {error}");
            if let Some(metrics) = metrics.as_ref() {
                metrics.record_consensus_refusal(&error.to_string());
            }
            // And where that leaves this node, in the sentence every other batch
            // is reported in. The error names the peer's chain; this names ours.
            println!("{}", failed_round_report(from, executor.tip()));
            backfill_missing_header(&mut executor, peer, &error.to_string()).await;
            give_back_states_above_the_tip(&mut executor, &error.to_string());
            peer_failed = true;
        }
    }
    // What execution makes this node able to serve. The transport derives its burn
    // view directly from the local Bitcoin source; execution supplies the cycle and
    // inventory.
    advertised.publish(LocalAnnouncement {
        cycle_start: executor.cycle_start_consensus_hash(pox),
        inventories: executor.tenure_inventories(pox),
    });
    ExecutedRound {
        sealed: sealed_tip(executor.tip(), executor.bitcoin_height()),
        executed_height: executor.tip().header.chain_length,
        peer_failed,
    }
}

/// Everything the following role runs on, as one value for the same reason the
/// miner's is: the loop needs the peer, the chain, the served state and the
/// blocks the API admitted, and a list of eight arguments hides which is which.
struct Follower {
    config: Config,
    network: Network,
    peer: SyncClient,
    /// The peers p2p discovery found, which this loop re-weighs alongside the
    /// configured ones. `None` when the transport is off.
    discovered: Option<Discovered>,
    /// Where to publish what this node tells its peers about itself. The discovery
    /// loop starts before there is a chain to describe, so this is how it eventually
    /// gets one.
    advertised: Advertised,
    /// Blocks and transactions peers pushed, waiting for the only loop that can
    /// check them, and where what passed goes back out.
    relay: nano_p2p::Relay,
    /// Where a relayed transaction is admitted, on this node's own rules against its
    /// own view of the accounts.
    mempool: Arc<Mutex<nano_mempool::Mempool>>,
    pox: PoxInfo,
    source: [u8; 32],
    state: Option<RpcState>,
    executor: Option<SharedExecutor>,
    miner_owns_execution: Arc<AtomicBool>,
    /// Blocks the public API authenticated, waiting to be staged.
    offered: nano_queue::Receiver<NakamotoBlock>,
    /// Transactions the public API admitted, waiting to be passed on.
    submitted: nano_queue::Receiver<nano_codec::Transaction>,
}

/// What arrived other than by following: what this node's own API admitted, and
/// what peers pushed at it.
struct AdmittedInputs<'a> {
    offered: &'a mut nano_queue::Receiver<NakamotoBlock>,
    submitted: &'a mut nano_queue::Receiver<nano_codec::Transaction>,
    executor: Option<&'a SharedExecutor>,
    mempool: &'a Arc<Mutex<nano_mempool::Mempool>>,
    relay: &'a nano_p2p::Relay,
    staging: &'a Staging,
    metrics: Option<nano_rpc::NodeMetrics>,
}

/// Take everything that arrived other than by following, before the round that
/// executes it.
///
/// Blocks the public API admitted go into the same store the peer's do — nothing
/// about them is special from here on, which is the point — and so does
/// everything peers pushed, so a block pushed a moment ago is executed in the
/// round that follows rather than the one after it.
async fn take_admitted(inputs: AdmittedInputs<'_>) {
    let AdmittedInputs {
        offered,
        submitted,
        executor,
        mempool,
        relay,
        staging,
        metrics,
    } = inputs;
    stage_admitted_blocks(offered, staging);
    relay_admitted_transactions(submitted, relay);
    if let Some(executor) = executor {
        check_relayed(executor, mempool, relay, staging, metrics.as_ref()).await;
    }
}

/// The store staged blocks wait in, or the role's own failure.
fn open_staging(config: &Config) -> Result<Staging, Role> {
    let staging = Staging::open(
        &config
            .chainstate_dir(NODE_CHAINSTATE)
            .join("staging.sqlite"),
    )
    .map_err(|error| Err(format!("cannot open the staging store: {error}")))?;
    let quarantined = staging.quarantined_rows();
    if quarantined > 0 {
        println!(
            "the staging store retains {quarantined} quarantined legacy representations; \
             they cannot execute"
        );
    }
    Ok(staging)
}

/// What the follow loop carries from one round to the next.
///
/// One value rather than eight locals because they move together: which peer this
/// round follows, how long it has been followed, whether it let a round down, and
/// the two heights that decide whether this node is catching up or keeping up.
struct Rounds {
    /// Which peer this round follows. Re-chosen from everything this node knows
    /// of — the endpoints the operator configured and the ones p2p discovery
    /// found — so that a peer which stalls, falls behind or starts refusing costs
    /// one round rather than the node's liveness.
    peer: SyncClient,
    node: Node,
    /// Rounds this peer has been followed for.
    on_this_peer: u32,
    /// Whether the current peer let the last round down, which re-weighs the pool
    /// without waiting for its turn.
    failed: bool,
    peer_height: u64,
    executed_height: u64,
    published: RewardCyclePublication,
    /// Bulk history comes from every peer known, which is not the same question as
    /// which peer this round *follows*: following is a fork choice and has to land
    /// on one answer, while fetching history is work to be spread.
    history: BulkHistory,
}

struct ExecutedPublication<'a> {
    state: Option<&'a RpcState>,
    executor: &'a SharedExecutor,
    sealed: SealedTip,
    pox: &'a PoxInfo,
    config: &'a Config,
    network: Network,
    published: &'a mut RewardCyclePublication,
    peer: &'a SyncClient,
}

async fn publish_executed_round(publication: ExecutedPublication<'_>) {
    let ExecutedPublication {
        state: Some(state),
        executor,
        sealed,
        pox,
        config,
        network,
        published,
        peer,
    } = publication
    else {
        return;
    };
    let (sortitions, cache_usage, notifications) = {
        let mut executor = executor.lock().await;
        // On Bitcoin's clock: a node at the chain tip with nothing staged still
        // has to derive and report the burn view its own tip stands on.
        let (_, notifications) = executor.follow_burnchain_deferred(pox);
        (
            executor.derived_sortitions(),
            executor.cache_usage(),
            notifications,
        )
    };
    state.metrics().publish_execution_caches(cache_usage);
    state
        .publish_executed(sealed, sortitions, pox.clone())
        .await;
    executor.lock().await.announce_burn_blocks(&notifications);
    publish_reward_cycles_for_current_execution(
        state, executor, network, pox, config, published, peer,
    )
    .await;
}

impl Rounds {
    fn new(peer: SyncClient) -> Self {
        Self {
            node: Node::new(peer.clone()),
            history: BulkHistory::new(peer.clone()),
            peer,
            // Starting at the reselection point rather than at zero, so the
            // *first* round weighs the pool instead of keeping whichever peer
            // answered `reachable_peer` first. That peer was chosen for being
            // reachable, which is all a node can ask before it has opened its
            // state.
            on_this_peer: RESELECT_ROUNDS,
            failed: false,
            peer_height: u64::MAX,
            executed_height: 0,
            published: RewardCyclePublication::default(),
        }
    }

    const fn catching_up(&self) -> bool {
        self.peer_height.saturating_sub(self.executed_height) > FOLLOW_WHEN_WITHIN
    }

    /// Re-weigh the pool on a timer, or immediately after the current peer let a
    /// round down.
    ///
    /// Every round would be two requests per peer per second for an answer that
    /// moves on the order of a tenure; never would be the single-peer node task
    /// 027 set out to remove.
    async fn choose_peer(
        &mut self,
        config: &Config,
        discovered: Option<&Discovered>,
        executor: Option<&SharedExecutor>,
        pox: &PoxInfo,
        state: Option<&RpcState>,
    ) {
        self.on_this_peer = self.on_this_peer.saturating_add(1);
        if !self.failed && self.on_this_peer < RESELECT_ROUNDS {
            return;
        }
        self.on_this_peer = 0;
        self.failed = false;
        if let Some(chosen) =
            better_peer(&self.peer, config, discovered, executor, pox, state).await
        {
            if let Some(state) = state {
                state.metrics().record_peer_failover();
            }
            self.node = Node::new(chosen.clone());
            self.peer = chosen;
        }
    }
}

/// Follow the peer, publishing what it validated and executing along it.
async fn follow(follower: Follower) -> Role {
    let Follower {
        config,
        network,
        peer,
        discovered,
        advertised,
        relay,
        mempool,
        mut pox,
        source,
        state,
        executor,
        miner_owns_execution,
        mut offered,
        mut submitted,
    } = follower;
    let interval = Duration::from_secs(config.node.poll_interval_secs);
    let staging = match open_staging(&config) {
        Ok(staging) => staging,
        Err(role) => return role,
    };
    let budget = follow_budget(&config);
    prepare_to_follow(executor.as_ref(), &peer, &pox, source).await?;
    let mut rounds = Rounds::new(peer);
    loop {
        rounds.history.refresh(&config, discovered.as_ref());
        publish_peer_report(state.as_ref(), &rounds.history, discovered.as_ref()).await;
        rounds
            .choose_peer(
                &config,
                discovered.as_ref(),
                executor.as_ref(),
                &pox,
                state.as_ref(),
            )
            .await;
        take_admitted(AdmittedInputs {
            offered: &mut offered,
            submitted: &mut submitted,
            executor: executor.as_ref(),
            mempool: &mempool,
            relay: &relay,
            staging: &staging,
            metrics: state.as_ref().map(RpcState::metrics),
        })
        .await;
        // Following the peer's current tenure is pointless while this node is
        // far from it — the tenure descends from blocks it has not executed, so
        // the walk fails every round — and the requests it spends are the ones
        // catching up needs. A node this far back has nothing to serve anyway.
        let catching_up = rounds.catching_up();
        rounds.failed |= track_peer(
            &mut rounds.node,
            &rounds.peer,
            state.as_ref(),
            &mut pox,
            &mut rounds.peer_height,
            catching_up,
        )
        .await;
        if let Some(executor) = executor.as_ref()
            && follower_owns_execution(&miner_owns_execution)
        {
            let inputs = RoundInputs {
                peer: &rounds.peer,
                history: &mut rounds.history.source,
                pox: &pox,
                staging: &staging,
                budget,
                advertised: &advertised,
                claims: discovered
                    .as_ref()
                    .map(Discovered::claims)
                    .unwrap_or_default(),
                metrics: state.as_ref().map(RpcState::metrics),
            };
            let round = execute_round(executor, inputs).await;
            rounds.executed_height = round.executed_height;
            rounds.failed |= round.peer_failed;
            publish_executed_round(ExecutedPublication {
                state: state.as_ref(),
                executor,
                sealed: round.sealed,
                pox: &pox,
                config: &config,
                network,
                published: &mut rounds.published,
                peer: &rounds.peer,
            })
            .await;
        }
        finish_round(
            state.as_ref(),
            &staging,
            &relay,
            &offered,
            &submitted,
            interval,
        )
        .await;
    }
}

const fn follow_budget(config: &Config) -> CatchUpBudget {
    // Bounded so a round ends and execution gets its turn: an unbounded descent
    // would spend every round fetching and never execute what it already holds.
    CatchUpBudget {
        fetch: ROUND_FETCH,
        execute: config.node.max_sync_blocks,
    }
}

/// Backfill the checkpoint's missing ancestor headers before the first round.
///
/// The sortition chain is already seeded while the executor is constructed: that
/// has to happen before its anchor can be applied, whereas these headers are fetched
/// through the peer the follower has selected.
async fn prepare_to_follow(
    executor: Option<&SharedExecutor>,
    peer: &SyncClient,
    pox: &PoxInfo,
    source: [u8; 32],
) -> Role {
    let Some(executor) = executor else {
        return Ok(());
    };
    backfill_ancestors(executor, peer, pox, source).await
}

/// Keep uploaded representations as downloads until execution authenticates them.
///
/// The route checks the self-contained block envelope. Local burn, miner, VRF
/// and signer-set authentication happens when the direct child is promoted into
/// executable staging. Draining the channel rather than awaiting it keeps this on the
/// round's own clock — an upload is visible within one poll interval, and a burst
/// of them cannot starve the peer.
///
/// It is relayed only after execution commits it, through the same announcement
/// path as a peer-fetched block.
fn stage_admitted_blocks(offered: &mut nano_queue::Receiver<NakamotoBlock>, staging: &Staging) {
    while let Ok(block) = offered.try_recv() {
        match staging.download(&block) {
            Ok(_) => println!(
                "downloaded block {} at height {} over the public API",
                block.block_id(),
                block.header.chain_length
            ),
            Err(error) => eprintln!(
                "keeping the uploaded block {} failed: {error}",
                block.block_id()
            ),
        }
    }
}

/// Pass on the transactions the public API admitted.
///
/// Same reasoning as the blocks above and the same place for it: the pool this
/// node keeps is read by its own miner alone, so a node that admits a transaction
/// and tells nobody has accepted it into a hole. Admission already happened — on
/// this node's own rules, against its own accounts — so all that is left is to
/// say so.
fn relay_admitted_transactions(
    submitted: &mut nano_queue::Receiver<nano_codec::Transaction>,
    relay: &nano_p2p::Relay,
) {
    while let Ok(transaction) = submitted.try_recv() {
        println!(
            "relaying the transaction {} this node admitted",
            transaction.txid()
        );
        relay.announce(nano_p2p::Offer::transaction(None, Box::new(transaction)));
    }
}

/// Keep self-consistent peer pushes as downloads and relay them.
///
/// This is the boundary the whole of task 054's relay item turns on, and the reason
/// it is *here* is that here is where the chainstate is.
/// The RPC boundary checks the block's own envelope. The locally derived burn,
/// miner, VRF and signer-set checks run only when a representation becomes the
/// direct child; only their opaque result can enter executable staging.
///
/// A rejected push is *not* a penalty. A block can fail because this node cannot yet
/// derive the cycle's reward set, or has not executed the tenure it builds on, and
/// scoring a peer for that would repeat the third slice's bug of isolating the peers
/// that were working hardest.
async fn check_relayed(
    executor: &SharedExecutor,
    mempool: &Arc<Mutex<nano_mempool::Mempool>>,
    relay: &nano_p2p::Relay,
    staging: &Staging,
    metrics: Option<&nano_rpc::NodeMetrics>,
) {
    let offers = relay.take_offered();
    if offers.is_empty() {
        return;
    }
    let mut transactions = Vec::new();
    let mut accepted = 0_usize;
    let mut rejected = 0_usize;
    {
        let mut executor = executor.lock().await;
        for offer in offers {
            let block = match offer.data {
                nano_p2p::Pushed::Block(block) => block,
                // Held back rather than handled here: admission wants the mempool's
                // lock as well, and taking it under the executor's would invert the
                // order `/v2/transactions` takes them in.
                nano_p2p::Pushed::Transaction(transaction) => {
                    transactions.push((offer.from, transaction));
                    continue;
                }
            };
            // The same call the public API's uploads go through, which is the point:
            // a node that admitted from a peer what it would refuse from its own API
            // is forkable through whichever of the two is laxer.
            match nano_rpc::BlockAdmission::authenticate(&mut *executor, &block) {
                Ok(()) => {
                    if let Err(error) = staging.download(&block) {
                        eprintln!("keeping a relayed block failed: {error}");
                        continue;
                    }
                    accepted += 1;
                }
                Err(error) => {
                    rejected += 1;
                    if let Some(metrics) = metrics {
                        metrics.record_block_refusal(&error);
                    }
                    eprintln!(
                        "a pushed block {} at height {} did not authenticate: {error}",
                        block.block_id(),
                        block.header.chain_length
                    );
                }
            }
        }
    }
    if let Some(metrics) = metrics {
        metrics.record_pushed_blocks(accepted, rejected);
    }
    let admitted = admit_relayed(executor, mempool, relay, transactions, metrics).await;
    if accepted > 0 || rejected > 0 || admitted > 0 {
        println!(
            "peers pushed {accepted} blocks this node accepted and {rejected} it refused, \
             and {admitted} transactions it will mine"
        );
    }
}

/// Admit the transactions peers relayed, and pass on the ones the pool kept.
///
/// Separated from the block half only because of the locks: admission needs the
/// mempool's as well as the executor's, and it takes them in the order
/// `/v2/transactions` takes them, because two loops taking the same pair in opposite
/// orders is a deadlock waiting for load.
async fn admit_relayed(
    executor: &SharedExecutor,
    mempool: &Arc<Mutex<nano_mempool::Mempool>>,
    relay: &nano_p2p::Relay,
    transactions: Vec<(
        Option<nano_primitives::Hash160>,
        Box<nano_codec::Transaction>,
    )>,
    metrics: Option<&nano_rpc::NodeMetrics>,
) -> usize {
    if transactions.is_empty() {
        return 0;
    }
    // Admitted first, relayed after: the announcement is a queue write that does not
    // need either lock, and holding the executor's while doing it would put an inbound
    // push in front of the loop that executes blocks.
    let mut kept = Vec::new();
    let mut mempool = mempool.lock().await;
    let mut executor = executor.lock().await;
    let accounts = ExecutedAccounts::new(&mut *executor);
    let now = now_unix();
    // A follower has no miner assembling blocks, so this is the only place its
    // pool ever drops what a tip confirmed or what aged out — without it the
    // pool was insert-only for the life of the process.
    mempool.advance(&accounts, now);
    for (from, transaction) in transactions {
        let admission = mempool.submit((*transaction).clone(), &accounts, now);
        if matches!(
            admission,
            Ok(nano_mempool::Admission::Added | nano_mempool::Admission::Replaced(_))
        ) {
            kept.push((from, transaction));
        }
    }
    let mempool_size = mempool.len();
    drop(accounts);
    drop(executor);
    drop(mempool);
    if let Some(metrics) = metrics {
        metrics.publish_mempool_size(mempool_size);
    }
    for (from, transaction) in &kept {
        relay.announce(nano_p2p::Offer::transaction(*from, transaction.clone()));
    }
    kept.len()
}

/// This node's own account view, for admitting a transaction a peer relayed.
///
/// Nano's executed state rather than the sending peer's answer about it: a relayed
/// transaction is a transaction, and the rules it has to pass are the ones
/// `/v2/transactions` applies. Accounts are read as the pool asks for them, because
/// which of the origin, payer and recipient it needs is the pool's business.
struct ExecutedAccounts<'a> {
    chain: std::cell::RefCell<&'a mut dyn ChainAccess>,
    accounts: std::cell::RefCell<HashMap<nano_address::StacksAddress, nano_mempool::Account>>,
}

impl<'a> ExecutedAccounts<'a> {
    fn new(chain: &'a mut dyn ChainAccess) -> Self {
        Self {
            chain: std::cell::RefCell::new(chain),
            accounts: std::cell::RefCell::new(HashMap::new()),
        }
    }
}

impl nano_mempool::ChainTip for ExecutedAccounts<'_> {
    fn account(&self, address: &nano_address::StacksAddress) -> nano_mempool::Account {
        if let Some(account) = self.accounts.borrow().get(address) {
            return *account;
        }
        let account = clarity::vm::types::PrincipalData::parse(&address.to_string())
            .ok()
            .and_then(|principal| self.chain.borrow_mut().account(&principal).ok())
            .map_or_else(nano_mempool::Account::default, |entry| {
                nano_mempool::Account {
                    nonce: entry.nonce,
                    balance: Some(entry.balance),
                }
            });
        self.accounts.borrow_mut().insert(*address, account);
        account
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Which reward cycles this node has already answered for, so a walk of the
/// pox-5 signer list happens once per cycle instead of once per round.
#[derive(Default)]
pub(crate) struct RewardCyclePublication {
    served: BTreeSet<u64>,
    /// The cycle whose derivation failure has been reported, so a chain with no
    /// pox-5 stackers says so once rather than every second.
    complained: Option<u64>,
    /// The cycle whose miner slots could not be assigned, for the same reason.
    ambiguous_miners: Option<u64>,
    /// Who `.miners` is currently replicated for, in slot order. Reconfiguring a
    /// contract clears every chunk in it, so this is only done when the writers
    /// change — doing it per round would drop the proposal a signer is reading.
    miner_writers: Option<Vec<nano_primitives::Hash160>>,
}

/// Publish the reward cycles represented by the shared executor, whichever
/// role currently owns execution.
///
/// The follower and optional miner take turns advancing the same executor. The
/// RPC surface must follow that executor rather than either role, or starting a
/// miner silently stops `/v3/stacker_set` at the cycle last published by the
/// follower.
pub(crate) async fn publish_reward_cycles_for_current_execution(
    state: &RpcState,
    executor: &SharedExecutor,
    network: Network,
    pox: &PoxInfo,
    config: &Config,
    published: &mut RewardCyclePublication,
    peer: &SyncClient,
) {
    let (local_writers, registered_miners, burn_height) = {
        let executor = executor.lock().await;
        (
            executor.local_miner_slot_writers(),
            executor.registered_local_miner_keys(),
            executor.derived_bitcoin_height(),
        )
    };
    publish_reward_cycle(RewardCycleInputs {
        state,
        executor,
        network,
        context: bitcoin_context(config, pox),
        local_writers,
        registered_miners: &registered_miners,
        published,
        burn_height,
        peer,
        registry: config.node.pox_5_sbtc_registry_contract.as_deref(),
        checkpoint: &config.checkpoint,
    })
    .await;
}

/// What publishing a cycle reads, as one value for the same reason
/// [`RoundInputs`] is one: eight positional arguments hide which is which.
struct RewardCycleInputs<'a> {
    state: &'a RpcState,
    executor: &'a SharedExecutor,
    network: Network,
    context: nano_chainstate::BitcoinBlockContext,
    /// The two locally elected writers in stacks-core's canonical pair order.
    local_writers: Option<[nano_primitives::Hash160; 2]>,
    /// Every miner key registered by this node's locally derived burnchain.
    registered_miners: &'a [nano_primitives::Hash160],
    published: &'a mut RewardCyclePublication,
    /// The newest burn block this node derived locally, including a block under
    /// which no Stacks block has executed yet.
    burn_height: u64,
    peer: &'a SyncClient,
    /// Where this chain's sBTC registry is deployed, which decides whether the
    /// document can carry a waterfall payout address at all.
    registry: Option<&'a str>,
    /// The checkpoint this node started from, which carries the only answer for
    /// its own reward cycle.
    checkpoint: &'a crate::config::CheckpointConfig,
}

/// Serve the `/v3/stacker_set` document the checkpoint carried, when the cycle
/// asked for is the checkpoint's own.
///
/// Answers `false` for every other cycle, including one this node simply has not
/// reached: the document is one cycle's and pretending otherwise would serve a
/// stale signer set, which is worse than serving none.
async fn carry_checkpoint_set(
    state: &RpcState,
    published: &mut RewardCyclePublication,
    checkpoint: &crate::config::CheckpointConfig,
    cycle: u64,
    context: nano_chainstate::BitcoinBlockContext,
) -> bool {
    let mut at_checkpoint = context;
    at_checkpoint.height = checkpoint.anchor_bitcoin_height;
    if nano_chainstate::signers::reward_cycle_at(at_checkpoint) != Some(cycle) {
        return false;
    }
    let Some(document) = checkpoint
        .attesting_reward_set
        .as_ref()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
    else {
        return false;
    };
    let stacker_set = document["stacker_set"].clone();
    if stacker_set.is_null() {
        return false;
    }
    if !published.served.contains(&cycle) {
        println!(
            "serving the reward set the checkpoint carried for cycle {cycle}, which was \
             stacked before this node's history begins and cannot be derived from its state"
        );
    }
    state.publish_stacker_set(cycle, stacker_set).await;
    published.served.insert(cycle);
    true
}

async fn configure_signer_slots(
    state: &RpcState,
    network: Network,
    cycle: u64,
    entries: &[nano_rpc::RewardSetSigner],
) {
    let writers = entries
        .iter()
        .map(|entry| nano_primitives::hash160(&entry.signing_key))
        .collect::<Vec<_>>();
    let store = state.stackerdb();
    let mut store = store.write().await;
    for message in SIGNER_MESSAGE_IDS {
        let contract = crate::config::cycle_contract(network, cycle, message);
        store.configure(
            &format!("{}.{}", contract.address, contract.name),
            writers.clone(),
        );
    }
    drop(store);
}

/// Publish the current and upcoming reward sets the executed state derives, and
/// configure the `StackerDB` contracts their signers write to.
///
/// Derived from this node's own pox-5 state rather than read from the peer, which
/// is the whole difference between serving a reward set and relaying one. The
/// document nano writes here is the one `SyncClient` parses, so a node's own
/// `/v3/stacker_set` can attest another node's checkpoint.
async fn publish_reward_cycle(inputs: RewardCycleInputs<'_>) {
    let RewardCycleInputs {
        state,
        executor,
        network,
        mut context,
        local_writers,
        registered_miners,
        published,
        burn_height,
        peer,
        registry,
        checkpoint,
    } = inputs;
    context.move_to_burn_block(burn_height);
    let Some(cycle) = nano_chainstate::signers::reward_cycle_at(context) else {
        return;
    };
    configure_miner_slots(
        state,
        network,
        cycle,
        local_writers,
        registered_miners,
        published,
        peer,
    )
    .await;
    let mut signer_inputs = SignerCycleInputs {
        state,
        executor,
        network,
        published,
        registry,
        checkpoint,
    };
    publish_signer_cycle(&mut signer_inputs, context).await;
    if let Some(next_context) = upcoming_signer_cycle_context(context) {
        publish_signer_cycle(&mut signer_inputs, next_context).await;
    }
}

fn upcoming_signer_cycle_context(
    context: nano_chainstate::BitcoinBlockContext,
) -> Option<nano_chainstate::BitcoinBlockContext> {
    let current = nano_chainstate::signers::reward_cycle_at(context)?;
    let upcoming = nano_chainstate::signers::prepare_phase_reward_cycle(context)?;
    if upcoming == current {
        return None;
    }
    let cycle_length = u64::from(context.prepare_phase_length)
        .saturating_add(u64::from(context.reward_phase_length));
    let mut next = context;
    next.move_to_burn_block(context.height.saturating_add(cycle_length));
    (nano_chainstate::signers::reward_cycle_at(next) == Some(upcoming)).then_some(next)
}

struct SignerCycleInputs<'a> {
    state: &'a RpcState,
    executor: &'a SharedExecutor,
    network: Network,
    published: &'a mut RewardCyclePublication,
    registry: Option<&'a str>,
    checkpoint: &'a crate::config::CheckpointConfig,
}

async fn publish_signer_cycle(
    inputs: &mut SignerCycleInputs<'_>,
    context: nano_chainstate::BitcoinBlockContext,
) {
    let Some(cycle) = nano_chainstate::signers::reward_cycle_at(context) else {
        return;
    };
    if inputs.published.served.contains(&cycle) {
        return;
    }
    // The lock is held only for the walk: it is the same lock every account read
    // takes, and the walk is one contract call per signer.
    let derived = nano_chainstate::signers::active_signer_set(
        inputs.executor.lock().await.chainstate_mut().vm_mut(),
        context,
    );
    let derived = match derived {
        Ok(derived) => derived,
        // Nothing to walk is the ordinary answer for the cycle a checkpointed
        // node starts in, and it is not a fault: that cycle was stacked before
        // the boundary — on mainnet, in pox-4 — so it has no pox-5 positions and
        // cannot be derived from this state at all. What the network published
        // for it is what the checkpoint already carries to attest itself with,
        // so it is served verbatim rather than not at all.
        Err(_)
            if carry_checkpoint_set(
                inputs.state,
                inputs.published,
                inputs.checkpoint,
                cycle,
                context,
            )
            .await =>
        {
            return;
        }
        Err(error) => {
            if inputs.published.complained != Some(cycle) {
                inputs.published.complained = Some(cycle);
                eprintln!(
                    "this node cannot derive the reward set for cycle {cycle} from its own \
                     state, so /v3/stacker_set will not answer for it and its signers' \
                     StackerDB contracts stay unconfigured: {error}"
                );
            }
            return;
        }
    };
    let entries = nano_rpc::derived_signers(&derived);
    // The one output a waterfall cycle pays, derived from this node's own sBTC
    // registry state. Without it the document cannot claim the 4.0 shape, so a
    // chain whose registry nano cannot read is served the version every reader
    // accepts and the reason is said once.
    let payout = inputs
        .executor
        .lock()
        .await
        .chainstate_mut()
        .sbtc_payout_address(inputs.registry);
    let sbtc_address = match payout {
        Ok(address) => Some(address),
        Err(error) => {
            if inputs.published.complained != Some(cycle) {
                inputs.published.complained = Some(cycle);
                eprintln!(
                    "this node cannot derive the waterfall payout address from its own sBTC \
                     registry state, so /v3/stacker_set/{cycle} carries the version 0 shape \
                     without it: {error}"
                );
            }
            None
        }
    };
    inputs
        .state
        .publish_stacker_set(
            cycle,
            nano_rpc::stacker_set_payload(
                &entries,
                derived.pox_ustx_threshold,
                sbtc_address.as_ref(),
            ),
        )
        .await;
    configure_signer_slots(inputs.state, inputs.network, cycle, &entries).await;
    inputs.published.served.insert(cycle);
    println!(
        "derived the reward set for cycle {cycle} from this node's own state: {} signers, \
         {} of weight, replicating their StackerDB contracts",
        entries.len(),
        entries
            .iter()
            .map(|entry| u64::from(entry.weight))
            .sum::<u64>()
    );
}

/// Proposal and pushed-block slots held by each of the two retained miners.
const MINER_SLOTS_PER_WRITER: usize = 2;
/// The current and prior sortition winner retained by `.miners`.
const MINER_WRITERS: usize = 2;

/// Replicate `.miners`, so a signer hosted here can read what a miner proposed.
///
/// The local sortition chain carries stacks-core's cumulative sortition count,
/// so its parity gives the exact pair order. Signed peer metadata is only a
/// bootstrap fallback for an old capture that did not retain that count. Every
/// writer still has to exist in this node's local leader-key registry.
async fn configure_miner_slots(
    state: &RpcState,
    network: Network,
    cycle: u64,
    local_writers: Option<[nano_primitives::Hash160; 2]>,
    registered_miners: &[nano_primitives::Hash160],
    published: &mut RewardCyclePublication,
    peer: &SyncClient,
) {
    if local_writers.is_none() && registered_miners.is_empty() {
        return;
    }
    let contract = crate::config::miner_contract(network);
    let assignment = match local_miner_slots(local_writers, registered_miners) {
        Some(assignment) => Some(assignment),
        None => miner_slots(peer, &contract, registered_miners).await,
    };
    let Some(assignment) = assignment else {
        if published.ambiguous_miners != Some(cycle) {
            published.ambiguous_miners = Some(cycle);
            eprintln!(
                "this node cannot authenticate both .miners pair owners against its local \
                 leader-key registry, so it replicates neither: a slot assigned to the wrong \
                 writer refuses the proposals it exists for"
            );
        }
        return;
    };
    if published.miner_writers.as_ref() == Some(&assignment) {
        return;
    }
    published.miner_writers = Some(assignment.clone());
    let names = assignment
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    state
        .stackerdb()
        .write()
        .await
        .configure(&crate::hosting::identifier(&contract), assignment);
    println!("replicating .miners for the miners that hold its slots, in order: {names}");
}

fn local_miner_slots(
    writers: Option<[nano_primitives::Hash160; 2]>,
    registered_miners: &[nano_primitives::Hash160],
) -> Option<Vec<nano_primitives::Hash160>> {
    let [first, second] = writers?;
    [first, second]
        .iter()
        .all(|writer| registered_miners.contains(writer))
        .then_some(vec![first, first, second, second])
}

/// Who owns each `.miners` slot, read off the peer's own listing.
///
/// Each slot's metadata is signed by the writer that owns it, so recovering the
/// signature says who that is — checked against the miners this node registered
/// from Bitcoin, so a peer naming a stranger gets nothing configured.
///
/// **Both miner pairs have to resolve.** A slot nano assigns to the wrong writer
/// refuses the very chunks it exists for, and a `.miners` replica that refuses
/// proposals is worse than one that has none: the first looks configured.
async fn miner_slots(
    peer: &SyncClient,
    contract: &nano_stackerdb::StackerDbContract,
    registered_miners: &[nano_primitives::Hash160],
) -> Option<Vec<nano_primitives::Hash160>> {
    let client = nano_stackerdb::StackerDbClient::new(peer.base_url().clone()).ok()?;
    let listing = client.slot_metadata(contract).await.ok()?;
    authenticated_miner_slots(listing, registered_miners)
}

fn authenticated_miner_slots(
    listing: impl IntoIterator<Item = nano_stackerdb::SlotMetadata>,
    registered_miners: &[nano_primitives::Hash160],
) -> Option<Vec<nano_primitives::Hash160>> {
    let mut owners = [None; MINER_WRITERS];
    for metadata in listing {
        if metadata.slot_version == 0 {
            continue;
        }
        let Ok(slot) = usize::try_from(metadata.slot_id) else {
            continue;
        };
        let owner = slot / MINER_SLOTS_PER_WRITER;
        if owner >= MINER_WRITERS {
            continue;
        }
        let writer = metadata.writer().ok()?;
        if !registered_miners.contains(&writer) {
            return None;
        }
        if owners[owner].is_some_and(|assigned| assigned != writer) {
            return None;
        }
        owners[owner] = Some(writer);
    }
    match owners {
        [Some(first), Some(second)] => Some(vec![first, first, second, second]),
        _ => None,
    }
}

/// Open the executed state, when this node is a role that reads it.
///
/// The chain is only executed when something reads it: a signer-only node validates
/// proposals in its own store and would be executing every block twice for nobody.
async fn open_executed_state(
    config: &Config,
    network: Network,
    pox: &PoxInfo,
    discovered: Option<&Discovered>,
) -> Result<Option<SharedExecutor>, Box<dyn Error>> {
    if config.node.rpc_bind.is_none() && config.miner.is_none() {
        return Ok(None);
    }
    let phase = Phase::start("opening the executed state");
    // Every peer known, so a resume asks the network rather than whichever peer
    // answered first.
    let mut resume_pool = TenureSource::new(follow_pool(config, discovered).into_clients());
    let executor = open_executor(
        config,
        network,
        pox,
        &mut resume_pool,
        &config.chainstate_dir(NODE_CHAINSTATE),
    )
    .await?;
    drop(phase);
    Ok(Some(Arc::new(Mutex::new(executor))))
}

/// Open the chain this node executes, resuming whatever is already on disk.
///
/// The first start imports the checkpoint and applies the block after it. Every
/// later start finds the store sealed at a block of its own and carries on from
/// there, importing and replaying nothing.
#[derive(Clone, Copy, Debug)]
struct LoadedBoundaryProof {
    parent_tenure_consensus_hash: nano_primitives::ConsensusHash,
    coinbase_vrf_proof: [u8; 80],
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundaryProofRecord {
    parent_tenure_consensus_hash: String,
    coinbase_vrf_proof: String,
}

fn fixed_hex<const N: usize>(field: &str, value: &str) -> Result<[u8; N], Box<dyn Error>> {
    let bytes = hex::decode(value).map_err(|_| format!("{field} is not hexadecimal"))?;
    <[u8; N]>::try_from(bytes.as_slice()).map_err(|_| format!("{field} is not {N} bytes").into())
}

fn load_checkpoint_authentication_history(
    config: &Config,
) -> Result<(LoadedBoundaryProof, Vec<NakamotoBlock>), Box<dyn Error>> {
    let root = config.checkpoint.authentication_history.as_ref().ok_or(
        "this fresh executing node has no authenticated checkpoint block suffix: set \
         `checkpoint.authentication_history` to a directory containing boundary.json and \
         blocks/*.bin",
    )?;
    let boundary_path = root.join("boundary.json");
    let record: BoundaryProofRecord = serde_json::from_slice(&fs::read(&boundary_path)?)
        .map_err(|error| format!("{}: {error}", boundary_path.display()))?;
    let boundary = LoadedBoundaryProof {
        parent_tenure_consensus_hash: nano_primitives::ConsensusHash::from_bytes(fixed_hex(
            "authentication boundary parent_tenure_consensus_hash",
            &record.parent_tenure_consensus_hash,
        )?),
        coinbase_vrf_proof: fixed_hex(
            "authentication boundary coinbase_vrf_proof",
            &record.coinbase_vrf_proof,
        )?,
    };

    let blocks_directory = root.join("blocks");
    let mut paths = fs::read_dir(&blocks_directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    paths.retain(|path| path.extension().is_some_and(|extension| extension == "bin"));
    paths.sort();
    if paths.is_empty() {
        return Err(format!(
            "checkpoint authentication history {} contains no block files",
            blocks_directory.display()
        )
        .into());
    }
    if paths.len() > CHECKPOINT_HISTORY_LIMIT {
        return Err(format!(
            "checkpoint authentication history has {} blocks, above the bounded limit of {CHECKPOINT_HISTORY_LIMIT}",
            paths.len()
        )
        .into());
    }
    let mut by_id = HashMap::with_capacity(paths.len());
    for path in paths {
        let block = NakamotoBlock::decode(&fs::read(&path)?)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let id = *block.block_id().as_bytes();
        if by_id.insert(id, block).is_some() {
            return Err(format!(
                "checkpoint authentication history contains block {} more than once",
                hex::encode(id)
            )
            .into());
        }
    }
    let source = config.checkpoint.source_state_id()?;
    let mut cursor = source;
    let mut reversed = Vec::with_capacity(by_id.len());
    while let Some(block) = by_id.remove(&cursor) {
        cursor = *block.header.parent_block_id.as_bytes();
        reversed.push(block);
    }
    if reversed.is_empty() {
        return Err(format!(
            "checkpoint authentication history contains no source block {}",
            hex::encode(source)
        )
        .into());
    }
    if !by_id.is_empty() {
        return Err(format!(
            "checkpoint authentication history has {} block(s) disconnected from source {}",
            by_id.len(),
            hex::encode(source)
        )
        .into());
    }
    reversed.reverse();
    Ok((boundary, reversed))
}

fn contextualize_checkpoint_history<S: nano_bitcoin::BitcoinSource>(
    pox: &PoxInfo,
    tracker: &SortitionTracker,
    bitcoin: &mut S,
    boundary: CheckpointBoundaryProof,
    blocks: &[NakamotoBlock],
) -> Result<(CheckpointBoundaryProof, Vec<CheckpointHistoryBlock>), Box<dyn Error>>
where
    S::Error: std::fmt::Display,
{
    let mut current_view = None;
    let mut history = Vec::with_capacity(blocks.len());
    for block in blocks {
        if let Some(view) = block.bitcoin_view_consensus_hash() {
            current_view = Some(view);
        }
        let view = current_view.unwrap_or(block.header.consensus_hash);
        let view_height = tracker.height_of_consensus_hash(view).ok_or_else(|| {
            format!(
                "checkpoint history block {} names burn view {view}, which the local sortition chain does not hold",
                block.header.chain_length
            )
        })?;
        let snapshot = tracker.snapshot_at(view_height).ok_or_else(|| {
            format!(
                "checkpoint history block {} needs local sortition snapshot at burn {view_height}, which was not retained",
                block.header.chain_length
            )
        })?;
        if snapshot.total_burn != block.header.bitcoin_spent {
            return Err(format!(
                "checkpoint history block {} says {} burn has been spent, local sortition at burn {view_height} derives {}",
                block.header.chain_length, block.header.bitcoin_spent, snapshot.total_burn
            )
            .into());
        }
        let tenure_height = tracker
            .height_of_consensus_hash(block.header.consensus_hash)
            .ok_or_else(|| {
                format!(
                    "checkpoint history block {} names tenure {}, which the local sortition chain does not hold",
                    block.header.chain_length, block.header.consensus_hash
                )
            })?;
        let mut context = pox.bitcoin_context();
        crate::LocalSortition::from_snapshot(snapshot).record(&mut context);
        if tenure_height != view_height {
            context.move_to_burn_block(tenure_height);
            context.extend_view_to(view_height);
        }
        let operations = bitcoin
            .block_at(tenure_height)
            .map_err(|error| format!("Bitcoin block {tenure_height}: {error}"))?
            .operations;
        history.push(CheckpointHistoryBlock {
            block: block.clone(),
            bitcoin_context: context,
            operations,
        });
    }
    Ok((boundary, history))
}

fn local_anchor_context(
    pox: &PoxInfo,
    tracker: &SortitionTracker,
    chainstate: &mut ChainState,
    anchor: &NakamotoBlock,
    view_height: u64,
) -> Result<BitcoinBlockContext, Box<dyn Error>> {
    if let Some(view) = anchor.bitcoin_view_consensus_hash()
        && tracker.height_of_consensus_hash(view) != Some(view_height)
    {
        return Err(format!(
            "anchor names burn view {view}, which the local sortition chain does not place at configured burn {view_height}"
        )
        .into());
    }
    let snapshot = tracker.snapshot_at(view_height).ok_or_else(|| {
        format!("the local sortition chain retained no snapshot for anchor burn {view_height}")
    })?;
    if snapshot.total_burn != anchor.header.bitcoin_spent {
        return Err(format!(
            "anchor says {} burn has been spent, local sortition at burn {view_height} derives {}",
            anchor.header.bitcoin_spent, snapshot.total_burn
        )
        .into());
    }
    let tenure_height = tracker
        .height_of_consensus_hash(anchor.header.consensus_hash)
        .ok_or_else(|| "the anchor's tenure is absent from the local sortition chain".to_owned())?;
    let mut context = pox.bitcoin_context();
    crate::LocalSortition::from_snapshot(snapshot).record(&mut context);
    if tenure_height != view_height {
        context.move_to_burn_block(tenure_height);
        context.extend_view_to(view_height);
    }
    if nano_chainstate::starts_new_tenure(anchor)
        && let Some(schedule) = chainstate.accounting_mut().schedule()
    {
        let previous = tracker.previous_sortition_height(view_height).ok_or_else(|| {
            format!(
                "the local sortition chain cannot derive accumulated coinbase for anchor burn {view_height}"
            )
        })?;
        context.accumulated_coinbase = schedule.accumulated_at(view_height, Some(previous));
    }
    Ok(context)
}

fn checkpoint_sortition_tracker(
    config: &Config,
    chainstate: &ChainState,
    anchor: &NakamotoBlock,
    context: Option<BitcoinBlockContext>,
    fresh_boundary: Option<nano_primitives::ConsensusHash>,
) -> Result<(SortitionTracker, BurnchainSource), Box<dyn Error>> {
    let capture = config.checkpoint.sortition.as_ref().ok_or(
        "this node executes blocks and has no checkpoint sortition history to derive burn \
         views from: set `checkpoint.sortition` to a directory holding snapshots.json, \
         consensus-hashes.json and leader-keys.json, which `cargo xtask export-sortition` writes",
    )?;
    let executed_burn_view = context.map_or_else(
        || {
            chainstate
                .recorded_header(*anchor.block_id().as_bytes())
                .map_or(0, |header| u64::from(header.burn_block_height))
        },
        |context| context.height,
    );
    let mut tracker = if let Some(boundary) = fresh_boundary {
        SortitionTracker::from_capture_at_consensus(capture, boundary)?
    } else {
        SortitionTracker::resume_or_capture_below(
            &config.node.working_dir,
            capture,
            executed_burn_view,
        )
        .map_err(|error| {
            format!(
                "this node cannot derive sortitions of its own, and will not execute blocks under \
                 a burn view a peer chose: {error}"
            )
        })?
    };
    if tracker.leader_keys() == 0 {
        return Err(format!(
            "this checkpoint carries no leader-key registry, so this node could check no \
             tenure's coinbase proof and no miner's signature against the key that won the \
             sortition -- and would accept every tenure without checking either. \
             `cargo xtask export-leader-keys` writes one into {}",
            capture.display()
        )
        .into());
    }
    let mut bitcoin = bitcoin_source(config)?;
    tracker.recover_seed(|height| bitcoin.block_at(height))?;
    Ok((tracker, bitcoin))
}

fn derive_checkpoint_authentication<S: nano_bitcoin::BitcoinSource>(
    pox: &PoxInfo,
    tracker: &mut SortitionTracker,
    bitcoin: &mut S,
    boundary: LoadedBoundaryProof,
    history: &[NakamotoBlock],
    target: u64,
) -> Result<(CheckpointBoundaryProof, Vec<CheckpointHistoryBlock>), Box<dyn Error>>
where
    S::Error: std::fmt::Display,
{
    advance_to_checkpoint_boundary(
        pox,
        tracker,
        bitcoin,
        boundary.parent_tenure_consensus_hash,
        target,
    )?;
    let boundary_snapshot = tracker.tip();
    let boundary_consensus_hash = boundary_snapshot.consensus_hash;
    let boundary_sortition_hash = *boundary_snapshot.sortition_hash.as_bytes();
    if boundary_consensus_hash != boundary.parent_tenure_consensus_hash {
        return Err(format!(
            "fresh checkpoint sortition seed {} does not equal authentication boundary {}",
            boundary_consensus_hash, boundary.parent_tenure_consensus_hash
        )
        .into());
    }
    let boundary_height = boundary_snapshot.bitcoin_height;
    if target < boundary_height {
        return Err(format!(
            "checkpoint anchor burn {target} is below authentication boundary burn {boundary_height}"
        )
        .into());
    }
    let boundary_block = bitcoin
        .block_at(boundary_height)
        .map_err(|error| format!("Bitcoin block {boundary_height}: {error}"))?;
    let winner_vrf_public_key = tracker.authenticate_boundary_winner(&boundary_block)?;
    let boundary = CheckpointBoundaryProof {
        parent_tenure_consensus_hash: boundary.parent_tenure_consensus_hash,
        coinbase_vrf_proof: boundary.coinbase_vrf_proof,
        sortition_hash: boundary_sortition_hash,
        winner_vrf_public_key,
    };
    let payouts = crate::payout_schedule(pox).ok_or(
        "the checkpoint authentication history cannot be checked without a PoX payout calendar",
    )?;
    tracker.keep_from(boundary_height);
    let limit = target - boundary_height;
    tracker.catch_up(|height| bitcoin.block_at(height), target, payouts, limit)?;
    if tracker.tip().bitcoin_height != target {
        return Err(format!(
            "local sortition derivation stopped at burn {}, before checkpoint anchor burn {target}",
            tracker.tip().bitcoin_height
        )
        .into());
    }
    contextualize_checkpoint_history(pox, tracker, bitcoin, boundary, history)
}

fn advance_to_checkpoint_boundary<S: nano_bitcoin::BitcoinSource>(
    pox: &PoxInfo,
    tracker: &mut SortitionTracker,
    bitcoin: &mut S,
    boundary: nano_primitives::ConsensusHash,
    target: u64,
) -> Result<(), Box<dyn Error>>
where
    S::Error: std::fmt::Display,
{
    let payouts = crate::payout_schedule(pox).ok_or(
        "the checkpoint authentication history cannot be checked without a PoX payout calendar",
    )?;
    while tracker.tip().consensus_hash != boundary {
        let height = tracker.tip().bitcoin_height;
        if height >= target {
            return Err(format!(
                "local sortition derivation reached checkpoint anchor burn {target} without authentication boundary {boundary}"
            )
            .into());
        }
        let next = height
            .checked_add(1)
            .ok_or("checkpoint authentication boundary burn height overflow")?;
        tracker.catch_up(|height| bitcoin.block_at(height), next, payouts, 1)?;
        if tracker.tip().bitcoin_height != next {
            return Err(format!(
                "local sortition derivation stopped at burn {}, before authentication boundary {boundary}",
                tracker.tip().bitcoin_height
            )
            .into());
        }
    }
    Ok(())
}

fn persist_validator_sortitions_at_standing<S: nano_bitcoin::BitcoinSource>(
    pox: &PoxInfo,
    tracker: &mut SortitionTracker,
    bitcoin: &mut S,
    state: &Path,
    standing: u64,
) -> Result<(), Box<dyn Error>>
where
    S::Error: std::fmt::Display,
{
    if tracker.tip().bitcoin_height < standing {
        let payouts = crate::payout_schedule(pox).ok_or(
            "the proposal validator cannot derive its standing burn view without a PoX payout calendar",
        )?;
        let limit = standing - tracker.tip().bitcoin_height;
        tracker.catch_up(|height| bitcoin.block_at(height), standing, payouts, limit)?;
    }
    if tracker.snapshot_at(standing).is_none() {
        return Err(format!(
            "local sortition derivation stopped at burn {}, before the proposal validator's standing burn {standing}",
            tracker.tip().bitcoin_height
        )
        .into());
    }
    tracker.save_standing_on(state, standing)?;
    Ok(())
}

fn authenticate_fresh_checkpoint(
    config: &Config,
    pox: &PoxInfo,
    chainstate: &mut ChainState,
    tracker: &mut SortitionTracker,
    bitcoin: &mut BurnchainSource,
    boundary: LoadedBoundaryProof,
    history: &[NakamotoBlock],
) -> Result<(), Box<dyn Error>> {
    let (boundary, history) = derive_checkpoint_authentication(
        pox,
        tracker,
        bitcoin,
        boundary,
        history,
        config.checkpoint.anchor_bitcoin_height,
    )?;
    let tenure_starts = history
        .iter()
        .filter(|entry| nano_chainstate::starts_new_tenure(&entry.block))
        .count();
    chainstate.authenticate_checkpoint_history(
        config.checkpoint.source_state_id()?,
        config.checkpoint.state_root()?,
        boundary,
        &history,
    )?;
    println!(
        "authenticated checkpoint history: {} block envelope/miner-signature/winner-sortition/signer-threshold checks, {} tenure-continuity checks, {tenure_starts} tenure-start coinbase-vrf/parent-seed checks, and 1 finite-boundary parent proof",
        history.len(),
        history.len().saturating_sub(1),
    );
    Ok(())
}

pub async fn open_executor(
    config: &Config,
    network: Network,
    pox: &PoxInfo,
    peers: &mut TenureSource,
    directory: &Path,
) -> Result<CheckpointExecutor<BurnchainSource>, Box<dyn Error>> {
    open_executor_with_sortition_copy(config, network, pox, peers, directory, None).await
}

pub(crate) async fn open_validator_executor(
    config: &Config,
    network: Network,
    pox: &PoxInfo,
    peers: &mut TenureSource,
    directory: &Path,
    sortition_state: &Path,
) -> Result<CheckpointExecutor<BurnchainSource>, Box<dyn Error>> {
    open_executor_with_sortition_copy(
        config,
        network,
        pox,
        peers,
        directory,
        Some(sortition_state),
    )
    .await
}

async fn open_executor_with_sortition_copy(
    config: &Config,
    network: Network,
    pox: &PoxInfo,
    peers: &mut TenureSource,
    directory: &Path,
    sortition_copy: Option<&Path>,
) -> Result<CheckpointExecutor<BurnchainSource>, Box<dyn Error>> {
    let (mut chainstate, anchor, context) =
        open_chainstate(config, network, pox, peers, directory).await?;
    let checkpoint_history = context
        .as_ref()
        .map(|_| load_checkpoint_authentication_history(config))
        .transpose()?;
    let fresh_boundary = checkpoint_history
        .as_ref()
        .map(|(boundary, _)| boundary.parent_tenure_consensus_hash);
    let (mut tracker, mut bitcoin) =
        checkpoint_sortition_tracker(config, &chainstate, &anchor, context, fresh_boundary)?;
    if let Some((boundary, history)) = checkpoint_history {
        authenticate_fresh_checkpoint(
            config,
            pox,
            &mut chainstate,
            &mut tracker,
            &mut bitcoin,
            boundary,
            &history,
        )?;
    }
    if let Some(state) = sortition_copy {
        let standing = context.as_ref().map_or_else(
            || {
                chainstate
                    .recorded_header(*anchor.block_id().as_bytes())
                    .map_or(0, |header| u64::from(header.burn_block_height))
            },
            |context| context.height,
        );
        persist_validator_sortitions_at_standing(pox, &mut tracker, &mut bitcoin, state, standing)?;
    }
    println!(
        "deriving sortitions locally from burn {} on PoX history {}",
        tracker.tip().bitcoin_height,
        tracker.tip().pox_id
    );
    let mut executor = if context.is_some() {
        let context = local_anchor_context(
            pox,
            &tracker,
            &mut chainstate,
            &anchor,
            config.checkpoint.anchor_bitcoin_height,
        )?;
        CheckpointExecutor::from_chainstate_using_registry(
            chainstate,
            anchor,
            context,
            bitcoin,
            config.node.pox_5_sbtc_registry_contract.clone(),
        )?
    } else {
        let mut executor = CheckpointExecutor::resume(chainstate, anchor, bitcoin);
        executor.use_waterfall_registry(config.node.pox_5_sbtc_registry_contract.clone());
        executor
    };
    executor.track_sortitions(tracker, config.node.working_dir.clone());
    Ok(executor)
}

/// The chainstate a role executes from, and the block it is sealed at.
///
/// The returned context is the one the anchor still has to be applied under,
/// and is `None` when the state on disk already holds it.
pub async fn open_chainstate(
    config: &Config,
    network: Network,
    pox: &PoxInfo,
    peers: &mut TenureSource,
    directory: &Path,
) -> Result<(ChainState, NakamotoBlock, Option<BitcoinBlockContext>), Box<dyn Error>> {
    let source = config.checkpoint.source_state_id()?;
    adopt(config, directory, source)?;
    let mut chainstate = ChainState::open_from_checkpoint(
        network,
        directory,
        &config.checkpoint.marf,
        source,
        config.checkpoint.state_root()?,
    )?;

    let Some(tip) = chainstate.tip()?.filter(|tip| *tip != source) else {
        // Nothing has been sealed here, so there is no ledger to recover: the
        // first tenures a node executes pay out rewards earned before it
        // existed, and only the checkpoint knows them.
        *chainstate.accounting_mut() = accounting(config, directory)?;
        let anchor = NakamotoBlock::decode(&fs::read(&config.checkpoint.anchor_block)?)?;
        let mut context = bitcoin_context(config, pox);
        context.move_to_burn_block(config.checkpoint.anchor_bitcoin_height);
        return Ok((chainstate, anchor, Some(context)));
    };
    let tip = deepest_block_a_ledger_names(&chainstate, tip, config.node.max_sync_blocks)?;
    // A peer that does not have this block yet is usually one still catching
    // up, not a chain that moved: it is worth waiting for. A peer that never
    // produces it means this state descends from a block the network dropped,
    // and no amount of waiting fixes that.
    //
    // Collected before any request, so the store is not borrowed across one, and
    // *bounded*. A checkpoint import brings the whole ancestry with it — 8.6
    // million blocks on mainnet — and walking all of it was one SQLite row read
    // per block against a 23 GB database: minutes of a node printing nothing,
    // gigabytes read, and a 277 MB list, on every start. To reach a fork race
    // that is one block deep. The bound is what the list is actually for.
    let mut ancestors = Vec::new();
    let mut walk = tip;
    while ancestors.len() < RESUME_ANCESTORS {
        let Some(parent) = chainstate.parent_of(walk)? else {
            break;
        };
        ancestors.push(parent);
        walk = parent;
    }
    let tip = resume_from(ancestors, peers, tip, directory).await?;
    println!(
        "resuming {} from the state on disk, sealed at block {} of height {}",
        directory.display(),
        tip.block_id(),
        tip.header.chain_length
    );
    // A state above the block this node is resuming at is one no ledger names: the
    // residue of a block that was executed and then abandoned, either because a kill
    // landed between the two writes of a commit or because the chain moved on from it
    // while this node was down. It has to be given back rather than ignored, because
    // the MARF refuses to begin a version that already exists -- so a node that
    // fetched that block again failed *every* round for as long as it ran, which is
    // what a live mainnet node did until this was here.
    if let Ok(height) = u32::try_from(tip.header.chain_length) {
        match chainstate.discard_above(height) {
            Ok(0) => {}
            Ok(given_back) => println!(
                "gave back {given_back} sealed states above {height}, which no ledger named"
            ),
            // Reported rather than fatal: a node that cannot give them back is a node
            // that will fail at the first block it re-executes, and saying so here is
            // more use than a start that dies without naming the reason.
            Err(error) => eprintln!("cannot give back the states above {height}: {error}"),
        }
    }
    recover_ledger(&mut chainstate, *tip.block_id().as_bytes())?;
    Ok((chainstate, tip, None))
}

/// The deepest sealed state at or below `tip` that a ledger names.
///
/// The deepest sealed state is not always a block this node executed. A block is
/// committed by writing its ledger and *then* sealing the MARF, so a state whose
/// ledger is gone is one nothing points at — and standing on it costs the whole
/// recovery: no reorganization reach, no tenure start heights and no parent tenure
/// proof. Such a restart is refused instead of rebuilding partial state from
/// `accounting.json`.
///
/// A live mainnet state was left exactly there. It held sealed states to 8,713,522
/// and one single ledger, for 8,713,222, because the block it could not execute
/// re-wrote that row 766 times and pruned the rest away. Resuming at the seal put
/// it on a block no ledger named; resuming at the ledger puts it back on its own
/// chain with 300 states to give back and re-execute.
///
/// The reach is a catch-up round's worth of blocks, because that is how deep a run
/// can seal before it fails: past that there is nothing to find, since every block
/// sealed writes a ledger first.
fn deepest_block_a_ledger_names(
    chainstate: &ChainState,
    tip: [u8; 32],
    reach: usize,
) -> Result<[u8; 32], ChainStateError> {
    let mut walk = tip;
    for walked in 0..reach {
        if chainstate.has_ledger(walk) {
            if walked > 0 {
                println!(
                    "the deepest sealed state {} has no ledger to stand on, so this run \
                     resumes {walked} blocks back at {}, which has one, and gives back what \
                     is above it",
                    hex::encode(tip),
                    hex::encode(walk)
                );
            }
            return Ok(walk);
        }
        let Some(parent) = chainstate.parent_of(walk)? else {
            break;
        };
        walk = parent;
    }
    // Nothing within reach has one. Reported here too, because the seal it is
    // about to resume at is not the reason -- the missing ledgers are. Recovery
    // will refuse once the network confirms which sealed block it would resume.
    eprintln!(
        "no sealed state at or within {reach} blocks below {} has a ledger, so this run \
         cannot stand on one",
        hex::encode(tip)
    );
    Ok(tip)
}

/// Stand on the state the run that sealed this block kept beside the MARF.
///
/// Recovered for the block this node is *resuming at*, which is not always the
/// deepest one it sealed: a tip that lost a fork race while the node was down is
/// abandoned for an ancestor, and the ledger has to be that ancestor's.
fn recover_ledger(chainstate: &mut ChainState, tip: [u8; 32]) -> Result<(), Box<dyn Error>> {
    if chainstate.recover_ledger_at(tip)? {
        // Named field by field, because each one is a thing this node can do that
        // a run without it silently could not: walk a reorganization back, answer
        // `get-tenure-info?` for the tenure in flight, check the seed the next
        // tenure commits.
        let tenure = chainstate
            .recorded_header(tip)
            .map(|header| header.tenure_height);
        println!(
            "recovered the ledger committed with block {}: {} executed blocks to walk back \
             over, tenure {} starting at height {}, parent tenure proof {}",
            hex::encode(tip),
            chainstate.executed_blocks().len(),
            tenure.map_or_else(|| "unknown".to_owned(), |height| height.to_string()),
            tenure
                .and_then(|height| chainstate.tenure_start_height(height))
                .map_or_else(|| "unknown".to_owned(), |height| height.to_string()),
            if chainstate.parent_tenure_proof().is_some() {
                "present"
            } else {
                "absent"
            }
        );
        // The same check the checkpoint and `accounting.json` get. It was missing
        // here, and a resumed node is the one that runs for hours: the live
        // mainnet state carried a hole at 251,322–251,329 — eight tenures nano
        // executed and did not record — through 8,000 blocks of replay, because
        // the only path that validates a maturity window was the one this node
        // had stopped taking. `known_earnings_span` answers the *contiguous* run,
        // so a hole shortens it rather than hiding inside it.
        return check_maturity_window(chainstate.accounting_mut());
    }
    Err(format!(
        "block {} has no committed ledger, so this node cannot authenticate a restart: \
         restore a complete checkpoint or repair the ledger before starting",
        hex::encode(tip)
    )
    .into())
}

/// Find the block a resumed state can carry on from.
///
/// A peer that does not have our sealed tip is usually one still catching up,
/// so it is worth waiting for. If it never produces the block, the tip lost a
/// fork race while this node was down — an ordinary event, one block deep —
/// and the answer is to walk back to the nearest ancestor the peer does have
/// rather than to refuse to start. Only a state with no ancestor on the
/// network at all is one nothing can extend.
/// How far back a resumed node looks for a block the network still has.
///
/// A tip that lost a fork race while the node was down is one block behind the
/// canonical chain, sometimes a few. This is generous for that and cheap, where
/// the whole ancestry is neither.
const RESUME_ANCESTORS: usize = 256;

async fn resume_from(
    ancestors: Vec<[u8; 32]>,
    peers: &mut TenureSource,
    tip: [u8; 32],
    directory: &Path,
) -> Result<NakamotoBlock, Box<dyn Error>> {
    let sealed = StacksBlockId::from_bytes(tip);
    let mut waited = 0;
    loop {
        // The pool, not one peer. One peer's 404 is that peer's answer: a node
        // whose sealed tip is a block the peer it happened to reach has not got
        // yet would walk its own chain back and abandon state that is perfectly
        // canonical. `TenureSource::block` asks the others and refuses an answer
        // that is not the block asked for, so the walk below only starts once
        // *nobody* has it.
        match peers.block(sealed).await {
            Ok(block) => return Ok(block),
            Err(_) if waited < RESUME_ATTEMPTS => {
                waited += 1;
                peers.forgive_throttles();
                println!("waiting for the peers to catch up to block {sealed}");
                sleep(Duration::from_secs(1)).await;
            }
            Err(_) => break,
        }
    }

    for (walked, ancestor) in ancestors.iter().enumerate() {
        peers.forgive_throttles();
        if let Ok(block) = peers.block(StacksBlockId::from_bytes(*ancestor)).await {
            println!(
                "block {sealed} left the chain; carrying on from {}, {} back",
                block.block_id(),
                walked + 1
            );
            return Ok(block);
        }
    }

    Err(format!(
        "the state in {} is sealed at block {sealed}, and no peer in the pool has any of its \
         {} ancestors either; nothing the network serves extends it, so it needs a peer \
         nobody here has reached or a fresh checkpoint",
        directory.display(),
        ancestors.len()
    )
    .into())
}

/// What the public API hands the rest of the node, and what it reads.
///
/// Three channels and two shared values, together because they are the whole of
/// the API's connection to the node: a route can only ever offer something to a
/// loop that can check it, and each of these is one such offer.
struct ApiWiring {
    executor: Option<SharedExecutor>,
    mempool: Arc<Mutex<nano_mempool::Mempool>>,
    /// The blocks this node kept because it executed them, which is what
    /// `/v3/blocks/:id` and `/v3/tenures/:id` answer from.
    archive: Option<Arc<crate::archive::Archive>>,
    /// Where a block upload admitted over the public API is handed to the executor,
    /// drained by the follow loop into the same staging store the peer's blocks
    /// land in — so an upload and a followed block are the same thing from the
    /// moment they are authenticated.
    blocks: nano_queue::Sender<NakamotoBlock>,
    proposals: nano_queue::Sender<nano_rpc::ProposalRequest>,
    chunks: nano_queue::Sender<(String, nano_stackerdb::Chunk)>,
    submitted: nano_queue::Sender<nano_codec::Transaction>,
}

/// The far ends of the channels the follow loop drains.
struct FollowedChannels {
    offered: nano_queue::Receiver<NakamotoBlock>,
    submitted: nano_queue::Receiver<nano_codec::Transaction>,
}

/// The far ends of the channels the hosting role drains.
struct HostedChannels {
    proposed: nano_queue::Receiver<nano_rpc::ProposalRequest>,
    written: nano_queue::Receiver<(String, nano_stackerdb::Chunk)>,
}

impl ApiWiring {
    /// Build the wiring, handing back the ends that belong to other roles.
    fn new(
        executor: Option<SharedExecutor>,
        mempool: Arc<Mutex<nano_mempool::Mempool>>,
        archive: Option<Arc<crate::archive::Archive>>,
    ) -> (Self, FollowedChannels, HostedChannels) {
        let (blocks, offered) = nano_queue::channel(nano_rpc::BLOCK_QUEUE_LIMITS);
        let (proposals, proposed) = nano_queue::channel(nano_rpc::PROPOSAL_QUEUE_LIMITS);
        let (chunks, written) = nano_queue::channel(nano_rpc::CHUNK_QUEUE_LIMITS);
        let (relayed, submitted) = nano_queue::channel(nano_rpc::TRANSACTION_QUEUE_LIMITS);
        (
            Self {
                executor,
                mempool,
                archive,
                blocks,
                proposals,
                chunks,
                submitted: relayed,
            },
            FollowedChannels { offered, submitted },
            HostedChannels { proposed, written },
        )
    }
}

/// Serve the public RPC, if this node is configured to.
async fn start_rpc(
    config: &Config,
    network: Network,
    wiring: ApiWiring,
    dispatcher: &EventDispatcher,
    metrics: nano_rpc::NodeMetrics,
    roles: &mut JoinSet<(Job, Result<(), String>)>,
) -> Result<RpcState, Box<dyn Error>> {
    let ApiWiring {
        executor,
        mempool,
        archive,
        blocks,
        proposals,
        chunks,
        submitted,
    } = wiring;
    let mut state = RpcState::new(network)
        .with_metrics(metrics)
        .with_roles(nano_rpc::NodeRoles {
            follower: config.node.rpc_bind.is_some() || config.miner.is_some(),
            signer: config.signer.is_some(),
            miner: config.miner.is_some(),
        })
        .with_mempool(mempool)
        .with_block_sink(blocks)
        .with_chunk_relay(chunks)
        .with_transaction_relay(submitted);
    // Only when a validator is actually running: a channel with nobody at the far
    // end would have the route promise a verdict that never arrives.
    if config.signer.is_none() && config.node.block_proposal_token.is_some() {
        state = state.with_proposal_validator(proposals);
    }
    if let Some(archive) = archive {
        state = state.with_executed_blocks(archive as Arc<dyn nano_rpc::ExecutedBlocks>);
    }
    if let Some(executor) = executor {
        // The same mutex behind two trait objects, so an account read and a block
        // admission are serialized against each other and against execution: the
        // one thing the RPC must never do is authenticate against a chainstate
        // that a round is halfway through moving.
        state = state
            .with_block_admission(executor.clone() as Arc<Mutex<dyn nano_rpc::BlockAdmission>>)
            .with_chain(executor as Arc<Mutex<dyn ChainAccess>>);
    }
    // The RPC shares this dispatcher even without configured POST observers:
    // its local stream is what lets `/events` expose execution receipts.
    state = state.with_observers(dispatcher.clone());
    if let Some(token) = config.node.block_proposal_token.clone() {
        state = state.with_proposal_token(token);
    }
    if let Some(address) = config.node.rpc_bind {
        state = state.with_pox_config(config.pox_rpc_config(network)?);
        let listener = TcpListener::bind(address).await?;
        println!("serving the public RPC on {address}");
        let served = state.clone();
        roles.spawn(async move {
            (
                Job::Rpc,
                serve(listener, served)
                    .await
                    .map_err(|error| error.to_string()),
            )
        });
    }
    let metrics_address = config.node.metrics_bind();
    match TcpListener::bind(metrics_address).await {
        Ok(listener) => {
            println!("serving Prometheus metrics on {metrics_address}");
            let metrics = state.metrics();
            roles.spawn(async move {
                (
                    Job::Metrics,
                    nano_rpc::serve_metrics(listener, metrics)
                        .await
                        .map_err(|error| error.to_string()),
                )
            });
        }
        Err(error) => {
            eprintln!(
                "cannot bind Prometheus metrics on {metrics_address}: {error}; continuing without the metrics port"
            );
        }
    }
    Ok(state)
}

/// Mine the tenures this node wins, if it is configured to mine at all.
fn start_miner(
    config: &Config,
    network: Network,
    pox: &PoxInfo,
    peer: &SyncClient,
    chain: (
        Option<SharedExecutor>,
        Arc<Mutex<nano_mempool::Mempool>>,
        Arc<AtomicBool>,
    ),
    announce: (
        EventDispatcher,
        nano_p2p::Relay,
        nano_rpc::NodeMetrics,
        RpcState,
    ),
    roles: &mut JoinSet<(Job, Role)>,
) {
    let (executor, mempool, owns_execution) = chain;
    let (dispatcher, relay, metrics, rpc) = announce;
    let (Some(miner), Some(executor)) = (config.miner.clone(), executor) else {
        return;
    };
    let runtime = miner::Runtime {
        config: config.clone(),
        miner,
        network,
        pox: pox.clone(),
        peer: peer.clone(),
        executor,
        dispatcher,
        mempool,
        relay,
        metrics,
        rpc,
    };
    let lease = MinerExecutionLease::claim(owns_execution);
    roles.spawn(async move {
        let result = miner::run(runtime).await;
        drop(lease);
        (Job::Miner, result)
    });
}

/// Validate proposals for the active reward cycle, if this node signs.
///
/// The signer's chain state is opened here rather than in the task, so a state it
/// cannot open stops the node at startup instead of a second later.
async fn start_signer(
    config: &Config,
    network: Network,
    pox: &PoxInfo,
    discovered: Option<&Discovered>,
    roles: &mut JoinSet<(Job, Role)>,
) -> Result<(), Box<dyn Error>> {
    let Some(signer) = config.signer.clone() else {
        return Ok(());
    };
    let mut pool = TenureSource::new(follow_pool(config, discovered).into_clients());
    let validator = signer::open(
        config,
        network,
        pox,
        &mut pool,
        &config.chainstate_dir(SIGNER_CHAINSTATE),
    )
    .await?;
    // The same pool the resume walked, kept rather than dropped: a signer handed one
    // client out of it depends on that client for the life of the node.
    let (running, found, cycles) = (config.clone(), discovered.cloned(), pox.clone());
    roles.spawn(async move {
        (
            Job::Signer,
            Box::pin(signer::run(
                running, signer, network, cycles, found, pool, validator,
            ))
            .await,
        )
    });
    Ok(())
}

/// Run the two halves of hosting somebody else's signer, if this node serves an
/// RPC for one to use.
///
/// The proposal validator keeps a chain state that may hold a candidate, and
/// nano's embedded signer keeps the same one — so a node does one or the other.
/// A configuration asking for both would open the same store twice, and the
/// honest reading of it is that the operator meant the signer they configured.
async fn start_hosting(
    config: &Config,
    network: Network,
    pox: &PoxInfo,
    discovered: Option<&Discovered>,
    state: Option<&RpcState>,
    hosted: HostedChannels,
    roles: &mut JoinSet<(Job, Role)>,
) -> Result<(), Box<dyn Error>> {
    let HostedChannels { proposed, written } = hosted;
    let Some(state) = state.cloned() else {
        return Ok(());
    };
    let (running, replicating) = (config.clone(), config.clone());
    let replicating_state = state.clone();
    let endpoints = follow_endpoints(config, discovered);
    let replicas = crate::hosting::Replicas::from_endpoints(&endpoints);
    let (validating_found, replicating_found) = (discovered.cloned(), discovered.cloned());
    roles.spawn(async move {
        (
            Job::Replication,
            crate::hosting::replicate(
                replicating,
                network,
                replicating_found,
                replicas,
                replicating_state,
                written,
            )
            .await,
        )
    });
    // A validator is a second chain state, so it is opened only for a node that can
    // actually be asked: `/v3/block_proposal` refuses every request without the
    // token, and there is nothing for a validator to answer.
    if config.signer.is_some() || config.node.block_proposal_token.is_none() {
        return Ok(());
    }
    // The pool, not one member of it: everything this role asks a peer for -- the
    // tip it catches up to, the sortition a proposal names, the coinbase its tenure
    // accumulated -- is content-addressed or checked against this node's own burn
    // view, so spreading it costs nothing and pinning it costs the liveness of every
    // signer this node hosts.
    let mut pool = TenureSource::new(follow_pool(config, discovered).into_clients());
    let validator = signer::open(
        config,
        network,
        pox,
        &mut pool,
        &config.chainstate_dir(SIGNER_CHAINSTATE),
    )
    .await?;
    let cycles = pox.clone();
    roles.spawn(async move {
        (
            Job::Proposals,
            crate::hosting::validate_proposals(
                running,
                cycles,
                validating_found,
                pool,
                validator,
                state,
                proposed,
            )
            .await,
        )
    });
    Ok(())
}

/// Send locally derived Bitcoin blocks and executed Stacks blocks to observers.
///
/// The executor owns both boundaries: the local sortition tracker derives Bitcoin
/// blocks, and the chainstate executes Stacks blocks.
async fn announce_node_events(executor: Option<&SharedExecutor>, dispatcher: &EventDispatcher) {
    if let Some(executor) = executor {
        executor.lock().await.announce_to(dispatcher.clone());
    }
}

/// Whether a round failed because the MARF already holds a version it was asked to
/// write.
///
/// The name is the whole test: `VersionAlreadyExists` has one `Display`, and matching
/// on the text is what keeps this out of the execution path's error type.
fn round_hit_marf_residue(error: &str) -> bool {
    error.contains("MARF version already exists")
}

/// Give back sealed states above this node's own tip, mid-run.
///
/// Startup already does this, and that was not enough. A round can leave the MARF
/// ahead of the ledger *while the node is up* -- a block sealed and then not
/// committed, because the peer went away between the two writes -- and the MARF
/// refuses to begin a version it already holds, so every later round fails on the
/// same block. A live mainnet node did exactly that: **691 identical failures in one
/// run**, at height 8,713,221, ending only when somebody restarted it and startup
/// swept up.
///
/// Only on the error that names it, and only above the tip: states at or below the
/// tip are what the ledger points at, and re-executing what is above is cheap
/// because the blocks are fetched again anyway
/// ([[056-make-rejected-block-execution-leave-no-state]]).
fn give_back_states_above_the_tip(executor: &mut CheckpointExecutor<BurnchainSource>, error: &str) {
    if !round_hit_marf_residue(error) {
        return;
    }
    let Ok(height) = u32::try_from(executor.tip().header.chain_length) else {
        return;
    };
    match executor.discard_above(height) {
        Ok(0) => eprintln!(
            "the MARF refused a version it already holds, but there is nothing above \
             {height} to give back: this is not the residue of an abandoned block"
        ),
        Ok(given_back) => println!(
            "gave back {given_back} sealed states above {height} that no ledger names, \
             so the next round can execute that block instead of failing on it"
        ),
        Err(error) => eprintln!("cannot give back the states above {height}: {error}"),
    }
}

/// The block a "no burnchain block height" failure names, if that is the failure.
fn block_without_a_header(error: &str) -> Option<[u8; 32]> {
    let marker = "no burnchain block height found for Stacks block ";
    let rest = error.split_once(marker)?.1;
    let hex: String = rest.chars().take_while(char::is_ascii_hexdigit).collect();
    <[u8; 32]>::try_from(hex::decode(hex).ok()?.as_slice()).ok()
}

/// Fetch the header of an ancestor older than the checkpoint, so the retry can pass.
///
/// A checkpointed node holds the block index for all of history but state only
/// from the anchor's parent forward, so a contract asking about an older block
/// stops it — and stops it for good, because the node retries the same block
/// forever. The five fields a stock peer can answer exactly are enough to get
/// past the epoch check that guards every `get-stacks-block-info?`.
///
/// Nothing is retried here: the block is written down and the ordinary next
/// round picks it up, so a peer that cannot answer costs one request.
async fn backfill_missing_header(
    executor: &mut CheckpointExecutor<BurnchainSource>,
    peer: &SyncClient,
    error: &str,
) {
    // Two ways a header can be wanted and absent. The epoch lookup fails loudly
    // and names the block in its message; an ordinary `get-stacks-block-info?`
    // just answers `none`, the contract takes its error path, and the only
    // symptom is a state root that does not match. So take both: what the error
    // named, and what the VM recorded asking for.
    let mut wanted = executor.chainstate.take_missing_headers();
    if let Some(named) = block_without_a_header(error) {
        wanted.push(named);
    }
    for block in wanted {
        backfill_one_header(executor, peer, block).await;
    }
}

async fn backfill_one_header(
    executor: &mut CheckpointExecutor<BurnchainSource>,
    peer: &SyncClient,
    block: [u8; 32],
) {
    if executor.chainstate.knows_block_header(&block) {
        return;
    }
    let id = nano_primitives::StacksBlockId::from_bytes(block);
    let Ok(header) = peer.block(id).await.map(|block| block.header) else {
        eprintln!("cannot fetch the header of {}", hex::encode(block));
        return;
    };
    let Ok((burn_height, burn_hash)) = executor.local_ancestor_burn_context(header.consensus_hash)
    else {
        eprintln!(
            "cannot place ancestor {} on this node's local burnchain",
            hex::encode(block)
        );
        return;
    };
    let Ok(burn_block_height) = u32::try_from(burn_height) else {
        return;
    };
    if let Err(error) = executor.chainstate.backfill_ancestor_header(
        block,
        burn_hash,
        burn_block_height,
        header.timestamp,
        *header.block_hash().as_bytes(),
        *header.consensus_hash.as_bytes(),
    ) {
        // Said rather than swallowed: the next round tries the same block and
        // stops on the same missing header, so an operator has to be able to see
        // why one never arrives.
        eprintln!(
            "writing down the header of ancestor {} failed: {error}",
            hex::encode(block)
        );
        return;
    }
    println!(
        "wrote down the header of ancestor {} at burn height {burn_block_height}, \
         which this node never executed",
        hex::encode(block)
    );
}

/// Check the checkpoint against a signed header before any of it is opened.
///
/// A checkpoint stating its own root is not evidence of anything. A Nakamoto
/// header at that height carries the same root and a reward set put threshold
/// weight behind it, so that is what makes one trustworthy — and the reward set
/// has to come from somewhere other than the checkpoint.
///
/// A state directory that already carries provenance was adopted once and is
/// not re-adopted; it is checked to be the same checkpoint, so a directory
/// cannot quietly become descended from a different one.
fn adopt(config: &Config, directory: &Path, source: [u8; 32]) -> Result<(), Box<dyn Error>> {
    let manifest = CheckpointManifest::load(
        config
            .checkpoint
            .marf
            .parent()
            .ok_or("the checkpoint has no directory")?,
    )?;
    if config.network().is_some_and(Network::is_mainnet) {
        manifest.check_profile(nano_vm::compatibility_profile_fingerprint())?;
    }
    if manifest.source_state_id != source {
        return Err(format!(
            "the checkpoint names state {} where this node is configured for {}",
            hex::encode(manifest.source_state_id),
            hex::encode(source)
        )
        .into());
    }
    if let Some(recorded) = CheckpointProvenance::load(directory)? {
        already_adopted(
            recorded.checkpoint.source_state_id,
            manifest.source_state_id,
        )?;
        return Ok(());
    }

    let (Some(block), Some(reward_set)) = (
        config.checkpoint.attesting_block.as_ref(),
        config.checkpoint.attesting_reward_set.as_ref(),
    ) else {
        return Err(
            "a checkpoint needs an attesting block and the reward set that \
                    signed it before it can be imported"
                .into(),
        );
    };
    let block = NakamotoBlock::decode(&fs::read(block)?)?;
    let signers = attesting_reward_set(&fs::read(reward_set)?)?;
    let attestation = crate::adopt_checkpoint(directory, &manifest, &block.header, &signers)?;
    println!(
        "checkpoint {} attested by {} of {} signer weight",
        hex::encode(manifest.source_state_id),
        attestation.signer_weight,
        attestation.approval_threshold
    );
    Ok(())
}

/// Whether a state directory may carry on under this checkpoint.
///
/// A directory descended from one checkpoint cannot be reused for another: its
/// trie stands on the first one's state, and nothing later would notice.
fn already_adopted(recorded: [u8; 32], configured: [u8; 32]) -> Result<(), String> {
    if recorded == configured {
        Ok(())
    } else {
        Err(format!(
            "this state descends from checkpoint {} and cannot be reused for {}",
            hex::encode(recorded),
            hex::encode(configured)
        ))
    }
}

/// The reward set a `/v3/stacker_set/:cycle` document names.
fn attesting_reward_set(bytes: &[u8]) -> Result<SignerSet, Box<dyn Error>> {
    let document: serde_json::Value = serde_json::from_slice(bytes)?;
    let entries = document["stacker_set"]["signers"]
        .as_array()
        .ok_or("the reward set names no signers")?;
    let signers = entries
        .iter()
        .map(|entry| {
            let key = entry["signing_key"]
                .as_str()
                .ok_or("a signer has no signing key")?;
            Ok(Signer {
                public_key: StacksPublicKey::from_bytes(&hex::decode(
                    key.trim_start_matches("0x"),
                )?)
                .map_err(|error| format!("a signing key is not a public key: {error:?}"))?,
                weight: u32::try_from(entry["weight"].as_u64().ok_or("a signer has no weight")?)?,
            })
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    Ok(SignerSet::new(signers)?)
}

/// The rewards a role still owes: what it last wrote, or what the checkpoint
/// said before it had written anything.
fn accounting(config: &Config, directory: &Path) -> Result<TenureAccounting, Box<dyn Error>> {
    let persisted = directory.join(ACCOUNTING_FILE);
    if persisted.exists() {
        let accounting = TenureAccounting::from_json(&fs::read(&persisted)?)?;
        // The same check the checkpoint gets: a state whose accounting was
        // written before the checkpoint carried a full window owes less than
        // the chain does, and only finds out at the first payout it cannot
        // derive.
        // No exemption here: a state that has executed anything and owes
        // nothing is not a genesis start, it is accounting written before the
        // checkpoint carried a window.
        if accounting.known_earnings_span().is_none() {
            return Err(format!(
                "the accounting at {} carries no tenure earnings, so it was written before \
                 the checkpoint carried a maturity window; remove it to re-seed from the \
                 checkpoint",
                persisted.display()
            )
            .into());
        }
        check_maturity_window(&accounting)?;
        return Ok(accounting);
    }
    match &config.checkpoint.tenure_accounting {
        Some(path) => {
            let accounting = TenureAccounting::from_json(&fs::read(path)?)?;
            check_maturity_window(&accounting)?;
            Ok(accounting)
        }
        None => Ok(TenureAccounting::default()),
    }
}

/// Refuse a checkpoint that does not owe what the chain owes.
///
/// Every tenure a node executes before its own mature pays out one from the
/// hundred before the checkpoint, which it can only read and never derive. A
/// checkpoint short of them runs perfectly until the first payout it cannot
/// make and then stops with `UnknownTenure` — hours in, having written state
/// that has to be thrown away. Saying so at startup costs one comparison.
fn check_maturity_window(accounting: &TenureAccounting) -> Result<(), Box<dyn Error>> {
    let Some((first, last)) = accounting.known_earnings_span() else {
        // Nothing seeded at all is a genesis start, which owes nothing yet.
        return Ok(());
    };
    // A chain younger than the maturity horizon owes nothing before its own first
    // tenure: the earliest payout any block can ask for is tenure 1, because a
    // tenure below the horizon matures nothing at all. So earnings that reach back
    // to the chain's beginning are complete however few of them there are, and
    // demanding a hundred of them would make nano unable to start from any chain
    // less than a hundred tenures old — which is every fresh test network.
    if first > 1 && last - first < MINER_REWARD_MATURITY {
        return Err(format!(
            "the checkpoint carries earnings for tenures {first} to {last}, which is {} of the \
             {} a node needs: every tenure it executes before its own mature pays out one of \
             them",
            last - first + 1,
            MINER_REWARD_MATURITY + 1
        )
        .into());
    }
    Ok(())
}

/// The execution context this network fixes, before a height is chosen.
#[must_use]
pub fn bitcoin_context(config: &Config, pox: &PoxInfo) -> BitcoinBlockContext {
    let mut context = pox.bitcoin_context();
    if let Some(height) = config.burnchain.pox_5_activation_height {
        context.pox_5_activation_height = height;
    }
    context
}

/// Connect to the burnchain the configuration names.
///
/// Either kind of source answers the one question a follower asks — the block
/// at a height — so which one is configured decides nothing but where the
/// bytes come from.
pub fn bitcoin_source(config: &Config) -> Result<BurnchainSource, Box<dyn Error>> {
    if let Some(rest) = config.burnchain.rest_url.as_ref() {
        return Ok(BurnchainSource::Rest(Box::new(BitcoinRestSource::new(
            rest,
            config.burnchain.magic()?,
        )?)));
    }
    Ok(BurnchainSource::Rpc(Box::new(BitcoinRpcSource::new(
        &config.burnchain.rpc_url,
        config.burnchain.rpc_user.clone(),
        config.burnchain.rpc_password.clone(),
        config.burnchain.magic()?,
    )?)))
}

/// The burnchain this node reads, however it reaches it.
#[derive(Debug)]
pub enum BurnchainSource {
    Rpc(Box<BitcoinRpcSource>),
    Rest(Box<BitcoinRestSource>),
}

impl nano_bitcoin::BitcoinSource for BurnchainSource {
    type Error = nano_bitcoin::BitcoinRpcSourceError;

    fn block_at(&mut self, height: u64) -> Result<nano_bitcoin::BitcoinBlock, Self::Error> {
        match self {
            Self::Rpc(source) => source.block_at(height),
            Self::Rest(source) => source.block_at(height),
        }
    }

    fn block_hash_at(&self, height: u64) -> Result<[u8; 32], Self::Error> {
        match self {
            Self::Rpc(source) => source.block_hash_at(height),
            Self::Rest(source) => source.block_hash_at(height),
        }
    }

    fn tip_height(&self) -> Result<u64, Self::Error> {
        match self {
            Self::Rpc(source) => source.tip_height(),
            Self::Rest(source) => source.tip_height(),
        }
    }

    fn invalidate_from(&mut self, height: u64) {
        match self {
            Self::Rpc(source) => source.invalidate_from(height),
            Self::Rest(source) => source.invalidate_from(height),
        }
    }
}

/// The first configured peer that answers, so one dead peer is not a dead node.
/// Run a startup step, waiting out a peer that is rate limiting this node.
///
/// A round of following can give up on a 429 and ask again next poll. Startup
/// has no next poll: giving up there ends the process, so a node that a public
/// endpoint merely asked to slow down never comes up at all.
async fn patiently<T, F, S>(mut step: F) -> Result<T, SyncError>
where
    F: FnMut() -> S,
    S: Future<Output = Result<T, SyncError>>,
{
    let mut wait = Duration::from_secs(1);
    loop {
        match step().await {
            Err(error) if error.is_rate_limited() && wait < STARTUP_PATIENCE => {
                eprintln!("the peer is rate limiting this node, waiting {wait:?}");
                sleep(wait).await;
                wait = wait.saturating_mul(2);
            }
            outcome => return outcome,
        }
    }
}

async fn reachable_peer(
    config: &Config,
    discovered: Option<&Discovered>,
) -> Result<SyncClient, Box<dyn Error>> {
    let mut last = None;
    // Configured peers first, because an operator naming one is expressing a
    // preference; then whatever the p2p network turned out to hold, which is what
    // makes an empty `node.peers` a workable configuration rather than a fatal one.
    let configured = config.node.peers()?;
    let found = discovered.map(Discovered::endpoints).unwrap_or_default();
    let candidates = configured
        .into_iter()
        .chain(found.iter().filter_map(|endpoint| endpoint.parse().ok()));
    for url in candidates {
        let Ok(client) = SyncClient::new(url.clone()) else {
            continue;
        };
        match patiently(|| client.node_info()).await {
            Ok(_) => return Ok(client),
            Err(error) => {
                eprintln!("peer {url} is not answering: {error}");
                last = Some(error);
            }
        }
    }
    Err(last.map_or_else(
        || Box::<dyn Error>::from("no peer to follow"),
        |error| Box::new(error) as Box<dyn Error>,
    ))
}

/// Wait for a peer that answers, because none answering *yet* is not a reason to
/// stop.
///
/// Discovery is still running while this waits: the swarm keeps handshaking and
/// learning addresses, so the set `reachable_peer` is offered grows between
/// attempts. Measured on a live mainnet follower, which is the only place this
/// shows: one start was handed seven peers and ran, and the next was handed four
/// that all refused HTTP within the same minute -- and the node exited on a
/// condition that had cleared by the time anybody read the log.
///
/// Bounded by `startup_peer_wait_secs` rather than forever, so a genuinely
/// unroutable configuration still fails instead of hanging. Each round says what it
/// is waiting for, because a silent wait and a hang look identical.
async fn awaited_peer(
    config: &Config,
    discovered: Option<&Discovered>,
) -> Result<SyncClient, Box<dyn Error>> {
    let deadline = Duration::from_secs(config.node.startup_peer_wait_secs);
    let began = std::time::Instant::now();
    let mut attempt = 0u32;
    loop {
        // The failure becomes its own text before the wait: a `Box<dyn Error>` is not
        // `Send`, and holding one across an await would make the whole startup future
        // unspawnable.
        let refused = match reachable_peer(config, discovered).await {
            Ok(client) => return Ok(client),
            Err(error) if began.elapsed() >= deadline => return Err(error),
            Err(error) => error.to_string(),
        };
        attempt += 1;
        let known = discovered.map_or(0, |found| found.endpoints().len());
        eprintln!(
            "no peer answered on attempt {attempt} ({refused}); {known} discovered so far, waiting \
             for one within {}s of startup",
            deadline.as_secs()
        );
        sleep(Duration::from_secs(config.node.poll_interval_secs.max(1))).await;
    }
}

/// Say which peers this round stands on, so `/nano/sync_status` can name the
/// pool instead of only the one peer fork choice picked from it.
async fn publish_peer_report(
    state: Option<&RpcState>,
    history: &BulkHistory,
    discovered: Option<&Discovered>,
) {
    let Some(state) = state else { return };
    state
        .publish_peers(nano_rpc::PeerReport {
            fetching: history.named.clone(),
            p2p_connected: discovered.map_or(0, Discovered::connected),
            p2p_known: discovered.map_or(0, Discovered::known),
        })
        .await;
}

/// Publish the queues the follow loop and p2p relay own, then wait for the next round.
async fn finish_round(
    state: Option<&RpcState>,
    staging: &Staging,
    relay: &nano_p2p::Relay,
    offered: &nano_queue::Receiver<NakamotoBlock>,
    submitted: &nano_queue::Receiver<nano_codec::Transaction>,
    interval: Duration,
) {
    if let Some(state) = state {
        let staged_blocks = staging.len().ok();
        let relay = relay.status();
        let blocks = offered.status();
        let transactions = submitted.status();
        state
            .metrics()
            .publish_ingress_queue(nano_rpc::IngressQueue::BlockUploads, blocks.into());
        state
            .metrics()
            .publish_ingress_queue(nano_rpc::IngressQueue::Transactions, transactions.into());
        if let Some(relay) = relay {
            state.metrics().publish_ingress_queue(
                nano_rpc::IngressQueue::RelayOffered,
                nano_rpc::IngressQueueStatus {
                    items: relay.offered,
                    bytes: relay.offered_bytes,
                    item_limit: relay.offered_item_limit,
                    byte_limit: relay.offered_byte_limit,
                    oldest_age: relay.offered_oldest_age,
                    dropped: relay.offered_dropped,
                    saturations: relay.offered_saturations,
                },
            );
            state.metrics().publish_ingress_queue(
                nano_rpc::IngressQueue::RelayAnnouncing,
                nano_rpc::IngressQueueStatus {
                    items: relay.announcing,
                    bytes: relay.announcing_bytes,
                    item_limit: relay.announcing_item_limit,
                    byte_limit: relay.announcing_byte_limit,
                    oldest_age: relay.announcing_oldest_age,
                    dropped: relay.announcing_dropped,
                    saturations: relay.announcing_saturations,
                },
            );
        }
        state
            .publish_queues(nano_rpc::QueueReport {
                staged_blocks,
                relay_offered: relay.map(|status| status.offered),
                relay_announcing: relay.map(|status| status.announcing),
                relay_dropped: relay.map(|status| status.dropped),
                queued_blocks: Some(blocks.items),
                queued_proposals: None,
                queued_stackerdb_chunks: None,
                queued_transactions: Some(transactions.items),
            })
            .await;
    }
    sleep(interval).await;
}

/// Every peer this node could follow: the ones configured, and the ones p2p found.
///
/// Configured first, so an operator naming a peer still gets it weighed; discovered
/// ones after, de-duplicated, because the same node can be both. A pool of one is
/// still a pool — it just cannot protect against that one.
fn follow_pool(config: &Config, discovered: Option<&Discovered>) -> PeerPool {
    PeerPool::from_endpoints(&follow_endpoints(config, discovered))
}

pub(crate) fn follow_endpoints(config: &Config, discovered: Option<&Discovered>) -> Vec<String> {
    let mut endpoints = config.node.peers.clone();
    for endpoint in discovered.map(Discovered::endpoints).unwrap_or_default() {
        if !endpoints.contains(&endpoint) {
            endpoints.push(endpoint);
        }
    }
    endpoints
}

/// Where this round fetches bulk history from.
///
/// The work list, rather than one client: `catch_up`'s descent asks for a tenure at
/// a time, and on mainnet from the checkpoint that is tens of thousands of blocks.
/// Sent down one connection to a hosted API, the rate limit *is* the catch-up speed
/// — which is the thing joining the peer network was for, and which stayed true for
/// two slices after the transport landed because nothing changed who the descent
/// asked.
///
/// Rebuilt when the endpoint list changes rather than every round, so a peer set that
/// has not moved keeps the position it had walked to and the throttles it had learned.
struct BulkHistory {
    source: TenureSource,
    endpoints: Vec<String>,
    /// The pool as it was actually built — parsed, de-duplicated — which is
    /// what `/nano/sync_status` names. `endpoints` keeps the raw strings so the
    /// rebuild guard still compares what discovery handed over.
    named: Vec<String>,
    claiming: Vec<String>,
}

impl BulkHistory {
    fn new(peer: SyncClient) -> Self {
        Self {
            source: TenureSource::only(peer),
            endpoints: Vec::new(),
            named: Vec::new(),
            claiming: Vec::new(),
        }
    }

    /// Take account of what discovery has learned since the last round.
    ///
    /// Both halves are guarded on the thing having *changed*, because rebuilding the
    /// source or reordering it discards the position the round-robin had walked to and
    /// the throttles it had learned — so doing it every round would leave the descent
    /// asking the same first peer forever.
    fn refresh(&mut self, config: &Config, discovered: Option<&Discovered>) {
        let endpoints = follow_endpoints(config, discovered);
        let rebuilt = PeerPool::from_endpoints(&endpoints);
        if endpoints != self.endpoints && !rebuilt.is_empty() {
            println!(
                "fetching history from {} peers: {}",
                rebuilt.len(),
                rebuilt.endpoints().join(", ")
            );
            self.named = rebuilt.endpoints();
            self.source = TenureSource::new(rebuilt.into_clients());
            self.endpoints = endpoints;
            // A fresh source has no order to preserve, so the shortlist applies again.
            self.claiming.clear();
        }
        // Ask the peers the inventory says hold this cycle first. A peer that claims
        // none of it has nothing to serve, and finding that out by asking it for a
        // tenure is the round trip an inventory exists to avoid.
        let claiming = discovered.map(Discovered::claiming).unwrap_or_default();
        if !claiming.is_empty() && claiming != self.claiming {
            self.source.prefer(&claiming);
            self.claiming = claiming;
        }
    }
}

/// Re-weigh the peers and hand back a better one, if there is one.
///
/// The weighing is `nano_sync::choose_canonical_tip`, which is the boundary that
/// has to stay in one place: a tip is compared against this node's *own* answers —
/// the burn view it derived from its own burnchain and the signer set its own
/// executed state records — and then on the length of headers this node fetched.
/// Nothing here is a peer's claim about its own height, its own cycle or its own
/// burnchain.
///
/// The two phases are separated on purpose. Gathering the candidates is network
/// work and takes no lock; weighing them locks the executed state and awaits
/// nothing, because holding that lock across an HTTP round trip would stall every
/// account read and every block admission behind the slowest peer in the pool.
async fn better_peer(
    current: &SyncClient,
    config: &Config,
    discovered: Option<&Discovered>,
    executor: Option<&SharedExecutor>,
    pox: &PoxInfo,
    state: Option<&RpcState>,
) -> Option<SyncClient> {
    let pool = follow_pool(config, discovered);
    let candidates = pool.candidate_tips().await;
    let selected = match executor {
        Some(executor) => {
            let mut held = executor.lock().await;
            let signers = held.recorded_signer_set(bitcoin_context(config, pox));
            let burn = held.burn_view();
            let chosen = nano_sync::choose_canonical_tip(&candidates, signers.as_ref(), burn)
                .map(|tip| (tip.peer, tip.header.block_id(), tip.header.chain_length));
            // Released before the answer is acted on: every account read and every
            // block admission takes this same lock.
            drop(held);
            chosen?
        }
        // A node with no executed state of its own — a signer-only or RPC-only
        // configuration — has neither answer to weigh with, and says so by
        // passing neither rather than by substituting a peer's.
        None => nano_sync::choose_canonical_tip(&candidates, None, None)
            .map(|tip| (tip.peer, tip.header.block_id(), tip.header.chain_length))?,
    };
    let (peer, stacks_tip, stacks_height) = selected;
    let chosen = pool.peer(peer)?.clone();
    // What the choice chose, published from the choice: it is remade on a timer
    // whether or not the answer moves, and it is the one of the three heights
    // that is nobody else's — a peer advertises, this node selects, the executor
    // executes.
    if let Some(state) = state {
        state
            .publish_selected(nano_rpc::SelectedTip {
                stacks_height,
                stacks_tip,
                peer: chosen.base_url().to_string(),
            })
            .await;
    }
    (chosen.base_url() != current.base_url()).then(|| {
        println!(
            "following {} now, of {} peers known",
            chosen.base_url(),
            pool.len()
        );
        chosen
    })
}

/// Learn how far ahead the peer is, and refresh what the RPC serves.
///
/// Two independent jobs on one peer, and they are separated because gating
/// execution on a successful poll is how a node twenty thousand blocks behind
/// executed nothing at all: that far back the follower's own tenure walk fails
/// every round, and it took the executor down with it. Returns whether the peer let
/// this round down, which is what makes the next one choose again.
async fn track_peer(
    node: &mut Node,
    peer: &SyncClient,
    state: Option<&RpcState>,
    pox: &mut PoxInfo,
    peer_height: &mut u64,
    catching_up: bool,
) -> bool {
    if catching_up {
        match peer.node_info().await {
            Ok(info) => {
                *peer_height = info.stacks_height;
                // Said out loud, because this is the branch a node that cannot catch
                // up sits in: without it `/nano/sync_status` reported no distance at
                // all for the one node that has one.
                if let Some(state) = state {
                    state.publish_followed_height(*peer_height).await;
                }
                false
            }
            Err(error) => {
                eprintln!("asking the peer how far ahead it is failed: {error}");
                if let Some(state) = state {
                    state.metrics().record_sync_round_unanswered();
                }
                true
            }
        }
    } else {
        match node.poll().await {
            Ok(_) => {
                if let Some(view) = node.view() {
                    *peer_height = view.node_info.stacks_height;
                    *pox = view.pox_info.clone();
                    if let Some(state) = state {
                        state.publish(view).await;
                    }
                }
                false
            }
            Err(error) => {
                eprintln!("following the peer failed: {error}");
                if let Some(state) = state {
                    state.metrics().record_sync_round_unanswered();
                }
                true
            }
        }
    }
}

/// The Bitcoin view this node advertises to its peers.
///
/// Derived from the node's own Bitcoin source, never from what a peer said: a
/// preamble view is a gossip hint rather than a consensus input, but a node repeating
/// a peer's claim back at the network would be laundering it into one.
///
/// The fallback matters as much as the derivation. A peer refuses a message whose
/// *stable* header hash contradicts its own at that height, and it keeps roughly 288
/// blocks below its own stable height — so a view older than that cannot be
/// contradicted, and stacks-core reads not-contradictable as merely stale. A node
/// whose Bitcoin source cannot answer yet advertises exactly that and gets in; the
/// next discovery round retries the local source.
fn advertised_view<S: nano_bitcoin::BitcoinSource>(
    bitcoin: &S,
    stable_confirmations: u64,
) -> nano_p2p::ChainView {
    let Ok(height) = bitcoin.tip_height() else {
        return stale_peer_view(stable_confirmations);
    };
    let Some(settled) = height.checked_sub(stable_confirmations) else {
        return stale_peer_view(stable_confirmations);
    };
    let (Ok(tip_hash), Ok(stable_hash)) = (
        nano_bitcoin::BitcoinSource::block_hash_at(bitcoin, height),
        nano_bitcoin::BitcoinSource::block_hash_at(bitcoin, settled),
    ) else {
        return stale_peer_view(stable_confirmations);
    };
    nano_p2p::ChainView::with_stable_confirmations(
        height,
        nano_primitives::BitcoinHeaderHash::from_bytes(tip_hash),
        nano_primitives::BitcoinHeaderHash::from_bytes(stable_hash),
        stable_confirmations,
    )
    .unwrap_or_else(|| stale_peer_view(stable_confirmations))
}

fn stale_peer_view(stable_confirmations: u64) -> nano_p2p::ChainView {
    nano_p2p::ChainView::with_stable_confirmations(
        100_000,
        nano_primitives::BitcoinHeaderHash::from_bytes([0; 32]),
        nano_primitives::BitcoinHeaderHash::from_bytes([0; 32]),
        stable_confirmations,
    )
    .expect("a height above the confirmation window")
}

/// What this node tells its peers about itself, written by the loop that knows it.
///
/// The transport comes up *before* there is a chainstate — that is its whole point,
/// since it is the way in that does not depend on a configured HTTP peer — so the
/// discovery loop cannot read an executor that is built after it. This is the handle
/// the follow loop writes each round and discovery reads: which reward cycle to ask
/// peers' inventories about and what inventory to serve.
///
/// Both are *this node's* answers. A cycle identifier taken from a peer would make
/// its view of the burnchain the thing nano's own requests are keyed on.
#[derive(Clone, Default)]
pub struct Advertised {
    inner: Arc<std::sync::Mutex<Option<LocalAnnouncement>>>,
    /// The locally derived Bitcoin view attached to every inbound reply.
    view: Arc<std::sync::Mutex<Option<nano_p2p::ChainView>>>,
    /// The inventory that outlives the process, when there is a working directory to
    /// keep it in.
    ///
    /// Behind the same kind of mutex as the snapshot and for the same reason: this is
    /// written by the follow loop once a round and read by an inbound reply, both of
    /// which are sub-millisecond sqlite operations. What it deliberately never takes
    /// is the *executor's* lock, because a reply that could block on execution is a
    /// reply that lets one inbound peer stall the loop that executes blocks.
    served: Option<Arc<std::sync::Mutex<nano_p2p::ServedTenures>>>,
}

/// What the follow loop knows and the peer-facing loops need.
#[derive(Clone, Debug)]
struct LocalAnnouncement {
    /// The consensus hash naming the reward cycle being walked, when derivable.
    cycle_start: Option<nano_primitives::ConsensusHash>,
    /// Every locally known cycle, including empty historical cycles a stock node
    /// must walk through before it reaches the recent tenures nano can serve.
    inventories: Vec<crate::TenureInventory>,
}

impl Advertised {
    /// Keep the inventory this node serves under `working_dir`, so a restart can
    /// still answer for the cycles it has walked.
    ///
    /// A store that cannot be opened is not fatal: the node then answers from the
    /// round's own window exactly as it did before, which is smaller and still
    /// truthful.
    fn open(working_dir: &Path) -> Self {
        let served = match nano_p2p::ServedTenures::open(&working_dir.join("served.sqlite")) {
            Ok(served) => Some(Arc::new(std::sync::Mutex::new(served))),
            Err(error) => {
                eprintln!("cannot keep the served inventory across restarts: {error}");
                None
            }
        };
        Self {
            inner: Arc::default(),
            view: Arc::default(),
            served,
        }
    }

    fn publish(&self, announcement: LocalAnnouncement) {
        // Recorded before the snapshot so that the durable answer is never behind the
        // live one: a peer reading between the two would otherwise be told about a
        // tenure whose bit had not been written down yet.
        if let Some(served) = self.served.as_ref()
            && let Ok(served) = served.lock()
        {
            for (cycle_height, cycle_start, tenures) in &announcement.inventories {
                if let Err(error) = served.record(*cycle_height, *cycle_start, tenures) {
                    eprintln!("cannot record the tenures this node serves: {error}");
                }
            }
        }
        if let Ok(mut held) = self.inner.lock() {
            *held = Some(announcement);
        }
    }

    fn read(&self) -> Option<LocalAnnouncement> {
        // A poisoned lock means a panic while publishing a height, which is not a
        // reason to stop talking to peers: the stale view below is a correct answer.
        self.inner.lock().ok().and_then(|held| held.clone())
    }

    fn publish_view(&self, view: nano_p2p::ChainView) {
        if let Ok(mut held) = self.view.lock() {
            *held = Some(view);
        }
    }

    fn chain_view(&self, stable_confirmations: u64) -> nano_p2p::ChainView {
        self.view
            .lock()
            .ok()
            .and_then(|held| *held)
            .unwrap_or_else(|| stale_peer_view(stable_confirmations))
    }

    /// Answer a peer's inventory request, or `None` for a cycle this node cannot
    /// speak to — which becomes a `Nack`, and is the honest answer.
    ///
    /// The durable store answers first, because it knows strictly more: it holds every
    /// bit the live window has ever reported for that cycle, including the ones from
    /// before the last restart. The live snapshot is the fallback for a node whose
    /// store would not open, and for the first round after a cycle rolls over.
    fn tenure_inventory(
        &self,
        cycle_start: nano_primitives::ConsensusHash,
    ) -> Option<nano_primitives::BitVec<2100>> {
        if let Some(served) = self.served.as_ref()
            && let Ok(served) = served.lock()
            && let Ok(Some(tenures)) = served.inventory(cycle_start)
        {
            return Some(tenures);
        }
        self.read()?
            .inventories
            .into_iter()
            .find_map(|(_, known, tenures)| (known == cycle_start).then_some(tenures))
    }
}

/// This node's p2p identity, which has to survive a restart.
///
/// Peers remember a node by its key hash, and a node that re-keyed every start
/// would be a new stranger to the whole network each time — including to the peer
/// tables that had it on a backoff, which is the half that would make restarting a
/// way to launder a bad reputation.
fn p2p_identity(working_dir: &Path) -> Result<nano_crypto::StacksPrivateKey, Box<dyn Error>> {
    // The file holds the *seed*, not the key: a seed is what `from_seed` derives an
    // identity from, so storing it stores the thing that regenerates the identity
    // rather than a second encoding of the same secret.
    let path = working_dir.join("p2p-seed");
    if let Ok(text) = fs::read_to_string(&path)
        && let Ok(seed) = hex::decode(text.trim())
        && !seed.is_empty()
    {
        return Ok(nano_crypto::StacksPrivateKey::from_seed(&seed));
    }
    // Drawn from the working directory and the clock: two nodes sharing a
    // configuration but not a directory must not share an identity, because a peer
    // that sees one key from two addresses treats the second as a connection cycle
    // and drops it.
    let mut seed = working_dir.as_os_str().as_encoded_bytes().to_vec();
    seed.extend_from_slice(
        &std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos())
            .to_be_bytes(),
    );
    fs::write(&path, hex::encode(&seed))?;
    Ok(nano_crypto::StacksPrivateKey::from_seed(&seed))
}

fn advertise_peer_services(local: &mut nano_p2p::LocalPeer, rpc: Option<std::net::SocketAddr>) {
    // This node accepts and forwards peer messages whenever its p2p transport is
    // running. Stock peers require both bits before they consider an HTTP endpoint
    // usable for mempool and block exchange.
    local.services |= nano_p2p::wire::services::RELAY;
    if let Some(rpc) = rpc
        && !rpc.ip().is_unspecified()
    {
        local.data_url = format!("http://{rpc}");
        local.services |= nano_p2p::wire::services::RPC;
    }
}

/// Join the binary p2p network: seed the peer table, discover peers, and answer
/// the ones that dial us.
///
/// Returns the handle the rest of the node reads endpoints from, or `None` when
/// there is no way in — no seeds, or a configuration that leaves the chain
/// identifier to be discovered, which cannot work here because on this protocol
/// the network id *is* the chain id and it is in the first field of the first
/// message.
async fn start_configured_transport(
    config: &Config,
    advertised: &Advertised,
    relay: &nano_p2p::Relay,
    metrics: nano_rpc::NodeMetrics,
    roles: &mut JoinSet<(Job, Role)>,
) -> Option<Discovered> {
    start_transport(config, config.network()?, advertised, relay, metrics, roles).await
}

async fn start_transport(
    config: &Config,
    network: Network,
    advertised: &Advertised,
    relay: &nano_p2p::Relay,
    metrics: nano_rpc::NodeMetrics,
    roles: &mut JoinSet<(Job, Role)>,
) -> Option<Discovered> {
    let seeds = config.node.bootstrap_seeds();
    if seeds.is_empty() && config.node.p2p_bind.is_none() {
        return None;
    }
    let stable_confirmations = config.burnchain.stable_confirmations;
    let protocol = nano_p2p::Protocol::for_network(network)
        .with_stable_confirmations(stable_confirmations)
        .expect("the configuration validates the stable confirmation count");
    let identity = match p2p_identity(&config.node.working_dir) {
        Ok(identity) => identity,
        Err(error) => {
            eprintln!("cannot establish a p2p identity: {error}");
            return None;
        }
    };
    let bind = config.node.p2p_bind;
    let advertise = config.node.p2p_address.or(bind);
    let mut local =
        nano_p2p::LocalPeer::quiet(identity, advertise.map_or(20444, |address| address.port()));
    if let Some(address) = advertise
        && !address.ip().is_unspecified()
    {
        local.address = nano_p2p::PeerAddress::from_ip(address.ip());
    }
    advertise_peer_services(&mut local, config.node.rpc_bind);
    let peers = match nano_p2p::PeerDb::open(&config.node.working_dir.join("peers.sqlite")) {
        Ok(peers) => peers,
        Err(error) => {
            eprintln!("cannot open the peer table: {error}");
            return None;
        }
    };
    // One service, answering both directions. The listener needs it so a stock node
    // can sync *from* nano; the swarm needs it because a peer nano dialled asks nano
    // the same questions on that same socket, and a node that only answered inbound
    // connections is invisible to every peer behind a NAT.
    let service = match nano_p2p::PeerDb::open(&config.node.working_dir.join("peers.sqlite")) {
        // A second connection rather than a shared one: sqlite's `Connection` is not
        // `Sync`, so sharing would mean a lock held across every inbound reply.
        Ok(table) => Arc::new(PeerService {
            peers: std::sync::Mutex::new(table),
            advertised: advertised.clone(),
            relay: relay.clone(),
            stable_confirmations,
        }),
        Err(error) => {
            eprintln!("cannot open the peer table for serving: {error}");
            return None;
        }
    };
    let frame_budget = nano_p2p::FrameBudget::default();
    let mut swarm = nano_p2p::Swarm::new(peers, local, protocol, nano_p2p::SwarmLimits::default())
        .with_frame_budget(frame_budget.clone())
        .serving(service.clone());
    for seed in &seeds {
        if let Err(error) = swarm.seed(seed).await {
            eprintln!("cannot record the bootstrap peer {seed}: {error}");
        }
    }
    let discovered = swarm.discovered();

    // One round before anything else runs, so that a node with no configured HTTP
    // peer has somewhere to fetch from by the time it looks.
    let bitcoin = match bitcoin_source(config) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("cannot reach the Bitcoin source to build a p2p view: {error}");
            return None;
        }
    };
    let view = advertised_view(&bitcoin, stable_confirmations);
    advertised.publish_view(view);
    let round = swarm.maintain(view, None).await;
    metrics.publish_ingress_queue(
        nano_rpc::IngressQueue::PeerPushes,
        swarm.pushed_status().into(),
    );
    publish_frame_budget(&metrics, swarm.frame_budget_status());
    println!(
        "p2p: {} peers connected, {} known, {} endpoints to fetch from",
        round.connected,
        discovered.known(),
        discovered.endpoints().len()
    );

    // The tick is the node's own poll interval and discovery happens every tenth of
    // them. Two things want the shorter clock: relay, which is pointless if it is
    // minutes late, and reading each peer's socket, which is what keeps a busy peer's
    // announcements out of the receive buffer.
    let tick = Duration::from_secs(config.node.poll_interval_secs.max(1));
    let advertised = advertised.clone();
    let relay = relay.clone();
    let discovery_metrics = metrics.clone();
    roles.spawn(async move {
        (
            Job::Peers,
            peer_discovery(
                swarm,
                bitcoin,
                advertised,
                relay,
                discovery_metrics,
                tick,
                stable_confirmations,
            )
            .await,
        )
    });

    if let Some(bind) = bind {
        start_listener(config, network, bind, service, frame_budget, metrics, roles);
    }
    Some(discovered)
}

/// How many ticks between neighbour walks and inventory exchanges.
///
/// A ping and a dial round per tick would be two requests per peer per second for an
/// answer that moves on the order of a tenure; relay, which shares this loop, has to
/// be far more prompt than that.
const DISCOVERY_TICKS: u64 = 10;

/// Keep the peer set at strength, and carry what this node accepted back out to it.
///
/// One loop for both because both need `&mut Swarm`, and a swarm holds a
/// `rusqlite::Connection` that is `Send` but not `Sync` — two tasks sharing it is not
/// a thing the borrow checker will allow, and a mutex around it would make a relay
/// push wait behind a neighbour walk.
async fn peer_discovery(
    mut swarm: nano_p2p::Swarm,
    bitcoin: BurnchainSource,
    advertised: Advertised,
    relay: nano_p2p::Relay,
    metrics: nano_rpc::NodeMetrics,
    tick: Duration,
    stable_confirmations: u64,
) -> Role {
    let mut ticks = 0_u64;
    loop {
        sleep(tick).await;
        ticks = ticks.wrapping_add(1);
        let published = advertised.read();
        let view = advertised_view(&bitcoin, stable_confirmations);
        advertised.publish_view(view);
        // `None` before there is a chain to name a cycle, and a peer is then not
        // asked at all rather than asked about a guess.
        let cycle_start = published.and_then(|announced| announced.cycle_start);
        let mut round = if ticks.is_multiple_of(DISCOVERY_TICKS) {
            swarm.maintain(view, cycle_start).await
        } else {
            nano_p2p::Round::default()
        };
        // What the follow loop authenticated, on its way to every peer that did not
        // send it. This is the whole of relay's outbound half: the checks ran where
        // the chainstate is, and what arrives here has already passed them.
        let announcing = relay.take_announcing();
        let sent = swarm.relay(&announcing, &mut round).await;
        if sent > 0 {
            println!(
                "p2p: relayed {} accepted items to peers in {sent} pushes",
                announcing.len()
            );
        }
        if round.dialled > 0 || round.dropped > 0 || round.isolated > 0 {
            println!(
                "p2p: {} connected ({} new, {} lost, {} isolated), {} addresses learned, \
                 {} claiming this cycle",
                round.connected,
                round.dialled,
                round.dropped,
                round.isolated,
                round.learned,
                round.claiming,
            );
        }
        // Everything peers said unprompted. The count used to be reported as
        // "unsolicited messages dropped", which was two mistakes in one phrase: they
        // are announcements a peer is *supposed* to send — mainnet's are almost all
        // signer chunks, at up to 0.8 a second per peer — and counting enough of them
        // as misbehaviour is what was isolating four peers in seven.
        //
        // Pushed blocks and transactions no longer arrive here at all: a session with
        // a `Service` hands them to it, and this node's `Service` puts them on the
        // relay queue. What `take_pushed` still returns is the signer and StackerDB
        // chunks, which nano replicates over HTTP.
        metrics.publish_ingress_queue(
            nano_rpc::IngressQueue::PeerPushes,
            swarm.pushed_status().into(),
        );
        publish_frame_budget(&metrics, swarm.frame_budget_status());
        let carried = swarm.take_pushed().len();
        if round.collected > 0 {
            println!(
                "p2p: {} messages peers sent unprompted, {carried} of them for a role \
                 nano serves over HTTP",
                round.collected
            );
        }
    }
}

fn publish_frame_budget(metrics: &nano_rpc::NodeMetrics, status: nano_p2p::FrameBudgetStatus) {
    metrics.publish_p2p_frames(nano_rpc::AdmissionStatus {
        used: status.bytes,
        subjects: status.addresses,
        limit: status.global_byte_limit,
        per_subject_limit: status.per_address_byte_limit,
        saturations: status.saturations,
    });
}

/// Answer peers that dial this node.
///
/// A node that does not listen can sync perfectly well; what it cannot do is get
/// into anybody else's peer table, which is the difference between using the
/// network and being part of it.
fn start_listener(
    config: &Config,
    network: Network,
    bind: std::net::SocketAddr,
    service: Arc<PeerService>,
    frame_budget: nano_p2p::FrameBudget,
    metrics: nano_rpc::NodeMetrics,
    roles: &mut JoinSet<(Job, Role)>,
) {
    let Ok(identity) = p2p_identity(&config.node.working_dir) else {
        return;
    };
    let protocol = nano_p2p::Protocol::for_network(network)
        .with_stable_confirmations(config.burnchain.stable_confirmations)
        .expect("the configuration validates the stable confirmation count");
    let advertise = config.node.p2p_address.unwrap_or(bind);
    let mut local = nano_p2p::LocalPeer::quiet(identity, advertise.port());
    if !advertise.ip().is_unspecified() {
        local.address = nano_p2p::PeerAddress::from_ip(advertise.ip());
    }
    advertise_peer_services(&mut local, config.node.rpc_bind);
    roles.spawn(async move {
        (
            Job::Peers,
            answer_peers(bind, local, protocol, service, frame_budget, metrics).await,
        )
    });
}

/// Answer inbound peers until the socket fails.
async fn answer_peers(
    bind: std::net::SocketAddr,
    local: nano_p2p::LocalPeer,
    protocol: nano_p2p::Protocol,
    service: Arc<PeerService>,
    frame_budget: nano_p2p::FrameBudget,
    metrics: nano_rpc::NodeMetrics,
) -> Role {
    {
        let listener = nano_p2p::Listener::bind(bind)
            .await
            .map_err(|error| format!("cannot listen for peers on {bind}: {error}"))?;
        println!("p2p: listening for peers on {bind}");
        let mut conversations: JoinSet<()> = JoinSet::new();
        let address_slots = InboundAddressSlots::new(metrics);
        loop {
            let (stream, from) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    eprintln!("accepting a peer failed: {error}");
                    continue;
                }
            };
            while conversations.try_join_next().is_some() {}
            let Some(address_slot) = address_slots.try_acquire(from.ip()) else {
                continue;
            };
            let service = service.clone();
            let local = local.clone();
            let frame_budget = frame_budget.clone();
            conversations.spawn(async move {
                let _address_slot = address_slot;
                if let Err(error) = nano_p2p::serve_peer_with_budget(
                    stream,
                    from,
                    &local,
                    protocol,
                    service.as_ref(),
                    nano_p2p::InboundLimits::default(),
                    frame_budget,
                )
                .await
                {
                    // Per-peer and unremarkable: a peer that hangs up mid-sentence
                    // is the common case, not an incident.
                    eprintln!("inbound peer {from} ended: {error}");
                }
            });
        }
    }
}

/// How many inbound peers to hold conversations with at once.
const MAX_INBOUND_PEERS: usize = 64;

/// How many inbound conversations one IP may hold simultaneously.
const MAX_INBOUND_PEERS_PER_ADDRESS: usize = 4;

#[derive(Clone)]
struct InboundAddressSlots {
    held: Arc<std::sync::Mutex<InboundSessionAccounting>>,
    metrics: nano_rpc::NodeMetrics,
}

impl InboundAddressSlots {
    fn new(metrics: nano_rpc::NodeMetrics) -> Self {
        let slots = Self {
            held: Arc::new(std::sync::Mutex::new(InboundSessionAccounting::default())),
            metrics,
        };
        slots.publish(
            &slots
                .held
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        slots
    }

    fn try_acquire(&self, address: std::net::IpAddr) -> Option<InboundAddressSlot> {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if held.total >= MAX_INBOUND_PEERS
            || held.addresses.get(&address).copied().unwrap_or(0) >= MAX_INBOUND_PEERS_PER_ADDRESS
        {
            held.saturations = held.saturations.saturating_add(1);
            self.publish(&held);
            return None;
        }
        held.total += 1;
        *held.addresses.entry(address).or_default() += 1;
        self.publish(&held);
        drop(held);
        Some(InboundAddressSlot {
            held: self.held.clone(),
            metrics: self.metrics.clone(),
            address,
        })
    }

    fn publish(&self, held: &InboundSessionAccounting) {
        self.metrics.publish_p2p_inbound_sessions(held.status());
    }

    #[cfg(test)]
    fn status(&self) -> nano_rpc::AdmissionStatus {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status()
    }
}

struct InboundAddressSlot {
    held: Arc<std::sync::Mutex<InboundSessionAccounting>>,
    metrics: nano_rpc::NodeMetrics,
    address: std::net::IpAddr,
}

impl Drop for InboundAddressSlot {
    fn drop(&mut self) {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        held.total -= 1;
        let count = held
            .addresses
            .get_mut(&self.address)
            .expect("an inbound slot has an address count");
        *count -= 1;
        if *count == 0 {
            held.addresses.remove(&self.address);
        }
        self.metrics.publish_p2p_inbound_sessions(held.status());
    }
}

#[derive(Default)]
struct InboundSessionAccounting {
    total: usize,
    addresses: HashMap<std::net::IpAddr, usize>,
    saturations: u64,
}

impl InboundSessionAccounting {
    fn status(&self) -> nano_rpc::AdmissionStatus {
        nano_rpc::AdmissionStatus {
            used: self.total,
            subjects: self.addresses.len(),
            limit: MAX_INBOUND_PEERS,
            per_subject_limit: MAX_INBOUND_PEERS_PER_ADDRESS,
            saturations: self.saturations,
        }
    }
}

/// What this node tells a peer that dialled it.
struct PeerService {
    peers: std::sync::Mutex<nano_p2p::PeerDb>,
    /// What the follow loop last published about this node's own chain.
    advertised: Advertised,
    /// Where a pushed block or transaction goes: onto a bounded queue, and nowhere
    /// near a decision. This runs on the listener's task and has no chainstate, so
    /// the most it can honestly do is write down who said what.
    relay: nano_p2p::Relay,
    stable_confirmations: u64,
}

impl nano_p2p::Service for PeerService {
    fn chain_view(&self) -> nano_p2p::ChainView {
        // The async discovery loop reads Bitcoin and publishes the result. Inbound
        // replies only copy it, so a peer cannot make this synchronous listener wait
        // on Bitcoin I/O and still sees the same local view the outbound swarm sends.
        self.advertised.chain_view(self.stable_confirmations)
    }

    fn tenure_inventory(
        &self,
        cycle_start: nano_primitives::ConsensusHash,
    ) -> Option<nano_primitives::BitVec<2100>> {
        // Answered from the snapshot the follow loop published, never by asking the
        // executor: a reply that took the executor's lock would let one inbound peer
        // stall the loop that executes blocks.
        self.advertised.tenure_inventory(cycle_start)
    }

    /// Queue what a peer pushed, and nothing else.
    ///
    /// Every check that matters — the signer weight, the miner signature, the
    /// coinbase VRF proof, the committed seed, the header's cumulative burn against
    /// nano's own burnchain, and then the state root at execution — runs in the follow
    /// loop, where there is a chainstate to run them against. What happens here is a
    /// bounded write to a queue, because a listener that could reject a block is a
    /// listener that could accept one.
    fn offer_blocks(&self, from: nano_primitives::Hash160, blocks: Vec<NakamotoBlock>) {
        for block in blocks {
            self.relay.offer(nano_p2p::Offer::block(Some(from), block));
        }
    }

    fn offer_transaction(
        &self,
        from: nano_primitives::Hash160,
        transaction: Box<nano_codec::Transaction>,
    ) {
        self.relay
            .offer(nano_p2p::Offer::transaction(Some(from), transaction));
    }

    fn neighbors(&self) -> Vec<nano_p2p::NeighborAddress> {
        // Only peers a handshake proved a key for. Passing on an address this node
        // merely heard about would make it a relay for somebody else's claims, and
        // `MAX_NEIGHBORS_DATA_LEN` is 128.
        self.peers
            .lock()
            .ok()
            .and_then(|peers| peers.candidates(128).ok())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|peer| {
                Some(nano_p2p::NeighborAddress {
                    address: peer.address,
                    port: peer.port,
                    public_key_hash: peer.public_key_hash.filter(|_| peer.last_seen.is_some())?,
                })
            })
            .collect()
    }
}

/// Resolve when the process is asked to stop.
async fn terminated() {
    let mut terminate = match tokio::signal::unix::signal(SignalKind::terminate()) {
        Ok(signal) => signal,
        Err(error) => {
            eprintln!("cannot listen for SIGTERM: {error}");
            return std::future::pending().await;
        }
    };
    tokio::select! {
        _ = terminate.recv() => {}
        result = tokio::signal::ctrl_c() => {
            if let Err(error) = result {
                eprintln!("cannot listen for SIGINT: {error}");
                std::future::pending::<()>().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, path::Path};

    use nano_bitcoin::{
        BitcoinBlock, BitcoinOperationKind, BitcoinSource, PreStxCache, decode_block_with_pre_stx,
    };
    use nano_chainstate::{
        ChainState, ChainStateError, CheckpointBoundaryProof, CheckpointHistoryBlock,
        CheckpointHistoryError, ConsensusError, NakamotoBlock, TenureVrfError, coinbase_vrf_proof,
        starts_new_tenure,
    };
    use nano_crypto::StacksPrivateKey;
    use nano_primitives::{ConsensusHash, Network, Sha256Sum, TrieHash, hash160};
    use nano_sync::PoxInfo;

    #[test]
    fn one_address_cannot_own_the_inbound_task_set() {
        let slots = super::InboundAddressSlots::new(nano_rpc::NodeMetrics::default());
        let first = "127.0.0.1".parse().expect("an address");
        let second = "127.0.0.2".parse().expect("an address");
        let mut held = (0..super::MAX_INBOUND_PEERS_PER_ADDRESS)
            .map(|_| slots.try_acquire(first).expect("the peer has capacity"))
            .collect::<Vec<_>>();
        assert!(slots.try_acquire(first).is_none());
        let other = slots
            .try_acquire(second)
            .expect("another address has independent capacity");

        drop(held.pop());
        let recovered = slots.try_acquire(first).expect("a closed slot recovers");
        drop((held, other, recovered));

        let global = (0..super::MAX_INBOUND_PEERS)
            .map(|index| {
                let address = format!("127.0.1.{}", index + 1)
                    .parse()
                    .expect("a distinct address");
                slots.try_acquire(address).expect("the global slot fits")
            })
            .collect::<Vec<_>>();
        assert!(
            slots
                .try_acquire("127.0.2.1".parse().expect("an address"))
                .is_none(),
            "the global session limit is enforced in the same accounting"
        );
        assert_eq!(
            slots.status(),
            nano_rpc::AdmissionStatus {
                used: super::MAX_INBOUND_PEERS,
                subjects: super::MAX_INBOUND_PEERS,
                limit: super::MAX_INBOUND_PEERS,
                per_subject_limit: super::MAX_INBOUND_PEERS_PER_ADDRESS,
                saturations: 2,
            }
        );
        drop(global);
        assert_eq!(slots.status().used, 0);
    }

    #[test]
    fn a_serving_peer_advertises_relay_and_its_rpc_endpoint() {
        let mut local = nano_p2p::LocalPeer::quiet(StacksPrivateKey::from_seed(b"peer"), 20444);
        super::advertise_peer_services(&mut local, None);
        assert_eq!(local.services, nano_p2p::wire::services::RELAY);
        assert!(local.data_url.is_empty());

        let rpc = "127.0.0.1:20443".parse().expect("an RPC address");
        super::advertise_peer_services(&mut local, Some(rpc));
        assert_eq!(
            local.services,
            nano_p2p::wire::services::RELAY | nano_p2p::wire::services::RPC
        );
        assert_eq!(local.data_url, "http://127.0.0.1:20443");
    }

    #[test]
    fn inbound_replies_share_the_locally_derived_bitcoin_view() {
        let advertised = super::Advertised::default();
        let stable_confirmations = 7;
        assert_eq!(
            advertised.chain_view(stable_confirmations).height,
            100_000,
            "a listener with no local view uses only the uncontradictable stale view"
        );

        let view = nano_p2p::ChainView::with_stable_confirmations(
            285,
            nano_primitives::BitcoinHeaderHash::from_bytes([1; 32]),
            nano_primitives::BitcoinHeaderHash::from_bytes([2; 32]),
            stable_confirmations,
        )
        .expect("a settled view");
        advertised.publish_view(view);

        assert_eq!(advertised.chain_view(stable_confirmations), view);
    }

    #[test]
    fn the_first_peer_handshake_uses_the_local_bitcoin_tip() {
        struct ViewSource;

        impl BitcoinSource for ViewSource {
            type Error = &'static str;

            fn block_at(&mut self, _height: u64) -> Result<BitcoinBlock, Self::Error> {
                Err("this view reads headers only")
            }

            fn block_hash_at(&self, height: u64) -> Result<[u8; 32], Self::Error> {
                match height {
                    278 => Ok([2; 32]),
                    285 => Ok([1; 32]),
                    _ => Err("height outside the view"),
                }
            }

            fn tip_height(&self) -> Result<u64, Self::Error> {
                Ok(285)
            }
        }

        let view = super::advertised_view(&ViewSource, 7);
        assert_eq!(view.height, 285);
        assert_eq!(view.stable_height, 278);
        assert_eq!(view.hash.as_bytes(), &[1; 32]);
        assert_eq!(view.stable_hash.as_bytes(), &[2; 32]);
    }

    /// Every execution batch says where it started, where it ended, how many
    /// blocks that was and the root it sealed — and a batch that executed nothing
    /// says *that*, in a sentence nothing else produces.
    ///
    /// This is the line an operator reads to know whether a node is moving. It
    /// was once one sentence with a count in it, so a node that had executed
    /// nothing past its checkpoint printed the same shape as one executing a
    /// thousand blocks a round, and looked healthy for hours.
    #[test]
    fn every_execution_batch_names_its_start_end_count_and_root() {
        let mut tip = test_block(1_200);
        tip.header.state_index_root = nano_primitives::TrieHash::from_bytes([0xab; 32]);
        let round = |executed: usize, rate_limited: bool| crate::CatchUpRound {
            reorganized: None,
            fetched: 40,
            executed,
            authenticated_tenure_starts: usize::from(executed != 0),
            staged: 9,
            scheduled: 0,
            rate_limited,
        };

        let moved = super::round_report(1_100, &round(100, false), &tip);
        assert_eq!(
            moved,
            format!(
                "executed 100 blocks, 1100 to 1200, state root {}, 9 staged, 40 fetched, authentication passed: 100 block envelope/miner-signature/winner-sortition/signer-threshold/tenure-continuity checks and 1 tenure-start coinbase-vrf/parent-seed checks",
                tip.header.state_index_root
            )
        );

        // A round that executed nothing has no root to name and must not be
        // mistakable for one that did.
        let still = super::round_report(1_100, &round(0, false), &tip);
        assert_eq!(
            still,
            "executed nothing: sealed at 1100, 9 staged, 40 fetched"
        );
        assert!(!still.contains("state root"));

        // The peer asking a node to slow down is not the node failing to move.
        assert!(
            super::round_report(1_100, &round(0, true), &tip).ends_with(", peer rate limiting")
        );

        // A round that *failed* is reported the same way, because its error is
        // about the peer's chain and says nothing about where this node is: a node
        // whose every round fails would otherwise never state its own height.
        assert_eq!(
            super::failed_round_report(1_200, &tip),
            "executed nothing: sealed at 1200, then the round failed"
        );
        assert_eq!(
            super::failed_round_report(1_150, &tip),
            format!(
                "executed 50 blocks, 1150 to 1200, state root {}, then the round failed",
                tip.header.state_index_root
            )
        );
    }

    #[test]
    fn local_miner_slots_require_registered_writers() {
        let first = nano_primitives::Hash160::from_bytes([1; 20]);
        let second = nano_primitives::Hash160::from_bytes([2; 20]);

        assert_eq!(
            super::local_miner_slots(Some([first, second]), &[second, first]),
            Some(vec![first, first, second, second])
        );
        assert_eq!(
            super::local_miner_slots(Some([first, second]), &[first]),
            None,
            "a locally named writer still has to exist in the leader-key registry"
        );
    }

    #[test]
    fn miner_slots_accept_only_registered_metadata_signers() {
        let first_key = StacksPrivateKey::from_seed(b"first miner");
        let second_key = StacksPrivateKey::from_seed(b"second miner");
        let stranger_key = StacksPrivateKey::from_seed(b"stranger");
        let first_writer = hash160(&first_key.public_key().to_bytes_compressed());
        let second_writer = hash160(&second_key.public_key().to_bytes_compressed());

        let signed = |slot_id, key: &StacksPrivateKey| {
            let mut metadata =
                nano_stackerdb::SlotMetadata::unsigned(slot_id, 7, Sha256Sum::default());
            metadata.sign(key);
            metadata
        };
        let listing = [
            signed(0, &first_key),
            signed(1, &first_key),
            signed(2, &second_key),
            nano_stackerdb::SlotMetadata::unsigned(3, 0, Sha256Sum::default()),
        ];

        assert_eq!(
            super::authenticated_miner_slots(listing.clone(), &[second_writer, first_writer]),
            Some(vec![
                first_writer,
                first_writer,
                second_writer,
                second_writer
            ]),
            "slot order comes from authenticated metadata, not registry order"
        );
        assert_eq!(
            super::authenticated_miner_slots(listing, &[second_writer]),
            None,
            "both recovered writers must exist in the local registry"
        );
        assert_eq!(
            super::authenticated_miner_slots(
                [
                    signed(0, &first_key),
                    signed(1, &first_key),
                    signed(2, &stranger_key),
                ],
                &[first_writer, second_writer],
            ),
            None,
            "a peer cannot introduce a writer by signing its listing with a stranger key"
        );
        assert_eq!(
            super::authenticated_miner_slots(
                [
                    signed(0, &first_key),
                    signed(1, &second_key),
                    signed(2, &second_key),
                ],
                &[first_writer, second_writer],
            ),
            None,
            "both slots in a miner's pair must recover the same writer"
        );
    }

    /// A header with nothing in it but the two fields the report reads.
    fn test_block(chain_length: u64) -> super::NakamotoBlock {
        super::NakamotoBlock {
            header: nano_chainstate::NakamotoBlockHeader {
                version: 1,
                chain_length,
                bitcoin_spent: 0,
                consensus_hash: nano_primitives::ConsensusHash::from_bytes([0; 20]),
                parent_block_id: nano_primitives::StacksBlockId::from_bytes([7; 32]),
                transaction_merkle_root: nano_primitives::Sha256Sum::default(),
                state_index_root: nano_primitives::TrieHash::from_bytes([0; 32]),
                timestamp: 0,
                miner_signature: nano_crypto::MessageSignature::from_bytes([0; 65]),
                signer_signatures: Vec::new(),
                pox_treatment: nano_primitives::BitVec::zeros(1).expect("a bit vector"),
                problematic_transactions: Vec::new(),
            },
            transactions: Vec::new(),
        }
    }

    fn captured_fixtures() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../nano-conformance/fixtures")
    }

    #[derive(serde::Deserialize)]
    struct CapturedBurnBlock {
        block_height: u64,
        burn_header_hash: String,
        consensus_hash: String,
    }

    #[derive(Clone)]
    struct CapturedBurnchain {
        blocks: BTreeMap<u64, BitcoinBlock>,
    }

    impl BitcoinSource for CapturedBurnchain {
        type Error = String;

        fn block_at(&mut self, height: u64) -> Result<BitcoinBlock, Self::Error> {
            self.blocks
                .get(&height)
                .cloned()
                .ok_or_else(|| format!("no captured Bitcoin block at {height}"))
        }

        fn block_hash_at(&self, height: u64) -> Result<[u8; 32], Self::Error> {
            self.blocks
                .get(&height)
                .map(|block| block.hash)
                .ok_or_else(|| format!("no captured Bitcoin block at {height}"))
        }

        fn tip_height(&self) -> Result<u64, Self::Error> {
            self.blocks
                .last_key_value()
                .map(|(height, _)| *height)
                .ok_or_else(|| "the captured burnchain is empty".to_owned())
        }
    }

    fn captured_burnchain(root: &Path) -> (CapturedBurnchain, BTreeMap<String, u64>) {
        let mut rows: Vec<CapturedBurnBlock> = serde_json::from_slice(
            &fs::read(root.join("sortition/snapshots.json")).expect("read snapshots"),
        )
        .expect("decode snapshots");
        rows.sort_by_key(|row| row.block_height);
        let mut cache = PreStxCache::new();
        let mut blocks = BTreeMap::new();
        let mut heights = BTreeMap::new();
        for row in rows {
            let encoded = fs::read_to_string(
                root.join("bitcoin/blocks")
                    .join(format!("{}.hex", row.burn_header_hash)),
            )
            .expect("read captured Bitcoin block");
            let raw = hex::decode(encoded.trim()).expect("decode captured Bitcoin block hex");
            let block = decode_block_with_pre_stx(row.block_height, &raw, *b"T3", &mut cache)
                .expect("decode captured Bitcoin block");
            heights.insert(row.consensus_hash, row.block_height);
            blocks.insert(row.block_height, block);
        }
        (CapturedBurnchain { blocks }, heights)
    }

    fn captured_blocks(root: &Path) -> Vec<NakamotoBlock> {
        let mut paths = fs::read_dir(root.join("nakamoto/blocks"))
            .expect("read captured block directory")
            .map(|entry| entry.expect("read captured block entry").path())
            .collect::<Vec<_>>();
        paths.sort();
        paths
            .into_iter()
            .map(|path| {
                NakamotoBlock::decode(&fs::read(path).expect("read captured block"))
                    .expect("decode captured block")
            })
            .collect()
    }

    fn captured_pox() -> PoxInfo {
        PoxInfo {
            first_bitcoin_height: 0,
            bitcoin_height: 0,
            prepare_phase_length: 5,
            reward_phase_length: 15,
            reward_slots: 30,
            rejection_fraction: None,
            pox_5_activation_height: Some(262),
            v1_unlock_height: None,
            v2_unlock_height: None,
            v3_unlock_height: None,
        }
    }

    #[test]
    fn validator_sortitions_are_saved_at_the_sealed_standing_height() {
        let root = captured_fixtures();
        let chain = captured_blocks(&root);
        let (boundary_index, _, source_index) = authentication_window(&chain);
        let boundary = chain[boundary_index].header.consensus_hash;
        let (mut bitcoin, heights) = captured_burnchain(&root);
        let mut tracker = crate::sortition::SortitionTracker::from_capture_at_consensus(
            &root.join("sortition"),
            boundary,
        )
        .expect("seed at the authentication boundary");
        tracker
            .recover_seed(|height| bitcoin.block_at(height))
            .expect("recover the boundary winner seed");
        let source_view = chain[source_index]
            .bitcoin_view_consensus_hash()
            .unwrap_or(chain[source_index].header.consensus_hash)
            .to_string();
        let standing = heights[&source_view];
        assert!(standing > tracker.tip().bitcoin_height);

        let state = tempfile::tempdir().expect("a role-specific state directory");
        super::persist_validator_sortitions_at_standing(
            &captured_pox(),
            &mut tracker,
            &mut bitcoin,
            state.path(),
            standing,
        )
        .expect("derive and persist through the validator's standing burn");
        let expected = tracker
            .consensus_hash_at(standing)
            .expect("the derived chain names the standing burn");

        let resumed = crate::sortition::SortitionTracker::resume_or_capture_below(
            state.path(),
            &root.join("sortition"),
            standing,
        )
        .expect("the role resumes its locally derived chain");
        assert_eq!(resumed.tip().bitcoin_height, standing);
        assert_eq!(resumed.consensus_hash_at(standing), Some(expected));
    }

    struct AuthenticatedHistoryFixture {
        checkpoint: std::path::PathBuf,
        source: [u8; 32],
        root: TrieHash,
        boundary: CheckpointBoundaryProof,
        history: Vec<CheckpointHistoryBlock>,
        other_vrf_key: [u8; 32],
    }

    fn authentication_window(chain: &[NakamotoBlock]) -> (usize, usize, usize) {
        let starts = chain
            .iter()
            .enumerate()
            .filter_map(|(index, block)| starts_new_tenure(block).then_some(index))
            .take(3)
            .collect::<Vec<_>>();
        assert_eq!(starts.len(), 3, "the capture crosses three tenures");
        (starts[0], starts[1], starts[2] - 1)
    }

    fn alternate_vrf_key(root: &Path, boundary: [u8; 32]) -> [u8; 32] {
        #[derive(serde::Deserialize)]
        struct LeaderKey {
            public_key: String,
        }
        let keys: Vec<LeaderKey> = serde_json::from_slice(
            &fs::read(root.join("sortition/leader-keys.json")).expect("read leader keys"),
        )
        .expect("decode leader keys");
        keys.into_iter()
            .filter_map(|key| hex::decode(key.public_key).ok())
            .filter_map(|key| <[u8; 32]>::try_from(key).ok())
            .find(|key| *key != boundary)
            .expect("the capture carries another valid VRF key")
    }

    fn authenticated_history_fixture() -> AuthenticatedHistoryFixture {
        let root = captured_fixtures();
        let chain = captured_blocks(&root);
        let (boundary_index, first, source_index) = authentication_window(&chain);
        let source_block = &chain[source_index];
        let boundary_block = &chain[boundary_index];
        let (mut burnchain, heights) = captured_burnchain(&root);
        let loaded_boundary = super::LoadedBoundaryProof {
            parent_tenure_consensus_hash: boundary_block.header.consensus_hash,
            coinbase_vrf_proof: coinbase_vrf_proof(boundary_block)
                .expect("boundary tenure carries a coinbase proof"),
        };
        let mut tracker = crate::sortition::SortitionTracker::from_capture_at_consensus(
            &root.join("sortition"),
            loaded_boundary.parent_tenure_consensus_hash,
        )
        .expect("seed exactly at the captured authentication boundary");
        let boundary_height = tracker.tip().bitcoin_height;
        tracker
            .recover_seed(|height| burnchain.block_at(height))
            .expect("recover the boundary winner seed");
        let source_view = source_block
            .bitcoin_view_consensus_hash()
            .unwrap_or(source_block.header.consensus_hash)
            .to_string();
        let target = heights[&source_view];
        assert!(target > boundary_height, "the source is above its boundary");
        let (boundary, history) = super::derive_checkpoint_authentication(
            &captured_pox(),
            &mut tracker,
            &mut burnchain,
            loaded_boundary,
            &chain[first..=source_index],
            target,
        )
        .expect("derive locally from the boundary before authenticating the history");
        assert_eq!(tracker.tip().bitcoin_height, target);
        AuthenticatedHistoryFixture {
            checkpoint: root.join("chainstate/checkpoint-H/marf.sqlite"),
            source: *source_block.block_id().as_bytes(),
            root: source_block.header.state_index_root,
            other_vrf_key: alternate_vrf_key(&root, boundary.winner_vrf_public_key),
            boundary,
            history,
        }
    }

    #[test]
    fn checkpoint_boundary_winner_comes_from_bitcoin_and_the_local_registry() {
        let root = captured_fixtures();
        let chain = captured_blocks(&root);
        let (boundary_index, _, _) = authentication_window(&chain);
        let boundary_hash = chain[boundary_index].header.consensus_hash;
        let (mut burnchain, heights) = captured_burnchain(&root);
        let mut tracker = crate::sortition::SortitionTracker::from_capture_at_consensus(
            &root.join("sortition"),
            boundary_hash,
        )
        .expect("select the captured boundary");
        tracker
            .recover_seed(|height| burnchain.block_at(height))
            .expect("recover the boundary seed from Bitcoin");
        super::advance_to_checkpoint_boundary(
            &captured_pox(),
            &mut tracker,
            &mut burnchain,
            boundary_hash,
            heights[&boundary_hash.to_string()],
        )
        .expect("derive the boundary locally from the captured seed");
        let boundary_height = tracker.tip().bitcoin_height;
        let block = burnchain
            .block_at(boundary_height)
            .expect("read boundary Bitcoin block");
        tracker
            .authenticate_boundary_winner(&block)
            .expect("the named commitment resolves through the local registry");

        let mut missing = block.clone();
        missing.operations.clear();
        assert!(matches!(
            tracker.authenticate_boundary_winner(&missing),
            Err(crate::sortition::TrackerError::BoundaryWinnerCommitmentMissing { .. })
        ));

        let winner_txid = tracker
            .tip()
            .winner_txid
            .expect("the boundary has a winner");
        let mut absent_key = block;
        let winner = absent_key
            .operations
            .iter_mut()
            .find(|operation| operation.txid == winner_txid)
            .expect("the Bitcoin block carries its winner");
        let BitcoinOperationKind::LeaderBlockCommit {
            key_block_height,
            key_transaction_index,
            ..
        } = &mut winner.kind
        else {
            panic!("the winner is a leader commitment");
        };
        *key_block_height = u32::MAX;
        *key_transaction_index = u16::MAX;
        assert!(matches!(
            tracker.authenticate_boundary_winner(&absent_key),
            Err(crate::sortition::TrackerError::BoundaryWinnerKeyUnavailable { .. })
        ));
    }

    #[derive(Debug, Eq, PartialEq)]
    struct AuthenticationState {
        tip: Option<[u8; 32]>,
        executed: Vec<[u8; 32]>,
        tenures: Vec<ConsensusHash>,
        parent_proof: Option<[u8; 80]>,
        tenure_height: u32,
        tenure_start: Option<u32>,
        clarity_tenure_start: Option<u32>,
    }

    fn authentication_state(chainstate: &mut ChainState) -> AuthenticationState {
        let tenure_height = chainstate
            .vm_mut()
            .tenure_height()
            .expect("read checkpoint tenure height");
        AuthenticationState {
            tip: chainstate.tip().expect("read checkpoint tip"),
            executed: chainstate.executed_blocks(),
            tenures: chainstate.executed_tenures(),
            parent_proof: chainstate.parent_tenure_proof(),
            tenure_height,
            tenure_start: chainstate.tenure_start_height(tenure_height),
            clarity_tenure_start: chainstate.clarity_tenure_start_height(tenure_height),
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum AuthenticationRefusal {
        MalformedBoundaryProof,
        WrongBoundaryKey,
        WrongBoundaryTenure,
        MissingWinnerKey,
        MissingSigningKey,
        WrongCommittedParentSeed,
    }

    fn corrupt_authentication_history(
        refusal: AuthenticationRefusal,
        boundary: &mut CheckpointBoundaryProof,
        history: &mut [CheckpointHistoryBlock],
        other_vrf_key: [u8; 32],
    ) {
        match refusal {
            AuthenticationRefusal::MalformedBoundaryProof => {
                boundary.coinbase_vrf_proof = [0; 80];
            }
            AuthenticationRefusal::WrongBoundaryKey => {
                boundary.winner_vrf_public_key = other_vrf_key;
            }
            AuthenticationRefusal::WrongBoundaryTenure => {
                boundary.parent_tenure_consensus_hash = ConsensusHash::from_bytes([0xff; 20]);
            }
            AuthenticationRefusal::MissingWinnerKey => {
                history[0].bitcoin_context.winner_vrf_public_key = None;
            }
            AuthenticationRefusal::MissingSigningKey => {
                history[0].bitcoin_context.winner_signing_key_hash = None;
                history[0].operations.clear();
            }
            AuthenticationRefusal::WrongCommittedParentSeed => {
                history[0].bitcoin_context.vrf_seed[0] ^= 0xff;
            }
        }
    }

    fn expected_authentication_refusal(
        refusal: AuthenticationRefusal,
        error: &CheckpointHistoryError,
    ) -> bool {
        match refusal {
            AuthenticationRefusal::MalformedBoundaryProof => matches!(
                error,
                CheckpointHistoryError::BoundaryProof(TenureVrfError::MalformedProof)
            ),
            AuthenticationRefusal::WrongBoundaryKey => matches!(
                error,
                CheckpointHistoryError::BoundaryProof(TenureVrfError::ProofNotFromLeaderKey)
            ),
            AuthenticationRefusal::WrongBoundaryTenure => {
                matches!(error, CheckpointHistoryError::BoundaryTenure)
            }
            AuthenticationRefusal::MissingWinnerKey => matches!(
                error,
                CheckpointHistoryError::Block {
                    error: ChainStateError::InvalidTransaction(reason),
                    ..
                } if reason == &ConsensusError::WinnerVrfKeyUnavailable.to_string()
            ),
            AuthenticationRefusal::MissingSigningKey => matches!(
                error,
                CheckpointHistoryError::Block {
                    error: ChainStateError::InvalidTransaction(reason),
                    ..
                } if reason == &ConsensusError::WinnerSigningKeyUnavailable.to_string()
            ),
            AuthenticationRefusal::WrongCommittedParentSeed => matches!(
                error,
                CheckpointHistoryError::Vrf {
                    error: TenureVrfError::SeedNotFromParentProof,
                    ..
                }
            ),
        }
    }

    #[test]
    fn checkpoint_history_authentication_is_fail_closed_and_atomic() {
        let fixture = authenticated_history_fixture();
        let directory = tempfile::tempdir().expect("a chainstate directory");
        let mut chainstate = ChainState::open_from_checkpoint(
            Network::TESTNET,
            directory.path(),
            &fixture.checkpoint,
            fixture.source,
            fixture.root,
        )
        .expect("open the captured checkpoint at the authenticated source");
        let unchanged = authentication_state(&mut chainstate);
        let refusals = [
            AuthenticationRefusal::MalformedBoundaryProof,
            AuthenticationRefusal::WrongBoundaryKey,
            AuthenticationRefusal::WrongBoundaryTenure,
            AuthenticationRefusal::MissingWinnerKey,
            AuthenticationRefusal::MissingSigningKey,
            AuthenticationRefusal::WrongCommittedParentSeed,
        ];
        for refusal in refusals {
            let mut boundary = fixture.boundary;
            let mut history = fixture.history.clone();
            corrupt_authentication_history(
                refusal,
                &mut boundary,
                &mut history,
                fixture.other_vrf_key,
            );
            let error = chainstate
                .authenticate_checkpoint_history(fixture.source, fixture.root, boundary, &history)
                .expect_err("a corrupted authentication history must be refused");
            assert!(
                expected_authentication_refusal(refusal, &error),
                "{refusal:?} reached the wrong refusal: {error}"
            );
            assert_eq!(
                authentication_state(&mut chainstate),
                unchanged,
                "{refusal:?} changed chainstate before refusing"
            );
        }

        chainstate
            .authenticate_checkpoint_history(
                fixture.source,
                fixture.root,
                fixture.boundary,
                &fixture.history,
            )
            .expect("the unmodified authentication history is accepted");
    }

    #[test]
    fn checkpoint_boundary_file_requires_a_complete_well_formed_proof() {
        let directory = tempfile::tempdir().expect("an authentication history directory");
        let history = directory.path().join("authentication-history");
        fs::create_dir_all(&history).expect("create authentication history");
        let config_text = format!(
            r#"
[node]
working_dir = "{}"
peers = []
p2p_seeds = []

[burnchain]

[checkpoint]
marf = "{}"
source_state_id = "{}"
state_root = "{}"
anchor_block = "{}"
anchor_bitcoin_height = 0
authentication_history = "{}"
"#,
            directory.path().display(),
            directory.path().join("marf.sqlite").display(),
            "00".repeat(32),
            "00".repeat(32),
            directory.path().join("anchor.bin").display(),
            history.display(),
        );
        let config: crate::config::Config = toml::from_str(&config_text).expect("parse config");
        let cases = [
            (
                "missing",
                serde_json::json!({
                    "parent_tenure_consensus_hash": "00".repeat(20),
                }),
                "missing field `coinbase_vrf_proof`",
            ),
            (
                "malformed",
                serde_json::json!({
                    "parent_tenure_consensus_hash": "00".repeat(20),
                    "coinbase_vrf_proof": "00",
                }),
                "coinbase_vrf_proof is not 80 bytes",
            ),
        ];
        for (name, boundary, expected) in cases {
            fs::write(
                history.join("boundary.json"),
                serde_json::to_vec(&boundary).expect("encode boundary"),
            )
            .expect("write boundary");
            let error = super::load_checkpoint_authentication_history(&config)
                .expect_err("an absent or malformed boundary proof must be refused")
                .to_string();
            assert!(error.contains(expected), "{name}: {error}");
        }
    }

    /// The cycle a checkpointed node starts in has no pox-5 positions to walk,
    /// and the checkpoint is the only thing that can answer for it.
    ///
    /// Mainnet's cycle 140 was stacked in pox-4, before the boundary the export
    /// was taken at, so `active_signer_set` finds nothing and is right to. The
    /// document that attested the checkpoint *is* what the network published for
    /// that cycle, so it is served verbatim — and only for that cycle, because
    /// serving one cycle's signers as another's is worse than serving none.
    #[tokio::test]
    async fn the_checkpoint_answers_for_the_cycle_it_was_taken_in() {
        let directory = tempfile::tempdir().expect("a directory");
        let document = directory.path().join("stacker-set.json");
        std::fs::write(
            &document,
            br#"{"stacker_set":{"signers":[{"signing_key":"00","weight":1}]}}"#,
        )
        .expect("write the document");
        let checkpoint = crate::config::CheckpointConfig {
            marf: directory.path().join("marf.sqlite"),
            source_state_id: String::new(),
            state_root: String::new(),
            anchor_block: directory.path().join("anchor.bin"),
            anchor_bitcoin_height: 960_231,
            tenure_accounting: None,
            attesting_block: None,
            attesting_reward_set: Some(document),
            sortition: None,
            authentication_history: None,
        };
        let mut context = nano_chainstate::BitcoinBlockContext::at_height(961_300);
        context.first_height = 666_050;
        context.prepare_phase_length = 100;
        context.reward_phase_length = 2_000;

        assert_eq!(super::upcoming_signer_cycle_context(context), None);
        context.move_to_burn_block(962_051);
        assert_eq!(
            super::upcoming_signer_cycle_context(context)
                .and_then(nano_chainstate::signers::reward_cycle_at),
            Some(141)
        );

        let state = super::RpcState::new(super::Network::MAINNET);
        let mut published = super::RewardCyclePublication::default();
        assert!(
            super::carry_checkpoint_set(&state, &mut published, &checkpoint, 140, context).await,
            "the checkpoint's own cycle is answered"
        );
        assert_eq!(published.served, std::collections::BTreeSet::from([140]));
        assert!(
            !super::carry_checkpoint_set(&state, &mut published, &checkpoint, 141, context).await,
            "a later cycle is not answered with the checkpoint's signers"
        );
    }

    /// A state directory belongs to the checkpoint it was built from.
    ///
    /// Its trie stands on that checkpoint's state, so pointing it at another
    /// would leave a node executing on a chain it never imported, and nothing
    /// later would notice.
    #[test]
    fn a_state_directory_is_not_reused_for_another_checkpoint() {
        super::already_adopted([1; 32], [1; 32]).expect("the same checkpoint carries on");
        let refused = super::already_adopted([1; 32], [2; 32])
            .expect_err("a different checkpoint is refused");
        assert!(refused.contains("descends from checkpoint"), "{refused}");
    }

    /// A checkpoint owes the hundred tenures before it, unless the chain is
    /// younger than that — in which case it owes everything there is.
    ///
    /// The earliest payout any block can ask for is tenure 1, because a tenure
    /// below the maturity horizon matures nothing. So earnings reaching back to
    /// the chain's first tenure are complete however few they are, and the
    /// alternative is a node that cannot start from any network less than a
    /// hundred tenures old.
    #[test]
    fn a_short_window_is_enough_only_when_it_reaches_the_chain_s_beginning() {
        let earnings = |first: u64, last: u64| {
            let tenures = (first..=last)
                .map(|height| {
                    format!(
                        r#"{{"coinbase_height":{height},"recipient":"ST000000000000000000002AMW42H",
                            "coinbase":1000,"fees":0}}"#
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            nano_chainstate::TenureAccounting::from_json(
                format!(r#"{{"matured_effects":[],"tenures":[{tenures}]}}"#).as_bytes(),
            )
            .expect("the accounting reads")
        };
        super::check_maturity_window(&earnings(1, 12))
            .expect("a young chain owes nothing before its own first tenure");
        super::check_maturity_window(&earnings(50, 200)).expect("a full window is enough");
        let refused = super::check_maturity_window(&earnings(50, 60))
            .expect_err("a window that neither reaches back nor spans the horizon is refused");
        assert!(
            refused.to_string().contains("tenures 50 to 60"),
            "{refused}"
        );
    }

    /// The reward set that attests a checkpoint is read from what a node serves.
    #[test]
    fn an_attesting_reward_set_is_read_from_a_stacker_set_document() {
        let document = br#"{"stacker_set":{"signers":[
            {"signing_key":"0x03adb8de4bfb65db2cfd6120d55c6526ae9c52e675db7e47308636534ba7786110",
             "weight":3},
            {"signing_key":"02adb8de4bfb65db2cfd6120d55c6526ae9c52e675db7e47308636534ba7786110",
             "weight":1}]}}"#;
        let signers = super::attesting_reward_set(document).expect("the reward set reads");
        assert_eq!(signers.signers().len(), 2);
        assert_eq!(
            signers
                .signers()
                .iter()
                .map(|signer| signer.weight)
                .sum::<u32>(),
            4
        );

        // A document naming no signers is not a reward set, and a checkpoint
        // attested by nobody is not attested.
        assert!(super::attesting_reward_set(br#"{"stacker_set":{"signers":[]}}"#).is_err());
        assert!(super::attesting_reward_set(b"{}").is_err());
    }

    use super::Job;

    /// A network's liveness rests on its signers, so only the jobs that keep
    /// the node honest about what it is doing may end it.
    #[test]
    fn only_signing_and_following_are_fatal() {
        assert!(Job::Signer.is_fatal());
        assert!(Job::Follower.is_fatal());
        assert!(!Job::Miner.is_fatal());
        assert!(!Job::Rpc.is_fatal());
        assert!(!Job::Metrics.is_fatal());
    }

    #[test]
    fn a_stopped_optional_miner_returns_execution_to_the_follower() {
        let active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let lease = super::MinerExecutionLease::claim(active.clone());
        assert!(!super::follower_owns_execution(&active));

        drop(lease);
        assert!(super::follower_owns_execution(&active));
    }
}

#[cfg(test)]
mod backfill_tests {
    use super::block_without_a_header;

    /// The shape the node actually printed when replay stopped at 8,669,750.
    const REAL: &str = "executing the peer's chain failed: node execution failed: \
checkpoint execution failed: Clarity execution error: Internal(Expect(\"FATAL: no burnchain \
block height found for Stacks block dd254a1691f90df22c1d4585c6526feda3b88b941f6ffa8c85d2e6b4bfb0b291\"))";

    #[test]
    fn the_block_is_taken_out_of_the_message_the_node_printed() {
        let block = block_without_a_header(REAL).expect("the block is named");
        assert_eq!(
            hex::encode(block),
            "dd254a1691f90df22c1d4585c6526feda3b88b941f6ffa8c85d2e6b4bfb0b291"
        );
    }

    /// The message a live mainnet node printed 691 times in one run, at height
    /// 8,713,221, and which only a restart cleared.
    ///
    /// The residue is real: that state's MARF held versions to 8,713,522 while its
    /// ledger named 8,713,221, so every round asked the MARF to begin a version it
    /// already had. Startup swept it; nothing did while the node was up.
    const RESIDUE: &str = "executing the peer's chain failed: node execution failed: \
checkpoint execution failed: state storage error: MARF error: MARF version already exists";

    #[test]
    fn the_marf_residue_failure_is_recognised_from_what_the_node_printed() {
        assert!(super::round_hit_marf_residue(RESIDUE));
    }

    /// And nothing else is, because giving back state is not a thing to do on a
    /// guess: every other round failure leaves the states above the tip alone.
    #[test]
    fn no_other_round_failure_gives_back_state() {
        for other in [
            REAL,
            "executing the peer's chain failed: HTTP sync error: no peer left to ask",
            "executing the peer's chain failed: node execution failed: checkpoint execution \
             failed: state root mismatch: expected aa, got bb",
            "executing the peer's chain failed: node execution failed: checkpoint execution \
             failed: invalid transaction: committed seed is not the hash of the parent \
             tenure's VRF proof",
            "executing the peer's chain failed: state storage error: MARF error: write in progress",
        ] {
            assert!(
                !super::round_hit_marf_residue(other),
                "this failure must not give back state: {other}"
            );
        }
    }

    /// A resume stands on the deepest block a *ledger* names, not the deepest seal.
    ///
    /// The two part exactly where a mainnet state parted them: sealed states above
    /// the last one a ledger names. Standing on the seal costs the whole recovery —
    /// no reorganization reach, no tenure start heights and no parent tenure proof.
    /// It also hides the residue from the give-back, because there is nothing above
    /// the seal to give back. Such a restart is now refused rather than reconstructed
    /// from `accounting.json`.
    #[test]
    fn a_resume_stands_on_the_deepest_ledger_not_the_deepest_seal() {
        let directory = tempfile::tempdir().expect("a directory");
        let mut chainstate =
            nano_chainstate::ChainState::open(nano_primitives::Network::MAINNET, directory.path())
                .expect("open");
        let block = |height: u8| [height; 32];

        let mut parent = None;
        for height in 1..=3u8 {
            chainstate
                .vm_mut()
                .begin_block(parent, block(height))
                .expect("begin");
            chainstate
                .vm_mut()
                .commit_block(
                    block(height),
                    &nano_vm::BlockCommit {
                        header: nano_vm::BlockHeader::default(),
                        ledger: b"a block this node committed".to_vec(),
                    },
                )
                .expect("commit");
            parent = Some(block(height));
        }
        // The residue: sealed, and no ledger names them.
        for height in 4..=5u8 {
            chainstate
                .vm_mut()
                .begin_block(parent, block(height))
                .expect("begin");
            chainstate
                .vm_mut()
                .seal_block_to(block(height))
                .expect("seal");
            parent = Some(block(height));
        }

        assert_eq!(
            chainstate.tip().expect("read the deepest seal"),
            Some(block(5)),
            "the deepest seal"
        );
        assert_eq!(
            super::deepest_block_a_ledger_names(&chainstate, block(5), 500)
                .expect("walk sealed parents"),
            block(3),
            "and the deepest block a ledger names is two below it"
        );
        // Which is what makes the give-back reach them at all.
        let height = chainstate
            .height_of(block(3))
            .expect("read the sealed height")
            .expect("a sealed height");
        assert_eq!(chainstate.discard_above(height).expect("give back"), 2);
    }

    #[test]
    fn a_sealed_state_without_a_ledger_cannot_resume_unauthenticated() {
        let directory = tempfile::tempdir().expect("a directory");
        let mut chainstate =
            nano_chainstate::ChainState::open(nano_primitives::Network::MAINNET, directory.path())
                .expect("open");
        let block_id = [1; 32];
        chainstate
            .vm_mut()
            .begin_block(None, block_id)
            .expect("begin");
        chainstate
            .vm_mut()
            .seal_block_to(block_id)
            .expect("seal without a ledger");

        let error = super::recover_ledger(&mut chainstate, block_id)
            .expect_err("an unverifiable restart must be refused")
            .to_string();
        assert!(error.contains("has no committed ledger"), "{error}");
        assert!(error.contains("cannot authenticate a restart"), "{error}");
    }

    /// A reach that does not span the residue leaves the tip where it was.
    ///
    /// Said rather than guessed: resuming somewhere arbitrary because the walk ran
    /// out is worse than resuming at the seal and reporting what that costs.
    #[test]
    fn a_ledger_out_of_reach_leaves_the_resume_at_the_seal() {
        let directory = tempfile::tempdir().expect("a directory");
        let mut chainstate =
            nano_chainstate::ChainState::open(nano_primitives::Network::MAINNET, directory.path())
                .expect("open");
        chainstate
            .vm_mut()
            .begin_block(None, [1; 32])
            .expect("begin");
        chainstate
            .vm_mut()
            .commit_block(
                [1; 32],
                &nano_vm::BlockCommit {
                    header: nano_vm::BlockHeader::default(),
                    ledger: b"the only ledger".to_vec(),
                },
            )
            .expect("commit");
        chainstate
            .vm_mut()
            .begin_block(Some([1; 32]), [2; 32])
            .expect("begin");
        chainstate.vm_mut().seal_block_to([2; 32]).expect("seal");

        assert_eq!(
            super::deepest_block_a_ledger_names(&chainstate, [2; 32], 1)
                .expect("walk sealed parents"),
            [2; 32]
        );
    }

    #[test]
    fn a_trailing_quote_does_not_become_part_of_the_block() {
        // The id is followed by `"))` in the real message, so stopping at the
        // first non-hexadecimal character is what makes this work at all.
        let block = block_without_a_header(REAL).expect("the block is named");
        assert_eq!(block.len(), 32);
    }

    #[test]
    fn other_failures_are_not_mistaken_for_this_one() {
        for error in [
            "state root mismatch: expected 5a301da0, got 6fb41024",
            "a deployment was rejected: contract analysis failed",
            "",
        ] {
            assert!(
                block_without_a_header(error).is_none(),
                "{error} is not a missing header"
            );
        }
    }

    #[test]
    fn a_truncated_block_is_refused_rather_than_padded() {
        // A short id would otherwise be padded into some *other* block, and
        // writing a header under the wrong id is worse than not writing one.
        let error = "no burnchain block height found for Stacks block dd254a16";
        assert!(block_without_a_header(error).is_none());
    }

    /// One state directory holds one node, and the second one says so.
    ///
    /// Both halves matter. A node that refused a directory whose *previous* holder
    /// had exited would be worse than no check at all -- a killed node leaves the
    /// file behind -- so the lock is released with the descriptor and the directory
    /// is takeable again straight after.
    #[test]
    fn a_state_directory_holds_one_node() {
        let directory = tempfile::tempdir().expect("a state directory");
        let held = super::hold_state_directory(directory.path()).expect("the first node holds it");
        let refused = super::hold_state_directory(directory.path());
        assert!(
            refused.is_err(),
            "a second node took a directory another one is running on"
        );
        drop(held);
        super::hold_state_directory(directory.path())
            .expect("a directory whose holder has gone is takeable");
    }
}
