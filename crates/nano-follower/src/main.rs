//! The minimal Epoch-4 follower artifact.

use std::{error::Error, path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use nano_follower::{BUILD_TARGET, RUSTC_VERSION, SOURCE_REVISION, config::Config, runtime};

#[derive(Parser)]
#[command(
    name = "stacks-follower",
    about = "Authenticate and follow the Stacks Epoch-4 chain"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Follow the chain until SIGINT or SIGTERM.
    Start {
        #[arg(long)]
        config: PathBuf,
    },
    /// Validate the closed follower configuration without opening state.
    CheckConfig {
        #[arg(long)]
        config: PathBuf,
    },
    /// Print the complete accepted configuration surface.
    ConfigSchema,
    /// Print immutable build and consensus identities.
    BuildIdentity,
    /// Print the executable Epoch-4 compatibility profile.
    CompatibilityProfile,
}

#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() -> ExitCode {
    let result = dispatch(Cli::parse().command).await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("stacks-follower: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn dispatch(command: Command) -> Result<(), Box<dyn Error>> {
    match command {
        Command::Start { config } => runtime::run(Config::load(config)?).await,
        Command::CheckConfig { config } => {
            Config::load(config)?;
            println!("follower configuration is valid");
            Ok(())
        }
        Command::ConfigSchema => {
            println!(
                "{}",
                serde_json::to_string_pretty(&schemars::schema_for!(Config))?
            );
            Ok(())
        }
        Command::BuildIdentity => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "source_revision": SOURCE_REVISION,
                    "compiler_identity": nano_vm::COMPILER_IDENTITY,
                    "rustc": RUSTC_VERSION,
                    "target": BUILD_TARGET,
                    "wasmtime": nano_vm::WASMTIME_VERSION,
                    "wasmtime_engine": nano_vm::WASMTIME_ENGINE_CONFIG,
                    "compatibility_profile": nano_consensus_profile::PROFILE_ID,
                    "compatibility_profile_sha256": nano_consensus_profile::profile_sha256(),
                    "compatibility_vectors_sha256": nano_consensus_profile::vectors_sha256(),
                    "compatibility_fingerprint": nano_vm::compatibility_profile_fingerprint(),
                }))?
            );
            Ok(())
        }
        Command::CompatibilityProfile => {
            println!("{}", nano_vm::compatibility_profile_json()?);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory as _, Parser as _};

    use super::Cli;

    #[test]
    fn the_binary_has_no_optional_role_or_public_service_switch() {
        let help = Cli::command().render_long_help().to_string();
        for forbidden in [
            "miner",
            "signer",
            "proposal",
            "stackerdb",
            "mempool",
            "rpc-bind",
            "event-observer",
            "tui",
        ] {
            assert!(!help.to_lowercase().contains(forbidden), "{forbidden}");
        }
        assert!(Cli::try_parse_from(["stacks-follower", "start", "--config", "x"]).is_ok());
    }
}
