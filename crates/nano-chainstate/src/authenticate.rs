//! What a block has to be able to say about itself before any of it runs.
//!
//! Everything here answers from the block and this chain's own identity. Nothing
//! is asked of the peer that supplied the candidate, and nothing here reads
//! state, which is what lets it run before the VM is touched.
//!
//! A state root would catch none of it. A node that skips these checks and
//! executes anyway computes a perfectly self-consistent state — for a chain
//! nobody else is on.

use nano_bitcoin::{BitcoinOperation, BitcoinOperationKind};
use nano_codec::{
    TenureChangeCause, TenureChangePayload, Transaction, TransactionPayloadData, TransactionVersion,
};
use nano_primitives::{Hash160, Network, hash160};

use crate::{NakamotoBlock, NakamotoBlockHeader, SignerSetError};

/// The header version epoch 4.0 blocks carry, below the shadow flag.
pub const NAKAMOTO_BLOCK_VERSION_EPOCH_4: u8 = 1;

/// The most `problematic_txs` markers a block may carry.
///
/// `stackslib`'s `MAX_PROBLEMATIC_TX_MARKERS` is the largest block divided by
/// the smallest transaction, which is the most markers that could ever point at
/// distinct transactions. `block_authentication` pins the arithmetic against
/// that constant rather than trusting this comment.
pub const MAX_PROBLEMATIC_TRANSACTION_MARKERS: usize = (2 * 1024 * 1024) / 180;

/// What a block claimed about itself that this chain does not accept.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsensusError {
    HeaderVersion(u8),
    /// A block with no transactions cannot be a block: every one carries at
    /// least a tenure change or, mid-tenure, something a miner was paid for.
    EmptyBlock,
    TransactionNetwork {
        txid: [u8; 32],
    },
    TransactionChainId {
        txid: [u8; 32],
        chain_id: u32,
    },
    TransactionAnchorMode {
        txid: [u8; 32],
    },
    /// A coinbase without the VRF proof every Nakamoto coinbase carries.
    CoinbaseWithoutVrfProof {
        txid: [u8; 32],
    },
    /// More than one coinbase, or more than one tenure change.
    TenureTransactionCount {
        coinbases: usize,
        tenure_changes: usize,
    },
    /// A coinbase with no tenure change to justify paying it.
    CoinbaseWithoutTenureChange,
    /// The tenure change is not the first transaction, or the coinbase that
    /// accompanies it is not the second.
    TenureTransactionPosition {
        tenure_change: usize,
        coinbase: Option<usize>,
    },
    /// A tenure change that expects a sortition without the coinbase one pays,
    /// or an extension carrying a coinbase it is not owed.
    TenureChangeCause(TenureChangeCause),
    /// The tenure change does not confirm the block's own parent.
    TenureChangeParent,
    /// The tenure change names a tenure other than the block's own.
    TenureChangeConsensusHash,
    /// An extension claims to start a tenure, or a start claims to continue one.
    TenureChangePreviousTenure,
    /// The tenure change names a previous tenure this chain did not execute.
    TenureChangeParentTenure,
    /// The tenure change miscounts the blocks in the tenure it ends.
    TenureChangeBlockCount {
        claimed: u32,
        executed: u32,
    },
    /// The miner signature does not recover to a public key at all.
    MinerSignatureUnrecoverable,
    /// The tenure change was not signed by the miner that signed the block.
    TenureChangeMinerKey,
    /// The block was not signed by the miner that won its sortition.
    ///
    /// Both hashes are carried because the bare sentence cannot be acted on: which
    /// of the two is wrong is the whole question, and answering it from a running
    /// node otherwise means deriving the sortition again by hand.
    MinerIsNotTheSortitionWinner {
        /// What the winning leader key was registered with, as this node has it.
        registered: Hash160,
        /// What the header's own miner signature recovers to.
        signed: Hash160,
    },
    /// More `problematic_txs` markers than a block could hold transactions.
    ProblematicMarkerCount(usize),
    /// Markers that repeat or run backwards, so a replay cannot follow them.
    ProblematicMarkerOrder(u32),
    /// A marker pointing past the end of the block.
    ProblematicMarkerOutOfBounds(u32),
    /// A marker pointing at a coinbase or tenure change, which a replay may not
    /// skip: they are the block's own structure, not a miner's choice.
    ProblematicMarkerTarget(u32),
    /// The signatures do not carry the reward set's threshold weight.
    SignerWeight(SignerSetError),
    /// The header's cumulative burn is not the total this node derived from its
    /// own burnchain for the same burn view.
    BitcoinSpent { header: u64, derived: u64 },
}

impl std::fmt::Display for ConsensusError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HeaderVersion(version) => {
                write!(
                    formatter,
                    "block header version {version} is not epoch 4.0's"
                )
            }
            Self::EmptyBlock => formatter.write_str("block carries no transactions"),
            Self::TransactionNetwork { txid } => write!(
                formatter,
                "transaction {} is for another network",
                hex::encode(txid)
            ),
            Self::TransactionChainId { txid, chain_id } => write!(
                formatter,
                "transaction {} names chain {chain_id:#010x}",
                hex::encode(txid)
            ),
            Self::TransactionAnchorMode { txid } => write!(
                formatter,
                "transaction {} is anchored off-chain, which 4.0 has no place for",
                hex::encode(txid)
            ),
            Self::CoinbaseWithoutVrfProof { txid } => write!(
                formatter,
                "coinbase {} carries no VRF proof, which every Nakamoto coinbase does",
                hex::encode(txid)
            ),
            Self::TenureTransactionCount {
                coinbases,
                tenure_changes,
            } => write!(
                formatter,
                "block carries {coinbases} coinbases and {tenure_changes} tenure changes"
            ),
            Self::CoinbaseWithoutTenureChange => {
                formatter.write_str("block carries a coinbase without a tenure change")
            }
            Self::TenureTransactionPosition {
                tenure_change,
                coinbase,
            } => write!(
                formatter,
                "the tenure change is transaction {tenure_change} and the coinbase is {coinbase:?}, \
                 where a tenure starts with the change first and the coinbase second"
            ),
            Self::TenureChangeCause(cause) => {
                write!(
                    formatter,
                    "tenure change cause {cause:?} does not match the transactions accompanying it"
                )
            }
            Self::TenureChangeParent => {
                formatter.write_str("the tenure change does not end at this block's parent")
            }
            Self::TenureChangeConsensusHash => {
                formatter.write_str("the tenure change names a tenure other than the block's own")
            }
            Self::TenureChangePreviousTenure => formatter
                .write_str("the tenure change's previous tenure is not the one its cause requires"),
            Self::TenureChangeParentTenure => formatter
                .write_str("the tenure change names a previous tenure this chain did not execute"),
            Self::TenureChangeBlockCount { claimed, executed } => write!(
                formatter,
                "the tenure change reports {claimed} blocks in the tenure it ends, \
                 where this chain executed {executed}"
            ),
            Self::MinerSignatureUnrecoverable => {
                formatter.write_str("the miner signature recovers to no public key")
            }
            Self::TenureChangeMinerKey => formatter
                .write_str("the tenure change was not signed by the miner that signed the block"),
            Self::MinerIsNotTheSortitionWinner { registered, signed } => write!(
                formatter,
                "the block was not signed by the miner whose leader key won its sortition: \
                 the winning key is registered with {registered}, the header signed by {signed}"
            ),
            Self::ProblematicMarkerCount(markers) => write!(
                formatter,
                "{markers} problematic-transaction markers exceed the cap of \
                 {MAX_PROBLEMATIC_TRANSACTION_MARKERS}"
            ),
            Self::ProblematicMarkerOrder(index) => write!(
                formatter,
                "problematic-transaction marker {index} does not follow the one before it"
            ),
            Self::ProblematicMarkerOutOfBounds(index) => write!(
                formatter,
                "problematic-transaction marker {index} points past the end of the block"
            ),
            Self::ProblematicMarkerTarget(index) => write!(
                formatter,
                "problematic-transaction marker {index} points at a coinbase or tenure change"
            ),
            Self::BitcoinSpent { header, derived } => write!(
                formatter,
                "the header spends {header} burn and this node's burnchain makes it {derived}"
            ),
            Self::SignerWeight(error) => write!(formatter, "signer signatures: {error}"),
        }
    }
}

impl std::error::Error for ConsensusError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SignerWeight(error) => Some(error),
            _ => None,
        }
    }
}

/// Whether the block's signatures are there to be checked.
///
/// A candidate this node is assembling has none yet — the miner signs the header
/// at seal time, after validation, and the signers only see it after that — so
/// the rules that read one are asked of a followed block and not of a candidate.
/// The miner's own answer to each of them is the code that builds the block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Signatures {
    Present,
    Pending,
}

/// Everything a block has to satisfy that needs nothing but the block itself.
pub fn authenticate_block(
    block: &NakamotoBlock,
    network: Network,
    signatures: Signatures,
) -> Result<(), ConsensusError> {
    if block.header.version & 0x7f != NAKAMOTO_BLOCK_VERSION_EPOCH_4 {
        return Err(ConsensusError::HeaderVersion(block.header.version));
    }
    // A block with nothing in it is not merely pointless: it could not have
    // started a tenure and nobody was paid to produce it.
    //
    // Asked of a block that arrived, never of one being assembled: a mid-tenure
    // candidate is *born* empty and is filled from the mempool by the execution
    // that follows this check, so refusing it here refused every block a miner
    // could build out of its pool. `execute_nakamoto_block` asks the same question
    // of the assembled block once the pool has had its turn, which is the moment
    // the answer means anything.
    if signatures == Signatures::Present && block.transactions.is_empty() {
        return Err(ConsensusError::EmptyBlock);
    }
    for transaction in &block.transactions {
        check_transaction(transaction, network)?;
    }
    check_problematic_markers(block)?;
    check_tenure_transactions(block)?;
    if signatures == Signatures::Present {
        check_tenure_change_miner(block)?;
    }
    Ok(())
}

/// The miner that signed the header is the miner the tenure change names.
///
/// Whatever the cause. Without this a tenure change lifted out of another miner's
/// block would carry over intact, and nothing else in the block would object:
/// the change is a transaction with its own valid signature, and the header it
/// travels in has a valid signature of its own.
fn check_tenure_change_miner(block: &NakamotoBlock) -> Result<(), ConsensusError> {
    let Some(payload) = tenure_change_payload(block) else {
        return Ok(());
    };
    if payload.public_key_hash == recovered_miner_key_hash(&block.header)? {
        Ok(())
    } else {
        Err(ConsensusError::TenureChangeMinerKey)
    }
}

/// What one transaction has to claim to be on this chain at all.
fn check_transaction(transaction: &Transaction, network: Network) -> Result<(), ConsensusError> {
    let txid = || *transaction.txid().as_bytes();
    let mainnet = matches!(transaction.version(), TransactionVersion::Mainnet);
    if mainnet != network.is_mainnet() {
        return Err(ConsensusError::TransactionNetwork { txid: txid() });
    }
    if transaction.chain_id() != network.chain_id() {
        return Err(ConsensusError::TransactionChainId {
            txid: txid(),
            chain_id: transaction.chain_id(),
        });
    }
    if matches!(
        transaction.anchor_mode(),
        nano_codec::AnchorMode::OffChainOnly
    ) {
        return Err(ConsensusError::TransactionAnchorMode { txid: txid() });
    }
    // Every coinbase from epoch 3.0 on carries a VRF proof, and the proof is
    // what ties the tenure to the sortition it claims. The wire forms without
    // one are 2.x coinbases, which decode here and belong to no 4.0 block.
    if matches!(
        transaction.payload().data(),
        TransactionPayloadData::Coinbase { .. }
            | TransactionPayloadData::CoinbaseToAltRecipient { .. }
    ) {
        return Err(ConsensusError::CoinbaseWithoutVrfProof { txid: txid() });
    }
    Ok(())
}

/// The shape a tenure change and a coinbase may take in one block.
///
/// This is `stackslib`'s `is_wellformed_tenure_start_block` and
/// `is_wellformed_tenure_extend_block` read together, plus `check_tenure_tx`.
/// Reading them apart is how the shapes get confused: the two functions overlap
/// on every block, each returning "this is not one of mine" for the other's, and
/// only their *union* says which blocks are refused outright.
fn check_tenure_transactions(block: &NakamotoBlock) -> Result<(), ConsensusError> {
    let position = |wanted: fn(&TransactionPayloadData) -> bool| {
        block
            .transactions
            .iter()
            .enumerate()
            .filter(|(_, transaction)| wanted(transaction.payload().data()))
            .map(|(index, _)| index)
            .collect::<Vec<_>>()
    };
    let coinbases = position(|payload| {
        matches!(
            payload,
            TransactionPayloadData::Coinbase { .. }
                | TransactionPayloadData::CoinbaseToAltRecipient { .. }
                | TransactionPayloadData::NakamotoCoinbase { .. }
        )
    });
    let tenure_changes =
        position(|payload| matches!(payload, TransactionPayloadData::TenureChange(_)));
    if coinbases.len() > 1 || tenure_changes.len() > 1 {
        return Err(ConsensusError::TenureTransactionCount {
            coinbases: coinbases.len(),
            tenure_changes: tenure_changes.len(),
        });
    }
    let (coinbase, tenure_change) = (coinbases.first().copied(), tenure_changes.first().copied());
    let Some(tenure_change) = tenure_change else {
        // No tenure change: a coinbase here is a payment nothing authorized.
        return if coinbase.is_some() {
            Err(ConsensusError::CoinbaseWithoutTenureChange)
        } else {
            Ok(())
        };
    };
    let payload =
        tenure_change_payload(block).ok_or(ConsensusError::TenureTransactionPosition {
            tenure_change,
            coinbase,
        })?;
    let expected_coinbase = if payload.cause == TenureChangeCause::BlockFound {
        Some(1)
    } else {
        None
    };
    if tenure_change != 0 || coinbase != expected_coinbase {
        // Naming the cause separately from the positions: a coinbase beside an
        // extension and an extension beside a coinbase are the same two
        // transactions and different faults.
        return if coinbase.is_some() == (expected_coinbase.is_some()) {
            Err(ConsensusError::TenureTransactionPosition {
                tenure_change,
                coinbase,
            })
        } else {
            Err(ConsensusError::TenureChangeCause(payload.cause))
        };
    }
    if payload.previous_tenure_end != block.header.parent_block_id {
        return Err(ConsensusError::TenureChangeParent);
    }
    // In every cause, the change names the tenure its block belongs to.
    if payload.tenure_consensus_hash != block.header.consensus_hash {
        return Err(ConsensusError::TenureChangeConsensusHash);
    }
    // An extension does not change miner, so it claims its own tenure as its
    // previous one; a block found in a new sortition cannot.
    let extends = payload.previous_tenure_consensus_hash == payload.tenure_consensus_hash;
    if extends != (payload.cause != TenureChangeCause::BlockFound) {
        return Err(ConsensusError::TenureChangePreviousTenure);
    }
    Ok(())
}

/// Markers naming transactions a replay is to skip.
///
/// They are in the block hash for header version 1, so a wrong one is a
/// different block rather than a different opinion — but a replay follows them,
/// so a marker pointing at nothing, or at the block's own structure, would make
/// two nodes execute different transaction sets from the same bytes.
fn check_problematic_markers(block: &NakamotoBlock) -> Result<(), ConsensusError> {
    let markers = &block.header.problematic_transactions;
    if markers.len() > MAX_PROBLEMATIC_TRANSACTION_MARKERS {
        return Err(ConsensusError::ProblematicMarkerCount(markers.len()));
    }
    let mut previous: Option<u32> = None;
    for marker in markers {
        if previous.is_some_and(|previous| marker.index <= previous) {
            return Err(ConsensusError::ProblematicMarkerOrder(marker.index));
        }
        previous = Some(marker.index);
        let transaction = usize::try_from(marker.index)
            .ok()
            .and_then(|index| block.transactions.get(index))
            .ok_or(ConsensusError::ProblematicMarkerOutOfBounds(marker.index))?;
        if matches!(
            transaction.payload().data(),
            TransactionPayloadData::Coinbase { .. }
                | TransactionPayloadData::CoinbaseToAltRecipient { .. }
                | TransactionPayloadData::NakamotoCoinbase { .. }
                | TransactionPayloadData::TenureChange(_)
        ) {
            return Err(ConsensusError::ProblematicMarkerTarget(marker.index));
        }
    }
    Ok(())
}

/// The tenure change a block carries, if it carries one.
#[must_use]
pub fn tenure_change_payload(block: &NakamotoBlock) -> Option<&TenureChangePayload> {
    block
        .transactions
        .iter()
        .find_map(|transaction| match transaction.payload().data() {
            TransactionPayloadData::TenureChange(payload) => Some(payload),
            _ => None,
        })
}

/// The `Hash160` of the public key that signed this header as its miner.
///
/// Recovered without validating low-S, which is what the network does for both
/// miner and signer signatures: a naive verifier rejects signatures consensus
/// accepts.
pub fn recovered_miner_key_hash(header: &NakamotoBlockHeader) -> Result<Hash160, ConsensusError> {
    let public_key = header
        .miner_signature
        .recover(header.miner_signature_hash().as_bytes())
        .map_err(|_| ConsensusError::MinerSignatureUnrecoverable)?;
    Ok(hash160(&public_key.to_bytes_compressed()))
}

/// Verify a block was signed by the miner that registered `signing_key_hash`.
///
/// The hash comes from the winning leader key's registration on Bitcoin, so this
/// is the rule that ties a block to the sortition rather than to its own
/// contents — the tenure-change check above ties the signature to the block, and
/// a miner that forges both is still not the winner.
pub fn verify_miner_signature(
    header: &NakamotoBlockHeader,
    signing_key_hash: &[u8; 20],
) -> Result<(), ConsensusError> {
    let signed = recovered_miner_key_hash(header)?;
    let registered = Hash160::from_bytes(*signing_key_hash);
    if signed == registered {
        Ok(())
    } else {
        Err(ConsensusError::MinerIsNotTheSortitionWinner { registered, signed })
    }
}

/// The block-signing key hash registered with a VRF public key, if the
/// registration is among these Bitcoin operations.
///
/// Keyed by the VRF key because that is what a sortition names: a leader-key
/// registration binds a VRF key and a block-signing key hash together, and the
/// burnchain refuses a VRF key that is already registered, so one key names one
/// registration.
#[must_use]
pub fn registered_signing_key_hash(
    operations: &[BitcoinOperation],
    vrf_public_key: &[u8; 32],
) -> Option<[u8; 20]> {
    operations
        .iter()
        .find_map(|operation| match operation.kind {
            BitcoinOperationKind::LeaderKeyRegistration {
                vrf_public_key: registered,
                block_signing_key_hash,
                ..
            } if registered == *vrf_public_key => block_signing_key_hash,
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use nano_primitives::{BitVec, ConsensusHash, Sha256Sum, StacksBlockId, TrieHash};

    use super::{ConsensusError, MAX_PROBLEMATIC_TRANSACTION_MARKERS, authenticate_block};
    use crate::{NakamotoBlock, NakamotoBlockHeader};

    /// A header with no transactions under it, which is the one shape every
    /// other check is downstream of.
    fn header() -> NakamotoBlockHeader {
        NakamotoBlockHeader {
            version: 1,
            chain_length: 1,
            bitcoin_spent: 0,
            consensus_hash: ConsensusHash::from_bytes([1; 20]),
            parent_block_id: StacksBlockId::from_bytes([2; 32]),
            transaction_merkle_root: Sha256Sum::default(),
            state_index_root: TrieHash::from_bytes([4; 32]),
            timestamp: 5,
            miner_signature: nano_crypto::StacksPrivateKey::from_seed(b"miner").sign(&[5; 32]),
            signer_signatures: Vec::new(),
            pox_treatment: BitVec::zeros(1).expect("a one-bit vector"),
            problematic_transactions: Vec::new(),
        }
    }

    #[test]
    fn a_block_with_no_transactions_is_refused() {
        let block = NakamotoBlock {
            header: header(),
            transactions: Vec::new(),
        };
        assert_eq!(
            authenticate_block(
                &block,
                nano_primitives::Network::TESTNET,
                super::Signatures::Present
            ),
            Err(ConsensusError::EmptyBlock)
        );
    }

    /// The cap is a count, so it is worth pinning that the number is the one
    /// stacks-core derives — `block_authentication` checks it against the
    /// constant itself, and rejects a block that exceeds it.
    #[test]
    fn the_marker_cap_is_a_block_of_smallest_transactions() {
        assert_eq!(MAX_PROBLEMATIC_TRANSACTION_MARKERS, 11_650);
    }
}
