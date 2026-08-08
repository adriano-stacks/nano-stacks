//! Every `#[ignore]` and conditional gate is accounted for by name and owner.
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
//! What this deliberately does *not* do is turn an accounted gap green. Declared
//! semantic differentials still fail the release report; naming them only prevents
//! them from hiding behind a passing count. `conditional-tests.toml` does the same
//! by stable call-site identity
//! for `skip_gate`: module, containing function and ordinal within that function.
//! Moving a line does not invalidate an entry, while adding or removing a call does.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

#[derive(Default)]
struct ConditionalEntry {
    site: String,
    class: String,
    owner: String,
    job: String,
    requires: String,
    policy: String,
}

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

fn scalar(line: &str) -> Option<String> {
    let (_, value) = line.split_once('=')?;
    value
        .trim()
        .strip_prefix('"')?
        .strip_suffix('"')
        .map(str::to_owned)
}

fn conditional_inventory() -> Vec<ConditionalEntry> {
    let text = fs::read_to_string(workspace_root().join("conditional-tests.toml"))
        .expect("conditional-tests.toml is part of the release gate and has to be there");
    let mut entries = Vec::new();
    let mut current: Option<ConditionalEntry> = None;
    for line in text.lines().map(str::trim) {
        if line == "[[conditional]]" {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(ConditionalEntry::default());
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        let Some((name, _)) = line.split_once('=') else {
            continue;
        };
        let Some(value) = scalar(line) else {
            continue;
        };
        match name.trim() {
            "site" => entry.site = value,
            "class" => entry.class = value,
            "owner" => entry.owner = value,
            "job" => entry.job = value,
            "requires" => entry.requires = value,
            "policy" => entry.policy = value,
            _ => {}
        }
    }
    if let Some(entry) = current {
        entries.push(entry);
    }
    entries
}

/// Stable identities of the conditional calls in the conformance suite.
fn conditional_sites() -> BTreeSet<String> {
    let directory = workspace_root().join("crates/nano-conformance/tests/conformance");
    let mut paths: Vec<PathBuf> = fs::read_dir(directory)
        .expect("read conformance sources")
        .map(|entry| entry.expect("read conformance entry").path())
        .filter(|path| {
            path.extension().is_some_and(|extension| extension == "rs")
                && path
                    .file_stem()
                    .is_none_or(|stem| stem != "release_inventory")
        })
        .collect();
    paths.sort();
    let mut sites = BTreeSet::new();
    for path in paths {
        let module = path
            .file_stem()
            .expect("a Rust source has a stem")
            .to_string_lossy();
        let source = fs::read_to_string(&path).expect("read conformance source");
        let mut function = "<module>".to_owned();
        let mut ordinals: BTreeMap<String, usize> = BTreeMap::new();
        for line in source.lines() {
            let trimmed = line.trim_start();
            for prefix in ["fn ", "async fn ", "pub fn ", "pub async fn "] {
                if let Some(rest) = trimmed.strip_prefix(prefix) {
                    function = rest
                        .split(['(', '<', ' '])
                        .next()
                        .unwrap_or(rest)
                        .to_owned();
                    break;
                }
            }
            let calls = line.matches("nano_conformance::skip_gate(").count()
                + usize::from(trimmed.starts_with("skip_gate("));
            for _ in 0..calls {
                let ordinal = ordinals.entry(function.clone()).or_default();
                *ordinal += 1;
                sites.insert(format!("{module}::{function}#{ordinal}"));
            }
        }
    }
    sites
}

fn task_statuses(root: &Path, found: &mut BTreeMap<String, String>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            task_statuses(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "md")
            && let Ok(text) = fs::read_to_string(path)
        {
            let field = |name: &str| {
                text.lines()
                    .find_map(|line| line.strip_prefix(&format!("{name}: ")))
                    .map(|value| value.trim().trim_matches('"').to_owned())
            };
            if let (Some(id), Some(status)) = (field("id"), field("status")) {
                found.insert(id, status);
            }
        }
    }
}

fn asserted_engine_differences(root: &Path, found: &mut BTreeSet<String>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            asserted_engine_differences(&path, found);
            continue;
        }
        if path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        let Ok(source) = fs::read_to_string(path) else {
            continue;
        };
        let lines: Vec<&str> = source.lines().collect();
        let mut function = "<module>".to_owned();
        for (index, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            for prefix in ["fn ", "async fn ", "pub fn ", "pub async fn "] {
                if let Some(rest) = trimmed.strip_prefix(prefix) {
                    function = rest
                        .split(['(', '<', ' '])
                        .next()
                        .unwrap_or(rest)
                        .to_owned();
                    break;
                }
            }
            if trimmed.starts_with("assert_ne!(") {
                let assertion = lines[index..lines.len().min(index + 10)].join(" ");
                if assertion.contains("compiled, interpreted")
                    || assertion.contains("close the accounting")
                {
                    found.insert(function.clone());
                }
            }
        }
    }
}

fn known_differentials() -> BTreeSet<String> {
    fs::read_to_string(workspace_root().join("known-differentials.toml"))
        .expect("known-differentials.toml is part of the release gate")
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("test = \"")?.strip_suffix('"'))
        .map(str::to_owned)
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

#[test]
fn every_conditional_gate_is_owned_by_the_inventory() {
    let live = conditional_sites();
    assert!(!live.is_empty(), "the conditional-gate scan found nothing");
    let entries = conditional_inventory();
    let known: BTreeSet<String> = entries.iter().map(|entry| entry.site.clone()).collect();
    let missing: Vec<&String> = live.difference(&known).collect();
    let stale: Vec<&String> = known.difference(&live).collect();
    assert!(
        missing.is_empty(),
        "{} conditional gate(s) are absent from conditional-tests.toml:\n  {}",
        missing.len(),
        missing
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n  ")
    );
    assert!(
        stale.is_empty(),
        "{} conditional inventory entries no longer exist:\n  {}",
        stale.len(),
        stale
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

#[test]
fn every_conditional_gate_has_release_policy_and_an_open_owner() {
    let entries = conditional_inventory();
    let mut statuses = BTreeMap::new();
    task_statuses(&workspace_root().join("tasks"), &mut statuses);
    let invalid: Vec<String> = entries
        .iter()
        .filter_map(|entry| {
            let status = statuses.get(&entry.owner).map(String::as_str);
            (entry.site.is_empty()
                || entry.class != "infrastructure"
                || entry.job.is_empty()
                || entry.requires.is_empty()
                || entry.policy != "required"
                || matches!(status, None | Some("completed" | "cancelled")))
            .then(|| {
                format!(
                    "{}: class={:?}, owner={} ({status:?}), job={:?}, requires={:?}, policy={:?}",
                    entry.site, entry.class, entry.owner, entry.job, entry.requires, entry.policy,
                )
            })
        })
        .collect();
    assert!(
        invalid.is_empty(),
        "conditional gate entries need an explicit release policy and open owner:\n  {}",
        invalid.join("\n  ")
    );
}

#[test]
fn every_asserted_engine_difference_is_a_declared_release_blocker() {
    let mut live = BTreeSet::new();
    asserted_engine_differences(
        &workspace_root().join("crates/nano-conformance/tests/conformance"),
        &mut live,
    );
    asserted_engine_differences(
        &workspace_root().join("vendor/clarity-wasm/clar2wasm"),
        &mut live,
    );
    let known = known_differentials();
    assert_eq!(
        live, known,
        "tests that assert compiled and interpreted answers differ must be declared in \
         known-differentials.toml, and stale declarations must be removed"
    );
}
