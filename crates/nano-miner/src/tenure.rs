//! Assembly of the first block of a tenure won on Bitcoin.

use std::fmt;

use nano_address::StacksAddress;
use nano_bitcoin::{BitcoinOperationKind, BitcoinRpcSource, BitcoinRpcSourceError, BitcoinSource};
use nano_chainstate::{NakamotoBlock, NakamotoBlockHeader};
use nano_codec::{
    AnchorMode, CodecError, TenureChangeCause, TenureChangePayload, Transaction,
    TransactionPayloadData, TransactionVersion, transaction_merkle_root,
};
use nano_crypto::{MessageSignature, StacksPrivateKey, Vrf, VrfError, VrfPrivateKey};
use nano_primitives::{BitVec, BitcoinHeaderHash, TrieHash, hash160};
use nano_sortition::SortitionHash;
use nano_sync::{SortitionInfo, SyncClient, SyncError};
use serde::{Deserialize, Serialize};

/// Under waterfall `PoX` a block treats exactly one payout address.
const WATERFALL_POX_TREATMENT_LEN: u16 = 1;

/// Bitcoin block heights are taken modulo this value to pin a commitment to one block.
const BITCOIN_BLOCK_MINED_AT_MODULUS: u64 = 5;

/// The Bitcoin facts a tenure-start block must commit to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BitcoinTenureView {
    /// Cumulative Bitcoin spent through the sortition this miner won.
    pub total_burn: u64,
    /// The sortition hash the coinbase's VRF proof is taken over.
    pub sortition_hash: [u8; 32],
}

#[derive(Debug)]
pub enum TenureStartError {
    Sync(SyncError),
    Codec(CodecError),
    Vrf(VrfError),
    Bitcoin(BitcoinRpcSourceError),
    NotWon,
    EmptyTenure,
}

impl fmt::Display for TenureStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sync(error) => write!(formatter, "node query failed: {error}"),
            Self::Codec(error) => write!(formatter, "transaction encoding failed: {error}"),
            Self::Vrf(error) => write!(formatter, "VRF proof failed: {error}"),
            Self::Bitcoin(error) => write!(formatter, "Bitcoin query failed: {error}"),
            Self::NotWon => formatter.write_str("the sortition was not won by this miner"),
            Self::EmptyTenure => formatter.write_str("the parent tenure has no blocks"),
        }
    }
}

impl std::error::Error for TenureStartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sync(error) => Some(error),
            Self::Codec(error) => Some(error),
            Self::Bitcoin(error) => Some(error),
            Self::NotWon | Self::EmptyTenure | Self::Vrf(_) => None,
        }
    }
}

impl From<SyncError> for TenureStartError {
    fn from(error: SyncError) -> Self {
        Self::Sync(error)
    }
}

impl From<CodecError> for TenureStartError {
    fn from(error: CodecError) -> Self {
        Self::Codec(error)
    }
}

impl From<VrfError> for TenureStartError {
    fn from(error: VrfError) -> Self {
        Self::Vrf(error)
    }
}

impl From<BitcoinRpcSourceError> for TenureStartError {
    fn from(error: BitcoinRpcSourceError) -> Self {
        Self::Bitcoin(error)
    }
}

/// Hexadecimal encoding for the persisted sortition-hash checkpoint.
mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<[u8; 32], D::Error> {
        let value = String::deserialize(deserializer)?;
        let bytes = hex::decode(&value).map_err(D::Error::custom)?;
        bytes
            .try_into()
            .map_err(|_| D::Error::custom("expected 32 bytes"))
    }
}

/// A resumable point in the chain of sortition hashes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct SortitionHashPoint {
    pub bitcoin_height: u64,
    #[serde(with = "hex_bytes")]
    pub sortition_hash: [u8; 32],
}

impl SortitionHashPoint {
    /// The chain's starting point, which is the first Bitcoin block a node indexes.
    #[must_use]
    pub const fn genesis(first_bitcoin_height: u64) -> Self {
        Self {
            bitcoin_height: first_bitcoin_height,
            sortition_hash: *SortitionHash::initial().as_bytes(),
        }
    }
}

/// Extend the sortition-hash chain up to one Bitcoin height.
///
/// Each block mixes its own header hash, and a block with a sortition then
/// mixes the winning commitment's seed. Both come from the peer's record of
/// which blocks produced a sortition.
pub async fn extend_sortition_hash(
    node: &SyncClient,
    bitcoin: &BitcoinRpcSource,
    from: SortitionHashPoint,
    to_bitcoin_height: u64,
) -> Result<SortitionHashPoint, TenureStartError> {
    let mut hash = SortitionHash::from_bytes(from.sortition_hash);
    for height in from.bitcoin_height.saturating_add(1)..=to_bitcoin_height {
        hash = hash.mix_bitcoin_header(BitcoinHeaderHash::from_bytes(
            bitcoin.block_hash_at(height)?,
        ));
        let sortition = node.sortition_at_height(height).await?;
        if let Some(seed) = sortition.vrf_seed {
            hash = hash.mix_vrf_seed(seed);
        }
    }
    Ok(SortitionHashPoint {
        bitcoin_height: to_bitcoin_height,
        sortition_hash: *hash.as_bytes(),
    })
}

/// Accumulate the Bitcoin committed between a tenure's sortition and a later one.
///
/// Only commitments that target the block they landed in are counted, matching
/// the totals stacks-core carries forward in each tenure-start block header.
pub fn total_burn_after<S>(
    bitcoin: &mut S,
    from_total_burn: u64,
    sortition_heights: &[u64],
) -> Result<u64, TenureStartError>
where
    S: BitcoinSource,
    TenureStartError: From<S::Error>,
{
    let mut total = from_total_burn;
    for height in sortition_heights {
        let expected_modulus = u8::try_from(
            height.checked_sub(1).ok_or(TenureStartError::EmptyTenure)?
                % BITCOIN_BLOCK_MINED_AT_MODULUS,
        )
        .expect("a Bitcoin modulus below five fits in a byte");
        for operation in bitcoin.block_at(*height)?.operations {
            let BitcoinOperationKind::LeaderBlockCommit { parent_modulus, .. } = operation.kind
            else {
                continue;
            };
            if parent_modulus != expected_modulus {
                continue;
            }
            let paid = operation
                .outputs
                .first()
                .map_or(0, |output| output.amount_sats);
            total = total
                .checked_add(paid)
                .ok_or(TenureStartError::EmptyTenure)?;
        }
    }
    Ok(total)
}

/// Build the unexecuted tenure-start block for a sortition this miner won.
///
/// The returned block still needs execution to seal its state root and the
/// miner's signature, which `nano_chainstate` fills in when it assembles.
pub async fn build_tenure_start_block(
    node: &SyncClient,
    won: &SortitionInfo,
    view: BitcoinTenureView,
    chain_id: u32,
    miner_key: &StacksPrivateKey,
    vrf_key: &VrfPrivateKey,
    timestamp: u64,
) -> Result<NakamotoBlock, TenureStartError> {
    if !won.was_sortition
        || won.miner_public_key_hash != Some(hash160(&miner_key.public_key().to_bytes_compressed()))
    {
        return Err(TenureStartError::NotWon);
    }
    let parent_tenure = node.tenure_info().await?;
    let parent = node.block(parent_tenure.tip_block_id).await?;
    let parent_tenure_start = node.block(parent_tenure.tenure_start_block_id).await?;
    let previous_tenure_blocks = parent
        .header
        .chain_length
        .checked_sub(parent_tenure_start.header.chain_length)
        .and_then(|blocks| blocks.checked_add(1))
        .ok_or(TenureStartError::EmptyTenure)?;

    let miner_hash = hash160(&miner_key.public_key().to_bytes_compressed());
    let nonce = node
        .account_nonce(StacksAddress::single_signature(miner_hash, false))
        .await?;
    let transactions = vec![
        Transaction::sign_standard(
            TransactionVersion::Testnet,
            chain_id,
            AnchorMode::OnChainOnly,
            miner_key,
            nonce,
            0,
            TransactionPayloadData::TenureChange(TenureChangePayload {
                tenure_consensus_hash: won.consensus_hash,
                previous_tenure_consensus_hash: parent_tenure.consensus_hash,
                bitcoin_view_consensus_hash: won.consensus_hash,
                previous_tenure_end: parent_tenure.tip_block_id,
                previous_tenure_blocks: u32::try_from(previous_tenure_blocks)
                    .map_err(|_| TenureStartError::EmptyTenure)?,
                cause: TenureChangeCause::BlockFound,
                public_key_hash: miner_hash,
            }),
        )?,
        Transaction::sign_standard(
            TransactionVersion::Testnet,
            chain_id,
            AnchorMode::OnChainOnly,
            miner_key,
            nonce.saturating_add(1),
            0,
            TransactionPayloadData::NakamotoCoinbase {
                payload: [0; 32],
                recipient: None,
                vrf_proof: Vrf::prove(vrf_key, &view.sortition_hash)?.to_bytes(),
            },
        )?,
    ];

    Ok(NakamotoBlock {
        header: NakamotoBlockHeader {
            version: 0,
            chain_length: parent.header.chain_length.saturating_add(1),
            bitcoin_spent: view.total_burn,
            consensus_hash: won.consensus_hash,
            parent_block_id: parent_tenure.tip_block_id,
            transaction_merkle_root: transaction_merkle_root(&transactions),
            state_index_root: TrieHash::from_bytes([0; 32]),
            timestamp,
            miner_signature: MessageSignature::from_bytes([0; 65]),
            signer_signatures: Vec::new(),
            pox_treatment: BitVec::ones(WATERFALL_POX_TREATMENT_LEN)
                .expect("a one-bit vector is valid"),
            problematic_transactions: Vec::new(),
        },
        transactions,
    })
}

#[cfg(test)]
mod tests {
    use nano_bitcoin::{BitcoinOperationKind, BitcoinRpcSource};
    use nano_crypto::{Vrf, VrfPublicKey};
    use nano_sync::SyncClient;
    use reqwest::Url;

    use super::{SortitionHashPoint, extend_sortition_hash, total_burn_after};

    fn hacknet() -> (SyncClient, BitcoinRpcSource) {
        (
            SyncClient::new(Url::parse("http://127.0.0.1:20443/").expect("valid Hacknet URL"))
                .expect("create sync client"),
            BitcoinRpcSource::new("http://127.0.0.1:18443", "hacknet", "hacknet", *b"T3")
                .expect("connect to Hacknet Bitcoin Core"),
        )
    }

    /// Every tenure-start block header carries the total Bitcoin committed so far,
    /// so consecutive tenures are a live oracle for the accumulation rule.
    #[tokio::test]
    #[ignore = "requires a running Hacknet node and Bitcoin Core on localhost"]
    async fn hacknet_total_burn_continues_the_parent_tenure() {
        let (node, mut bitcoin) = hacknet();
        let tenure = node.tenure_info().await.expect("fetch tenure info");
        let parent_start = node
            .block(tenure.parent_tenure_start_block_id)
            .await
            .expect("fetch the parent tenure's start block");
        let start = node
            .block(tenure.tenure_start_block_id)
            .await
            .expect("fetch the tenure's start block");
        let parent_height = node
            .sortition(tenure.parent_consensus_hash)
            .await
            .expect("fetch the parent sortition")
            .bitcoin_height;
        let height = node
            .sortition(tenure.consensus_hash)
            .await
            .expect("fetch the tenure's sortition")
            .bitcoin_height;

        let mut sortition_heights = Vec::new();
        for candidate in parent_height + 1..=height {
            if node
                .sortition_at_height(candidate)
                .await
                .expect("fetch sortition by height")
                .was_sortition
            {
                sortition_heights.push(candidate);
            }
        }

        assert_eq!(
            total_burn_after(
                &mut bitcoin,
                parent_start.header.bitcoin_spent,
                &sortition_heights
            )
            .expect("accumulate Bitcoin committed"),
            start.header.bitcoin_spent
        );
    }

    /// The winning miner proved its coinbase VRF over the sortition hash of the block
    /// it won, so an accepted tenure-start block verifies the whole derived chain.
    #[tokio::test]
    #[ignore = "requires a running Hacknet node and Bitcoin Core on localhost"]
    async fn hacknet_sortition_hash_verifies_the_winning_vrf_proof() {
        let (node, mut bitcoin) = hacknet();
        let calendar = node.pox_info().await.expect("fetch stacking calendar");
        let tenure = node.tenure_info().await.expect("fetch tenure info");
        let sortition = node
            .sortition(tenure.consensus_hash)
            .await
            .expect("fetch the tenure's sortition");
        let start = node
            .block(tenure.tenure_start_block_id)
            .await
            .expect("fetch the tenure's start block");
        let proof = start
            .transactions
            .iter()
            .find_map(|transaction| match transaction.payload().data() {
                nano_codec::TransactionPayloadData::NakamotoCoinbase { vrf_proof, .. } => {
                    Some(*vrf_proof)
                }
                _ => None,
            })
            .expect("tenure-start block carries a VRF proof");

        let winner = bitcoin
            .block_at(sortition.bitcoin_height)
            .expect("read the winning Bitcoin block")
            .operations
            .into_iter()
            .find_map(|operation| match operation.kind {
                BitcoinOperationKind::LeaderBlockCommit {
                    block_header_hash,
                    key_block_height,
                    key_transaction_index,
                    ..
                } if block_header_hash
                    == *sortition
                        .committed_block_hash
                        .expect("the sortition has a winner")
                        .as_bytes() =>
                {
                    Some((key_block_height, key_transaction_index))
                }
                _ => None,
            })
            .expect("the winning commitment is on Bitcoin");
        let vrf_public_key = bitcoin
            .block_at(u64::from(winner.0))
            .expect("read the leader key's Bitcoin block")
            .operations
            .into_iter()
            .find_map(|operation| match operation.kind {
                BitcoinOperationKind::LeaderKeyRegistration { vrf_public_key, .. }
                    if operation.transaction_index == u32::from(winner.1) =>
                {
                    Some(vrf_public_key)
                }
                _ => None,
            })
            .expect("the winner registered a leader key");

        let point = extend_sortition_hash(
            &node,
            &bitcoin,
            SortitionHashPoint::genesis(calendar.first_bitcoin_height),
            sortition.bitcoin_height,
        )
        .await
        .expect("derive the sortition hash");

        assert!(
            Vrf::verify(
                &VrfPublicKey::from_bytes(vrf_public_key).expect("valid VRF key"),
                &nano_crypto::VrfProof::from_bytes(&proof).expect("valid VRF proof"),
                &point.sortition_hash,
            )
            .expect("verify the winning VRF proof")
        );
    }
}
