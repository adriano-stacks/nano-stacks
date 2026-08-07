//! Starting a node: open the state, pick a peer, run the configured roles.
//!
//! Everything a role needs is derived here, once, so that following, signing
//! and mining are three tasks over one configuration rather than three
//! programs over three command lines.

use std::{
    collections::HashMap,
    error::Error,
    fs::{self, File, OpenOptions},
    future::Future,
    path::Path,
    sync::Arc,
    time::Duration,
};

use fs2::FileExt as _;

use nano_bitcoin::{BitcoinRestSource, BitcoinRpcSource};
use nano_crypto::StacksPublicKey;
use nano_chainstate::{
    MINER_REWARD_MATURITY, Signer, SignerSet,
    BitcoinBlockContext, ChainState, NakamotoBlock, TenureAccounting,
};
use nano_primitives::{Network, StacksBlockId};
use nano_rpc::{ChainAccess, EventDispatcher, RpcState, SealedTip, serve};
use nano_p2p::Discovered;
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
const SIGNER_CHAINSTATE: &str = "signer-chainstate";
/// The accounting a role used to rewrite as it executed.
///
/// Read only, now: the ledger is committed with the seal, which is what makes it
/// as of the tip rather than as of the last catch-up round. This is still read so
/// that a state directory written before that keeps opening.
const ACCOUNTING_FILE: &str = "accounting.json";

/// The shared executed chain the node follows along and answers reads from.
pub type SharedExecutor = Arc<Mutex<CheckpointExecutor<BurnchainSource>>>;

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
            Self::Rpc | Self::Miner | Self::Peers | Self::Proposals | Self::Replication => false,
        }
    }
}

impl std::fmt::Display for Job {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Rpc => "RPC server",
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
    // Held for the lifetime of the process, and released by the kernel whatever
    // way it ends. Makes the directory as well: nothing runs before this.
    let _state = hold_state_directory(&config.node.working_dir)?;
    let mut roles: JoinSet<(Job, Role)> = JoinSet::new();
    // Written by the follow loop once there is a chain to describe, and read by the
    // discovery loop that starts before there is one.
    let advertised = Advertised::open(&config.node.working_dir);
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
    let discovered = match config.network() {
        Some(network) => {
            start_transport(&config, network, &advertised, &relay, &mut roles).await
        }
        None => None,
    };
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
    announce_executed_blocks(executor.as_ref(), &dispatcher).await;
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
    let (wiring, api_to_loop, hosted) =
        ApiWiring::new(executor.clone(), mempool.clone(), archive);
    let state = start_rpc(&config, network, wiring, &dispatcher, &mut roles).await?;
    publish_sealed_tip(state.as_ref(), executor.as_ref()).await;
    // The miner executes the chain itself, because it has to build on its own
    // blocks the moment it makes them; the follower then only keeps the served
    // view fresh.
    let executing_follower = config.miner.is_none();
    start_miner(
        &config,
        network,
        &pox,
        &peer,
        (executor.clone(), mempool.clone()),
        (dispatcher, relay.clone()),
        &mut roles,
    );
    start_signer(&config, network, &pox, discovered.as_ref(), &mut roles).await?;
    start_hosting(
        &config,
        network,
        &pox,
        discovered.as_ref(),
        state.as_ref(),
        hosted,
        &mut roles,
    )
    .await?;
    let executor = executor.filter(|_| executing_follower);
    // Following is only worth a task when someone reads what it produces: a
    // signer-only node validates from its own store and needs no second view.
    if state.is_some() || executor.is_some() {
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
            state,
            executor,
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
async fn publish_sealed_tip(state: Option<&RpcState>, executor: Option<&SharedExecutor>) {
    if let (Some(state), Some(executor)) = (state, executor) {
        let (sealed, sortitions) = {
            let executor = executor.lock().await;
            (
                sealed_tip(executor.tip(), executor.bitcoin_height()),
                executor.derived_sortitions(),
            )
        };
        state.publish_executed(sealed, sortitions).await;
    }
}

/// A block offered over the public API passes the boundary a followed block
/// passes, and it is the same call: `ChainState::authenticate_block`, which
/// [[050-authenticate-every-followed-nakamoto-block]] put in front of execution.
///
/// Nothing is reimplemented here on purpose. A node that admits over its own API
/// what it would refuse from a peer is forkable through its own API, and the only
/// way to be sure the two agree is for there to be one of them.
impl<S: Send> nano_rpc::BlockAdmission for CheckpointExecutor<S> {
    fn authenticate(&mut self, block: &NakamotoBlock) -> Result<(), String> {
        self.chainstate
            .authenticate_block(block)
            .map_err(|error| error.to_string())
    }
}

/// What this node has sealed, for the RPC to answer from.
fn sealed_tip(tip: &NakamotoBlock, bitcoin_height: u64) -> SealedTip {
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
    batch_report(
        from,
        round.executed,
        tip,
        &format!(
            ", {} staged, {} fetched{scheduled}{limited}",
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
    let executed = usize::try_from(tip.header.chain_length.saturating_sub(from)).unwrap_or(usize::MAX);
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
    peer: &SyncClient,
    pox: &PoxInfo,
    source: [u8; 32],
) {
    let _phase = Phase::start("backfilling ancestor headers");
    let mut executor = executor.lock().await;
    match executor.backfill_headers(peer, pox, source).await {
        Ok(0) => {}
        Ok(recorded) => println!("wrote down {recorded} headers this state was missing"),
        Err(error) => eprintln!("writing down the missing headers failed: {error}"),
    }
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
            // And where that leaves this node, in the sentence every other batch
            // is reported in. The error names the peer's chain; this names ours.
            println!("{}", failed_round_report(from, executor.tip()));
            backfill_missing_header(&mut executor, peer, &error.to_string()).await;
            give_back_states_above_the_tip(&mut executor, &error.to_string());
            peer_failed = true;
        }
    }
    // What this node tells its peers about itself, now that there is a chain to
    // describe: the discovery loop starts before there is one and advertises a
    // deliberately old view until this runs.
    advertised.publish(LocalAnnouncement {
        bitcoin_height: executor.bitcoin_height(),
        cycle_start: executor.cycle_start_consensus_hash(pox),
        inventory: executor.tenure_inventory(pox),
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
    /// Blocks the public API authenticated, waiting to be staged.
    offered: tokio::sync::mpsc::UnboundedReceiver<NakamotoBlock>,
    /// Transactions the public API admitted, waiting to be passed on.
    submitted: tokio::sync::mpsc::UnboundedReceiver<nano_codec::Transaction>,
}

/// What arrived other than by following: what this node's own API admitted, and
/// what peers pushed at it.
struct AdmittedInputs<'a> {
    offered: &'a mut tokio::sync::mpsc::UnboundedReceiver<NakamotoBlock>,
    submitted: &'a mut tokio::sync::mpsc::UnboundedReceiver<nano_codec::Transaction>,
    executor: Option<&'a SharedExecutor>,
    mempool: &'a Arc<Mutex<nano_mempool::Mempool>>,
    relay: &'a nano_p2p::Relay,
    staging: &'a Staging,
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
    } = inputs;
    stage_admitted_blocks(offered, staging, relay);
    relay_admitted_transactions(submitted, relay);
    if let Some(executor) = executor {
        check_relayed(executor, mempool, relay, staging).await;
    }
}

/// The store staged blocks wait in, or the role's own failure.
fn open_staging(config: &Config) -> Result<Staging, Role> {
    Staging::open(&config.chainstate_dir(NODE_CHAINSTATE).join("staging.sqlite"))
        .map_err(|error| Err(format!("cannot open the staging store: {error}")))
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
        pox,
        source,
        state,
        executor,
        mut offered,
        mut submitted,
    } = follower;
    let interval = Duration::from_secs(config.node.poll_interval_secs);
    let staging = match open_staging(&config) {
        Ok(staging) => staging,
        Err(role) => return role,
    };
    let budget = CatchUpBudget {
        // Bounded so that a round ends and execution gets its turn: an
        // unbounded descent over a gap of tens of thousands of blocks spends
        // every round fetching and never executes what it already holds.
        fetch: ROUND_FETCH,
        execute: config.node.max_sync_blocks,
    };
    let mut pox = pox;
    prepare_to_follow(executor.as_ref(), &config, &peer, &pox, source).await;
    let mut rounds = Rounds::new(peer);
    loop {
        rounds.history.refresh(&config, discovered.as_ref());
        rounds
            .choose_peer(&config, discovered.as_ref(), executor.as_ref(), &pox, state.as_ref())
            .await;
        take_admitted(
            AdmittedInputs {
                offered: &mut offered,
                submitted: &mut submitted,
                executor: executor.as_ref(),
                mempool: &mempool,
                relay: &relay,
                staging: &staging,
            },
        )
        .await;
        // Following the peer's current tenure is pointless while this node is
        // far from it — the tenure descends from blocks it has not executed, so
        // the walk fails every round — and the requests it spends are the ones
        // catching up needs. A node this far back has nothing to serve anyway.
        let catching_up =
            rounds.peer_height.saturating_sub(rounds.executed_height) > FOLLOW_WHEN_WITHIN;
        rounds.failed |= track_peer(
            &mut rounds.node,
            &rounds.peer,
            state.as_ref(),
            &mut pox,
            &mut rounds.peer_height,
            catching_up,
        )
        .await;
        if let Some(executor) = executor.as_ref() {
            let inputs = RoundInputs {
                peer: &rounds.peer,
                history: &mut rounds.history.source,
                pox: &pox,
                staging: &staging,
                budget,
                advertised: &advertised,
                claims: discovered.as_ref().map(Discovered::claims).unwrap_or_default(),
            };
            let round = execute_round(executor, inputs).await;
            rounds.executed_height = round.executed_height;
            rounds.failed |= round.peer_failed;
            let sealed = round.sealed;
            if let Some(state) = state.as_ref() {
                let sortitions = {
                    let mut executor = executor.lock().await;
                    // On Bitcoin's clock rather than execution's: a node at the chain
                    // tip with nothing staged derives no burn view otherwise, and
                    // cannot then describe the one its own tip stands on.
                    executor.follow_burnchain(&pox);
                    executor.derived_sortitions()
                };
                state.publish_executed(sealed, sortitions).await;
                publish_reward_cycle(RewardCycleInputs {
                    state,
                    executor,
                    network,
                    context: bitcoin_context(&config, &pox),
                    winners: &last_sortition_winners(rounds.node.view().as_ref()),
                    published: &mut rounds.published,
                    peer: &rounds.peer,
                    registry: config.node.pox_5_sbtc_registry_contract.as_deref(),
                    checkpoint: &config.checkpoint,
                })
                .await;
            }
        }
        sleep(interval).await;
    }
}

/// What a follower does once, before its first round.
///
/// Both halves are the executor's and neither belongs in the loop: a sortition
/// chain is seeded from the checkpoint once, and the ancestor headers a state was
/// written without are written down once. Extracted so the loop below is the loop.
async fn prepare_to_follow(
    executor: Option<&SharedExecutor>,
    config: &Config,
    peer: &SyncClient,
    pox: &PoxInfo,
    source: [u8; 32],
) {
    let Some(executor) = executor else {
        return;
    };
    // Derive sortitions alongside the peer's answers, when the checkpoint
    // carries the history that makes it possible.
    if let Some(directory) = config.checkpoint.sortition.as_ref() {
        let phase = Phase::start("seeding the local sortition chain");
        start_deriving_sortitions(executor, directory, &config.node.working_dir).await;
        drop(phase);
    }
    backfill_ancestors(executor, peer, pox, source).await;
}

/// Take the blocks the public API admitted and stage them.
///
/// Nothing is validated here on purpose: the route already put each one through
/// `ChainState::authenticate_block`, and the executor checks its state root when
/// it runs it. Draining the channel rather than awaiting it keeps this on the
/// round's own clock — an upload is visible within one poll interval, and a burst
/// of them cannot starve the peer.
///
/// Having passed that boundary, an uploaded block is also relayed. A node that
/// accepted a block and told nobody is a hole in the network's propagation, and the
/// only thing that made these blocks special — that they arrived over HTTP — stops
/// being true the moment they are authenticated.
fn stage_admitted_blocks(
    offered: &mut tokio::sync::mpsc::UnboundedReceiver<NakamotoBlock>,
    staging: &Staging,
    relay: &nano_p2p::Relay,
) {
    while let Ok(block) = offered.try_recv() {
        match staging.put(&block) {
            Ok(()) => println!(
                "admitted block {} at height {} over the public API",
                block.block_id(),
                block.header.chain_length
            ),
            Err(error) => eprintln!(
                "staging the admitted block {} failed: {error}",
                block.block_id()
            ),
        }
        relay.announce(nano_p2p::Offer::block(None, block));
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
    submitted: &mut tokio::sync::mpsc::UnboundedReceiver<nano_codec::Transaction>,
    relay: &nano_p2p::Relay,
) {
    while let Ok(transaction) = submitted.try_recv() {
        println!("relaying the transaction {} this node admitted", transaction.txid());
        relay.announce(nano_p2p::Offer::transaction(None, Box::new(transaction)));
    }
}

/// Put everything peers pushed through this node's own checks, and relay what
/// passes.
///
/// This is the boundary the whole of task 054's relay item turns on, and the reason
/// it is *here* is that here is where the chainstate is.
/// `ChainState::authenticate_block` enforces the signer weight against the reward set
/// nano derived, the miner signature against the sortition winner, the coinbase VRF
/// proof, the seed the winning commit committed to, and the header's cumulative burn
/// against nano's own burnchain — all before any of the block runs. A block that
/// passes it is staged, and from that point it is indistinguishable from one this node
/// fetched itself; a block that fails it is dropped, and the state root check at
/// execution is still in front of everything that survives.
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
            let (from, block) = match offer.data {
                nano_p2p::Pushed::Block(block) => (offer.from, block),
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
                    if let Err(error) = staging.put(&block) {
                        eprintln!("staging a relayed block failed: {error}");
                        continue;
                    }
                    accepted += 1;
                    relay.announce(nano_p2p::Offer::block(from, *block));
                }
                Err(error) => {
                    rejected += 1;
                    eprintln!(
                        "a pushed block {} at height {} did not authenticate: {error}",
                        block.block_id(),
                        block.header.chain_length
                    );
                }
            }
        }
    }
    let admitted = admit_relayed(executor, mempool, relay, transactions).await;
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
    transactions: Vec<(Option<nano_primitives::Hash160>, Box<nano_codec::Transaction>)>,
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
    for (from, transaction) in transactions {
        let admission = mempool.submit((*transaction).clone(), &accounts, now);
        if matches!(
            admission,
            Ok(nano_mempool::Admission::Added | nano_mempool::Admission::Replaced(_))
        ) {
            kept.push((from, transaction));
        }
    }
    drop(accounts);
    drop(executor);
    drop(mempool);
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

/// Which reward cycle this node has already answered for, so a walk of the
/// pox-5 signer list happens once per cycle instead of once per round.
#[derive(Default)]
struct RewardCyclePublication {
    served: Option<u64>,
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

/// What publishing a cycle reads, as one value for the same reason
/// [`RoundInputs`] is one: eight positional arguments hide which is which.
struct RewardCycleInputs<'a> {
    state: &'a RpcState,
    executor: &'a SharedExecutor,
    network: Network,
    context: nano_chainstate::BitcoinBlockContext,
    /// Who won the recent sortitions, which is who may write to `.miners`.
    winners: &'a [nano_primitives::Hash160],
    published: &'a mut RewardCyclePublication,
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
    if published.served != Some(cycle) {
        println!(
            "serving the reward set the checkpoint carried for cycle {cycle}, which was \
             stacked before this node's history begins and cannot be derived from its state"
        );
    }
    state.publish_stacker_set(cycle, stacker_set).await;
    published.served = Some(cycle);
    true
}

/// Publish the reward set the executed state derives, and configure the
/// `StackerDB` contracts that cycle's signers write to.
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
        winners,
        published,
        peer,
        registry,
        checkpoint,
    } = inputs;
    context.move_to_burn_block(executor.lock().await.bitcoin_height());
    let Some(cycle) = nano_chainstate::signers::reward_cycle_at(context) else {
        return;
    };
    configure_miner_slots(state, network, cycle, winners, published, peer).await;
    if published.served == Some(cycle) {
        return;
    }
    // The lock is held only for the walk: it is the same lock every account read
    // takes, and the walk is one contract call per signer.
    let derived = nano_chainstate::signers::active_signer_set(
        executor.lock().await.chainstate_mut().vm_mut(),
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
        Err(_) if carry_checkpoint_set(state, published, checkpoint, cycle, context).await => {
            return;
        }
        Err(error) => {
            if published.complained != Some(cycle) {
                published.complained = Some(cycle);
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
    let payout = executor
        .lock()
        .await
        .chainstate_mut()
        .sbtc_payout_address(registry);
    let sbtc_address = match payout {
        Ok(address) => Some(address),
        Err(error) => {
            if published.complained != Some(cycle) {
                published.complained = Some(cycle);
                eprintln!(
                    "this node cannot derive the waterfall payout address from its own sBTC \
                     registry state, so /v3/stacker_set/{cycle} carries the version 0 shape \
                     without it: {error}"
                );
            }
            None
        }
    };
    state
        .publish_stacker_set(
            cycle,
            nano_rpc::stacker_set_payload(
                &entries,
                derived.pox_ustx_threshold,
                sbtc_address.as_ref(),
            ),
        )
        .await;
    let writers: Vec<nano_primitives::Hash160> = entries
        .iter()
        .map(|entry| nano_primitives::hash160(&entry.signing_key))
        .collect();
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
    published.served = Some(cycle);
    println!(
        "derived the reward set for cycle {cycle} from this node's own state: {} signers, \
         {} of weight, replicating their StackerDB contracts",
        entries.len(),
        entries.iter().map(|entry| u64::from(entry.weight)).sum::<u64>()
    );
}

/// The block-signing keys that won the most recent sortitions, newest first.
///
/// Whose commitment won a burn block needs the burn distribution, which nano
/// cannot derive ([[049-derive-sortitions-locally]]), so this is the peer's
/// answer — and it is used for nothing but naming who may write a `StackerDB`
/// slot, which is replication rather than consensus.
fn last_sortition_winners(view: Option<&nano_sync::NodeView>) -> Vec<nano_primitives::Hash160> {
    let mut winners = Vec::new();
    let Some(view) = view else {
        return winners;
    };
    for tenure in view.tenures.iter().rev() {
        if !tenure.sortition.was_sortition {
            continue;
        }
        if let Some(hash) = tenure.sortition.miner_public_key_hash {
            winners.push(hash);
        }
        if winners.len() == MINER_SLOT_CANDIDATES {
            break;
        }
    }
    winners
}

/// How many recent sortition winners a `.miners` slot may be attributed to.
///
/// Two would be the answer if both slots were always rewritten every tenure, and
/// they are not: a slot keeps the last chunk its owner wrote, which can be several
/// tenures old. So the candidates are the recent winners rather than the last two,
/// and every attribution is still checked against a signature.
const MINER_SLOT_CANDIDATES: usize = 8;

/// Replicate `.miners`, so a signer hosted here can read what a miner proposed.
///
/// The two slots belong to the last two sortition winners, and which winner gets
/// which is `num_sortitions % 2` in stacks-core — a count over the whole
/// burnchain that a checkpointed node has never made and no snapshot nano holds
/// carries. A `.miners` replica with its two slots swapped refuses the very chunks
/// it exists for, so the count has to come from somewhere.
///
/// It comes from the chunks. Every slot's metadata is signed by the writer that
/// owns it, so asking the peer for its `.miners` listing and recovering each
/// signature says which winner holds which slot — and says it in a form this node
/// checks rather than believes: the recovered key has to be one of the two winners
/// this node saw win. A peer that lies is a peer whose chunks stop verifying.
///
/// Only the winners are needed a priori, and where those come from is unchanged.
async fn configure_miner_slots(
    state: &RpcState,
    network: Network,
    cycle: u64,
    winners: &[nano_primitives::Hash160],
    published: &mut RewardCyclePublication,
    peer: &SyncClient,
) {
    let Some(&latest) = winners.first() else {
        return;
    };
    let contract = crate::config::miner_contract(network);
    let previous = winners.get(1).copied().unwrap_or(latest);
    let assignment = if previous == latest {
        // One miner, so no order to get wrong.
        Some(vec![latest, latest])
    } else {
        miner_slots(peer, &contract, winners).await
    };
    let Some(assignment) = assignment else {
        if published.ambiguous_miners != Some(cycle) {
            published.ambiguous_miners = Some(cycle);
            eprintln!(
                "the recent sortitions were won by more than one miner and this node \
                 cannot attribute both .miners slots to one of them from the chunks they \
                 hold, so it replicates neither: a slot assigned to the wrong writer \
                 refuses the proposals it exists for"
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

/// Who owns each `.miners` slot, read off the peer's own listing.
///
/// Each slot's metadata is signed by the writer that owns it, so recovering the
/// signature says who that is — checked against the miners this node saw win a
/// sortition, so a peer naming a stranger gets nothing configured.
///
/// **Both slots have to resolve.** A slot nano assigns to the wrong writer refuses
/// the very chunks it exists for, and a `.miners` replica that refuses proposals is
/// worse than one that has none: the first looks configured. Guessing the second
/// slot from the first is exactly that mistake, and it is what left a hosted signer
/// with no proposals to answer while the log said `.miners` was replicated.
async fn miner_slots(
    peer: &SyncClient,
    contract: &nano_stackerdb::StackerDbContract,
    winners: &[nano_primitives::Hash160],
) -> Option<Vec<nano_primitives::Hash160>> {
    let client = nano_stackerdb::StackerDbClient::new(peer.base_url().clone()).ok()?;
    let listing = client.slot_metadata(contract).await.ok()?;
    let (mut first, mut second) = (None, None);
    for metadata in listing {
        if metadata.slot_version == 0 {
            continue;
        }
        for writer in winners {
            if !metadata.verify(*writer).unwrap_or(false) {
                continue;
            }
            match metadata.slot_id {
                0 => first = Some(*writer),
                1 => second = Some(*writer),
                _ => {}
            }
        }
    }
    match (first, second) {
        (Some(first), Some(second)) if first != second => Some(vec![first, second]),
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
pub async fn open_executor(
    config: &Config,
    network: Network,
    pox: &PoxInfo,
    peers: &mut TenureSource,
    directory: &Path,
) -> Result<CheckpointExecutor<BurnchainSource>, Box<dyn Error>> {
    let (chainstate, anchor, context) =
        open_chainstate(config, network, pox, peers, directory).await?;
    let bitcoin = bitcoin_source(config)?;
    match context {
        Some(context) => Ok(CheckpointExecutor::from_chainstate(
            chainstate, anchor, context, bitcoin,
        )?),
        None => Ok(CheckpointExecutor::resume(chainstate, anchor, bitcoin)),
    }
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

    let Some(tip) = chainstate.tip().filter(|tip| *tip != source) else {
        // Nothing has been sealed here, so there is no ledger to recover: the
        // first tenures a node executes pay out rewards earned before it
        // existed, and only the checkpoint knows them.
        *chainstate.accounting_mut() = accounting(config, directory)?;
        let anchor = NakamotoBlock::decode(&fs::read(&config.checkpoint.anchor_block)?)?;
        let mut context = bitcoin_context(config, pox);
        context.move_to_burn_block(config.checkpoint.anchor_bitcoin_height);
        return Ok((chainstate, anchor, Some(context)));
    };
    let tip = deepest_block_a_ledger_names(&chainstate, tip, config.node.max_sync_blocks);
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
        let Some(parent) = chainstate.parent_of(walk) else {
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
    recover_ledger(&mut chainstate, config, directory, &tip)?;
    Ok((chainstate, tip, None))
}

/// The deepest sealed state at or below `tip` that a ledger names.
///
/// The deepest sealed state is not always a block this node executed. A block is
/// committed by writing its ledger and *then* sealing the MARF, so a state whose
/// ledger is gone is one nothing points at — and standing on it costs the whole
/// recovery: no reorganization reach, no tenure start heights, no parent tenure
/// proof, and a maturity window read back from `accounting.json` instead of from
/// the tip, which a mainnet node then refuses to start on.
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
fn deepest_block_a_ledger_names(chainstate: &ChainState, tip: [u8; 32], reach: usize) -> [u8; 32] {
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
            return walk;
        }
        let Some(parent) = chainstate.parent_of(walk) else {
            break;
        };
        walk = parent;
    }
    // Nothing within reach has one, so the caller falls back to `accounting.json`
    // and says what that costs. Reported here too, because the seal it is about to
    // resume at is not the reason -- the missing ledgers are.
    eprintln!(
        "no sealed state at or within {reach} blocks below {} has a ledger, so this run \
         cannot stand on one",
        hex::encode(tip)
    );
    tip
}

/// Stand on the state the run that sealed this block kept beside the MARF.
///
/// Recovered for the block this node is *resuming at*, which is not always the
/// deepest one it sealed: a tip that lost a fork race while the node was down is
/// abandoned for an ancestor, and the ledger has to be that ancestor's.
fn recover_ledger(
    chainstate: &mut ChainState,
    config: &Config,
    directory: &Path,
    tip: &NakamotoBlock,
) -> Result<(), Box<dyn Error>> {
    if chainstate.recover_ledger_at(*tip.block_id().as_bytes())? {
        // Named field by field, because each one is a thing this node can do that
        // a run without it silently could not: walk a reorganization back, answer
        // `get-tenure-info?` for the tenure in flight, check the seed the next
        // tenure commits.
        let tenure = chainstate
            .recorded_header(*tip.block_id().as_bytes())
            .map(|header| header.tenure_height);
        println!(
            "recovered the ledger committed with block {}: {} executed blocks to walk back \
             over, tenure {} starting at height {}, parent tenure proof {}",
            tip.block_id(),
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
    // A state directory written before the ledger was committed with the seal
    // has none, and the three things beside the accounting were never written
    // anywhere at all. So say exactly what this run cannot do: it owes what the
    // last catch-up *round* wrote rather than what its tip owes, it cannot walk
    // a reorganization back past this restart, it will report the first block of
    // the tenure in flight as that tenure's start height, and it cannot check
    // the seed the next tenure commits. The first block this run seals writes a
    // ledger, so the restart after it is whole.
    eprintln!(
        "no ledger was committed with block {}, so this run resumes from \
         {ACCOUNTING_FILE} — which is written per catch-up round and may be behind the \
         tip — with no reorganization reach, no tenure start heights and no parent \
         tenure proof. The next block sealed writes one.",
        tip.block_id()
    );
    *chainstate.accounting_mut() = accounting(config, directory)?;
    Ok(())
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
    /// Where a block admitted over the public API is handed to the executor,
    /// drained by the follow loop into the same staging store the peer's blocks
    /// land in — so an upload and a followed block are the same thing from the
    /// moment they are authenticated.
    blocks: tokio::sync::mpsc::UnboundedSender<NakamotoBlock>,
    proposals: tokio::sync::mpsc::UnboundedSender<nano_rpc::ProposalRequest>,
    chunks: tokio::sync::mpsc::UnboundedSender<(String, nano_stackerdb::Chunk)>,
    submitted: tokio::sync::mpsc::UnboundedSender<nano_codec::Transaction>,
}

/// The far ends of the channels the follow loop drains.
struct FollowedChannels {
    offered: tokio::sync::mpsc::UnboundedReceiver<NakamotoBlock>,
    submitted: tokio::sync::mpsc::UnboundedReceiver<nano_codec::Transaction>,
}

/// The far ends of the channels the hosting role drains.
struct HostedChannels {
    proposed: tokio::sync::mpsc::UnboundedReceiver<nano_rpc::ProposalRequest>,
    written: tokio::sync::mpsc::UnboundedReceiver<(String, nano_stackerdb::Chunk)>,
}

impl ApiWiring {
    /// Build the wiring, handing back the ends that belong to other roles.
    fn new(
        executor: Option<SharedExecutor>,
        mempool: Arc<Mutex<nano_mempool::Mempool>>,
        archive: Option<Arc<crate::archive::Archive>>,
    ) -> (Self, FollowedChannels, HostedChannels) {
        let (blocks, offered) = tokio::sync::mpsc::unbounded_channel();
        let (proposals, proposed) = tokio::sync::mpsc::unbounded_channel();
        let (chunks, written) = tokio::sync::mpsc::unbounded_channel();
        let (relayed, submitted) = tokio::sync::mpsc::unbounded_channel();
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
    roles: &mut JoinSet<(Job, Result<(), String>)>,
) -> Result<Option<RpcState>, Box<dyn Error>> {
    let ApiWiring {
        executor,
        mempool,
        archive,
        blocks,
        proposals,
        chunks,
        submitted,
    } = wiring;
    let Some(address) = config.node.rpc_bind else {
        return Ok(None);
    };
    let mut state = RpcState::new(network)
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
    // Only when there is somewhere for them to go: `/v3/block_proposal` refuses a
    // proposal it could not report the verdict on, as stacks-core does.
    if !dispatcher.is_empty() {
        state = state.with_observers(dispatcher.clone());
    }
    if let Some(token) = config.node.block_proposal_token.clone() {
        state = state.with_proposal_token(token);
    }
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
    Ok(Some(state))
}

/// Mine the tenures this node wins, if it is configured to mine at all.
fn start_miner(
    config: &Config,
    network: Network,
    pox: &PoxInfo,
    peer: &SyncClient,
    chain: (Option<SharedExecutor>, Arc<Mutex<nano_mempool::Mempool>>),
    announce: (EventDispatcher, nano_p2p::Relay),
    roles: &mut JoinSet<(Job, Role)>,
) {
    let (executor, mempool) = chain;
    let (dispatcher, relay) = announce;
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
    };
    roles.spawn(async move { (Job::Miner, miner::run(runtime).await) });
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
    let (running, found) = (config.clone(), discovered.cloned());
    roles.spawn(async move {
        (
            Job::Signer,
            signer::run(running, signer, network, found, pool, validator).await,
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
                state,
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
                proposed,
            )
            .await,
        )
    });
    Ok(())
}

/// Send the blocks this node executes to the configured observers.
///
/// An observer wants what a node *executed*, which only the executor knows it
/// has: everything else in the runtime sees blocks that have merely been
/// downloaded.
async fn announce_executed_blocks(executor: Option<&SharedExecutor>, dispatcher: &EventDispatcher) {
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
    let fetched = async {
        let header = peer.block(id).await?.header;
        let sortition = peer.sortition(header.consensus_hash).await?;
        Ok::<_, nano_sync::SyncError>((header, sortition))
    }
    .await;
    let Ok((header, sortition)) = fetched else {
        eprintln!("cannot fetch the header of {}", hex::encode(block));
        return;
    };
    let Ok(burn_block_height) = u32::try_from(sortition.bitcoin_height) else {
        return;
    };
    if let Err(error) = executor.chainstate.backfill_ancestor_header(
        block,
        *sortition.bitcoin_block_hash.as_bytes(),
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

/// Derive sortitions alongside the peer's answers, when the checkpoint carries
/// the history that makes it possible.
async fn start_deriving_sortitions(executor: &SharedExecutor, capture: &Path, state: &Path) {
    // No `PoxId` is passed: the seed's own sortition identifier states the bit
    // vector it was produced under, so the tracker reads it off the checkpoint.
    match SortitionTracker::resume_or_capture(state, capture) {
        Ok(tracker) => {
            println!(
                "deriving sortitions locally from burn {} on PoX history {}",
                tracker.tip().bitcoin_height,
                tracker.tip().pox_id
            );
            executor
                .lock()
                .await
                .track_sortitions(tracker, state.to_path_buf());
        }
        Err(error) => eprintln!("cannot derive sortitions locally: {error}"),
    }
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
    if manifest.source_state_id != source {
        return Err(format!(
            "the checkpoint names state {} where this node is configured for {}",
            hex::encode(manifest.source_state_id),
            hex::encode(source)
        )
        .into());
    }
    if let Some(recorded) = CheckpointProvenance::load(directory)? {
        already_adopted(recorded.checkpoint.source_state_id, manifest.source_state_id)?;
        return Ok(());
    }

    let (Some(block), Some(reward_set)) = (
        config.checkpoint.attesting_block.as_ref(),
        config.checkpoint.attesting_reward_set.as_ref(),
    ) else {
        return Err("a checkpoint needs an attesting block and the reward set that \
                    signed it before it can be imported"
            .into());
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
                weight: u32::try_from(
                    entry["weight"].as_u64().ok_or("a signer has no weight")?,
                )?,
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
    claiming: Vec<String>,
}

impl BulkHistory {
    fn new(peer: SyncClient) -> Self {
        Self {
            source: TenureSource::only(peer),
            endpoints: Vec::new(),
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
                true
            }
        }
    }
}

/// The Bitcoin view this node advertises to its peers.
///
/// Derived from the node's own executed height and its own Bitcoin source, never
/// from what a peer said: a preamble view is a gossip hint rather than a consensus
/// input, but a node repeating a peer's claim back at the network would be laundering
/// it into one.
///
/// The fallback matters as much as the derivation. A peer refuses a message whose
/// *stable* header hash contradicts its own at that height, and it keeps roughly 288
/// blocks below its own stable height — so a view older than that cannot be
/// contradicted, and stacks-core reads not-contradictable as merely stale. A node
/// with no executed chain yet advertises exactly that and gets in, which is what
/// lets discovery run before there is a chain to describe.
fn advertised_view(
    bitcoin: &BurnchainSource,
    published: Option<&LocalAnnouncement>,
) -> nano_p2p::ChainView {
    let stale = || {
        nano_p2p::ChainView::new(
            100_000,
            nano_primitives::BitcoinHeaderHash::from_bytes([0; 32]),
            nano_primitives::BitcoinHeaderHash::from_bytes([0; 32]),
        )
        .expect("a height above the confirmation window")
    };
    let Some(height) = published.map(|announced| announced.bitcoin_height) else {
        return stale();
    };
    let Some(settled) = height.checked_sub(nano_p2p::STABLE_CONFIRMATIONS) else {
        return stale();
    };
    let (Ok(tip_hash), Ok(stable_hash)) = (
        nano_bitcoin::BitcoinSource::block_hash_at(bitcoin, height),
        nano_bitcoin::BitcoinSource::block_hash_at(bitcoin, settled),
    ) else {
        return stale();
    };
    nano_p2p::ChainView::new(
        height,
        nano_primitives::BitcoinHeaderHash::from_bytes(tip_hash),
        nano_primitives::BitcoinHeaderHash::from_bytes(stable_hash),
    )
    .unwrap_or_else(stale)
}

/// What this node tells its peers about itself, written by the loop that knows it.
///
/// The transport comes up *before* there is a chainstate — that is its whole point,
/// since it is the way in that does not depend on a configured HTTP peer — so the
/// discovery loop cannot read an executor that is built after it. This is the handle
/// the follow loop writes each round and discovery reads: how far this node's own
/// burnchain view has got, and which reward cycle to ask peers' inventories about.
///
/// Both are *this node's* answers. A preamble view is a gossip hint, but repeating a
/// peer's claim back at the network is how a hint becomes a consensus input, and a
/// cycle identifier taken from a peer would make its view of the burnchain the thing
/// nano's own requests are keyed on.
#[derive(Clone, Default)]
pub struct Advertised {
    inner: Arc<std::sync::Mutex<Option<LocalAnnouncement>>>,
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
    /// The Bitcoin height the sealed tip was executed under.
    bitcoin_height: u64,
    /// The consensus hash naming the reward cycle being walked, when derivable.
    cycle_start: Option<nano_primitives::ConsensusHash>,
    /// The burn height the cycle opens at, the hash naming it, and which of its
    /// tenures this node has executed and so will serve.
    inventory: Option<(u64, nano_primitives::ConsensusHash, nano_primitives::BitVec<2100>)>,
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
            served,
        }
    }

    fn publish(&self, announcement: LocalAnnouncement) {
        // Recorded before the snapshot so that the durable answer is never behind the
        // live one: a peer reading between the two would otherwise be told about a
        // tenure whose bit had not been written down yet.
        if let (Some(served), Some((cycle_height, cycle_start, tenures))) =
            (self.served.as_ref(), announcement.inventory.as_ref())
            && let Ok(served) = served.lock()
            && let Err(error) = served.record(*cycle_height, *cycle_start, tenures)
        {
            eprintln!("cannot record the tenures this node serves: {error}");
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
        let (_, known, tenures) = self.read()?.inventory?;
        (known == cycle_start).then_some(tenures)
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

/// Join the binary p2p network: seed the peer table, discover peers, and answer
/// the ones that dial us.
///
/// Returns the handle the rest of the node reads endpoints from, or `None` when
/// there is no way in — no seeds, or a configuration that leaves the chain
/// identifier to be discovered, which cannot work here because on this protocol
/// the network id *is* the chain id and it is in the first field of the first
/// message.
async fn start_transport(
    config: &Config,
    network: Network,
    advertised: &Advertised,
    relay: &nano_p2p::Relay,
    roles: &mut JoinSet<(Job, Role)>,
) -> Option<Discovered> {
    let seeds = config.node.bootstrap_seeds();
    if seeds.is_empty() && config.node.p2p_bind.is_none() {
        return None;
    }
    let protocol = nano_p2p::Protocol::for_network(network);
    let identity = match p2p_identity(&config.node.working_dir) {
        Ok(identity) => identity,
        Err(error) => {
            eprintln!("cannot establish a p2p identity: {error}");
            return None;
        }
    };
    let bind = config.node.p2p_bind;
    let advertise = config.node.p2p_address.or(bind);
    let mut local = nano_p2p::LocalPeer::quiet(
        identity,
        advertise.map_or(20444, |address| address.port()),
    );
    if let Some(address) = advertise
        && !address.ip().is_unspecified()
    {
        local.address = nano_p2p::PeerAddress::from_ip(address.ip());
    }
    // Only claim to serve what this node actually serves. A peer that records an
    // RPC endpoint here and finds nothing listening has spent a connection slot on
    // us, and will stop offering us to its own neighbours.
    if let Some(rpc) = config.node.rpc_bind
        && !rpc.ip().is_unspecified()
    {
        local.data_url = format!("http://{rpc}");
        local.services |= nano_p2p::wire::services::RPC;
    }
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
        }),
        Err(error) => {
            eprintln!("cannot open the peer table for serving: {error}");
            return None;
        }
    };
    let mut swarm = nano_p2p::Swarm::new(peers, local, protocol, nano_p2p::SwarmLimits::default())
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
    let round = swarm
        .maintain(advertised_view(&bitcoin, advertised.read().as_ref()), None)
        .await;
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
    roles.spawn(async move {
        (
            Job::Peers,
            peer_discovery(swarm, bitcoin, advertised, relay, tick).await,
        )
    });

    if let Some(bind) = bind {
        start_listener(config, network, bind, service, roles);
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
    tick: Duration,
) -> Role {
    let mut ticks = 0_u64;
    loop {
        sleep(tick).await;
        ticks = ticks.wrapping_add(1);
        let published = advertised.read();
        let view = advertised_view(&bitcoin, published.as_ref());
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
    roles: &mut JoinSet<(Job, Role)>,
) {
    let Ok(identity) = p2p_identity(&config.node.working_dir) else {
        return;
    };
    let protocol = nano_p2p::Protocol::for_network(network);
    let advertise = config.node.p2p_address.unwrap_or(bind);
    let mut local = nano_p2p::LocalPeer::quiet(identity, advertise.port());
    if !advertise.ip().is_unspecified() {
        local.address = nano_p2p::PeerAddress::from_ip(advertise.ip());
    }
    if let Some(rpc) = config.node.rpc_bind
        && !rpc.ip().is_unspecified()
    {
        local.data_url = format!("http://{rpc}");
        local.services |= nano_p2p::wire::services::RPC;
    }
    roles.spawn(async move {
        (
            Job::Peers,
            answer_peers(bind, local, protocol, service).await,
        )
    });
}

/// Answer inbound peers until the socket fails.
async fn answer_peers(
    bind: std::net::SocketAddr,
    local: nano_p2p::LocalPeer,
    protocol: nano_p2p::Protocol,
    service: Arc<PeerService>,
) -> Role {
    {
        let listener = nano_p2p::Listener::bind(bind)
            .await
            .map_err(|error| format!("cannot listen for peers on {bind}: {error}"))?;
        println!("p2p: listening for peers on {bind}");
        let mut conversations: JoinSet<()> = JoinSet::new();
        loop {
            let (stream, from) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    eprintln!("accepting a peer failed: {error}");
                    continue;
                }
            };
            // Bounded, so that a flood of connections cannot become a flood of
            // tasks. Beyond the cap the oldest finished conversations are reaped
            // first, and if none has, the connection waits its turn.
            while conversations.len() >= MAX_INBOUND_PEERS {
                let _ = conversations.join_next().await;
            }
            let service = service.clone();
            let local = local.clone();
            conversations.spawn(async move {
                if let Err(error) = nano_p2p::serve_peer(
                    stream,
                    from,
                    &local,
                    protocol,
                    service.as_ref(),
                    nano_p2p::InboundLimits::default(),
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

/// What this node tells a peer that dialled it.
struct PeerService {
    peers: std::sync::Mutex<nano_p2p::PeerDb>,
    /// What the follow loop last published about this node's own chain.
    advertised: Advertised,
    /// Where a pushed block or transaction goes: onto a bounded queue, and nowhere
    /// near a decision. This runs on the listener's task and has no chainstate, so
    /// the most it can honestly do is write down who said what.
    relay: nano_p2p::Relay,
}

impl nano_p2p::Service for PeerService {
    fn chain_view(&self) -> nano_p2p::ChainView {
        // The stale view is what a node with no executed chain says, and it is safe
        // rather than a placeholder: a peer keeps only about 288 blocks below its
        // stable height, so a claim older than that is uncontradictable rather than
        // wrong. Once there is a chain, the height is this node's own.
        //
        // Deliberately not derived from a Bitcoin fetch: this trait is synchronous
        // because an inbound reply that can block on I/O is one that can stall the
        // listener, so the honest tip *hash* is not reachable from here and the height
        // alone would be a view a peer could contradict. That is why the whole thing
        // stays the stale one for now, and why the swarm — which can await — is where
        // the real view is advertised.
        nano_p2p::ChainView::new(
            100_000,
            nano_primitives::BitcoinHeaderHash::from_bytes([0; 32]),
            nano_primitives::BitcoinHeaderHash::from_bytes([0; 32]),
        )
        .expect("a height above the confirmation window")
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
            staged: 9,
            scheduled: 0,
            rate_limited,
        };

        let moved = super::round_report(1_100, &round(100, false), &tip);
        assert_eq!(
            moved,
            format!(
                "executed 100 blocks, 1100 to 1200, state root {}, 9 staged, 40 fetched",
                tip.header.state_index_root
            )
        );

        // A round that executed nothing has no root to name and must not be
        // mistakable for one that did.
        let still = super::round_report(1_100, &round(0, false), &tip);
        assert_eq!(still, "executed nothing: sealed at 1100, 9 staged, 40 fetched");
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
        };
        let mut context = nano_chainstate::BitcoinBlockContext::at_height(961_300);
        context.first_height = 666_050;
        context.prepare_phase_length = 100;
        context.reward_phase_length = 2_000;

        let state = super::RpcState::new(super::Network::MAINNET);
        let mut published = super::RewardCyclePublication::default();
        assert!(
            super::carry_checkpoint_set(&state, &mut published, &checkpoint, 140, context).await,
            "the checkpoint's own cycle is answered"
        );
        assert_eq!(published.served, Some(140));
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
        assert!(refused.to_string().contains("tenures 50 to 60"), "{refused}");
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
            signers.signers().iter().map(|signer| signer.weight).sum::<u32>(),
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
    /// no reorganization reach, no tenure start heights, no parent tenure proof and
    /// a maturity window read from `accounting.json`, which a mainnet node then
    /// refuses to start on — and it also hides the residue from the give-back,
    /// because there is nothing above the seal to give back.
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

        assert_eq!(chainstate.tip(), Some(block(5)), "the deepest seal");
        assert_eq!(
            super::deepest_block_a_ledger_names(&chainstate, block(5), 500),
            block(3),
            "and the deepest block a ledger names is two below it"
        );
        // Which is what makes the give-back reach them at all.
        let height = chainstate.height_of(block(3)).expect("a sealed height");
        assert_eq!(chainstate.discard_above(height).expect("give back"), 2);
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
            super::deepest_block_a_ledger_names(&chainstate, [2; 32], 1),
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
