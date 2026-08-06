//! The signing role: validate what the miners propose, and sign what holds.
//!
//! The signer keeps a chain state of its own because it executes candidate
//! blocks that are not on the canonical chain yet, which is exactly what the
//! node's own executor must never do.

use std::{error::Error, path::Path, time::Duration};

use crate::runtime::BurnchainSource;
use nano_bitcoin::BitcoinSource as _;
use nano_chainstate::SignerSet;
use nano_crypto::StacksPrivateKey;
use nano_primitives::Network;
use nano_signer::{
    AccumulatedCoinbase, ActiveSortitionValidator, ChainstateProposalValidator, EmbeddedSigner,
    LiveSigner, SignerConfig as EmbeddedSignerConfig, SignerService, StateAnnouncer,
};
use nano_stackerdb::{StackerDbClient, StackerDbContract};
use nano_sync::{PoxInfo, SyncClient};
use tokio::time::sleep;

use crate::{
    config::{Config, SignerConfig, cycle_contract, miner_contract},
    runtime,
};

/// Contract index carrying block responses.
const RESPONSE_MESSAGE_ID: u32 = 1;
/// Contract index carrying signer state machine updates.
const STATE_MESSAGE_ID: u32 = 2;
/// Contract index carrying the promises signers publish before they sign.
const PRE_COMMIT_MESSAGE_ID: u32 = 3;

/// The state a signer validates proposals against.
pub type Validator = ActiveSortitionValidator<ChainstateProposalValidator<BurnchainSource>>;

/// Where the signer records what it has already signed.
const STATE_FILE: &str = "signer.json";

/// Open the chain state the signer validates from, resuming what is on disk.
pub async fn open(
    config: &Config,
    network: Network,
    pox: &PoxInfo,
    peers: &mut nano_sync::TenureSource,
    directory: &Path,
) -> Result<Validator, Box<dyn Error>> {
    let (mut chainstate, anchor, pending) =
        runtime::open_chainstate(config, network, pox, peers, directory).await?;
    let mut bitcoin = runtime::bitcoin_source(config)?;
    let mut context = runtime::bitcoin_context(config, pox);
    context.height = config.checkpoint.anchor_bitcoin_height;
    if let Some(pending) = pending {
        let operations = bitcoin.block_at(pending.height)?;
        let parent = chainstate.tip();
        chainstate.append_nakamoto_block_with_bitcoin_operations(
            pending,
            &operations.operations,
            parent,
            &anchor,
        )?;
        context = pending;
    }
    Ok(ActiveSortitionValidator::new(
        ChainstateProposalValidator::new(chainstate, &anchor, context, bitcoin),
    ))
}

/// Sign for whichever reward cycle is active, rebinding across rollovers.
pub async fn run(
    config: Config,
    signer: SignerConfig,
    network: Network,
    peer: SyncClient,
    validator: Validator,
) -> runtime::Role {
    start(config, signer, network, peer, validator)
        .await
        .map_err(|error| format!("the signer stopped: {error}"))
}

async fn start(
    config: Config,
    signer: SignerConfig,
    network: Network,
    peer: SyncClient,
    validator: Validator,
) -> Result<(), Box<dyn Error>> {
    let key = signer.private_key()?;
    let embedded = EmbeddedSigner::from_state_file(
        EmbeddedSignerConfig {
            private_key: key.clone(),
            writer_slot: 0,
            next_slot_version: 1,
            conflict_timeout_secs: signer.conflict_timeout_secs,
        },
        validator,
        config.node.working_dir.join(STATE_FILE),
    )?;
    let service = SignerService::new(
        StackerDbClient::new(peer.base_url().clone())?,
        miner_contract(network),
        cycle_contract(network, 0, RESPONSE_MESSAGE_ID),
        cycle_contract(network, 0, PRE_COMMIT_MESSAGE_ID),
        embedded,
    );
    let mut live = LiveSigner::new(peer.clone(), service);
    let mut announcer = StateAnnouncer::new(
        StackerDbClient::new(peer.base_url().clone())?,
        cycle_contract(network, 0, STATE_MESSAGE_ID),
        0,
        key.clone(),
    );

    let interval = Duration::from_secs(config.node.poll_interval_secs);
    let mut bound_cycle = None;
    loop {
        let signers = match binding(&peer, network, &key).await {
            Ok(binding) => {
                if bound_cycle != Some(binding.cycle) {
                    println!(
                        "signing reward cycle {} from slot {}",
                        binding.cycle, binding.slot
                    );
                    live.service_mut()
                        .rebind(binding.responses, binding.pre_commits, binding.slot);
                    announcer.rebind(binding.states, binding.slot);
                    bound_cycle = Some(binding.cycle);
                }
                binding.signers
            }
            Err(error) => {
                eprintln!("signer is not in the active reward set: {error}");
                sleep(interval).await;
                continue;
            }
        };
        if let Err(error) = announcer.announce(&peer, &signers).await {
            eprintln!("signer state announcement failed: {error}");
        }
        if let Err(error) = catch_up(&peer, &mut live, config.node.max_sync_blocks).await {
            eprintln!("signer chainstate sync failed: {error}");
            sleep(interval).await;
            continue;
        }
        if let Err(error) = live.poll().await {
            eprintln!("signer poll failed: {error}");
        }
        sleep(interval).await;
    }
}

/// What the active reward cycle assigns this signer.
struct Binding {
    cycle: u64,
    responses: StackerDbContract,
    pre_commits: StackerDbContract,
    states: StackerDbContract,
    slot: u32,
    signers: SignerSet,
}

async fn binding(
    peer: &SyncClient,
    network: Network,
    key: &StacksPrivateKey,
) -> Result<Binding, String> {
    let cycle = peer
        .tenure_info()
        .await
        .map_err(|error| error.to_string())?
        .reward_cycle;
    let public_key = key.public_key().to_bytes_compressed();
    let signers = peer
        .stacker_set(cycle)
        .await
        .map_err(|error| error.to_string())?
        .signer_set;
    let slot = signers
        .signers()
        .iter()
        .position(|signer| signer.public_key.to_bytes_compressed() == public_key)
        .ok_or_else(|| "this signer holds no slot in the active reward set".to_owned())?;
    Ok(Binding {
        cycle,
        responses: cycle_contract(network, cycle, RESPONSE_MESSAGE_ID),
        pre_commits: cycle_contract(network, cycle, PRE_COMMIT_MESSAGE_ID),
        states: cycle_contract(network, cycle, STATE_MESSAGE_ID),
        slot: u32::try_from(slot).map_err(|error| error.to_string())?,
        signers,
    })
}

/// Execute every canonical block the signer has not seen yet.
///
/// A signer that has not executed the chain the proposal builds on cannot
/// verify its state root, so this runs before every round of signing.
async fn catch_up(
    peer: &SyncClient,
    signer: &mut LiveSigner<ChainstateProposalValidator<BurnchainSource>>,
    max_blocks: usize,
) -> Result<(), String> {
    let tip = peer
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
        let block = peer
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

    for block in blocks.iter().rev() {
        let sortition = peer
            .sortition(block.header.consensus_hash)
            .await
            .map_err(|error| error.to_string())?;
        let schedule = signer.validator_mut().coinbase_schedule();
        if let Some(accumulated) = peer
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
