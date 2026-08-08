//! Killing the shipped binary, over and over, and starting it again.
//!
//! `restart.rs` and `kill_during_replay.rs` prove the durability invariants at
//! the library level, including twenty `SIGKILL`s a run. What the release gate
//! ([[053]]) asks for and they do not give is the **assembled binary**: the
//! process an operator runs, with its own startup, its own checkpoint adoption,
//! its own configuration and its own recovery path. A library test cannot reach
//! any of those, and every one of them is a place a node can fail to come back.
//!
//! What stands the whole environment up offline: the captured 340-block chain
//! served over loopback as two Stacks peers, and its Bitcoin blocks served as a
//! fake Esplora — the same two endpoints (`block-height/:n`, `block/:hash/raw`)
//! `BitcoinRestSource` reads, so the node's own burnchain source is exercised
//! rather than replaced. Nothing is written to any state a node is running
//! against: each start gets a directory of its own, and the fixtures are read.
//!
//! The kill is `SIGKILL`, at a height the node reports itself, so it lands in the
//! middle of whatever the next block was doing — the commit boundaries the gate
//! names. After each one the state directory is opened *while nothing writes it*,
//! which is the difference between this and the reflink copy [[053]] records as
//! not being a snapshot.
//!
//! What is asserted, and why each one is a separate claim:
//!
//! - the state opens after every kill, which is what "no partially committed
//!   block" means in practice: a torn write that recovery cannot read is a node
//!   that never comes back;
//! - the durable tip is always a block of the canonical chain, never a block
//!   nobody signed and never a half-written one;
//! - the tip never goes backwards across a restart;
//! - the node makes progress after each restart, so the state is usable rather
//!   than merely readable;
//! - it ends on the served tip, having verified every state root on the way —
//!   the executor refuses a block whose root differs, so reaching the tip *is*
//!   the root check for every block below it, and every executed height is
//!   recorded here rather than sampled.

use std::{
    collections::BTreeMap,
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use axum::{Router, extract::State, http::StatusCode, routing::get};
use nano_chainstate::{ChainState, NakamotoBlock};

use crate::follow_path::{CHAIN_ID, Served, captured_chain, fixtures, serve, snapshots};

/// Blocks the peers serve, and so the height the node has to reach.
const SERVED_BLOCKS: usize = 12;

/// How many times the node is killed and started again.
///
/// Each run waits for a height *above* the one the run before it reached, so every
/// kill interrupts work this process did rather than landing on a node with
/// nothing left to do — which is what makes this failure injection at every block
/// commit boundary rather than nine kills at the same one. Nine of the eleven
/// blocks above the anchor, leaving the last for the run that finishes — which is
/// why `max_sync_blocks` is one: a round that executed more would run past the
/// blocks the peers serve before the kills were spent.
const KILLS: usize = 9;

/// How long to wait for a height before calling the node stuck.
pub const PATIENCE: Duration = Duration::from_mins(1);

/// Build the shipped binary and say where it landed.
///
/// Asked of cargo rather than assembled from `target/release/stacks-node`, for
/// the same reason `one_engine_in_the_artifact` asks: a `CARGO_TARGET_DIR`, a
/// `--target` triple or a workspace `build.target-dir` each move it, and a test
/// that ran a stale path would report on a binary nobody built.
pub fn artifact() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| {
        let output = Command::new(env!("CARGO"))
            .args([
                "build",
                "--release",
                "--bin",
                "stacks-node",
                "--message-format",
                "json-render-diagnostics",
            ])
            .current_dir(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .and_then(Path::parent)
                    .expect("the workspace root is two levels above this crate"),
            )
            .output()
            .expect("cargo build runs");
        assert!(
            output.status.success(),
            "the release binary does not build: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|message| message["reason"] == "compiler-artifact")
            .filter_map(|message| message["executable"].as_str().map(PathBuf::from))
            .find(|path| path.file_name().is_some_and(|name| name == "stacks-node"))
            .expect("cargo reports where it put stacks-node")
    })
}

/// The captured Bitcoin blocks, by the hash the node will ask for them under.
///
/// Keyed by `hex::encode(BitcoinBlock::hash)` rather than by the file name: what
/// has to agree is the hash this fake answers at a height and the hash the node
/// computes from the bytes, because `BitcoinRestSource` compares the two on every
/// read and calls a difference a reorganization.
fn burnchain_files() -> (BTreeMap<u64, String>, BTreeMap<String, Vec<u8>>) {
    let mut hashes = BTreeMap::new();
    let mut bodies = BTreeMap::new();
    for row in snapshots() {
        let path = fixtures()
            .join("bitcoin/blocks")
            .join(format!("{}.hex", row.burn_header_hash));
        let Ok(encoded) = fs::read_to_string(&path) else {
            continue;
        };
        let raw = hex::decode(encoded.trim()).expect("the captured block is hexadecimal");
        let block = nano_bitcoin::decode_block(row.block_height, &raw, *b"T3")
            .expect("a captured Bitcoin block decodes");
        let hash = hex::encode(block.hash);
        hashes.insert(row.block_height, hash.clone());
        bodies.insert(hash, raw);
    }
    (hashes, bodies)
}

/// Serve the captured Bitcoin blocks the way Esplora does.
pub async fn serve_burnchain() -> (String, tokio::task::JoinHandle<()>) {
    type Burnchain = Arc<(BTreeMap<u64, String>, BTreeMap<String, Vec<u8>>)>;
    let (hashes, bodies) = burnchain_files();
    let state: Burnchain = Arc::new((hashes, bodies));
    let router = Router::new()
        .route(
            "/block-height/{height}",
            get(
                |State(state): State<Burnchain>,
                 axum::extract::Path(height): axum::extract::Path<u64>| async move {
                    state.0.get(&height).map_or_else(
                        || (StatusCode::NOT_FOUND, String::new()),
                        |hash| (StatusCode::OK, hash.clone()),
                    )
                },
            ),
        )
        .route(
            "/block/{hash}/raw",
            get(
                |State(state): State<Burnchain>,
                 axum::extract::Path(hash): axum::extract::Path<String>| async move {
                    state.1.get(&hash.to_lowercase()).map_or_else(
                        || (StatusCode::NOT_FOUND, Vec::new()),
                        |raw| (StatusCode::OK, raw.clone()),
                    )
                },
            ),
        )
        // Where this burnchain ends, which is what bounds the node's walk forward.
        // Without it the walk is skipped entirely and the node cannot name a burn
        // view above its seed — so it refused every block of the capture past the
        // first tenure, correctly and for a reason that was this rig's and not the
        // node's.
        .route(
            "/blocks/tip/height",
            get(|State(state): State<Burnchain>| async move {
                state.0.keys().next_back().map_or_else(
                    || (StatusCode::NOT_FOUND, String::new()),
                    |height| (StatusCode::OK, height.to_string()),
                )
            }),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind a loopback port");
    let address = listener.local_addr().expect("the bound address");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{address}"), handle)
}

/// A free loopback port, released before the node is told to bind it.
///
/// There is a race here and it is the smallest one available: nothing else in
/// this binary binds a fixed port, and the alternative — a port written down —
/// collides with whatever else the machine is running.
pub async fn free_port() -> u16 {
    tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind a loopback port")
        .local_addr()
        .expect("the bound address")
        .port()
}

/// The configuration file the node is started with.
///
/// Two peers, because the gate asks for catch-up over more than one and because a
/// node that followed only the first would not be exercising the pool. No
/// `[miner]` and no `[signer]`: this is the follower, and the roles are switched
/// on by their tables being present.
pub fn write_config(
    directory: &Path,
    peers: &[String],
    burnchain: &str,
    rpc: u16,
    anchor: &NakamotoBlock,
    anchor_bitcoin_height: u64,
) -> PathBuf {
    let fixtures = fixtures();
    let checkpoint = fixtures.join("chainstate/checkpoint-H");
    let manifest = nano_node::CheckpointManifest::load(&checkpoint).expect("the manifest reads");
    let anchor_path = directory.join("anchor.bin");
    fs::write(&anchor_path, anchor.encode()).expect("the anchor block is written");
    let peers = peers
        .iter()
        .map(|peer| format!("\"{peer}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let config = format!(
        r#"
[node]
working_dir = "{working}"
chain_id = {CHAIN_ID}
peers = [{peers}]
p2p_seeds = []
rpc_bind = "127.0.0.1:{rpc}"
poll_interval_secs = 1
max_sync_blocks = 1

[burnchain]
rest_url = "{burnchain}"
magic = "T3"

[checkpoint]
marf = "{marf}"
source_state_id = "{source}"
state_root = "{root}"
anchor_block = "{anchor_path}"
anchor_bitcoin_height = {anchor_bitcoin_height}
tenure_accounting = "{accounting}"
sortition = "{sortition}"
"#,
        working = directory.display(),
        marf = checkpoint.join("marf.sqlite").display(),
        source = hex::encode(manifest.source_state_id),
        root = manifest.state_index_root,
        anchor_path = anchor_path.display(),
        accounting = checkpoint.join("native-effects.json").display(),
        // The capture's own burn/sortition history, which a node that executes
        // blocks refuses to start without: it derives every burn view from this
        // rather than from a peer, and the consensus hashes behind the seed are
        // what let the skip-list reach past the checkpoint. The in-process rigs
        // have always been given it; these two were started without it and
        // reported exactly that.
        sortition = fixtures.join("sortition").display(),
    );
    let path = directory.join("config.toml");
    fs::write(&path, config).expect("the configuration is written");

    // The checkpoint is adopted here rather than by the node, because attesting it
    // needs the block that *sealed* it and the capture does not hold one: its
    // blocks start above the checkpoint's own height. So the provenance is recorded
    // with no attestation, which is exactly what the node writes for a checkpoint
    // it was given no attesting block for — and it is why this test says nothing
    // about checkpoint trust, which `mainnet_checkpoint` covers.
    let chainstate = directory.join("chainstate");
    fs::create_dir_all(&chainstate).expect("a state directory");
    nano_node::CheckpointProvenance {
        checkpoint: manifest,
        attestation: None,
    }
    .record(&chainstate)
    .expect("the provenance is recorded");
    path
}

/// A running node, and where it writes.
pub struct Running {
    child: Child,
    pub rpc: u16,
    pub log: PathBuf,
}

impl Running {
    pub fn start(config: &Path, rpc: u16, log: PathBuf) -> Self {
        let output = fs::File::options()
            .create(true)
            .append(true)
            .open(&log)
            .expect("the log opens");
        let errors = output.try_clone().expect("the log clones");
        let child = Command::new(artifact())
            .args(["start", "--config"])
            .arg(config)
            // Every executed height and the root its header committed to, which is
            // the record the release gate asks for. A switch rather than the
            // default because a mainnet catch-up would print thirty thousand of
            // these.
            .env("NANO_TRACE_ROOTS", "1")
            .stdout(Stdio::from(output))
            .stderr(Stdio::from(errors))
            .spawn()
            .expect("the node starts");
        Self { child, rpc, log }
    }

    /// The height the node says it has *executed*, or nothing if it is not
    /// answering yet.
    ///
    /// `/v2/info`'s `stacks_tip_height` is published from the executed tip rather
    /// than from the peer's, which is the distinction the release gate turns on.
    pub async fn executed_height(&self) -> Option<u64> {
        let body = reqwest::get(format!("http://127.0.0.1:{}/v2/info", self.rpc))
            .await
            .ok()?
            .text()
            .await
            .ok()?;
        serde_json::from_str::<serde_json::Value>(&body)
            .ok()?
            .get("stacks_tip_height")?
            .as_u64()
    }

    /// Wait until the node reports having executed `height`, or say what it did
    /// instead.
    async fn wait_for(&mut self, height: u64) -> u64 {
        let started = Instant::now();
        let mut last = 0;
        while started.elapsed() < PATIENCE {
            if let Some(reported) = self.executed_height().await {
                last = reported;
                if reported >= height {
                    return reported;
                }
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!(
                    "the node stopped with {status} before reaching height {height}:\n{}",
                    fs::read_to_string(&self.log).unwrap_or_default()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "the node reached height {last} and not {height} within {PATIENCE:?}:\n{}",
            fs::read_to_string(&self.log).unwrap_or_default()
        );
    }

    /// Kill it outright and wait for the process to be gone.
    ///
    /// `SIGKILL`, not `SIGTERM`: a clean shutdown is a different test, and the
    /// one that matters is the one the node has no chance to prepare for.
    pub fn kill(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A failing assertion must not leave a node running.
///
/// Learned by leaving one behind: a panic inside `wait_for` skips the kill, and the
/// orphan goes on polling peers that no longer exist for as long as the machine is
/// up — on a machine shared with other work that is somebody else's afternoon.
impl Drop for Running {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The durable tip a state directory holds, read while nothing writes it.
fn durable_tip(directory: &Path) -> Option<[u8; 32]> {
    let chainstate = ChainState::open(
        nano_conformance::captured_network(&fixtures()),
        directory.join("chainstate"),
    )
    .expect("the state directory opens after a kill");
    chainstate.tip().expect("read the durable tip")
}

/// What one kill and the restart after it has to leave behind.
///
/// Separated from the loop because each of the three is a different claim, and a
/// failure has to name which: the state opens, its tip is a block of the canonical
/// chain, and that tip is the height the node reported or the one below it — the
/// parent-or-child shape a crash between the ledger write and the MARF seal leaves.
fn check_durable_state(directory: &Path, served: &[NakamotoBlock], kill: usize, reached: u64) {
    let tip = durable_tip(directory).expect("the state is sealed at a block");
    let sealed = served
        .iter()
        .find(|block| *block.block_id().as_bytes() == tip)
        .unwrap_or_else(|| {
            panic!(
                "after kill {kill} at height {reached} the durable tip {} is not a block of \
                 the chain the peers serve",
                hex::encode(tip)
            )
        });
    let sealed_height = sealed.header.chain_length;
    assert!(
        sealed_height + 1 >= reached,
        "after kill {kill} the node had reported height {reached} and its state is sealed \
         at {sealed_height}, which is more than one block behind"
    );
}

/// Every height the node executed, with the root its header commits to.
///
/// Read out of the node's own log rather than out of the test's bookkeeping: what
/// the release gate asks to be recorded is what the node said. The root is the
/// header's, and the seal had already refused the block for differing from it — so
/// the line *is* the verified root rather than a second opinion about it.
fn check_every_height_recorded(log: &Path, served: &[NakamotoBlock]) {
    let recorded = fs::read_to_string(log).expect("the node's log reads");
    for block in served.iter().skip(1) {
        let line = format!("executed {} at burn ", block.header.chain_length);
        let root = block.header.state_index_root.to_string();
        assert!(
            recorded
                .lines()
                .any(|printed| printed.starts_with(&line) && printed.ends_with(&root)),
            "no run recorded executing height {} with root {root}",
            block.header.chain_length
        );
    }
}

/// The whole offline environment: two Stacks peers, a burnchain, a configuration.
struct Environment {
    config: PathBuf,
    log: PathBuf,
    rpc: u16,
    served: Vec<NakamotoBlock>,
    directory: tempfile::TempDir,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

async fn stand_up() -> Environment {
    let chain = captured_chain();
    let served: Vec<_> = chain[..SERVED_BLOCKS].to_vec();
    let anchor = served.first().expect("the capture has blocks").clone();
    let anchor_bitcoin_height = snapshots()
        .into_iter()
        .find(|row| row.consensus_hash == anchor.header.consensus_hash.to_string())
        .map(|row| row.block_height)
        .expect("the anchor's own burn block");

    // Two peers over the same chain, and a burnchain of this chain's own Bitcoin
    // blocks. The peers are honest here on purpose: `follow_path` is where a peer
    // lies, and this test is about the process rather than about the choice.
    let (first, first_task) = serve(Served::honest(served.clone(), snapshots())).await;
    let (second, second_task) = serve(Served::honest(served.clone(), snapshots())).await;
    let (burnchain, burnchain_task) = serve_burnchain().await;

    let directory = tempfile::tempdir().expect("a directory");
    let rpc = free_port().await;
    let config = write_config(
        directory.path(),
        &[first.base_url().to_string(), second.base_url().to_string()],
        &burnchain,
        rpc,
        &anchor,
        anchor_bitcoin_height,
    );
    Environment {
        log: directory.path().join("node.log"),
        config,
        rpc,
        served,
        directory,
        tasks: vec![first_task, second_task, burnchain_task],
    }
}

/// Kill the shipped binary at every block boundary and start it again.
#[tokio::test]
async fn the_binary_resumes_the_same_chain_after_a_kill_at_every_block() {
    let environment = stand_up().await;
    let Environment {
        config,
        log,
        rpc,
        served,
        directory,
        tasks,
    } = &environment;
    let anchor = served.first().expect("an anchor");

    let mut highest = anchor.header.chain_length;
    let mut executed_heights = Vec::new();
    for kill in 0..KILLS {
        let mut node = Running::start(config, *rpc, log.clone());
        let reached = node.wait_for(highest + 1).await;
        node.kill();
        assert!(
            reached > highest,
            "kill {kill}: the node reported height {reached} having already been at \
             {highest}, so this kill interrupted nothing"
        );
        executed_heights.push(reached);
        highest = reached;
        check_durable_state(directory.path(), served, kill, reached);
    }

    // And it finishes: started once more, it reaches the tip the peers serve.
    let tip = served.last().expect("a tip");
    let mut node = Running::start(config, *rpc, log.clone());
    node.wait_for(tip.header.chain_length).await;
    node.kill();
    assert_eq!(
        durable_tip(directory.path()),
        Some(*tip.block_id().as_bytes()),
        "the node did not end on the tip its peers serve"
    );
    assert_eq!(
        executed_heights.len(),
        KILLS,
        "a kill did not produce a reported height"
    );
    check_every_height_recorded(log, served);

    for task in tasks {
        task.abort();
    }
}
