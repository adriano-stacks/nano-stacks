//! Outbound-only Stacks peer discovery for the follower.

use std::{error::Error, fs, path::Path, time::Duration};

use nano_p2p::{ChainView, Discovered, LocalPeer, PeerDb, Protocol, Swarm, SwarmLimits};
use nano_primitives::{BitcoinHeaderHash, ConsensusHash, Network};
use tokio::{sync::watch, task::JoinHandle, time::sleep};

use crate::{burnchain::bitcoin_source, config::Config};

const DISCOVERY_TICKS: u64 = 10;

/// The discovery handle shared with the execution loop.
pub struct OutboundNetwork {
    discovered: Discovered,
    cycle_start: watch::Sender<Option<ConsensusHash>>,
    task: JoinHandle<()>,
}

impl OutboundNetwork {
    /// Start outbound sessions without a listener or peer-facing service.
    pub async fn start(config: &Config, network: Network) -> Result<Self, Box<dyn Error>> {
        fs::create_dir_all(&config.follower.working_dir)?;
        let protocol = Protocol::for_network(network)
            .with_stable_confirmations(config.burnchain.stable_confirmations)
            .ok_or("burnchain.stable_confirmations must be greater than zero")?;
        let peers = PeerDb::open(&config.follower.working_dir.join("peers.sqlite"))?;
        let local = LocalPeer::quiet(identity(&config.follower.working_dir)?, 20444);
        let mut swarm = Swarm::new(peers, local, protocol, SwarmLimits::default());
        for seed in config.follower.bootstrap_seeds() {
            let recorded = swarm.seed(&seed).await?;
            if recorded == 0 {
                eprintln!("p2p seed {seed} resolved to no address");
            }
        }

        let bitcoin = bitcoin_source(config)?;
        let view = advertised_view(&bitcoin, config.burnchain.stable_confirmations);
        let round = swarm.maintain(view, None).await;
        let discovered = swarm.discovered();
        println!(
            "p2p: {} outbound peers connected, {} known, {} HTTP endpoints discovered",
            round.connected,
            discovered.known(),
            discovered.endpoints().len()
        );

        let (cycle_start, cycle) = watch::channel(None);
        let tick = Duration::from_secs(
            config
                .follower
                .poll_interval_secs
                .max(1)
                .saturating_mul(DISCOVERY_TICKS),
        );
        let confirmations = config.burnchain.stable_confirmations;
        let task = tokio::spawn(maintain(swarm, bitcoin, cycle, tick, confirmations));
        Ok(Self {
            discovered,
            cycle_start,
            task,
        })
    }

    #[must_use]
    pub fn discovered(&self) -> Discovered {
        self.discovered.clone()
    }

    pub fn publish_cycle(&self, cycle: Option<ConsensusHash>) {
        self.cycle_start.send_replace(cycle);
    }

    #[must_use]
    pub fn is_running(&self) -> bool {
        !self.task.is_finished()
    }
}

impl Drop for OutboundNetwork {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn maintain(
    mut swarm: Swarm,
    bitcoin: crate::burnchain::BurnchainSource,
    cycle: watch::Receiver<Option<ConsensusHash>>,
    tick: Duration,
    confirmations: u64,
) {
    loop {
        sleep(tick).await;
        let view = advertised_view(&bitcoin, confirmations);
        let cycle_start = *cycle.borrow();
        let round = swarm.maintain(view, cycle_start).await;
        if round.dialled > 0 || round.dropped > 0 || round.isolated > 0 {
            println!(
                "p2p: {} connected ({} new, {} lost, {} isolated), {} addresses learned",
                round.connected, round.dialled, round.dropped, round.isolated, round.learned
            );
        }
    }
}

fn identity(directory: &Path) -> Result<nano_crypto::StacksPrivateKey, Box<dyn Error>> {
    let path = directory.join("p2p-seed");
    if let Ok(seed) = fs::read(&path)
        && seed.len() == 32
    {
        return Ok(nano_crypto::StacksPrivateKey::from_seed(&seed));
    }
    let mut seed = [0; 32];
    getrandom::fill(&mut seed).map_err(|error| format!("cannot draw a p2p identity: {error}"))?;
    fs::write(path, seed)?;
    Ok(nano_crypto::StacksPrivateKey::from_seed(&seed))
}

fn advertised_view<S: nano_bitcoin::BitcoinSource>(bitcoin: &S, confirmations: u64) -> ChainView {
    let Some((height, settled)) = bitcoin.tip_height().ok().and_then(|height| {
        height
            .checked_sub(confirmations)
            .map(|settled| (height, settled))
    }) else {
        return stale_view(confirmations);
    };
    let (Ok(tip), Ok(stable)) = (
        bitcoin.block_hash_at(height),
        bitcoin.block_hash_at(settled),
    ) else {
        return stale_view(confirmations);
    };
    ChainView::with_stable_confirmations(
        height,
        BitcoinHeaderHash::from_bytes(tip),
        BitcoinHeaderHash::from_bytes(stable),
        confirmations,
    )
    .unwrap_or_else(|| stale_view(confirmations))
}

fn stale_view(confirmations: u64) -> ChainView {
    ChainView::with_stable_confirmations(
        100_000,
        BitcoinHeaderHash::from_bytes([0; 32]),
        BitcoinHeaderHash::from_bytes([0; 32]),
        confirmations,
    )
    .expect("configuration requires nonzero stable confirmations")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::identity;

    #[test]
    fn outbound_identity_is_private_persistent_and_offers_no_service() {
        let directory = tempfile::tempdir().expect("directory");
        let first = identity(directory.path()).expect("first identity");
        let second = identity(directory.path()).expect("same identity");
        assert_eq!(
            first.public_key().to_bytes_compressed(),
            second.public_key().to_bytes_compressed()
        );

        let local = nano_p2p::LocalPeer::quiet(first, 20444);
        assert_eq!(local.services, 0);
        assert!(local.data_url.is_empty());
        assert_eq!(
            fs::metadata(directory.path().join("p2p-seed"))
                .unwrap()
                .len(),
            32
        );
    }
}
