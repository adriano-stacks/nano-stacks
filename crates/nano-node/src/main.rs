//! The nano-stacks node: one binary, one configuration file, one state directory.

use std::{error::Error, path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use nano_node::{config::Config, runtime};

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
}

#[tokio::main]
async fn main() -> ExitCode {
    let Cli { command } = Cli::parse();
    let result = match command {
        Command::Start { config } => start(&config).await,
        Command::CheckConfig { config } => check(&config),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("stacks-node: {error}");
            ExitCode::FAILURE
        }
    }
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
