//! The nano-stacks node: one binary, one configuration file, one state directory.

use std::{error::Error, path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use nano_node::{BUILD_TARGET, RUSTC_VERSION, SOURCE_REVISION, config::Config, runtime};

#[derive(Parser)]
#[command(name = "stacks-node", about = "A Stacks epoch-4 node")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the roles this configuration switches on until SIGTERM.
    Start {
        #[arg(long)]
        config: PathBuf,
    },
    /// Read the configuration and report what it would run, without running it.
    CheckConfig {
        #[arg(long)]
        config: PathBuf,
    },
    /// Print the accepted configuration as JSON Schema.
    ConfigSchema,
    /// Print the immutable identities embedded in this artifact.
    BuildIdentity,
}

/// The same allocator the replay tool measured: 14% off a mainnet replay, and
/// an allocator with page decay also returns freed churn to the OS instead of
/// ratcheting RSS the way glibc arenas did on the follower.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() -> ExitCode {
    let Cli { command } = Cli::parse();
    let result = match command {
        Command::Start { config } => start(&config).await,
        Command::CheckConfig { config } => check(&config),
        Command::ConfigSchema => config_schema(),
        Command::BuildIdentity => build_identity(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("stacks-node: {error}");
            ExitCode::FAILURE
        }
    }
}

fn config_schema() -> Result<(), Box<dyn Error>> {
    println!(
        "{}",
        serde_json::to_string_pretty(&schemars::schema_for!(Config))?
    );
    Ok(())
}

fn build_identity() -> Result<(), Box<dyn Error>> {
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "source_revision": SOURCE_REVISION,
            "compiler_identity": nano_vm::COMPILER_IDENTITY,
            "rustc": RUSTC_VERSION,
            "target": BUILD_TARGET,
            "wasmtime": nano_vm::WASMTIME_VERSION,
            "wasmtime_engine": nano_vm::WASMTIME_ENGINE_CONFIG,
        }))?
    );
    Ok(())
}

async fn start(path: &PathBuf) -> Result<(), Box<dyn Error>> {
    runtime::run(Config::load(path)?).await
}

fn check(path: &PathBuf) -> Result<(), Box<dyn Error>> {
    let config = Config::load(path)?;
    let mut roles = vec!["follower"];
    if config.signer.is_some() {
        roles.push("signer");
    }
    if config.miner.is_some() {
        roles.push("miner");
    }
    println!(
        "{} would run as {} over {} peer(s), state under {}",
        path.display(),
        roles.join(" + "),
        config.node.peers.len(),
        config.node.working_dir.display()
    );
    Ok(())
}
