//! Every `#[ignore]` is accounted for by name, in a file, with an owner.
//!
//! The release report used to decide whether an ignored test was an environment
//! problem or a Clarity differential by searching its reason for substrings — and
//! one of the markers it searched for was `needs to be implemented`, which filed
//! *"Clarity 4 costs needs to be implemented"* under environment. A cost decides
//! block admission even where the state root matches, so that is a differential
//! being waived by a prose coincidence.
//!
//! `ignored-tests.toml` replaces the guess, keyed by the test's own name — twelve
//! sites share one reason string and are not one thing. This is the gate that
//! keeps it honest: a new `#[ignore]` whose test is not in the inventory fails here,
//! which is the "adding an unowned ignore fails CI" half of [[085]]. The report
//! reads the same file and counts anything unlisted as `unclassified`, which it
//! treats exactly as `semantic` — so the undecided case cannot be the quiet one.
//!
//! What this deliberately does *not* do is assert that nothing is blocking. Five
//! entries are measured semantic differentials the release cannot ship with, and
//! hiding them behind a green test is the failure mode this whole file exists to
//! prevent. Task 053's report is where that count has to reach zero.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root is two above this crate")
        .to_path_buf()
}

/// Every `#[ignore]` reason under `root`, with where it is.
fn ignored_reasons(root: &Path, found: &mut Vec<(String, String, String)>) {
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
            // Keyed by the test's own name, not by its reason. Twelve sites share
            // the wording "test system needs to be improved relative to versioning
            // and epochs", and they are not one thing: some are words epoch 4.0
            // removed and some are `asserts!` and `as-contract`, which it very much
            // has. One key for both would classify the second by the first.
            // The first declaration below the attribute, whatever else sits
            // between them: `#[ignore]` may come before `#[test]`, and the
            // declaration may be `async fn`, `pub fn` or a `proptest!` body's
            // plain `fn`. Anything else in between is another attribute.
            let name = source
                .lines()
                .skip(line + 1)
                .take(8)
                .find_map(|next| {
                    let next = next.trim_start();
                    let rest = ["fn ", "async fn ", "pub fn ", "pub async fn "]
                        .into_iter()
                        .find_map(|prefix| next.strip_prefix(prefix))?;
                    Some(
                        rest.split(['(', '<', ' '])
                            .next()
                            .unwrap_or(rest)
                            .to_owned(),
                    )
                })
                .unwrap_or_else(|| format!("{shown}:{}", line + 1));
            found.push((format!("{shown}:{}", line + 1), name, reason));
        }
    }
}

/// The test names the inventory accounts for.
fn inventoried() -> BTreeSet<String> {
    let text = fs::read_to_string(workspace_root().join("ignored-tests.toml"))
        .expect("ignored-tests.toml is part of the release gate and has to be there");
    text.lines()
        .map(str::trim)
        .filter(|line| line.starts_with("test"))
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
    let unaccounted: Vec<&(String, String, String)> = found
        .iter()
        .filter(|(_, name, _)| !known.contains(name))
        .collect();
    assert!(
        unaccounted.is_empty(),
        "{} ignored test(s) are not in ignored-tests.toml. Add each by name there with a \
         class and an owner task before ignoring the test:\n{}",
        unaccounted.len(),
        unaccounted
            .iter()
            .map(|(where_, name, reason)| format!("  {where_} {name}: {reason}"))
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
    let live: BTreeSet<&str> = found.iter().map(|(_, name, _)| name.as_str()).collect();
    let stale: Vec<String> = inventoried()
        .into_iter()
        .filter(|name| !live.contains(name.as_str()))
        .collect();
    assert!(
        stale.is_empty(),
        "ignored-tests.toml accounts for {} test(s) that are no longer ignored; remove them:\n  {}",
        stale.len(),
        stale.join("\n  ")
    );
}
