//! The mining role: commit on Bitcoin, and mine the tenures those commitments win.
//!
//! A tenure has to be proposed within seconds of the sortition that awarded it,
//! because signers track one active miner and reject a block whose tenure is no
//! longer the one they follow. So the miner drives the node's executor rather
//! than one of its own: it builds on its own blocks the moment it makes them.

use std::{
    error::Error,
    fs,
    str::FromStr,
    time::{Duration, Instant},
};

use bitcoin::{Amount, OutPoint, Txid};
use bitcoincore_rpc::Auth;
use nano_address::StacksAddress;
use crate::runtime::BurnchainSource;
use nano_chainstate::{NakamotoBlock, SignerSetError};
use nano_crypto::{StacksPrivateKey, VrfPrivateKey};
use std::sync::Arc;

use nano_mempool::Mempool;
use nano_sync::TenureSource;
use tokio::sync::Mutex;
use nano_miner::{
    BitcoinTenureView, BitcoinWallet, ProposalCoordinator, ProposalError, RegisteredLeaderKey,
    SortitionHashPoint, TenureExtension, TenureTip, build_tenure_continuation_block,
    build_tenure_extend_block, build_tenure_start_block, extend_sortition_hash, plan_commitment,
    total_burn_after,
};
use nano_primitives::{ConsensusHash, Hash160, Network, hash160};
use nano_rpc::{EventDispatcher, EventKind, mined_nakamoto_block_payload};
use nano_stackerdb::{BlockProposal, StackerDbClient};
use nano_sync::{PoxInfo, SortitionInfo, SyncClient};
use tokio::time::sleep;

use crate::{
    CatchUpBudget, CheckpointExecutor,
    config::{Config, MinerConfig, cycle_contract, miner_contract},
    runtime::{self, NODE_CHAINSTATE, SharedExecutor},
    staging::Staging,
};

/// The previous commitment's change output, which the next commitment must
/// spend so the sortition attributes them both to one miner.
const COMMITMENT_CHAIN_FILE: &str = "commit-chain.txt";
/// The sortition-hash chain point, extended and rewritten as it advances.
const SORTITION_HASH_FILE: &str = "sortition-hash.json";
/// Contract index carrying block responses.
const RESPONSE_MESSAGE_ID: u32 = 1;

/// Everything the mining role runs on.
pub struct Runtime {
    pub config: Config,
    pub miner: MinerConfig,
    pub network: Network,
    pub pox: PoxInfo,
    pub peer: SyncClient,
    pub executor: SharedExecutor,
    pub dispatcher: EventDispatcher,
    /// Where a block this node mined is announced to the peer network.
    ///
    /// A miner that only pushes its block to one HTTP peer depends on that peer
    /// to spread it, which is the dependency the p2p work exists to remove — and
    /// nano relays everybody else's blocks, so not relaying its own would be the
    /// one gap in that.
    pub relay: nano_p2p::Relay,
    /// The pool the RPC admits transactions into, so they are the same ones.
    pub mempool: Arc<Mutex<Mempool>>,
}

/// Commit on every Bitcoin block and mine every tenure this miner wins.
pub async fn run(runtime: Runtime) -> runtime::Role {
    start(runtime)
        .await
        .map_err(|error| format!("the miner stopped: {error}"))
}

async fn start(runtime: Runtime) -> Result<(), Box<dyn Error>> {
    let Runtime {
        config,
        miner,
        mempool,
        network,
        pox,
        peer,
        executor,
        dispatcher,
        relay,
    } = runtime;
    let miner_key = miner.block_signing_private_key()?;
    let vrf_key = miner.vrf_private_key()?;
    let miner_hash = hash160(&miner_key.public_key().to_bytes_compressed());
    let miner_address = StacksAddress::single_signature(miner_hash, network.is_mainnet());
    let wallet = BitcoinWallet::connect(
        &format!(
            "{}/wallet/{}",
            config.burnchain.rpc_url.trim_end_matches('/'),
            miner.bitcoin_wallet
        ),
        Auth::UserPass(
            config.burnchain.rpc_user.clone(),
            config.burnchain.rpc_password.clone(),
        ),
    )?;
    let mut state = State {
        config,
        miner,
        network,
        pox,
        peer,
        dispatcher,
        relay,
        miner_key,
        vrf_key,
        miner_hash,
        miner_address,
        leader_key: RegisteredLeaderKey {
            bitcoin_height: 0,
            transaction_index: 0,
        },
        committed_at: 0,
        mined: Vec::new(),
        mempool,
        tenure: None,
    };
    state.leader_key = state.registered_key(&wallet).await?;
    println!("mining as {miner_hash} from the state on disk");

    let interval = Duration::from_secs(state.config.node.poll_interval_secs);
    let staging = Staging::open(
        &state
            .config
            .chainstate_dir(NODE_CHAINSTATE)
            .join("staging.sqlite"),
    )
    .map_err(|error| format!("cannot open the staging store: {error}"))?;
    let budget = CatchUpBudget {
        fetch: runtime::ROUND_FETCH,
        execute: state.config.node.max_sync_blocks,
    };
    // A miner follows the peer it was configured with: it is close to the tip by
    // definition, so bulk history over a pool buys it nothing.
    let mut history = TenureSource::only(state.peer.clone());
    loop {
        let mut executor = executor.lock().await;
        if let Err(error) = executor
            // No schedule, for the same reason it follows one peer: a miner sits at
            // the tip, where the tenure it wants next is the one that peer is
            // currently producing and no inventory names it yet.
            .catch_up(&state.peer, &mut history, &state.pox, &staging, budget, &[])
            .await
        {
            eprintln!("following the peer failed: {error}");
            drop(executor);
            sleep(interval).await;
            continue;
        }
        let bitcoin_height = wallet.block_count()?;
        if bitcoin_height > state.committed_at {
            match state.commit(&wallet).await {
                Ok(()) => state.committed_at = bitcoin_height,
                Err(error) => {
                    eprintln!("committing at Bitcoin height {bitcoin_height} failed: {error}");
                }
            }
        }
        if let Err(error) = state.advance_tenure(&mut executor).await {
            eprintln!("advancing the tenure failed: {error}");
            state.tenure = None;
        }
        drop(executor);
        sleep(interval).await;
    }
}

/// A tenure this miner started and is still building on.
struct TenureState {
    tip: TenureTip,
    /// Blocks mined in this tenure, which its next tenure change reports.
    blocks: u32,
    /// Next nonce the miner key spends, for the transactions only it signs.
    nonce: u64,
    /// When the tenure began or was last extended.
    since: Instant,
    /// Whether the tenure has already been extended at its current age.
    extended: bool,
}

impl TenureState {
    fn started(won: &SortitionInfo, block: &NakamotoBlock, nonce: u64) -> Self {
        Self {
            tip: TenureTip {
                consensus_hash: won.consensus_hash,
                block_id: block.block_id(),
                height: block.header.chain_length,
                bitcoin_spent: block.header.bitcoin_spent,
                timestamp: block.header.timestamp,
            },
            blocks: 1,
            nonce,
            since: Instant::now(),
            extended: false,
        }
    }

    fn advance(&mut self, block: &NakamotoBlock) {
        self.tip.block_id = block.block_id();
        self.tip.height = block.header.chain_length;
        self.tip.timestamp = block.header.timestamp;
        self.blocks = self.blocks.saturating_add(1);
        if nano_chainstate::starts_or_extends_tenure(block) {
            self.nonce = self.nonce.saturating_add(1);
            self.since = Instant::now();
            self.extended = true;
        }
    }
}

/// What this miner is, and what it has answered for so far.
struct State {
    config: Config,
    miner: MinerConfig,
    network: Network,
    pox: PoxInfo,
    peer: SyncClient,
    dispatcher: EventDispatcher,
    /// Where a block this node mined is announced to the peer network.
    relay: nano_p2p::Relay,
    miner_key: StacksPrivateKey,
    vrf_key: VrfPrivateKey,
    miner_hash: Hash160,
    miner_address: StacksAddress,
    leader_key: RegisteredLeaderKey,
    /// Bitcoin height this miner last committed at.
    committed_at: u64,
    /// Tenures already started, so a won sortition is not mined twice.
    mined: Vec<ConsensusHash>,
    /// The transactions this node holds for the blocks it still owes.
    ///
    /// Shared with the RPC: a node whose RPC admits transactions into a pool the
    /// miner cannot see accepts them and never mines them.
    mempool: Arc<Mutex<Mempool>>,
    tenure: Option<TenureState>,
}

impl State {
    /// Locate the leader-key registration once Bitcoin has confirmed it.
    ///
    /// A miner is usually started right after registering its key, so the
    /// registration is still in the mempool and has no position to commit
    /// against yet.
    async fn registered_key(
        &self,
        wallet: &BitcoinWallet,
    ) -> Result<RegisteredLeaderKey, Box<dyn Error>> {
        let txid = Txid::from_str(&self.miner.key_txid)?;
        loop {
            match wallet.confirmed_position(txid) {
                Ok((bitcoin_height, transaction_index)) => {
                    return Ok(RegisteredLeaderKey {
                        bitcoin_height: u32::try_from(bitcoin_height)?,
                        transaction_index: u16::try_from(transaction_index)?,
                    });
                }
                Err(nano_miner::MinerError::Unconfirmed) => {
                    println!("waiting for Bitcoin to confirm the leader key {txid}");
                    sleep(Duration::from_secs(self.config.node.poll_interval_secs)).await;
                }
                // A transaction the wallet has never seen is a configuration
                // naming a burnchain this node is not on — usually a key
                // registered against a chain that has since been replaced.
                // Say which key, so the answer is not a bare RPC code.
                Err(error) => {
                    return Err(format!(
                        "miner.key_txid {txid} is not a transaction this burnchain wallet knows \
                         ({error}); register a leader key against the chain this node follows"
                    )
                    .into());
                }
            }
        }
    }

    /// Commit to the next tenure, chained to the previous commitment's change.
    async fn commit(&self, wallet: &BitcoinWallet) -> Result<(), Box<dyn Error>> {
        let mut bitcoin = runtime::bitcoin_source(&self.config)?;
        let plan = plan_commitment(
            &self.peer,
            &mut bitcoin,
            self.leader_key,
            wallet.block_count()?,
        )
        .await?;
        let chain_file = self.config.node.working_dir.join(COMMITMENT_CHAIN_FILE);
        let previous_change = fs::read_to_string(&chain_file)
            .ok()
            .and_then(|value| parse_outpoint(value.trim()));
        let submitted = wallet.submit_leader_commitment(
            self.config.burnchain.magic()?,
            plan.commitment,
            &plan.sbtc_address,
            Amount::from_sat(self.miner.commitment_sats),
            self.miner.fee_rate_sats_per_vbyte,
            previous_change,
        )?;
        fs::write(
            &chain_file,
            format!("{}:{}", submitted.transaction_id, submitted.change_output),
        )?;
        println!(
            "committed to tenure {} at Bitcoin height {} paying {} sats",
            hex::encode(plan.commitment.block_header_hash),
            plan.target_bitcoin_height,
            self.miner.commitment_sats
        );
        Ok(())
    }

    /// Start the tenure nano has won, carry on the one it owns, or do neither.
    async fn advance_tenure(
        &mut self,
        executor: &mut CheckpointExecutor<BurnchainSource>,
    ) -> Result<(), Box<dyn Error>> {
        let Some(won) = self.won_tenure().await? else {
            // A tenure is not one block: while nano still owns the current one,
            // it keeps confirming what the mempool holds, and says on chain
            // when the tenure outlives the budget it started with.
            if self.tenure.is_some()
                && let Some(block) = self.continue_tenure(executor).await?
            {
                println!(
                    "the network accepted nano's block {} at height {}",
                    block.block_id(),
                    block.header.chain_length
                );
                if let Some(tenure) = self.tenure.as_mut() {
                    tenure.advance(&block);
                }
                executor.accept_own_block(block);
            }
            return Ok(());
        };

        self.mined.push(won.consensus_hash);
        let nonce = self.peer.account_nonce(self.miner_address).await?;
        // A tenure already under way is one to carry on with, not to start
        // again: its first block is on the chain, and proposing another would
        // ask the signers to replace one they have signed.
        if self.peer.tenure_info().await?.consensus_hash == won.consensus_hash {
            let resumed = self.resume_tenure(&won, nonce).await?;
            println!(
                "carrying on tenure {} from height {}",
                resumed.tip.consensus_hash, resumed.tip.height
            );
            self.tenure = Some(resumed);
            return Ok(());
        }
        let block = self.mine(executor, &won).await?;
        println!("the network accepted nano's block {}", block.block_id());
        self.tenure = Some(TenureState::started(&won, &block, nonce));
        executor.accept_own_block(block);
        Ok(())
    }

    /// Adopt a tenure this miner started but is no longer tracking, which is
    /// what a restart in the middle of one leaves behind.
    async fn resume_tenure(
        &self,
        won: &SortitionInfo,
        nonce: u64,
    ) -> Result<TenureState, Box<dyn Error>> {
        let info = self.peer.tenure_info().await?;
        let tip = self.peer.block(info.tip_block_id).await?;
        let start = self.peer.block(info.tenure_start_block_id).await?;
        Ok(TenureState {
            tip: TenureTip {
                consensus_hash: won.consensus_hash,
                block_id: info.tip_block_id,
                height: info.tip_height,
                bitcoin_spent: tip.header.bitcoin_spent,
                timestamp: tip.header.timestamp,
            },
            blocks: u32::try_from(
                info.tip_height
                    .saturating_sub(start.header.chain_length)
                    .saturating_add(1),
            )?,
            nonce,
            since: Instant::now(),
            extended: false,
        })
    }

    /// The tenure this miner has won and not yet mined, if there is one.
    ///
    /// A Bitcoin block without a sortition does not end the previous tenure, so
    /// the tenure to mine is the last sortition that chose a miner.
    async fn won_tenure(&self) -> Result<Option<SortitionInfo>, Box<dyn Error>> {
        let tip = self.peer.sortition_tip().await?;
        let current = if tip.was_sortition {
            Some(tip)
        } else {
            match tip.last_sortition_consensus_hash {
                Some(consensus_hash) => Some(self.peer.sortition(consensus_hash).await?),
                None => None,
            }
        };
        Ok(current.filter(|sortition| {
            sortition.was_sortition
                && sortition.miner_public_key_hash == Some(self.miner_hash)
                && !self.mined.contains(&sortition.consensus_hash)
        }))
    }

    /// Mine the next block of a tenure nano still owns, if there is anything to
    /// say.
    ///
    /// Nothing is proposed when the peer has moved past nano's tenure or its
    /// tip, when the mempool is empty, and when no extension is due: a block
    /// with no transactions and no tenure change would only ask the signers to
    /// sign the state they already agreed to.
    async fn continue_tenure(
        &self,
        executor: &mut CheckpointExecutor<BurnchainSource>,
    ) -> Result<Option<NakamotoBlock>, Box<dyn Error>> {
        let state = self.tenure.as_ref().expect("a tenure to continue");
        let tenure = self.peer.tenure_info().await?;
        if tenure.consensus_hash != state.tip.consensus_hash
            || tenure.tip_block_id != state.tip.block_id
        {
            return Ok(None);
        }
        let now = now_unix();
        // Held only while the pool is touched: the peer calls below await, and
        // the RPC admits into the same pool meanwhile.
        {
            let mut mempool = self.mempool.lock().await;
            self.peer.fill_mempool(&mut mempool, now).await?;
        }
        let accounts = {
            let mempool = self.mempool.lock().await;
            self.peer.accounts_for(&mempool).await?
        };
        let pending = {
            let mut mempool = self.mempool.lock().await;
            mempool.advance(&accounts, now);
            mempool.candidates(&accounts)
        };
        let extend_due = !state.extended
            && state.since.elapsed() >= Duration::from_secs(self.miner.tenure_extend_after_secs);
        if pending.is_empty() && !extend_due {
            return Ok(None);
        }

        let sortition = self.peer.sortition(state.tip.consensus_hash).await?;
        let mut context = runtime::bitcoin_context(&self.config, &self.pox);
        context.height = sortition.bitcoin_height;
        let burn_view = self.peer.sortition_tip().await?;
        let candidate = if extend_due {
            println!(
                "extending tenure {} after {:?} into burn view {}",
                state.tip.consensus_hash,
                state.since.elapsed(),
                burn_view.consensus_hash
            );
            build_tenure_extend_block(
                &state.tip,
                TenureExtension {
                    burn_view_consensus_hash: burn_view.consensus_hash,
                    blocks_in_tenure: state.blocks,
                    nonce: state.nonce,
                    now,
                },
                self.network,
                &self.miner_key,
                Vec::new(),
            )?
        } else {
            build_tenure_continuation_block(&state.tip, Vec::new(), now)
        };

        let (block, applied) =
            executor.assemble_selecting(candidate, context, &pending, &self.miner_key)?;
        if block.transactions.is_empty() {
            return Ok(None);
        }
        println!(
            "assembled block {} at height {} carrying {} transactions with state root {}",
            block.block_id(),
            block.header.chain_length,
            block.transactions.len(),
            hex::encode(applied.execution.state_root.0)
        );
        self.dispatcher.dispatch(
            EventKind::MinedNakamotoBlock,
            &mined_nakamoto_block_payload(&block, &applied, sortition.bitcoin_height),
        );
        let block = self.submit(block, sortition.bitcoin_height).await?;
        // A confirmed transaction leaves now rather than when the peer's
        // account nonces catch up, so the next block does not offer it again.
        for transaction in &block.transactions {
            self.mempool.lock().await.remove(transaction.txid());
        }
        Ok(Some(block))
    }

    /// Assemble the tenure's first block, gather threshold signatures, submit it.
    async fn mine(
        &self,
        executor: &mut CheckpointExecutor<BurnchainSource>,
        won: &SortitionInfo,
    ) -> Result<NakamotoBlock, Box<dyn Error>> {
        println!(
            "won the sortition at Bitcoin height {} with consensus hash {}",
            won.bitcoin_height, won.consensus_hash
        );
        let view = self.tenure_view(won).await?;
        let candidate = build_tenure_start_block(
            &self.peer,
            won,
            view,
            self.network,
            &self.miner_key,
            &self.vrf_key,
            won.bitcoin_timestamp,
        )
        .await?;

        let mut context = runtime::bitcoin_context(&self.config, &self.pox);
        context.height = won.bitcoin_height;
        let context = self
            .peer
            .tenure_coinbase_context(
                &candidate,
                executor.chainstate_mut().accounting_mut().schedule(),
                context,
            )
            .await?;
        let (block, applied) = executor.assemble(candidate, context, &self.miner_key)?;
        println!(
            "assembled block {} at height {} with state root {}",
            block.block_id(),
            block.header.chain_length,
            hex::encode(applied.execution.state_root.0)
        );
        self.dispatcher.dispatch(
            EventKind::MinedNakamotoBlock,
            &mined_nakamoto_block_payload(&block, &applied, won.bitcoin_height),
        );
        self.submit(block, won.bitcoin_height).await
    }

    /// Publish a block to the signers and submit it once they have signed it.
    async fn submit(
        &self,
        block: NakamotoBlock,
        bitcoin_height: u64,
    ) -> Result<NakamotoBlock, Box<dyn Error>> {
        let reward_cycle = self.pox.reward_cycle(bitcoin_height);
        let reward_set = self.peer.stacker_set(reward_cycle).await?;
        let proposal = BlockProposal {
            block,
            bitcoin_height,
            reward_cycle,
            data: BlockProposal::empty_data(),
        };
        let coordinator = ProposalCoordinator::new(
            StackerDbClient::new(self.peer.base_url().clone())?,
            miner_contract(self.network),
            cycle_contract(self.network, reward_cycle, RESPONSE_MESSAGE_ID),
            self.miner_key.clone(),
        );
        coordinator.publish_proposal(&proposal).await?;
        println!("published the proposal to the miner slots this key owns");

        let deadline = Instant::now() + Duration::from_secs(self.miner.signer_timeout_secs);
        loop {
            match coordinator
                .finalize_and_submit(&proposal, &reward_set.signer_set, &self.peer)
                .await
            {
                Ok(block) => {
                    // Announced, not offered: `announce` is the outbound queue,
                    // and a block this node mined needs no authenticating against
                    // itself — it was assembled on this node's own tip and its
                    // state root was sealed here.
                    self.relay
                        .announce(nano_p2p::Offer::block(None, block.clone()));
                    return Ok(block);
                }
                Err(ProposalError::SignerSet(SignerSetError::InsufficientWeight))
                    if Instant::now() < deadline =>
                {
                    sleep(Duration::from_secs(self.config.node.poll_interval_secs)).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// The burn total and sortition hash the won tenure must commit to.
    async fn tenure_view(&self, won: &SortitionInfo) -> Result<BitcoinTenureView, Box<dyn Error>> {
        let mut bitcoin = runtime::bitcoin_source(&self.config)?;
        let tenure = self.peer.tenure_info().await?;
        let parent = self.peer.sortition(tenure.consensus_hash).await?;
        let parent_start = self.peer.block(tenure.tenure_start_block_id).await?;
        let mut sortition_heights = Vec::new();
        for height in parent.bitcoin_height + 1..=won.bitcoin_height {
            if self.peer.sortition_at_height(height).await?.was_sortition {
                sortition_heights.push(height);
            }
        }
        let total_burn = total_burn_after(
            &mut bitcoin,
            parent_start.header.bitcoin_spent,
            &sortition_heights,
        )?;

        let cache = self.config.node.working_dir.join(SORTITION_HASH_FILE);
        let cached = fs::read(&cache)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<SortitionHashPoint>(&bytes).ok())
            .filter(|point| point.bitcoin_height <= won.bitcoin_height)
            .unwrap_or_else(|| SortitionHashPoint::genesis(self.pox.first_bitcoin_height));
        let point = extend_sortition_hash(&self.peer, &bitcoin, cached, won.bitcoin_height).await?;
        fs::write(&cache, serde_json::to_vec(&point)?)?;
        Ok(BitcoinTenureView {
            total_burn,
            sortition_hash: point.sortition_hash,
        })
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn parse_outpoint(value: &str) -> Option<OutPoint> {
    let (transaction_id, index) = value.split_once(':')?;
    Some(OutPoint {
        txid: Txid::from_str(transaction_id).ok()?,
        vout: index.parse().ok()?,
    })
}
