#![forbid(unsafe_code)]

use std::{error::Error, fs, io, path::PathBuf, str::FromStr, time::Duration};

use clap::Parser;
use nano_address::StacksAddress;
use nano_bitcoin::BitcoinRpcSource;
use nano_chainstate::{
    BitcoinBlockContext, ChainState, NakamotoBlock, SignerSetError, TenureAccounting,
};
use nano_crypto::StacksPrivateKey;
use nano_miner::{MinerSlots, ProposalCoordinator, ProposalError};
use nano_primitives::TrieHash;
use nano_stackerdb::{BlockProposal, StackerDbClient, StackerDbContract};
use nano_sync::{PoxInfo, SyncClient};
use reqwest::Url;
use tokio::time::{Instant, sleep};

#[derive(Parser)]
#[command(name = "stacks-miner")]
/// Assemble, propose, and submit one candidate block from a trusted checkpoint.
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
    /// Signer `StackerDB` contract as ADDRESS/name.
    #[arg(long)]
    signer_contract: String,
    /// Hex-encoded 32-byte private key for the registered miner.
    #[arg(long)]
    private_key: String,
    /// The registered miner slot used for proposals.
    #[arg(long)]
    proposal_slot: u32,
    /// The registered miner slot used for finalized-block notifications.
    #[arg(long)]
    pushed_block_slot: u32,
    /// `SQLite` MARF checkpoint path.
    #[arg(long)]
    checkpoint: PathBuf,
    /// Optional portable tenure-accounting checkpoint for matured native rewards.
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
    /// Consensus-encoded candidate block with its transactions selected already.
    #[arg(long)]
    candidate_block: PathBuf,
    /// Bitcoin height for an offline dry run when the peer no longer retains the sortition.
    #[arg(long, requires = "dry_run")]
    candidate_bitcoin_height: Option<u64>,
    /// Validate and assemble locally without publishing anything.
    #[arg(long)]
    dry_run: bool,
    /// Seconds to wait for the threshold signer response set.
    #[arg(long, default_value_t = 120)]
    signer_timeout_secs: u64,
    /// Seconds between signer response checks.
    #[arg(long, default_value_t = 1)]
    poll_interval_secs: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let client = SyncClient::new(Url::parse(&cli.peer)?)?;
    let pox = client.pox_info().await?;
    let mut bitcoin = bitcoin_source(&cli)?;

    let source = parse_array(&cli.source_state_id)?;
    let root = TrieHash::from_bytes(parse_array(&cli.state_root)?);
    let anchor = NakamotoBlock::decode(&fs::read(&cli.anchor_block)?)?;
    let candidate = NakamotoBlock::decode(&fs::read(&cli.candidate_block)?)?;
    let bitcoin_height = if let Some(height) = cli.candidate_bitcoin_height {
        height
    } else {
        let sortition = client.sortition(candidate.header.consensus_hash).await?;
        if !sortition.was_sortition {
            return Err(
                "candidate consensus hash does not identify a winning Bitcoin sortition".into(),
            );
        }
        sortition.bitcoin_height
    };

    let anchor_context = execution_context(&cli, &pox, cli.anchor_bitcoin_height);
    let candidate_context = execution_context(&cli, &pox, bitcoin_height);
    let mut chainstate = ChainState::from_checkpoint(&cli.checkpoint, source, root)?;
    if let Some(path) = &cli.tenure_accounting {
        *chainstate.accounting_mut() = TenureAccounting::from_json(&fs::read(path)?)?;
    }
    let anchor_operations = bitcoin.block_at(cli.anchor_bitcoin_height)?;
    chainstate.append_nakamoto_block_with_bitcoin_operations(
        anchor_context,
        &anchor_operations.operations,
        Some(source),
        &anchor,
    )?;
    let miner_key = StacksPrivateKey::from_bytes(parse_array(&cli.private_key)?)?;
    let candidate_operations = bitcoin.block_at(bitcoin_height)?;
    let (block, applied) = chainstate.assemble_nakamoto_block_with_bitcoin_operations(
        candidate_context,
        &candidate_operations.operations,
        Some(*anchor.block_id().as_bytes()),
        candidate,
        &miner_key,
    )?;
    println!(
        "assembled block {} with state root {} at Bitcoin height {}",
        block.block_id(),
        hex::encode(applied.execution.state_root.0),
        bitcoin_height
    );
    if cli.dry_run {
        return Ok(());
    }

    let tenure = client.tenure_info().await?;
    let reward_set = client.stacker_set(tenure.reward_cycle).await?;
    let proposal = BlockProposal {
        block,
        bitcoin_height,
        reward_cycle: tenure.reward_cycle,
        data: BlockProposal::empty_data(),
    };
    let coordinator = ProposalCoordinator::new(
        StackerDbClient::new(Url::parse(&cli.peer)?)?,
        parse_contract(&cli.miner_contract)?,
        parse_contract(&cli.signer_contract)?,
        miner_key,
        MinerSlots {
            proposal: cli.proposal_slot,
            pushed_block: cli.pushed_block_slot,
        },
    );
    coordinator.publish_proposal(&proposal).await?;
    let deadline = Instant::now() + Duration::from_secs(cli.signer_timeout_secs);
    loop {
        match coordinator
            .finalize_and_submit(&proposal, &reward_set.signer_set, &client)
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

fn bitcoin_source(cli: &Cli) -> Result<BitcoinRpcSource, Box<dyn Error>> {
    let password = fs::read_to_string(&cli.bitcoin_rpc_password_file)?;
    Ok(BitcoinRpcSource::new(
        &cli.bitcoin_rpc,
        &cli.bitcoin_rpc_user,
        password.trim_end(),
        *b"T3",
    )?)
}

fn execution_context(cli: &Cli, pox: &PoxInfo, height: u64) -> BitcoinBlockContext {
    let mut context = pox.bitcoin_context();
    context.height = height;
    context.v1_unlock_height = cli.pox_v1_unlock_height;
    context.v2_unlock_height = cli.pox_v2_unlock_height;
    context.v3_unlock_height = cli.pox_v3_unlock_height;
    if let Some(activation_height) = cli.pox_5_activation_height {
        context.pox_5_activation_height = activation_height;
    }
    context
}

fn parse_contract(value: &str) -> Result<StackerDbContract, io::Error> {
    let (address, name) = value.split_once('/').ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "contract must use ADDRESS/name syntax",
        )
    })?;
    if name.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "contract name cannot be empty",
        ));
    }
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

fn parse_array<const N: usize>(value: &str) -> Result<[u8; N], io::Error> {
    let bytes = hex::decode(value.trim_start_matches("0x")).map_err(|error| {
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

#[cfg(test)]
mod tests {
    use super::{parse_array, parse_contract};

    #[test]
    fn parses_stackerdb_contracts() {
        let contract =
            parse_contract("ST000000000000000000002AMW42H/miners").expect("valid contract");
        assert_eq!(contract.name, "miners");
    }

    #[test]
    fn parses_fixed_size_hex_values() {
        assert_eq!(parse_array::<2>("0x1234").expect("two bytes"), [0x12, 0x34]);
        assert!(parse_array::<2>("123").is_err());
    }
}
