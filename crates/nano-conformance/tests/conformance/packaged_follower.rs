//! The exact Nix follower package, through checkpoint, P2P, fork and restart.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use nano_chainstate::{ChainState, NakamotoBlock};
use nano_p2p::wire::{ChainView, PeerAddress, services};
use nano_p2p::{InboundLimits, Listener, LocalPeer, Protocol, Service};
use nano_primitives::BitcoinHeaderHash;

use crate::{
    binary_restart::{
        PATIENCE, PreparedCheckpoint, authenticated_anchor_index, free_port, prepare_checkpoint,
        serve_burnchain,
    },
    follow_path::{
        CHAIN_ID, Policy, Served, alternative_history, captured_burnchain, captured_chain,
        fixtures, pox, serve, snapshots,
    },
};

const STABLE_CONFIRMATIONS: u64 = 7;
const SERVED_BLOCKS: usize = 12;

struct Advertiser(ChainView);

impl Service for Advertiser {
    fn chain_view(&self) -> ChainView {
        self.0
    }

    fn neighbors(&self) -> Vec<nano_p2p::NeighborAddress> {
        Vec::new()
    }
}

async fn advertise(endpoint: String) -> (String, tokio::task::JoinHandle<()>) {
    let blocks = captured_burnchain();
    let tip_height = *blocks.keys().next_back().expect("captured Bitcoin tip");
    let stable_height = tip_height - STABLE_CONFIRMATIONS;
    let view = ChainView::with_stable_confirmations(
        tip_height,
        BitcoinHeaderHash::from_bytes(blocks[&tip_height].hash),
        BitcoinHeaderHash::from_bytes(blocks[&stable_height].hash),
        STABLE_CONFIRMATIONS,
    )
    .expect("captured P2P view");
    let listener = Listener::bind("127.0.0.1:0".parse().expect("loopback"))
        .await
        .expect("P2P listener");
    let address = listener.local_addr().expect("P2P address");
    let mut local = LocalPeer::quiet(
        nano_crypto::StacksPrivateKey::from_seed(b"packaged follower source"),
        address.port(),
    );
    local.address = PeerAddress::from_ip(address.ip());
    local.services = services::RPC;
    local.data_url = endpoint;
    let service = Arc::new(Advertiser(view));
    let task = tokio::spawn(async move {
        let mut conversations = tokio::task::JoinSet::new();
        while let Ok((stream, from)) = listener.accept().await {
            let local = local.clone();
            let service = Arc::clone(&service);
            conversations.spawn(async move {
                let _ = nano_p2p::serve_peer(
                    stream,
                    from,
                    &local,
                    Protocol::testnet()
                        .with_stable_confirmations(STABLE_CONFIRMATIONS)
                        .expect("nonzero confirmations"),
                    service.as_ref(),
                    InboundLimits {
                        timeout: Duration::from_secs(10),
                        ..InboundLimits::default()
                    },
                )
                .await;
            });
        }
    });
    (address.to_string(), task)
}

fn write_config(
    directory: &Path,
    checkpoint: &PreparedCheckpoint,
    seed: &str,
    burnchain: &str,
    health: u16,
    metrics: u16,
) -> PathBuf {
    let anchor = directory.join("anchor.bin");
    fs::write(&anchor, checkpoint.anchor.encode()).expect("write anchor");
    let activation = pox()
        .pox_5_activation_height
        .expect("captured PoX-5 activation");
    let config = format!(
        r#"
[follower]
working_dir = "{working}"
network = "testnet"
chain_id = {CHAIN_ID}
peers = []
p2p_seeds = ["{seed}"]
health_bind = "127.0.0.1:{health}"
metrics_bind = "127.0.0.1:{metrics}"
poll_interval_secs = 1
max_sync_blocks = 2
startup_peer_wait_secs = 20

[burnchain]
rest_url = "{burnchain}"
magic = "T3"
pox_5_activation_height = {activation}
stable_confirmations = {STABLE_CONFIRMATIONS}

[checkpoint]
marf = "{marf}"
source_state_id = "{source}"
state_root = "{root}"
anchor_block = "{anchor}"
anchor_bitcoin_height = {anchor_bitcoin_height}
tenure_accounting = "{accounting}"
attesting_block = "{attesting_block}"
attesting_reward_set = "{attesting_reward_set}"
sortition = "{sortition}"
authentication_history = "{authentication_history}"
"#,
        working = directory.display(),
        marf = checkpoint.marf.display(),
        source = hex::encode(checkpoint.source),
        root = checkpoint.root,
        anchor = anchor.display(),
        anchor_bitcoin_height = checkpoint.anchor_bitcoin_height,
        accounting = checkpoint.accounting.display(),
        attesting_block = checkpoint.attesting_block.display(),
        attesting_reward_set = checkpoint.attesting_reward_set.display(),
        sortition = fixtures().join("sortition").display(),
        authentication_history = checkpoint.authentication_history.display(),
    );
    let path = directory.join("follower.toml");
    fs::write(&path, config).expect("write follower config");
    path
}

struct Running {
    child: Child,
    health: u16,
    log: PathBuf,
}

impl Running {
    fn start(binary: &Path, config: &Path, health: u16, log: PathBuf) -> Self {
        let output = fs::File::options()
            .create(true)
            .append(true)
            .open(&log)
            .expect("open follower log");
        let errors = output.try_clone().expect("clone follower log");
        let child = Command::new(binary)
            .args(["start", "--config"])
            .arg(config)
            .env("NANO_TRACE_ROOTS", "1")
            .stdout(Stdio::from(output))
            .stderr(Stdio::from(errors))
            .spawn()
            .expect("start packaged follower");
        Self { child, health, log }
    }

    async fn snapshot(&self) -> Option<serde_json::Value> {
        reqwest::get(format!("http://127.0.0.1:{}/health", self.health))
            .await
            .ok()?
            .json()
            .await
            .ok()
    }

    async fn wait_for_height(&mut self, height: u64) -> serde_json::Value {
        self.wait_for(|snapshot| snapshot["stacks_height"].as_u64() >= Some(height))
            .await
    }

    async fn wait_for_error(&mut self) -> serde_json::Value {
        self.wait_for(|snapshot| snapshot["last_error"].is_string())
            .await
    }

    async fn wait_for(&mut self, ready: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let began = Instant::now();
        while began.elapsed() < PATIENCE {
            if let Some(snapshot) = self.snapshot().await
                && ready(&snapshot)
            {
                return snapshot;
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!(
                    "the packaged follower stopped with {status}:\n{}",
                    fs::read_to_string(&self.log).unwrap_or_default()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "the packaged follower did not reach its condition within {PATIENCE:?}:\n{}",
            fs::read_to_string(&self.log).unwrap_or_default()
        );
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop();
    }
}

fn packaged_binary() -> Option<PathBuf> {
    let binary = std::env::var_os("NANO_FOLLOWER_ARTIFACT").map(PathBuf::from);
    if binary.is_none() {
        nano_conformance::skip_gate(
            "NANO_FOLLOWER_ARTIFACT must name the Nix-packaged stacks-follower",
        );
    }
    binary
}

fn assert_package_identity(binary: &Path) {
    assert!(
        binary.starts_with("/nix/store"),
        "the behavioral gate ran a non-Nix binary: {}",
        binary.display()
    );
    let output = Command::new(binary)
        .arg("build-identity")
        .output()
        .expect("read packaged build identity");
    assert!(output.status.success(), "packaged build identity failed");
    let identity: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse packaged build identity");
    if let Ok(revision) = std::env::var("NANO_FOLLOWER_REVISION") {
        assert_eq!(identity["source_revision"], revision);
    }
}

fn durable_tip(directory: &Path) -> [u8; 32] {
    let tip = ChainState::open(
        nano_conformance::captured_network(&fixtures()),
        directory.join("chainstate"),
    )
    .expect("open packaged follower state")
    .tip()
    .expect("read packaged follower tip")
    .expect("packaged follower has a tip");
    remove_harness_module_cache(directory);
    tip
}

fn assert_checkpoint(directory: &Path, source: [u8; 32]) {
    let provenance = nano_node::CheckpointProvenance::load(directory.join("chainstate"))
        .expect("read checkpoint provenance")
        .expect("checkpoint provenance exists");
    assert_eq!(provenance.checkpoint.source_state_id, source);
    assert!(provenance.attestation.is_some());
}

fn assert_outbound_only_config(path: &Path) {
    assert!(
        fs::read_to_string(path)
            .expect("read follower config")
            .contains("peers = []"),
        "the packaged gate bypassed P2P discovery"
    );
}

fn assert_forged_blocks_were_not_executed(directory: &Path, forged: &BTreeSet<[u8; 32]>) {
    let chainstate = ChainState::open(
        nano_conformance::captured_network(&fixtures()),
        directory.join("chainstate"),
    )
    .expect("open state after fork refusal");
    assert!(
        chainstate
            .executed_blocks()
            .iter()
            .all(|block| !forged.contains(block)),
        "the packaged follower executed a forged fork"
    );
    drop(chainstate);
    remove_harness_module_cache(directory);
}

fn assert_no_persistent_modules(directory: &Path) {
    assert!(
        !directory.join("chainstate/native-modules").exists(),
        "the follower persisted native modules despite its closed package policy"
    );
}

fn remove_harness_module_cache(directory: &Path) {
    let cache = directory.join("chainstate/native-modules");
    if cache.exists() {
        fs::remove_dir_all(cache).expect("remove the inspection VM's native-module cache");
    }
}

fn assert_roots(log: &Path, blocks: &[NakamotoBlock]) {
    let log = fs::read_to_string(log).expect("read packaged follower log");
    for block in blocks.iter().skip(1) {
        let height = block.header.chain_length;
        let root = block.header.state_index_root.to_string();
        assert!(
            log.lines().any(|line| {
                line.starts_with(&format!("executed {height} at burn "))
                    && line.ends_with(&format!("verified root {root}"))
            }),
            "height {height} and root {root} were not recorded"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_packaged_follower_imports_catches_up_forks_restarts_and_tracks_tip() {
    let Some(binary) = packaged_binary() else {
        return;
    };
    assert_package_identity(&binary);

    let chain = captured_chain();
    let anchor_index = authenticated_anchor_index(&chain);
    let served = chain[anchor_index..anchor_index + SERVED_BLOCKS].to_vec();
    let directory = tempfile::tempdir().expect("packaged follower directory");
    let checkpoint = prepare_checkpoint(directory.path(), &chain);
    let health = free_port().await;
    let metrics = free_port().await;
    let (burnchain, _burnchain_server) = serve_burnchain();
    let log = directory.path().join("follower.log");

    let policy = Policy::default().showing(4);
    let (honest, honest_http) =
        serve(Served::honest(served.clone(), snapshots()).under(policy.clone())).await;
    let (honest_seed, honest_p2p) = advertise(honest.base_url().to_string()).await;
    let config = write_config(
        directory.path(),
        &checkpoint,
        &honest_seed,
        &burnchain,
        health,
        metrics,
    );
    assert_outbound_only_config(&config);

    let mut follower = Running::start(&binary, &config, health, log.clone());
    let first = follower
        .wait_for_height(served[3].header.chain_length)
        .await;
    assert!(
        first["p2p_connected"]
            .as_u64()
            .is_some_and(|count| count > 0)
    );
    assert_checkpoint(directory.path(), checkpoint.source);
    policy.show(8);
    follower
        .wait_for_height(served[7].header.chain_length)
        .await;
    follower.stop();
    assert_no_persistent_modules(directory.path());
    assert_eq!(
        durable_tip(directory.path()),
        *served[7].block_id().as_bytes()
    );
    honest_http.abort();
    honest_p2p.abort();

    let fork_from = 3;
    let forged = alternative_history(&served, fork_from);
    let forged_ids = forged[fork_from..]
        .iter()
        .map(|block| *block.block_id().as_bytes())
        .collect::<BTreeSet<_>>();
    let (fork, fork_http) = serve(Served::honest(forged, snapshots())).await;
    let (fork_seed, fork_p2p) = advertise(fork.base_url().to_string()).await;
    write_config(
        directory.path(),
        &checkpoint,
        &fork_seed,
        &burnchain,
        health,
        metrics,
    );
    let mut follower = Running::start(&binary, &config, health, log.clone());
    let refused = follower.wait_for_error().await;
    let refusal = refused["last_error"].as_str().expect("fork refusal");
    assert!(
        refusal.contains("signer") || refusal.contains("miner"),
        "the forged fork failed for another reason: {refusal}"
    );
    follower.stop();
    assert_no_persistent_modules(directory.path());
    let common = *served[fork_from - 1].block_id().as_bytes();
    assert_eq!(durable_tip(directory.path()), common);
    assert_forged_blocks_were_not_executed(directory.path(), &forged_ids);
    fork_http.abort();
    fork_p2p.abort();

    let (honest, honest_http) = serve(Served::honest(served.clone(), snapshots())).await;
    let (honest_seed, honest_p2p) = advertise(honest.base_url().to_string()).await;
    write_config(
        directory.path(),
        &checkpoint,
        &honest_seed,
        &burnchain,
        health,
        metrics,
    );
    let mut follower = Running::start(&binary, &config, health, log.clone());
    follower
        .wait_for_height(served.last().expect("served tip").header.chain_length)
        .await;
    follower.stop();
    assert_no_persistent_modules(directory.path());
    let tip = served.last().expect("served tip");
    assert_eq!(durable_tip(directory.path()), *tip.block_id().as_bytes());
    assert_roots(&log, &served);
    assert_no_persistent_modules(directory.path());

    honest_http.abort();
    honest_p2p.abort();
}
