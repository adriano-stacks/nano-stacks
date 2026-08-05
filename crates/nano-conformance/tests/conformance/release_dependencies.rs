//! What the shipped node is allowed to be built from.
//!
//! Reusing clarity-wasm necessarily brings `clarity`, `clarity-types` and
//! `stacks-common` into the VM's ABI — the frontend, the value types, the
//! database traits and the cost machinery are inseparable from the engine. That
//! is the whole of the allowed reference surface. Anything else is either a
//! reference *node* (a second implementation of the thing under test) or test
//! behaviour, and neither belongs in a release build.
//!
//! Cargo unifies features across a build graph, so this cannot be left to
//! inspection: one dev dependency asking for `clarity/testing` anywhere in the
//! workspace used to be enough to put test schedules and overrides into
//! `stacks-node` itself. `cargo tree` is the only thing that actually knows, so
//! it is what these tests ask.

use std::process::Command;

/// The crates the shipped `stacks-node` is built from.
const PRODUCTION: [&str; 17] = [
    "nano-primitives",
    "nano-crypto",
    "nano-address",
    "nano-codec",
    "nano-bitcoin",
    "nano-sortition",
    "nano-marf",
    "nano-mempool",
    "nano-vm",
    "nano-chainstate",
    "nano-p2p",
    "nano-sync",
    "nano-stackerdb",
    "nano-signer",
    "nano-miner",
    "nano-rpc",
    "nano-node",
];

fn workspace() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("the workspace root is two levels above this crate")
        .to_path_buf()
}

fn tree(edges: &str) -> String {
    let output = Command::new(env!("CARGO"))
        .args(["tree", "--package", "nano-node", "--edges", edges])
        .current_dir(workspace())
        .output()
        .expect("cargo tree runs");
    assert!(
        output.status.success(),
        "cargo tree --edges {edges} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// No reference crate is built with its test behaviour.
///
/// `testing` is what pulls in substitutable coinbase and emission schedules,
/// global test overrides and faucet helpers. A node that observes those is not
/// observing mainnet.
#[test]
fn the_node_enables_no_reference_test_feature() {
    let tree = tree("features");
    for crate_name in ["clarity", "clarity-types", "stacks-common", "stacks_common"] {
        let wanted = format!("{crate_name} feature \"testing\"");
        assert!(
            !tree.contains(&wanted),
            "nano-node enables {wanted}, so it is built with reference test behaviour"
        );
    }
}

/// No second implementation of the chain is linked into the node.
///
/// `stackslib` is stacks-core's chainstate — the thing nano is a
/// reimplementation of — and the codec, signer and `StackerDB` libraries are the
/// parts nano rewrote. They are legitimate *oracles*, which is why
/// `nano-conformance` takes them as dev-dependencies, and linking one into the
/// node would mean a release could answer from the implementation under test.
#[test]
fn the_node_links_no_reference_node_crate() {
    let tree = tree("normal");
    for crate_name in [
        "stackslib",
        "stacks-codec",
        "libsigner",
        "libstackerdb",
        "pox-locking",
    ] {
        assert!(
            !tree.contains(&format!("{crate_name} v")),
            "nano-node links {crate_name}, which is a reference implementation of \
             something nano implements itself"
        );
    }
}

/// Every production crate builds on its own, without a dev-dependency's features.
///
/// `cargo build --workspace --all-targets` unifies features across everything it
/// builds, so `nano-conformance`'s `stackslib = { features = ["testing"] }` was
/// quietly making `clarity/testing` available to crates that must not need it —
/// and a whole-workspace build reported clean while `cargo build -p xtask` did
/// not. Building each production crate alone is what actually asks the question.
#[test]
fn the_production_closure_compiles_without_a_dev_dependencys_features() {
    // One `cargo check` over exactly the production crates: features unify across
    // a build graph, and a graph containing only these crates is one no
    // dev-dependency can reach into. `check` rather than `build` because the
    // question is whether it compiles, and a release build of all sixteen took
    // four minutes — a gate against a slow loop should not be the slow part of it.
    // `--release --tests`, not `check --all-targets`: a debug-profile check builds
    // a whole second graph of the same crates, and this workspace only ever builds
    // release -- 452 GB of `target/debug` had accumulated for no other reason.
    // `--tests` because `--all-targets` adds benches and examples, which answer
    // nothing this asks.
    let mut command = Command::new(env!("CARGO"));
    command.arg("check").arg("--release").arg("--tests");
    for crate_name in PRODUCTION {
        command.arg("--package").arg(crate_name);
    }
    let output = command
        .current_dir(workspace())
        .output()
        .expect("cargo check runs");
    assert!(
        output.status.success(),
        "the production crates do not compile on their own, so one of them depends \
         on a feature something else in the workspace turns on:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The oracles are still available where they belong.
///
/// Half of this boundary is that `nano-conformance` keeps the pinned reference
/// implementation. A test suite that lost it would report green because it
/// stopped comparing.
#[test]
fn the_conformance_suite_still_takes_the_reference_implementation() {
    let manifest = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
    )
    .expect("read this crate's manifest");
    for crate_name in ["stackslib", "stacks-common", "clarity"] {
        assert!(
            manifest.contains(&format!("{crate_name} = ")),
            "nano-conformance no longer depends on {crate_name}, so it has no oracle"
        );
    }
}

/// The operator procedure describes the file the node actually writes.
///
/// `docs/checkpoint-trust.md` is how somebody adopts a mainnet checkpoint, and
/// its worked example of `checkpoint-provenance.toml` is the part they will
/// compare their own file against. A renamed field would make the document
/// quietly wrong in the one place an operator checks by eye, so every key the
/// document shows has to be a key the node still writes.
#[test]
fn the_checkpoint_procedure_names_the_fields_the_node_writes() {
    let doc = std::fs::read_to_string(workspace().join("docs/checkpoint-trust.md"))
        .expect("read the checkpoint procedure");
    let written = nano_marf::CheckpointProvenance {
        checkpoint: nano_marf::CheckpointManifest {
            format: "stacks-core-marf-sqlite-v2".to_owned(),
            stacks_height: 400,
            source_state_id: [0x59; 32],
            state_index_root: nano_primitives::TrieHash::from_bytes([0x34; 32]),
            first_bitcoin_height: 277,
        },
        attestation: Some(nano_node::CheckpointAttestation {
            attesting_block_id: [0x59; 32],
            signer_weight: 12,
            approval_threshold: 9,
        }),
    };
    // Written through the real `record`, so the keys compared are the ones a node
    // puts on disk rather than the ones a test thought it would. In a fresh
    // directory each time: this used to be one fixed path under the temp
    // directory, which two runs of the suite at once would delete under each
    // other.
    let directory = tempfile::tempdir().expect("a directory");
    written.record(directory.path()).expect("record provenance");
    let toml = std::fs::read_to_string(directory.path().join("checkpoint-provenance.toml"))
        .expect("read the recorded provenance");
    let mut checked = 0_usize;
    for line in toml.lines() {
        let Some((key, _)) = line.split_once(" = ") else {
            continue;
        };
        assert!(
            doc.contains(&format!("{key} = ")),
            "the node writes `{key}` and docs/checkpoint-trust.md does not show it"
        );
        checked += 1;
    }
    assert!(checked >= 8, "only {checked} provenance keys were checked");
}

/// `developer-mode` stays on, deliberately.
///
/// All it does is keep source spans on AST nodes, and a stock mainnet node has
/// it — so turning it off to tidy the feature list would make nano's parser
/// produce different diagnostics from the network's for no benefit, the opposite
/// of the point of this file. It is now asked for by name rather than inherited
/// from `stacks-common`'s default (see the two `Cargo.toml` lines that spell the
/// feature list out), which is why this is asserted: switching it off is a
/// decision somebody has to make on purpose.
#[test]
fn the_node_keeps_developer_mode_because_the_network_does() {
    assert!(
        tree("features").contains("stacks-common feature \"developer-mode\""),
        "developer-mode is what keeps source spans, and mainnet runs with it"
    );
}

/// The reference crates' features are the ones nano chose, and no others.
///
/// `clarity` asks for `stacks_common` with `default-features = false`, so
/// everything `stacks-common`'s own default carries reached nano through nano's
/// *own* two direct dependencies on it — `nano-vm`'s and vendored `clar2wasm`'s.
/// That put `ctrlc-handler` in the release graph: two extra crates and a SIGINT
/// handler nano never installs, next to the SIGTERM handling it does have. Both
/// lines now spell the list out, so a future `stacks-common` release cannot add
/// to nano's closure by adding to its own default.
///
/// Asserted as an exact set rather than a deny-list. A deny-list only refuses
/// what somebody already thought of, and the whole failure mode here is a
/// feature arriving that nobody chose.
#[test]
fn the_node_enables_only_the_reference_features_it_asked_for() {
    let tree = tree("features");
    let enabled: std::collections::BTreeSet<&str> = tree
        .lines()
        .filter_map(|line| {
            let feature = line
                .trim_start_matches(['│', '├', '└', '─', ' '])
                .strip_prefix("stacks-common feature \"")
                .or_else(|| {
                    line.trim_start_matches(['│', '├', '└', '─', ' '])
                        .strip_prefix("stacks_common feature \"")
                })?;
            feature.strip_suffix('"')
        })
        .collect();

    // `developer-mode` for the parser's source spans, `rand` because the
    // reference's own crypto helpers need it, `rusqlite` because the Clarity
    // database ABI is SQLite-backed. Nothing else is required to compile the VM
    // boundary, which is all nano takes from these crates. `default` is
    // *absent*, which is the change: the set is now chosen rather than inherited.
    let wanted = ["developer-mode", "rand", "rusqlite"];
    assert_eq!(
        enabled,
        wanted.into_iter().collect(),
        "the release graph's stacks-common features are not the ones nano asked for"
    );
}
