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

/// `developer-mode` stays on, deliberately.
///
/// It is in `stacks-common`'s default features, so a stock mainnet node runs
/// with it, and all it does is keep source spans on AST nodes. Turning it off to
/// tidy the feature list would make nano's parser produce different diagnostics
/// from the network's for no benefit — the opposite of the point of this file.
/// Asserted rather than merely allowed, so switching it off is a decision
/// somebody has to make on purpose.
#[test]
fn the_node_keeps_developer_mode_because_the_network_does() {
    assert!(
        tree("features").contains("stacks-common feature \"developer-mode\""),
        "developer-mode is a stacks-common default and mainnet runs with it"
    );
}
