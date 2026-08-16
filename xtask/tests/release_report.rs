use std::{fs, path::Path, process::Command};

fn source_revision() -> String {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask is under the workspace");
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(workspace)
        .output()
        .expect("read source revision");
    assert!(output.status.success(), "git rev-parse failed");
    String::from_utf8(output.stdout)
        .expect("UTF-8 revision")
        .trim()
        .to_owned()
}

fn test_artifact(path: &Path) {
    fs::write(
        path,
        format!("{}\n{}\n", nano_vm::COMPILER_IDENTITY, source_revision()),
    )
    .expect("write identity-bearing artifact");
}

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
    let mut history: serde_json::Value = serde_json::from_slice(
        &fs::read(fixtures.join("sortition/consensus-hashes.json"))
            .expect("read captured consensus history"),
    )
    .expect("captured consensus history JSON");
    let snapshots_path = fixtures.join("sortition/snapshots.json");
    let mut snapshots: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(&snapshots_path).expect("read captured snapshots"))
            .expect("captured snapshots JSON");
    let operations = nano_conformance::captured_bitcoin_operations(fixtures)
        .expect("decode captured Bitcoin operations");
    let (history_end, seed_consensus_hash) = history["hashes"]
        .as_array()
        .expect("captured history has consensus hashes")
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, hash)| {
            let hash = hash.as_str()?;
            let height = snapshots
                .iter()
                .find(|snapshot| snapshot["consensus_hash"] == hash)?["block_height"]
                .as_u64()?;
            let mut seeds =
                operations
                    .get(hash)?
                    .iter()
                    .filter_map(|operation| match &operation.kind {
                        nano_bitcoin::BitcoinOperationKind::LeaderBlockCommit {
                            new_seed,
                            parent_modulus,
                            ..
                        } if nano_sortition::commitment_is_on_time(*parent_modulus, height) => {
                            Some(new_seed)
                        }
                        _ => None,
                    });
            let first = seeds.next()?;
            seeds
                .any(|seed| seed != first)
                .then(|| (index + 1, hash.to_owned()))
        })
        .expect("captured history has a seed with disagreeing commitments");
    history["hashes"]
        .as_array_mut()
        .expect("captured history has consensus hashes")
        .truncate(history_end);
    fs::write(
        fixtures.join("sortition/consensus-hashes.json"),
        serde_json::to_vec(&history).expect("encode adversarial history"),
    )
    .expect("write adversarial history");
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
fn the_embedded_compiler_identity_matches_the_current_vendor_tree() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask is under the workspace");
    let xtask = Path::new(env!("CARGO_BIN_EXE_xtask"));
    let embedded = Command::new(xtask)
        .arg("compiler-identity")
        .output()
        .expect("read the embedded compiler identity");
    let current = Command::new(xtask)
        .args([
            "compiler-identity",
            workspace
                .join("vendor/clarity-wasm")
                .to_str()
                .expect("UTF-8 workspace path"),
        ])
        .output()
        .expect("hash the current compiler tree");

    assert!(
        embedded.status.success(),
        "the artifact has no compiler identity: {}",
        String::from_utf8_lossy(&embedded.stderr)
    );
    assert!(
        current.status.success(),
        "the current compiler tree has no identity: {}",
        String::from_utf8_lossy(&current.stderr)
    );
    let embedded = String::from_utf8(embedded.stdout).expect("UTF-8 identity");
    let embedded = embedded.trim();
    let digest = embedded
        .strip_prefix("sha256:")
        .expect("the embedded identity names its hash algorithm");
    assert_eq!(
        digest.len(),
        64,
        "the embedded identity is a SHA-256 digest"
    );
    assert!(
        digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "the embedded identity is hexadecimal: {embedded}"
    );
    assert_eq!(
        embedded,
        String::from_utf8(current.stdout)
            .expect("UTF-8 identity")
            .trim(),
        "the artifact names a compiler other than the current vendor tree"
    );
}

#[test]
fn a_missing_embedded_compiler_identity_fails_the_report() {
    let temporary = tempfile::tempdir().expect("temporary release inputs");
    let fixtures = temporary.path().join("fixtures");
    baseline_fixture(&fixtures);
    let artifact = temporary.path().join("stacks-node");
    fs::write(&artifact, b"an artifact with no compiler identity")
        .expect("write an identity-free artifact");

    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args([
            "release-report",
            "--no-gates",
            "--artifact",
            artifact.to_str().expect("UTF-8 temporary path"),
        ])
        .env("NANO_FIXTURES", &fixtures)
        .output()
        .expect("run release report command");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert_eq!(
        output.status.code(),
        Some(1),
        "the report did not treat a missing identity as an audit failure:\n{stdout}"
    );
    assert!(
        stdout.contains("embedded compiler    MISSING"),
        "the report did not name the missing identity:\n{stdout}"
    );
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
    test_artifact(&artifact);
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
fn an_artifact_from_another_revision_is_an_audit_failure() {
    let temporary = tempfile::tempdir().expect("temporary release inputs");
    let fixtures = temporary.path().join("fixtures");
    baseline_fixture(&fixtures);
    let artifact = temporary.path().join("stacks-node");
    fs::write(
        &artifact,
        format!("{}\n{}\n", nano_vm::COMPILER_IDENTITY, "0".repeat(40)),
    )
    .expect("write stale artifact");

    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args([
            "release-report",
            "--no-gates",
            "--artifact",
            artifact.to_str().expect("UTF-8 artifact path"),
        ])
        .env("NANO_FIXTURES", &fixtures)
        .output()
        .expect("run release report command");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert_eq!(
        output.status.code(),
        Some(1),
        "stale artifact passed:\n{stdout}"
    );
    assert!(
        stdout.contains("embedded source      STALE OR MISSING"),
        "the report did not name the stale artifact:\n{stdout}"
    );
}

#[test]
fn missing_checkpoint_authentication_stops_before_artifact_evidence() {
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
    fs::remove_dir_all(fixtures.join("chainstate/checkpoint-H/authentication-history"))
        .expect("remove checkpoint authentication history");
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

    assert_eq!(
        output.status.code(),
        Some(1),
        "missing history passed:\n{stdout}"
    );
    assert!(
        stdout.contains("checkpoint authentication history"),
        "the report did not name the missing history:\n{stdout}"
    );
    assert!(
        stdout.contains("release qualification stopped"),
        "the report did not say it short-circuited:\n{stdout}"
    );
    assert!(
        !stdout.contains("\nartifact\n"),
        "the report inspected an artifact after invalid validation:\n{stdout}"
    );
}

#[test]
fn no_gates_is_non_qualifying_and_names_every_unexecuted_owner() {
    let temporary = tempfile::tempdir().expect("temporary release inputs");
    let fixtures = temporary.path().join("fixtures");
    baseline_fixture(&fixtures);
    let xtask = Path::new(env!("CARGO_BIN_EXE_xtask"));
    let artifact = temporary.path().join("stacks-node");
    test_artifact(&artifact);

    let output = Command::new(xtask)
        .args([
            "release-report",
            "--no-gates",
            "--artifact",
            artifact.to_str().expect("UTF-8 temporary path"),
        ])
        .env("NANO_FIXTURES", &fixtures)
        .env("NANO_MAINNET_KEY", "not-for-logs")
        .output()
        .expect("run non-qualifying release report");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        !output.status.success(),
        "--no-gates qualified a release:\n{stdout}"
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "a complete offline audit did not use the non-qualifying exit code:\n{stdout}"
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
    assert!(
        stdout.contains(
            "UNEXECUTED nano epoch4_profile::every_mandatory_epoch4_vector_executes_against_nano"
        ) && stdout.contains("UNEXECUTED stock revision efc34a07a225c4b950ab9404a1652aa5e14affaf")
            && stdout
                .contains("UNEXECUTED stock revision 6d58b498d3bd4f5ee19c69dc97559b4cba8153e8"),
        "the report omitted an unexecuted compatibility runner:\n{stdout}"
    );
    for vector in nano_consensus_profile::vectors()
        .expect("vector corpus")
        .vectors
    {
        assert!(
            stdout.contains(&format!("mandatory vector {}", vector.id)),
            "the report omitted mandatory vector {}:\n{stdout}",
            vector.id
        );
    }
    assert!(
        stdout.contains("reference snapshot   PASS stacks-core at-block refusal")
            && stdout.contains("observed mainnet     PASS block 8686666 tx f33840c5"),
        "the report did not distinguish reference and observed epoch evidence:\n{stdout}"
    );
    assert!(
        stdout.contains("NANO_MAINNET_KEY         <redacted>") && !stdout.contains("not-for-logs"),
        "the report exposed a private mainnet key:\n{stdout}"
    );
}
