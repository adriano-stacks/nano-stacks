//! Assembly of the first block of a tenure won on Bitcoin.

use std::fmt;

use nano_bitcoin::{BitcoinOperationKind, BitcoinRpcSourceError, BitcoinSource};
use nano_chainstate::{NakamotoBlock, NakamotoBlockHeader};
use nano_codec::{
    AnchorMode, CodecError, TenureChangeCause, TenureChangePayload, Transaction,
    TransactionPayloadData, TransactionVersion, transaction_merkle_root,
};
use nano_crypto::{MessageSignature, StacksPrivateKey, Vrf, VrfError, VrfPrivateKey};
use nano_primitives::{
    BitVec, BitcoinHeaderHash, ConsensusHash, Network, StacksBlockId, TrieHash, hash160,
};
use nano_sortition::SortitionHash;
use nano_sync::SortitionInfo;
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

/// Locally executed parent state needed to build a tenure start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParentTenure {
    pub tip: TenureTip,
    pub start_block_id: StacksBlockId,
    pub blocks: u32,
    pub miner_nonce: u64,
}

#[derive(Debug)]
pub enum TenureStartError {
    Codec(CodecError),
    Vrf(VrfError),
    Bitcoin(BitcoinRpcSourceError),
    NotWon,
    EmptyTenure,
    SortitionHashGap { expected: u64, found: u64 },
}

impl fmt::Display for TenureStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Codec(error) => write!(formatter, "transaction encoding failed: {error}"),
            Self::Vrf(error) => write!(formatter, "VRF proof failed: {error}"),
            Self::Bitcoin(error) => write!(formatter, "Bitcoin query failed: {error}"),
            Self::NotWon => formatter.write_str("the sortition was not won by this miner"),
            Self::EmptyTenure => formatter.write_str("the parent tenure has no blocks"),
            Self::SortitionHashGap { expected, found } => write!(
                formatter,
                "sortition-hash input jumps from expected burn {expected} to {found}"
            ),
        }
    }
}

impl std::error::Error for TenureStartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Codec(error) => Some(error),
            Self::Bitcoin(error) => Some(error),
            Self::NotWon | Self::EmptyTenure | Self::SortitionHashGap { .. } | Self::Vrf(_) => None,
        }
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

/// One locally derived burn block mixed into the sortition-hash chain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SortitionHashBlock {
    pub bitcoin_height: u64,
    pub bitcoin_header_hash: BitcoinHeaderHash,
    pub winner_vrf_seed: Option<[u8; 32]>,
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
/// mixes the locally derived winning commitment's seed.
pub fn extend_sortition_hash(
    from: SortitionHashPoint,
    blocks: impl IntoIterator<Item = SortitionHashBlock>,
) -> Result<SortitionHashPoint, TenureStartError> {
    let mut hash = SortitionHash::from_bytes(from.sortition_hash);
    let mut bitcoin_height = from.bitcoin_height;
    for block in blocks {
        let expected = bitcoin_height.saturating_add(1);
        if block.bitcoin_height != expected {
            return Err(TenureStartError::SortitionHashGap {
                expected,
                found: block.bitcoin_height,
            });
        }
        hash = hash.mix_bitcoin_header(block.bitcoin_header_hash);
        if let Some(seed) = block.winner_vrf_seed {
            hash = hash.mix_vrf_seed(seed);
        }
        bitcoin_height = block.bitcoin_height;
    }
    Ok(SortitionHashPoint {
        bitcoin_height,
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

/// Header version an epoch-4 block must carry, which also puts the
/// miner-flagged transaction list in the hash and signature preimages
/// (`nakamoto/mod.rs`, `expected_version_for_epoch`).
pub const NAKAMOTO_BLOCK_VERSION_EPOCH_4: u8 = 1;

/// Build the unexecuted tenure-start block for a sortition this miner won.
///
/// The returned block still needs execution to seal its state root and the
/// miner's signature, which `nano_chainstate` fills in when it assembles.
pub fn build_tenure_start_block(
    won: &SortitionInfo,
    parent_tenure: ParentTenure,
    view: BitcoinTenureView,
    network: Network,
    miner_key: &StacksPrivateKey,
    vrf_key: &VrfPrivateKey,
    timestamp: u64,
) -> Result<NakamotoBlock, TenureStartError> {
    if !won.was_sortition
        || won.miner_public_key_hash != Some(hash160(&miner_key.public_key().to_bytes_compressed()))
    {
        return Err(TenureStartError::NotWon);
    }
    if parent_tenure.blocks == 0 {
        return Err(TenureStartError::EmptyTenure);
    }
    let miner_hash = hash160(&miner_key.public_key().to_bytes_compressed());
    let nonce = parent_tenure.miner_nonce;
    let transactions = vec![
        Transaction::sign_standard(
            TransactionVersion::for_network(network),
            network.chain_id(),
            AnchorMode::OnChainOnly,
            miner_key,
            nonce,
            0,
            TransactionPayloadData::TenureChange(TenureChangePayload {
                tenure_consensus_hash: won.consensus_hash,
                previous_tenure_consensus_hash: parent_tenure.tip.consensus_hash,
                bitcoin_view_consensus_hash: won.consensus_hash,
                previous_tenure_end: parent_tenure.tip.block_id,
                previous_tenure_blocks: parent_tenure.blocks,
                cause: TenureChangeCause::BlockFound,
                public_key_hash: miner_hash,
            }),
        )?,
        Transaction::sign_standard(
            TransactionVersion::for_network(network),
            network.chain_id(),
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
            version: NAKAMOTO_BLOCK_VERSION_EPOCH_4,
            chain_length: parent_tenure.tip.height.saturating_add(1),
            bitcoin_spent: view.total_burn,
            consensus_hash: won.consensus_hash,
            parent_block_id: parent_tenure.tip.block_id,
            transaction_merkle_root: transaction_merkle_root(&transactions),
            state_index_root: TrieHash::from_bytes([0; 32]),
            timestamp: timestamp.max(parent_tenure.tip.timestamp.saturating_add(1)),
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
    use nano_codec::TransactionPayloadData;
    use nano_crypto::{StacksPrivateKey, Vrf, VrfPrivateKey, VrfPublicKey};
    use nano_primitives::{
        BitcoinHeaderHash, BlockHeaderHash, ConsensusHash, Network, SortitionId, StacksBlockId,
        hash160,
    };
    use nano_sync::{SortitionInfo, SyncClient};
    use reqwest::Url;

    use super::{
        BitcoinTenureView, ParentTenure, SortitionHashBlock, SortitionHashPoint, TenureStartError,
        TenureTip, build_tenure_start_block, extend_sortition_hash, total_burn_after,
    };

    fn hacknet() -> (SyncClient, BitcoinRpcSource) {
        (
            SyncClient::new(Url::parse("http://127.0.0.1:20443/").expect("valid Hacknet URL"))
                .expect("create sync client"),
            BitcoinRpcSource::new("http://127.0.0.1:18443", "hacknet", "hacknet", *b"T3")
                .expect("connect to Hacknet Bitcoin Core"),
        )
    }

    fn winning_vrf_public_key(
        bitcoin: &mut BitcoinRpcSource,
        sortition: &SortitionInfo,
    ) -> [u8; 32] {
        let miner = *sortition
            .miner_public_key_hash
            .expect("the sortition names its winning miner")
            .as_bytes();
        let committed = *sortition
            .committed_block_hash
            .expect("the sortition has a winner")
            .as_bytes();

        for operation in bitcoin
            .block_at(sortition.bitcoin_height)
            .expect("read the winning Bitcoin block")
            .operations
        {
            let BitcoinOperationKind::LeaderBlockCommit {
                block_header_hash,
                key_block_height,
                key_transaction_index,
                ..
            } = operation.kind
            else {
                continue;
            };
            if block_header_hash != committed {
                continue;
            }

            let registration = bitcoin
                .block_at(u64::from(key_block_height))
                .expect("read a leader key's Bitcoin block")
                .operations
                .into_iter()
                .find_map(|operation| match operation.kind {
                    BitcoinOperationKind::LeaderKeyRegistration {
                        vrf_public_key,
                        block_signing_key_hash: Some(signing_key_hash),
                        ..
                    } if operation.transaction_index == u32::from(key_transaction_index) => {
                        Some((vrf_public_key, signing_key_hash))
                    }
                    _ => None,
                })
                .expect("the commitment references a registered leader key");
            if registration.1 == miner {
                return registration.0;
            }
        }

        panic!("the winning miner has no commitment in its Bitcoin block")
    }

    #[test]
    fn tenure_start_construction_requires_an_explicit_local_parent() {
        let miner = StacksPrivateKey::from_seed(b"local miner");
        let miner_hash = hash160(&miner.public_key().to_bytes_compressed());
        let won = SortitionInfo {
            bitcoin_block_hash: BitcoinHeaderHash::from_bytes([1; 32]),
            bitcoin_height: 10,
            bitcoin_timestamp: 11,
            sortition_id: SortitionId::from_bytes([2; 32]),
            parent_sortition_id: SortitionId::from_bytes([3; 32]),
            consensus_hash: ConsensusHash::from_bytes([4; 20]),
            was_sortition: true,
            miner_public_key_hash: Some(miner_hash),
            stacks_parent_consensus_hash: Some(ConsensusHash::from_bytes([5; 20])),
            last_sortition_consensus_hash: Some(ConsensusHash::from_bytes([5; 20])),
            committed_block_hash: Some(BlockHeaderHash::from_bytes([6; 32])),
            vrf_seed: Some([7; 32]),
            mining_competition: None,
        };
        let parent = ParentTenure {
            tip: TenureTip {
                consensus_hash: ConsensusHash::from_bytes([5; 20]),
                block_id: StacksBlockId::from_bytes([8; 32]),
                height: 100,
                bitcoin_spent: 9,
                timestamp: 10,
            },
            start_block_id: StacksBlockId::from_bytes([9; 32]),
            blocks: 7,
            miner_nonce: 12,
        };
        let view = BitcoinTenureView {
            total_burn: 13,
            sortition_hash: [14; 32],
        };
        let vrf = VrfPrivateKey::from_bytes([15; 32]);

        let mut missing = parent;
        missing.blocks = 0;
        assert!(matches!(
            build_tenure_start_block(&won, missing, view, Network::TESTNET, &miner, &vrf, 20,),
            Err(TenureStartError::EmptyTenure)
        ));

        let block =
            build_tenure_start_block(&won, parent, view, Network::TESTNET, &miner, &vrf, 10)
                .expect("build from local parent");
        assert_eq!(block.header.parent_block_id, parent.tip.block_id);
        assert_eq!(block.header.timestamp, parent.tip.timestamp + 1);
        let TransactionPayloadData::TenureChange(change) = block.transactions[0].payload().data()
        else {
            panic!("first transaction is the tenure change");
        };
        assert_eq!(change.previous_tenure_end, parent.tip.block_id);
        assert_eq!(change.previous_tenure_blocks, parent.blocks);
        assert_eq!(
            change.previous_tenure_consensus_hash,
            parent.tip.consensus_hash
        );
    }

    #[test]
    fn sortition_hash_extension_refuses_a_missing_local_burn_block() {
        let error = extend_sortition_hash(
            SortitionHashPoint::genesis(10),
            [SortitionHashBlock {
                bitcoin_height: 12,
                bitcoin_header_hash: BitcoinHeaderHash::from_bytes([1; 32]),
                winner_vrf_seed: Some([2; 32]),
            }],
        )
        .expect_err("a missing burn block cannot be skipped");
        assert!(matches!(
            error,
            TenureStartError::SortitionHashGap {
                expected: 11,
                found: 12
            }
        ));
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

        let vrf_public_key = winning_vrf_public_key(&mut bitcoin, &sortition);

        let mut blocks = Vec::new();
        for bitcoin_height in
            calendar.first_bitcoin_height.saturating_add(1)..=sortition.bitcoin_height
        {
            let entry = node
                .sortition_at_height(bitcoin_height)
                .await
                .expect("fetch sortition by height");
            blocks.push(SortitionHashBlock {
                bitcoin_height,
                bitcoin_header_hash: BitcoinHeaderHash::from_bytes(
                    bitcoin
                        .block_hash_at(bitcoin_height)
                        .expect("read Bitcoin block hash"),
                ),
                winner_vrf_seed: entry.vrf_seed,
            });
        }
        let point = extend_sortition_hash(
            SortitionHashPoint::genesis(calendar.first_bitcoin_height),
            blocks,
        )
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

/// Build the unexecuted next block of a tenure this miner already started.
///
/// A tenure is not one block: after its first, the miner keeps building on its
/// own tip for as long as the tenure lasts, which is where the transactions
/// waiting in the mempool are confirmed. Such a block carries no tenure change
/// and no coinbase, only the transactions execution admits.
#[must_use]
pub fn build_tenure_continuation_block(
    tenure: &TenureTip,
    transactions: Vec<Transaction>,
    now: u64,
) -> NakamotoBlock {
    NakamotoBlock {
        header: NakamotoBlockHeader {
            version: NAKAMOTO_BLOCK_VERSION_EPOCH_4,
            chain_length: tenure.height.saturating_add(1),
            bitcoin_spent: tenure.bitcoin_spent,
            consensus_hash: tenure.consensus_hash,
            parent_block_id: tenure.block_id,
            transaction_merkle_root: transaction_merkle_root(&transactions),
            state_index_root: TrieHash::from_bytes([0; 32]),
            // A block's timestamp has to advance on its parent's, and a tenure
            // can produce more than one block a second.
            timestamp: now.max(tenure.timestamp.saturating_add(1)),
            miner_signature: MessageSignature::from_bytes([0; 65]),
            signer_signatures: Vec::new(),
            pox_treatment: BitVec::ones(WATERFALL_POX_TREATMENT_LEN)
                .expect("a one-bit vector is valid"),
            problematic_transactions: Vec::new(),
        },
        transactions,
    }
}

/// Build the unexecuted block that extends a tenure into a later burn view.
///
/// A tenure that outlives the Bitcoin block which awarded it has to say so on
/// chain before it may keep spending a fresh budget, and a signer only accepts
/// the extension once threshold signing power has offered it. The tenure change
/// keeps the tenure's own consensus hash on both sides, because an extension
/// does not change the miner (`nakamoto/mod.rs`,
/// `is_wellformed_tenure_extend_block`).
pub fn build_tenure_extend_block(
    tenure: &TenureTip,
    extension: TenureExtension,
    network: Network,
    miner_key: &StacksPrivateKey,
    transactions: Vec<Transaction>,
) -> Result<NakamotoBlock, TenureStartError> {
    let mut extended = vec![Transaction::sign_standard(
        TransactionVersion::for_network(network),
        network.chain_id(),
        AnchorMode::OnChainOnly,
        miner_key,
        extension.nonce,
        0,
        TransactionPayloadData::TenureChange(TenureChangePayload {
            tenure_consensus_hash: tenure.consensus_hash,
            previous_tenure_consensus_hash: tenure.consensus_hash,
            bitcoin_view_consensus_hash: extension.burn_view_consensus_hash,
            previous_tenure_end: tenure.block_id,
            previous_tenure_blocks: extension.blocks_in_tenure,
            cause: TenureChangeCause::Extended,
            public_key_hash: hash160(&miner_key.public_key().to_bytes_compressed()),
        }),
    )?];
    extended.extend(transactions);
    Ok(build_tenure_continuation_block(
        tenure,
        extended,
        extension.now,
    ))
}

/// What a tenure extension says about the tenure it continues.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TenureExtension {
    /// Burn view the tenure carries on into.
    pub burn_view_consensus_hash: ConsensusHash,
    /// Blocks the tenure has produced so far.
    pub blocks_in_tenure: u32,
    /// Next nonce the miner key spends.
    pub nonce: u64,
    /// Wall-clock time the extension is built at.
    pub now: u64,
}

/// The tip of a tenure this miner owns, which its next block builds on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TenureTip {
    pub consensus_hash: ConsensusHash,
    pub block_id: StacksBlockId,
    pub height: u64,
    /// Burn total the tenure committed to, which its later blocks repeat.
    pub bitcoin_spent: u64,
    /// Timestamp of the tip, which the next block has to advance on.
    pub timestamp: u64,
}
