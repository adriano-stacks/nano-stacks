//! The epoch4 executor protocol, hosted from the conformance crate.
//!
//! `epoch4_shadow` spawns this over a fixture state directory to prove the
//! decision boundary answers identically in and out of process. It is the same
//! [`epoch4_consensus::host::serve`] loop the production `epoch4-executor`
//! shell runs; a separate binary exists only because `CARGO_BIN_EXE_*` reaches
//! binaries of the crate under test and nothing else.

use std::process::ExitCode;

use epoch4_consensus::host;

#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let [directory, network] = arguments.as_slice() else {
        eprintln!("usage: epoch4-shadow-executor <state-dir> <mainnet|testnet|chain-id:N>");
        return ExitCode::FAILURE;
    };
    let network = match host::parse_network(network) {
        Ok(network) => network,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    match host::serve(
        directory,
        network,
        None,
        std::io::stdin().lock(),
        std::io::stdout().lock(),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("epoch4-shadow-executor: {error}");
            ExitCode::FAILURE
        }
    }
}
