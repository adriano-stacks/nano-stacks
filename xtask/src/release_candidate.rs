use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs,
    path::{Component, Path, PathBuf},
    process::{Command, ExitCode},
};

use nano_primitives::sha256;
use serde_json::{Value, json};

use crate::{
    release_advisory::{ADVISORY_POLICY, AdvisoryPolicy},
    release_artifact::source_status,
};

const ARTIFACT: &str = "artifact";
const AUDIT: &str = "cargo-audit.json";
const CHECKPOINT: &str = "checkpoint.toml";
const CLOSURE: &str = "nix-closure.json";
const CONFIGURATION: &str = "configuration.json";
const PREPARED_SUMS: &str = "candidate.SHA256SUMS";
const PREPARED_SIGNATURE: &str = "candidate.SHA256SUMS.minisig";
const PROVENANCE: &str = "provenance.intoto.json";
const PUBLISHER_KEY: &str = "publisher.pub";
const REPORT: &str = "qualification-report.txt";
const RELEASE_SUMS: &str = "release.SHA256SUMS";
const RELEASE_SIGNATURE: &str = "release.SHA256SUMS.minisig";
const SOURCE_INPUTS: &[&str] = &[
    ADVISORY_POLICY,
    "Cargo.lock",
    "flake.lock",
    "flake.nix",
    "rust-toolchain.toml",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateVerification {
    pub binary: PathBuf,
    pub candidate_manifest_sha256: String,
    pub checkpoint_sha256: String,
    pub configuration_sha256: String,
    pub source_revision: String,
}

pub fn verify_qualification_inputs(
    candidate: &CandidateVerification,
    configuration: &Path,
    checkpoint: &Path,
) -> Result<(), String> {
    require_file(configuration, "release configuration")?;
    let checkpoint = checkpoint_path(checkpoint);
    require_file(&checkpoint, "release checkpoint manifest")?;
    let configuration_sha256 = file_sha256(configuration)?;
    if configuration_sha256 != candidate.configuration_sha256 {
        return Err(format!(
            "release configuration changed: candidate {}, current {configuration_sha256}",
            candidate.configuration_sha256
        ));
    }
    let checkpoint_sha256 = file_sha256(&checkpoint)?;
    if checkpoint_sha256 != candidate.checkpoint_sha256 {
        return Err(format!(
            "checkpoint manifest changed: candidate {}, current {checkpoint_sha256}",
            candidate.checkpoint_sha256
        ));
    }
    Ok(())
}

pub fn command(arguments: &[String], workspace: &Path) -> ExitCode {
    let result = match arguments.first().map(String::as_str) {
        Some("audit") => audit(&arguments[1..], workspace),
        Some("prepare") => prepare(&arguments[1..], workspace),
        Some("finalize") => finalize(&arguments[1..]),
        Some("verify") => verify(&arguments[1..]),
        _ => Err(usage()),
    };
    match result {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("release-candidate: {error}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> String {
    "usage:\n  cargo xtask release-candidate audit --advisory-db <RustSec.git>\n  cargo xtask release-candidate prepare --output <dir> --checkpoint <dir-or-file> \
     --config <config.toml> --advisory-db <RustSec.git> --secret-key <minisign.key> \
     --public-key <minisign.pub> [--artifact <nix-output> \
     --artifact-store <rootless-store-root>]\n  cargo xtask release-candidate \
     finalize --candidate <dir> --report <release-report.txt> --secret-key <minisign.key> \
     --public-key <minisign.pub>\n  cargo xtask release-candidate verify --candidate <dir> \
     --public-key <minisign.pub> [--prepared]"
        .to_owned()
}

fn audit(arguments: &[String], workspace: &Path) -> Result<String, String> {
    let options = options(arguments)?;
    reject_unknown(&options, &["--advisory-db"])?;
    let advisory_db = required_path(&options, "--advisory-db")?;
    let report = run_advisory_policy(workspace, &advisory_db)?;
    let report: Value = serde_json::from_slice(&report)
        .map_err(|error| format!("cannot reread cargo-audit report: {error}"))?;
    let warnings = report["warnings"]
        .as_object()
        .into_iter()
        .flat_map(|warnings| warnings.values())
        .filter_map(Value::as_array)
        .map(Vec::len)
        .sum::<usize>();
    Ok(format!(
        "advisory policy PASS: no vulnerabilities; {warnings} exact, owned, unexpired exception(s)"
    ))
}

fn prepare(arguments: &[String], workspace: &Path) -> Result<String, String> {
    let options = options(arguments)?;
    reject_unknown(
        &options,
        &[
            "--output",
            "--checkpoint",
            "--config",
            "--advisory-db",
            "--secret-key",
            "--public-key",
            "--artifact",
            "--artifact-store",
        ],
    )?;
    let output = required_path(&options, "--output")?;
    let checkpoint = checkpoint_path(&required_path(&options, "--checkpoint")?);
    let configuration = required_path(&options, "--config")?;
    let advisory_db = required_path(&options, "--advisory-db")?;
    let secret_key = required_path(&options, "--secret-key")?;
    let public_key = required_path(&options, "--public-key")?;
    require_absent(&output)?;
    require_file(&checkpoint, "checkpoint manifest")?;
    require_file(&configuration, "node configuration")?;
    require_file(&secret_key, "minisign secret key")?;
    require_file(&public_key, "minisign public key")?;

    let source = source_status(workspace)?;
    if !source.clean() {
        return Err(format!(
            "build-relevant source is dirty: {}",
            source.changes.join(", ")
        ));
    }
    let (artifact, store_path, artifact_store) = artifact_input(&options, workspace)?;
    validate_artifact(&artifact, &source.revision)?;
    let audit = run_advisory_policy(workspace, &advisory_db)?;
    let advisory_revision = clean_git_revision(&advisory_db, "advisory database")?;
    let closure = nix_closure(&store_path, artifact_store.as_deref())?;

    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| format!("cannot create output parent: {error}"))?;
    let staging = tempfile::Builder::new()
        .prefix(".nano-release-candidate.")
        .tempdir_in(parent)
        .map_err(|error| format!("cannot create candidate staging directory: {error}"))?;
    populate_candidate(
        staging.path(),
        workspace,
        &artifact,
        &checkpoint,
        &configuration,
        &public_key,
        &audit,
        &closure,
        &source.revision,
        &advisory_revision,
    )?;
    write_checksum_file(staging.path(), PREPARED_SUMS, &prepared_exclusions())?;
    sign(
        &staging.path().join(PREPARED_SUMS),
        &staging.path().join(PREPARED_SIGNATURE),
        &secret_key,
        &format!("nano-stacks candidate {}", source.revision),
    )?;
    verify_prepared_candidate(staging.path(), &public_key, Some(&source.revision))?;

    let staging = staging.keep();
    fs::rename(&staging, &output).map_err(|error| {
        format!(
            "cannot publish candidate {} as {}: {error}",
            staging.display(),
            output.display()
        )
    })?;
    Ok(format!("prepared signed candidate {}", output.display()))
}

#[allow(clippy::too_many_arguments)]
fn populate_candidate(
    candidate: &Path,
    workspace: &Path,
    artifact: &Path,
    checkpoint: &Path,
    configuration: &Path,
    public_key: &Path,
    audit: &[u8],
    closure: &Value,
    source_revision: &str,
    advisory_revision: &str,
) -> Result<(), String> {
    copy_tree(artifact, &candidate.join(ARTIFACT))?;
    fs::copy(checkpoint, candidate.join(CHECKPOINT))
        .map_err(|error| format!("cannot copy checkpoint manifest: {error}"))?;
    fs::copy(public_key, candidate.join(PUBLISHER_KEY))
        .map_err(|error| format!("cannot copy publisher key: {error}"))?;
    fs::write(candidate.join(AUDIT), audit)
        .map_err(|error| format!("cannot write advisory report: {error}"))?;
    write_json(&candidate.join(CLOSURE), closure)?;
    let configuration_sha256 = file_sha256(configuration)?;
    write_json(
        &candidate.join(CONFIGURATION),
        &json!({
            "schema": "nano-stacks/release-configuration/v1",
            "sha256": configuration_sha256,
        }),
    )?;
    let input_dir = candidate.join("source-inputs");
    fs::create_dir(&input_dir)
        .map_err(|error| format!("cannot create source input directory: {error}"))?;
    for input in SOURCE_INPUTS {
        fs::copy(workspace.join(input), input_dir.join(input))
            .map_err(|error| format!("cannot copy source input {input}: {error}"))?;
    }
    let provenance = provenance(candidate, workspace, source_revision, advisory_revision)?;
    write_json(&candidate.join(PROVENANCE), &provenance)
}

fn provenance(
    candidate: &Path,
    workspace: &Path,
    source_revision: &str,
    advisory_revision: &str,
) -> Result<Value, String> {
    let binary = candidate.join(ARTIFACT).join("bin/stacks-node");
    let origin = git(workspace, &["remote", "get-url", "origin"])?;
    let mut dependencies = Vec::new();
    for input in SOURCE_INPUTS {
        dependencies.push(json!({
            "uri": format!("file:{input}"),
            "digest": { "sha256": file_sha256(&workspace.join(input))? },
        }));
    }
    dependencies.push(json!({
        "uri": format!("git+https://github.com/RustSec/advisory-db@{advisory_revision}"),
        "digest": { "gitCommit": advisory_revision },
    }));
    dependencies.push(json!({
        "uri": "file:checkpoint.toml",
        "digest": { "sha256": file_sha256(&candidate.join(CHECKPOINT))? },
    }));
    dependencies.push(json!({
        "uri": "private:config.toml",
        "digest": { "sha256": read_json(&candidate.join(CONFIGURATION))?["sha256"] },
    }));
    Ok(json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{
            "name": "artifact/bin/stacks-node",
            "digest": { "sha256": file_sha256(&binary)? },
        }],
        "predicateType": "https://slsa.dev/provenance/v1",
        "predicate": {
            "buildDefinition": {
                "buildType": "https://nixos.org/nix/derivation",
                "externalParameters": {
                    "flake_output": ".#stacks-node",
                    "source_repository": origin,
                    "source_revision": source_revision,
                },
                "internalParameters": {
                    "build_identity": read_json(&candidate.join(ARTIFACT).join("share/nano-stacks/build-identity.json"))?,
                    "configuration_sha256": read_json(&candidate.join(CONFIGURATION))?["sha256"],
                    "tools": {
                        "cargo_audit": tool_version("cargo", &["audit", "--version"] )?,
                        "minisign": tool_version("minisign", &["-v"] )?,
                        "nix": tool_version("nix", &["--version"] )?,
                    },
                },
                "resolvedDependencies": dependencies,
            },
            "runDetails": {
                "builder": { "id": "https://nixos.org/nix" },
                "metadata": {
                    "invocationId": read_json(&candidate.join(CLOSURE))?["outputNarHash"],
                },
            },
        },
    }))
}

fn finalize(arguments: &[String]) -> Result<String, String> {
    let options = options(arguments)?;
    reject_unknown(
        &options,
        &["--candidate", "--report", "--secret-key", "--public-key"],
    )?;
    let candidate = required_path(&options, "--candidate")?;
    let report = required_path(&options, "--report")?;
    let secret_key = required_path(&options, "--secret-key")?;
    let public_key = required_path(&options, "--public-key")?;
    require_file(&report, "qualification report")?;
    require_file(&secret_key, "minisign secret key")?;
    let verified = verify_prepared_candidate(&candidate, &public_key, None)?;
    for file in [REPORT, RELEASE_SUMS, RELEASE_SIGNATURE] {
        require_absent(&candidate.join(file))?;
    }
    let report_text = fs::read_to_string(&report)
        .map_err(|error| format!("cannot read qualification report: {error}"))?;
    let binding = format!(
        "candidate manifest     sha256:{}",
        verified.candidate_manifest_sha256
    );
    if !report_text.contains(&binding) || !report_text.contains("release qualification PASS") {
        return Err(format!(
            "qualification report does not contain {binding:?} and a PASS verdict"
        ));
    }
    fs::copy(&report, candidate.join(REPORT))
        .map_err(|error| format!("cannot add qualification report: {error}"))?;
    write_checksum_file(&candidate, RELEASE_SUMS, &release_exclusions())?;
    sign(
        &candidate.join(RELEASE_SUMS),
        &candidate.join(RELEASE_SIGNATURE),
        &secret_key,
        &format!("nano-stacks release {}", verified.source_revision),
    )?;
    verify_final_candidate(&candidate, &public_key, None)?;
    Ok(format!("finalized signed release {}", candidate.display()))
}

fn verify(arguments: &[String]) -> Result<String, String> {
    let options = options(arguments)?;
    reject_unknown(&options, &["--candidate", "--public-key", "--prepared"])?;
    let candidate = required_path(&options, "--candidate")?;
    let public_key = required_path(&options, "--public-key")?;
    let prepared = options.contains_key("--prepared");
    let verified = if prepared {
        verify_prepared_candidate(&candidate, &public_key, None)?
    } else {
        verify_final_candidate(&candidate, &public_key, None)?
    };
    Ok(format!(
        "verified {} candidate {} from {}",
        if prepared { "prepared" } else { "final" },
        verified.candidate_manifest_sha256,
        verified.source_revision
    ))
}

pub fn verify_prepared_candidate(
    candidate: &Path,
    public_key: &Path,
    expected_revision: Option<&str>,
) -> Result<CandidateVerification, String> {
    require_file(public_key, "minisign public key")?;
    require_file(&candidate.join(PREPARED_SUMS), "candidate checksums")?;
    require_file(&candidate.join(PREPARED_SIGNATURE), "candidate signature")?;
    verify_signature(
        &candidate.join(PREPARED_SUMS),
        &candidate.join(PREPARED_SIGNATURE),
        public_key,
    )?;
    verify_checksums(candidate, PREPARED_SUMS, &prepared_exclusions())?;
    required_candidate_files(candidate)?;
    verify_advisory_policy(
        &candidate.join(AUDIT),
        &candidate.join("source-inputs").join(ADVISORY_POLICY),
    )?;
    verify_publisher_key(candidate, public_key)?;
    let source_revision = verify_identity(candidate, expected_revision)?;
    let configuration = read_json(&candidate.join(CONFIGURATION))?;
    let configuration_sha256 = json_sha256(&configuration, "sha256", CONFIGURATION)?;
    let checkpoint_sha256 = file_sha256(&candidate.join(CHECKPOINT))?;
    Ok(CandidateVerification {
        binary: candidate.join(ARTIFACT).join("bin/stacks-node"),
        candidate_manifest_sha256: file_sha256(&candidate.join(PREPARED_SUMS))?,
        checkpoint_sha256,
        configuration_sha256,
        source_revision,
    })
}

fn verify_final_candidate(
    candidate: &Path,
    public_key: &Path,
    expected_revision: Option<&str>,
) -> Result<CandidateVerification, String> {
    require_file(&candidate.join(RELEASE_SUMS), "release checksums")?;
    require_file(&candidate.join(RELEASE_SIGNATURE), "release signature")?;
    verify_signature(
        &candidate.join(RELEASE_SUMS),
        &candidate.join(RELEASE_SIGNATURE),
        public_key,
    )?;
    verify_checksums(candidate, RELEASE_SUMS, &release_exclusions())?;
    let verified = verify_prepared_candidate(candidate, public_key, expected_revision)?;
    let report = fs::read_to_string(candidate.join(REPORT))
        .map_err(|error| format!("cannot read qualification report: {error}"))?;
    let binding = format!(
        "candidate manifest     sha256:{}",
        verified.candidate_manifest_sha256
    );
    if !report.contains(&binding) || !report.contains("release qualification PASS") {
        return Err(
            "signed qualification report is not bound to this candidate or is not a PASS"
                .to_owned(),
        );
    }
    Ok(verified)
}

fn required_candidate_files(candidate: &Path) -> Result<(), String> {
    for file in [
        "artifact/bin/stacks-node",
        "artifact/share/nano-stacks/build-identity.json",
        "artifact/share/nano-stacks/config.schema.json",
        "artifact/share/nano-stacks/dependencies.txt",
        "artifact/share/nano-stacks/sbom.cdx.json",
        "artifact/share/nano-stacks/container/Containerfile",
        "artifact/share/nano-stacks/systemd/nano-stacks.service",
        "artifact/share/doc/nano-stacks/README.md",
        AUDIT,
        CHECKPOINT,
        CLOSURE,
        CONFIGURATION,
        PROVENANCE,
        PUBLISHER_KEY,
    ] {
        require_file(&candidate.join(file), file)?;
    }
    for input in SOURCE_INPUTS {
        require_file(&candidate.join("source-inputs").join(input), input)?;
    }
    Ok(())
}

fn verify_identity(candidate: &Path, expected_revision: Option<&str>) -> Result<String, String> {
    let identity_path = candidate
        .join(ARTIFACT)
        .join("share/nano-stacks/build-identity.json");
    let identity = read_json(&identity_path)?;
    let revision = json_string(&identity, "source_revision", "build identity")?;
    let compiler_identity = json_string(&identity, "compiler_identity", "build identity")?;
    if compiler_identity
        .strip_prefix("sha256:")
        .is_none_or(|digest| {
            digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        return Err("build identity has no clarity-wasm SHA-256 identity".to_owned());
    }
    for field in ["rustc", "target"] {
        if json_string(&identity, field, "build identity")?.is_empty() {
            return Err(format!("build identity has an empty {field}"));
        }
    }
    if expected_revision.is_some_and(|expected| expected != revision) {
        return Err(format!(
            "candidate source {revision} does not match current source {}",
            expected_revision.unwrap_or_default()
        ));
    }
    let provenance = read_json(&candidate.join(PROVENANCE))?;
    let provenance_revision = provenance
        .pointer("/predicate/buildDefinition/externalParameters/source_revision")
        .and_then(Value::as_str)
        .ok_or_else(|| "provenance has no source revision".to_owned())?;
    if provenance_revision != revision {
        return Err(format!(
            "provenance source {provenance_revision} differs from artifact source {revision}"
        ));
    }
    let sbom = read_json(
        &candidate
            .join(ARTIFACT)
            .join("share/nano-stacks/sbom.cdx.json"),
    )?;
    if sbom["bomFormat"] != "CycloneDX" || sbom["metadata"]["component"]["name"] != "nano-node" {
        return Err("SBOM does not describe the nano-node CycloneDX component".to_owned());
    }
    let locked_wasmtime = sbom["components"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|component| component["name"] == "wasmtime")
        .filter_map(|component| component["version"].as_str())
        .collect::<BTreeSet<_>>();
    if locked_wasmtime.len() != 1 {
        return Err(format!(
            "SBOM names {} Wasmtime versions instead of one",
            locked_wasmtime.len()
        ));
    }
    let embedded_wasmtime = identity["wasmtime"]
        .as_str()
        .or_else(|| identity["wasmtime"]["version"].as_str())
        .ok_or_else(|| "build identity has no Wasmtime version".to_owned())?;
    if !locked_wasmtime.contains(embedded_wasmtime) {
        return Err(format!(
            "build identity names Wasmtime {embedded_wasmtime}, SBOM names {}",
            locked_wasmtime.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    let embedded_engine = identity["wasmtime_engine"]
        .as_str()
        .ok_or_else(|| "build identity has no Wasmtime engine configuration".to_owned())?;
    if embedded_engine != nano_vm::WASMTIME_ENGINE_CONFIG {
        return Err("build identity names a different Wasmtime engine configuration".to_owned());
    }
    if !embedded_engine.starts_with(&format!("wasmtime={embedded_wasmtime};")) {
        return Err("Wasmtime engine configuration names a different runtime version".to_owned());
    }
    Ok(revision.to_owned())
}

fn validate_artifact(artifact: &Path, revision: &str) -> Result<(), String> {
    require_file(&artifact.join("bin/stacks-node"), "Nix-built stacks-node")?;
    for file in [
        "share/nano-stacks/build-identity.json",
        "share/nano-stacks/config.schema.json",
        "share/nano-stacks/dependencies.txt",
        "share/nano-stacks/sbom.cdx.json",
        "share/doc/nano-stacks/README.md",
    ] {
        require_file(&artifact.join(file), file)?;
    }
    let identity = read_json(&artifact.join("share/nano-stacks/build-identity.json"))?;
    if identity["source_revision"] != revision {
        return Err(format!(
            "artifact source {} does not match clean source {revision}",
            identity["source_revision"]
        ));
    }
    Ok(())
}

fn verify_advisory_policy(report: &Path, policy: &Path) -> Result<(), String> {
    let report = read_json(report)?;
    AdvisoryPolicy::load(policy)?.verify_report_now(&report)
}

fn run_advisory_policy(workspace: &Path, database: &Path) -> Result<Vec<u8>, String> {
    let policy = AdvisoryPolicy::load(&workspace.join(ADVISORY_POLICY))?;
    policy.verify_owners(workspace)?;
    let output = Command::new("cargo")
        .args([
            OsStr::new("audit"),
            OsStr::new("--db"),
            database.as_os_str(),
            OsStr::new("--no-fetch"),
            OsStr::new("--file"),
            OsStr::new("Cargo.lock"),
            OsStr::new("--format"),
            OsStr::new("json"),
        ])
        .current_dir(workspace)
        .output()
        .map_err(|error| format!("cannot run cargo audit: {error}"))?;
    let report: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("cargo audit did not emit JSON: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo audit failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    policy.verify_report_now(&report)?;
    Ok(output.stdout)
}

fn clean_git_revision(repository: &Path, name: &str) -> Result<String, String> {
    let status = git(repository, &["status", "--porcelain"])?;
    if !status.is_empty() {
        return Err(format!("{name} is dirty"));
    }
    git(repository, &["rev-parse", "HEAD"])
}

fn build_artifact(workspace: &Path) -> Result<PathBuf, String> {
    let output = Command::new("nix")
        .args(["build", ".#stacks-node", "--no-link", "--print-out-paths"])
        .current_dir(workspace)
        .output()
        .map_err(|error| format!("cannot run Nix build: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Nix build failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|line| PathBuf::from(line.trim()))
        .ok_or_else(|| "Nix build printed no output path".to_owned())
}

fn artifact_input(
    options: &BTreeMap<&str, &str>,
    workspace: &Path,
) -> Result<(PathBuf, PathBuf, Option<PathBuf>), String> {
    let Some(artifact) = options.get("--artifact").map(PathBuf::from) else {
        if options.contains_key("--artifact-store") {
            return Err("--artifact-store requires --artifact".to_owned());
        }
        let artifact = build_artifact(workspace)?;
        return Ok((artifact.clone(), artifact, None));
    };
    let Some(store) = options.get("--artifact-store").map(PathBuf::from) else {
        return Ok((artifact.clone(), artifact, None));
    };
    if !store.is_absolute() || !store.is_dir() {
        return Err("--artifact-store must name an absolute store directory".to_owned());
    }
    if artifact.parent() != Some(Path::new("/nix/store")) {
        return Err("a rootless-store artifact must be one direct /nix/store path".to_owned());
    }
    let name = artifact
        .file_name()
        .ok_or_else(|| "rootless-store artifact has no file name".to_owned())?;
    let physical = store.join("nix/store").join(name);
    Ok((physical, artifact, Some(store)))
}

fn nix_closure(artifact: &Path, store: Option<&Path>) -> Result<Value, String> {
    let mut command = Command::new("nix");
    let store_uri;
    if let Some(store) = store {
        store_uri = format!("local?root={}", store.display());
        command.args([OsStr::new("--store"), OsStr::new(&store_uri)]);
    }
    let output = command
        .args([
            OsStr::new("path-info"),
            OsStr::new("--json"),
            OsStr::new("--json-format"),
            OsStr::new("1"),
            OsStr::new("--recursive"),
            OsStr::new("--closure-size"),
            artifact.as_os_str(),
        ])
        .output()
        .map_err(|error| format!("cannot inspect Nix closure: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "nix path-info failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let raw: BTreeMap<String, Value> = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("nix path-info emitted invalid JSON: {error}"))?;
    let paths = raw
        .iter()
        .map(|(path, metadata)| {
            json!({
                "path": path,
                "narHash": metadata["narHash"],
                "narSize": metadata["narSize"],
                "references": metadata["references"],
            })
        })
        .collect::<Vec<_>>();
    let artifact_key = artifact
        .to_str()
        .ok_or_else(|| "Nix output path is not UTF-8".to_owned())?;
    let output_hash = raw
        .get(artifact_key)
        .and_then(|metadata| metadata["narHash"].as_str())
        .ok_or_else(|| "Nix closure omitted its output NAR hash".to_owned())?;
    Ok(json!({
        "schema": "nano-stacks/nix-closure/v1",
        "output": artifact_key,
        "outputNarHash": output_hash,
        "paths": paths,
    }))
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir(destination).map_err(|error| {
        format!(
            "cannot create copied directory {}: {error}",
            destination.display()
        )
    })?;
    let mut entries = fs::read_dir(source)
        .map_err(|error| format!("cannot read {}: {error}", source.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot read {}: {error}", source.display()))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let kind = entry
            .file_type()
            .map_err(|error| format!("cannot inspect {}: {error}", entry.path().display()))?;
        let target = destination.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &target)
                .map_err(|error| format!("cannot copy {}: {error}", entry.path().display()))?;
        } else {
            return Err(format!(
                "candidate input {} is not a regular file or directory",
                entry.path().display()
            ));
        }
    }
    Ok(())
}

fn write_checksum_file(
    root: &Path,
    name: &str,
    exclusions: &BTreeSet<&'static str>,
) -> Result<(), String> {
    let files = candidate_files(root, exclusions)?;
    let mut contents = String::new();
    for relative in files {
        contents.push_str(&file_sha256(&root.join(&relative))?);
        contents.push_str("  ");
        contents.push_str(&relative);
        contents.push('\n');
    }
    fs::write(root.join(name), contents).map_err(|error| format!("cannot write {name}: {error}"))
}

fn verify_checksums(
    root: &Path,
    name: &str,
    exclusions: &BTreeSet<&'static str>,
) -> Result<(), String> {
    let contents = fs::read_to_string(root.join(name))
        .map_err(|error| format!("cannot read {name}: {error}"))?;
    let mut listed = BTreeSet::new();
    for line in contents.lines() {
        let (expected, relative) = line
            .split_once("  ")
            .ok_or_else(|| format!("invalid checksum line {line:?}"))?;
        if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!("invalid SHA-256 digest in {name}: {expected}"));
        }
        safe_relative(relative)?;
        if !listed.insert(relative.to_owned()) {
            return Err(format!("duplicate checksum path {relative}"));
        }
        let actual = file_sha256(&root.join(relative))?;
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(format!(
                "checksum mismatch for {relative}: expected {expected}, found {actual}"
            ));
        }
    }
    let actual = candidate_files(root, exclusions)?;
    if listed != actual {
        let missing = actual.difference(&listed).cloned().collect::<Vec<_>>();
        let stale = listed.difference(&actual).cloned().collect::<Vec<_>>();
        return Err(format!(
            "{name} inventory differs: unlisted {missing:?}, absent {stale:?}"
        ));
    }
    Ok(())
}

fn candidate_files(
    root: &Path,
    exclusions: &BTreeSet<&'static str>,
) -> Result<BTreeSet<String>, String> {
    let mut found = BTreeSet::new();
    collect_files(root, root, exclusions, &mut found)?;
    Ok(found)
}

fn collect_files(
    root: &Path,
    directory: &Path,
    exclusions: &BTreeSet<&'static str>,
    found: &mut BTreeSet<String>,
) -> Result<(), String> {
    for entry in fs::read_dir(directory)
        .map_err(|error| format!("cannot read {}: {error}", directory.display()))?
    {
        let entry = entry.map_err(|error| format!("cannot read candidate entry: {error}"))?;
        let kind = entry
            .file_type()
            .map_err(|error| format!("cannot inspect {}: {error}", entry.path().display()))?;
        if kind.is_dir() {
            collect_files(root, &entry.path(), exclusions, found)?;
        } else if kind.is_file() {
            let relative = entry
                .path()
                .strip_prefix(root)
                .map_err(|error| format!("cannot make candidate path relative: {error}"))?
                .to_str()
                .ok_or_else(|| "candidate path is not UTF-8".to_owned())?
                .replace('\\', "/");
            if !exclusions.contains(relative.as_str()) {
                found.insert(relative);
            }
        } else {
            return Err(format!(
                "candidate contains a symlink or special file: {}",
                entry.path().display()
            ));
        }
    }
    Ok(())
}

fn prepared_exclusions() -> BTreeSet<&'static str> {
    [
        PREPARED_SUMS,
        PREPARED_SIGNATURE,
        REPORT,
        RELEASE_SUMS,
        RELEASE_SIGNATURE,
    ]
    .into_iter()
    .collect()
}

fn release_exclusions() -> BTreeSet<&'static str> {
    [RELEASE_SUMS, RELEASE_SIGNATURE].into_iter().collect()
}

fn sign(message: &Path, signature: &Path, key: &Path, trusted: &str) -> Result<(), String> {
    let status = Command::new("minisign")
        .args([OsStr::new("-S"), OsStr::new("-s"), key.as_os_str()])
        .args([OsStr::new("-m"), message.as_os_str()])
        .args([OsStr::new("-x"), signature.as_os_str()])
        .args([OsStr::new("-c"), OsStr::new("nano-stacks release")])
        .args([OsStr::new("-t"), OsStr::new(trusted)])
        .status()
        .map_err(|error| format!("cannot run minisign: {error}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "minisign refused to sign the candidate".to_owned())
}

fn verify_signature(message: &Path, signature: &Path, key: &Path) -> Result<(), String> {
    let output = Command::new("minisign")
        .args([OsStr::new("-V"), OsStr::new("-q")])
        .args([OsStr::new("-p"), key.as_os_str()])
        .args([OsStr::new("-m"), message.as_os_str()])
        .args([OsStr::new("-x"), signature.as_os_str()])
        .output()
        .map_err(|error| format!("cannot run minisign: {error}"))?;
    output.status.success().then_some(()).ok_or_else(|| {
        format!(
            "minisign rejected {}: {}",
            message.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    })
}

fn verify_publisher_key(candidate: &Path, expected: &Path) -> Result<(), String> {
    let packaged = fs::read(candidate.join(PUBLISHER_KEY))
        .map_err(|error| format!("cannot read packaged publisher key: {error}"))?;
    let expected = fs::read(expected)
        .map_err(|error| format!("cannot read expected publisher key: {error}"))?;
    (packaged == expected)
        .then_some(())
        .ok_or_else(|| "packaged publisher key differs from the trusted key".to_owned())
}

fn checkpoint_path(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.join("checkpoint.toml")
    } else {
        path.to_path_buf()
    }
}

fn options(arguments: &[String]) -> Result<BTreeMap<&str, &str>, String> {
    let mut options = BTreeMap::new();
    let mut rest = arguments.iter();
    while let Some(name) = rest.next() {
        let name = name.as_str();
        if name == "--prepared" {
            if options.insert(name, "").is_some() {
                return Err(format!("duplicate option {name}"));
            }
            continue;
        }
        if !name.starts_with("--") {
            return Err(format!("unexpected argument {name}"));
        }
        let value = rest
            .next()
            .ok_or_else(|| format!("{name} requires a value"))?;
        if options.insert(name, value.as_str()).is_some() {
            return Err(format!("duplicate option {name}"));
        }
    }
    Ok(options)
}

fn reject_unknown(options: &BTreeMap<&str, &str>, allowed: &[&str]) -> Result<(), String> {
    let allowed = allowed.iter().copied().collect::<BTreeSet<_>>();
    let unknown = options
        .keys()
        .filter(|name| !allowed.contains(**name))
        .copied()
        .collect::<Vec<_>>();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(format!("unknown option(s): {}", unknown.join(", ")))
    }
}

fn required_path(options: &BTreeMap<&str, &str>, name: &str) -> Result<PathBuf, String> {
    options
        .get(name)
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing {name}\n{}", usage()))
}

fn require_file(path: &Path, description: &str) -> Result<(), String> {
    path.is_file().then_some(()).ok_or_else(|| {
        format!(
            "{description} {} is absent or not a regular file",
            path.display()
        )
    })
}

fn require_absent(path: &Path) -> Result<(), String> {
    (!path.exists())
        .then_some(())
        .ok_or_else(|| format!("refusing to overwrite {}", path.display()))
}

fn safe_relative(path: &str) -> Result<(), String> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("unsafe checksum path {}", path.display()));
    }
    Ok(())
}

fn file_sha256(path: &Path) -> Result<String, String> {
    fs::read(path)
        .map(|bytes| hex::encode(sha256(&bytes).as_bytes()))
        .map_err(|error| format!("cannot hash {}: {error}", path.display()))
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("cannot serialize {}: {error}", path.display()))?;
    bytes.push(b'\n');
    fs::write(path, bytes).map_err(|error| format!("cannot write {}: {error}", path.display()))
}

fn read_json(path: &Path) -> Result<Value, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("{} is not valid JSON: {error}", path.display()))
}

fn json_string<'a>(value: &'a Value, key: &str, context: &str) -> Result<&'a str, String> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("{context} has no string {key}"))
}

fn json_sha256(value: &Value, key: &str, context: &str) -> Result<String, String> {
    let digest = json_string(value, key, context)?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("{context} has invalid SHA-256 {digest}"));
    }
    Ok(digest.to_owned())
}

fn git(root: &Path, arguments: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(root)
        .output()
        .map_err(|error| format!("cannot run git {}: {error}", arguments.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn tool_version(program: &str, arguments: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(arguments)
        .output()
        .map_err(|error| format!("cannot run {program}: {error}"))?;
    if !output.status.success() {
        return Err(format!("{program} did not report its version"));
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if stdout.is_empty() {
        Ok(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    } else {
        Ok(stdout)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs,
        path::PathBuf,
        process::Command,
    };

    use serde_json::json;

    use crate::release_advisory::ADVISORY_POLICY;

    use super::{
        ARTIFACT, AUDIT, CHECKPOINT, CLOSURE, CONFIGURATION, PREPARED_SIGNATURE, PREPARED_SUMS,
        PROVENANCE, PUBLISHER_KEY, REPORT, SOURCE_INPUTS, artifact_input, finalize,
        prepared_exclusions, read_json, required_candidate_files, sign, verify_checksums,
        verify_final_candidate, verify_identity, verify_prepared_candidate, write_checksum_file,
    };

    const REVISION: &str = "1111111111111111111111111111111111111111";

    #[test]
    fn a_rootless_store_resolves_only_one_direct_nix_output() {
        let root = tempfile::tempdir().expect("temporary rootless store");
        let store = root.path().to_str().expect("UTF-8 store path");
        let options = BTreeMap::from([
            (
                "--artifact",
                "/nix/store/11111111111111111111111111111111-node",
            ),
            ("--artifact-store", store),
        ]);
        let (physical, store_path, selected_store) =
            artifact_input(&options, root.path()).expect("resolve rootless artifact");
        assert_eq!(
            physical,
            root.path()
                .join("nix/store/11111111111111111111111111111111-node")
        );
        assert_eq!(
            store_path,
            PathBuf::from("/nix/store/11111111111111111111111111111111-node")
        );
        assert_eq!(selected_store.as_deref(), Some(root.path()));

        let traversal = BTreeMap::from([
            ("--artifact", "/nix/store/../outside"),
            ("--artifact-store", store),
        ]);
        assert!(artifact_input(&traversal, root.path()).is_err());
    }

    struct SignedCandidate {
        root: tempfile::TempDir,
        path: PathBuf,
        public_key: PathBuf,
        secret_key: PathBuf,
    }

    fn candidate(audit: &serde_json::Value, include_sbom: bool, signed: bool) -> SignedCandidate {
        let root = tempfile::tempdir().expect("temporary signed candidate");
        let public_key = root.path().join("trusted.pub");
        let secret_key = root.path().join("trusted.key");
        let candidate = root.path().join("candidate");
        fs::create_dir(&candidate).expect("create candidate directory");
        let status = Command::new("minisign")
            .args(["-G", "-W", "-p"])
            .arg(&public_key)
            .arg("-s")
            .arg(&secret_key)
            .status()
            .expect("generate minisign test key");
        assert!(status.success(), "generate minisign test key");

        write_artifact(&candidate, include_sbom);
        write_json_test(&candidate.join(AUDIT), audit);
        fs::write(candidate.join(CHECKPOINT), "format = 4\n").expect("write checkpoint");
        write_json_test(
            &candidate.join(CLOSURE),
            &json!({ "outputNarHash": "sha256:test" }),
        );
        write_json_test(
            &candidate.join(CONFIGURATION),
            &json!({ "sha256": "3".repeat(64) }),
        );
        write_json_test(
            &candidate.join(PROVENANCE),
            &json!({
                "predicate": {
                    "buildDefinition": {
                        "externalParameters": { "source_revision": REVISION },
                    },
                },
            }),
        );
        fs::copy(&public_key, candidate.join(PUBLISHER_KEY)).expect("copy publisher key");
        fs::create_dir(candidate.join("source-inputs")).expect("create source inputs");
        for input in SOURCE_INPUTS {
            let path = candidate.join("source-inputs").join(input);
            if *input == ADVISORY_POLICY {
                write_json_test(&path, &green_policy());
            } else {
                fs::write(path, input).expect("write source input");
            }
        }
        write_checksum_file(&candidate, PREPARED_SUMS, &prepared_exclusions())
            .expect("write candidate checksums");
        if signed {
            sign(
                &candidate.join(PREPARED_SUMS),
                &candidate.join(PREPARED_SIGNATURE),
                &secret_key,
                "test candidate",
            )
            .expect("sign candidate");
        }
        SignedCandidate {
            root,
            path: candidate,
            public_key,
            secret_key,
        }
    }

    fn write_artifact(candidate: &std::path::Path, include_sbom: bool) {
        let artifact = candidate.join(ARTIFACT);
        fs::create_dir_all(artifact.join("bin")).expect("create binary directory");
        fs::create_dir_all(artifact.join("share/nano-stacks/container"))
            .expect("create container directory");
        fs::create_dir_all(artifact.join("share/nano-stacks/systemd"))
            .expect("create service directory");
        fs::create_dir_all(artifact.join("share/doc/nano-stacks"))
            .expect("create documentation directory");
        fs::write(artifact.join("bin/stacks-node"), "binary").expect("write binary");
        write_json_test(
            &artifact.join("share/nano-stacks/build-identity.json"),
            &json!({
                "source_revision": REVISION,
                "compiler_identity": format!("sha256:{}", "2".repeat(64)),
                "rustc": "rustc test",
                "target": "x86_64-unknown-linux-gnu",
                "wasmtime": nano_vm::WASMTIME_VERSION,
                "wasmtime_engine": nano_vm::WASMTIME_ENGINE_CONFIG,
            }),
        );
        write_json_test(
            &artifact.join("share/nano-stacks/config.schema.json"),
            &json!({"title": "Config"}),
        );
        fs::write(
            artifact.join("share/nano-stacks/dependencies.txt"),
            "nano-node\n",
        )
        .expect("write dependencies");
        if include_sbom {
            write_json_test(
                &artifact.join("share/nano-stacks/sbom.cdx.json"),
                &json!({
                    "bomFormat": "CycloneDX",
                    "metadata": { "component": { "name": "nano-node" } },
                    "components": [{ "name": "wasmtime", "version": nano_vm::WASMTIME_VERSION }],
                }),
            );
        }
        fs::write(
            artifact.join("share/nano-stacks/container/Containerfile"),
            "FROM scratch\n",
        )
        .expect("write container profile");
        fs::write(
            artifact.join("share/nano-stacks/systemd/nano-stacks.service"),
            "[Service]\n",
        )
        .expect("write service profile");
        fs::write(
            artifact.join("share/doc/nano-stacks/README.md"),
            "# Running nano-stacks\n",
        )
        .expect("write operator documentation");
    }

    fn green_audit() -> serde_json::Value {
        json!({
            "settings": { "ignore": [] },
            "vulnerabilities": { "found": false, "count": 0, "list": [] },
            "warnings": {},
        })
    }

    fn green_policy() -> serde_json::Value {
        json!({
            "schema": "nano-stacks/advisory-policy/v1",
            "exceptions": [],
        })
    }

    fn write_json_test(path: &std::path::Path, value: &serde_json::Value) {
        fs::write(
            path,
            serde_json::to_vec(value).expect("serialize test JSON"),
        )
        .expect("write test JSON");
    }

    #[test]
    fn checksum_inventory_rejects_changed_and_unlisted_files() {
        let root = tempfile::tempdir().expect("temporary candidate");
        fs::write(root.path().join("one"), "one").expect("write first file");
        write_checksum_file(root.path(), "sums", &BTreeSet::from(["sums"]))
            .expect("write checksums");
        assert!(verify_checksums(root.path(), "sums", &BTreeSet::from(["sums"])).is_ok());
        fs::write(root.path().join("one"), "changed").expect("change first file");
        assert!(verify_checksums(root.path(), "sums", &BTreeSet::from(["sums"])).is_err());
        fs::write(root.path().join("one"), "one").expect("restore first file");
        fs::write(root.path().join("two"), "two").expect("write unlisted file");
        assert!(verify_checksums(root.path(), "sums", &BTreeSet::from(["sums"])).is_err());
    }

    #[test]
    fn prepared_manifest_has_fixed_finalization_exclusions() {
        let exclusions = prepared_exclusions();
        assert!(exclusions.contains("qualification-report.txt"));
        assert!(exclusions.contains("release.SHA256SUMS"));
        assert!(exclusions.contains("release.SHA256SUMS.minisig"));
    }

    #[test]
    fn a_signed_complete_candidate_verifies_and_any_change_breaks_it() {
        let candidate = candidate(&green_audit(), true, true);
        assert!(
            verify_prepared_candidate(&candidate.path, &candidate.public_key, Some(REVISION))
                .is_ok()
        );
        fs::write(
            candidate.path.join(ARTIFACT).join("bin/stacks-node"),
            "changed",
        )
        .expect("change signed binary");
        assert!(
            verify_prepared_candidate(&candidate.path, &candidate.public_key, Some(REVISION))
                .is_err()
        );
    }

    #[test]
    fn unsigned_missing_sbom_and_advisory_failures_are_each_rejected() {
        let unsigned = candidate(&green_audit(), true, false);
        assert!(
            verify_prepared_candidate(&unsigned.path, &unsigned.public_key, Some(REVISION))
                .is_err()
        );
        let missing_sbom = candidate(&green_audit(), false, true);
        assert!(
            verify_prepared_candidate(&missing_sbom.path, &missing_sbom.public_key, Some(REVISION))
                .is_err()
        );
        let advisory = candidate(
            &json!({
                "vulnerabilities": { "found": true, "count": 1, "list": [{}] },
                "settings": { "ignore": [] },
                "warnings": {},
            }),
            true,
            true,
        );
        assert!(
            verify_prepared_candidate(&advisory.path, &advisory.public_key, Some(REVISION))
                .is_err()
        );
    }

    #[test]
    fn missing_release_identity_or_documentation_is_rejected() {
        let missing_identity = candidate(&green_audit(), true, true);
        let identity_path = missing_identity
            .path
            .join(ARTIFACT)
            .join("share/nano-stacks/build-identity.json");
        let mut identity = read_json(&identity_path).expect("read build identity");
        identity
            .as_object_mut()
            .expect("build identity object")
            .remove("compiler_identity");
        write_json_test(&identity_path, &identity);
        assert!(verify_identity(&missing_identity.path, Some(REVISION)).is_err());

        let missing_documentation = candidate(&green_audit(), true, true);
        fs::remove_file(
            missing_documentation
                .path
                .join(ARTIFACT)
                .join("share/doc/nano-stacks/README.md"),
        )
        .expect("remove operator documentation");
        assert!(required_candidate_files(&missing_documentation.path).is_err());
    }

    #[test]
    fn final_signature_binds_the_qualification_report() {
        let candidate = candidate(&green_audit(), true, true);
        let prepared =
            verify_prepared_candidate(&candidate.path, &candidate.public_key, Some(REVISION))
                .expect("verify prepared candidate");
        let report = candidate.root.path().join("report.txt");
        fs::write(
            &report,
            format!(
                "candidate manifest     sha256:{}\nrelease qualification PASS\n",
                prepared.candidate_manifest_sha256
            ),
        )
        .expect("write qualification report");
        finalize(&[
            "--candidate".to_owned(),
            candidate.path.to_string_lossy().into_owned(),
            "--report".to_owned(),
            report.to_string_lossy().into_owned(),
            "--secret-key".to_owned(),
            candidate.secret_key.to_string_lossy().into_owned(),
            "--public-key".to_owned(),
            candidate.public_key.to_string_lossy().into_owned(),
        ])
        .expect("finalize candidate");
        assert!(
            verify_final_candidate(&candidate.path, &candidate.public_key, Some(REVISION)).is_ok()
        );
        fs::write(candidate.path.join(REPORT), "changed report\n")
            .expect("change qualification report");
        assert!(
            verify_final_candidate(&candidate.path, &candidate.public_key, Some(REVISION)).is_err()
        );
    }
}
