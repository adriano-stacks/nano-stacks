//! Authenticate the bounded block suffix joining a checkpoint to execution.

use std::{collections::HashMap, error::Error, fs};

use nano_bitcoin::BitcoinSource as _;
use nano_chainstate::{
    BitcoinBlockContext, CHECKPOINT_HISTORY_LIMIT, ChainState, CheckpointBoundaryProof,
    CheckpointHistoryBlock, NakamotoBlock,
};
use nano_sync::PoxInfo;
use serde::Deserialize;

use crate::{
    LocalSortition,
    burnchain::{BurnchainSource, bitcoin_source},
    config::Config,
    payout_schedule,
    sortition::SortitionTracker,
};

#[derive(Clone, Copy, Debug)]
struct LoadedBoundaryProof {
    parent_tenure_consensus_hash: nano_primitives::ConsensusHash,
    coinbase_vrf_proof: [u8; 80],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundaryProofRecord {
    parent_tenure_consensus_hash: String,
    coinbase_vrf_proof: String,
}

fn fixed_hex<const N: usize>(field: &str, value: &str) -> Result<[u8; N], Box<dyn Error>> {
    let bytes = hex::decode(value).map_err(|_| format!("{field} is not hexadecimal"))?;
    <[u8; N]>::try_from(bytes.as_slice()).map_err(|_| format!("{field} is not {N} bytes").into())
}

fn load(config: &Config) -> Result<(LoadedBoundaryProof, Vec<NakamotoBlock>), Box<dyn Error>> {
    let root = config.checkpoint.authentication_history.as_ref().ok_or(
        "this fresh follower has no authenticated checkpoint block suffix: set checkpoint.authentication_history to a directory containing boundary.json and blocks/*.bin",
    )?;
    let boundary_path = root.join("boundary.json");
    let record: BoundaryProofRecord = serde_json::from_slice(&fs::read(&boundary_path)?)
        .map_err(|error| format!("{}: {error}", boundary_path.display()))?;
    let boundary = LoadedBoundaryProof {
        parent_tenure_consensus_hash: nano_primitives::ConsensusHash::from_bytes(fixed_hex(
            "authentication boundary parent_tenure_consensus_hash",
            &record.parent_tenure_consensus_hash,
        )?),
        coinbase_vrf_proof: fixed_hex(
            "authentication boundary coinbase_vrf_proof",
            &record.coinbase_vrf_proof,
        )?,
    };

    let blocks_directory = root.join("blocks");
    let mut paths = fs::read_dir(&blocks_directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    paths.retain(|path| path.extension().is_some_and(|extension| extension == "bin"));
    paths.sort();
    if paths.is_empty() {
        return Err(format!(
            "checkpoint authentication history {} contains no block files",
            blocks_directory.display()
        )
        .into());
    }
    if paths.len() > CHECKPOINT_HISTORY_LIMIT {
        return Err(format!(
            "checkpoint authentication history has {} blocks, above the bounded limit of {CHECKPOINT_HISTORY_LIMIT}",
            paths.len()
        )
        .into());
    }
    let mut by_id = HashMap::with_capacity(paths.len());
    for path in paths {
        let block = NakamotoBlock::decode(&fs::read(&path)?)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let id = *block.block_id().as_bytes();
        if by_id.insert(id, block).is_some() {
            return Err(format!(
                "checkpoint authentication history contains block {} more than once",
                hex::encode(id)
            )
            .into());
        }
    }
    let source = config.checkpoint.source_state_id()?;
    let mut cursor = source;
    let mut reversed = Vec::with_capacity(by_id.len());
    while let Some(block) = by_id.remove(&cursor) {
        cursor = *block.header.parent_block_id.as_bytes();
        reversed.push(block);
    }
    if reversed.is_empty() {
        return Err(format!(
            "checkpoint authentication history contains no source block {}",
            hex::encode(source)
        )
        .into());
    }
    if !by_id.is_empty() {
        return Err(format!(
            "checkpoint authentication history has {} block(s) disconnected from source {}",
            by_id.len(),
            hex::encode(source)
        )
        .into());
    }
    reversed.reverse();
    Ok((boundary, reversed))
}

fn contextualize<S: nano_bitcoin::BitcoinSource>(
    pox: &PoxInfo,
    tracker: &SortitionTracker,
    bitcoin: &mut S,
    boundary: CheckpointBoundaryProof,
    blocks: &[NakamotoBlock],
) -> Result<(CheckpointBoundaryProof, Vec<CheckpointHistoryBlock>), Box<dyn Error>>
where
    S::Error: std::fmt::Display,
{
    let mut current_view = None;
    let mut history = Vec::with_capacity(blocks.len());
    for block in blocks {
        if let Some(view) = block.bitcoin_view_consensus_hash() {
            current_view = Some(view);
        }
        let view = current_view.unwrap_or(block.header.consensus_hash);
        let view_height = tracker.height_of_consensus_hash(view).ok_or_else(|| {
            format!(
                "checkpoint history block {} names burn view {view}, which the local sortition chain does not hold",
                block.header.chain_length
            )
        })?;
        let snapshot = tracker.snapshot_at(view_height).ok_or_else(|| {
            format!(
                "checkpoint history block {} needs local sortition snapshot at burn {view_height}, which was not retained",
                block.header.chain_length
            )
        })?;
        if snapshot.total_burn != block.header.bitcoin_spent {
            return Err(format!(
                "checkpoint history block {} says {} burn has been spent, local sortition at burn {view_height} derives {}",
                block.header.chain_length, block.header.bitcoin_spent, snapshot.total_burn
            )
            .into());
        }
        let tenure_height = tracker
            .height_of_consensus_hash(block.header.consensus_hash)
            .ok_or_else(|| {
                format!(
                    "checkpoint history block {} names tenure {}, which the local sortition chain does not hold",
                    block.header.chain_length, block.header.consensus_hash
                )
            })?;
        let mut context = pox.bitcoin_context();
        LocalSortition::from_snapshot(snapshot).record(&mut context);
        if tenure_height != view_height {
            context.move_to_burn_block(tenure_height);
            context.extend_view_to(view_height);
        }
        let operations = bitcoin
            .block_at(tenure_height)
            .map_err(|error| format!("Bitcoin block {tenure_height}: {error}"))?
            .operations;
        history.push(CheckpointHistoryBlock {
            block: block.clone(),
            bitcoin_context: context,
            operations,
        });
    }
    Ok((boundary, history))
}

pub fn anchor_context(
    pox: &PoxInfo,
    tracker: &SortitionTracker,
    chainstate: &mut ChainState,
    anchor: &NakamotoBlock,
    view_height: u64,
) -> Result<BitcoinBlockContext, Box<dyn Error>> {
    if let Some(view) = anchor.bitcoin_view_consensus_hash()
        && tracker.height_of_consensus_hash(view) != Some(view_height)
    {
        return Err(format!(
            "anchor names burn view {view}, which the local sortition chain does not place at configured burn {view_height}"
        )
        .into());
    }
    let snapshot = tracker.snapshot_at(view_height).ok_or_else(|| {
        format!("the local sortition chain retained no snapshot for anchor burn {view_height}")
    })?;
    if snapshot.total_burn != anchor.header.bitcoin_spent {
        return Err(format!(
            "anchor says {} burn has been spent, local sortition at burn {view_height} derives {}",
            anchor.header.bitcoin_spent, snapshot.total_burn
        )
        .into());
    }
    let tenure_height = tracker
        .height_of_consensus_hash(anchor.header.consensus_hash)
        .ok_or_else(|| "the anchor's tenure is absent from the local sortition chain".to_owned())?;
    let mut context = pox.bitcoin_context();
    LocalSortition::from_snapshot(snapshot).record(&mut context);
    if tenure_height != view_height {
        context.move_to_burn_block(tenure_height);
        context.extend_view_to(view_height);
    }
    if nano_chainstate::starts_new_tenure(anchor)
        && let Some(schedule) = chainstate.accounting_mut().schedule()
    {
        let previous = tracker.previous_sortition_height(view_height).ok_or_else(|| {
            format!(
                "the local sortition chain cannot derive accumulated coinbase for anchor burn {view_height}"
            )
        })?;
        context.accumulated_coinbase = schedule.accumulated_at(view_height, Some(previous));
    }
    Ok(context)
}

fn tracker(
    config: &Config,
    chainstate: &ChainState,
    anchor: &NakamotoBlock,
    context: Option<BitcoinBlockContext>,
    fresh_boundary: Option<nano_primitives::ConsensusHash>,
) -> Result<(SortitionTracker, BurnchainSource), Box<dyn Error>> {
    let capture = config.checkpoint.sortition.as_ref().ok_or(
        "this follower has no checkpoint sortition history: set checkpoint.sortition to the exported snapshots, consensus hashes and leader keys",
    )?;
    let executed_burn_view = context.map_or_else(
        || {
            chainstate
                .recorded_header(*anchor.block_id().as_bytes())
                .map_or(0, |header| u64::from(header.burn_block_height))
        },
        |context| context.height,
    );
    let mut tracker = if let Some(boundary) = fresh_boundary {
        SortitionTracker::from_capture_at_consensus(capture, boundary)?
    } else {
        SortitionTracker::resume_or_capture_below(
            &config.follower.working_dir,
            capture,
            executed_burn_view,
        )
        .map_err(|error| {
            format!(
                "this follower cannot derive sortitions of its own and will not execute under a peer-selected burn view: {error}"
            )
        })?
    };
    if tracker.leader_keys() == 0 {
        return Err(format!(
            "checkpoint sortition history {} carries no leader-key registry",
            capture.display()
        )
        .into());
    }
    let mut bitcoin = bitcoin_source(config)?;
    tracker.recover_seed(|height| bitcoin.block_at(height))?;
    Ok((tracker, bitcoin))
}

fn derive<S: nano_bitcoin::BitcoinSource>(
    pox: &PoxInfo,
    tracker: &mut SortitionTracker,
    bitcoin: &mut S,
    boundary: LoadedBoundaryProof,
    history: &[NakamotoBlock],
    target: u64,
) -> Result<(CheckpointBoundaryProof, Vec<CheckpointHistoryBlock>), Box<dyn Error>>
where
    S::Error: std::fmt::Display,
{
    advance_to_boundary(
        pox,
        tracker,
        bitcoin,
        boundary.parent_tenure_consensus_hash,
        target,
    )?;
    let snapshot = tracker.tip();
    if snapshot.consensus_hash != boundary.parent_tenure_consensus_hash {
        return Err(format!(
            "fresh checkpoint sortition seed {} does not equal authentication boundary {}",
            snapshot.consensus_hash, boundary.parent_tenure_consensus_hash
        )
        .into());
    }
    let boundary_height = snapshot.bitcoin_height;
    if target < boundary_height {
        return Err(format!(
            "checkpoint anchor burn {target} is below authentication boundary burn {boundary_height}"
        )
        .into());
    }
    let boundary_block = bitcoin
        .block_at(boundary_height)
        .map_err(|error| format!("Bitcoin block {boundary_height}: {error}"))?;
    let boundary = CheckpointBoundaryProof {
        parent_tenure_consensus_hash: boundary.parent_tenure_consensus_hash,
        coinbase_vrf_proof: boundary.coinbase_vrf_proof,
        sortition_hash: *snapshot.sortition_hash.as_bytes(),
        winner_vrf_public_key: tracker.authenticate_boundary_winner(&boundary_block)?,
    };
    let payouts = payout_schedule(pox)
        .ok_or("checkpoint history cannot be checked without a PoX payout calendar")?;
    tracker.keep_from(boundary_height);
    tracker.catch_up(
        |height| bitcoin.block_at(height),
        target,
        payouts,
        target - boundary_height,
    )?;
    if tracker.tip().bitcoin_height != target {
        return Err(format!(
            "local sortition derivation stopped at burn {}, before checkpoint anchor burn {target}",
            tracker.tip().bitcoin_height
        )
        .into());
    }
    contextualize(pox, tracker, bitcoin, boundary, history)
}

fn advance_to_boundary<S: nano_bitcoin::BitcoinSource>(
    pox: &PoxInfo,
    tracker: &mut SortitionTracker,
    bitcoin: &mut S,
    boundary: nano_primitives::ConsensusHash,
    target: u64,
) -> Result<(), Box<dyn Error>>
where
    S::Error: std::fmt::Display,
{
    let payouts = payout_schedule(pox)
        .ok_or("checkpoint history cannot be checked without a PoX payout calendar")?;
    while tracker.tip().consensus_hash != boundary {
        let height = tracker.tip().bitcoin_height;
        if height >= target {
            return Err(format!(
                "local sortition derivation reached anchor burn {target} without authentication boundary {boundary}"
            )
            .into());
        }
        let next = height
            .checked_add(1)
            .ok_or("checkpoint authentication boundary burn height overflow")?;
        tracker.catch_up(|height| bitcoin.block_at(height), next, payouts, 1)?;
        if tracker.tip().bitcoin_height != next {
            return Err(format!(
                "local sortition derivation stopped at burn {}, before authentication boundary {boundary}",
                tracker.tip().bitcoin_height
            )
            .into());
        }
    }
    Ok(())
}

pub fn authenticate(
    config: &Config,
    pox: &PoxInfo,
    chainstate: &mut ChainState,
    anchor: &NakamotoBlock,
    fresh: bool,
) -> Result<(SortitionTracker, BurnchainSource), Box<dyn Error>> {
    let history = fresh.then(|| load(config)).transpose()?;
    let boundary = history
        .as_ref()
        .map(|(boundary, _)| boundary.parent_tenure_consensus_hash);
    let initial_context = fresh.then(|| {
        let mut context = pox.bitcoin_context();
        context.move_to_burn_block(config.checkpoint.anchor_bitcoin_height);
        context
    });
    let (mut tracker, mut bitcoin) =
        tracker(config, chainstate, anchor, initial_context, boundary)?;
    if let Some((boundary, blocks)) = history {
        let (boundary, history) = derive(
            pox,
            &mut tracker,
            &mut bitcoin,
            boundary,
            &blocks,
            config.checkpoint.anchor_bitcoin_height,
        )?;
        let tenure_starts = history
            .iter()
            .filter(|entry| nano_chainstate::starts_new_tenure(&entry.block))
            .count();
        chainstate.authenticate_checkpoint_history(
            config.checkpoint.source_state_id()?,
            config.checkpoint.state_root()?,
            boundary,
            &history,
        )?;
        println!(
            "authenticated checkpoint history: {} blocks, {} continuity checks, {tenure_starts} tenure starts and one boundary proof",
            history.len(),
            history.len().saturating_sub(1),
        );
    }
    Ok((tracker, bitcoin))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::load;
    use crate::config::Config;

    fn config(root: &std::path::Path) -> Config {
        Config::parse(&format!(
            r#"
[follower]
working_dir = "{}"
network = "testnet"
peers = ["http://127.0.0.1:20443/"]

[burnchain]
rpc_url = "http://127.0.0.1:18443"

[checkpoint]
marf = "{}"
source_state_id = "{}"
state_root = "{}"
anchor_block = "{}"
anchor_bitcoin_height = 10
authentication_history = "{}"
"#,
            root.display(),
            root.join("marf.sqlite").display(),
            "00".repeat(32),
            "00".repeat(32),
            root.join("anchor.bin").display(),
            root.join("history").display(),
        ))
        .expect("config")
    }

    #[test]
    fn a_boundary_proof_is_complete_and_fixed_width() {
        let root = tempfile::tempdir().expect("root");
        let history = root.path().join("history");
        fs::create_dir_all(history.join("blocks")).expect("history");
        let config = config(root.path());
        for (record, expected) in [
            (
                serde_json::json!({
                    "parent_tenure_consensus_hash": "00".repeat(20),
                }),
                "missing field `coinbase_vrf_proof`",
            ),
            (
                serde_json::json!({
                    "parent_tenure_consensus_hash": "00".repeat(20),
                    "coinbase_vrf_proof": "00",
                }),
                "coinbase_vrf_proof is not 80 bytes",
            ),
        ] {
            fs::write(
                history.join("boundary.json"),
                serde_json::to_vec(&record).expect("record"),
            )
            .expect("boundary");
            let error = load(&config).expect_err("invalid boundary").to_string();
            assert!(error.contains(expected), "{error}");
        }
    }
}
