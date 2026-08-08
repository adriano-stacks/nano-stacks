//! What is actually in the binary somebody would ship.
//!
//! `wasm_is_the_engine` asks the *sources* and the dependency graph whether an
//! interpreter path exists. That is the right question to ask a working tree and
//! the wrong one to ask a release: `cargo tree` describes an intent, and a
//! `#[cfg]`, a monomorphization or a trait object can put code into an
//! executable that no line of source appears to call. The release gate asks the
//! artifact.
//!
//! Three things are asked of it, and the third is the one that decides:
//!
//! 1. No interpreter *entry point* is present. `clarity::vm::eval_all` is the
//!    only way to run a Clarity program body, `initialize_versioned_contract`
//!    the only interpreted deployment, `OwnedEnvironment::execute_transaction`
//!    the only interpreted contract call. None is in the symbol table.
//! 2. No retired engine switch appears in the binary's string data, so an
//!    operator who sets one is setting something nothing in the image can read.
//! 3. **The interpreter's evaluator cannot be entered.** Its leaf machinery *is*
//!    in the image — `clarity` is one rlib whose frontend, ABI types and cost
//!    machinery clarity-wasm consumes, and the linker keeps whole code
//!    generation units, so `clarity::vm::eval`, `apply` and the `special_*`
//!    builtins are all there as bytes. What matters is whether anything reaches
//!    them, and the disassembly says nothing does: `lookup_reserved_functions`,
//!    `lookup_function` and `apply` — every route from a function name to an
//!    interpreted implementation — have **zero reference sites** in the whole
//!    executable, and the three references to `eval` are from `special_let` and
//!    `special_map`, which are inside that unreachable region.
//!
//! Point 3 is a stronger statement than "no interpreter is linked", which is not
//! true and cannot be: it is that the interpreter present in the image is dead
//! code with no edge into it.
//!
//! These run `nm` and `objdump`. On a machine without them the gate says so
//! rather than passing, which is what `skip_gate` is for.

use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
};

use nano_conformance::skip_gate;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root is two levels above this crate")
}

/// Build the shipped binary and say where it landed.
///
/// Asked of cargo rather than assembled from `target/release/stacks-node`: a
/// `CARGO_TARGET_DIR`, a `--target` triple or a workspace `build.target-dir`
/// each move it, and a test that inspected a stale path would report on a binary
/// nobody built. Built once for the whole test binary, because three tests want
/// it and the modules are one process.
fn artifact() -> &'static Path {
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
            .current_dir(workspace())
            .output()
            .expect("cargo build runs");
        assert!(
            output.status.success(),
            "the release binary does not build: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let path = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|message| message["reason"] == "compiler-artifact")
            .filter_map(|message| message["executable"].as_str().map(PathBuf::from))
            .find(|path| path.file_name().is_some_and(|name| name == "stacks-node"))
            .expect("cargo reports where it put stacks-node");
        assert!(path.is_file(), "{} is not a file", path.display());
        path
    })
}

/// Every symbol in the artifact, demangled, or `None` if `nm` is unavailable.
fn symbols() -> Option<&'static str> {
    static SYMBOLS: OnceLock<Option<String>> = OnceLock::new();
    SYMBOLS
        .get_or_init(|| {
            let output = Command::new("nm").arg("-C").arg(artifact()).output().ok()?;
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
        })
        .as_deref()
}

/// Lines of the artifact's disassembly that mention any of `wanted`.
///
/// Streamed and filtered rather than collected: the whole disassembly is 4.8
/// million lines, and every question here is about a handful of them. Roughly
/// five seconds.
fn disassembly_mentioning(wanted: &[&str]) -> Option<Vec<String>> {
    let mut child = Command::new("objdump")
        .args(["-d", "--no-show-raw-insn"])
        .arg(artifact())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let matched = BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .filter(|line| wanted.iter().any(|pattern| line.contains(pattern)))
        .collect();
    child.wait().ok()?.success().then_some(matched)
}

/// No interpreter entry point is in the shipped image.
///
/// These are the three ways to begin an interpreted execution. A binary without
/// any of them cannot start one, whatever else it carries.
#[test]
fn the_artifact_holds_no_interpreter_entry_point() {
    let Some(symbols) = symbols() else {
        skip_gate("nm is not available, so the artifact cannot be inspected");
        return;
    };
    for entry_point in [
        "clarity::vm::eval_all",
        "clarity::vm::execute",
        "OwnedEnvironment>::initialize_versioned_contract",
        "OwnedEnvironment>::execute_transaction",
        "OwnedEnvironment>::execute_in_env",
        "Environment>::execute_contract",
        "Environment>::initialize_contract",
        "nano_oracle",
    ] {
        assert!(
            !symbols.contains(entry_point),
            "the shipped stacks-node contains {entry_point}, which can begin an \
             interpreted execution"
        );
    }

    // Stated as the exact set rather than as a deny-list, for the reason
    // `release_dependencies` states it about features: a deny-list refuses only
    // what somebody already thought of, and the failure being guarded against is
    // an entry point arriving that nobody chose. `stx_transfer` is the one
    // legitimate member — a token transfer evaluates no Clarity and is the same
    // native path stacks-core takes — and `new_cost_limited` and `commit` are
    // what it needs to be called and closed. Anything else on this type is an
    // interpreted execution.
    let present: std::collections::BTreeSet<&str> = symbols
        .lines()
        .filter_map(|line| {
            line.split("<clarity::vm::contexts::OwnedEnvironment>::")
                .nth(1)
        })
        .collect();
    assert_eq!(
        present,
        ["commit", "new_cost_limited", "stx_transfer"]
            .into_iter()
            .collect(),
        "the interpreter's environment has methods in the shipped binary that \
         nano did not ask for"
    );

    // And the engine it does execute through is present, so none of the above
    // passed because the binary was empty or stripped.
    for engine in ["clar2wasm", "wasmtime"] {
        assert!(
            symbols.contains(engine),
            "the shipped stacks-node has no {engine} symbols, so this test asked \
             nothing"
        );
    }
}

/// Nothing in the artifact can reach the interpreter's evaluator.
///
/// Every interpreted call goes through one of these three: a function name is
/// resolved by `lookup_reserved_functions` or `lookup_function` and the result is
/// invoked by `apply`. Each is present in the image as dead code and each has no
/// reference site anywhere in it — no call, and no address taken for a later
/// call.
///
/// `DefineFunctionsParsed::try_parse` was tried here and taken back out: it is
/// how `eval_all` recognizes a top-level definition, but it is *also* how
/// `ArithmeticOnlyChecker` recognizes one, and that pass is the analyzer's. Its
/// one reference site in the image comes from there, so it says nothing about
/// evaluation. Worth recording — the three below are witnesses precisely because
/// the analyzer has no use for them.
#[test]
fn the_artifact_cannot_enter_the_interpreters_evaluator() {
    // Matched on the mangled names, which is what a disassembly's cross-
    // reference annotations carry. `v0`'s hash differs per build, so the crate
    // and path components are what is spelled out.
    const UNREACHABLE: [(&str, &str); 3] = [
        (
            "lookup_reserved_functions",
            "9functions25lookup_reserved_functions",
        ),
        (
            "clarity::vm::lookup_function",
            "7clarity2vm15lookup_function",
        ),
        ("clarity::vm::apply", "7clarity2vm5apply"),
    ];
    let patterns: Vec<&str> = UNREACHABLE.iter().map(|(_, mangled)| *mangled).collect();
    let Some(lines) = disassembly_mentioning(&patterns) else {
        skip_gate("objdump is not available, so the artifact cannot be disassembled");
        return;
    };

    for (name, mangled) in UNREACHABLE {
        // A definition is `<addr> <symbol>:`; a cross reference is an
        // instruction whose comment ends `<symbol>`. Only the second is an edge
        // into the function, and the first is worth counting too — a symbol that
        // vanished from the image entirely would otherwise pass this vacuously,
        // and would want somebody to look rather than to be told nothing.
        let mut definitions = 0_usize;
        let mut references = 0_usize;
        for line in &lines {
            let Some(rest) = line.split(&format!("{mangled}>")).nth(1) else {
                continue;
            };
            if rest.starts_with(':') {
                definitions += 1;
            } else if rest.is_empty() {
                references += 1;
            }
        }
        assert_eq!(
            references, 0,
            "{name} is reachable from {references} site(s) in the shipped binary, \
             so an interpreted evaluation can be started in it"
        );
        assert_eq!(
            definitions, 1,
            "{name} is not in the shipped image at all; this test no longer says \
             what it claims and needs rewriting against whatever replaced it"
        );
    }
}

/// No retired engine switch survives in the binary's data.
///
/// The source-level version of this lives in `wasm_is_the_engine`. This is the
/// same question asked of the shipped bytes, which is where an operator's
/// `NANO_INTERPRETER_FALLBACK=1` would have to be compared against something.
#[test]
fn the_artifact_names_no_retired_engine_switch() {
    let bytes = std::fs::read(artifact()).expect("read the artifact");
    for switch in [
        "NANO_INTERPRETER_ONLY",
        "NANO_INTERPRETER_FALLBACK",
        "NANO_CROSSCHECK",
        "interpret_contract_calls",
    ] {
        assert!(
            !contains(&bytes, switch.as_bytes()),
            "the shipped stacks-node contains the bytes {switch:?}"
        );
    }
    // The binary does read *some* environment, so the absence above is not
    // because nothing is ever compared: this one is real and documented.
    assert!(
        contains(&bytes, b"NANO_DUMP_REFUSED_WASM"),
        "not even the diagnostic dump variable is in the image, so this test \
         cannot distinguish an absent switch from an absent string table"
    );
}

/// No configuration file can select an engine.
///
/// Asked of the assembled binary through the subcommand that does nothing but
/// read a configuration, so what is proved is that the *shipped* parser refuses
/// it — `deny_unknown_fields` in a struct is the mechanism, and this is the
/// observable consequence.
#[test]
fn the_binary_refuses_a_configuration_that_names_an_engine() {
    let directory = tempfile::tempdir().expect("a directory");
    let config = directory.path().join("nano.toml");
    let write = |extra: &str| {
        std::fs::write(
            &config,
            format!(
                "[node]\nworking_dir = {:?}\npeers = [\"http://127.0.0.1:20443\"]\n{extra}\n\
                 [burnchain]\nrpc_url = \"http://127.0.0.1:8332\"\n\
                 rpc_user = \"u\"\nrpc_password = \"p\"\nmagic = \"X2\"\n\
                 [checkpoint]\nmarf = \"marf.sqlite\"\n\
                 source_state_id = \"{}\"\nstate_root = \"{}\"\n\
                 anchor_block = \"anchor.bin\"\nanchor_bitcoin_height = 1\n",
                directory.path(),
                "00".repeat(32),
                "00".repeat(32),
            ),
        )
        .expect("write a configuration");
    };
    let check = || {
        Command::new(artifact())
            .args(["check-config", "--config"])
            .arg(&config)
            .output()
            .expect("the binary runs")
    };

    // First that the configuration is otherwise acceptable, so the refusal
    // below is about the field and not about the file.
    write("");
    let accepted = check();
    assert!(
        accepted.status.success(),
        "the baseline configuration is not readable, so this test proves nothing: {}",
        String::from_utf8_lossy(&accepted.stderr)
    );

    for field in [
        "engine = \"interpreter\"",
        "interpreter = true",
        "interpreter_fallback = true",
        "crosscheck = true",
    ] {
        write(field);
        let refused = check();
        assert!(
            !refused.status.success(),
            "the shipped binary accepted `{field}` in [node]"
        );
        let complaint = String::from_utf8_lossy(&refused.stderr);
        assert!(
            complaint.contains("unknown field"),
            "the shipped binary refused `{field}` for some other reason: {complaint}"
        );
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
