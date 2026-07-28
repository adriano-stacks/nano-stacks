#![forbid(unsafe_code)]

use std::{error::Error, fs, io, path::PathBuf, str::FromStr, time::Duration};

use clap::{Parser, Subcommand};
use nano_address::StacksAddress;
use nano_chainstate::{ChainState, NakamotoBlock};
use nano_crypto::StacksPrivateKey;
use nano_primitives::TrieHash;
use nano_signer::{
    ActiveSortitionValidator, ChainstateProposalValidator, EmbeddedSigner, LiveSigner,
    SignerConfig, SignerService,
};
use nano_stackerdb::{StackerDbClient, StackerDbContract};
use nano_sync::SyncClient;
use reqwest::Url;
use tokio::time::sleep;

#[derive(Parser)]
#[command(name = "stacks-signer")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a checkpoint-backed signer against an HTTP peer.
    Run {
        #[arg(long, default_value = "http://127.0.0.1:20443/")]
        peer: String,
        #[arg(long)]
        miner_contract: String,
        #[arg(long)]
        signer_contract: String,
        #[arg(long)]
        private_key: String,
        #[arg(long)]
        writer_slot: u32,
        #[arg(long)]
        state_file: PathBuf,
        #[arg(long)]
        checkpoint: PathBuf,
        #[arg(long)]
        source_state_id: String,
        #[arg(long)]
        state_root: String,
        #[arg(long)]
        anchor_block: PathBuf,
        #[arg(long)]
        anchor_bitcoin_height: u64,
        #[arg(long, default_value_t = 1)]
        poll_interval_secs: u64,
        /// Maximum canonical blocks to fetch before requiring a nearer checkpoint.
        #[arg(long, default_value_t = 20_000)]
        max_sync_blocks: usize,
        /// Verify checkpoint-to-tip execution without polling or publishing signer messages.
        #[arg(long)]
        sync_only: bool,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let Cli {
        command:
            Command::Run {
                peer,
                miner_contract,
                signer_contract,
                private_key,
                writer_slot,
                state_file,
                checkpoint,
                source_state_id,
                state_root,
                anchor_block,
                anchor_bitcoin_height,
                poll_interval_secs,
                max_sync_blocks,
                sync_only,
            },
    } = Cli::parse();
    let client = SyncClient::new(Url::parse(&peer)?)?;
    let pox = client.pox_info().await?;
    let mut bitcoin_context = pox.bitcoin_context();
    bitcoin_context.height = anchor_bitcoin_height;
    let source = parse_array(&source_state_id)?;
    let root = TrieHash::from_bytes(parse_array(&state_root)?);
    let anchor = NakamotoBlock::decode(&fs::read(anchor_block)?)?;
    let mut chainstate = ChainState::from_checkpoint(checkpoint, source, root)?;
    chainstate.append_nakamoto_block_with_bitcoin_context(
        bitcoin_context,
        Some(source),
        &anchor,
    )?;
    let validator = ChainstateProposalValidator::new(chainstate, &anchor, bitcoin_context);
    let signer = EmbeddedSigner::from_state_file(
        SignerConfig {
            private_key: StacksPrivateKey::from_bytes(parse_array(&private_key)?)?,
            writer_slot,
            next_slot_version: 1,
        },
        ActiveSortitionValidator::new(validator),
        state_file,
    )?;
    let service = SignerService::new(
        StackerDbClient::new(Url::parse(&peer)?)?,
        parse_contract(&miner_contract)?,
        parse_contract(&signer_contract)?,
        signer,
    );
    let mut signer = LiveSigner::new(client.clone(), service);
    if sync_only {
        sync_chainstate(&client, &mut signer, max_sync_blocks).await?;
        return Ok(());
    }
    loop {
        if let Err(error) = sync_chainstate(&client, &mut signer, max_sync_blocks).await {
            eprintln!("signer chainstate sync failed: {error}");
            sleep(Duration::from_secs(poll_interval_secs)).await;
            continue;
        }
        if let Err(error) = signer.poll().await {
            eprintln!("signer poll failed: {error}");
        }
        sleep(Duration::from_secs(poll_interval_secs)).await;
    }
}

async fn sync_chainstate(
    client: &SyncClient,
    signer: &mut LiveSigner<ChainstateProposalValidator>,
    max_blocks: usize,
) -> Result<(), Box<dyn Error>> {
    let tip = client.tenure_info().await?.tip_block_id;
    if signer
        .validator_mut()
        .validator_mut()
        .has_trusted_block(&tip)
    {
        return Ok(());
    }

    let mut blocks = Vec::new();
    let mut block_id = tip;
    for _ in 0..max_blocks {
        let block = client.block(block_id).await.map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("could not decode canonical block {block_id}: {error}"),
            )
        })?;
        if signer
            .validator_mut()
            .validator_mut()
            .has_trusted_block(&block.block_id())
        {
            break;
        }
        block_id = block.header.parent_block_id;
        blocks.push(block);
        if blocks.len() % 1_000 == 0 {
            eprintln!("downloaded {} canonical blocks", blocks.len());
        }
    }
    if blocks.len() == max_blocks {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "checkpoint is farther from the canonical tip than max_sync_blocks",
        )
        .into());
    }

    eprintln!("validating {} canonical blocks", blocks.len());
    for block in blocks.iter().rev() {
        let sortition = client.sortition(block.header.consensus_hash).await?;
        signer
            .validator_mut()
            .validator_mut()
            .observe(block, sortition.bitcoin_height)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    }
    Ok(())
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
