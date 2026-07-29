#![forbid(unsafe_code)]

//! Mine as a Hacknet participant for as long as the process runs.
//!
//! A tenure has to be proposed within seconds of the sortition that awarded it:
//! signers track one active miner, and reject a block whose tenure is no longer
//! the one they follow. So the executed chain state is kept across burn blocks
//! rather than replayed from the checkpoint for each attempt, and committing,
//! following, and mining share one loop.

use std::{
    error::Error,
    fs, io,
    path::PathBuf,
    str::FromStr,
    time::{Duration, Instant},
};

use bitcoin::{Amount, Txid};
use bitcoincore_rpc::Auth;
use clap::Parser;
use nano_address::StacksAddress;
use nano_bitcoin::BitcoinRpcSource;
use nano_chainstate::{NakamotoBlock, SignerSetError, TenureAccounting};
use nano_crypto::{StacksPrivateKey, VrfPrivateKey};
use nano_miner::{
    BitcoinTenureView, BitcoinWallet, CommitmentPlanError, ProposalCoordinator, ProposalError,
    RegisteredLeaderKey, SortitionHashPoint, TenureExtension, TenureTip,
    build_tenure_continuation_block, build_tenure_extend_block, build_tenure_start_block,
    extend_sortition_hash, plan_commitment, total_burn_after,
};
use nano_node::CheckpointExecutor;
use nano_primitives::{ConsensusHash, TrieHash, hash160};
use nano_stackerdb::{BlockProposal, StackerDbClient, StackerDbContract};
use nano_sync::{PoxInfo, SortitionInfo, SyncClient};
use reqwest::Url;
use tokio::time::sleep;

#[derive(Parser)]
#[command(name = "stacks-miner-run")]
struct Cli {
    /// Stock node HTTP endpoint.
    #[arg(long, default_value = "http://127.0.0.1:20443/")]
    peer: String,
    #[arg(long)]
    bitcoin_rpc: String,
    #[arg(long)]
    bitcoin_rpc_user: String,
    #[arg(long)]
    bitcoin_rpc_password_file: PathBuf,
    /// Bitcoin Core wallet funding the commitments, which must hold its keys.
    #[arg(long)]
    bitcoin_wallet: String,
    /// Bitcoin transaction that registered this miner's leader key.
    #[arg(long)]
    key_txid: String,
    /// Satoshis each commitment pays the reward cycle's sBTC address.
    #[arg(long, default_value_t = 20_000)]
    commitment_sats: u64,
    #[arg(long)]
    fee_rate_sats_per_vbyte: Option<u64>,
    /// Miner `StackerDB` contract as ADDRESS/name.
    #[arg(long, default_value = "ST000000000000000000002AMW42H/miners")]
    miner_contract: String,
    /// Boot address hosting the per-cycle signer `StackerDB` contracts.
    #[arg(long, default_value = "ST000000000000000000002AMW42H")]
    signer_contract_address: String,
    #[arg(long)]
    block_signing_private_key_file: PathBuf,
    #[arg(long)]
    vrf_private_key_file: PathBuf,
    #[arg(long)]
    checkpoint: PathBuf,
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
    /// Cached sortition-hash chain point, extended and rewritten as it advances.
    #[arg(long)]
    sortition_hash_cache: PathBuf,
    /// File holding the previous commitment's change output, which the next
    /// commitment must spend so the sortition attributes them to one miner.
    #[arg(long)]
    commitment_chain_file: PathBuf,
    #[arg(long, default_value_t = 0x8000_0000)]
    chain_id: u32,
    #[arg(long)]
    pox_5_activation_height: Option<u32>,
    /// Seconds to wait for the threshold signer response set.
    #[arg(long, default_value_t = 60)]
    signer_timeout_secs: u64,
    #[arg(long, default_value_t = 1)]
    poll_interval_secs: u64,
    #[arg(long, default_value_t = 20_000)]
    max_sync_blocks: usize,
    /// Seconds a tenure may run before nano extends it, matching the idle
    /// timeout a signer offers an extension after.
    #[arg(long, default_value_t = 122)]
    tenure_extend_after_secs: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let node = SyncClient::new(Url::parse(&cli.peer)?)?;
    let pox = node.pox_info().await?;
    let password = fs::read_to_string(&cli.bitcoin_rpc_password_file)?
        .trim_end()
        .to_owned();
    let miner_key =
        StacksPrivateKey::from_bytes(read_hex_array(&cli.block_signing_private_key_file)?)?;
    let vrf_key = VrfPrivateKey::from_bytes(read_hex_array(&cli.vrf_private_key_file)?);
    let miner_hash = hash160(&miner_key.public_key().to_bytes_compressed());
    let miner_address = StacksAddress::single_signature(miner_hash, false);

    let wallet = BitcoinWallet::connect(
        &format!(
            "{}/wallet/{}",
            cli.bitcoin_rpc.trim_end_matches('/'),
            cli.bitcoin_wallet
        ),
        Auth::UserPass(cli.bitcoin_rpc_user.clone(), password.clone()),
    )?;
    let leader_key = registered_key(&wallet, &cli).await?;

    let mut executor = open_checkpoint(&cli, &pox, &password)?;
    println!("mining as {miner_hash} from the checkpoint");

    let mut committed_at = 0;
    let mut mined = Vec::new();
    let mut tenure: Option<TenureState> = None;
    loop {
        if let Err(error) = executor
            .follow_to_tip(&node, &pox, cli.max_sync_blocks)
            .await
        {
            eprintln!("following the peer failed: {error}");
            sleep(Duration::from_secs(cli.poll_interval_secs)).await;
            continue;
        }
        let bitcoin_height = wallet.block_count()?;
        if bitcoin_height > committed_at {
            match commit(&cli, &node, &wallet, &password, leader_key).await {
                Ok(()) => committed_at = bitcoin_height,
                Err(error) => {
                    eprintln!("committing at Bitcoin height {bitcoin_height} failed: {error}");
                }
            }
        }
        match won_tenure(&node, miner_hash, &mined).await {
            Ok(Some(won)) => {
                let consensus_hash = won.consensus_hash;
                match mine(
                    &cli,
                    &node,
                    &pox,
                    &password,
                    &miner_key,
                    &vrf_key,
                    &mut executor,
                    &won,
                )
                .await
                {
                    Ok(block) => {
                        println!("the network accepted nano's block {}", block.block_id());
                        tenure = Some(TenureState::started(
                            &won,
                            &block,
                            node.account_nonce(miner_address).await?,
                        ));
                        executor.accept_own_block(block);
                    }
                    Err(error) => eprintln!("mining tenure {consensus_hash} failed: {error}"),
                }
                mined.push(consensus_hash);
            }
            // A tenure is not one block: while nano still owns the current
            // one, it keeps confirming what the mempool holds, and says on
            // chain when the tenure outlives the budget it started with.
            Ok(None) => {
                if let Some(state) = tenure.as_mut() {
                    match continue_tenure(&cli, &node, &pox, &miner_key, &mut executor, state).await
                    {
                        Ok(Some(block)) => {
                            println!(
                                "the network accepted nano's block {} at height {}",
                                block.block_id(),
                                block.header.chain_length
                            );
                            state.advance(&block);
                            executor.accept_own_block(block);
                        }
                        Ok(None) => {}
                        Err(error) => {
                            eprintln!("continuing the tenure failed: {error}");
                            tenure = None;
                        }
                    }
                }
            }
            Err(error) => eprintln!("reading the sortition failed: {error}"),
        }
        sleep(Duration::from_secs(cli.poll_interval_secs)).await;
    }
}

/// Locate the leader-key registration once Bitcoin has confirmed it.
///
/// A miner is usually started right after registering its key, so the
/// registration is still in the mempool and has no position to commit against
/// yet.
async fn registered_key(
    wallet: &BitcoinWallet,
    cli: &Cli,
) -> Result<RegisteredLeaderKey, Box<dyn Error>> {
    let txid = Txid::from_str(&cli.key_txid)?;
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
                sleep(Duration::from_secs(cli.poll_interval_secs)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Open the checkpoint the miner extends, with the rewards it still owes.
fn open_checkpoint(
    cli: &Cli,
    pox: &PoxInfo,
    password: &str,
) -> Result<CheckpointExecutor<BitcoinRpcSource>, Box<dyn Error>> {
    let mut context = pox.bitcoin_context();
    context.height = cli.anchor_bitcoin_height;
    if let Some(height) = cli.pox_5_activation_height {
        context.pox_5_activation_height = height;
    }
    Ok(CheckpointExecutor::from_checkpoint_with_accounting(
        &cli.checkpoint,
        parse_hex_array(&cli.source_state_id)?,
        TrieHash::from_bytes(parse_hex_array(&cli.state_root)?),
        NakamotoBlock::decode(&fs::read(&cli.anchor_block)?)?,
        context,
        bitcoin_source(cli, password)?,
        match &cli.tenure_accounting {
            Some(path) => Some(TenureAccounting::from_json(&fs::read(path)?)?),
            None => None,
        },
    )?)
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
        self.blocks = self.blocks.saturating_add(1);
        if nano_chainstate::starts_or_extends_tenure(block) {
            self.nonce = self.nonce.saturating_add(1);
            self.since = Instant::now();
            self.extended = true;
        }
    }
}

/// Mine the next block of a tenure nano still owns, if there is anything to say.
///
/// Nothing is proposed when the peer has moved past nano's tenure or its tip,
/// when the mempool is empty, and when no extension is due: a block with no
/// transactions and no tenure change would only ask the signers to sign the
/// state it already agreed to.
async fn continue_tenure(
    cli: &Cli,
    node: &SyncClient,
    pox: &PoxInfo,
    miner_key: &StacksPrivateKey,
    executor: &mut CheckpointExecutor<BitcoinRpcSource>,
    state: &TenureState,
) -> Result<Option<NakamotoBlock>, Box<dyn Error>> {
    let tenure = node.tenure_info().await?;
    if tenure.consensus_hash != state.tip.consensus_hash
        || tenure.tip_block_id != state.tip.block_id
    {
        return Ok(None);
    }
    let (pending, _) = node.mempool_page(None).await?;
    let extend_due = !state.extended
        && state.since.elapsed() >= Duration::from_secs(cli.tenure_extend_after_secs);
    if pending.is_empty() && !extend_due {
        return Ok(None);
    }

    let sortition = node.sortition(state.tip.consensus_hash).await?;
    let mut context = pox.bitcoin_context();
    context.height = sortition.bitcoin_height;
    if let Some(height) = cli.pox_5_activation_height {
        context.pox_5_activation_height = height;
    }
    let burn_view = node.sortition_tip().await?;
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
                timestamp: burn_view.bitcoin_timestamp,
            },
            cli.chain_id,
            miner_key,
            Vec::new(),
        )?
    } else {
        build_tenure_continuation_block(&state.tip, Vec::new(), burn_view.bitcoin_timestamp)
    };

    let (block, applied) = executor.assemble_selecting(candidate, context, &pending, miner_key)?;
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
    submit(cli, node, pox, miner_key, block, sortition.bitcoin_height)
        .await
        .map(Some)
}

/// The tenure this miner has won and not yet mined, if there is one.
///
/// A Bitcoin block without a sortition does not end the previous tenure, so the
/// tenure to mine is the last sortition that chose a miner.
async fn won_tenure(
    node: &SyncClient,
    miner_hash: nano_primitives::Hash160,
    mined: &[ConsensusHash],
) -> Result<Option<SortitionInfo>, Box<dyn Error>> {
    let tip = node.sortition_tip().await?;
    let current = if tip.was_sortition {
        Some(tip)
    } else {
        match tip.last_sortition_consensus_hash {
            Some(consensus_hash) => Some(node.sortition(consensus_hash).await?),
            None => None,
        }
    };
    Ok(current.filter(|sortition| {
        sortition.was_sortition
            && sortition.miner_public_key_hash == Some(miner_hash)
            && !mined.contains(&sortition.consensus_hash)
    }))
}

/// Commit to the next tenure, chained to the previous commitment's change.
async fn commit(
    cli: &Cli,
    node: &SyncClient,
    wallet: &BitcoinWallet,
    password: &str,
    leader_key: RegisteredLeaderKey,
) -> Result<(), Box<dyn Error>> {
    let mut bitcoin = bitcoin_source(cli, password)?;
    let plan = match plan_commitment(node, &mut bitcoin, leader_key, wallet.block_count()?).await {
        Err(CommitmentPlanError::StaleNodeView { node, bitcoin }) => {
            return Err(CommitmentPlanError::StaleNodeView { node, bitcoin }.into());
        }
        result => result?,
    };
    let previous_change = fs::read_to_string(&cli.commitment_chain_file)
        .ok()
        .and_then(|value| parse_outpoint(value.trim()));
    let submitted = wallet.submit_leader_commitment(
        *b"T3",
        plan.commitment,
        &plan.sbtc_address,
        Amount::from_sat(cli.commitment_sats),
        cli.fee_rate_sats_per_vbyte,
        previous_change,
    )?;
    fs::write(
        &cli.commitment_chain_file,
        format!("{}:{}", submitted.transaction_id, submitted.change_output),
    )?;
    println!(
        "committed to tenure {} at Bitcoin height {} paying {} sats",
        hex::encode(plan.commitment.block_header_hash),
        plan.target_bitcoin_height,
        cli.commitment_sats
    );
    Ok(())
}

/// Assemble the tenure's first block, gather threshold signatures, submit it.
#[allow(clippy::too_many_arguments)]
async fn mine(
    cli: &Cli,
    node: &SyncClient,
    pox: &PoxInfo,
    password: &str,
    miner_key: &StacksPrivateKey,
    vrf_key: &VrfPrivateKey,
    executor: &mut CheckpointExecutor<BitcoinRpcSource>,
    won: &SortitionInfo,
) -> Result<NakamotoBlock, Box<dyn Error>> {
    println!(
        "won the sortition at Bitcoin height {} with consensus hash {}",
        won.bitcoin_height, won.consensus_hash
    );
    let view = tenure_view(cli, node, password, won, pox.first_bitcoin_height).await?;
    let candidate = build_tenure_start_block(
        node,
        won,
        view,
        cli.chain_id,
        miner_key,
        vrf_key,
        won.bitcoin_timestamp,
    )
    .await?;

    let mut context = pox.bitcoin_context();
    context.height = won.bitcoin_height;
    if let Some(height) = cli.pox_5_activation_height {
        context.pox_5_activation_height = height;
    }
    let context = node
        .tenure_coinbase_context(
            &candidate,
            executor.chainstate_mut().accounting_mut().schedule(),
            context,
        )
        .await?;
    let (block, applied) = executor.assemble(candidate, context, miner_key)?;
    println!(
        "assembled block {} at height {} with state root {}",
        block.block_id(),
        block.header.chain_length,
        hex::encode(applied.execution.state_root.0)
    );

    submit(cli, node, pox, miner_key, block, won.bitcoin_height).await
}

/// Publish a block to the signers and submit it once they have signed it.
async fn submit(
    cli: &Cli,
    node: &SyncClient,
    pox: &PoxInfo,
    miner_key: &StacksPrivateKey,
    block: NakamotoBlock,
    bitcoin_height: u64,
) -> Result<NakamotoBlock, Box<dyn Error>> {
    let reward_cycle = pox.reward_cycle(bitcoin_height);
    let reward_set = node.stacker_set(reward_cycle).await?;
    let proposal = BlockProposal {
        block,
        bitcoin_height,
        reward_cycle,
        data: BlockProposal::empty_data(),
    };
    let coordinator = ProposalCoordinator::new(
        StackerDbClient::new(Url::parse(&cli.peer)?)?,
        parse_contract(&cli.miner_contract)?,
        StackerDbContract {
            address: StacksAddress::from_str(&cli.signer_contract_address)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?,
            // Signer contracts are named by reward-cycle parity and message id.
            name: format!("signers-{}-1", reward_cycle % 2),
        },
        miner_key.clone(),
    );
    coordinator.publish_proposal(&proposal).await?;
    println!("published the proposal to the miner slots this key owns");

    let deadline = Instant::now() + Duration::from_secs(cli.signer_timeout_secs);
    loop {
        match coordinator
            .finalize_and_submit(&proposal, &reward_set.signer_set, node)
            .await
        {
            Ok(block) => return Ok(block),
            Err(ProposalError::SignerSet(SignerSetError::InsufficientWeight))
                if Instant::now() < deadline =>
            {
                sleep(Duration::from_secs(cli.poll_interval_secs)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// The burn total and sortition hash the won tenure must commit to.
async fn tenure_view(
    cli: &Cli,
    node: &SyncClient,
    password: &str,
    won: &SortitionInfo,
    first_bitcoin_height: u64,
) -> Result<BitcoinTenureView, Box<dyn Error>> {
    let mut bitcoin = bitcoin_source(cli, password)?;
    let tenure = node.tenure_info().await?;
    let parent = node.sortition(tenure.consensus_hash).await?;
    let parent_start = node.block(tenure.tenure_start_block_id).await?;
    let mut sortition_heights = Vec::new();
    for height in parent.bitcoin_height + 1..=won.bitcoin_height {
        if node.sortition_at_height(height).await?.was_sortition {
            sortition_heights.push(height);
        }
    }
    let total_burn = total_burn_after(
        &mut bitcoin,
        parent_start.header.bitcoin_spent,
        &sortition_heights,
    )?;

    let cached = fs::read(&cli.sortition_hash_cache)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<SortitionHashPoint>(&bytes).ok())
        .filter(|point| point.bitcoin_height <= won.bitcoin_height)
        .unwrap_or_else(|| SortitionHashPoint::genesis(first_bitcoin_height));
    let point = extend_sortition_hash(node, &bitcoin, cached, won.bitcoin_height).await?;
    fs::write(&cli.sortition_hash_cache, serde_json::to_vec(&point)?)?;
    Ok(BitcoinTenureView {
        total_burn,
        sortition_hash: point.sortition_hash,
    })
}

fn bitcoin_source(cli: &Cli, password: &str) -> Result<BitcoinRpcSource, Box<dyn Error>> {
    Ok(BitcoinRpcSource::new(
        &cli.bitcoin_rpc,
        cli.bitcoin_rpc_user.clone(),
        password.to_owned(),
        *b"T3",
    )?)
}

fn parse_outpoint(value: &str) -> Option<bitcoin::OutPoint> {
    let (transaction_id, index) = value.split_once(':')?;
    Some(bitcoin::OutPoint {
        txid: Txid::from_str(transaction_id).ok()?,
        vout: index.parse().ok()?,
    })
}

fn parse_contract(value: &str) -> Result<StackerDbContract, io::Error> {
    let (address, name) = value.split_once('/').ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "contract must use ADDRESS/name syntax",
        )
    })?;
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

fn read_hex_array<const N: usize>(path: &PathBuf) -> Result<[u8; N], io::Error> {
    parse_hex_array(&fs::read_to_string(path)?)
}

fn parse_hex_array<const N: usize>(value: &str) -> Result<[u8; N], io::Error> {
    let bytes = hex::decode(value.trim().trim_start_matches("0x")).map_err(|error| {
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
