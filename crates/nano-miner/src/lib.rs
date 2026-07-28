#![forbid(unsafe_code)]

//! Bitcoin wallet integration for nano-stacks mining.

mod commitment;

pub use commitment::{
    CommitmentPlan, CommitmentPlanError, EPOCH_4_MARKER, RegisteredLeaderKey, plan_commitment,
};

use std::fmt;

use bitcoin::{Amount, Transaction, Txid, consensus::encode::serialize_hex};
use bitcoincore_rpc::{Auth, Client, RpcApi, json};
use nano_address::PoxAddress;
use nano_bitcoin::{
    LeaderBlockCommitment, LeaderCommitmentTransactionError, LeaderKeyRegistration,
    LeaderKeyRegistrationTransactionError, build_leader_commitment_transaction,
    build_leader_key_registration_transaction,
};
use nano_chainstate::{SignerSet, SignerSetError};
use nano_crypto::{MessageSignature, StacksPrivateKey};
use nano_stackerdb::{
    BlockProposal, BlockResponse, Chunk, ChunkAck, SignerMessage, SignerMessageError,
    StackerDbClient, StackerDbClientError, StackerDbContract, StackerDbError,
};
use nano_sync::{SyncClient, SyncError};
use serde::Deserialize;
use serde_json::{Value, json as json_value};

#[derive(Debug)]
pub enum MinerError {
    BitcoinRpc(bitcoincore_rpc::Error),
    Commitment(LeaderCommitmentTransactionError),
    Registration(LeaderKeyRegistrationTransactionError),
    TransactionDecode(bitcoin::consensus::encode::Error),
    MissingInputs,
    AlteredProtocolOutputs,
    UnexpectedChangePosition(i32),
    IncompleteSignature,
    MempoolRejected(String),
    Unconfirmed,
}

impl fmt::Display for MinerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BitcoinRpc(error) => error.fmt(formatter),
            Self::Commitment(error) => error.fmt(formatter),
            Self::Registration(error) => error.fmt(formatter),
            Self::TransactionDecode(error) => error.fmt(formatter),
            Self::MissingInputs => {
                formatter.write_str("Bitcoin wallet did not fund the transaction")
            }
            Self::AlteredProtocolOutputs => {
                formatter.write_str("Bitcoin wallet altered protocol outputs")
            }
            Self::UnexpectedChangePosition(position) => {
                write!(
                    formatter,
                    "Bitcoin wallet placed change at output {position}"
                )
            }
            Self::IncompleteSignature => {
                formatter.write_str("Bitcoin wallet could not fully sign the transaction")
            }
            Self::MempoolRejected(reason) => {
                write!(formatter, "Bitcoin mempool rejected transaction: {reason}")
            }
            Self::Unconfirmed => {
                formatter.write_str("Bitcoin transaction is not in a confirmed block")
            }
        }
    }
}

impl std::error::Error for MinerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BitcoinRpc(error) => Some(error),
            Self::Commitment(error) => Some(error),
            Self::Registration(error) => Some(error),
            Self::TransactionDecode(error) => Some(error),
            Self::MissingInputs
            | Self::AlteredProtocolOutputs
            | Self::UnexpectedChangePosition(_)
            | Self::IncompleteSignature
            | Self::MempoolRejected(_)
            | Self::Unconfirmed => None,
        }
    }
}

impl From<bitcoincore_rpc::Error> for MinerError {
    fn from(error: bitcoincore_rpc::Error) -> Self {
        Self::BitcoinRpc(error)
    }
}

impl From<LeaderCommitmentTransactionError> for MinerError {
    fn from(error: LeaderCommitmentTransactionError) -> Self {
        Self::Commitment(error)
    }
}

impl From<LeaderKeyRegistrationTransactionError> for MinerError {
    fn from(error: LeaderKeyRegistrationTransactionError) -> Self {
        Self::Registration(error)
    }
}

impl From<bitcoin::consensus::encode::Error> for MinerError {
    fn from(error: bitcoin::consensus::encode::Error) -> Self {
        Self::TransactionDecode(error)
    }
}

/// A leader commitment accepted by the local Bitcoin wallet and mempool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmittedCommitment {
    pub transaction_id: Txid,
    pub transaction: Transaction,
    pub fee: Amount,
    pub change_output: usize,
}

/// The replacement transaction created by Bitcoin Core's `bumpfee` RPC.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplacedCommitment {
    pub transaction_id: Txid,
}

/// A leader-key registration accepted by the local Bitcoin wallet and mempool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmittedLeaderKeyRegistration {
    pub transaction_id: Txid,
    pub transaction: Transaction,
    pub fee: Amount,
    pub change_output: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WalletSubmission {
    transaction_id: Txid,
    transaction: Transaction,
    fee: Amount,
    change_output: usize,
}

/// A wallet-scoped Bitcoin Core RPC client.
pub struct BitcoinWallet {
    rpc: Client,
}

impl BitcoinWallet {
    pub fn connect(url: &str, auth: Auth) -> Result<Self, MinerError> {
        Ok(Self {
            rpc: Client::new(url, auth)?,
        })
    }

    #[must_use]
    pub const fn from_rpc(rpc: Client) -> Self {
        Self { rpc }
    }

    /// Fund, sign, verify, and broadcast a replaceable waterfall commitment.
    pub fn submit_leader_commitment(
        &self,
        magic: [u8; 2],
        commitment: LeaderBlockCommitment,
        sbtc_address: &PoxAddress,
        commitment_amount: Amount,
        fee_rate_sats_per_vbyte: Option<u64>,
    ) -> Result<SubmittedCommitment, MinerError> {
        let template = build_leader_commitment_transaction(
            magic,
            commitment,
            Vec::new(),
            sbtc_address,
            commitment_amount,
            None,
        )?;
        let submitted = self.submit_protocol_transaction(&template, 2, fee_rate_sats_per_vbyte)?;

        Ok(SubmittedCommitment {
            transaction_id: submitted.transaction_id,
            transaction: submitted.transaction,
            fee: submitted.fee,
            change_output: submitted.change_output,
        })
    }

    /// Fund, sign, verify, and broadcast a leader-key registration.
    pub fn submit_leader_key_registration(
        &self,
        magic: [u8; 2],
        registration: &LeaderKeyRegistration,
        fee_rate_sats_per_vbyte: Option<u64>,
    ) -> Result<SubmittedLeaderKeyRegistration, MinerError> {
        let template = build_leader_key_registration_transaction(magic, registration, Vec::new())?;
        let submitted = self.submit_protocol_transaction(&template, 1, fee_rate_sats_per_vbyte)?;
        Ok(SubmittedLeaderKeyRegistration {
            transaction_id: submitted.transaction_id,
            transaction: submitted.transaction,
            fee: submitted.fee,
            change_output: submitted.change_output,
        })
    }

    /// Locate a confirmed transaction by the Bitcoin block and position that contain it.
    pub fn confirmed_position(&self, transaction_id: Txid) -> Result<(u64, u32), MinerError> {
        let transaction = self.rpc.get_raw_transaction_info(&transaction_id, None)?;
        let block_hash = transaction.blockhash.ok_or(MinerError::Unconfirmed)?;
        let block = self.rpc.get_block_info(&block_hash)?;
        let index = block
            .tx
            .iter()
            .position(|candidate| *candidate == transaction_id)
            .ok_or(MinerError::Unconfirmed)?;
        Ok((
            u64::try_from(block.height).map_err(|_| MinerError::Unconfirmed)?,
            u32::try_from(index).map_err(|_| MinerError::Unconfirmed)?,
        ))
    }

    /// Replace an unconfirmed commitment at a higher fee rate.
    pub fn bump_commitment_fee(
        &self,
        transaction_id: Txid,
        fee_rate_sats_per_vbyte: u64,
    ) -> Result<ReplacedCommitment, MinerError> {
        let response: BumpFeeResponse = self.rpc.call(
            "bumpfee",
            &[
                Value::from(transaction_id.to_string()),
                json_value!({
                    "fee_rate": fee_rate_sats_per_vbyte,
                    "replaceable": true,
                }),
            ],
        )?;
        Ok(ReplacedCommitment {
            transaction_id: response.transaction_id,
        })
    }

    fn submit_protocol_transaction(
        &self,
        template: &Transaction,
        protocol_output_count: usize,
        fee_rate_sats_per_vbyte: Option<u64>,
    ) -> Result<WalletSubmission, MinerError> {
        let change_position = i32::try_from(protocol_output_count)
            .map_err(|_| MinerError::UnexpectedChangePosition(i32::MAX))?;
        let funded: json::FundRawTransactionResult = self.rpc.call(
            "fundrawtransaction",
            &[
                Value::from(serialize_hex(&template)),
                funding_options(change_position, fee_rate_sats_per_vbyte),
            ],
        )?;
        let funded_transaction = funded.transaction()?;
        let change_output = validate_funded_transaction(
            template,
            &funded_transaction,
            funded.change_position,
            change_position,
        )?;
        let signed = self
            .rpc
            .sign_raw_transaction_with_wallet(&funded_transaction, None, None)?;
        if !signed.complete {
            return Err(MinerError::IncompleteSignature);
        }
        let transaction = signed.transaction()?;
        let acceptance = self.rpc.test_mempool_accept(&[&transaction])?;
        let Some(result) = acceptance.first() else {
            return Err(MinerError::MempoolRejected(
                "Bitcoin Core returned no acceptance result".to_owned(),
            ));
        };
        if !result.allowed {
            return Err(MinerError::MempoolRejected(
                result
                    .reject_reason
                    .clone()
                    .unwrap_or_else(|| "unknown rejection".to_owned()),
            ));
        }
        let transaction_id = self.rpc.send_raw_transaction(&transaction)?;
        Ok(WalletSubmission {
            transaction_id,
            transaction,
            fee: funded.fee,
            change_output,
        })
    }
}

fn funding_options(change_position: i32, fee_rate_sats_per_vbyte: Option<u64>) -> Value {
    let mut options = json_value!({
        "changePosition": change_position,
        "replaceable": true,
    });
    if let Some(fee_rate) = fee_rate_sats_per_vbyte {
        options["fee_rate"] = Value::from(fee_rate);
    }
    options
}

fn validate_funded_transaction(
    template: &Transaction,
    funded: &Transaction,
    change_position: i32,
    expected_change_position: i32,
) -> Result<usize, MinerError> {
    if funded.input.is_empty() {
        return Err(MinerError::MissingInputs);
    }
    if funded.output.get(..template.output.len()) != Some(template.output.as_slice()) {
        return Err(MinerError::AlteredProtocolOutputs);
    }
    if change_position != expected_change_position
        || funded.output.len() != template.output.len() + 1
    {
        return Err(MinerError::UnexpectedChangePosition(change_position));
    }
    usize::try_from(change_position)
        .map_err(|_| MinerError::UnexpectedChangePosition(change_position))
}

#[derive(Deserialize)]
struct BumpFeeResponse {
    #[serde(rename = "txid")]
    transaction_id: Txid,
}

/// Errors raised while publishing a miner proposal or collecting signer responses.
#[derive(Debug)]
pub enum ProposalError {
    Client(StackerDbClientError),
    Chunk(StackerDbError),
    Message(SignerMessageError),
    SignerSet(SignerSetError),
    Sync(SyncError),
    SlotVersionOverflow,
    Rejected {
        reason: Option<String>,
        code: Option<u32>,
    },
}

impl fmt::Display for ProposalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(error) => write!(formatter, "StackerDB client error: {error}"),
            Self::Chunk(error) => write!(formatter, "StackerDB chunk error: {error}"),
            Self::Message(error) => write!(formatter, "signer message error: {error}"),
            Self::SignerSet(error) => write!(formatter, "invalid signer response set: {error}"),
            Self::Sync(error) => write!(formatter, "block upload failed: {error}"),
            Self::SlotVersionOverflow => formatter.write_str("StackerDB slot version overflow"),
            Self::Rejected { reason, code } => {
                formatter.write_str("StackerDB rejected miner chunk")?;
                if let Some(code) = code {
                    write!(formatter, " (code {code})")?;
                }
                if let Some(reason) = reason {
                    write!(formatter, ": {reason}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ProposalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Client(error) => Some(error),
            Self::Chunk(error) => Some(error),
            Self::Message(error) => Some(error),
            Self::SignerSet(error) => Some(error),
            Self::Sync(error) => Some(error),
            Self::SlotVersionOverflow | Self::Rejected { .. } => None,
        }
    }
}

impl From<StackerDbClientError> for ProposalError {
    fn from(error: StackerDbClientError) -> Self {
        Self::Client(error)
    }
}

impl From<StackerDbError> for ProposalError {
    fn from(error: StackerDbError) -> Self {
        Self::Chunk(error)
    }
}

impl From<SignerMessageError> for ProposalError {
    fn from(error: SignerMessageError) -> Self {
        Self::Message(error)
    }
}

impl From<SignerSetError> for ProposalError {
    fn from(error: SignerSetError) -> Self {
        Self::SignerSet(error)
    }
}

impl From<SyncError> for ProposalError {
    fn from(error: SyncError) -> Self {
        Self::Sync(error)
    }
}

/// Coordinates a miner's proposal and finalized-block `StackerDB` slots.
pub struct ProposalCoordinator {
    client: StackerDbClient,
    miner_contract: StackerDbContract,
    signer_contract: StackerDbContract,
    miner_key: StacksPrivateKey,
}

const MINER_PROPOSAL_SLOT: u32 = 0;
const MINER_PUSHED_BLOCK_SLOT: u32 = 1;

impl ProposalCoordinator {
    #[must_use]
    pub const fn new(
        client: StackerDbClient,
        miner_contract: StackerDbContract,
        signer_contract: StackerDbContract,
        miner_key: StacksPrivateKey,
    ) -> Self {
        Self {
            client,
            miner_contract,
            signer_contract,
            miner_key,
        }
    }

    /// Write a proposal to the active miner proposal slot.
    pub async fn publish_proposal(
        &self,
        proposal: &BlockProposal,
    ) -> Result<ChunkAck, ProposalError> {
        self.write_miner_message(
            MINER_PROPOSAL_SLOT,
            SignerMessage::BlockProposal(proposal.clone()),
        )
        .await
    }

    /// Read all current signer responses and return ordered threshold signatures.
    pub async fn collect_signatures(
        &self,
        proposal: &BlockProposal,
        signer_set: &SignerSet,
    ) -> Result<Vec<MessageSignature>, ProposalError> {
        let slots = self.client.slot_versions(&self.signer_contract).await?;
        let mut messages = Vec::with_capacity(slots.len());
        for slot in slots {
            if let Some(bytes) = self
                .client
                .latest_chunk(&self.signer_contract, slot.slot_id)
                .await?
            {
                messages.push(bytes);
            }
        }
        response_signatures(proposal, signer_set, messages)
    }

    /// Announce a threshold-signed block through the miner's pushed-block slot.
    pub async fn publish_block(
        &self,
        block: nano_chainstate::NakamotoBlock,
    ) -> Result<ChunkAck, ProposalError> {
        self.write_miner_message(MINER_PUSHED_BLOCK_SLOT, SignerMessage::BlockPushed(block))
            .await
    }

    /// Finalize signer responses, submit the block to a node, then notify signers.
    pub async fn finalize_and_submit(
        &self,
        proposal: &BlockProposal,
        signer_set: &SignerSet,
        node: &SyncClient,
    ) -> Result<nano_chainstate::NakamotoBlock, ProposalError> {
        let signatures = self.collect_signatures(proposal, signer_set).await?;
        let block = finalize_block(proposal, signatures);
        node.upload_block(&block).await?;
        self.publish_block(block.clone()).await?;
        Ok(block)
    }

    async fn write_miner_message(
        &self,
        slot_id: u32,
        message: SignerMessage,
    ) -> Result<ChunkAck, ProposalError> {
        let slot_version = self
            .client
            .slot_versions(&self.miner_contract)
            .await?
            .into_iter()
            .find(|slot| slot.slot_id == slot_id)
            .map_or(0, |slot| slot.slot_version)
            .checked_add(1)
            .ok_or(ProposalError::SlotVersionOverflow)?;
        let mut chunk = Chunk::new(slot_id, slot_version, message.encode()?);
        chunk.sign(&self.miner_key)?;
        let acknowledgement = self.client.put_chunk(&self.miner_contract, &chunk).await?;
        if !acknowledgement.accepted {
            return Err(ProposalError::Rejected {
                reason: acknowledgement.reason,
                code: acknowledgement.code,
            });
        }
        Ok(acknowledgement)
    }
}

fn response_signatures(
    proposal: &BlockProposal,
    signer_set: &SignerSet,
    messages: impl IntoIterator<Item = Vec<u8>>,
) -> Result<Vec<MessageSignature>, ProposalError> {
    let expected_hash = proposal.block.header.signer_signature_hash();
    let mut signatures = Vec::new();
    for bytes in messages {
        let SignerMessage::BlockResponse(BlockResponse::Accepted(response)) =
            SignerMessage::decode(&bytes)?
        else {
            continue;
        };
        if response.signer_signature_hash == expected_hash {
            signatures.push(response.signature);
        }
    }
    Ok(signer_set.order_responses(&proposal.block.header, signatures)?)
}

fn finalize_block(
    proposal: &BlockProposal,
    signatures: Vec<MessageSignature>,
) -> nano_chainstate::NakamotoBlock {
    let mut block = proposal.block.clone();
    block.header.signer_signatures = signatures;
    block
}

#[cfg(test)]
mod tests {
    use bitcoin::{Amount, OutPoint, TxIn};
    use nano_chainstate::{NakamotoBlock, NakamotoBlockHeader, Signer, SignerSet};
    use nano_crypto::StacksPrivateKey;
    use nano_primitives::{BitVec, ConsensusHash, Sha256Sum, StacksBlockId, TrieHash};
    use nano_stackerdb::{BlockAcceptance, BlockProposal, BlockResponse, SignerMessage};

    use super::{
        MinerError, finalize_block, funding_options, response_signatures,
        validate_funded_transaction,
    };

    #[test]
    fn funding_requests_rbf_with_change_after_protocol_outputs() {
        let options = funding_options(2, Some(7));
        assert_eq!(options["changePosition"], 2);
        assert_eq!(options["replaceable"], true);
        assert_eq!(options["fee_rate"], 7);
    }

    #[test]
    fn funding_validation_requires_wallet_to_preserve_output_order() {
        let template = bitcoin::Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: Vec::new(),
            output: vec![
                bitcoin::TxOut {
                    value: Amount::ZERO,
                    script_pubkey: bitcoin::ScriptBuf::new(),
                },
                bitcoin::TxOut {
                    value: Amount::from_sat(1),
                    script_pubkey: bitcoin::ScriptBuf::new(),
                },
            ],
        };
        let mut funded = template.clone();
        funded.input.push(TxIn {
            previous_output: OutPoint::default(),
            ..TxIn::default()
        });
        funded.output.push(bitcoin::TxOut {
            value: Amount::from_sat(2),
            script_pubkey: bitcoin::ScriptBuf::new(),
        });
        assert!(matches!(
            validate_funded_transaction(&template, &funded, 2, 2),
            Ok(2)
        ));
        funded.output.swap(1, 2);
        assert!(matches!(
            validate_funded_transaction(&template, &funded, 2, 2),
            Err(MinerError::AlteredProtocolOutputs)
        ));
    }

    #[test]
    fn response_collection_uses_only_the_active_proposal_and_reward_set() {
        let first = StacksPrivateKey::from_seed(b"first signer");
        let second = StacksPrivateKey::from_seed(b"second signer");
        let proposal = proposal(&first);
        let digest = proposal.block.header.signer_signature_hash();
        let set = SignerSet::new(vec![
            Signer {
                public_key: first.public_key(),
                weight: 3,
            },
            Signer {
                public_key: second.public_key(),
                weight: 7,
            },
        ])
        .expect("valid signer set");
        let stale = SignerMessage::BlockResponse(BlockResponse::Accepted(BlockAcceptance::new(
            Sha256Sum::from_bytes([9; 32]),
            first.sign(digest.as_bytes()),
        )))
        .encode()
        .expect("encode stale response");
        let second_response = second.sign(digest.as_bytes());
        let active = SignerMessage::BlockResponse(BlockResponse::Accepted(BlockAcceptance::new(
            digest,
            second_response,
        )))
        .encode()
        .expect("encode active response");

        let signatures =
            response_signatures(&proposal, &set, vec![stale, active]).expect("threshold response");
        assert_eq!(signatures, vec![second_response]);
        let finalized = finalize_block(&proposal, signatures);
        assert_eq!(finalized.header.signer_signatures, vec![second_response]);
    }

    fn proposal(miner: &StacksPrivateKey) -> BlockProposal {
        BlockProposal {
            block: NakamotoBlock {
                header: NakamotoBlockHeader {
                    version: 1,
                    chain_length: 1,
                    bitcoin_spent: 0,
                    consensus_hash: ConsensusHash::from_bytes([1; 20]),
                    parent_block_id: StacksBlockId::from_bytes([2; 32]),
                    transaction_merkle_root: Sha256Sum::from_bytes([3; 32]),
                    state_index_root: TrieHash::from_bytes([4; 32]),
                    timestamp: 5,
                    miner_signature: miner.sign(&[6; 32]),
                    signer_signatures: Vec::new(),
                    pox_treatment: BitVec::zeros(1).expect("valid bit vector"),
                },
                transactions: Vec::new(),
            },
            bitcoin_height: 1,
            reward_cycle: 1,
            data: BlockProposal::empty_data(),
        }
    }
}
