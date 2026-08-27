//! The nano-stacks node: one binary, one configuration file, one state directory.

use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::{Args, Parser, Subcommand};
use nano_bitcoin::BitcoinRpcSource;
use nano_node::{BUILD_TARGET, RUSTC_VERSION, SOURCE_REVISION, config::Config, runtime};

#[derive(Parser)]
#[command(name = "stacks-node", about = "A Stacks epoch-4 node")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct LocalBitcoin {
    /// This operator's locally verified Bitcoin Core RPC endpoint.
    #[arg(long = "bitcoin-rpc-url")]
    url: String,
    #[arg(long = "bitcoin-rpc-user")]
    user: String,
    /// File containing the Bitcoin Core RPC password, kept out of argv.
    #[arg(long = "bitcoin-rpc-password-file")]
    password_file: PathBuf,
}

impl LocalBitcoin {
    fn open(&self) -> Result<BitcoinRpcSource, Box<dyn Error>> {
        Ok(BitcoinRpcSource::new(
            &self.url,
            self.user.clone(),
            fs::read_to_string(&self.password_file)?.trim().to_owned(),
            [0; 2],
        )?)
    }
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
    /// Print the exact executable Epoch-4 profile embedded in this artifact.
    CompatibilityProfile,
    /// Build a new content-addressed manifest after checking its signer proof.
    BuildCheckpointManifest {
        #[arg(long)]
        bundle: PathBuf,
        #[command(flatten)]
        bitcoin: LocalBitcoin,
    },
    /// Add one immutable builder signature over a verified checkpoint manifest.
    SignCheckpointManifest {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        policy: PathBuf,
        #[arg(long)]
        signatures: PathBuf,
        #[arg(long)]
        builder: String,
        #[arg(long)]
        private_key: PathBuf,
        #[command(flatten)]
        bitcoin: LocalBitcoin,
    },
    /// Let this artifact use a state another compiler imported, if it can prove it.
    AdoptImportedState {
        #[arg(long)]
        state: PathBuf,
        #[arg(long)]
        checkpoint: PathBuf,
    },
    /// Verify a checkpoint bundle without opening or writing node state.
    VerifyCheckpoint {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        policy: PathBuf,
        #[arg(long)]
        signatures: PathBuf,
        #[command(flatten)]
        bitcoin: LocalBitcoin,
    },
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
        Command::CompatibilityProfile => compatibility_profile(),
        Command::BuildCheckpointManifest { bundle, bitcoin } => {
            build_checkpoint_manifest(&bundle, &bitcoin)
        }
        Command::SignCheckpointManifest {
            bundle,
            policy,
            signatures,
            builder,
            private_key,
            bitcoin,
        } => sign_checkpoint_manifest(
            &bundle,
            &policy,
            &signatures,
            &builder,
            &private_key,
            &bitcoin,
        ),
        Command::AdoptImportedState { state, checkpoint } => {
            adopt_imported_state(&state, &checkpoint)
        }
        Command::VerifyCheckpoint {
            bundle,
            policy,
            signatures,
            bitcoin,
        } => verify_checkpoint(&bundle, &policy, &signatures, &bitcoin),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("stacks-node: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Adopt an untouched import under this artifact's profile.
///
/// A compiler change makes a node refuse a state another compiler imported, and
/// the sanctioned answer is a fresh import — three hours reproducing a state that
/// is a pure function of the bundle, because nothing has executed yet. This is
/// that answer without the three hours, and it is a proof rather than a repin:
/// the checks are in `nano_vm::adopt_state_under_active_profile`, and a state that
/// fails any of them is refused untouched.
fn adopt_imported_state(state: &Path, checkpoint: &Path) -> Result<(), Box<dyn Error>> {
    let manifest = nano_marf::CheckpointManifest::load(checkpoint)?;
    let adopted = nano_vm::adopt_state_under_active_profile(state, &manifest)?;
    println!(
        "adopted {} at height {} sealing {}",
        state.display(),
        adopted.stacks_height,
        adopted.state_index_root
    );
    match adopted.was {
        Some(was) => println!("  profile {was} -> {}", adopted.now),
        None => println!("  profile <none recorded> -> {}", adopted.now),
    }
    println!(
        "  the state root it seals is the one the checkpoint claims and a signed header endorsed"
    );
    Ok(())
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
            "compatibility_profile": nano_consensus_profile::PROFILE_ID,
            "compatibility_profile_sha256": nano_consensus_profile::profile_sha256().to_string(),
            "compatibility_vectors_sha256": nano_consensus_profile::vectors_sha256().to_string(),
            "compatibility_fingerprint": nano_vm::compatibility_profile_fingerprint().to_string(),
        }))?
    );
    Ok(())
}

fn compatibility_profile() -> Result<(), Box<dyn Error>> {
    println!("{}", nano_vm::compatibility_profile_json()?);
    Ok(())
}

fn build_checkpoint_manifest(bundle: &Path, bitcoin: &LocalBitcoin) -> Result<(), Box<dyn Error>> {
    let bitcoin = bitcoin.open()?;
    let manifest =
        nano_node::checkpoint_bundle::build_checkpoint_bundle_manifest(bundle, &bitcoin)?;
    println!("checkpoint content root {}", manifest.content_root());
    Ok(())
}

fn sign_checkpoint_manifest(
    bundle: &Path,
    policy: &Path,
    signatures: &Path,
    builder: &str,
    private_key: &Path,
    bitcoin: &LocalBitcoin,
) -> Result<(), Box<dyn Error>> {
    let policy = nano_node::checkpoint_signatures::BuilderPolicy::load(policy)?;
    let bitcoin = bitcoin.open()?;
    let private_key = fs::read_to_string(private_key)?;
    let bytes: [u8; 32] = hex::decode(private_key.trim())?
        .try_into()
        .map_err(|_| "builder private key is not 32-byte hexadecimal")?;
    let private_key = nano_crypto::StacksPrivateKey::from_bytes(bytes)?;
    let path = nano_node::checkpoint_signatures::sign_checkpoint_bundle(
        bundle,
        &bitcoin,
        &policy,
        signatures,
        builder,
        &private_key,
    )?;
    println!("wrote builder signature {}", path.display());
    Ok(())
}

fn verify_checkpoint(
    bundle: &Path,
    policy: &Path,
    signatures: &Path,
    bitcoin: &LocalBitcoin,
) -> Result<(), Box<dyn Error>> {
    let policy = nano_node::checkpoint_signatures::BuilderPolicy::load(policy)?;
    let bitcoin = bitcoin.open()?;
    let verified = nano_node::checkpoint_signatures::verify_signed_checkpoint_bundle(
        bundle, &bitcoin, &policy, signatures,
    )?;
    println!(
        "checkpoint content root {} verified by {}",
        verified.content_root,
        verified.names.join(", ")
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
