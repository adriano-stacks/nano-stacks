#![forbid(unsafe_code)]

use std::{error::Error, fs, io, path::PathBuf, str::FromStr, time::Duration};

use bitcoin::{Amount, Txid};
use bitcoincore_rpc::Auth;
use clap::Parser;
use nano_bitcoin::BitcoinRpcSource;
use nano_miner::{
    BitcoinWallet, CommitmentPlan, CommitmentPlanError, RegisteredLeaderKey, plan_commitment,
};
use nano_sync::SyncClient;
use reqwest::Url;
use tokio::time::{Instant, sleep};

#[derive(Parser)]
#[command(name = "stacks-commit-block")]
/// Commit to the next Bitcoin sortition for the peer's canonical tenure.
struct Cli {
    /// Stock node HTTP endpoint.
    #[arg(long, default_value = "http://127.0.0.1:20443/")]
    peer: String,
    /// Bitcoin Core RPC endpoint.
    #[arg(long)]
    bitcoin_rpc: String,
    /// Bitcoin Core RPC username.
    #[arg(long)]
    bitcoin_rpc_user: String,
    /// File containing the Bitcoin Core RPC password.
    #[arg(long)]
    bitcoin_rpc_password_file: PathBuf,
    /// Bitcoin Core wallet funding the commitment.
    #[arg(long)]
    bitcoin_wallet: String,
    /// Bitcoin transaction that registered this miner's leader key.
    #[arg(long, required_unless_present = "key_bitcoin_height")]
    key_txid: Option<String>,
    /// Bitcoin height of this miner's leader-key registration.
    #[arg(long, requires = "key_transaction_index", conflicts_with = "key_txid")]
    key_bitcoin_height: Option<u32>,
    /// Position of this miner's leader-key registration within its Bitcoin block.
    #[arg(long, requires = "key_bitcoin_height")]
    key_transaction_index: Option<u16>,
    /// Satoshis committed to the reward cycle's sBTC address.
    #[arg(long)]
    commitment_sats: u64,
    /// Optional Bitcoin fee rate in satoshis per vbyte.
    #[arg(long)]
    fee_rate_sats_per_vbyte: Option<u64>,
    /// Two hexadecimal magic bytes for the target network.
    #[arg(long, default_value = "5433")]
    magic: String,
    /// Seconds to wait for the peer to catch up with the Bitcoin tip.
    #[arg(long, default_value_t = 30)]
    peer_timeout_secs: u64,
    /// Derive and print the commitment without broadcasting it.
    #[arg(long)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let password = fs::read_to_string(&cli.bitcoin_rpc_password_file)?
        .trim_end()
        .to_owned();
    let magic = parse_hex_array(&cli.magic)?;
    let mut bitcoin = BitcoinRpcSource::new(
        &cli.bitcoin_rpc,
        cli.bitcoin_rpc_user.clone(),
        password.clone(),
        magic,
    )?;
    let wallet = BitcoinWallet::connect(
        &format!(
            "{}/wallet/{}",
            cli.bitcoin_rpc.trim_end_matches('/'),
            cli.bitcoin_wallet
        ),
        Auth::UserPass(cli.bitcoin_rpc_user.clone(), password),
    )?;

    let key = leader_key(&cli, &wallet)?;
    let node = SyncClient::new(Url::parse(&cli.peer)?)?;
    let plan = synchronized_plan(&node, &mut bitcoin, &wallet, key, cli.peer_timeout_secs).await?;
    println!(
        "committing to tenure {} at Bitcoin height {} in reward cycle {}",
        hex::encode(plan.commitment.block_header_hash),
        plan.target_bitcoin_height,
        plan.reward_cycle
    );
    if cli.dry_run {
        println!("{:?}", plan.commitment);
        return Ok(());
    }

    let submitted = wallet.submit_leader_commitment(
        magic,
        plan.commitment,
        &plan.sbtc_address,
        Amount::from_sat(cli.commitment_sats),
        cli.fee_rate_sats_per_vbyte,
    )?;
    println!(
        "submitted leader commitment {} paying {} sats with fee {}",
        submitted.transaction_id, cli.commitment_sats, submitted.fee
    );
    Ok(())
}

/// Derive a commitment once the peer has processed the Bitcoin tip it targets.
async fn synchronized_plan(
    node: &SyncClient,
    bitcoin: &mut BitcoinRpcSource,
    wallet: &BitcoinWallet,
    key: RegisteredLeaderKey,
    timeout_secs: u64,
) -> Result<CommitmentPlan, Box<dyn Error>> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        match plan_commitment(node, bitcoin, key, wallet.block_count()?).await {
            Err(CommitmentPlanError::StaleNodeView { node, bitcoin }) => {
                if Instant::now() >= deadline {
                    return Err(CommitmentPlanError::StaleNodeView { node, bitcoin }.into());
                }
                sleep(Duration::from_millis(500)).await;
            }
            result => return Ok(result?),
        }
    }
}

fn leader_key(cli: &Cli, wallet: &BitcoinWallet) -> Result<RegisteredLeaderKey, Box<dyn Error>> {
    if let (Some(bitcoin_height), Some(transaction_index)) =
        (cli.key_bitcoin_height, cli.key_transaction_index)
    {
        return Ok(RegisteredLeaderKey {
            bitcoin_height,
            transaction_index,
        });
    }
    let txid = cli.key_txid.as_deref().expect("clap requires a key source");
    let (height, index) = wallet.confirmed_position(Txid::from_str(txid)?)?;
    Ok(RegisteredLeaderKey {
        bitcoin_height: u32::try_from(height)?,
        transaction_index: u16::try_from(index)?,
    })
}

fn parse_hex_array<const N: usize>(value: &str) -> Result<[u8; N], io::Error> {
    let bytes = hex::decode(value.trim().trim_start_matches("0x")).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid hexadecimal value: {error}"),
        )
    })?;
    let length = bytes.len();
    bytes.try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("expected {N} bytes, found {length}"),
        )
    })
}
