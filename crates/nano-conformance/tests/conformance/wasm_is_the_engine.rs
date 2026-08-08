//! clarity-wasm is the node's only execution engine, and the boundary is
//! structural rather than a switch.
//!
//! The interpreter is the differential oracle clarity-wasm is checked against.
//! Letting it *execute* means the chain advances on results the engine under
//! test never produced — which is exactly what happened here: a replay reported
//! depth 8,673,863 while the compiler had actually stopped at 8,668,161, and the
//! blocks in between were the interpreter's answers, not nano's.
//!
//! A runtime guard is not enough for that. "Fallback disabled" leaves a dormant
//! branch, an environment-gated branch and a crosscheck that happens to discard
//! its result — all production interpreter paths. So the interpreter lives in
//! `nano-oracle`, a crate the shipped binary does not depend on, and these tests
//! check that rather than any flag: a node cannot execute through an engine that
//! is not linked into it.

use std::{fs, path::Path, process::Command};

/// Crates the shipped `stacks-node` is built from.
const PRODUCTION: [&str; 16] = [
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
    "nano-sync",
    "nano-stackerdb",
    "nano-signer",
    "nano-miner",
    "nano-rpc",
    "nano-node",
];

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root is two levels above this crate")
}

/// The interpreter crate is not in the shipped binary's dependency closure.
///
/// This is the whole boundary: no environment variable, configuration field,
/// Cargo feature or failure mode can reach an engine that was never linked in.
#[test]
fn the_node_does_not_depend_on_the_interpreter_oracle() {
    let output = Command::new(env!("CARGO"))
        .args([
            "tree",
            "--package",
            "nano-node",
            "--edges",
            "normal",
            "--prefix",
            "none",
        ])
        .current_dir(workspace())
        .output()
        .expect("cargo tree runs");
    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8_lossy(&output.stdout);
    assert!(
        !tree.lines().any(|line| line.starts_with("nano-oracle ")),
        "nano-oracle is reachable from nano-node, so the node links an interpreter"
    );
}

/// No production crate names an interpreter entry point.
///
/// A dependency on `clarity` is unavoidable — clarity-wasm consumes its frontend
/// and ABI types — so the check is on what is *called*, not on what is linked.
#[test]
fn production_crates_name_no_interpreter_entry_point() {
    // `eval_all` is the interpreter's evaluator, and
    // `initialize_versioned_contract` and `execute_transaction` are the only two
    // ways to drive a deployment or a contract call through it. None has any
    // business in a crate the node is built from.
    //
    // `OwnedEnvironment::stx_transfer` is deliberately not on this list: a token
    // transfer evaluates no Clarity, and it is the same native path stacks-core
    // takes. Forbidding the type rather than the entry points would ban the
    // node's own transfer implementation and say nothing about engines.
    const FORBIDDEN: [&str; 3] = [
        "eval_all",
        "initialize_versioned_contract",
        "environment.execute_transaction(",
    ];
    // And the oracle does use them, so this cannot pass because they were
    // renamed out from under it.
    let oracle = fs::read_to_string(workspace().join("crates/nano-oracle/src/lib.rs"))
        .expect("read the oracle");
    for symbol in FORBIDDEN {
        assert!(
            oracle.contains(symbol),
            "the oracle no longer names {symbol}, so this test proves nothing"
        );
    }
    let root = workspace().join("crates");
    let mut checked = 0_usize;
    for crate_name in PRODUCTION {
        let source = root.join(crate_name).join("src");
        for entry in walk(&source) {
            let text = fs::read_to_string(&entry).expect("read a source file");
            for symbol in FORBIDDEN {
                assert!(
                    !text.contains(symbol),
                    "{} names {symbol}, which only the interpreter needs",
                    entry.display()
                );
            }
            checked += 1;
        }
    }
    assert!(checked > 0, "no production sources were checked");
}

/// The historical switches are gone, not merely refused.
///
/// An operator who sets one of these should find that nothing reads it, which is
/// only true if no crate in the tree mentions it at all.
#[test]
fn the_historical_interpreter_switches_are_absent() {
    const RETIRED: [&str; 4] = [
        "NANO_INTERPRETER_ONLY",
        "NANO_INTERPRETER_FALLBACK",
        "NANO_CROSSCHECK",
        "NANO_CROSSCHECK_TRANSACTIONS",
    ];
    // Two files name them in order to forbid them: this one, at the source, and
    // `one_engine_in_the_artifact`, which looks for the same strings in the
    // shipped binary's data. Spelled out rather than derived from `file!()`,
    // because the second one is the release-gate half of the same question and a
    // reader should see both named here.
    const FORBIDDING: [&str; 2] = ["wasm_is_the_engine.rs", "one_engine_in_the_artifact.rs"];
    for directory in [workspace().join("crates"), workspace().join("xtask")] {
        for entry in walk(&directory) {
            if entry
                .file_name()
                .is_some_and(|name| FORBIDDING.iter().any(|allowed| name == *allowed))
            {
                continue;
            }
            let text = fs::read_to_string(&entry).expect("read a source file");
            for switch in RETIRED {
                assert!(
                    !text.contains(switch),
                    "{} still mentions {switch}",
                    entry.display()
                );
            }
        }
    }
}

/// Every `.rs` file under a directory.
fn walk(directory: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(directory) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(walk(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
    found
}
