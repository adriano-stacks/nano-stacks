//! The epoch4 executor protocol, hosted from the conformance crate.
//!
//! `epoch4_shadow` spawns this to prove the decision boundary answers
//! identically in and out of process. Two doors in, one protocol out: a
//! durable state directory served exactly as the production `epoch4-executor`
//! shell serves it, or `--capture <root> <state-dir>` — a captured chain's
//! checkpoint imported durably into `<state-dir>` through the same
//! conformance helper the in-process side of the gate uses, and resumed from
//! it when the directory already holds the import. A separate binary exists
//! only because
//! `CARGO_BIN_EXE_*` reaches binaries of the crate under test and nothing
//! else.

use std::process::ExitCode;

use epoch4_consensus::host;

#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let outcome = match arguments.as_slice() {
        [capture, root, directory] if capture == "--capture" => serve_capture(root, directory),
        [directory, network] => match host::parse_network(network) {
            Ok(network) => host::serve(
                directory,
                network,
                None,
                std::io::stdin().lock(),
                std::io::stdout().lock(),
            ),
            Err(error) => Err(error),
        },
        _ => Err(
            "usage: epoch4-shadow-executor <state-dir> <mainnet|testnet|chain-id:N> \
             | --capture <capture-root> <state-dir>"
                .to_owned(),
        ),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("epoch4-shadow-executor: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Serve decisions over a captured chain's checkpoint, imported durably.
fn serve_capture(root: &str, directory: &str) -> Result<(), String> {
    let (chainstate, standing, _) = nano_conformance::shadow_capture_chainstate(
        std::path::Path::new(root),
        std::path::Path::new(directory),
    )?;
    let mut lines = std::io::stdin().lock();
    let stand = host::read_stand(&mut lines)?;
    if stand.block_id() != standing.block_id() {
        return Err(format!(
            "the parent stands on {} but this capture state stands on {}",
            stand.block_id(),
            standing.block_id()
        ));
    }
    host::serve_over(chainstate, standing, None, lines, std::io::stdout().lock())
}
