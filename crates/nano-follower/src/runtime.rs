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
        let error = outcome.err().map(|error| {
            eprintln!("follower round failed: {error}");
            error.to_string()
        });
        if error.is_none() {
            println!(
                "followed {from} -> {}",
                self.executor.tip().header.chain_length
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
    Snapshot {
        ready,
        stacks_height: Some(executor.tip().header.chain_length),
        bitcoin_height: Some(executor.bitcoin_height()),
        state_root: Some(hex::encode(
            executor.tip().header.state_index_root.as_bytes(),
        )),
        p2p_connected: discovered.connected(),
        p2p_known: discovered.known(),
        last_error,
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
    use super::StateLock;

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
