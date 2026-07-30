use std::{
    collections::{BTreeSet, HashMap},
    fmt,
    time::Duration,
};

use nano_address::{PoxAddress, PoxAddressType32, StacksAddress};
use nano_chainstate::{
    BitcoinBlockContext, CoinbaseSchedule, NakamotoBlock, NakamotoCodecError, Signer, SignerSet,
    SignerSetError, TenureError,
};
use nano_codec::Transaction;
use nano_crypto::{CryptoError, StacksPublicKey};
use nano_mempool::{Account, Admission, ChainTip, Mempool};
use nano_primitives::{
    BitcoinHeaderHash, BlockHeaderHash, ConsensusHash, Hash160, Sha256Sum, SortitionId,
    StacksBlockId,
};
use reqwest::{Client, Url, header::CONTENT_TYPE};
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct SyncClient {
    client: Client,
    base_url: Url,
}

/// The accounts a peer reports, as the tip a mempool judges against.
///
/// A node that follows a peer has no account index of its own until it
/// executes, so the peer's view of the tip is the one it admits against.
#[derive(Clone, Debug, Default)]
pub struct PeerAccounts(HashMap<StacksAddress, Account>);

impl ChainTip for PeerAccounts {
    fn account(&self, address: &StacksAddress) -> Account {
        self.0.account(address)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeInfo {
    pub bitcoin_height: u64,
    pub stacks_height: u64,
    pub stacks_tip: BlockHeaderHash,
    pub consensus_hash: ConsensusHash,
    pub network_id: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TenureInfo {
    pub consensus_hash: ConsensusHash,
    pub tenure_start_block_id: StacksBlockId,
    pub parent_consensus_hash: ConsensusHash,
    pub parent_tenure_start_block_id: StacksBlockId,
    pub tip_block_id: StacksBlockId,
    pub tip_height: u64,
    pub reward_cycle: u64,
}

/// Bitcoin sortition data used to authenticate a Nakamoto tenure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SortitionInfo {
    pub bitcoin_block_hash: BitcoinHeaderHash,
    pub bitcoin_height: u64,
    pub bitcoin_timestamp: u64,
    pub sortition_id: SortitionId,
    pub parent_sortition_id: SortitionId,
    pub consensus_hash: ConsensusHash,
    pub was_sortition: bool,
    pub miner_public_key_hash: Option<Hash160>,
    pub stacks_parent_consensus_hash: Option<ConsensusHash>,
    pub last_sortition_consensus_hash: Option<ConsensusHash>,
    pub committed_block_hash: Option<BlockHeaderHash>,
    /// The winning commitment's new seed, which seeds the next sortition hash.
    pub vrf_seed: Option<[u8; 32]>,
}

/// A locally validated tenure downloaded from a peer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FollowedTenure {
    pub info: TenureInfo,
    pub sortition: SortitionInfo,
    pub blocks: Vec<NakamotoBlock>,
}

/// Stateful HTTP follower for the peer's current tenure.
#[derive(Clone, Debug)]
pub struct TenureFollower {
    client: SyncClient,
    latest: Option<TenureInfo>,
    history: Vec<FollowedTenure>,
}

/// One burn block on the view a peer holds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForkInfo {
    pub bitcoin_height: u64,
    pub consensus_hash: ConsensusHash,
    pub was_sortition: bool,
    pub first_block_mined: Option<StacksBlockId>,
}

/// The first burn block two views agree on, which is where they parted.
///
/// Both are ordered newest first, as the endpoint returns them.
#[must_use]
pub fn fork_point(ours: &[ForkInfo], theirs: &[ForkInfo]) -> Option<ConsensusHash> {
    let theirs: BTreeSet<_> = theirs.iter().map(|entry| entry.consensus_hash).collect();
    ours.iter()
        .find(|entry| theirs.contains(&entry.consensus_hash))
        .map(|entry| entry.consensus_hash)
}

/// A chain tip a peer is offering, and the header that proves it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateTip {
    /// Which of the followed peers offered it.
    pub peer: usize,
    pub info: TenureInfo,
    pub header: nano_chainstate::NakamotoBlockHeader,
}

/// Why a candidate tip is not one to follow.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TipRejection {
    /// Its signatures do not add up to the threshold the reward set requires.
    InsufficientWeight,
    /// It carries a signature from outside the reward set, or out of order.
    UnknownOrUnorderedSigner,
    /// Its signatures do not recover.
    UnrecoverableSignature,
}

impl fmt::Display for TipRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InsufficientWeight => "tip does not carry threshold signer weight",
            Self::UnknownOrUnorderedSigner => "tip carries a signature from outside the reward set",
            Self::UnrecoverableSignature => "tip carries a signature that does not recover",
        })
    }
}

impl std::error::Error for TipRejection {}

/// Whether a tip is one the network would accept, and what weight signed it.
///
/// Length decides nothing on its own: a peer can serve a longer chain that no
/// signer put its name to. Weight is what makes a chain canonical, so it is
/// checked before length is even compared.
pub fn weigh_tip(
    header: &nano_chainstate::NakamotoBlockHeader,
    signers: &SignerSet,
) -> Result<u32, TipRejection> {
    signers.verify(header).map_err(|error| match error {
        SignerSetError::InsufficientWeight => TipRejection::InsufficientWeight,
        SignerSetError::Signature(_) => TipRejection::UnrecoverableSignature,
        _ => TipRejection::UnknownOrUnorderedSigner,
    })
}

/// Choose the tip to follow among the ones the peers are offering.
///
/// A candidate has to carry threshold signer weight before its length counts.
/// Among those, the longest chain wins, and an exact tie goes to the lowest
/// block identifier so that every node following the same peers lands on the
/// same block rather than on whichever answered first.
#[must_use]
pub fn choose_canonical_tip<'a>(
    candidates: &'a [CandidateTip],
    signers: &SignerSet,
) -> Option<&'a CandidateTip> {
    candidates
        .iter()
        .filter(|candidate| weigh_tip(&candidate.header, signers).is_ok())
        .max_by(|left, right| {
            left.header
                .chain_length
                .cmp(&right.header.chain_length)
                .then_with(|| right.header.block_id().cmp(&left.header.block_id()))
        })
}

/// Bitcoin calendar and stacking parameters advertised by a node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PoxInfo {
    pub first_bitcoin_height: u64,
    pub bitcoin_height: u64,
    pub prepare_phase_length: u32,
    pub reward_phase_length: u32,
    pub reward_slots: u32,
    pub rejection_fraction: Option<u64>,
    pub pox_5_activation_height: Option<u32>,
    /// Height at which PoX-1 locks expire, one block after epoch 2.1 begins.
    pub v1_unlock_height: Option<u32>,
    /// Height at which PoX-2 locks expire, one block after epoch 2.2 begins.
    pub v2_unlock_height: Option<u32>,
    /// Height at which PoX-3 locks expire, one block after epoch 2.5 begins.
    pub v3_unlock_height: Option<u32>,
}

/// The threshold signer set and optional waterfall payout address for one reward cycle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackerSet {
    pub pox_ustx_threshold: u128,
    pub sbtc_address: Option<PoxAddress>,
    pub signer_set: SignerSet,
}

/// A stock node's acknowledgement of a finalized block upload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockUpload {
    pub accepted: bool,
    pub block_id: StacksBlockId,
}

impl PoxInfo {
    /// The reward cycle a Bitcoin height belongs to.
    #[must_use]
    pub fn reward_cycle(&self, bitcoin_height: u64) -> u64 {
        let length = u64::from(self.prepare_phase_length) + u64::from(self.reward_phase_length);
        bitcoin_height.saturating_sub(self.first_bitcoin_height) / length.max(1)
    }

    /// Convert the node response into the context required for VM execution.
    #[must_use]
    pub fn bitcoin_context(&self) -> BitcoinBlockContext {
        BitcoinBlockContext {
            height: self.bitcoin_height,
            first_height: self.first_bitcoin_height,
            prepare_phase_length: self.prepare_phase_length,
            reward_phase_length: self.reward_phase_length,
            rejection_fraction: self.rejection_fraction.unwrap_or(0),
            v1_unlock_height: self.v1_unlock_height.unwrap_or(u32::MAX),
            v2_unlock_height: self.v2_unlock_height.unwrap_or(u32::MAX),
            v3_unlock_height: self.v3_unlock_height.unwrap_or(u32::MAX),
            pox_5_activation_height: self.pox_5_activation_height.unwrap_or(u32::MAX),
            // Only a tenure-start block collects a coinbase, so its caller
            // fills this in from the sortitions around it.
            accumulated_coinbase: 0,
            // The tenure's own burn block, which its caller reads from the
            // sortition that awarded it.
            ..BitcoinBlockContext::at_height(self.bitcoin_height)
        }
    }
}

#[derive(Debug)]
pub enum SyncError {
    InvalidBaseUrl,
    Http(reqwest::Error),
    Block(NakamotoCodecError),
    EmptyTenure,
    TenureStart,
    TenureLink(TenureError),
    InvalidHash,
    EmptySortition,
    InvalidSortition,
    InvalidRewardSet,
    Crypto(CryptoError),
    SignerSet(SignerSetError),
    BlockUploadRejected,
    UnstableTip,
    Fork,
    InvalidMempool,
    InvalidAccount,
}

impl fmt::Display for SyncError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBaseUrl => formatter.write_str("sync base URL cannot be a base"),
            Self::Http(error) => write!(formatter, "HTTP sync error: {error}"),
            Self::Block(error) => write!(formatter, "invalid Nakamoto block response: {error}"),
            Self::EmptyTenure => formatter.write_str("tenure response contains no blocks"),
            Self::TenureStart => formatter.write_str("tenure response starts at the wrong block"),
            Self::TenureLink(error) => write!(formatter, "invalid tenure link: {error}"),
            Self::InvalidHash => formatter.write_str("sync response contains an invalid hash"),
            Self::EmptySortition => formatter.write_str("sortition response contains no entries"),
            Self::InvalidSortition => formatter.write_str("sortition response is inconsistent"),
            Self::InvalidRewardSet => formatter.write_str("reward set response is invalid"),
            Self::Crypto(error) => write!(formatter, "invalid signer key: {error}"),
            Self::SignerSet(error) => write!(formatter, "invalid signer set: {error}"),
            Self::BlockUploadRejected => formatter.write_str("node rejected uploaded block"),
            Self::UnstableTip => formatter.write_str("peer tip changed during tenure download"),
            Self::Fork => formatter.write_str("peer tenure does not extend the followed chain"),
            Self::InvalidMempool => formatter.write_str("mempool page is not a transaction stream"),
            Self::InvalidAccount => formatter.write_str("account response has no readable balance"),
        }
    }
}

impl std::error::Error for SyncError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Http(error) => Some(error),
            Self::Block(error) => Some(error),
            Self::TenureLink(error) => Some(error),
            Self::Crypto(error) => Some(error),
            Self::SignerSet(error) => Some(error),
            Self::InvalidBaseUrl
            | Self::EmptyTenure
            | Self::TenureStart
            | Self::InvalidHash
            | Self::EmptySortition
            | Self::InvalidSortition
            | Self::InvalidRewardSet
            | Self::BlockUploadRejected
            | Self::UnstableTip
            | Self::Fork
            | Self::InvalidMempool
            | Self::InvalidAccount => None,
        }
    }
}

impl From<reqwest::Error> for SyncError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error)
    }
}

impl From<CryptoError> for SyncError {
    fn from(error: CryptoError) -> Self {
        Self::Crypto(error)
    }
}

impl From<SignerSetError> for SyncError {
    fn from(error: SignerSetError) -> Self {
        Self::SignerSet(error)
    }
}

impl SyncClient {
    pub fn new(base_url: Url) -> Result<Self, SyncError> {
        if base_url.cannot_be_a_base() {
            Err(SyncError::InvalidBaseUrl)
        } else {
            Ok(Self {
                client: Client::builder().timeout(Duration::from_secs(30)).build()?,
                base_url,
            })
        }
    }

    /// The peer this client talks to, for the other clients aimed at it.
    #[must_use]
    pub const fn base_url(&self) -> &Url {
        &self.base_url
    }

    pub async fn node_info(&self) -> Result<NodeInfo, SyncError> {
        let response: NodeInfoWire = self.get("v2/info").await?;
        Ok(NodeInfo {
            bitcoin_height: response.burn_block_height,
            stacks_height: response.stacks_tip_height,
            stacks_tip: parse_block_hash(&response.stacks_tip)?,
            consensus_hash: parse_consensus_hash(&response.stacks_tip_consensus_hash)?,
            network_id: response.network_id,
        })
    }

    pub async fn tenure_info(&self) -> Result<TenureInfo, SyncError> {
        let response: TenureInfoWire = self.get("v3/tenures/info").await?;
        Ok(TenureInfo {
            consensus_hash: parse_consensus_hash(&response.consensus_hash)?,
            tenure_start_block_id: parse_block_id(&response.tenure_start_block_id)?,
            parent_consensus_hash: parse_consensus_hash(&response.parent_consensus_hash)?,
            parent_tenure_start_block_id: parse_block_id(&response.parent_tenure_start_block_id)?,
            tip_block_id: parse_block_id(&response.tip_block_id)?,
            tip_height: response.tip_height,
            reward_cycle: response.reward_cycle,
        })
    }

    /// Fetch the Bitcoin calendar and stacking parameters used by the node.
    pub async fn pox_info(&self) -> Result<PoxInfo, SyncError> {
        let response: PoxInfoWire = self.get("v2/pox").await?;
        Ok(PoxInfo {
            first_bitcoin_height: response.first_burnchain_block_height,
            bitcoin_height: response.current_burnchain_block_height,
            prepare_phase_length: response.prepare_phase_block_length,
            reward_phase_length: response.reward_phase_block_length,
            reward_slots: response.reward_slots,
            rejection_fraction: response.rejection_fraction,
            pox_5_activation_height: response
                .contract_versions
                .iter()
                .find(|version| version.contract_id.ends_with(".pox-5"))
                .map(|version| version.activation_burnchain_block_height),
            // Each legacy PoX version unlocks one block into the epoch that
            // retires it, which is how a node derives its own constants.
            v1_unlock_height: response.unlock_height_after("Epoch21"),
            v2_unlock_height: response.unlock_height_after("Epoch22"),
            v3_unlock_height: response.unlock_height_after("Epoch25"),
        })
    }

    /// Fetch the next nonce an account's transactions must use.
    pub async fn account_nonce(&self, address: StacksAddress) -> Result<u64, SyncError> {
        Ok(self.account(address).await?.nonce)
    }

    /// Fetch the nonce and spendable balance a peer holds for an account.
    pub async fn account(&self, address: StacksAddress) -> Result<Account, SyncError> {
        let response: AccountWire = self.get(&format!("v2/accounts/{address}?proof=0")).await?;
        let balance = u128::from_str_radix(response.balance.trim_start_matches("0x"), 16)
            .map_err(|_| SyncError::InvalidAccount)?;
        Ok(Account {
            nonce: response.nonce,
            balance: Some(balance),
        })
    }

    /// Fetch the account state every transaction a mempool holds is judged
    /// against.
    pub async fn accounts_for(&self, mempool: &Mempool) -> Result<PeerAccounts, SyncError> {
        let mut accounts = HashMap::new();
        for address in mempool.addresses() {
            accounts.insert(address, self.account(address).await?);
        }
        Ok(PeerAccounts(accounts))
    }

    /// Offer a peer's whole mempool to a local one, and report what it kept.
    ///
    /// A peer's mempool is a source of transactions, not the node's answer to
    /// what it will mine: every transaction it hands over is admitted on this
    /// node's own rules against this node's own view of the accounts.
    pub async fn fill_mempool(&self, mempool: &mut Mempool, now: u64) -> Result<usize, SyncError> {
        let mut pending = Vec::new();
        let mut page = None;
        loop {
            let (transactions, next) = self.mempool_page(page).await?;
            pending.extend(transactions);
            match next {
                Some(next) => page = Some(next),
                None => break,
            }
        }

        let mut accounts = HashMap::new();
        for transaction in &pending {
            for address in [transaction.origin_address(), transaction.sponsor_address()]
                .into_iter()
                .flatten()
            {
                if let std::collections::hash_map::Entry::Vacant(slot) = accounts.entry(address) {
                    slot.insert(self.account(address).await?);
                }
            }
        }
        let accounts = PeerAccounts(accounts);
        let mut admitted = 0;
        for transaction in pending {
            if matches!(
                mempool.submit(transaction, &accounts, now),
                Ok(Admission::Added | Admission::Replaced(_))
            ) {
                admitted += 1;
            }
        }
        Ok(admitted)
    }

    /// Fetch the waterfall reward set active for one reward cycle.
    pub async fn stacker_set(&self, reward_cycle: u64) -> Result<StackerSet, SyncError> {
        let response: StackerSetResponseWire =
            self.get(&format!("v3/stacker_set/{reward_cycle}")).await?;
        parse_stacker_set(response.stacker_set)
    }

    /// Fetch the Bitcoin sortition identified by its consensus hash.
    pub async fn sortition(
        &self,
        consensus_hash: ConsensusHash,
    ) -> Result<SortitionInfo, SyncError> {
        let sortition = self
            .single_sortition(&format!("v3/sortitions/consensus/{consensus_hash}"))
            .await?;
        if sortition.consensus_hash != consensus_hash {
            return Err(SyncError::InvalidSortition);
        }
        Ok(sortition)
    }

    /// Fetch the sortition of the peer's current Bitcoin tip.
    pub async fn sortition_tip(&self) -> Result<SortitionInfo, SyncError> {
        self.single_sortition("v3/sortitions").await
    }

    /// Fetch the sortition recorded for one Bitcoin height.
    pub async fn sortition_at_height(&self, height: u64) -> Result<SortitionInfo, SyncError> {
        let sortition = self
            .single_sortition(&format!("v3/sortitions/burn_height/{height}"))
            .await?;
        if sortition.bitcoin_height != height {
            return Err(SyncError::InvalidSortition);
        }
        Ok(sortition)
    }

    /// Fetch the transactions a peer holds that this node has not seen.
    ///
    /// The request says which transactions are already known; asking with an
    /// empty set of tags asks for everything. The response is the transactions
    /// back to back with the page identifier for the next request as its last
    /// thirty-two bytes (`core/mempool.rs`, `decode_tx_stream`).
    pub async fn mempool_page(
        &self,
        page: Option<Sha256Sum>,
    ) -> Result<(Vec<Transaction>, Option<Sha256Sum>), SyncError> {
        let path = page.map_or_else(
            || "v2/mempool/query".to_owned(),
            |page| format!("v2/mempool/query?page_id={page}"),
        );
        let url = self
            .base_url
            .join(&path)
            .map_err(|_| SyncError::InvalidBaseUrl)?;
        // An empty tag list under a zero seed claims no knowledge of the peer's
        // mempool, which is how a node that has just started asks for all of it.
        let mut query = vec![MEMPOOL_QUERY_TX_TAGS];
        query.extend_from_slice(&[0; 32]);
        query.extend_from_slice(&0_u32.to_be_bytes());
        let body = self
            .client
            .post(url)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(query)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        decode_mempool_page(&body)
    }

    /// The last Bitcoin height before this one that chose a miner.
    ///
    /// A tenure collects the coinbase of every burn block since that height, so
    /// finding it is what makes a tenure-start block's reward derivable.
    pub async fn previous_sortition_height(
        &self,
        bitcoin_height: u64,
    ) -> Result<Option<u64>, SyncError> {
        let Some(parent_height) = bitcoin_height.checked_sub(1) else {
            return Ok(None);
        };
        let parent = self.sortition_at_height(parent_height).await?;
        if parent.was_sortition {
            return Ok(Some(parent.bitcoin_height));
        }
        match parent.last_sortition_consensus_hash {
            Some(consensus_hash) => Ok(Some(self.sortition(consensus_hash).await?.bitcoin_height)),
            None => Ok(None),
        }
    }

    /// The coinbase a block's tenure accumulated, or nothing when the block
    /// starts no tenure or no schedule says what a coinbase is worth.
    pub async fn accumulated_coinbase(
        &self,
        block: &NakamotoBlock,
        schedule: Option<CoinbaseSchedule>,
        bitcoin_height: u64,
    ) -> Result<Option<u128>, SyncError> {
        let Some(schedule) = schedule.filter(|_| nano_chainstate::starts_new_tenure(block)) else {
            return Ok(None);
        };
        let previous = self.previous_sortition_height(bitcoin_height).await?;
        Ok(Some(schedule.accumulated_at(bitcoin_height, previous)))
    }

    /// Complete a block's execution context with the coinbase its tenure earns.
    pub async fn tenure_coinbase_context(
        &self,
        block: &NakamotoBlock,
        schedule: Option<CoinbaseSchedule>,
        mut context: BitcoinBlockContext,
    ) -> Result<BitcoinBlockContext, SyncError> {
        if let Some(accumulated) = self
            .accumulated_coinbase(block, schedule, context.height)
            .await?
        {
            context.accumulated_coinbase = accumulated;
        }
        Ok(context)
    }

    async fn single_sortition(&self, path: &str) -> Result<SortitionInfo, SyncError> {
        let mut sortitions: Vec<SortitionInfoWire> = self.get(path).await?;
        let sortition = sortitions.pop().ok_or(SyncError::EmptySortition)?;
        if !sortitions.is_empty() {
            return Err(SyncError::InvalidSortition);
        }
        Ok(SortitionInfo {
            bitcoin_block_hash: parse_prefixed_bitcoin_block_hash(&sortition.burn_block_hash)?,
            bitcoin_height: sortition.burn_block_height,
            bitcoin_timestamp: sortition.burn_header_timestamp,
            sortition_id: parse_prefixed_sortition_id(&sortition.sortition_id)?,
            parent_sortition_id: parse_prefixed_sortition_id(&sortition.parent_sortition_id)?,
            consensus_hash: parse_prefixed_consensus_hash(&sortition.consensus_hash)?,
            was_sortition: sortition.was_sortition,
            miner_public_key_hash: sortition
                .miner_pk_hash160
                .as_deref()
                .map(parse_prefixed_hash160)
                .transpose()?,
            stacks_parent_consensus_hash: sortition
                .stacks_parent_ch
                .as_deref()
                .map(parse_prefixed_consensus_hash)
                .transpose()?,
            last_sortition_consensus_hash: sortition
                .last_sortition_ch
                .as_deref()
                .map(parse_prefixed_consensus_hash)
                .transpose()?,
            committed_block_hash: sortition
                .committed_block_hash
                .as_deref()
                .map(parse_prefixed_block_hash)
                .transpose()?,
            vrf_seed: sortition
                .vrf_seed
                .as_deref()
                .map(parse_prefixed_hex)
                .transpose()?,
        })
    }

    /// Download and validate one Nakamoto block by its block ID.
    pub async fn block(&self, block_id: StacksBlockId) -> Result<NakamotoBlock, SyncError> {
        let bytes = self.bytes(&format!("v3/blocks/{block_id}")).await?;
        NakamotoBlock::decode(&bytes).map_err(SyncError::Block)
    }

    /// Upload a finalized block to a stock node and require its exact acknowledgement.
    pub async fn upload_block(&self, block: &NakamotoBlock) -> Result<BlockUpload, SyncError> {
        let url = self
            .base_url
            .join("v3/blocks/upload")
            .map_err(|_| SyncError::InvalidBaseUrl)?;
        let response: BlockUploadWire = self
            .client
            .post(url)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(block.encode())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let upload = BlockUpload {
            accepted: response.accepted,
            block_id: parse_block_id(&response.stacks_block_id)?,
        };
        // A node that already holds the block reports it as not newly accepted,
        // which is success for the miner that produced it.
        if upload.block_id != block.block_id() {
            return Err(SyncError::BlockUploadRejected);
        }
        Ok(upload)
    }

    /// Walk the burn view between two consensus hashes.
    ///
    /// This is how a node finds where a candidate chain left the one it holds:
    /// the answer runs backwards from `start` to `stop`, one entry per burn
    /// block, and the first entry both chains agree on is the fork point.
    pub async fn tenure_fork_info(
        &self,
        start: ConsensusHash,
        stop: ConsensusHash,
    ) -> Result<Vec<ForkInfo>, SyncError> {
        let wire: Vec<ForkInfoWire> = self
            .get(&format!("v3/tenures/fork_info/{start}/{stop}"))
            .await?;
        wire.into_iter()
            .map(|entry| {
                Ok(ForkInfo {
                    bitcoin_height: entry.burn_block_height,
                    consensus_hash: parse_consensus_hash(&entry.consensus_hash)?,
                    was_sortition: entry.was_sortition,
                    first_block_mined: entry
                        .first_block_mined
                        .as_deref()
                        .map(parse_block_id)
                        .transpose()?,
                })
            })
            .collect()
    }

    /// Download and validate all Nakamoto blocks in a tenure.
    pub async fn tenure(
        &self,
        start_block_id: StacksBlockId,
        stop_block_id: Option<StacksBlockId>,
    ) -> Result<Vec<NakamotoBlock>, SyncError> {
        let mut path = format!("v3/tenures/{start_block_id}");
        if let Some(stop_block_id) = stop_block_id {
            use std::fmt::Write;

            write!(path, "?stop={stop_block_id}").expect("writing to a string cannot fail");
        }
        let bytes = self.bytes(&path).await?;
        let mut blocks = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let (block, consumed) =
                NakamotoBlock::decode_prefix(&bytes[offset..]).map_err(SyncError::Block)?;
            offset = offset.checked_add(consumed).ok_or(SyncError::InvalidHash)?;
            blocks.push(block);
        }
        validate_tenure(start_block_id, &blocks)?;
        Ok(blocks)
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, SyncError> {
        let url = self
            .base_url
            .join(path)
            .map_err(|_| SyncError::InvalidBaseUrl)?;
        Ok(self
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn bytes(&self, path: &str) -> Result<Vec<u8>, SyncError> {
        let url = self
            .base_url
            .join(path)
            .map_err(|_| SyncError::InvalidBaseUrl)?;
        Ok(self
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?
            .to_vec())
    }
}

/// Several peers, and which of them have proved unreliable.
///
/// A node with one peer follows whatever that peer says: the peer decides both
/// what nano sees and what it cannot see. A pool asks all of them, so a peer
/// that stalls, lies or withholds costs nano nothing as long as one other peer
/// is honest.
#[derive(Debug)]
pub struct PeerPool {
    peers: Vec<SyncClient>,
    distrusted: BTreeSet<usize>,
}

/// A node's validated view of a remote Nakamoto tenure stream.
#[derive(Clone, Debug)]
pub struct Node {
    client: SyncClient,
    follower: TenureFollower,
    peer_info: Option<NodeInfo>,
    pox_info: Option<PoxInfo>,
}

/// A consistent read-only snapshot that can be served by the public RPC.
#[derive(Clone, Debug)]
pub struct NodeView {
    pub node_info: NodeInfo,
    pub pox_info: PoxInfo,
    pub tenures: Vec<FollowedTenure>,
}

impl Node {
    /// Construct a node that follows the supplied HTTP peer.
    #[must_use]
    pub fn new(client: SyncClient) -> Self {
        Self {
            follower: TenureFollower::new(client.clone()),
            client,
            peer_info: None,
            pox_info: None,
        }
    }

    /// Return the latest validated peer tenure.
    #[must_use]
    pub const fn latest_tenure(&self) -> Option<&TenureInfo> {
        self.follower.latest()
    }

    /// Fetch and validate the peer's next tenure update.
    pub async fn poll(&mut self) -> Result<Option<FollowedTenure>, SyncError> {
        let followed = self.follower.poll().await?;
        self.peer_info = Some(self.client.node_info().await?);
        self.pox_info = Some(self.client.pox_info().await?);
        Ok(followed)
    }

    /// Return the latest complete local view after at least one successful poll.
    #[must_use]
    pub fn view(&self) -> Option<NodeView> {
        Some(NodeView {
            node_info: self.peer_info.clone()?,
            pox_info: self.pox_info.clone()?,
            tenures: self.follower.history().to_vec(),
        })
    }
}

impl PeerPool {
    /// Follow the given peers, trusting all of them until one misbehaves.
    #[must_use]
    pub const fn new(peers: Vec<SyncClient>) -> Self {
        Self {
            peers,
            distrusted: BTreeSet::new(),
        }
    }

    /// The peers still worth asking.
    pub fn trusted(&self) -> impl Iterator<Item = (usize, &SyncClient)> {
        self.peers
            .iter()
            .enumerate()
            .filter(|(index, _)| !self.distrusted.contains(index))
    }

    /// Stop asking a peer that served something invalid.
    ///
    /// Serving a bad block is that peer's fault, not a reason to stop
    /// following the chain, so it is set aside rather than raised.
    pub fn distrust(&mut self, peer: usize) {
        self.distrusted.insert(peer);
    }

    /// Whether a peer is still being asked.
    #[must_use]
    pub fn is_trusted(&self, peer: usize) -> bool {
        !self.distrusted.contains(&peer) && peer < self.peers.len()
    }

    /// A peer by index, whether or not it is still trusted.
    #[must_use]
    pub fn peer(&self, peer: usize) -> Option<&SyncClient> {
        self.peers.get(peer)
    }

    /// Ask every trusted peer what its tip is.
    ///
    /// A peer that fails to answer is skipped for this round rather than
    /// distrusted: not answering is usually a restart, not a lie.
    pub async fn candidate_tips(&self) -> Vec<CandidateTip> {
        let mut candidates = Vec::new();
        for (index, client) in self.trusted() {
            let Ok(info) = client.tenure_info().await else {
                continue;
            };
            let Ok(tip) = client.block(info.tip_block_id).await else {
                continue;
            };
            candidates.push(CandidateTip {
                peer: index,
                info,
                header: tip.header,
            });
        }
        candidates
    }
}

impl TenureFollower {
    /// Start following a peer without assuming an initial tenure.
    #[must_use]
    pub const fn new(client: SyncClient) -> Self {
        Self {
            client,
            latest: None,
            history: Vec::new(),
        }
    }

    /// Return the latest tenure accepted from the peer.
    #[must_use]
    pub const fn latest(&self) -> Option<&TenureInfo> {
        self.latest.as_ref()
    }

    /// Return the validated tenure history retained by this follower.
    #[must_use]
    pub fn history(&self) -> &[FollowedTenure] {
        &self.history
    }

    /// Download the current tenure when the peer's tip has advanced.
    pub async fn poll(&mut self) -> Result<Option<FollowedTenure>, SyncError> {
        for _ in 0..3 {
            let requested_info = self.client.tenure_info().await?;
            if self
                .latest
                .as_ref()
                .is_some_and(|latest| latest.tip_block_id == requested_info.tip_block_id)
            {
                return Ok(None);
            }
            if self.latest.as_ref().is_some_and(|latest| {
                latest.tenure_start_block_id == requested_info.tenure_start_block_id
            }) {
                let tip = self.client.block(requested_info.tip_block_id).await?;
                let previous = self.history.last().ok_or(SyncError::Fork)?;
                let parent = previous.blocks.last().ok_or(SyncError::EmptyTenure)?;
                if tip.validate_successor(&parent.header).is_ok() {
                    let mut blocks = previous.blocks.clone();
                    blocks.push(tip);
                    let block_consensus_hash = blocks
                        .last()
                        .expect("the appended tip is present")
                        .header
                        .consensus_hash;
                    let sortition = self.client.sortition(block_consensus_hash).await?;
                    if !sortition.was_sortition {
                        return Err(SyncError::InvalidSortition);
                    }
                    let followed = FollowedTenure {
                        info: requested_info,
                        sortition,
                        blocks,
                    };
                    self.record(followed.clone());
                    return Ok(Some(followed));
                }
            }
            let mut blocks = self
                .client
                .tenure(requested_info.tenure_start_block_id, None)
                .await?;
            let info = self.client.tenure_info().await?;
            if info.tenure_start_block_id != requested_info.tenure_start_block_id {
                continue;
            }
            if blocks.last().map(NakamotoBlock::block_id) != Some(info.tip_block_id) {
                let tip = self.client.block(info.tip_block_id).await?;
                let parent = blocks.last().ok_or(SyncError::EmptyTenure)?;
                tip.validate_successor(&parent.header)
                    .map_err(SyncError::TenureLink)?;
                blocks.push(tip);
            }
            if let Some(latest) = &self.latest {
                validate_tenure_transition(latest, &info)?;
                if latest.tenure_start_block_id == info.tenure_start_block_id {
                    let previous = self.history.last().ok_or(SyncError::Fork)?;
                    if !blocks.starts_with(&previous.blocks) {
                        return Err(SyncError::Fork);
                    }
                } else if blocks
                    .first()
                    .is_none_or(|block| block.header.parent_block_id != latest.tip_block_id)
                {
                    return Err(SyncError::Fork);
                }
            }
            let block_consensus_hash = blocks
                .last()
                .map(|block| block.header.consensus_hash)
                .ok_or(SyncError::EmptyTenure)?;
            let sortition = self.client.sortition(block_consensus_hash).await?;
            if !sortition.was_sortition {
                return Err(SyncError::InvalidSortition);
            }
            let followed = FollowedTenure {
                info: info.clone(),
                sortition,
                blocks,
            };
            self.record(followed.clone());
            return Ok(Some(followed));
        }
        Err(SyncError::UnstableTip)
    }

    fn record(&mut self, followed: FollowedTenure) {
        if self.history.last().is_some_and(|latest| {
            latest.info.tenure_start_block_id == followed.info.tenure_start_block_id
        }) {
            self.history.pop();
        }
        self.latest = Some(followed.info.clone());
        self.history.push(followed);
    }
}

fn validate_tenure(
    start_block_id: StacksBlockId,
    blocks: &[NakamotoBlock],
) -> Result<(), SyncError> {
    let first = blocks.first().ok_or(SyncError::EmptyTenure)?;
    if first.block_id() != start_block_id {
        return Err(SyncError::TenureStart);
    }
    for pair in blocks.windows(2) {
        pair[1]
            .validate_successor(&pair[0].header)
            .map_err(SyncError::TenureLink)?;
    }
    Ok(())
}

fn validate_tenure_transition(previous: &TenureInfo, next: &TenureInfo) -> Result<(), SyncError> {
    if previous.tenure_start_block_id == next.tenure_start_block_id {
        return Ok(());
    }
    if next.parent_consensus_hash == previous.consensus_hash
        && next.parent_tenure_start_block_id == previous.tenure_start_block_id
    {
        Ok(())
    } else {
        Err(SyncError::Fork)
    }
}

#[derive(Deserialize)]
struct NodeInfoWire {
    burn_block_height: u64,
    stacks_tip_height: u64,
    stacks_tip: String,
    stacks_tip_consensus_hash: String,
    network_id: u32,
}

#[derive(Deserialize)]
struct TenureInfoWire {
    consensus_hash: String,
    tenure_start_block_id: String,
    parent_consensus_hash: String,
    parent_tenure_start_block_id: String,
    tip_block_id: String,
    tip_height: u64,
    reward_cycle: u64,
}

#[derive(Deserialize)]
struct PoxInfoWire {
    first_burnchain_block_height: u64,
    current_burnchain_block_height: u64,
    prepare_phase_block_length: u32,
    reward_phase_block_length: u32,
    reward_slots: u32,
    rejection_fraction: Option<u64>,
    #[serde(default)]
    contract_versions: Vec<PoxContractVersionWire>,
    #[serde(default)]
    epochs: Vec<PoxEpochWire>,
}

impl PoxInfoWire {
    /// The first height inside a named epoch, which is when the locks the
    /// preceding `PoX` contract holds expire.
    fn unlock_height_after(&self, epoch_id: &str) -> Option<u32> {
        self.epochs
            .iter()
            .find(|epoch| epoch.epoch_id == epoch_id)
            .and_then(|epoch| u32::try_from(epoch.start_height).ok())
            .map(|height| height.saturating_add(1))
    }
}

#[derive(Deserialize)]
struct PoxContractVersionWire {
    activation_burnchain_block_height: u32,
    contract_id: String,
}

#[derive(Deserialize)]
struct PoxEpochWire {
    epoch_id: String,
    start_height: u64,
}

#[derive(Deserialize)]
struct StackerSetResponseWire {
    stacker_set: StackerSetWire,
}

#[derive(Deserialize)]
struct StackerSetWire {
    pox_ustx_threshold: u128,
    #[serde(default)]
    sbtc_address: Option<Value>,
    signers: Vec<StackerWire>,
}

#[derive(Deserialize)]
struct StackerWire {
    signing_key: String,
    weight: u32,
}

#[derive(Deserialize)]
struct BlockUploadWire {
    accepted: bool,
    stacks_block_id: String,
}

#[derive(Deserialize)]
struct AccountWire {
    nonce: u64,
    /// The balance the account can spend now, as thirty-two hexadecimal digits.
    balance: String,
}

#[derive(Deserialize)]
struct SortitionInfoWire {
    burn_block_hash: String,
    burn_block_height: u64,
    burn_header_timestamp: u64,
    sortition_id: String,
    parent_sortition_id: String,
    consensus_hash: String,
    was_sortition: bool,
    miner_pk_hash160: Option<String>,
    stacks_parent_ch: Option<String>,
    last_sortition_ch: Option<String>,
    committed_block_hash: Option<String>,
    vrf_seed: Option<String>,
}

#[derive(Deserialize)]
struct ForkInfoWire {
    burn_block_height: u64,
    consensus_hash: String,
    was_sortition: bool,
    first_block_mined: Option<String>,
}

/// Wire tag for a mempool query that lists the transactions already known
/// (`core/mempool.rs`, `MemPoolSyncDataID::TxTags`).
const MEMPOOL_QUERY_TX_TAGS: u8 = 0x02;

/// Split a mempool page into its transactions and the identifier of the page
/// after it.
///
/// Nothing frames either: the transactions run back to back, and a peer with
/// more to send appends the next page's identifier, which is why a stream that
/// ends on a transaction boundary is the last page (`core/mempool.rs`,
/// `decode_tx_stream`).
fn decode_mempool_page(body: &[u8]) -> Result<(Vec<Transaction>, Option<Sha256Sum>), SyncError> {
    let mut stream = body;
    let mut transactions = Vec::new();
    while !stream.is_empty() {
        let Ok((transaction, consumed)) = Transaction::decode(stream) else {
            let page = stream.try_into().map_err(|_| SyncError::InvalidMempool)?;
            return Ok((transactions, Some(Sha256Sum::from_bytes(page))));
        };
        stream = stream.get(consumed..).ok_or(SyncError::InvalidMempool)?;
        transactions.push(transaction);
    }
    Ok((transactions, None))
}

fn parse_block_id(value: &str) -> Result<StacksBlockId, SyncError> {
    parse_hex(value).map(StacksBlockId::from_bytes)
}

fn parse_block_hash(value: &str) -> Result<BlockHeaderHash, SyncError> {
    parse_hex(value).map(BlockHeaderHash::from_bytes)
}

fn parse_consensus_hash(value: &str) -> Result<ConsensusHash, SyncError> {
    parse_hex(value).map(ConsensusHash::from_bytes)
}

fn parse_prefixed_block_hash(value: &str) -> Result<BlockHeaderHash, SyncError> {
    parse_prefixed_hex(value).map(BlockHeaderHash::from_bytes)
}

fn parse_prefixed_bitcoin_block_hash(value: &str) -> Result<BitcoinHeaderHash, SyncError> {
    parse_prefixed_hex(value).map(BitcoinHeaderHash::from_bytes)
}

fn parse_prefixed_sortition_id(value: &str) -> Result<SortitionId, SyncError> {
    parse_prefixed_hex(value).map(SortitionId::from_bytes)
}

fn parse_prefixed_consensus_hash(value: &str) -> Result<ConsensusHash, SyncError> {
    parse_prefixed_hex(value).map(ConsensusHash::from_bytes)
}

fn parse_prefixed_hash160(value: &str) -> Result<Hash160, SyncError> {
    parse_prefixed_hex(value).map(Hash160::from_bytes)
}

fn parse_stacker_set(value: StackerSetWire) -> Result<StackerSet, SyncError> {
    let sbtc_address = value
        .sbtc_address
        .as_ref()
        .map(parse_waterfall_address)
        .transpose()?;
    let mut signers = value
        .signers
        .into_iter()
        .map(|signer| {
            Ok(Signer {
                public_key: StacksPublicKey::from_bytes(&parse_hex::<33>(&signer.signing_key)?)?,
                weight: signer.weight,
            })
        })
        .collect::<Result<Vec<_>, SyncError>>()?;
    signers.sort_by_key(|signer| signer.public_key.to_bytes_compressed());
    Ok(StackerSet {
        pox_ustx_threshold: value.pox_ustx_threshold,
        sbtc_address,
        signer_set: SignerSet::new(signers)?,
    })
}

fn parse_waterfall_address(value: &Value) -> Result<PoxAddress, SyncError> {
    let address = value.get("Addr32").ok_or(SyncError::InvalidRewardSet)?;
    let (mainnet, address_type, bytes): (bool, String, [u8; 32]) =
        serde_json::from_value(address.clone()).map_err(|_| SyncError::InvalidRewardSet)?;
    if address_type != "P2TR" {
        return Err(SyncError::InvalidRewardSet);
    }
    Ok(PoxAddress::Addr32 {
        mainnet,
        address_type: PoxAddressType32::P2tr,
        bytes,
    })
}

fn parse_prefixed_hex<const LENGTH: usize>(value: &str) -> Result<[u8; LENGTH], SyncError> {
    parse_hex(value.strip_prefix("0x").ok_or(SyncError::InvalidHash)?)
}

fn parse_hex<const LENGTH: usize>(value: &str) -> Result<[u8; LENGTH], SyncError> {
    if value.len() != LENGTH * 2 {
        return Err(SyncError::InvalidHash);
    }
    let mut bytes = [0; LENGTH];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| SyncError::InvalidHash)?;
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, path::Path, time::Duration};

    use reqwest::Url;
    use tokio::time::{sleep, timeout};

    use super::{
        BlockUploadWire, StackerSetResponseWire, StackerSetWire, SyncClient, SyncError,
        parse_block_hash, parse_block_id, parse_consensus_hash, parse_prefixed_hash160,
        parse_stacker_set, validate_tenure, validate_tenure_transition,
    };
    use super::{Node, TenureFollower, TenureInfo};
    use nano_chainstate::{NakamotoBlock, TenureError};
    use nano_primitives::{ConsensusHash, StacksBlockId};

    #[test]
    fn node_starts_without_a_followed_tenure() {
        let client = SyncClient::new(Url::parse("http://127.0.0.1:20443/").expect("valid URL"))
            .expect("create sync client");

        assert!(Node::new(client).latest_tenure().is_none());
    }

    #[test]
    fn hashes_must_be_exact_lower_or_upper_hex() {
        assert!(parse_block_id("00").is_err());
        assert!(parse_consensus_hash("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").is_err());
        assert_eq!(
            parse_block_id("0000000000000000000000000000000000000000000000000000000000000000")
                .expect("valid block ID")
                .as_bytes(),
            &[0; 32]
        );
        assert_eq!(
            parse_block_hash("0000000000000000000000000000000000000000000000000000000000000000")
                .expect("valid block hash")
                .as_bytes(),
            &[0; 32]
        );
        assert_eq!(
            parse_consensus_hash("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
                .expect("valid uppercase consensus hash")
                .as_bytes(),
            &[0xaa; 20]
        );
        assert!(matches!(
            parse_consensus_hash("00"),
            Err(SyncError::InvalidHash)
        ));
        assert_eq!(
            parse_prefixed_hash160("0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
                .expect("valid prefixed hash")
                .as_bytes(),
            &[0xaa; 20]
        );
        assert!(parse_prefixed_hash160("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").is_err());
    }

    #[test]
    fn recorded_waterfall_stacker_set_parses_and_orders_signers() {
        #[derive(serde::Deserialize)]
        struct Fixture {
            stacker_set: StackerSetWire,
        }

        let fixture: Fixture =
            serde_json::from_slice(include_bytes!("../tests/waterfall-stacker-set.json"))
                .expect("parse recorded stacker set");
        let set = parse_stacker_set(fixture.stacker_set).expect("parse active stacker set");
        assert_eq!(set.pox_ustx_threshold, 14_666_666_667);
        assert_eq!(set.signer_set.weights(), vec![8, 8, 7, 7]);
        assert!(matches!(
            set.sbtc_address,
            Some(nano_address::PoxAddress::Addr32 { .. })
        ));
    }

    #[test]
    fn version_zero_stacker_set_parses_without_waterfall_address() {
        let response: StackerSetResponseWire = serde_json::from_str(
            r#"{"stacker_set":{"pox_ustx_threshold":50000000000,"reward_set_version":0,"rewarded_addresses":[],"signers":[{"signing_key":"02007311430123d4cad97f4f7e86e023b28143130a18099ecf094d36fef0f6135c","stacked_amt":2566784000000000,"weight":2},{"signing_key":"031a4d9f4903da97498945a4e01a5023a1d53bc96ad670bfe03adf8a06c52e6380","stacked_amt":2566784000000000,"weight":2},{"signing_key":"035249137286c077ccee65ecc43e724b9b9e5a588e3d7f51e3b62f9624c2a49e46","stacked_amt":2566784000000000,"weight":2}]}}"#,
        )
        .expect("parse version-zero response");
        let set = parse_stacker_set(response.stacker_set).expect("parse signer set");

        assert_eq!(set.sbtc_address, None);
        assert_eq!(set.signer_set.weights(), vec![2, 2, 2]);
    }

    #[test]
    fn block_upload_acknowledgement_requires_a_valid_block_id() {
        let acknowledgement: BlockUploadWire = serde_json::from_str(
            r#"{"accepted":true,"stacks_block_id":"0000000000000000000000000000000000000000000000000000000000000000"}"#,
        )
        .expect("parse upload acknowledgement");
        assert!(acknowledgement.accepted);
        assert_eq!(
            parse_block_id(&acknowledgement.stacks_block_id)
                .expect("valid block ID")
                .as_bytes(),
            &[0; 32]
        );
    }

    #[test]
    fn tenure_validation_requires_a_contiguous_stream() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../nano-conformance/fixtures/nakamoto/blocks");
        let mut paths = fs::read_dir(directory)
            .expect("read fixture blocks")
            .map(|entry| entry.expect("fixture block").path())
            .collect::<Vec<_>>();
        paths.sort();
        let blocks = paths
            .into_iter()
            .take(3)
            .map(|path| NakamotoBlock::decode(&fs::read(path).expect("read fixture block")))
            .collect::<Result<Vec<_>, _>>()
            .expect("decode fixture blocks");

        validate_tenure(blocks[0].block_id(), &blocks).expect("valid fixture tenure");
        assert!(matches!(
            validate_tenure(blocks[1].block_id(), &blocks),
            Err(SyncError::TenureStart)
        ));
        let mut invalid = blocks.clone();
        invalid[1].header.parent_block_id = StacksBlockId::from_bytes([0; 32]);
        assert!(matches!(
            validate_tenure(blocks[0].block_id(), &invalid),
            Err(SyncError::TenureLink(TenureError::ParentBlockId))
        ));
    }

    #[test]
    fn follower_requires_new_tenures_to_extend_the_previous_one() {
        let previous = tenure_info([1; 20], [1; 32], [2; 32]);
        let extension = tenure_info([1; 20], [1; 32], [3; 32]);
        let successor = TenureInfo {
            consensus_hash: ConsensusHash::from_bytes([4; 20]),
            tenure_start_block_id: StacksBlockId::from_bytes([4; 32]),
            parent_consensus_hash: previous.consensus_hash,
            parent_tenure_start_block_id: previous.tenure_start_block_id,
            tip_block_id: StacksBlockId::from_bytes([5; 32]),
            tip_height: 2,
            reward_cycle: 1,
        };
        let fork = tenure_info([6; 20], [6; 32], [7; 32]);

        assert!(validate_tenure_transition(&previous, &extension).is_ok());
        assert!(validate_tenure_transition(&previous, &successor).is_ok());
        assert!(matches!(
            validate_tenure_transition(&previous, &fork),
            Err(SyncError::Fork)
        ));
    }

    fn tenure_info(
        consensus_hash: [u8; 20],
        tenure_start_block_id: [u8; 32],
        tip_block_id: [u8; 32],
    ) -> TenureInfo {
        TenureInfo {
            consensus_hash: ConsensusHash::from_bytes(consensus_hash),
            tenure_start_block_id: StacksBlockId::from_bytes(tenure_start_block_id),
            parent_consensus_hash: ConsensusHash::from_bytes([0; 20]),
            parent_tenure_start_block_id: StacksBlockId::from_bytes([0; 32]),
            tip_block_id: StacksBlockId::from_bytes(tip_block_id),
            tip_height: 1,
            reward_cycle: 1,
        }
    }

    /// A mempool page is the transactions back to back with the next page
    /// identifier as its last thirty-two bytes, and nothing frames either.
    #[test]
    fn mempool_pages_split_into_transactions_and_a_page_identifier() {
        let transaction = nano_codec::Transaction::sign_standard(
            nano_codec::TransactionVersion::Testnet,
            0x8000_0000,
            nano_codec::AnchorMode::Any,
            &nano_crypto::StacksPrivateKey::from_seed(b"mempool"),
            3,
            180,
            nano_codec::TransactionPayloadData::TokenTransfer {
                recipient: nano_codec::Principal::Standard(
                    nano_address::StacksAddress::single_signature(
                        nano_primitives::Hash160::from_bytes([4; 20]),
                        false,
                    ),
                ),
                amount: 1_000,
                memo: [0; 34],
            },
        )
        .expect("sign a transfer");
        let mut page = transaction.encode();
        page.extend_from_slice(transaction.encode().as_slice());
        page.extend_from_slice(&[9; 32]);

        let (transactions, next) = super::decode_mempool_page(&page).expect("decode the page");
        assert_eq!(transactions.len(), 2);
        assert_eq!(transactions[0].txid(), transaction.txid());
        assert_eq!(next, Some(nano_primitives::Sha256Sum::from_bytes([9; 32])));

        // A stream that ends on a transaction boundary is the last page, which
        // is what a peer sends when its mempool fits in one.
        let (transactions, next) =
            super::decode_mempool_page(&transaction.encode()).expect("decode the last page");
        assert_eq!(transactions.len(), 1);
        assert_eq!(next, None);
        assert!(super::decode_mempool_page(&[0; 8]).is_err());
    }

    #[tokio::test]
    #[ignore = "requires a running Hacknet node on localhost"]
    async fn hacknet_tip_block_downloads_and_validates() {
        let client =
            SyncClient::new(Url::parse("http://127.0.0.1:20443/").expect("valid Hacknet URL"))
                .expect("create sync client");
        let tenure = client.tenure_info().await.expect("fetch tenure info");
        let block = client
            .block(tenure.tip_block_id)
            .await
            .expect("fetch tip block");

        assert_eq!(block.block_id(), tenure.tip_block_id);
    }

    #[tokio::test]
    #[ignore = "requires a running Hacknet node on localhost"]
    async fn hacknet_pox_calendar_is_available() {
        let client =
            SyncClient::new(Url::parse("http://127.0.0.1:20443/").expect("valid Hacknet URL"))
                .expect("create sync client");
        let calendar = client.pox_info().await.expect("fetch stacking calendar");

        assert!(calendar.bitcoin_height >= calendar.first_bitcoin_height);
        assert!(calendar.prepare_phase_length > 0);
        assert!(calendar.reward_phase_length > 0);
        assert!(calendar.reward_slots > 0);
        assert_eq!(calendar.bitcoin_context().height, calendar.bitcoin_height);
    }

    #[tokio::test]
    #[ignore = "requires a running Hacknet node on localhost"]
    async fn hacknet_tenure_downloads_and_links() {
        let client =
            SyncClient::new(Url::parse("http://127.0.0.1:20443/").expect("valid Hacknet URL"))
                .expect("create sync client");
        let tenure = client.tenure_info().await.expect("fetch tenure info");
        let blocks = client
            .tenure(tenure.tenure_start_block_id, None)
            .await
            .expect("fetch tenure");

        assert!(!blocks.is_empty());
        assert_eq!(
            blocks.first().expect("non-empty tenure").block_id(),
            tenure.tenure_start_block_id
        );
        for pair in blocks.windows(2) {
            pair[1]
                .validate_successor(&pair[0].header)
                .expect("tenure blocks link");
        }
    }

    #[tokio::test]
    #[ignore = "requires a running Hacknet node on localhost"]
    async fn hacknet_follower_retains_an_authenticated_tenure() {
        let client =
            SyncClient::new(Url::parse("http://127.0.0.1:20443/").expect("valid Hacknet URL"))
                .expect("create sync client");
        let mut follower = TenureFollower::new(client);
        let tenure = follower
            .poll()
            .await
            .expect("follow current tenure")
            .expect("initial tenure");

        assert_eq!(follower.history(), std::slice::from_ref(&tenure));
        assert_eq!(
            tenure.sortition.consensus_hash,
            tenure
                .blocks
                .last()
                .expect("non-empty tenure")
                .header
                .consensus_hash
        );
        assert!(tenure.sortition.was_sortition);
    }

    #[tokio::test]
    #[ignore = "requires a running Hacknet node on localhost"]
    async fn hacknet_follower_spans_prepare_phase_and_reward_cycle_rollover() {
        let client =
            SyncClient::new(Url::parse("http://127.0.0.1:20443/").expect("valid Hacknet URL"))
                .expect("create sync client");
        let mut follower = TenureFollower::new(client.clone());
        let initial = follower
            .poll()
            .await
            .expect("follow current tenure")
            .expect("initial tenure");
        let mut reward_cycles = BTreeSet::from([initial.info.reward_cycle]);
        let mut saw_prepare_phase = false;

        timeout(Duration::from_secs(75), async {
            loop {
                let calendar = client.pox_info().await.expect("fetch stacking calendar");
                saw_prepare_phase |= is_prepare_phase(&calendar);
                if let Some(tenure) = follower.poll().await.expect("follow tenure update") {
                    reward_cycles.insert(tenure.info.reward_cycle);
                }
                if saw_prepare_phase && reward_cycles.len() >= 2 {
                    break;
                }
                sleep(Duration::from_millis(500)).await;
            }
        })
        .await
        .expect("span a prepare phase and reward-cycle rollover");

        assert!(saw_prepare_phase);
        assert!(reward_cycles.len() >= 2);
        assert!(follower.history().len() >= 2);
    }

    #[tokio::test]
    #[ignore = "requires a running Hacknet node on localhost"]
    async fn hacknet_sortition_authenticates_the_current_tenure() {
        let client =
            SyncClient::new(Url::parse("http://127.0.0.1:20443/").expect("valid Hacknet URL"))
                .expect("create sync client");
        let tenure = client.tenure_info().await.expect("fetch tenure info");
        let block = client
            .block(tenure.tip_block_id)
            .await
            .expect("fetch tip block");
        let sortition = client
            .sortition(block.header.consensus_hash)
            .await
            .expect("fetch tenure sortition");

        assert_eq!(sortition.consensus_hash, block.header.consensus_hash);
        assert!(sortition.was_sortition);
        assert!(sortition.miner_public_key_hash.is_some());
    }

    fn is_prepare_phase(calendar: &super::PoxInfo) -> bool {
        let cycle_length = u64::from(calendar.reward_phase_length)
            .checked_add(u64::from(calendar.prepare_phase_length))
            .expect("PoX cycle length fits in u64");
        let position = calendar
            .bitcoin_height
            .checked_sub(calendar.first_bitcoin_height)
            .expect("Bitcoin height does not predate the first height")
            % cycle_length;
        position >= u64::from(calendar.reward_phase_length)
    }
}
