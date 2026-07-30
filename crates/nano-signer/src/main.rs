use std::{error::Error, fs, io, path::PathBuf, str::FromStr, time::Duration};

use clap::{Parser, Subcommand};
use nano_address::StacksAddress;
use nano_bitcoin::BitcoinRpcSource;
use nano_chainstate::{ChainState, NakamotoBlock, TenureAccounting};
use nano_crypto::StacksPrivateKey;
use nano_primitives::{Network, TrieHash};
use nano_signer::{
    AccumulatedCoinbase, ActiveSortitionValidator, ChainstateProposalValidator, EmbeddedSigner,
    LiveSigner, SignerConfig, SignerService, StateAnnouncer,
};
use nano_stackerdb::{StackerDbClient, StackerDbContract};
use nano_sync::SyncClient;
use reqwest::Url;
use tokio::time::sleep;

/// `StackerDB` contract indices for block responses, signer state updates, and
/// the promises signers publish before they sign.
///
/// A message's contract index is not its payload type byte: a state machine
/// update is payload type 6 but travels on `signers-{parity}-2`
/// (`libsigner/src/v0/messages.rs`, `MessageSlotID` against
/// `SignerMessageTypePrefix`).
const RESPONSE_MESSAGE_ID: u32 = 1;
const STATE_MESSAGE_ID: u32 = 2;
const PRE_COMMIT_MESSAGE_ID: u32 = 3;

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
        bitcoin_rpc: String,
        #[arg(long)]
        bitcoin_rpc_user: String,
        #[arg(long)]
        bitcoin_rpc_password_file: PathBuf,
        #[arg(long)]
        miner_contract: String,
        /// Boot address hosting the per-cycle signer `StackerDB` contracts.
        #[arg(long, default_value = "ST000000000000000000002AMW42H")]
        signer_contract_address: String,
        #[arg(long)]
        private_key: String,
        #[arg(long)]
        state_file: PathBuf,
        #[arg(long)]
        checkpoint: PathBuf,
        /// Optional portable tenure-accounting checkpoint for matured native rewards.
        #[arg(long)]
        tenure_accounting: Option<PathBuf>,
        #[arg(long)]
        source_state_id: String,
        #[arg(long)]
        state_root: String,
        #[arg(long)]
        anchor_block: PathBuf,
        #[arg(long)]
        anchor_bitcoin_height: u64,
        /// Bitcoin height at which PoX-5 activates for this network.
        #[arg(long)]
        pox_5_activation_height: Option<u32>,
        /// Two-byte burnchain magic prefixing every Stacks `OP_RETURN`.
        #[arg(long, default_value = "T3")]
        bitcoin_magic: String,
        #[arg(long, default_value_t = 1)]
        poll_interval_secs: u64,
        /// Maximum canonical blocks to fetch before requiring a nearer checkpoint.
        #[arg(long, default_value_t = 20_000)]
        max_sync_blocks: usize,
        /// Seconds a signed block is protected before its replacement may be signed.
        #[arg(long, default_value_t = nano_signer::DEFAULT_CONFLICT_TIMEOUT_SECS)]
        conflict_timeout_secs: u64,
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
                bitcoin_rpc,
                bitcoin_rpc_user,
                bitcoin_rpc_password_file,
                miner_contract,
                signer_contract_address,
                private_key,
                state_file,
                checkpoint,
                tenure_accounting,
                source_state_id,
                state_root,
                anchor_block,
                anchor_bitcoin_height,
                pox_5_activation_height,
                bitcoin_magic,
                poll_interval_secs,
                max_sync_blocks,
                conflict_timeout_secs,
                sync_only,
            },
    } = Cli::parse();
    let client = SyncClient::new(Url::parse(&peer)?)?;
    let pox = client.pox_info().await?;
    // The chain nano executes is whichever one the peer it follows reports.
    let network = Network::from_chain_id(client.node_info().await?.network_id);
    let magic: [u8; 2] = bitcoin_magic
        .as_bytes()
        .try_into()
        .map_err(|_| "burnchain magic must be exactly two bytes")?;
    let password = fs::read_to_string(bitcoin_rpc_password_file)?;
    let mut bitcoin =
        BitcoinRpcSource::new(&bitcoin_rpc, bitcoin_rpc_user, password.trim_end(), magic)?;
    let mut bitcoin_context = pox.bitcoin_context();
    bitcoin_context.height = anchor_bitcoin_height;
    if let Some(height) = pox_5_activation_height {
        bitcoin_context.pox_5_activation_height = height;
    }
    let source = parse_array(&source_state_id)?;
    let root = TrieHash::from_bytes(parse_array(&state_root)?);
    let anchor = NakamotoBlock::decode(&fs::read(anchor_block)?)?;
    let mut chainstate = ChainState::from_checkpoint(network, checkpoint, source, root)?;
    if let Some(path) = tenure_accounting {
        *chainstate.accounting_mut() = TenureAccounting::from_json(&fs::read(path)?)?;
    }
    let anchor_operations = bitcoin.block_at(anchor_bitcoin_height)?;
    chainstate.append_nakamoto_block_with_bitcoin_operations(
        bitcoin_context,
        &anchor_operations.operations,
        Some(source),
        &anchor,
    )?;
    let validator = ChainstateProposalValidator::new(chainstate, &anchor, bitcoin_context, bitcoin);
    let key = StacksPrivateKey::from_bytes(parse_array(&private_key)?)?;
    let signer = EmbeddedSigner::from_state_file(
        SignerConfig {
            private_key: key.clone(),
            writer_slot: 0,
            next_slot_version: 1,
            conflict_timeout_secs,
        },
        ActiveSortitionValidator::new(validator),
        state_file,
    )?;
    let boot_address = StacksAddress::from_str(&signer_contract_address)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let service = SignerService::new(
        StackerDbClient::new(Url::parse(&peer)?)?,
        parse_contract(&miner_contract)?,
        cycle_contract(boot_address, 0, RESPONSE_MESSAGE_ID),
        cycle_contract(boot_address, 0, PRE_COMMIT_MESSAGE_ID),
        signer,
    );
    let mut signer = LiveSigner::new(client.clone(), service);
    let mut announcer = StateAnnouncer::new(
        StackerDbClient::new(Url::parse(&peer)?)?,
        cycle_contract(boot_address, 0, STATE_MESSAGE_ID),
        0,
        key.clone(),
    );
    if sync_only {
        sync_chainstate(&client, &mut signer, max_sync_blocks).await?;
        return Ok(());
    }
    run(
        &client,
        &mut signer,
        &mut announcer,
        boot_address,
        &key,
        poll_interval_secs,
        max_sync_blocks,
    )
    .await
}

/// Sign for whichever reward cycle is active, rebinding across rollovers.
#[allow(clippy::too_many_arguments)]
async fn run(
    client: &SyncClient,
    signer: &mut LiveSigner<ChainstateProposalValidator<BitcoinRpcSource>>,
    announcer: &mut StateAnnouncer,
    boot_address: StacksAddress,
    key: &StacksPrivateKey,
    poll_interval_secs: u64,
    max_sync_blocks: usize,
) -> Result<(), Box<dyn Error>> {
    let mut bound_cycle = None;
    loop {
        let signers = match reward_cycle_binding(client, boot_address, key).await {
            Ok(binding) => {
                if bound_cycle != Some(binding.cycle) {
                    eprintln!(
                        "signing reward cycle {} from slot {}",
                        binding.cycle, binding.slot
                    );
                    signer.service_mut().rebind(
                        binding.responses,
                        binding.pre_commits,
                        binding.slot,
                    );
                    announcer.rebind(binding.states, binding.slot);
                    bound_cycle = Some(binding.cycle);
                }
                binding.signers
            }
            Err(error) => {
                eprintln!("signer is not in the active reward set: {error}");
                sleep(Duration::from_secs(poll_interval_secs)).await;
                continue;
            }
        };
        if let Err(error) = announcer.announce(client, &signers).await {
            eprintln!("signer state announcement failed: {error}");
        }
        match sync_chainstate(client, signer, max_sync_blocks).await {
            Ok(()) => {}
            Err(error) => {
                eprintln!("signer chainstate sync failed: {error}");
                sleep(Duration::from_secs(poll_interval_secs)).await;
                continue;
            }
        }
        if let Err(error) = signer.poll().await {
            eprintln!("signer poll failed: {error}");
        }
        sleep(Duration::from_secs(poll_interval_secs)).await;
    }
}

/// What the active reward cycle assigns this signer.
struct RewardCycleBinding {
    cycle: u64,
    responses: StackerDbContract,
    pre_commits: StackerDbContract,
    states: StackerDbContract,
    slot: u32,
    signers: nano_chainstate::SignerSet,
}

/// The contracts, slot, and reward set of the active reward cycle.
async fn reward_cycle_binding(
    client: &SyncClient,
    boot_address: StacksAddress,
    key: &StacksPrivateKey,
) -> Result<RewardCycleBinding, String> {
    let cycle = client
        .tenure_info()
        .await
        .map_err(|error| error.to_string())?
        .reward_cycle;
    let public_key = key.public_key().to_bytes_compressed();
    let signers = client
        .stacker_set(cycle)
        .await
        .map_err(|error| error.to_string())?
        .signer_set;
    let slot = signers
        .signers()
        .iter()
        .position(|signer| signer.public_key.to_bytes_compressed() == public_key)
        .ok_or_else(|| "this signer holds no slot in the active reward set".to_owned())?;
    Ok(RewardCycleBinding {
        cycle,
        responses: cycle_contract(boot_address, cycle, RESPONSE_MESSAGE_ID),
        pre_commits: cycle_contract(boot_address, cycle, PRE_COMMIT_MESSAGE_ID),
        states: cycle_contract(boot_address, cycle, STATE_MESSAGE_ID),
        slot: u32::try_from(slot).map_err(|error| error.to_string())?,
        signers,
    })
}

/// Signer contracts are named by reward-cycle parity and message identifier.
fn cycle_contract(address: StacksAddress, cycle: u64, message: u32) -> StackerDbContract {
    StackerDbContract {
        address,
        name: format!("signers-{}-{message}", cycle % 2),
    }
}

async fn sync_chainstate(
    client: &SyncClient,
    signer: &mut LiveSigner<ChainstateProposalValidator<BitcoinRpcSource>>,
    max_blocks: usize,
) -> Result<(), String> {
    let tip = client
        .tenure_info()
        .await
        .map_err(|error| error.to_string())?
        .tip_block_id;
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
        let block = client
            .block(block_id)
            .await
            .map_err(|error| format!("could not decode canonical block {block_id}: {error}"))?;
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
        return Err("checkpoint is farther from the canonical tip than max_sync_blocks".to_owned());
    }

    eprintln!(
        "validating {} canonical blocks from height {:?}",
        blocks.len(),
        blocks.last().map(|block| block.header.chain_length)
    );
    for block in blocks.iter().rev() {
        let sortition = client
            .sortition(block.header.consensus_hash)
            .await
            .map_err(|error| error.to_string())?;
        let schedule = signer.validator_mut().coinbase_schedule();
        if let Some(accumulated) = client
            .accumulated_coinbase(block, schedule, sortition.bitcoin_height)
            .await
            .map_err(|error| error.to_string())?
        {
            signer
                .validator_mut()
                .set_accumulated_coinbase(sortition.bitcoin_height, accumulated);
        }
        signer
            .validator_mut()
            .validator_mut()
            .observe(block, sortition.bitcoin_height)
            .map_err(|error| {
                format!(
                    "canonical block {} at height {} failed to validate: {error}",
                    block.block_id(),
                    block.header.chain_length
                )
            })?;
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
