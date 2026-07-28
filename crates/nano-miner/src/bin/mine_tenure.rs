#![forbid(unsafe_code)]

use std::{error::Error, fs, io, path::PathBuf, str::FromStr, time::Duration};

use clap::Parser;
use nano_address::StacksAddress;
use nano_bitcoin::BitcoinRpcSource;
use nano_chainstate::{NakamotoBlock, SignerSetError, TenureAccounting};
use nano_crypto::{StacksPrivateKey, VrfPrivateKey};
use nano_miner::{
    BitcoinTenureView, ProposalCoordinator, ProposalError, SortitionHashPoint,
    build_tenure_start_block, extend_sortition_hash, total_burn_after,
};
use nano_node::CheckpointExecutor;
use nano_primitives::{TrieHash, hash160};
use nano_stackerdb::{BlockProposal, StackerDbClient, StackerDbContract};
use nano_sync::SyncClient;
use reqwest::Url;
use tokio::time::{Instant, sleep};

#[derive(Parser)]
#[command(name = "stacks-mine-tenure")]
/// Mine the first block of a tenure this miner won on Bitcoin.
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
    /// Miner `StackerDB` contract as ADDRESS/name.
    #[arg(long)]
    miner_contract: String,
    /// Boot address hosting the per-cycle signer `StackerDB` contracts.
    #[arg(long, default_value = "ST000000000000000000002AMW42H")]
    signer_contract_address: String,
    /// File containing the hex-encoded 32-byte block-signing private key.
    #[arg(long)]
    block_signing_private_key_file: PathBuf,
    /// File containing the hex-encoded 32-byte VRF private key.
    #[arg(long)]
    vrf_private_key_file: PathBuf,
    /// `SQLite` MARF checkpoint path.
    #[arg(long)]
    checkpoint: PathBuf,
    /// Portable tenure-accounting checkpoint for matured native rewards.
    #[arg(long)]
    tenure_accounting: Option<PathBuf>,
    /// Hex-encoded 32-byte checkpoint state ID.
    #[arg(long)]
    source_state_id: String,
    /// Hex-encoded 32-byte checkpoint state root.
    #[arg(long)]
    state_root: String,
    /// Consensus-encoded block immediately after the checkpoint.
    #[arg(long)]
    anchor_block: PathBuf,
    /// Bitcoin height that anchored the checkpoint successor.
    #[arg(long)]
    anchor_bitcoin_height: u64,
    /// Cached sortition-hash chain point, extended and rewritten on each run.
    #[arg(long)]
    sortition_hash_cache: PathBuf,
    /// Network chain identifier.
    #[arg(long, default_value_t = 0x8000_0000)]
    chain_id: u32,
    /// Bitcoin height at which PoX-5 activates.
    #[arg(long)]
    pox_5_activation_height: Option<u32>,
    /// Bitcoin height used when evaluating PoX-1 STX locks.
    #[arg(long)]
    pox_v1_unlock_height: u32,
    /// Bitcoin height used when evaluating PoX-2 STX locks.
    #[arg(long)]
    pox_v2_unlock_height: u32,
    /// Bitcoin height used when evaluating PoX-3 STX locks.
    #[arg(long)]
    pox_v3_unlock_height: u32,
    /// Seconds to wait for a sortition this miner wins.
    #[arg(long, default_value_t = 600)]
    sortition_timeout_secs: u64,
    /// Seconds to wait for the threshold signer response set.
    #[arg(long, default_value_t = 120)]
    signer_timeout_secs: u64,
    /// Seconds between polls.
    #[arg(long, default_value_t = 2)]
    poll_interval_secs: u64,
    /// Maximum canonical blocks to execute while catching up to the peer.
    #[arg(long, default_value_t = 20_000)]
    max_sync_blocks: usize,
    /// Assemble and print the block without publishing it.
    #[arg(long)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let node = SyncClient::new(Url::parse(&cli.peer)?)?;
    let pox = node.pox_info().await?;
    let password = fs::read_to_string(&cli.bitcoin_rpc_password_file)?
        .trim_end()
        .to_owned();
    let miner_key =
        StacksPrivateKey::from_bytes(read_hex_array(&cli.block_signing_private_key_file)?)?;
    let vrf_key = VrfPrivateKey::from_bytes(read_hex_array(&cli.vrf_private_key_file)?);
    let miner_hash = hash160(&miner_key.public_key().to_bytes_compressed());

    let mut bitcoin_context = pox.bitcoin_context();
    bitcoin_context.height = cli.anchor_bitcoin_height;
    bitcoin_context.v1_unlock_height = cli.pox_v1_unlock_height;
    bitcoin_context.v2_unlock_height = cli.pox_v2_unlock_height;
    bitcoin_context.v3_unlock_height = cli.pox_v3_unlock_height;
    if let Some(height) = cli.pox_5_activation_height {
        bitcoin_context.pox_5_activation_height = height;
    }

    let mut executor = checkpoint_executor(&cli, &password, bitcoin_context)?;
    let executed = executor
        .follow_to_tip(&node, &pox, cli.max_sync_blocks)
        .await?;
    println!("executed {executed} canonical blocks up to the peer tip");

    let deadline = Instant::now() + Duration::from_secs(cli.sortition_timeout_secs);
    let won = loop {
        // A Bitcoin block without a sortition does not end the previous tenure,
        // so the tenure to mine is the last sortition that chose a miner.
        let tip = node.sortition_tip().await?;
        let current = if tip.was_sortition {
            Some(tip)
        } else {
            match tip.last_sortition_consensus_hash {
                Some(consensus_hash) => Some(node.sortition(consensus_hash).await?),
                None => None,
            }
        };
        if let Some(current) = current
            && current.was_sortition
            && current.miner_public_key_hash == Some(miner_hash)
        {
            break current;
        }
        if Instant::now() >= deadline {
            return Err("no sortition was won before the timeout".into());
        }
        sleep(Duration::from_secs(cli.poll_interval_secs)).await;
    };
    println!(
        "won the sortition at Bitcoin height {} with consensus hash {}",
        won.bitcoin_height, won.consensus_hash
    );

    executor
        .follow_to_tip(&node, &pox, cli.max_sync_blocks)
        .await?;
    let view = bitcoin_tenure_view(&cli, &node, &password, &won, pox.first_bitcoin_height).await?;
    let candidate = build_tenure_start_block(
        &node,
        &won,
        view,
        cli.chain_id,
        &miner_key,
        &vrf_key,
        won.bitcoin_timestamp,
    )
    .await?;

    let mut tenure_context = bitcoin_context;
    tenure_context.height = won.bitcoin_height;
    let (block, applied) = executor.assemble(candidate, tenure_context, &miner_key)?;
    println!(
        "assembled block {} at height {} with state root {}",
        block.block_id(),
        block.header.chain_length,
        hex::encode(applied.execution.state_root.0)
    );
    if cli.dry_run {
        return Ok(());
    }

    coordinate(&cli, &node, &pox, miner_key, block, &won).await
}

/// Open the checkpoint the miner extends, with the rewards it still owes.
fn checkpoint_executor(
    cli: &Cli,
    password: &str,
    bitcoin_context: nano_chainstate::BitcoinBlockContext,
) -> Result<CheckpointExecutor<BitcoinRpcSource>, Box<dyn Error>> {
    let accounting = match &cli.tenure_accounting {
        Some(path) => Some(TenureAccounting::from_json(&fs::read(path)?)?),
        None => None,
    };
    Ok(CheckpointExecutor::from_checkpoint_with_accounting(
        &cli.checkpoint,
        parse_hex_array(&cli.source_state_id)?,
        TrieHash::from_bytes(parse_hex_array(&cli.state_root)?),
        NakamotoBlock::decode(&fs::read(&cli.anchor_block)?)?,
        bitcoin_context,
        BitcoinRpcSource::new(
            &cli.bitcoin_rpc,
            cli.bitcoin_rpc_user.clone(),
            password.to_owned(),
            *b"T3",
        )?,
        accounting,
    )?)
}

/// Publish the proposal, gather threshold signatures, and submit the block.
async fn coordinate(
    cli: &Cli,
    node: &SyncClient,
    pox: &nano_sync::PoxInfo,
    miner_key: StacksPrivateKey,
    block: NakamotoBlock,
    won: &nano_sync::SortitionInfo,
) -> Result<(), Box<dyn Error>> {
    let reward_cycle = pox.reward_cycle(won.bitcoin_height);
    let reward_set = node.stacker_set(reward_cycle).await?;
    let proposal = BlockProposal {
        block,
        bitcoin_height: won.bitcoin_height,
        reward_cycle,
        data: BlockProposal::empty_data(),
    };
    let coordinator = ProposalCoordinator::new(
        StackerDbClient::new(Url::parse(&cli.peer)?)?,
        parse_contract(&cli.miner_contract)?,
        StackerDbContract {
            address: StacksAddress::from_str(&cli.signer_contract_address)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?,
            // Signer contracts are named by reward-cycle parity and message id.
            name: format!("signers-{}-1", reward_cycle % 2),
        },
        miner_key,
    );
    let published = coordinator.publish_proposal(&proposal).await?;
    println!(
        "published the proposal to miner slot {:?}",
        published.metadata.map(|slot| slot.slot_id)
    );

    let deadline = Instant::now() + Duration::from_secs(cli.signer_timeout_secs);
    loop {
        match coordinator
            .finalize_and_submit(&proposal, &reward_set.signer_set, node)
            .await
        {
            Ok(block) => {
                println!("submitted threshold-signed block {}", block.block_id());
                return Ok(());
            }
            Err(ProposalError::SignerSet(SignerSetError::InsufficientWeight))
                if Instant::now() < deadline =>
            {
                sleep(Duration::from_secs(cli.poll_interval_secs)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Derive the burn total and sortition hash the won tenure must commit to.
async fn bitcoin_tenure_view(
    cli: &Cli,
    node: &SyncClient,
    password: &str,
    won: &nano_sync::SortitionInfo,
    first_bitcoin_height: u64,
) -> Result<BitcoinTenureView, Box<dyn Error>> {
    let mut bitcoin = BitcoinRpcSource::new(
        &cli.bitcoin_rpc,
        cli.bitcoin_rpc_user.clone(),
        password.to_owned(),
        *b"T3",
    )?;
    let tenure = node.tenure_info().await?;
    let parent = node.sortition(tenure.consensus_hash).await?;
    let parent_start = node.block(tenure.tenure_start_block_id).await?;
    let mut sortition_heights = Vec::new();
    for height in parent.bitcoin_height + 1..=won.bitcoin_height {
        if node.sortition_at_height(height).await?.was_sortition {
            sortition_heights.push(height);
        }
    }
    let total_burn = total_burn_after(
        &mut bitcoin,
        parent_start.header.bitcoin_spent,
        &sortition_heights,
    )?;

    let cached = fs::read(&cli.sortition_hash_cache)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<SortitionHashPoint>(&bytes).ok())
        .filter(|point| point.bitcoin_height <= won.bitcoin_height)
        .unwrap_or_else(|| SortitionHashPoint::genesis(first_bitcoin_height));
    let point = extend_sortition_hash(node, &bitcoin, cached, won.bitcoin_height).await?;
    fs::write(&cli.sortition_hash_cache, serde_json::to_vec(&point)?)?;

    Ok(BitcoinTenureView {
        total_burn,
        sortition_hash: point.sortition_hash,
    })
}

fn read_hex_array<const N: usize>(path: &PathBuf) -> Result<[u8; N], io::Error> {
    parse_hex_array(&fs::read_to_string(path)?)
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

fn parse_contract(value: &str) -> Result<StackerDbContract, io::Error> {
    let (address, name) = value.split_once('/').ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "contract must use ADDRESS/name syntax",
        )
    })?;
    Ok(StackerDbContract {
        address: StacksAddress::from_str(address).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid contract address: {error}"),
            )
        })?,
        name: name.to_owned(),
    })
}
