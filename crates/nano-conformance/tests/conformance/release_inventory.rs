//! Every `#[ignore]` is accounted for by name, in a file, with an owner.
//!
//! The release report used to decide whether an ignored test was an environment
//! problem or a Clarity differential by searching its reason for substrings — and
//! one of the markers it searched for was `needs to be implemented`, which filed
//! *"Clarity 4 costs needs to be implemented"* under environment. A cost decides
//! block admission even where the state root matches, so that is a differential
//! being waived by a prose coincidence.
//!
//! `ignored-tests.toml` replaces the guess. This is the gate that keeps it
//! honest: a new `#[ignore]` whose reason is not in the inventory fails here,
//! which is the "adding an unowned ignore fails CI" half of [[085]]. The report
//! reads the same file and counts anything unlisted as `unclassified`, which it
//! treats exactly as `semantic` — so the undecided case cannot be the quiet one.
//!
//! What this deliberately does *not* do is assert that nothing is blocking. There
//! are fifteen entries the release cannot ship with, and hiding them behind a
//! green test is the failure mode this whole file exists to prevent. Task 053's
//! report is where that count has to reach zero.

use std::{collections::BTreeSet, fs, path::{Path, PathBuf}};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root is two above this crate")
        .to_path_buf()
}

/// Every `#[ignore]` reason under `root`, with where it is.
fn ignored_reasons(root: &Path, found: &mut Vec<(String, String)>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            ignored_reasons(&path, found);
            continue;
        }
        if path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        let Ok(source) = fs::read_to_string(&path) else {
            continue;
        };
        let shown = path
            .strip_prefix(workspace_root())
            .unwrap_or(&path)
            .display()
            .to_string();
        for (line, text) in source.lines().enumerate() {
            let trimmed = text.trim_start();
            // The attribute, not a mention of it in prose: a doc comment about
            // why something is *not* ignored would otherwise be counted.
            if !trimmed.starts_with("#[ignore") {
                continue;
            }
            let reason = trimmed
                .split_once('"')
                .and_then(|(_, rest)| rest.rsplit_once('"').map(|(reason, _)| reason.to_owned()))
                .unwrap_or_else(|| "no reason given".to_owned());
            found.push((format!("{shown}:{}", line + 1), reason));
        }
    }
}

/// The reasons the inventory accounts for.
fn inventoried() -> BTreeSet<String> {
    let text = fs::read_to_string(workspace_root().join("ignored-tests.toml"))
        .expect("ignored-tests.toml is part of the release gate and has to be there");
    text.lines()
        .map(str::trim)
        .filter(|line| line.starts_with("reason"))
        .filter_map(|line| {
            let (_, rest) = line.split_once('=')?;
            let inner = rest.trim().strip_prefix('"')?.strip_suffix('"')?;
            Some(inner.to_owned())
        })
        .collect()
}

#[test]
fn every_ignored_test_is_named_in_the_inventory() {
    let root = workspace_root();
    let mut found = Vec::new();
    for scanned in [
        "crates",
        "xtask",
        "vendor/clarity-wasm/clar2wasm/src",
        "vendor/clarity-wasm/clar2wasm/tests",
    ] {
        ignored_reasons(&root.join(scanned), &mut found);
    }
    assert!(
        !found.is_empty(),
        "the scan found no ignored tests at all, so it is not scanning anything"
    );

    let known = inventoried();
    let unaccounted: Vec<&(String, String)> = found
        .iter()
        .filter(|(_, reason)| !known.contains(reason))
        .collect();
    assert!(
        unaccounted.is_empty(),
        "{} ignored test(s) are not in ignored-tests.toml. Add each reason there with a \
         class and an owner task before ignoring the test:\n{}",
        unaccounted.len(),
        unaccounted
            .iter()
            .map(|(where_, reason)| format!("  {where_}: {reason}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Every entry in the inventory still describes a test that exists.
///
/// The other direction, and it is the one that rots: a reason left behind after
/// the test it covered was fixed or deleted is a waiver nobody is watching, and
/// the next test to reuse that wording inherits it silently.
#[test]
fn the_inventory_names_no_test_that_is_gone() {
    let root = workspace_root();
    let mut found = Vec::new();
    for scanned in [
        "crates",
        "xtask",
        "vendor/clarity-wasm/clar2wasm/src",
        "vendor/clarity-wasm/clar2wasm/tests",
    ] {
        ignored_reasons(&root.join(scanned), &mut found);
    }
    let live: BTreeSet<&str> = found.iter().map(|(_, reason)| reason.as_str()).collect();
    let stale: Vec<String> = inventoried()
        .into_iter()
        .filter(|reason| !live.contains(reason.as_str()))
        .collect();
    assert!(
        stale.is_empty(),
        "ignored-tests.toml accounts for {} reason(s) no test gives any more; remove them:\n  {}",
        stale.len(),
        stale.join("\n  ")
    );
}
