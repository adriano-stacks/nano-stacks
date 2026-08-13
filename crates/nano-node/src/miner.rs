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

use crate::runtime::BurnchainSource;
use bitcoin::{Amount, OutPoint, Txid};
use bitcoincore_rpc::Auth;
use clarity::vm::types::{PrincipalData, StandardPrincipalData};
use nano_address::StacksAddress;
use nano_chainstate::{BitcoinBlockContext, NakamotoBlock, SignerSetError};
use nano_crypto::{StacksPrivateKey, VrfPrivateKey};
use std::sync::Arc;

use nano_mempool::Mempool;
use nano_miner::{
    BitcoinWallet, CommitmentParent, ParentTenure, ProposalCoordinator, ProposalError,
    RegisteredLeaderKey, TenureExtension, TenureTip, build_tenure_continuation_block,
    build_tenure_extend_block, build_tenure_start_block, plan_local_commitment,
};
use nano_primitives::{ConsensusHash, Hash160, Network, hash160};
use nano_rpc::{EventDispatcher, EventKind, mined_nakamoto_block_payload};
use nano_stackerdb::{BlockProposal, StackerDbClient};
use nano_sync::TenureSource;
use nano_sync::{PoxInfo, SortitionInfo, SyncClient};
use tokio::sync::Mutex;
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
    pub metrics: nano_rpc::NodeMetrics,
    pub rpc: nano_rpc::RpcState,
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
        metrics,
        rpc,
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
        mined: MinedTenures::default(),
        mempool,
        metrics,
        rpc,
        tenure: None,
    };
    let funded = state.funded_wallet(&wallet)?;
    state.leader_key = state.registered_key(&wallet).await?;
    println!(
        "mining as {miner_hash} from the state on disk, wallet {} holding {funded} sats",
        state.miner.bitcoin_wallet
    );

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
        publish_burnchain(&state, &mut executor).await;
        let bitcoin_height = wallet.block_count()?;
        if bitcoin_height > state.committed_at {
            match state.commit(&wallet, &mut executor) {
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

async fn publish_burnchain(state: &State, executor: &mut CheckpointExecutor<BurnchainSource>) {
    let (advanced, notifications) = executor.follow_burnchain_deferred(&state.pox);
    if advanced == 0 {
        return;
    }
    state
        .rpc
        .publish_local_sortitions(executor.derived_sortitions())
        .await;
    executor.announce_burn_blocks(&notifications);
}

/// A tenure this miner started and is still building on.
struct TenureState {
    tip: TenureTip,
    /// Burn view inherited by continuation blocks until an extension moves it.
    burn_view: ConsensusHash,
    /// Blocks mined in this tenure, which its next tenure change reports.
    blocks: u32,
    /// Next nonce the miner key spends, for the transactions only it signs.
    nonce: u64,
    /// When the tenure began or was last extended.
    since: Instant,
    /// Whether the tenure has already been extended at its current age.
    extended: bool,
}

#[derive(Default)]
struct MinedTenures(Vec<ConsensusHash>);

impl MinedTenures {
    fn is_pending(&self, sortition: &SortitionInfo, miner: Hash160) -> bool {
        sortition.was_sortition
            && sortition.miner_public_key_hash == Some(miner)
            && !self.0.contains(&sortition.consensus_hash)
    }

    fn accepted(&mut self, consensus_hash: ConsensusHash) {
        if !self.0.contains(&consensus_hash) {
            self.0.push(consensus_hash);
        }
    }
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
            burn_view: won.consensus_hash,
            blocks: 1,
            // The tenure change and coinbase spend consecutive miner nonces.
            nonce: nonce.saturating_add(2),
            since: Instant::now(),
            extended: false,
        }
    }

    fn advance(&mut self, block: &NakamotoBlock) {
        self.tip.block_id = block.block_id();
        self.tip.height = block.header.chain_length;
        self.tip.timestamp = block.header.timestamp;
        self.blocks = self.blocks.saturating_add(1);
        if let Some(view) = block.bitcoin_view_consensus_hash() {
            self.burn_view = view;
        }
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
    mined: MinedTenures,
    /// The transactions this node holds for the blocks it still owes.
    ///
    /// Shared with the RPC: a node whose RPC admits transactions into a pool the
    /// miner cannot see accepts them and never mines them.
    mempool: Arc<Mutex<Mempool>>,
    metrics: nano_rpc::NodeMetrics,
    rpc: nano_rpc::RpcState,
    tenure: Option<TenureState>,
}

impl State {
    fn miner_principal(&self) -> PrincipalData {
        PrincipalData::Standard(
            StandardPrincipalData::new(
                self.miner_address.version(),
                *self.miner_address.hash160().as_bytes(),
            )
            .expect("a miner address has a valid standard-principal version"),
        )
    }

    /// Refuse a wallet that cannot pay for a commitment, before the loop.
    ///
    /// Both halves of a miner's Bitcoin identity are checked at start-up rather than
    /// at the first tenure it wins — this one and the leader-key registration below
    /// — because a miner that discovers either missing mid-tenure has already held a
    /// signer slot for the cycle, and on a network whose threshold needs every signer
    /// the operator's mistake is indistinguishable from a consensus fault.
    ///
    /// The usual cause is a miner address funded but never imported watch-only, which
    /// is the standing trap in hacknet's own setup notes: the wallet exists, answers
    /// every call, and holds nothing.
    fn funded_wallet(&self, wallet: &BitcoinWallet) -> Result<u64, Box<dyn Error>> {
        wallet.spendable_sats().map_err(|error| {
            format!(
                "the miner's Bitcoin wallet {} holds no spendable output ({error}); fund the \
                 miner address and import it watch-only before mining",
                self.miner.bitcoin_wallet
            )
            .into()
        })
    }

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
    fn commit(
        &self,
        wallet: &BitcoinWallet,
        executor: &mut CheckpointExecutor<BurnchainSource>,
    ) -> Result<(), Box<dyn Error>> {
        let mut bitcoin = runtime::bitcoin_source(&self.config)?;
        let bitcoin_tip_height = wallet.block_count()?;
        let local_tip = executor.local_burn_tip()?;
        if local_tip.bitcoin_height != bitcoin_tip_height {
            return Err(format!(
                "the local sortition chain is at burn {} while Bitcoin is at {bitcoin_tip_height}",
                local_tip.bitcoin_height
            )
            .into());
        }
        let parent_consensus_hash = executor.tip().header.consensus_hash;
        let latest_winner = executor.latest_local_winner()?;
        current_commitment_parent(parent_consensus_hash, latest_winner.as_ref())?;
        let parent_sortition = executor.local_sortition_info(parent_consensus_hash)?;
        let (tenure_start_block_id, _) = executor
            .chainstate_mut()
            .tenure_start(parent_consensus_hash)
            .ok_or_else(|| {
                format!(
                    "the locally executed tenure {parent_consensus_hash} has no authenticated start block"
                )
            })?;
        let tenure_vrf_proof = executor
            .chainstate_mut()
            .parent_tenure_proof()
            .ok_or("the locally executed tenure has no authenticated coinbase VRF proof")?;
        let target = bitcoin_tip_height
            .checked_add(1)
            .ok_or("Bitcoin height overflow")?;
        let reward_cycle = self.pox.reward_cycle(target);
        let sbtc_address = executor
            .chainstate_mut()
            .sbtc_payout_address(self.config.node.pox_5_sbtc_registry_contract.as_deref())?;
        let plan = plan_local_commitment(
            &mut bitcoin,
            self.leader_key,
            CommitmentParent {
                bitcoin_tip_height,
                tenure_start_block_id,
                sortition: parent_sortition,
                tenure_vrf_proof,
                sbtc_address,
                reward_cycle,
            },
        )?;
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
        let Some(won) = self.won_tenure(executor)? else {
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
            }
            return Ok(());
        };

        let parent = executor.local_parent_tenure(&self.miner_principal())?;
        // A tenure already under way is one to carry on with, not to start
        // again: its first block is on the chain, and proposing another would
        // ask the signers to replace one they have signed.
        if executor.tip().header.consensus_hash == won.consensus_hash {
            let burn_view = executor.local_executed_burn_view()?.consensus_hash;
            let resumed = Self::resume_tenure(&won, parent, burn_view)?;
            println!(
                "carrying on tenure {} from height {}",
                resumed.tip.consensus_hash, resumed.tip.height
            );
            self.tenure = Some(resumed);
            self.mined.accepted(won.consensus_hash);
            return Ok(());
        }
        let block = self.mine(executor, &won).await?;
        println!("the network accepted nano's block {}", block.block_id());
        self.tenure = Some(TenureState::started(&won, &block, parent.miner_nonce));
        // Only a proposal the network accepted is remembered. A transport or
        // signer timeout must be retried on the next round, not mistaken for a
        // tenure this process already mined.
        self.mined.accepted(won.consensus_hash);
        Ok(())
    }

    /// Adopt a tenure this miner started but is no longer tracking, which is
    /// what a restart in the middle of one leaves behind.
    fn resume_tenure(
        won: &SortitionInfo,
        parent: ParentTenure,
        burn_view: ConsensusHash,
    ) -> Result<TenureState, Box<dyn Error>> {
        if parent.tip.consensus_hash != won.consensus_hash {
            return Err(format!(
                "local tip tenure {} does not match won tenure {}",
                parent.tip.consensus_hash, won.consensus_hash
            )
            .into());
        }
        Ok(TenureState {
            tip: parent.tip,
            burn_view,
            blocks: parent.blocks,
            nonce: parent.miner_nonce,
            since: Instant::now(),
            extended: false,
        })
    }

    /// The tenure this miner has won and not yet mined, if there is one.
    ///
    /// A Bitcoin block without a sortition does not end the previous tenure, so
    /// the tenure to mine is the last sortition that chose a miner.
    fn won_tenure(
        &self,
        executor: &CheckpointExecutor<BurnchainSource>,
    ) -> Result<Option<SortitionInfo>, Box<dyn Error>> {
        Ok(executor
            .latest_local_winner()?
            .filter(|sortition| self.mined.is_pending(sortition, self.miner_hash)))
    }

    /// Mine the next block of a tenure nano still owns, if there is anything to
    /// say.
    ///
    /// Nothing is proposed when local execution has moved past nano's tenure or
    /// tip, when the mempool is empty, and when no extension is due: a block
    /// with no transactions and no tenure change would only ask the signers to
    /// sign the state they already agreed to.
    async fn continue_tenure(
        &self,
        executor: &mut CheckpointExecutor<BurnchainSource>,
    ) -> Result<Option<NakamotoBlock>, Box<dyn Error>> {
        let state = self.tenure.as_ref().expect("a tenure to continue");
        if executor.tip().header.consensus_hash != state.tip.consensus_hash
            || executor.tip().block_id() != state.tip.block_id
        {
            return Ok(None);
        }
        let now = now_unix();
        // Held only while the pool is touched: the peer calls below await, and
        // the RPC admits into the same pool meanwhile.
        let mempool_size = {
            let mut mempool = self.mempool.lock().await;
            // Candidate transport only. A peer may omit transactions (including
            // through its admission view) just as it may serve an empty page;
            // the local account map below decides what can enter this block.
            self.peer.fill_mempool(&mut mempool, now).await?;
            mempool.len()
        };
        self.metrics.publish_mempool_size(mempool_size);
        let accounts = {
            let mempool = self.mempool.lock().await;
            executor.local_mempool_accounts(&mempool)?
        };
        let (pending, mempool_size) = {
            let mut mempool = self.mempool.lock().await;
            mempool.advance(&accounts, now);
            (mempool.candidates(&accounts), mempool.len())
        };
        self.metrics.publish_mempool_size(mempool_size);
        let extend_due = !state.extended
            && state.since.elapsed() >= Duration::from_secs(self.miner.tenure_extend_after_secs);
        if pending.is_empty() && !extend_due {
            return Ok(None);
        }

        let sortition = executor.local_sortition_info(state.tip.consensus_hash)?;
        let burn_view = if extend_due {
            executor.local_burn_tip()?.consensus_hash
        } else {
            state.burn_view
        };
        let candidate = if extend_due {
            println!(
                "extending tenure {} after {:?} into burn view {}",
                state.tip.consensus_hash,
                state.since.elapsed(),
                burn_view
            );
            build_tenure_extend_block(
                &state.tip,
                TenureExtension {
                    burn_view_consensus_hash: burn_view,
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

        let context = executor.local_mining_context(&self.pox, &candidate, burn_view)?;
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
        let block = self
            .submit(block, sortition.bitcoin_height, context, executor)
            .await?;
        let applied = executor.accept_own_block(&block, context)?;
        self.dispatcher.dispatch(
            EventKind::MinedNakamotoBlock,
            &mined_nakamoto_block_payload(&block, &applied, sortition.bitcoin_height),
        );
        // A confirmed transaction leaves now rather than when the peer's
        // account nonces catch up, so the next block does not offer it again.
        let mut mempool = self.mempool.lock().await;
        for transaction in &block.transactions {
            mempool.remove(transaction.txid());
        }
        self.metrics.publish_mempool_size(mempool.len());
        drop(mempool);
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
        let view = executor.local_tenure_view(won.consensus_hash)?;
        let parent = executor.local_parent_tenure(&self.miner_principal())?;
        let candidate = build_tenure_start_block(
            won,
            parent,
            view,
            self.network,
            &self.miner_key,
            &self.vrf_key,
            won.bitcoin_timestamp,
        )?;

        let context = executor.local_mining_context(&self.pox, &candidate, won.consensus_hash)?;
        let (block, applied) = executor.assemble(candidate, context, &self.miner_key)?;
        println!(
            "assembled block {} at height {} with state root {}",
            block.block_id(),
            block.header.chain_length,
            hex::encode(applied.execution.state_root.0)
        );
        let block = self
            .submit(block, won.bitcoin_height, context, executor)
            .await?;
        let applied = executor.accept_own_block(&block, context)?;
        self.dispatcher.dispatch(
            EventKind::MinedNakamotoBlock,
            &mined_nakamoto_block_payload(&block, &applied, won.bitcoin_height),
        );
        Ok(block)
    }

    /// Publish a block to the signers and submit it once they have signed it.
    async fn submit(
        &self,
        block: NakamotoBlock,
        bitcoin_height: u64,
        context: BitcoinBlockContext,
        executor: &mut CheckpointExecutor<BurnchainSource>,
    ) -> Result<NakamotoBlock, Box<dyn Error>> {
        let reward_cycle = self.pox.reward_cycle(bitcoin_height);
        let signer_weights = executor.local_proposal_signers(context)?;
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
                .finalize_and_submit(&proposal, &signer_weights, &self.peer)
                .await
            {
                Ok(block) => {
                    // Announced, not offered: `announce` is the outbound queue,
                    // and a block this node mined needs no authenticating against
                    // itself — it was assembled on this node's own tip and its
                    // state root was derived here before the stock node accepted it.
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
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn current_commitment_parent(
    executed: ConsensusHash,
    latest: Option<&SortitionInfo>,
) -> Result<(), String> {
    let latest = latest.ok_or("the local sortition chain has elected no parent tenure")?;
    if latest.consensus_hash != executed {
        return Err(format!(
            "the executed tenure {executed} has not caught up to the latest locally elected \
             tenure {} at burn {}; delaying the commitment",
            latest.consensus_hash, latest.bitcoin_height
        ));
    }
    Ok(())
}

fn parse_outpoint(value: &str) -> Option<OutPoint> {
    let (transaction_id, index) = value.split_once(':')?;
    Some(OutPoint {
        txid: Txid::from_str(transaction_id).ok()?,
        vout: index.parse().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use nano_miner::{ParentTenure, TenureTip};
    use nano_primitives::{
        BitcoinHeaderHash, BlockHeaderHash, ConsensusHash, Hash160, SortitionId, StacksBlockId,
    };
    use nano_sync::SortitionInfo;

    use super::{MinedTenures, State, current_commitment_parent};

    fn won(miner: Hash160) -> SortitionInfo {
        SortitionInfo {
            bitcoin_block_hash: BitcoinHeaderHash::from_bytes([1; 32]),
            bitcoin_height: 10,
            bitcoin_timestamp: 11,
            sortition_id: SortitionId::from_bytes([2; 32]),
            parent_sortition_id: SortitionId::from_bytes([3; 32]),
            consensus_hash: ConsensusHash::from_bytes([4; 20]),
            was_sortition: true,
            miner_public_key_hash: Some(miner),
            stacks_parent_consensus_hash: Some(ConsensusHash::from_bytes([5; 20])),
            last_sortition_consensus_hash: Some(ConsensusHash::from_bytes([5; 20])),
            committed_block_hash: Some(BlockHeaderHash::from_bytes([6; 32])),
            vrf_seed: Some([7; 32]),
            mining_competition: None,
        }
    }

    #[test]
    fn a_failed_proposal_leaves_the_won_tenure_retryable() {
        let miner_hash = Hash160::from_bytes([8; 20]);
        let won = won(miner_hash);
        let mut mined = MinedTenures::default();

        assert!(mined.is_pending(&won, miner_hash));
        // A transport/signing failure makes no state transition.
        assert!(mined.is_pending(&won, miner_hash));
        mined.accepted(won.consensus_hash);
        assert!(!mined.is_pending(&won, miner_hash));
    }

    #[test]
    fn a_restart_resumes_from_the_local_tenure_identity_and_count() {
        let miner = Hash160::from_bytes([8; 20]);
        let won = won(miner);
        let parent = ParentTenure {
            tip: TenureTip {
                consensus_hash: won.consensus_hash,
                block_id: StacksBlockId::from_bytes([9; 32]),
                height: 120,
                bitcoin_spent: 50,
                timestamp: 60,
            },
            start_block_id: StacksBlockId::from_bytes([10; 32]),
            blocks: 9,
            miner_nonce: 22,
        };

        let burn_view = ConsensusHash::from_bytes([12; 20]);
        let resumed = State::resume_tenure(&won, parent, burn_view).expect("resume local tenure");
        assert_eq!(resumed.tip, parent.tip);
        assert_eq!(resumed.burn_view, burn_view);
        assert_eq!(resumed.blocks, 9);
        assert_eq!(resumed.nonce, 22);

        let mut foreign = won;
        foreign.consensus_hash = ConsensusHash::from_bytes([11; 20]);
        assert!(State::resume_tenure(&foreign, parent, burn_view).is_err());
    }

    #[test]
    fn a_commitment_waits_for_the_latest_elected_tenure_to_execute() {
        let miner = Hash160::from_bytes([8; 20]);
        let latest = won(miner);
        let stale = ConsensusHash::from_bytes([9; 20]);
        let error = current_commitment_parent(stale, Some(&latest))
            .expect_err("a stale executed tenure cannot produce a usable commitment");
        assert!(error.contains(&latest.consensus_hash.to_string()));
        assert!(error.contains(&latest.bitcoin_height.to_string()));

        assert_eq!(
            current_commitment_parent(latest.consensus_hash, Some(&latest)),
            Ok(())
        );
    }

    #[test]
    fn peer_consensus_routes_are_not_miner_proposal_inputs() {
        let source = include_str!("miner.rs");
        for (receiver, method) in [
            (".peer.", "tenure_info("),
            (".peer.", "sortition("),
            (".peer.", "sortition_tip("),
            (".peer.", "sortition_at_height("),
            (".peer.", "tenure_coinbase_context("),
            (".peer.", "stacker_set("),
            (".peer.", "account_nonce("),
            (".peer.", "accounts_for("),
            ("plan_", "commitment("),
        ] {
            let forbidden = format!("{receiver}{method}");
            assert!(
                !source.contains(&forbidden),
                "miner proposals must not read peer consensus input through {forbidden}"
            );
        }
    }
}
