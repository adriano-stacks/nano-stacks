use std::{fs, path::Path, process::Command};

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn tamper_one_receipt(fixtures: &Path) {
    let events = fixtures.join("events/new_block");
    let mut paths: Vec<_> = fs::read_dir(events)
        .expect("the fixture has receipt events")
        .map(|entry| entry.expect("read fixture event").path())
        .collect();
    paths.sort();
    for path in paths {
        let text = fs::read_to_string(&path).expect("read receipt event");
        let marker = "\"status\":\"success\"";
        if let Some(at) = text.find(marker) {
            let mut changed = text;
            changed.replace_range(at..at + marker.len(), "\"status\":\"abort_by_response\"");
            fs::write(path, changed).expect("tamper receipt event");
            return;
        }
    }
    panic!("the fixture has no successful receipt to tamper with");
}

fn make_checkpoint_winner_absent(fixtures: &Path) {
    let history: serde_json::Value = serde_json::from_slice(
        &fs::read(fixtures.join("sortition/consensus-hashes.json"))
            .expect("read captured consensus history"),
    )
    .expect("captured consensus history JSON");
    let seed_consensus_hash = history["hashes"]
        .as_array()
        .and_then(|hashes| hashes.last())
        .and_then(serde_json::Value::as_str)
        .expect("captured history has a seed consensus hash")
        .to_owned();
    let snapshots_path = fixtures.join("sortition/snapshots.json");
    let mut snapshots: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(&snapshots_path).expect("read captured snapshots"))
            .expect("captured snapshots JSON");
    let seed = snapshots
        .iter_mut()
        .find(|snapshot| snapshot["consensus_hash"].as_str() == Some(&seed_consensus_hash))
        .expect("captured snapshots carry the seed");
    seed["winning_block_txid"] = serde_json::Value::String("f".repeat(64));
    fs::write(
        snapshots_path,
        serde_json::to_vec(&snapshots).expect("encode adversarial snapshots"),
    )
    .expect("write adversarial snapshots");
}

fn baseline_fixture(root: &Path) {
    fs::create_dir_all(root).expect("create baseline fixture directory");
    fs::write(
        root.join("manifest.toml"),
        "mode = \"baseline\"\nreplay_blocks = 0\n",
    )
    .expect("write baseline fixture manifest");
}

#[test]
fn a_red_scoreboard_makes_both_commands_fail() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask is under the workspace");
    let temporary = tempfile::tempdir().expect("temporary fixture tree");
    let fixtures = temporary.path().join("fixtures");
    copy_tree(
        &workspace.join("crates/nano-conformance/fixtures"),
        &fixtures,
    )
    .expect("copy fixture tree");
    tamper_one_receipt(&fixtures);

    let xtask = Path::new(env!("CARGO_BIN_EXE_xtask"));
    let scoreboard = Command::new(xtask)
        .arg("scoreboard")
        .env("NANO_FIXTURES", &fixtures)
        .output()
        .expect("run scoreboard command");
    assert!(
        !scoreboard.status.success(),
        "the scoreboard command accepted a tampered receipt:\n{}",
        String::from_utf8_lossy(&scoreboard.stdout)
    );

    // An explicitly supplied artifact is hashed and checked for the embedded
    // compiler identity, so this command tests the report verdict without asking
    // a test process to recursively invoke Cargo.
    let artifact = temporary.path().join("stacks-node");
    fs::copy(xtask, &artifact).expect("copy an immutable test artifact");
    let report = Command::new(xtask)
        .args([
            "release-report",
            "--no-gates",
            "--artifact",
            artifact.to_str().expect("UTF-8 temporary path"),
        ])
        .env("NANO_FIXTURES", &fixtures)
        .output()
        .expect("run release report command");
    assert!(
        !report.status.success(),
        "the release report accepted the red scoreboard:\n{}",
        String::from_utf8_lossy(&report.stdout)
    );
    assert!(
        String::from_utf8_lossy(&report.stdout)
            .contains("a required surface diverged from its oracle"),
        "the report did not name why it failed:\n{}",
        String::from_utf8_lossy(&report.stdout)
    );
}

#[test]
fn an_invalid_checkpoint_seed_stops_before_replay_or_artifact_evidence() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask is under the workspace");
    let temporary = tempfile::tempdir().expect("temporary fixture tree");
    let fixtures = temporary.path().join("fixtures");
    copy_tree(
        &workspace.join("crates/nano-conformance/fixtures"),
        &fixtures,
    )
    .expect("copy fixture tree");
    make_checkpoint_winner_absent(&fixtures);
    let artifact = temporary.path().join("must-not-be-read");

    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args([
            "release-report",
            "--capture",
            fixtures.to_str().expect("UTF-8 fixture path"),
            "--artifact",
            artifact.to_str().expect("UTF-8 artifact path"),
            "--no-gates",
        ])
        .env("NANO_FIXTURES", &fixtures)
        .output()
        .expect("run release report command");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        !output.status.success(),
        "invalid capture passed:\n{stdout}"
    );
    assert!(
        stdout.contains("invalid checkpoint sortition seed"),
        "the report did not name the invalid seed:\n{stdout}"
    );
    assert!(
        stdout.contains("release qualification stopped"),
        "the report did not say it short-circuited:\n{stdout}"
    );
    for forbidden in ["\nartifact\n", "\nscoreboard\n", "\ngates\n"] {
        assert!(
            !stdout.contains(forbidden),
            "the report presented {forbidden:?} after invalid validation:\n{stdout}"
        );
    }
}

#[test]
fn no_gates_is_non_qualifying_and_names_every_unexecuted_owner() {
    let temporary = tempfile::tempdir().expect("temporary release inputs");
    let fixtures = temporary.path().join("fixtures");
    baseline_fixture(&fixtures);
    let xtask = Path::new(env!("CARGO_BIN_EXE_xtask"));
    let artifact = temporary.path().join("stacks-node");
    fs::copy(xtask, &artifact).expect("copy an immutable test artifact");

    let output = Command::new(xtask)
        .args([
            "release-report",
            "--no-gates",
            "--artifact",
            artifact.to_str().expect("UTF-8 temporary path"),
        ])
        .env("NANO_FIXTURES", &fixtures)
        .output()
        .expect("run non-qualifying release report");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        !output.status.success(),
        "--no-gates qualified a release:\n{stdout}"
    );
    assert!(
        stdout.contains(
            "--no-gates: required test commands were not run; this report is explicitly non-qualifying"
        ),
        "the report did not name the bypass:\n{stdout}"
    );
    assert!(
        stdout.contains(
            "conditional site signer_weight_enforcement::a_block_without_an_authenticated_signer_set_is_rejected#1 (owner task 076)"
        ),
        "the report omitted a required conditional site or its owner:\n{stdout}"
    );
    assert!(
        stdout.contains("ignored test reads_a_mainnet_burn_block (owner task 053)"),
        "the report omitted an ignored infrastructure test or its owner:\n{stdout}"
    );
    assert!(
        stdout.contains("NANO_REPLAY_BOTH_ENGINES=1"),
        "the report omitted its required semantic engine comparison:\n{stdout}"
    );
}
