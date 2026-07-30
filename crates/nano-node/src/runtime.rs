//! Starting a node: open the state, pick a peer, run the configured roles.
//!
//! Everything a role needs is derived here, once, so that following, signing
//! and mining are three tasks over one configuration rather than three
//! programs over three command lines.

use std::{error::Error, fs, path::Path, sync::Arc, time::Duration};

use nano_bitcoin::{BitcoinRpcSource, BitcoinSource};
use nano_chainstate::{
    BitcoinBlockContext, ChainState, NakamotoBlock, TenureAccounting, TenureAccountingError,
};
use nano_primitives::{Network, StacksBlockId};
use nano_rpc::{ChainAccess, EventDispatcher, RpcState, serve};
use nano_sync::{Node, PoxInfo, SyncClient};
use tokio::{net::TcpListener, signal::unix::SignalKind, sync::Mutex, task::JoinSet, time::sleep};

use crate::{CheckpointExecutor, config::Config, miner, signer};

/// The state directory the node executes the canonical chain in.
pub(crate) const NODE_CHAINSTATE: &str = "chainstate";
/// The state directory the signer validates proposals in.
const SIGNER_CHAINSTATE: &str = "signer-chainstate";
/// The accounting a role owes, rewritten as it executes.
const ACCOUNTING_FILE: &str = "accounting.json";

/// The shared executed chain the node follows along and answers reads from.
pub type SharedExecutor = Arc<Mutex<CheckpointExecutor<BitcoinRpcSource>>>;

/// What a role reports when it stops, which is always the end of the node.
pub type Role = Result<(), String>;

/// Polls a peer is given to produce the block a resumed state is sealed at,
/// before the state is declared to have left the chain.
const RESUME_ATTEMPTS: u32 = 30;

/// A job the node runs, and what its stopping means for the rest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Job {
    Rpc,
    Follower,
    Signer,
    Miner,
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
            Self::Rpc | Self::Miner => false,
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
        })
    }
}

/// Run a node until it is asked to stop or a role gives up.
pub async fn run(config: Config) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(&config.node.working_dir)?;
    let peer = reachable_peer(&config).await?;
    let network = match config.network() {
        Some(network) => network,
        // A private network's chain identifier is only knowable from the chain,
        // so a configuration that does not fix it takes what the peer reports.
        None => Network::from_chain_id(peer.node_info().await?.network_id),
    };
    let pox = peer.pox_info().await?;
    println!(
        "nano-stacks starting on chain {:#010x}, state under {}",
        network.chain_id(),
        config.node.working_dir.display()
    );

    // The chain is only executed when something reads the executed state: a
    // signer-only node validates proposals in its own store and would be
    // executing every block twice for nobody.
    let executor = if config.node.rpc_bind.is_some() || config.miner.is_some() {
        Some(Arc::new(Mutex::new(
            open_executor(
                &config,
                network,
                &pox,
                &peer,
                &config.chainstate_dir(NODE_CHAINSTATE),
            )
            .await?,
        )))
    } else {
        None
    };
    let dispatcher = EventDispatcher::new(config.node.event_observers()?);

    let mut roles = JoinSet::new();
    let state = match config.node.rpc_bind {
        Some(address) => {
            let mut state = RpcState::new();
            if let Some(executor) = executor.clone() {
                state = state.with_chain(executor as Arc<Mutex<dyn ChainAccess>>);
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
            Some(state)
        }
        None => None,
    };
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
        };
        roles.spawn(async move { (Job::Miner, miner::run(runtime).await) });
    }
    if let Some(signer) = config.signer.clone() {
        let validator = signer::open(
            &config,
            network,
            &pox,
            &peer,
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
    }
    let executor = executor.filter(|_| executing_follower);
    // Following is only worth a task when someone reads what it produces: a
    // signer-only node validates from its own store and needs no second view.
    if state.is_some() || executor.is_some() {
        roles.spawn(async move { (Job::Follower, follow(config, peer, state, executor).await) });
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

/// Follow the peer, publishing what it validated and executing along it.
async fn follow(
    config: Config,
    peer: SyncClient,
    state: Option<RpcState>,
    executor: Option<SharedExecutor>,
) -> Role {
    let directory = config.chainstate_dir(NODE_CHAINSTATE);
    let interval = Duration::from_secs(config.node.poll_interval_secs);
    let mut node = Node::new(peer.clone());
    loop {
        if let Err(error) = node.poll().await {
            eprintln!("following the peer failed: {error}");
        } else if let Some(view) = node.view() {
            let pox = view.pox_info.clone();
            if let Some(state) = state.as_ref() {
                state.publish(view).await;
            }
            if let Some(executor) = executor.as_ref() {
                let mut executor = executor.lock().await;
                match executor
                    .follow_to_tip(&peer, &pox, config.node.max_sync_blocks)
                    .await
                {
                    Ok(_) => persist_accounting(&directory, &mut executor)?,
                    Err(error) => eprintln!("executing the peer's chain failed: {error}"),
                }
            }
        }
        sleep(interval).await;
    }
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
) -> Result<CheckpointExecutor<BitcoinRpcSource>, Box<dyn Error>> {
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
    let mut chainstate = ChainState::open_from_checkpoint(
        network,
        directory,
        &config.checkpoint.marf,
        source,
        config.checkpoint.state_root()?,
    )?;
    *chainstate.accounting_mut() = accounting(config, directory)?;

    let Some(tip) = chainstate.tip().filter(|tip| *tip != source) else {
        let anchor = NakamotoBlock::decode(&fs::read(&config.checkpoint.anchor_block)?)?;
        let mut context = bitcoin_context(config, pox);
        context.height = config.checkpoint.anchor_bitcoin_height;
        return Ok((chainstate, anchor, Some(context)));
    };
    // A peer that does not have this block yet is usually one still catching
    // up, not a chain that moved: it is worth waiting for. A peer that never
    // produces it means this state descends from a block the network dropped,
    // and no amount of waiting fixes that.
    let sealed = StacksBlockId::from_bytes(tip);
    let mut waited = 0;
    let tip = loop {
        match peer.block(sealed).await {
            Ok(tip) => break tip,
            Err(error) if waited < RESUME_ATTEMPTS => {
                waited += 1;
                println!("waiting for the peer to catch up to block {sealed} ({error})");
                sleep(Duration::from_secs(config.node.poll_interval_secs)).await;
            }
            Err(error) => {
                return Err(format!(
                    "the state in {} is sealed at block {sealed}, which the peer does not have \
                     ({error}); it descends from a block the network dropped, so it needs another \
                     peer or a fresh checkpoint",
                    directory.display()
                )
                .into());
            }
        }
    };
    println!(
        "resuming {} from the state on disk, sealed at block {} of height {}",
        directory.display(),
        tip.block_id(),
        tip.header.chain_length
    );
    Ok((chainstate, tip, None))
}

/// The rewards a role still owes: what it last wrote, or what the checkpoint
/// said before it had written anything.
fn accounting(config: &Config, directory: &Path) -> Result<TenureAccounting, Box<dyn Error>> {
    let persisted = directory.join(ACCOUNTING_FILE);
    if persisted.exists() {
        return Ok(TenureAccounting::from_json(&fs::read(persisted)?)?);
    }
    match &config.checkpoint.tenure_accounting {
        Some(path) => Ok(TenureAccounting::from_json(&fs::read(path)?)?),
        None => Ok(TenureAccounting::default()),
    }
}

/// Write out what the chain owes, so that a restart owes the same.
pub fn persist_accounting<S>(
    directory: &Path,
    executor: &mut CheckpointExecutor<S>,
) -> Result<(), String>
where
    S: BitcoinSource,
    S::Error: std::fmt::Display,
{
    let encoded = executor
        .chainstate_mut()
        .accounting_mut()
        .to_json()
        .map_err(|error: TenureAccountingError| error.to_string())?;
    fs::write(directory.join(ACCOUNTING_FILE), encoded).map_err(|error| error.to_string())
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
pub fn bitcoin_source(config: &Config) -> Result<BitcoinRpcSource, Box<dyn Error>> {
    Ok(BitcoinRpcSource::new(
        &config.burnchain.rpc_url,
        config.burnchain.rpc_user.clone(),
        config.burnchain.rpc_password.clone(),
        config.burnchain.magic()?,
    )?)
}

/// The first configured peer that answers, so one dead peer is not a dead node.
async fn reachable_peer(config: &Config) -> Result<SyncClient, Box<dyn Error>> {
    let mut last = None;
    for url in config.node.peers()? {
        let client = SyncClient::new(url.clone())?;
        match client.node_info().await {
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
