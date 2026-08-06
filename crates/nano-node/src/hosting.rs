//! Hosting somebody else's signer: the two things a stock `stacks-signer`
//! needs from the node it is pointed at and cannot get anywhere else.
//!
//! A signer has no chain state and no peers. It reads the miner's proposal from
//! its node's `.miners` replica, asks its node whether the block executes, and
//! writes its answer back into its node's own replica. Everything after that is
//! the node's problem: nothing else carries the chunk to the miner counting it.
//!
//! So this module is two loops.
//!
//! **Validating proposals.** nano vouches for a state root by executing the
//! block, and a candidate off the tip must not be executed into the state the
//! node serves. The validator here keeps a chain state of its own — the same one
//! nano's embedded signer uses — and the route reaches it through a channel
//! rather than a lock, because deciding needs the peer as well as the state and
//! an HTTP handler is the wrong place to wait for either.
//!
//! **Replicating chunks.** A node that only serves its own replica hosts a
//! signer that can see no proposals and whose answers reach nobody. Chunks are
//! pulled from the peer and pushed back to it over the same `/v2/stackerdb`
//! routes the signer uses, and every pulled chunk is verified against the writer
//! this node assigned the slot — replication, not trust.

use std::time::Duration;

use nano_primitives::Network;
use nano_rpc::{ProposalRejectCode, ProposalRequest, RpcState};
use nano_signer::{AccumulatedCoinbase as _, ProposalValidator as _};
use nano_stackerdb::{BlockProposal, Chunk, StackerDbClient, StackerDbContract};
use nano_sync::{PoxInfo, SyncClient};
use tokio::{sync::mpsc::UnboundedReceiver, time::sleep};

use crate::{
    config::{Config, cycle_contract, miner_contract},
    runtime::Role,
    signer::{self, Validator},
};

/// Answer for proposals until the node stops.
///
/// Every proposal is answered, including the ones this node cannot judge: a
/// signer waiting on a verdict that never comes treats the wait as a rejection
/// after a timeout, which is the same outcome reached slower and with less said.
pub async fn validate_proposals(
    config: Config,
    pox: PoxInfo,
    peer: SyncClient,
    mut validator: Validator,
    mut requests: UnboundedReceiver<ProposalRequest>,
) -> Role {
    let interval = Duration::from_secs(config.node.poll_interval_secs);
    println!("validating block proposals for the signers this node hosts");
    loop {
        let request = tokio::select! {
            request = requests.recv() => request,
            // A validator that only catches up when asked would execute the whole
            // canonical chain inside the first proposal it is given.
            () = sleep(interval) => {
                if let Err(error) =
                    signer::catch_up(&peer, &mut validator, config.node.max_sync_blocks).await
                {
                    eprintln!("the proposal validator could not follow the chain: {error}");
                }
                continue;
            }
        };
        let Some(request) = request else {
            return Err("the proposal route closed".to_owned());
        };
        let verdict = judge(&config, &pox, &peer, &mut validator, &request).await;
        // Nobody is left to tell if the request was abandoned, which is not an
        // error: the block was still executed and the state was still checked.
        drop(request.verdict.send(verdict));
    }
}

/// Execute one proposal and say what happened to it.
async fn judge(
    config: &Config,
    pox: &PoxInfo,
    peer: &SyncClient,
    validator: &mut Validator,
    request: &ProposalRequest,
) -> Result<(), (String, ProposalRejectCode)> {
    let block = &request.block;
    signer::catch_up(peer, validator, config.node.max_sync_blocks)
        .await
        .map_err(|error| {
            (
                format!("this node could not follow the chain the proposal builds on: {error}"),
                ProposalRejectCode::ChainstateError,
            )
        })?;
    let sortition = peer
        .sortition(block.header.consensus_hash)
        .await
        .map_err(|error| {
            (
                format!(
                    "this node has no sortition for the tenure {} the proposal names: {error}",
                    block.header.consensus_hash
                ),
                ProposalRejectCode::NoSuchTenure,
            )
        })?;
    let cycle = pox.reward_cycle(sortition.bitcoin_height);
    // A tenure's coinbase depends on the burn blocks since the last sortition, so
    // a proposal validated without it would seal a root that differs from the
    // network's, and the validator refuses to guess.
    let schedule = validator.coinbase_schedule();
    match peer
        .accumulated_coinbase(block, schedule, sortition.bitcoin_height)
        .await
    {
        Ok(Some(accumulated)) => {
            validator.set_accumulated_coinbase(sortition.bitcoin_height, accumulated);
        }
        Ok(None) => {}
        Err(error) => {
            return Err((
                format!("this node could not read the tenure's accumulated coinbase: {error}"),
                ProposalRejectCode::ChainstateError,
            ));
        }
    }
    let bitcoin_height = sortition.bitcoin_height;
    validator.set_context(sortition, cycle);
    validator
        .validate(&BlockProposal {
            block: block.clone(),
            bitcoin_height,
            reward_cycle: cycle,
            data: BlockProposal::empty_data(),
        })
        .map_err(classify)
}

/// Say whether a refusal is about the block or about this node.
///
/// The validator answers in prose, and the distinction matters to a signer: a
/// block that does not execute is `InvalidBlock` and must never be signed, while
/// a node that was missing a parent or a burn block is saying nothing about the
/// block at all. Reporting the second as the first would have this node telling a
/// signer that a perfectly good block is invalid.
fn classify(error: String) -> (String, ProposalRejectCode) {
    let code = if error.contains("trusted chain view") {
        ProposalRejectCode::UnknownParent
    } else if error.contains("accumulated coinbase") || error.contains("Bitcoin operations") {
        ProposalRejectCode::ChainstateError
    } else {
        ProposalRejectCode::InvalidBlock
    };
    (error, code)
}

/// Every contract a reward cycle's signers and its miners exchange chunks on.
fn replicated(network: Network, cycle: u64) -> Vec<StackerDbContract> {
    let mut contracts = vec![miner_contract(network)];
    contracts.extend(
        crate::runtime::SIGNER_MESSAGE_IDS
            .into_iter()
            .map(|message| cycle_contract(network, cycle, message)),
    );
    contracts
}

/// Keep this node's replicas and its peer's in step, both ways.
pub async fn replicate(
    config: Config,
    network: Network,
    peer: SyncClient,
    state: RpcState,
    mut written: UnboundedReceiver<(String, Chunk)>,
) -> Role {
    let client = StackerDbClient::new(peer.base_url().clone())
        .map_err(|error| format!("the StackerDB peer is unusable: {error}"))?;
    let interval = Duration::from_secs(config.node.poll_interval_secs);
    println!("replicating StackerDB chunks with {}", peer.base_url());
    let mut complained = false;
    loop {
        let cycle = match peer.tenure_info().await {
            Ok(tenure) => tenure.reward_cycle,
            Err(error) => {
                if !complained {
                    complained = true;
                    eprintln!("StackerDB replication cannot read the active cycle: {error}");
                }
                sleep(interval).await;
                continue;
            }
        };
        complained = false;
        let contracts = replicated(network, cycle);
        // Outbound first: a chunk a hosted signer wrote is what the network is
        // waiting for, and it must not wait behind a round of pulling.
        while let Ok((contract_id, chunk)) = written.try_recv() {
            push(&client, &contracts, &contract_id, &chunk).await;
        }
        for contract in &contracts {
            pull(&client, &state, contract).await;
        }
        sleep(interval).await;
    }
}

/// Hand a chunk this node took to the peer that has to see it.
async fn push(
    client: &StackerDbClient,
    contracts: &[StackerDbContract],
    contract_id: &str,
    chunk: &Chunk,
) {
    let Some(contract) = contracts
        .iter()
        .find(|contract| identifier(contract) == contract_id)
    else {
        return;
    };
    match client.put_chunk(contract, chunk).await {
        // A refusal is the peer's own answer and usually means it already has the
        // chunk, which is replication working rather than failing.
        Ok(acknowledgement) if !acknowledgement.accepted => {
            eprintln!(
                "the peer refused the chunk this node took for {contract_id} slot {}: {}",
                chunk.slot_id,
                acknowledgement.reason.unwrap_or_default()
            );
        }
        Ok(_) => {}
        Err(error) => eprintln!("passing on a {contract_id} chunk failed: {error}"),
    }
}

/// Take whatever the peer holds that this node does not.
async fn pull(client: &StackerDbClient, state: &RpcState, contract: &StackerDbContract) {
    let contract_id = identifier(contract);
    // An unconfigured contract has no writers, so nothing could be checked
    // against anything: there is nothing to replicate into.
    let Some(held) = state.stackerdb().read().await.metadata(&contract_id) else {
        return;
    };
    let Ok(remote) = client.slot_metadata(contract).await else {
        return;
    };
    for metadata in remote {
        let slot = usize::try_from(metadata.slot_id).unwrap_or(usize::MAX);
        let newer = held
            .get(slot)
            .is_some_and(|held| held.slot_version < metadata.slot_version);
        if !newer {
            continue;
        }
        let Ok(Some(data)) = client
            .chunk_at(contract, metadata.slot_id, metadata.slot_version)
            .await
        else {
            continue;
        };
        let chunk = Chunk {
            slot_id: metadata.slot_id,
            slot_version: metadata.slot_version,
            signature: metadata.signature,
            data,
        };
        // Verified against the writer this node assigned the slot, inside `put`:
        // a peer serving a forged chunk gets it refused here.
        let taken = state.stackerdb().write().await.put(&contract_id, chunk);
        if let Err(refusal) = taken {
            eprintln!(
                "the chunk the peer holds for {contract_id} slot {} is not one this node \
                 will take: {}",
                metadata.slot_id,
                refusal.reason()
            );
        }
    }
}

/// The `address.name` a `/v2/stackerdb` route is keyed by.
#[must_use]
pub fn identifier(contract: &StackerDbContract) -> String {
    format!("{}.{}", contract.address, contract.name)
}
