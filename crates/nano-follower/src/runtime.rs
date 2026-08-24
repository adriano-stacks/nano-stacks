//! The standalone follower process: discover, select, fetch, execute, persist.

use std::{error::Error, fs::OpenOptions, future::Future, path::Path, sync::Arc, time::Duration};

use fs2::FileExt as _;
use nano_primitives::Network;
use nano_sync::{PeerPool, PoxInfo, SyncClient, TenureSource};
use tokio::{task::JoinHandle, time::sleep};

use crate::{
    CatchUpBudget, CheckpointExecutor,
    archive::Archive,
    burnchain::{self, BurnchainSource, bitcoin_context},
    config::Config,
    network::OutboundNetwork,
    observation::{Observation, Snapshot},
    staging::Staging,
    startup,
};

const ROUND_FETCH: usize = 4_000;

/// Run until SIGINT or SIGTERM, finishing the current bounded execution round.
pub async fn run(config: Config) -> Result<(), Box<dyn Error>> {
    run_until(config, shutdown()).await
}

async fn run_until<F>(config: Config, shutdown: F) -> Result<(), Box<dyn Error>>
where
    F: Future<Output = ()> + Send,
{
    let _lock = StateLock::acquire(&config.follower.working_dir)?;
    let observation = Observation::new();
    let observation_task = serve_observation(&config, observation.clone());
    let mut follower = Follower::open(&config, &observation).await?;

    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        if observation_task.is_finished() {
            return observation_task
                .await
                .map_err(|error| format!("observation task failed: {error}"))?
                .map_err(Into::into);
        }
        if !follower.transport.is_running() {
            return Err("outbound P2P maintenance stopped".into());
        }
        follower.follow_round(&config, &observation).await;

        tokio::select! {
            () = &mut shutdown => break,
            () = sleep(Duration::from_secs(config.follower.poll_interval_secs.max(1))) => {}
        }
    }
    observation_task.abort();
    Ok(())
}

struct Follower {
    transport: OutboundNetwork,
    discovered: nano_p2p::Discovered,
    peer: SyncClient,
    pox: PoxInfo,
    history: History,
    executor: CheckpointExecutor<BurnchainSource>,
    staging: Staging,
    stall: StallWatch,
}

impl Follower {
    async fn open(config: &Config, observation: &Observation) -> Result<Self, Box<dyn Error>> {
        let bootstrap = if config.network().is_none() {
            Some(await_peer(config, None, true).await?)
        } else {
            None
        };
        let network = config.network().unwrap_or_else(|| {
            Network::from_chain_id(
                bootstrap
                    .as_ref()
                    .expect("an inferred network has a bootstrap peer")
                    .1,
            )
        });
        let transport = OutboundNetwork::start(config, network).await?;
        let discovered = transport.discovered();
        let (peer, _, pox) = match bootstrap {
            Some(peer) => peer,
            None => await_peer(config, Some(&discovered), false).await?,
        };
        let initial_endpoints = endpoints(config, &discovered);
        let mut history = History::new(&initial_endpoints);
        let mut executor =
            startup::open_executor(config, network, &pox, &mut history.source).await?;
        executor.backfill_headers()?;
        executor.keep_executed_blocks(Arc::new(Archive::open(
            &config.chainstate_dir().join("executed.sqlite"),
        )?));
        executor.publish_executed_height_to(observation.executed_height_sink());
        let staging = Staging::open(&config.chainstate_dir().join("staging.sqlite"))?;
        transport.publish_cycle(executor.cycle_start_consensus_hash(&pox));
        observation
            .publish(snapshot(&executor, &discovered, true, None))
            .await;
        Ok(Self {
            transport,
            discovered,
            peer,
            pox,
            history,
            executor,
            staging,
            stall: StallWatch::new(),
        })
    }

    async fn follow_round(&mut self, config: &Config, observation: &Observation) {
        let current_endpoints = endpoints(config, &self.discovered);
        self.history.refresh(&current_endpoints);
        let pool = PeerPool::from_endpoints(&self.history.endpoints);
        if let Some((_, selected)) = select_peer(&pool, config, &self.pox, &mut self.executor).await
        {
            self.peer = selected;
        }
        if let Ok(current) = self.peer.pox_info().await {
            match burnchain::verified_pox(config, current) {
                Ok(current) => self.pox = current,
                Err(reason) => eprintln!("keeping the pinned PoX constants: {reason}"),
            }
        }
        let from = self.executor.tip().header.chain_length;
        let outcome = self
            .executor
            .catch_up(
                &self.peer,
                &mut self.history.source,
                &self.pox,
                &self.staging,
                CatchUpBudget {
                    fetch: ROUND_FETCH,
                    execute: config.follower.max_sync_blocks,
                },
                &self.discovered.claims(),
            )
            .await;
        self.executor.follow_burnchain(&self.pox);
        self.transport
            .publish_cycle(self.executor.cycle_start_consensus_hash(&self.pox));
        let mut error = outcome.err().map(|error| {
            eprintln!("follower round failed: {error}");
            error.to_string()
        });
        if error.is_none() {
            println!(
                "followed {from} -> {}",
                self.executor.tip().header.chain_length
            );
        }
        if error.is_none() {
            error = self.stall.observe(
                self.executor.tip().header.chain_length,
                self.executor.bitcoin_height(),
                self.pox.bitcoin_height,
            );
        }
        observation
            .publish(snapshot(
                &self.executor,
                &self.discovered,
                error.is_none(),
                error,
            ))
            .await;
    }
}

/// Notice a follower that stops without saying so.
///
/// The 2026-08-19 qualification catch-up sat for 45 minutes at the last burn
/// block of a reward cycle with every round returning success, `ready: true`
/// and nothing to read: no executed block, no burn-view movement, no error.
/// A round is allowed to wait — on staging, on a peer, on Bitcoin — but a
/// follower whose burn view sits far behind the network's while round after
/// round changes nothing is stalled, and a stall an operator cannot see is a
/// defect regardless of its cause.
struct StallWatch {
    stacks: u64,
    bitcoin: u64,
    rounds_without_progress: u32,
}

impl StallWatch {
    /// The follower's burn view legitimately trails the network by its stable
    /// confirmations; only a gap beyond that can indicate a stall.
    const NETWORK_MARGIN: u64 = 12;
    /// Stalled rounds are fast, so this is minutes of wall clock, not hours.
    const ROUNDS: u32 = 300;

    const fn new() -> Self {
        Self {
            stacks: 0,
            bitcoin: 0,
            rounds_without_progress: 0,
        }
    }

    fn observe(&mut self, stacks: u64, bitcoin: u64, network_bitcoin: u64) -> Option<String> {
        let progressed = stacks != self.stacks || bitcoin != self.bitcoin;
        self.stacks = stacks;
        self.bitcoin = bitcoin;
        let behind = network_bitcoin > bitcoin.saturating_add(Self::NETWORK_MARGIN);
        if progressed || !behind {
            self.rounds_without_progress = 0;
            return None;
        }
        self.rounds_without_progress = self.rounds_without_progress.saturating_add(1);
        if self.rounds_without_progress < Self::ROUNDS {
            return None;
        }
        let stall = format!(
            "stalled: no executed block and no burn-view progress across {} rounds \
             while the peer's burn tip {network_bitcoin} stands {} above this node's {bitcoin}",
            self.rounds_without_progress,
            network_bitcoin.saturating_sub(bitcoin),
        );
        eprintln!("{stall}");
        Some(stall)
    }
}

async fn select_peer(
    pool: &PeerPool,
    config: &Config,
    pox: &PoxInfo,
    executor: &mut CheckpointExecutor<BurnchainSource>,
) -> Option<(usize, SyncClient)> {
    let signers = executor.recorded_signer_set(bitcoin_context(config, pox));
    pool.choose_source(signers.as_ref(), executor.burn_view())
        .await
}

struct History {
    endpoints: Vec<String>,
    source: TenureSource,
}

impl History {
    fn new(endpoints: &[String]) -> Self {
        let pool = PeerPool::from_endpoints(endpoints);
        Self {
            endpoints: pool.endpoints(),
            source: TenureSource::new(pool.into_clients()),
        }
    }

    fn refresh(&mut self, endpoints: &[String]) {
        let pool = PeerPool::from_endpoints(endpoints);
        let normalized = pool.endpoints();
        if !pool.is_empty() && normalized != self.endpoints {
            println!("fetching history from {} peers", pool.len());
            self.endpoints = normalized;
            self.source = TenureSource::new(pool.into_clients());
        }
    }
}

fn endpoints(config: &Config, discovered: &nano_p2p::Discovered) -> Vec<String> {
    let mut endpoints = config.follower.peers.clone();
    for endpoint in discovered.endpoints() {
        if !endpoints.contains(&endpoint) {
            endpoints.push(endpoint);
        }
    }
    endpoints
}

async fn await_peer(
    config: &Config,
    discovered: Option<&nano_p2p::Discovered>,
    configured_only: bool,
) -> Result<(SyncClient, u32, PoxInfo), Box<dyn Error>> {
    let deadline = Duration::from_secs(config.follower.startup_peer_wait_secs);
    let began = std::time::Instant::now();
    loop {
        let candidates = if configured_only {
            config.follower.peers.clone()
        } else {
            discovered.map_or_else(
                || config.follower.peers.clone(),
                |discovered| endpoints(config, discovered),
            )
        };
        if configured_only && candidates.is_empty() {
            return Err("an inferred network needs at least one configured HTTP peer".into());
        }
        for endpoint in candidates {
            let Ok(url) = endpoint.parse() else {
                continue;
            };
            let Ok(client) = SyncClient::new(url) else {
                continue;
            };
            if let (Ok(info), Ok(pox)) = (client.node_info().await, client.pox_info().await) {
                match burnchain::verified_pox(config, pox) {
                    Ok(pox) => return Ok((client, info.network_id, pox)),
                    Err(reason) => eprintln!("refusing bootstrap peer {endpoint}: {reason}"),
                }
            }
        }
        if began.elapsed() >= deadline {
            return Err(
                "no configured or discovered peer answered before the startup deadline".into(),
            );
        }
        sleep(Duration::from_secs(
            config.follower.poll_interval_secs.max(1),
        ))
        .await;
    }
}

fn snapshot(
    executor: &CheckpointExecutor<BurnchainSource>,
    discovered: &nano_p2p::Discovered,
    ready: bool,
    last_error: Option<String>,
) -> Snapshot {
    // A node refusing a burn view it derived and dropped cannot recover on its
    // own, so it is not ready however healthy the rest of it looks. Reported here
    // rather than at each call site because every publisher needs it and none of
    // them can know it.
    let stalled = executor.dropped_view_stall().map(str::to_owned);
    Snapshot {
        ready: ready && stalled.is_none(),
        stacks_height: Some(executor.tip().header.chain_length),
        bitcoin_height: Some(executor.bitcoin_height()),
        state_root: Some(hex::encode(
            executor.tip().header.state_index_root.as_bytes(),
        )),
        p2p_connected: discovered.connected(),
        p2p_known: discovered.known(),
        last_error: last_error.or(stalled),
    }
}

fn serve_observation(config: &Config, observation: Observation) -> JoinHandle<std::io::Result<()>> {
    let health = config.follower.health_bind;
    let metrics = config.follower.metrics_bind;
    tokio::spawn(async move { observation.serve(health, metrics).await })
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

struct StateLock(std::fs::File);

impl StateLock {
    fn acquire(directory: &Path) -> Result<Self, Box<dyn Error>> {
        std::fs::create_dir_all(directory)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join("follower.lock"))?;
        lock.try_lock_exclusive()
            .map_err(|error| format!("another follower owns {}: {error}", directory.display()))?;
        Ok(Self(lock))
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::{StallWatch, StateLock};

    /// A quiet chain is not a stall, a trailing burn view within the stable
    /// margin is not a stall, and progress of either kind resets the count.
    #[test]
    fn a_stall_is_reported_only_behind_the_network_and_without_progress() {
        let mut watch = StallWatch::new();
        // Far behind and frozen: the report arrives at the threshold.
        for _ in 0..StallWatch::ROUNDS {
            assert_eq!(watch.observe(100, 950_000, 951_000), None);
        }
        let stall = watch
            .observe(100, 950_000, 951_000)
            .expect("a frozen follower far behind the network reports itself");
        assert!(stall.contains("stalled"), "{stall}");

        // One executed block resets the count entirely.
        assert_eq!(watch.observe(101, 950_000, 951_000), None);
        for _ in 0..StallWatch::ROUNDS - 1 {
            assert_eq!(watch.observe(101, 950_000, 951_000), None);
        }
        assert!(watch.observe(101, 950_000, 951_000).is_some());

        // At tip, no rounds ever accumulate, however long the chain is quiet.
        let mut quiet = StallWatch::new();
        for _ in 0..(StallWatch::ROUNDS * 2) {
            assert_eq!(quiet.observe(500, 963_000, 963_007), None);
        }
    }

    #[test]
    fn only_one_follower_can_own_a_state_directory() {
        let directory = tempfile::tempdir().expect("directory");
        let _first = StateLock::acquire(directory.path()).expect("first follower");
        let error = StateLock::acquire(directory.path())
            .err()
            .expect("second follower refused")
            .to_string();
        assert!(error.contains("another follower owns"), "{error}");
    }
}
