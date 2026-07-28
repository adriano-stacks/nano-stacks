#![forbid(unsafe_code)]

use std::{fmt, time::Duration};

use nano_address::{PoxAddress, PoxAddressType32};
use nano_chainstate::{
    BitcoinBlockContext, NakamotoBlock, NakamotoCodecError, Signer, SignerSet, SignerSetError,
    TenureError,
};
use nano_crypto::{CryptoError, StacksPublicKey};
use nano_primitives::{
    BitcoinHeaderHash, BlockHeaderHash, ConsensusHash, Hash160, SortitionId, StacksBlockId,
};
use reqwest::{Client, Url, header::CONTENT_TYPE};
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct SyncClient {
    client: Client,
    base_url: Url,
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
}

/// The active waterfall payout address and threshold signer set for one reward cycle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackerSet {
    pub pox_ustx_threshold: u128,
    pub sbtc_address: PoxAddress,
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
            v1_unlock_height: u32::MAX,
            v2_unlock_height: u32::MAX,
            v3_unlock_height: u32::MAX,
            pox_5_activation_height: self.pox_5_activation_height.unwrap_or(u32::MAX),
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
            | Self::Fork => None,
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
        })
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
        if !upload.accepted || upload.block_id != block.block_id() {
            return Err(SyncError::BlockUploadRejected);
        }
        Ok(upload)
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
}

#[derive(Deserialize)]
struct PoxContractVersionWire {
    activation_burnchain_block_height: u32,
    contract_id: String,
}

#[derive(Deserialize)]
struct StackerSetResponseWire {
    stacker_set: StackerSetWire,
}

#[derive(Deserialize)]
struct StackerSetWire {
    pox_ustx_threshold: u128,
    sbtc_address: Value,
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
    let sbtc_address = parse_waterfall_address(&value.sbtc_address)?;
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
        BlockUploadWire, StackerSetWire, SyncClient, SyncError, parse_block_hash, parse_block_id,
        parse_consensus_hash, parse_prefixed_hash160, parse_stacker_set, validate_tenure,
        validate_tenure_transition,
    };
    use super::{TenureFollower, TenureInfo};
    use nano_chainstate::{NakamotoBlock, TenureError};
    use nano_primitives::{ConsensusHash, StacksBlockId};

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

        let fixture: Fixture = serde_json::from_slice(include_bytes!(
            "../../nano-conformance/fixtures/stacker_set/cycle-18.json"
        ))
        .expect("parse recorded stacker set");
        let set = parse_stacker_set(fixture.stacker_set).expect("parse active stacker set");
        assert_eq!(set.pox_ustx_threshold, 11_000_000_000);
        assert_eq!(set.signer_set.weights(), vec![10, 10, 10]);
        assert!(matches!(
            set.sbtc_address,
            nano_address::PoxAddress::Addr32 { .. }
        ));
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
