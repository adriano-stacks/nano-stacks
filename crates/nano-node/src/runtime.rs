//! Starting a node: open the state, pick a peer, run the configured roles.
//!
//! Everything a role needs is derived here, once, so that following, signing
//! and mining are three tasks over one configuration rather than three
//! programs over three command lines.

use std::{error::Error, fs, future::Future, path::Path, sync::Arc, time::Duration};

use nano_bitcoin::{BitcoinRestSource, BitcoinRpcSource, BitcoinSource};
use nano_crypto::StacksPublicKey;
use nano_chainstate::{
    MINER_REWARD_MATURITY, Signer, SignerSet,
    BitcoinBlockContext, ChainState, NakamotoBlock, TenureAccounting, TenureAccountingError,
};
use nano_primitives::{Network, StacksBlockId};
use nano_rpc::{ChainAccess, EventDispatcher, RpcState, SealedTip, serve};
use nano_sync::{Node, PoxInfo, SyncClient, SyncError};
use tokio::{net::TcpListener, signal::unix::SignalKind, sync::Mutex, task::JoinSet, time::sleep};

use crate::{
    CatchUpBudget, CatchUpRound, CheckpointExecutor, CheckpointManifest, CheckpointProvenance,
    config::Config, miner, signer, staging::Staging,
};

/// How many blocks one round of catching up will fetch before executing.
pub(crate) const ROUND_FETCH: usize = 4_000;

/// How close a node has to be before it is worth following the peer's tenure
/// rather than spending every request catching up.
const FOLLOW_WHEN_WITHIN: u64 = 1_000;

/// How long a startup step waits out a rate-limited peer before giving up.
const STARTUP_PATIENCE: Duration = Duration::from_secs(64);

/// The state directory the node executes the canonical chain in.
pub(crate) const NODE_CHAINSTATE: &str = "chainstate";
/// The state directory the signer validates proposals in.
const SIGNER_CHAINSTATE: &str = "signer-chainstate";
/// The accounting a role owes, rewritten as it executes.
const ACCOUNTING_FILE: &str = "accounting.json";

/// The shared executed chain the node follows along and answers reads from.
pub type SharedExecutor = Arc<Mutex<CheckpointExecutor<BurnchainSource>>>;

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
        None => Network::from_chain_id(patiently(|| peer.node_info()).await?.network_id),
    };
    let pox = patiently(|| peer.pox_info()).await?;
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
    // Publish what this node is sealed at before it follows anything, so a node
    // that never manages to execute reports the height it is really on rather
    // than nothing at all.
    if let (Some(state), Some(executor)) = (state.as_ref(), executor.as_ref()) {
        let sealed = {
            let executor = executor.lock().await;
            sealed_tip(executor.tip(), executor.bitcoin_height())
        };
        state.publish_executed(sealed).await;
    }
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
        roles.spawn(
            async move { (Job::Follower, follow(config, peer, pox, state, executor).await) },
        );
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

/// Follow the peer, publishing what it validated and executing along it.
async fn follow(
    config: Config,
    peer: SyncClient,
    pox: PoxInfo,
    state: Option<RpcState>,
    executor: Option<SharedExecutor>,
) -> Role {
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
    let mut node = Node::new(peer.clone());
    let mut pox = pox;
    let mut peer_height = u64::MAX;
    let mut executed_height = 0;
    loop {
        // Following the peer's current tenure is pointless while this node is
        // far from it — the tenure descends from blocks it has not executed, so
        // the walk fails every round — and the requests it spends are the ones
        // catching up needs. A node this far back has nothing to serve anyway.
        let catching_up = peer_height.saturating_sub(executed_height) > FOLLOW_WHEN_WITHIN;
        // The served view and the executed chain are independent jobs on one
        // peer. Gating execution on a successful poll is how a node twenty
        // thousand blocks behind executed nothing at all: that far back the
        // follower's own tenure walk fails every round, and it took the
        // executor down with it.
        if catching_up {
            match peer.node_info().await {
                Ok(info) => peer_height = info.stacks_height,
                Err(error) => eprintln!("asking the peer how far ahead it is failed: {error}"),
            }
        } else {
            match node.poll().await {
                Ok(_) => {
                    if let Some(view) = node.view() {
                        peer_height = view.node_info.stacks_height;
                        pox = view.pox_info.clone();
                        if let Some(state) = state.as_ref() {
                            state.publish(view).await;
                        }
                    }
                }
                Err(error) => eprintln!("following the peer failed: {error}"),
            }
        }
        if let Some(executor) = executor.as_ref() {
            let sealed = {
                let mut executor = executor.lock().await;
                let from = executor.tip().header.chain_length;
                match executor.catch_up(&peer, &pox, &staging, budget).await {
                    Ok(round) => report_round(from, round, executor.tip()),
                    // A round that stops partway has still sealed everything up
                    // to where it stopped, and that is what has to be recorded:
                    // reporting only successful rounds left a node that had
                    // executed eighty-three blocks claiming twenty-two, and
                    // left its accounting behind its own chain.
                    Err(error) => eprintln!("executing the peer's chain failed: {error}"),
                }
                persist_accounting(&directory, &mut executor)?;
                executed_height = executor.tip().header.chain_length;
                sealed_tip(executor.tip(), executor.bitcoin_height())
            };
            if let Some(state) = state.as_ref() {
                state.publish_executed(sealed).await;
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
    // Collected before any request, so the store is not borrowed across one.
    let mut ancestors = Vec::new();
    let mut walk = tip;
    while let Some(parent) = chainstate.parent_of(walk) {
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
    Ok((chainstate, tip, None))
}

/// Find the block a resumed state can carry on from.
///
/// A peer that does not have our sealed tip is usually one still catching up,
/// so it is worth waiting for. If it never produces the block, the tip lost a
/// fork race while this node was down — an ordinary event, one block deep —
/// and the answer is to walk back to the nearest ancestor the peer does have
/// rather than to refuse to start. Only a state with no ancestor on the
/// network at all is one nothing can extend.
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
        return Ok(TenureAccounting::from_json(&fs::read(persisted)?)?);
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

async fn reachable_peer(config: &Config) -> Result<SyncClient, Box<dyn Error>> {
    let mut last = None;
    for url in config.node.peers()? {
        let client = SyncClient::new(url.clone())?;
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
