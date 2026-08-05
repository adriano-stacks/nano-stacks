//! Starting a node: open the state, pick a peer, run the configured roles.
//!
//! Everything a role needs is derived here, once, so that following, signing
//! and mining are three tasks over one configuration rather than three
//! programs over three command lines.

use std::{error::Error, fs, future::Future, path::Path, sync::Arc, time::Duration};

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
const SIGNER_MESSAGE_IDS: [u32; 3] = [1, 2, 3];

/// A job the node runs, and what its stopping means for the rest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Job {
    Rpc,
    Follower,
    Signer,
    Miner,
    /// The p2p transport: peer discovery, and the listener that answers peers.
    Peers,
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
            Self::Rpc | Self::Miner | Self::Peers => false,
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

/// Run a node until it is asked to stop or a role gives up.
pub async fn run(config: Config) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(&config.node.working_dir)?;
    let mut roles: JoinSet<(Job, Role)> = JoinSet::new();
    // Written by the follow loop once there is a chain to describe, and read by the
    // discovery loop that starts before there is one.
    let advertised = Advertised::default();
    // The p2p transport comes up first, because its whole point is to be a way in
    // that does not depend on a configured HTTP peer. It needs the chain identifier
    // up front — on this protocol the network id *is* the chain id and it is the
    // second field of the first message — so a configuration that leaves the chain
    // to be discovered gets no transport, and falls back to what it always did.
    let phase = Phase::start("joining the peer network");
    let discovered = match config.network() {
        Some(network) => start_transport(&config, network, &advertised, &mut roles).await,
        None => None,
    };
    drop(phase);
    let phase = Phase::start("reaching a peer");
    let peer = reachable_peer(&config, discovered.as_ref()).await?;
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
    let executor = if config.node.rpc_bind.is_some() || config.miner.is_some() {
        let phase = Phase::start("opening the executed state");
        let executor = open_executor(
            &config,
            network,
            &pox,
            &peer,
            &config.chainstate_dir(NODE_CHAINSTATE),
        )
        .await?;
        drop(phase);
        Some(Arc::new(Mutex::new(executor)))
    } else {
        None
    };
    let dispatcher = EventDispatcher::new(config.node.event_observers()?);
    let phase = Phase::start("announcing the blocks already executed");
    announce_executed_blocks(executor.as_ref(), &dispatcher).await;
    drop(phase);
    // One mempool, shared: a node whose RPC admits transactions into a pool the
    // miner cannot see accepts them and never mines them, which is worse than
    // refusing them.
    let mempool = Arc::new(Mutex::new(nano_mempool::Mempool::new(network)));
    // Where a block admitted over the public API is handed to the executor. One
    // channel, drained by the follow loop into the same staging store the peer's
    // blocks land in, so an upload and a followed block are the same thing from
    // the moment they are authenticated.
    let (blocks, offered) = tokio::sync::mpsc::unbounded_channel();

    let state = start_rpc(
        &config,
        network,
        executor.clone(),
        mempool.clone(),
        &dispatcher,
        blocks,
        &mut roles,
    )
    .await?;
    publish_sealed_tip(state.as_ref(), executor.as_ref()).await;
    // The miner executes the chain itself, because it has to build on its own
    // blocks the moment it makes them; the follower then only keeps the served
    // view fresh.
    let executing_follower = config.miner.is_none();
    if let (Some(miner), Some(executor)) = (config.miner.clone(), executor.clone()) {
        let runtime = miner::Runtime {
            config: config.clone(),
            miner,
            network,
            pox: pox.clone(),
            peer: peer.clone(),
            executor,
            dispatcher,
            mempool: mempool.clone(),
        };
        roles.spawn(async move { (Job::Miner, miner::run(runtime).await) });
    }
    start_signer(&config, network, &pox, &peer, &mut roles).await?;
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
            pox,
            source,
            state,
            executor,
            offered,
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

/// Publish what this node is sealed at before it follows anything, so a node
/// that never manages to execute reports the height it is really on rather than
/// nothing at all.
async fn publish_sealed_tip(state: Option<&RpcState>, executor: Option<&SharedExecutor>) {
    if let (Some(state), Some(executor)) = (state, executor) {
        let sealed = {
            let executor = executor.lock().await;
            sealed_tip(executor.tip(), executor.bitcoin_height())
        };
        state.publish_executed(sealed).await;
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
        consensus_hash: tip.header.consensus_hash,
        bitcoin_height,
        state_index_root: tip.header.state_index_root,
    }
}

/// Say what a round of catching up actually did.
///
/// A round that executed nothing reads exactly like one that executed a
/// thousand blocks unless it says so, which is how a node that had never
/// executed a single block past its checkpoint looked healthy for hours.
fn report_round(from: u64, round: CatchUpRound, tip: &NakamotoBlock) {
    let limited = if round.rate_limited {
        ", peer rate limiting"
    } else {
        ""
    };
    if round.executed == 0 {
        println!(
            "executed nothing: sealed at {from}, {} staged, {} fetched{limited}",
            round.staged, round.fetched
        );
    } else {
        println!(
            "executed {} blocks, {from} to {}, {} staged, state root {}{limited}",
            round.executed, tip.header.chain_length, round.staged, tip.header.state_index_root
        );
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

/// Run one catch-up round, and publish what it makes this node able to say.
///
/// Extracted from the follow loop because it is the only part that holds the
/// executor lock, and holding a lock is worth being able to see the boundary of.
async fn execute_round(
    executor: &SharedExecutor,
    peer: &SyncClient,
    history: &mut TenureSource,
    pox: &PoxInfo,
    staging: &Staging,
    budget: CatchUpBudget,
    advertised: &Advertised,
) -> ExecutedRound {
    let mut executor = executor.lock().await;
    let mut peer_failed = false;
    let from = executor.tip().header.chain_length;
    match executor.catch_up(peer, history, pox, staging, budget).await {
        Ok(round) => report_round(from, round, executor.tip()),
        // A round that stops partway has still sealed everything up to where it
        // stopped, and that is what has to be recorded: reporting only successful
        // rounds left a node that had executed eighty-three blocks claiming
        // twenty-two, and left its accounting behind its own chain.
        Err(error) => {
            eprintln!("executing the peer's chain failed: {error}");
            backfill_missing_header(&mut executor, peer, &error.to_string()).await;
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
    pox: PoxInfo,
    source: [u8; 32],
    state: Option<RpcState>,
    executor: Option<SharedExecutor>,
    /// Blocks the public API authenticated, waiting to be staged.
    offered: tokio::sync::mpsc::UnboundedReceiver<NakamotoBlock>,
}

/// Follow the peer, publishing what it validated and executing along it.
async fn follow(follower: Follower) -> Role {
    let Follower {
        config,
        network,
        peer,
        discovered,
        advertised,
        pox,
        source,
        state,
        executor,
        mut offered,
    } = follower;
    let directory = config.chainstate_dir(NODE_CHAINSTATE);
    let interval = Duration::from_secs(config.node.poll_interval_secs);
    let staging = match Staging::open(&directory.join("staging.sqlite")) {
        Ok(staging) => staging,
        Err(error) => return Err(format!("cannot open the staging store: {error}")),
    };
    let budget = CatchUpBudget {
        // Bounded so that a round ends and execution gets its turn: an
        // unbounded descent over a gap of tens of thousands of blocks spends
        // every round fetching and never executes what it already holds.
        fetch: ROUND_FETCH,
        execute: config.node.max_sync_blocks,
    };
    let mut pox = pox;
    // Derive sortitions alongside the peer's answers, when the checkpoint
    // carries the history that makes it possible.
    if let (Some(executor), Some(directory)) =
        (executor.as_ref(), config.checkpoint.sortition.as_ref())
    {
        let phase = Phase::start("seeding the local sortition chain");
        start_deriving_sortitions(executor, directory, &config.node.working_dir).await;
        drop(phase);
    }

    if let Some(executor) = executor.as_ref() {
        backfill_ancestors(executor, &peer, &pox, source).await;
    }
    let mut peer_height = u64::MAX;
    let mut executed_height = 0;
    let mut published = RewardCyclePublication::default();
    // Which peer this round follows. Re-chosen from everything this node knows of —
    // the endpoints the operator configured and the ones p2p discovery found — so
    // that a peer which stalls, falls behind or starts refusing costs one round
    // rather than the node's liveness. That was the open half of task 027: the
    // choosing already existed, and nothing called it.
    let mut peer = peer;
    let mut node = Node::new(peer.clone());
    let mut rounds_on_this_peer = 0_u32;
    let mut peer_failed = false;
    // Bulk history comes from every peer known, which is not the same question as
    // which peer this round *follows*: following is a fork choice and has to land on
    // one answer, while fetching history is work to be spread.
    let mut history = BulkHistory::new(peer.clone());
    loop {
        history.refresh(&config, discovered.as_ref());
        // Re-weigh on a timer, or immediately after the current peer let a round
        // down. Every round would be two requests per peer per second for an answer
        // that moves on the order of a tenure; never would be the single-peer node
        // this task set out to remove.
        rounds_on_this_peer = rounds_on_this_peer.saturating_add(1);
        if peer_failed || rounds_on_this_peer >= RESELECT_ROUNDS {
            rounds_on_this_peer = 0;
            peer_failed = false;
            if let Some(chosen) = better_peer(&peer, &config, discovered.as_ref()).await {
                peer = chosen;
                node = Node::new(peer.clone());
            }
        }
        // Blocks the public API admitted go into the same store the peer's do,
        // before the round that executes it: nothing about them is special from
        // here on, which is the point.
        stage_admitted_blocks(&mut offered, &staging);
        // Following the peer's current tenure is pointless while this node is
        // far from it — the tenure descends from blocks it has not executed, so
        // the walk fails every round — and the requests it spends are the ones
        // catching up needs. A node this far back has nothing to serve anyway.
        let catching_up = peer_height.saturating_sub(executed_height) > FOLLOW_WHEN_WITHIN;
        peer_failed |= track_peer(
            &mut node,
            &peer,
            state.as_ref(),
            &mut pox,
            &mut peer_height,
            catching_up,
        )
        .await;
        if let Some(executor) = executor.as_ref() {
            let round = execute_round(
                executor,
                &peer,
                &mut history.source,
                &pox,
                &staging,
                budget,
                &advertised,
            )
            .await;
            executed_height = round.executed_height;
            peer_failed |= round.peer_failed;
            let sealed = round.sealed;
            if let Some(state) = state.as_ref() {
                state.publish_executed(sealed).await;
                publish_reward_cycle(
                    state,
                    executor,
                    &config,
                    network,
                    &pox,
                    &last_sortition_winners(node.view().as_ref()),
                    &mut published,
                )
                .await;
            }
        }
        sleep(interval).await;
    }
}

/// Take the blocks the public API admitted and stage them.
///
/// Nothing is validated here on purpose: the route already put each one through
/// `ChainState::authenticate_block`, and the executor checks its state root when
/// it runs it. Draining the channel rather than awaiting it keeps this on the
/// round's own clock — an upload is visible within one poll interval, and a burst
/// of them cannot starve the peer.
fn stage_admitted_blocks(
    offered: &mut tokio::sync::mpsc::UnboundedReceiver<NakamotoBlock>,
    staging: &Staging,
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
    }
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
    /// The key `.miners` is currently replicated for. Reconfiguring a contract
    /// clears every chunk in it, so this is only done when the writer changes —
    /// doing it per round would drop the proposal a signer is reading.
    miner_writer: Option<nano_primitives::Hash160>,
}

/// Publish the reward set the executed state derives, and configure the
/// `StackerDB` contracts that cycle's signers write to.
///
/// Derived from this node's own pox-5 state rather than read from the peer, which
/// is the whole difference between serving a reward set and relaying one. The
/// document nano writes here is the one `SyncClient` parses, so a node's own
/// `/v3/stacker_set` can attest another node's checkpoint.
async fn publish_reward_cycle(
    state: &RpcState,
    executor: &SharedExecutor,
    config: &Config,
    network: Network,
    pox: &PoxInfo,
    winners: &[nano_primitives::Hash160],
    published: &mut RewardCyclePublication,
) {
    let mut context = bitcoin_context(config, pox);
    context.height = executor.lock().await.bitcoin_height();
    let Some(cycle) = nano_chainstate::signers::reward_cycle_at(context) else {
        return;
    };
    configure_miner_slots(state, network, cycle, winners, published).await;
    if published.served == Some(cycle) {
        return;
    }
    // The lock is held only for the walk: it is the same lock every account read
    // takes, and the walk is one contract call per signer.
    let derived = nano_chainstate::signers::active_signer_set(
        executor.lock().await.chainstate_mut().vm_mut(),
        context,
    );
    let (signers, threshold) = match derived {
        Ok(derived) => derived,
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
    let entries: Vec<nano_rpc::RewardSetSigner> = signers
        .signers()
        .iter()
        .map(|signer| nano_rpc::RewardSetSigner {
            signing_key: signer.public_key.to_bytes_compressed(),
            // Not carried by `SignerSet`, which keeps only the weight it
            // apportioned from the amount. A signer's own weight is what decides
            // whether a block is attested, so it is served; the amount behind it
            // is reported as zero rather than reconstructed from the weight,
            // which would be the threshold back again and not the amount.
            stacked_amount: 0,
            weight: signer.weight,
        })
        .collect();
    state
        .publish_stacker_set(cycle, nano_rpc::stacker_set_payload(&entries, threshold))
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

/// The block-signing keys that won the last two sortitions, newest first.
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
        if winners.len() == 2 {
            break;
        }
    }
    winners
}

/// Replicate `.miners`, so a signer hosted here can read what a miner proposed.
///
/// The two slots belong to the last two sortition winners, and which winner gets
/// which is `num_sortitions % 2` in stacks-core — a count over the whole
/// burnchain that a checkpointed node has never made and no snapshot nano holds
/// carries. So the slots are configured only where the answer cannot be got
/// wrong: when the last two winners are the same key, which is every chain with
/// one miner. Otherwise it says so and replicates nothing, because a `.miners`
/// replica with the two slots swapped refuses the very chunks it exists for.
async fn configure_miner_slots(
    state: &RpcState,
    network: Network,
    cycle: u64,
    winners: &[nano_primitives::Hash160],
    published: &mut RewardCyclePublication,
) {
    let Some(&latest) = winners.first() else {
        return;
    };
    let previous = winners.get(1).copied().unwrap_or(latest);
    if previous != latest {
        if published.ambiguous_miners != Some(cycle) {
            published.ambiguous_miners = Some(cycle);
            eprintln!(
                "the last two sortitions were won by different miners ({latest} and \
                 {previous}), and this node cannot say which of the two .miners slots each \
                 owns without a sortition count, so it replicates neither"
            );
        }
        return;
    }
    if published.miner_writer == Some(latest) {
        return;
    }
    published.miner_writer = Some(latest);
    let contract = crate::config::miner_contract(network);
    state.stackerdb().write().await.configure(
        &format!("{}.{}", contract.address, contract.name),
        vec![latest, latest],
    );
    println!("replicating .miners for the miner {latest} that won the last two sortitions");
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
    peer: &SyncClient,
    directory: &Path,
) -> Result<CheckpointExecutor<BurnchainSource>, Box<dyn Error>> {
    let (chainstate, anchor, context) =
        open_chainstate(config, network, pox, peer, directory).await?;
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
    peer: &SyncClient,
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
        context.height = config.checkpoint.anchor_bitcoin_height;
        return Ok((chainstate, anchor, Some(context)));
    };
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
    let tip = resume_from(ancestors, peer, tip, directory).await?;
    println!(
        "resuming {} from the state on disk, sealed at block {} of height {}",
        directory.display(),
        tip.block_id(),
        tip.header.chain_length
    );
    recover_ledger(&mut chainstate, config, directory, &tip)?;
    Ok((chainstate, tip, None))
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
    peer: &SyncClient,
    tip: [u8; 32],
    directory: &Path,
) -> Result<NakamotoBlock, Box<dyn Error>> {
    let sealed = StacksBlockId::from_bytes(tip);
    let mut waited = 0;
    loop {
        match patiently(|| peer.block(sealed)).await {
            Ok(block) => return Ok(block),
            Err(_) if waited < RESUME_ATTEMPTS => {
                waited += 1;
                println!("waiting for the peer to catch up to block {sealed}");
                sleep(Duration::from_secs(1)).await;
            }
            Err(_) => break,
        }
    }

    for (walked, ancestor) in ancestors.iter().enumerate() {
        if let Ok(block) = patiently(|| peer.block(StacksBlockId::from_bytes(*ancestor))).await {
            println!(
                "block {sealed} left the chain; carrying on from {}, {} back",
                block.block_id(),
                walked + 1
            );
            return Ok(block);
        }
    }

    Err(format!(
        "the state in {} is sealed at block {sealed}, and the peer has none of its {} ancestors \
         either; nothing on the network extends it, so it needs another peer or a fresh \
         checkpoint",
        directory.display(),
        ancestors.len()
    )
    .into())
}

/// Serve the public RPC, if this node is configured to.
async fn start_rpc(
    config: &Config,
    network: Network,
    executor: Option<SharedExecutor>,
    mempool: Arc<Mutex<nano_mempool::Mempool>>,
    dispatcher: &EventDispatcher,
    blocks: tokio::sync::mpsc::UnboundedSender<NakamotoBlock>,
    roles: &mut JoinSet<(Job, Result<(), String>)>,
) -> Result<Option<RpcState>, Box<dyn Error>> {
    let Some(address) = config.node.rpc_bind else {
        return Ok(None);
    };
    let mut state = RpcState::new()
        .on(network)
        .with_mempool(mempool)
        .with_block_sink(blocks);
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

/// Validate proposals for the active reward cycle, if this node signs.
///
/// The signer's chain state is opened here rather than in the task, so a state it
/// cannot open stops the node at startup instead of a second later.
async fn start_signer(
    config: &Config,
    network: Network,
    pox: &PoxInfo,
    peer: &SyncClient,
    roles: &mut JoinSet<(Job, Role)>,
) -> Result<(), Box<dyn Error>> {
    let Some(signer) = config.signer.clone() else {
        return Ok(());
    };
    let validator = signer::open(
        config,
        network,
        pox,
        peer,
        &config.chainstate_dir(SIGNER_CHAINSTATE),
    )
    .await?;
    let (running, peer) = (config.clone(), peer.clone());
    roles.spawn(async move {
        (
            Job::Signer,
            signer::run(running, signer, network, peer, validator).await,
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
    if last - first < MINER_REWARD_MATURITY {
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

/// Every peer this node could follow: the ones configured, and the ones p2p found.
///
/// Configured first, so an operator naming a peer still gets it weighed; discovered
/// ones after, de-duplicated, because the same node can be both. A pool of one is
/// still a pool — it just cannot protect against that one.
fn follow_pool(config: &Config, discovered: Option<&Discovered>) -> PeerPool {
    PeerPool::from_endpoints(&follow_endpoints(config, discovered))
}

fn follow_endpoints(config: &Config, discovered: Option<&Discovered>) -> Vec<String> {
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
/// The weighing is `PeerPool::choose_source`, which is the boundary that has to
/// stay in one place: a tip is compared on signer weight and length from headers
/// this node fetched, never on a peer's claim about its own height.
async fn better_peer(
    current: &SyncClient,
    config: &Config,
    discovered: Option<&Discovered>,
) -> Option<SyncClient> {
    let pool = follow_pool(config, discovered);
    let (_, chosen) = pool.choose_source(None).await?;
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
}

/// What the follow loop knows and the peer-facing loops need.
#[derive(Clone, Debug)]
struct LocalAnnouncement {
    /// The Bitcoin height the sealed tip was executed under.
    bitcoin_height: u64,
    /// The consensus hash naming the reward cycle being walked, when derivable.
    cycle_start: Option<nano_primitives::ConsensusHash>,
    /// Which tenures of that cycle this node has executed, and so will serve.
    inventory: Option<(nano_primitives::ConsensusHash, nano_primitives::BitVec<2100>)>,
}

impl Advertised {
    fn publish(&self, announcement: LocalAnnouncement) {
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
    fn tenure_inventory(
        &self,
        cycle_start: nano_primitives::ConsensusHash,
    ) -> Option<nano_primitives::BitVec<2100>> {
        let (known, tenures) = self.read()?.inventory?;
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

    let interval = Duration::from_secs(config.node.poll_interval_secs.max(1) * 10);
    let advertised = advertised.clone();
    roles.spawn(async move {
        (
            Job::Peers,
            peer_discovery(swarm, bitcoin, advertised, interval).await,
        )
    });

    if let Some(bind) = bind {
        start_listener(config, network, bind, service, roles);
    }
    Some(discovered)
}

/// Keep the peer set at strength for as long as the node runs.
async fn peer_discovery(
    mut swarm: nano_p2p::Swarm,
    bitcoin: BurnchainSource,
    advertised: Advertised,
    interval: Duration,
) -> Role {
    loop {
        sleep(interval).await;
        let published = advertised.read();
        let view = advertised_view(&bitcoin, published.as_ref());
        // `None` before there is a chain to name a cycle, and a peer is then not
        // asked at all rather than asked about a guess.
        let cycle_start = published.and_then(|announced| announced.cycle_start);
        let round = swarm.maintain(view, cycle_start).await;
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
        // Pushed blocks and transactions are still dropped here, because acting on
        // one means putting it through staging and the authenticated selection
        // boundary, and doing it from this loop would be the one place that trusted a
        // peer. That is the relay item in task 054.
        let pushed = swarm.take_pushed().len();
        if round.collected > 0 {
            println!(
                "p2p: {} messages peers sent unprompted, {pushed} of them pushed data",
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
}
