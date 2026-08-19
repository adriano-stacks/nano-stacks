//! The supervised Epoch 4.0 executor: the sole chainstate writer.
//!
//! One process, one state directory, no listener and no client capability.
//! The serving loop lives in [`epoch4_consensus::host`]; this shell only reads
//! its arguments and hands over stdin and stdout.

use std::process::ExitCode;

use epoch4_consensus::host;

/// Execution decodes trie nodes and boxes children on every cache miss; the
/// same measured choice every other execution binary in this tree makes.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let (directory, network, registry) = match arguments.as_slice() {
        [directory, network] => (directory, network, None),
        [directory, network, registry] => (directory, network, Some(registry.clone())),
        _ => {
            eprintln!(
                "usage: epoch4-executor <state-dir> <mainnet|testnet|chain-id:N> [waterfall-registry]"
            );
            return ExitCode::FAILURE;
        }
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
        registry.as_deref(),
        std::io::stdin().lock(),
        std::io::stdout().lock(),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("epoch4-executor: {error}");
            ExitCode::FAILURE
        }
    }
}
