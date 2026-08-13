//! The signing role: validate what the miners propose, and sign what holds.
//!
//! The signer keeps a chain state of its own because it executes candidate
//! blocks that are not on the canonical chain yet, which is exactly what the
//! node's own executor must never do.

use std::{error::Error, path::Path, time::Duration};

use crate::runtime::BurnchainSource;
use nano_chainstate::{SignerSet, SignerWeights};
use nano_crypto::StacksPrivateKey;
use nano_p2p::Discovered;
use nano_primitives::Network;
use nano_signer::{
    ActiveSortitionValidator, ChainstateProposalValidator, EmbeddedSigner, LiveSigner,
    SignerConfig as EmbeddedSignerConfig, SignerService, StateAnnouncer,
};
use nano_stackerdb::StackerDbContract;
use nano_sync::{PoxInfo, TenureSource};
use tokio::time::sleep;

use crate::{
    config::{Config, SignerConfig, cycle_contract, miner_contract},
    hosting::{LocalBurnView, Replicas},
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
type Live = LiveSigner<ChainstateProposalValidator<BurnchainSource>>;

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
    let sortition_state = crate::hosting::validator_sortition_state(config);
    let executor =
        runtime::open_validator_executor(config, network, pox, peers, directory, &sortition_state)
            .await?;
    let (chainstate, anchor, bitcoin_height, bitcoin) = executor.into_validator_parts();
    let mut context = pox.bitcoin_context();
    context.move_to_burn_block(bitcoin_height);
    Ok(ActiveSortitionValidator::new(
        ChainstateProposalValidator::new(chainstate, &anchor, context, bitcoin)
            .using_waterfall_registry(config.node.pox_5_sbtc_registry_contract.clone()),
    ))
}

/// Sign for whichever reward cycle is active, rebinding across rollovers.
pub async fn run(
    config: Config,
    signer: SignerConfig,
    network: Network,
    pox: PoxInfo,
    discovered: Option<Discovered>,
    peers: TenureSource,
    validator: Validator,
) -> runtime::Role {
    start(config, signer, network, pox, discovered, peers, validator)
        .await
        .map_err(|error| format!("the signer stopped: {error}"))
}

async fn start(
    config: Config,
    signer: SignerConfig,
    network: Network,
    pox: PoxInfo,
    discovered: Option<Discovered>,
    mut peers: TenureSource,
    mut validator: Validator,
) -> Result<(), Box<dyn Error>> {
    let mut burn = LocalBurnView::open(&config, validator.validator_mut().bitcoin_context())?;
    // The pool the chain is followed over, not one client picked out of it: a
    // signer bound to the peer it started with makes that peer's availability its
    // own, and on mainnet that peer was the hosted API this node is meant not to
    // need.
    let mut replicas = Replicas::from_endpoints(&peers.endpoints());
    let (peer, replica) = replicas
        .current_pair()
        .ok_or_else(|| Box::<dyn Error>::from("no peer for the signer to read proposals from"))?;
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
        replica.clone(),
        miner_contract(network),
        cycle_contract(network, 0, RESPONSE_MESSAGE_ID),
        cycle_contract(network, 0, PRE_COMMIT_MESSAGE_ID),
        embedded,
    );
    let mut live = LiveSigner::new(peer, service);
    let mut announcer = StateAnnouncer::new(
        replica,
        cycle_contract(network, 0, STATE_MESSAGE_ID),
        0,
        key.clone(),
    );

    let interval = Duration::from_secs(config.node.poll_interval_secs);
    let mut bound_cycle = None;
    let mut serving = replicas.serving().map(ToOwned::to_owned);
    loop {
        // A set-aside is a round's fact, and a signer's round is a poll: forgiving
        // them here is what stops a peer that failed once from being written off for
        // the life of the node.
        peers.forgive_throttles();
        replicas.refresh(&runtime::follow_endpoints(&config, discovered.as_ref()));
        // Retargeted only when the turn has actually moved on, so an ordinary round
        // keeps the connections it had.
        if let Some((peer, replica)) = replicas.retargeted(&mut serving) {
            println!(
                "the signer reads proposals and writes chunks through {}",
                peer.base_url()
            );
            live.use_peer(peer);
            live.service_mut().use_client(replica.clone());
            announcer.use_client(replica);
        }
        let binding = match local_binding(
            &mut peers,
            &mut burn,
            &pox,
            live.validator_mut(),
            config.node.max_sync_blocks,
            network,
            &key,
        )
        .await
        {
            Ok(binding) => binding,
            Err(error) => {
                eprintln!("signer has no authenticated reward-cycle binding: {error}");
                sleep(interval).await;
                continue;
            }
        };
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
        if let Err(error) = announcer.announce(live.peer(), &binding.signers).await {
            eprintln!("signer state announcement failed: {error}");
            replicas.rotate();
        }
        if let Err(error) = poll(&mut live, &mut burn, &pox, binding.cycle, &binding.signers).await
        {
            eprintln!("signer poll failed: {error}");
            replicas.rotate();
        } else {
            replicas.credit();
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

async fn local_binding(
    peers: &mut TenureSource,
    burn: &mut LocalBurnView,
    pox: &PoxInfo,
    validator: &mut Validator,
    max_blocks: usize,
    network: Network,
    key: &StacksPrivateKey,
) -> Result<Binding, String> {
    catch_up(peers, burn, pox, validator, max_blocks).await?;
    let bitcoin_height = burn.bitcoin_tip_height()?;
    let cycle = pox.reward_cycle(bitcoin_height);
    let expected = validator
        .validator_mut()
        .recorded_signer_weights_at(bitcoin_height)?;
    binding(peers, network, cycle, &expected, key).await
}

async fn poll(
    live: &mut Live,
    burn: &mut LocalBurnView,
    pox: &PoxInfo,
    cycle: u64,
    signers: &SignerSet,
) -> Result<(), String> {
    let Some(pending) = live
        .next_proposal(cycle)
        .await
        .map_err(|error| error.to_string())?
    else {
        return Ok(());
    };
    burn.prepare_proposal(&pending.proposal, pox, live.validator_mut())
        .map_err(|error| {
            format!(
                "proposal {} at Stacks height {} records burn {} and tenure {}: {error}",
                pending.proposal.block.block_id(),
                pending.proposal.block.header.chain_length,
                pending.proposal.bitcoin_height,
                pending.proposal.block.header.consensus_hash,
            )
        })?;
    live.answer(pending, signers)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

async fn binding(
    peers: &mut TenureSource,
    network: Network,
    cycle: u64,
    expected: &SignerWeights,
    key: &StacksPrivateKey,
) -> Result<Binding, String> {
    let public_key = key.public_key().to_bytes_compressed();
    let signers = peers
        .stacker_set(cycle)
        .await
        .map_err(|error| error.to_string())?
        .signer_set;
    let supplied = signers
        .signing_weights()
        .map_err(|error| error.to_string())?;
    if supplied != *expected {
        return Err(format!(
            "a peer supplied signer weights for cycle {cycle} that differ from local chainstate"
        ));
    }
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
pub(crate) async fn catch_up(
    peers: &mut nano_sync::TenureSource,
    burn: &mut LocalBurnView,
    pox: &PoxInfo,
    validator: &mut Validator,
    max_blocks: usize,
) -> Result<(), String> {
    let tip = peers
        .tenure_info()
        .await
        .map_err(|error| error.to_string())?
        .tip_block_id;
    if validator.validator_mut().has_trusted_block(&tip) {
        return Ok(());
    }

    let mut blocks = Vec::new();
    let mut block_id = tip;
    for _ in 0..max_blocks {
        let block = peers
            .block(block_id)
            .await
            .map_err(|error| format!("could not decode canonical block {block_id}: {error}"))?;
        if validator
            .validator_mut()
            .adopt_sealed_block(&block)
            .map_err(|error| format!("could not read the validator's sealed state: {error}"))?
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
        burn.observe(block, pox, validator)?;
    }
    Ok(())
}
