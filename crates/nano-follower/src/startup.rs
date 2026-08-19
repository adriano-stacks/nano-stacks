//! Open an authenticated follower state at its checkpoint or durable tip.

use std::{error::Error, fs, path::Path, time::Duration};

use nano_chainstate::{
    ChainState, ChainStateError, MINER_REWARD_MATURITY, NakamotoBlock, TenureAccounting,
};
use nano_primitives::{Network, StacksBlockId};
use nano_sync::{PoxInfo, TenureSource};
use tokio::time::sleep;

use crate::{
    CheckpointExecutor, adoption::adopt, burnchain::BurnchainSource, checkpoint_history,
    config::Config,
};

const ACCOUNTING_FILE: &str = "accounting.json";
const RESUME_ANCESTORS: usize = 256;
const RESUME_ATTEMPTS: u32 = 30;

/// Open the state the follower will extend and authenticate its local burn view.
pub async fn open_executor(
    config: &Config,
    network: Network,
    pox: &PoxInfo,
    peers: &mut TenureSource,
) -> Result<CheckpointExecutor<BurnchainSource>, Box<dyn Error>> {
    let directory = config.chainstate_dir();
    let (mut chainstate, anchor, fresh) =
        open_chainstate(config, network, peers, &directory).await?;
    let (tracker, bitcoin) =
        checkpoint_history::authenticate(config, pox, &mut chainstate, &anchor, fresh)?;
    println!(
        "deriving sortitions locally from burn {} on PoX history {}",
        tracker.tip().bitcoin_height,
        tracker.tip().pox_id
    );
    let mut executor = if fresh {
        let context = checkpoint_history::anchor_context(
            pox,
            &tracker,
            &mut chainstate,
            &anchor,
            config.checkpoint.anchor_bitcoin_height,
        )?;
        CheckpointExecutor::from_chainstate(chainstate, anchor, context, bitcoin)?
    } else {
        CheckpointExecutor::resume(chainstate, anchor, bitcoin)
    };
    executor.track_sortitions(tracker, config.follower.working_dir.clone());
    Ok(executor)
}

async fn open_chainstate(
    config: &Config,
    network: Network,
    peers: &mut TenureSource,
    directory: &Path,
) -> Result<(ChainState, NakamotoBlock, bool), Box<dyn Error>> {
    let source = config.checkpoint.source_state_id()?;
    adopt(config, directory)?;
    let mut chainstate = ChainState::open_from_checkpoint(
        network,
        directory,
        &config.checkpoint.marf,
        source,
        config.checkpoint.state_root()?,
    )?;

    let Some(tip) = chainstate.tip()?.filter(|tip| *tip != source) else {
        *chainstate.accounting_mut() = accounting(config, directory)?;
        let anchor = NakamotoBlock::decode(&fs::read(&config.checkpoint.anchor_block)?)?;
        return Ok((chainstate, anchor, true));
    };

    let tip = deepest_ledger_tip(&chainstate, tip, config.follower.max_sync_blocks)?;
    let mut ancestors = Vec::new();
    let mut walk = tip;
    while ancestors.len() < RESUME_ANCESTORS {
        let Some(parent) = chainstate.parent_of(walk)? else {
            break;
        };
        ancestors.push(parent);
        walk = parent;
    }
    let tip = resume_from(ancestors, peers, tip, directory).await?;
    println!(
        "resuming {} at block {} of height {}",
        directory.display(),
        tip.block_id(),
        tip.header.chain_length
    );
    if let Ok(height) = u32::try_from(tip.header.chain_length) {
        match chainstate.discard_above(height) {
            Ok(0) => {}
            Ok(discarded) => println!("discarded {discarded} unreferenced states above {height}"),
            Err(error) => eprintln!("cannot discard states above {height}: {error}"),
        }
    }
    recover_ledger(&mut chainstate, *tip.block_id().as_bytes())?;
    Ok((chainstate, tip, false))
}

fn deepest_ledger_tip(
    chainstate: &ChainState,
    tip: [u8; 32],
    reach: usize,
) -> Result<[u8; 32], ChainStateError> {
    let mut walk = tip;
    for walked in 0..reach {
        if chainstate.has_ledger(walk) {
            if walked > 0 {
                println!(
                    "the deepest state {} has no ledger; resuming {walked} blocks back at {}",
                    hex::encode(tip),
                    hex::encode(walk)
                );
            }
            return Ok(walk);
        }
        let Some(parent) = chainstate.parent_of(walk)? else {
            break;
        };
        walk = parent;
    }
    Ok(tip)
}

fn recover_ledger(chainstate: &mut ChainState, tip: [u8; 32]) -> Result<(), Box<dyn Error>> {
    if !chainstate.recover_ledger_at(tip)? {
        return Err(format!(
            "block {} has no committed ledger, so this follower cannot authenticate its restart",
            hex::encode(tip)
        )
        .into());
    }
    check_maturity_window(chainstate.accounting_mut())
}

async fn resume_from(
    ancestors: Vec<[u8; 32]>,
    peers: &mut TenureSource,
    tip: [u8; 32],
    directory: &Path,
) -> Result<NakamotoBlock, Box<dyn Error>> {
    let sealed = StacksBlockId::from_bytes(tip);
    for waited in 0..=RESUME_ATTEMPTS {
        match peers.block(sealed).await {
            Ok(block) => return Ok(block),
            Err(_) if waited < RESUME_ATTEMPTS => {
                peers.forgive_throttles();
                sleep(Duration::from_secs(1)).await;
            }
            Err(_) => break,
        }
    }
    for (walked, ancestor) in ancestors.iter().enumerate() {
        peers.forgive_throttles();
        if let Ok(block) = peers.block(StacksBlockId::from_bytes(*ancestor)).await {
            println!(
                "block {sealed} left the chain; carrying on from {}, {} back",
                block.block_id(),
                walked + 1
            );
            return Ok(block);
        }
    }
    Err(format!(
        "the state in {} is sealed at {sealed}, and no peer has any of its {} retained ancestors",
        directory.display(),
        ancestors.len()
    )
    .into())
}

fn accounting(config: &Config, directory: &Path) -> Result<TenureAccounting, Box<dyn Error>> {
    let persisted = directory.join(ACCOUNTING_FILE);
    if persisted.exists() {
        let accounting = TenureAccounting::from_json(&fs::read(&persisted)?)?;
        if accounting.known_earnings_span().is_none() {
            return Err(format!(
                "the accounting at {} carries no tenure earnings and cannot authenticate a restart",
                persisted.display()
            )
            .into());
        }
        check_maturity_window(&accounting)?;
        return Ok(accounting);
    }
    let accounting = match &config.checkpoint.tenure_accounting {
        Some(path) => TenureAccounting::from_json(&fs::read(path)?)?,
        None => TenureAccounting::default(),
    };
    check_maturity_window(&accounting)?;
    Ok(accounting)
}

fn check_maturity_window(accounting: &TenureAccounting) -> Result<(), Box<dyn Error>> {
    let Some((first, last)) = accounting.known_earnings_span() else {
        return Ok(());
    };
    if first > 1 && last - first < MINER_REWARD_MATURITY {
        return Err(format!(
            "checkpoint earnings cover tenures {first} to {last}, fewer than the {} required",
            MINER_REWARD_MATURITY + 1
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use nano_chainstate::{ChainState, TenureAccounting};
    use nano_primitives::Network;
    use nano_vm::{BlockCommit, BlockHeader};

    use super::{check_maturity_window, deepest_ledger_tip, recover_ledger};

    fn commit(chainstate: &mut ChainState, parent: Option<[u8; 32]>, block: [u8; 32]) {
        chainstate
            .vm_mut()
            .begin_block(parent, block)
            .expect("begin block");
        chainstate
            .vm_mut()
            .commit_block(
                block,
                &BlockCommit {
                    header: BlockHeader::default(),
                    ledger: b"committed ledger".to_vec(),
                    decision: None,
                },
            )
            .expect("commit block");
    }

    #[test]
    fn restart_uses_the_deepest_state_with_a_ledger() {
        let directory = tempfile::tempdir().expect("directory");
        let mut chainstate = ChainState::open(Network::MAINNET, directory.path()).expect("state");
        let mut parent = None;
        for height in 1..=3u8 {
            commit(&mut chainstate, parent, [height; 32]);
            parent = Some([height; 32]);
        }
        for height in 4..=5u8 {
            chainstate
                .vm_mut()
                .begin_block(parent, [height; 32])
                .expect("begin residue");
            chainstate
                .vm_mut()
                .seal_block_to([height; 32])
                .expect("seal residue");
            parent = Some([height; 32]);
        }

        assert_eq!(
            deepest_ledger_tip(&chainstate, [5; 32], 500).expect("ledger tip"),
            [3; 32]
        );
        let height = chainstate
            .height_of([3; 32])
            .expect("ledger height")
            .expect("sealed ledger");
        assert_eq!(
            chainstate.discard_above(height).expect("discard residue"),
            2
        );
    }

    #[test]
    fn a_state_without_a_committed_ledger_cannot_resume() {
        let directory = tempfile::tempdir().expect("directory");
        let mut chainstate = ChainState::open(Network::MAINNET, directory.path()).expect("state");
        chainstate
            .vm_mut()
            .begin_block(None, [1; 32])
            .expect("begin");
        chainstate.vm_mut().seal_block_to([1; 32]).expect("seal");

        let error = recover_ledger(&mut chainstate, [1; 32])
            .expect_err("missing ledger")
            .to_string();
        assert!(error.contains("cannot authenticate its restart"), "{error}");
    }

    #[test]
    fn a_short_reward_window_is_valid_only_at_the_chain_start() {
        let earnings = |first: u64, last: u64| {
            let tenures = (first..=last)
                .map(|height| {
                    format!(
                        r#"{{"coinbase_height":{height},"recipient":"ST000000000000000000002AMW42H","coinbase":1000,"fees":0}}"#
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            TenureAccounting::from_json(
                format!(r#"{{"matured_effects":[],"tenures":[{tenures}]}}"#).as_bytes(),
            )
            .expect("accounting")
        };

        check_maturity_window(&earnings(1, 12)).expect("young chain");
        check_maturity_window(&earnings(50, 200)).expect("full window");
        let error = check_maturity_window(&earnings(50, 60))
            .expect_err("short mature chain window")
            .to_string();
        assert!(error.contains("tenures 50 to 60"), "{error}");
    }
}
