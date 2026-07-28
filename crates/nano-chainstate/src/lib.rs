#![forbid(unsafe_code)]

mod nakamoto;

pub use nakamoto::{
    NakamotoBlock, NakamotoBlockHeader, NakamotoCodecError, Signer, SignerSet, SignerSetError,
    TenureError,
};

use clarity::vm::ClarityVersion as VmClarityVersion;
use clarity::vm::Value;
use clarity::vm::costs::{ExecutionCost, LimitedCostTracker};
use clarity::vm::errors::{ClarityEvalError, VmExecutionError};
use std::collections::{HashMap, HashSet};

use clarity::vm::contexts::{AssetMap, AssetMapEntry};
use clarity::vm::representations::ClarityName;
use clarity::vm::types::{AssetIdentifier, PrincipalData, QualifiedContractIdentifier};
use nano_codec::{
    AssetInfo, ClarityVersion, FungibleCondition, NonFungibleCondition, PostConditionData,
    PostConditionMode, PostConditionPrincipal, PoxCondition, Principal, TenureChangeCause,
    Transaction, TransactionPayloadData,
};
use nano_crypto::StacksPrivateKey;
use nano_marf::{MarfValue, TriePointer};
use nano_primitives::{Sha256Sum, TrieHash, sha512_256};
use nano_sortition::SortitionSnapshot;
pub use nano_vm::BitcoinBlockContext;
use nano_vm::{ContractCallOutcome, ExecutionResult, MarfStoreError, TransactionResult, Vm};
use std::path::Path;

/// M0 boundary that makes the final validation stage explicit.
#[derive(Clone, Debug, PartialEq)]
pub struct AppliedBlock {
    pub bitcoin_height: u64,
    pub execution: ExecutionResult,
    pub execution_cost: ExecutionCost,
    pub receipts: Vec<TransactionReceipt>,
}

/// Native accounting that is applied when an epoch-4 block is finalized.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NativeBlockEffects {
    pub credits: Vec<NativeStxCredit>,
    pub liquid_supply_increase: u128,
}

/// One liquid-STX credit produced by native chainstate accounting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeStxCredit {
    pub recipient: PrincipalData,
    pub amount: u128,
}

/// A transaction result retained while applying a Nakamoto block.
#[derive(Clone, Debug, PartialEq)]
pub struct TransactionReceipt {
    pub txid: Sha256Sum,
    pub status: TransactionStatus,
    pub committed: bool,
    pub result: TransactionResult,
}

/// The canonical outcome of executing a transaction in a block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransactionStatus {
    Success,
    PostConditionAborted(String),
    RuntimeFailure(String),
}

enum PayloadOutcome {
    Success(TransactionResult),
    RuntimeFailure {
        result: TransactionResult,
        error: String,
    },
}

/// A chainstate execution context backed by versioned VM state.
#[derive(Debug)]
pub struct ChainState {
    vm: Vm,
}

#[derive(Debug)]
pub enum ChainStateError {
    Storage(MarfStoreError),
    Evaluation(ClarityEvalError),
    Execution(VmExecutionError),
    InvalidTransaction(String),
    TransactionFailure {
        result: Box<TransactionResult>,
        status: TransactionStatus,
    },
    UnsupportedPayload,
    StateRootMismatch {
        expected: TrieHash,
        actual: TrieHash,
    },
}

impl std::fmt::Display for ChainStateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "state storage error: {error}"),
            Self::Evaluation(error) => write!(formatter, "Clarity evaluation error: {error}"),
            Self::Execution(error) => write!(formatter, "Clarity execution error: {error}"),
            Self::InvalidTransaction(error) => write!(formatter, "invalid transaction: {error}"),
            Self::TransactionFailure { status, .. } => {
                write!(formatter, "transaction failed: {status:?}")
            }
            Self::UnsupportedPayload => formatter.write_str("unsupported transaction payload"),
            Self::StateRootMismatch { expected, actual } => write!(
                formatter,
                "state root mismatch: expected {expected}, got {actual}"
            ),
        }
    }
}

impl std::error::Error for ChainStateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            Self::Evaluation(_)
            | Self::Execution(_)
            | Self::InvalidTransaction(_)
            | Self::TransactionFailure { .. }
            | Self::UnsupportedPayload
            | Self::StateRootMismatch { .. } => None,
        }
    }
}

impl From<MarfStoreError> for ChainStateError {
    fn from(error: MarfStoreError) -> Self {
        Self::Storage(error)
    }
}

impl From<ClarityEvalError> for ChainStateError {
    fn from(error: ClarityEvalError) -> Self {
        Self::Evaluation(error)
    }
}

impl From<VmExecutionError> for ChainStateError {
    fn from(error: VmExecutionError) -> Self {
        Self::Execution(error)
    }
}

impl ChainState {
    /// Create an empty chainstate.
    pub fn new() -> Result<Self, ChainStateError> {
        Ok(Self { vm: Vm::new()? })
    }

    /// Open chainstate from a checkpointed Clarity MARF.
    pub fn from_checkpoint(
        path: impl AsRef<Path>,
        source: [u8; 32],
        expected_root: TrieHash,
    ) -> Result<Self, ChainStateError> {
        Ok(Self {
            vm: Vm::from_checkpoint(path, source, expected_root)?,
        })
    }

    /// Return the committed MARF leaves for a block state.
    #[must_use]
    pub fn state_leaves(&self, block: [u8; 32]) -> Option<Vec<(TrieHash, MarfValue)>> {
        self.vm.state_leaves(block)
    }

    /// Return the MARF content hash before ancestry is incorporated.
    #[must_use]
    pub fn state_content_root(&self, block: [u8; 32]) -> Option<TrieHash> {
        self.vm.content_root(block)
    }

    /// Return the root pointers in their consensus serialization order.
    #[must_use]
    pub fn state_root_pointers(&self, block: [u8; 32]) -> Option<Vec<TriePointer>> {
        self.vm.root_pointers(block)
    }

    /// Execute a Clarity program for a block and seal its consensus state root.
    pub fn append_program(
        &mut self,
        snapshot: &SortitionSnapshot,
        parent: Option<[u8; 32]>,
        block: [u8; 32],
        source: &str,
    ) -> Result<AppliedBlock, ChainStateError> {
        self.vm.begin_block(parent, block)?;
        self.vm.execute(source, LimitedCostTracker::new_free())?;
        let state_root = self.vm.seal_block()?;
        Ok(AppliedBlock {
            bitcoin_height: snapshot.bitcoin_height,
            execution: ExecutionResult { state_root },
            execution_cost: ExecutionCost::ZERO,
            receipts: Vec::new(),
        })
    }

    /// Execute the supported transaction forms in a block and verify its committed state root.
    pub fn append_nakamoto_block(
        &mut self,
        snapshot: &SortitionSnapshot,
        parent: Option<[u8; 32]>,
        block: &NakamotoBlock,
    ) -> Result<AppliedBlock, ChainStateError> {
        self.append_nakamoto_block_with_bitcoin_context(
            BitcoinBlockContext::at_height(snapshot.bitcoin_height),
            parent,
            block,
        )
    }

    /// Execute a Nakamoto block with its complete Bitcoin context.
    pub fn append_nakamoto_block_with_bitcoin_context(
        &mut self,
        bitcoin_context: BitcoinBlockContext,
        parent: Option<[u8; 32]>,
        block: &NakamotoBlock,
    ) -> Result<AppliedBlock, ChainStateError> {
        self.append_nakamoto_block_with_effects(
            bitcoin_context,
            parent,
            block,
            NativeBlockEffects::default(),
        )
    }

    /// Execute native accounting and verify the state root committed by a Nakamoto block.
    pub fn append_nakamoto_block_with_effects(
        &mut self,
        bitcoin_context: BitcoinBlockContext,
        parent: Option<[u8; 32]>,
        block: &NakamotoBlock,
        effects: NativeBlockEffects,
    ) -> Result<AppliedBlock, ChainStateError> {
        let applied =
            self.execute_nakamoto_block_with_effects(bitcoin_context, parent, block, effects)?;
        let actual = TrieHash::from_bytes(applied.execution.state_root.0);
        if actual != block.header.state_index_root {
            return Err(ChainStateError::StateRootMismatch {
                expected: block.header.state_index_root,
                actual,
            });
        }
        Ok(applied)
    }

    /// Execute a Nakamoto block without checking its header's committed state root.
    pub fn execute_nakamoto_block_with_bitcoin_context(
        &mut self,
        bitcoin_context: BitcoinBlockContext,
        parent: Option<[u8; 32]>,
        block: &NakamotoBlock,
    ) -> Result<AppliedBlock, ChainStateError> {
        self.execute_nakamoto_block_with_effects(
            bitcoin_context,
            parent,
            block,
            NativeBlockEffects::default(),
        )
    }

    /// Execute a Nakamoto block with native accounting effects derived from its Bitcoin context.
    pub fn execute_nakamoto_block_with_effects(
        &mut self,
        bitcoin_context: BitcoinBlockContext,
        parent: Option<[u8; 32]>,
        block: &NakamotoBlock,
        effects: NativeBlockEffects,
    ) -> Result<AppliedBlock, ChainStateError> {
        let mut block = block.clone();
        self.execute_nakamoto_block(bitcoin_context, parent, &mut block, None, effects)
    }

    /// Execute a block candidate, derive its committed state root, and finalize its block ID.
    pub fn assemble_nakamoto_block_with_bitcoin_context(
        &mut self,
        bitcoin_context: BitcoinBlockContext,
        parent: Option<[u8; 32]>,
        mut block: NakamotoBlock,
        miner_key: &StacksPrivateKey,
    ) -> Result<(NakamotoBlock, AppliedBlock), ChainStateError> {
        let applied = self.execute_nakamoto_block(
            bitcoin_context,
            parent,
            &mut block,
            Some(miner_key),
            NativeBlockEffects::default(),
        )?;
        Ok((block, applied))
    }

    fn execute_nakamoto_block(
        &mut self,
        bitcoin_context: BitcoinBlockContext,
        parent: Option<[u8; 32]>,
        block: &mut NakamotoBlock,
        miner_key: Option<&StacksPrivateKey>,
        effects: NativeBlockEffects,
    ) -> Result<AppliedBlock, ChainStateError> {
        if let Some(parent) = parent {
            let parent_height = block.header.chain_length.checked_sub(1).ok_or_else(|| {
                ChainStateError::InvalidTransaction(
                    "Nakamoto block has no parent height".to_owned(),
                )
            })?;
            self.vm.set_checkpoint_height(
                parent,
                u32::try_from(parent_height).map_err(|_| {
                    ChainStateError::InvalidTransaction("Stacks height overflows u32".to_owned())
                })?,
            );
        }
        self.vm
            .begin_block_execution(parent, temporary_state_id(), bitcoin_context)?;
        let result = (|| {
            self.vm.setup_block_metadata(block.header.timestamp)?;
            if block_starts_new_tenure(block) {
                let next_height = self.vm.tenure_height()?.checked_add(1).ok_or_else(|| {
                    ChainStateError::InvalidTransaction("tenure height overflow".to_owned())
                })?;
                self.vm.set_tenure_height(next_height)?;
            }
            let mut execution_cost = ExecutionCost::ZERO;
            let mut receipts = Vec::with_capacity(block.transactions.len());
            for transaction in &block.transactions {
                transaction.verify_authorization().map_err(|error| {
                    ChainStateError::InvalidTransaction(format!(
                        "transaction authorization failed: {error}"
                    ))
                })?;
                let receipt = self.execute_transaction(transaction, &execution_cost)?;
                execution_cost.add(&receipt.result.cost).map_err(|error| {
                    ChainStateError::InvalidTransaction(format!("block cost overflow: {error}"))
                })?;
                receipts.push(receipt);
            }
            for credit in effects.credits {
                self.vm.credit_stx(&credit.recipient, credit.amount)?;
            }
            self.vm
                .increment_liquid_stx_supply(effects.liquid_supply_increase)?;
            let unlocked = self.vm.process_scheduled_unlocks()?;
            self.vm.increment_liquid_stx_supply(unlocked)?;
            if let Some(miner_key) = miner_key {
                block.header.state_index_root =
                    TrieHash::from_bytes(self.vm.pending_state_root()?.0);
                block.header.miner_signature =
                    miner_key.sign(block.header.miner_signature_hash().as_bytes());
            }
            let state_root = self.vm.seal_block_to(*block.block_id().as_bytes())?;
            Ok(AppliedBlock {
                bitcoin_height: bitcoin_context.height,
                execution: ExecutionResult { state_root },
                execution_cost,
                receipts,
            })
        })();
        if result.is_err() {
            self.vm.abort_block()?;
        }
        result
    }

    fn execute_transaction(
        &mut self,
        transaction: &Transaction,
        execution_cost: &ExecutionCost,
    ) -> Result<TransactionReceipt, ChainStateError> {
        self.vm.begin_transaction()?;
        let result = self.execute_transaction_in_transaction(transaction, execution_cost);
        match result {
            Ok(receipt) => {
                self.vm.commit_transaction()?;
                Ok(receipt)
            }
            Err(ChainStateError::TransactionFailure { result, status }) => {
                self.vm.rollback_transaction()?;
                self.vm.begin_transaction()?;
                let receipt = self.apply_transaction_failure(transaction, *result, status);
                match receipt {
                    Ok(receipt) => {
                        self.vm.commit_transaction()?;
                        Ok(receipt)
                    }
                    Err(error) => {
                        self.vm.rollback_transaction()?;
                        Err(error)
                    }
                }
            }
            Err(error) => {
                self.vm.rollback_transaction()?;
                Err(error)
            }
        }
    }

    fn execute_transaction_in_transaction(
        &mut self,
        transaction: &Transaction,
        execution_cost: &ExecutionCost,
    ) -> Result<TransactionReceipt, ChainStateError> {
        let origin = transaction.origin_address().ok_or_else(|| {
            ChainStateError::InvalidTransaction("transaction has no recognized network".to_owned())
        })?;
        let sender = principal_from_address(origin)?;
        if let Some(receipt) = system_receipt(transaction) {
            return self.execute_system_transaction(transaction, &sender, receipt);
        }
        let sponsor = transaction
            .sponsor_address()
            .map(principal_from_address)
            .transpose()?;
        let origin_condition = transaction.auth().origin();
        let payer_condition = transaction.auth().payer();
        if self.vm.account_nonce(&sender)? != origin_condition.nonce() {
            return Err(ChainStateError::InvalidTransaction(
                "origin nonce does not match account state".to_owned(),
            ));
        }
        let payer = sponsor.as_ref().unwrap_or(&sender);
        if self.vm.account_nonce(payer)? != payer_condition.nonce() {
            return Err(ChainStateError::InvalidTransaction(
                "payer nonce does not match account state".to_owned(),
            ));
        }
        self.vm.debit_fee(payer, payer_condition.fee())?;
        let result = self.execute_payload(
            transaction,
            origin,
            &sender,
            sponsor.as_ref(),
            execution_cost,
        )?;
        let result = match result {
            PayloadOutcome::Success(result) => result,
            PayloadOutcome::RuntimeFailure { result, error } => {
                return Err(ChainStateError::TransactionFailure {
                    result: Box::new(result),
                    status: TransactionStatus::RuntimeFailure(error),
                });
            }
        };
        if matches!(
            transaction.payload().data(),
            TransactionPayloadData::TokenTransfer { .. }
        ) {
            if !transaction.post_conditions().is_empty() {
                return Err(ChainStateError::InvalidTransaction(
                    "token transfers cannot have post-conditions".to_owned(),
                ));
            }
        } else if let Some(reason) = check_postconditions(transaction, &sender, &result.assets)? {
            return Err(ChainStateError::TransactionFailure {
                result: Box::new(result),
                status: TransactionStatus::PostConditionAborted(reason),
            });
        }
        self.update_transaction_nonces(&sender, payer, sponsor.is_some(), transaction)?;
        Ok(TransactionReceipt {
            txid: transaction.txid(),
            status: TransactionStatus::Success,
            committed: true,
            result,
        })
    }

    fn apply_transaction_failure(
        &mut self,
        transaction: &Transaction,
        result: TransactionResult,
        status: TransactionStatus,
    ) -> Result<TransactionReceipt, ChainStateError> {
        let origin = transaction.origin_address().ok_or_else(|| {
            ChainStateError::InvalidTransaction("transaction has no recognized network".to_owned())
        })?;
        let sender = principal_from_address(origin)?;
        let sponsor = transaction
            .sponsor_address()
            .map(principal_from_address)
            .transpose()?;
        let payer = sponsor.as_ref().unwrap_or(&sender);
        self.vm.debit_fee(payer, transaction.auth().payer().fee())?;
        self.update_transaction_nonces(&sender, payer, sponsor.is_some(), transaction)?;
        Ok(TransactionReceipt {
            txid: transaction.txid(),
            status,
            committed: false,
            result,
        })
    }

    fn execute_system_transaction(
        &mut self,
        transaction: &Transaction,
        sender: &PrincipalData,
        receipt: TransactionReceipt,
    ) -> Result<TransactionReceipt, ChainStateError> {
        let fee = transaction.auth().payer().fee();
        if fee == 0 {
            self.vm.touch_stx_balance(sender)?;
        } else {
            self.vm.debit_fee(sender, fee)?;
        }
        self.vm.set_account_nonce(
            sender,
            increment_nonce(transaction.auth().origin().nonce())?,
        )?;
        Ok(receipt)
    }

    fn execute_payload(
        &mut self,
        transaction: &Transaction,
        origin: nano_address::StacksAddress,
        sender: &PrincipalData,
        sponsor: Option<&PrincipalData>,
        execution_cost: &ExecutionCost,
    ) -> Result<PayloadOutcome, ChainStateError> {
        let cost_tracker = self
            .vm
            .transaction_cost_tracker_with_total(execution_cost.clone())?;
        let mut runtime_error = None;
        let mut result = match transaction.payload().data() {
            TransactionPayloadData::TokenTransfer {
                recipient,
                amount,
                memo,
            } => {
                let recipient = principal_from_codec(recipient)?;
                self.vm
                    .transfer_stx(sender, &recipient, u128::from(*amount), memo, cost_tracker)?
            }
            TransactionPayloadData::SmartContract {
                contract_name,
                source,
            } => self.vm.deploy_contract(
                contract_identifier(origin, contract_name)?,
                VmClarityVersion::Clarity6,
                source,
                cost_tracker,
            )?,
            TransactionPayloadData::VersionedSmartContract {
                clarity_version,
                contract_name,
                source,
            } => self.vm.deploy_contract(
                contract_identifier(origin, contract_name)?,
                clarity_version_to_vm(*clarity_version),
                source,
                cost_tracker,
            )?,
            TransactionPayloadData::ContractCall {
                address,
                contract_name,
                function_name,
                arguments,
            } => match self.vm.execute_contract_call_outcome(
                sender.clone(),
                sponsor.cloned(),
                contract_identifier(*address, contract_name)?,
                function_name,
                &arguments
                    .iter()
                    .map(|argument| argument.as_bytes().to_vec())
                    .collect::<Vec<_>>(),
                cost_tracker,
            )? {
                ContractCallOutcome::Success(result) => *result,
                ContractCallOutcome::RuntimeFailure { cost, error } => {
                    runtime_error = Some(error.to_string());
                    TransactionResult {
                        value: Some(Value::err_none()),
                        cost,
                        assets: AssetMap::new(),
                        events: Vec::new(),
                    }
                }
            },
            _ => return Err(ChainStateError::UnsupportedPayload),
        };
        result.cost.sub(execution_cost).map_err(|error| {
            ChainStateError::InvalidTransaction(format!("transaction cost underflow: {error}"))
        })?;
        Ok(match runtime_error {
            Some(error) => PayloadOutcome::RuntimeFailure { result, error },
            None => PayloadOutcome::Success(result),
        })
    }

    fn update_transaction_nonces(
        &mut self,
        sender: &PrincipalData,
        payer: &PrincipalData,
        sponsored: bool,
        transaction: &Transaction,
    ) -> Result<(), ChainStateError> {
        let origin_nonce = increment_nonce(transaction.auth().origin().nonce())?;
        self.vm.set_account_nonce(sender, origin_nonce)?;
        if sponsored {
            self.vm
                .set_account_nonce(payer, increment_nonce(transaction.auth().payer().nonce())?)?;
        }
        Ok(())
    }
}

fn block_starts_new_tenure(block: &NakamotoBlock) -> bool {
    block.transactions.iter().any(|transaction| {
        matches!(
            transaction.payload().data(),
            TransactionPayloadData::TenureChange(payload)
                if payload.cause == TenureChangeCause::BlockFound
        )
    })
}

fn temporary_state_id() -> [u8; 32] {
    *sha512_256(&[1; 52]).as_bytes()
}

fn increment_nonce(nonce: u64) -> Result<u64, ChainStateError> {
    nonce
        .checked_add(1)
        .ok_or_else(|| ChainStateError::InvalidTransaction("origin nonce overflow".to_owned()))
}

fn system_receipt(transaction: &Transaction) -> Option<TransactionReceipt> {
    matches!(
        transaction.payload().data(),
        TransactionPayloadData::Coinbase { .. }
            | TransactionPayloadData::CoinbaseToAltRecipient { .. }
            | TransactionPayloadData::NakamotoCoinbase { .. }
            | TransactionPayloadData::TenureChange(_)
    )
    .then(|| TransactionReceipt {
        txid: transaction.txid(),
        status: TransactionStatus::Success,
        committed: true,
        result: TransactionResult {
            value: Some(Value::okay(Value::Bool(true)).expect("boolean is a valid response")),
            cost: ExecutionCost::ZERO,
            assets: AssetMap::new(),
            events: Vec::new(),
        },
    })
}

fn check_postconditions(
    transaction: &Transaction,
    origin: &PrincipalData,
    assets: &AssetMap,
) -> Result<Option<String>, ChainStateError> {
    let mut checked_fungible = HashMap::<PrincipalData, HashSet<AssetIdentifier>>::new();
    let mut checked_nonfungible =
        HashMap::<PrincipalData, HashMap<AssetIdentifier, Vec<Value>>>::new();
    let mut checked_staking = HashSet::new();
    let mut checked_pox = HashSet::new();

    for postcondition in transaction.post_conditions() {
        match postcondition.data() {
            PostConditionData::Stx {
                principal,
                condition,
                amount,
            } => {
                let principal = postcondition_principal(principal, origin)?;
                let transferred = assets.get_stx(&principal).unwrap_or(0);
                let burned = assets.get_stx_burned(&principal).unwrap_or(0);
                let sent = transferred.checked_add(burned).ok_or_else(|| {
                    ChainStateError::InvalidTransaction(
                        "STX post-condition amount overflow".to_owned(),
                    )
                })?;
                if !matches_fungible_condition(*condition, u128::from(*amount), sent) {
                    return Ok(Some(format!(
                        "STX post-condition failed for {principal}: expected {condition:?} {amount}, got {sent}"
                    )));
                }
                let covered = checked_fungible.entry(principal).or_default();
                if transferred > 0 {
                    covered.insert(AssetIdentifier::STX());
                }
                if burned > 0 {
                    covered.insert(AssetIdentifier::STX_burned());
                }
            }
            PostConditionData::Fungible {
                principal,
                asset,
                condition,
                amount,
            } => {
                let principal = postcondition_principal(principal, origin)?;
                let asset = asset_identifier(asset)?;
                let sent = assets.get_fungible_tokens(&principal, &asset).unwrap_or(0);
                if !matches_fungible_condition(*condition, u128::from(*amount), sent) {
                    return Ok(Some(format!(
                        "fungible post-condition failed for {asset} owned by {principal}: expected {condition:?} {amount}, got {sent}"
                    )));
                }
                checked_fungible.entry(principal).or_default().insert(asset);
            }
            PostConditionData::NonFungible {
                principal,
                asset,
                asset_value,
                condition,
            } => {
                let principal = postcondition_principal(principal, origin)?;
                let asset = asset_identifier(asset)?;
                let value = deserialize_clarity_value(asset_value.as_bytes())?;
                let sent = assets
                    .get_nonfungible_tokens(&principal, &asset)
                    .map_or(&[][..], Vec::as_slice);
                let moved = sent.contains(&value);
                let passes = match condition {
                    NonFungibleCondition::DoesSend => moved,
                    NonFungibleCondition::DoesNotSend => !moved,
                };
                if !passes {
                    return Ok(Some(format!(
                        "non-fungible post-condition failed for {asset} owned by {principal}"
                    )));
                }
                checked_nonfungible
                    .entry(principal)
                    .or_default()
                    .entry(asset)
                    .or_default()
                    .push(value);
            }
            PostConditionData::Staking { .. } | PostConditionData::Pox { .. } => {
                if let Some(reason) = check_epoch4_postcondition(
                    postcondition.data(),
                    origin,
                    assets,
                    &mut checked_staking,
                    &mut checked_pox,
                )? {
                    return Ok(Some(reason));
                }
            }
        }
    }

    Ok(finish_postconditions(
        transaction.post_condition_mode(),
        origin,
        assets,
        &checked_fungible,
        &checked_nonfungible,
        &checked_staking,
        &checked_pox,
    ))
}

fn finish_postconditions(
    mode: PostConditionMode,
    origin: &PrincipalData,
    assets: &AssetMap,
    checked_fungible: &HashMap<PrincipalData, HashSet<AssetIdentifier>>,
    checked_nonfungible: &HashMap<PrincipalData, HashMap<AssetIdentifier, Vec<Value>>>,
    checked_staking: &HashSet<PrincipalData>,
    checked_pox: &HashSet<PrincipalData>,
) -> Option<String> {
    check_unchecked_assets(mode, origin, assets, checked_fungible, checked_nonfungible)
        .or_else(|| check_unchecked_pox_actions(mode, origin, assets, checked_staking, checked_pox))
}

fn check_epoch4_postcondition(
    postcondition: &PostConditionData,
    origin: &PrincipalData,
    assets: &AssetMap,
    checked_staking: &mut HashSet<PrincipalData>,
    checked_pox: &mut HashSet<PrincipalData>,
) -> Result<Option<String>, ChainStateError> {
    match postcondition {
        PostConditionData::Staking {
            principal,
            condition,
            amount,
        } => {
            let principal = postcondition_principal(principal, origin)?;
            let staked = assets.get_stacking(&principal).unwrap_or(0);
            if !matches_fungible_condition(*condition, u128::from(*amount), staked) {
                return Ok(Some(format!(
                    "staking post-condition failed for {principal}: expected {condition:?} {amount}, got {staked}"
                )));
            }
            checked_staking.insert(principal);
        }
        PostConditionData::Pox {
            principal,
            condition,
        } => {
            let principal = postcondition_principal(principal, origin)?;
            let performed = assets.did_pox_action(&principal);
            let passes = match condition {
                PoxCondition::NotPerformed => !performed,
                PoxCondition::MaybePerformed => true,
                PoxCondition::Performed => performed,
            };
            if !passes {
                return Ok(Some(format!("PoX post-condition failed for {principal}")));
            }
            checked_pox.insert(principal);
        }
        _ => unreachable!("only epoch 4 post-conditions are dispatched here"),
    }
    Ok(None)
}

fn check_unchecked_pox_actions(
    mode: PostConditionMode,
    origin: &PrincipalData,
    assets: &AssetMap,
    checked_staking: &HashSet<PrincipalData>,
    checked_pox: &HashSet<PrincipalData>,
) -> Option<String> {
    if mode == PostConditionMode::Allow {
        return None;
    }
    let requires_check =
        |principal: &PrincipalData| mode == PostConditionMode::Deny || principal == origin;
    if assets
        .get_all_stacking()
        .keys()
        .any(|principal| requires_check(principal) && !checked_staking.contains(principal))
    {
        return Some("staking action was not covered by a post-condition".to_owned());
    }
    if assets
        .get_all_pox_actions()
        .iter()
        .any(|principal| requires_check(principal) && !checked_pox.contains(principal))
    {
        return Some("PoX action was not covered by a post-condition".to_owned());
    }
    None
}

fn check_unchecked_assets(
    mode: PostConditionMode,
    origin: &PrincipalData,
    assets: &AssetMap,
    checked_fungible: &HashMap<PrincipalData, HashSet<AssetIdentifier>>,
    checked_nonfungible: &HashMap<PrincipalData, HashMap<AssetIdentifier, Vec<Value>>>,
) -> Option<String> {
    if mode == PostConditionMode::Allow {
        return None;
    }
    for (principal, assets) in assets.clone().to_table() {
        if mode == PostConditionMode::Originator && principal != *origin {
            continue;
        }
        for (asset, entry) in assets {
            match entry {
                AssetMapEntry::Asset(values) => {
                    let covered = checked_nonfungible
                        .get(&principal)
                        .and_then(|assets| assets.get(&asset));
                    if values
                        .iter()
                        .any(|value| !covered.is_some_and(|covered| covered.contains(value)))
                    {
                        return Some(format!(
                            "non-fungible asset {asset} moved by {principal} was not covered"
                        ));
                    }
                }
                _ => {
                    if !checked_fungible
                        .get(&principal)
                        .is_some_and(|covered| covered.contains(&asset))
                    {
                        return Some(format!(
                            "fungible asset {asset} moved by {principal} was not covered"
                        ));
                    }
                }
            }
        }
    }
    None
}

fn postcondition_principal(
    principal: &PostConditionPrincipal,
    origin: &PrincipalData,
) -> Result<PrincipalData, ChainStateError> {
    match principal {
        PostConditionPrincipal::Origin => Ok(origin.clone()),
        PostConditionPrincipal::Standard(address) => principal_from_address(*address),
        PostConditionPrincipal::Contract {
            address,
            contract_name,
        } => principal_from_codec(&Principal::Contract {
            address: *address,
            contract_name: contract_name.clone(),
        }),
    }
}

fn asset_identifier(asset: &AssetInfo) -> Result<AssetIdentifier, ChainStateError> {
    let asset_name = ClarityName::try_from(asset.asset_name.clone())
        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))?;
    Ok(AssetIdentifier {
        contract_identifier: contract_identifier(asset.address, &asset.contract_name)?,
        asset_name,
    })
}

fn deserialize_clarity_value(bytes: &[u8]) -> Result<Value, ChainStateError> {
    let mut bytes = bytes;
    let value = Value::deserialize_read(&mut bytes, None, false)
        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))?;
    if bytes.is_empty() {
        Ok(value)
    } else {
        Err(ChainStateError::InvalidTransaction(
            "non-fungible post-condition value has trailing bytes".to_owned(),
        ))
    }
}

const fn matches_fungible_condition(
    condition: FungibleCondition,
    expected: u128,
    actual: u128,
) -> bool {
    match condition {
        FungibleCondition::SentEqual => actual == expected,
        FungibleCondition::SentGreater => actual > expected,
        FungibleCondition::SentGreaterEqual => actual >= expected,
        FungibleCondition::SentLess => actual < expected,
        FungibleCondition::SentLessEqual => actual <= expected,
    }
}

fn principal_from_address(
    address: nano_address::StacksAddress,
) -> Result<PrincipalData, ChainStateError> {
    PrincipalData::parse(&address.to_string())
        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))
}

fn principal_from_codec(principal: &Principal) -> Result<PrincipalData, ChainStateError> {
    match principal {
        Principal::Standard(address) => principal_from_address(*address),
        Principal::Contract {
            address,
            contract_name,
        } => QualifiedContractIdentifier::parse(&format!("{address}.{contract_name}"))
            .map(PrincipalData::Contract)
            .map_err(|error| ChainStateError::InvalidTransaction(error.to_string())),
    }
}

fn contract_identifier(
    address: nano_address::StacksAddress,
    contract_name: &str,
) -> Result<QualifiedContractIdentifier, ChainStateError> {
    QualifiedContractIdentifier::parse(&format!("{address}.{contract_name}"))
        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))
}

const fn clarity_version_to_vm(version: ClarityVersion) -> VmClarityVersion {
    match version {
        ClarityVersion::Clarity1 => VmClarityVersion::Clarity1,
        ClarityVersion::Clarity2 => VmClarityVersion::Clarity2,
        ClarityVersion::Clarity3 => VmClarityVersion::Clarity3,
        ClarityVersion::Clarity4 => VmClarityVersion::Clarity4,
        ClarityVersion::Clarity5 => VmClarityVersion::Clarity5,
        ClarityVersion::Clarity6 => VmClarityVersion::Clarity6,
    }
}

#[must_use]
pub const fn append_stub(snapshot: &SortitionSnapshot) -> AppliedBlock {
    AppliedBlock {
        bitcoin_height: snapshot.bitcoin_height,
        execution: ExecutionResult {
            state_root: nano_marf::StateRoot::empty(),
        },
        execution_cost: ExecutionCost::ZERO,
        receipts: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use clarity::vm::contexts::AssetMap;
    use clarity::vm::costs::ExecutionCost;
    use clarity::vm::types::PrincipalData;
    use std::{fs, path::Path};

    use nano_address::StacksAddress;
    use nano_codec::Transaction;
    use nano_primitives::{BitcoinHeaderHash, Hash160, TrieHash};
    use nano_sortition::SortitionSnapshot;

    use super::{
        BitcoinBlockContext, ChainState, NakamotoBlock, NativeBlockEffects, NativeStxCredit,
        TransactionStatus, check_postconditions, principal_from_address,
    };

    #[test]
    fn append_program_seals_the_vm_state_root() {
        let snapshot = SortitionSnapshot::genesis(42, BitcoinHeaderHash::from_bytes([0; 32]));
        let mut chainstate = ChainState::new().expect("create chainstate");

        let applied = chainstate
            .append_program(
                &snapshot,
                None,
                [1; 32],
                "(define-data-var counter uint u1) (var-set counter u2) (var-get counter)",
            )
            .expect("append program");

        assert_eq!(applied.bitcoin_height, 42);
        assert_ne!(applied.execution.state_root, nano_marf::StateRoot::empty());
    }

    #[test]
    fn executes_decoded_contract_deployments_and_calls() {
        let mut chainstate = ChainState::new().expect("create chainstate");
        chainstate
            .vm
            .begin_block(None, [1; 32])
            .expect("begin block");

        let deployment_payload = versioned_contract_payload(
            "counter",
            "(define-public (increment (value uint)) (ok (+ value u1)))",
        );
        let call_payload = contract_call_payload("counter", "increment", 41);
        let deployment = decoded_transaction(&deployment_payload, 0, 0);
        let call = decoded_transaction(&call_payload, 1, 0);

        let deployed = chainstate
            .execute_transaction(&deployment, &ExecutionCost::ZERO)
            .expect("deploy decoded contract");
        let called = chainstate
            .execute_transaction(&call, &deployed.result.cost)
            .expect("call decoded contract");

        assert_eq!(deployed.result.value, None);
        assert_eq!(
            called.result.value,
            Some(clarity::vm::Value::okay(clarity::vm::Value::UInt(42)).expect("response"))
        );
        chainstate.vm.seal_block().expect("seal block");
    }

    #[test]
    fn executes_a_captured_checkpoint_token_transfer() {
        let source = [
            0x73, 0xd5, 0x36, 0xfd, 0x05, 0x5e, 0x08, 0x3f, 0x60, 0xbe, 0x70, 0x35, 0x0e, 0x72,
            0x9d, 0x99, 0xcc, 0xea, 0xc3, 0x47, 0xc5, 0xbf, 0xaa, 0xa7, 0x9f, 0xd4, 0x62, 0xd1,
            0xb8, 0x21, 0x53, 0xf3,
        ];
        let root = TrieHash::from_bytes([
            0x8f, 0xdf, 0xf0, 0x9f, 0xd8, 0x7a, 0xe7, 0x9f, 0x97, 0x0a, 0x23, 0x36, 0x27, 0x01,
            0x3f, 0x09, 0x47, 0x8e, 0xe1, 0x71, 0x53, 0x79, 0xa7, 0x34, 0x42, 0x58, 0x4b, 0xb4,
            0x3a, 0x64, 0xc0, 0x71,
        ]);
        let fixture_root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../nano-conformance/fixtures");
        let checkpoint = fixture_root.join("chainstate/checkpoint-H/marf.sqlite");
        let block = NakamotoBlock::decode(
            &fs::read(fixture_root.join(
                "nakamoto/blocks/00000110-4c936fa7021a9eed00dc0d6f7fcae52eb610e7f7cf44b2911e8be0c154f9ef9c.bin",
            ))
            .expect("read fixture block"),
        )
        .expect("decode fixture block");
        assert_eq!(block.transactions.len(), 1);
        let mut chainstate =
            ChainState::from_checkpoint(checkpoint, source, root).expect("open checkpoint");
        chainstate
            .vm
            .begin_block(Some(source), *block.block_id().as_bytes())
            .expect("begin block");

        let receipt = chainstate
            .execute_transaction(&block.transactions[0], &ExecutionCost::ZERO)
            .expect("execute captured transfer");

        assert_eq!(receipt.result.events.len(), 1);
    }

    #[test]
    fn applies_native_credits_after_block_transactions() {
        let source = [
            0x73, 0xd5, 0x36, 0xfd, 0x05, 0x5e, 0x08, 0x3f, 0x60, 0xbe, 0x70, 0x35, 0x0e, 0x72,
            0x9d, 0x99, 0xcc, 0xea, 0xc3, 0x47, 0xc5, 0xbf, 0xaa, 0xa7, 0x9f, 0xd4, 0x62, 0xd1,
            0xb8, 0x21, 0x53, 0xf3,
        ];
        let root = TrieHash::from_bytes([
            0x8f, 0xdf, 0xf0, 0x9f, 0xd8, 0x7a, 0xe7, 0x9f, 0x97, 0x0a, 0x23, 0x36, 0x27, 0x01,
            0x3f, 0x09, 0x47, 0x8e, 0xe1, 0x71, 0x53, 0x79, 0xa7, 0x34, 0x42, 0x58, 0x4b, 0xb4,
            0x3a, 0x64, 0xc0, 0x71,
        ]);
        let fixture_root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../nano-conformance/fixtures");
        let checkpoint = fixture_root.join("chainstate/checkpoint-H/marf.sqlite");
        let block = NakamotoBlock::decode(
            &fs::read(fixture_root.join(
                "nakamoto/blocks/00000110-4c936fa7021a9eed00dc0d6f7fcae52eb610e7f7cf44b2911e8be0c154f9ef9c.bin",
            ))
            .expect("read fixture block"),
        )
        .expect("decode fixture block");
        let context = BitcoinBlockContext::at_height(278);
        let baseline = ChainState::from_checkpoint(&checkpoint, source, root)
            .expect("open checkpoint")
            .execute_nakamoto_block_with_bitcoin_context(context, Some(source), &block)
            .expect("execute baseline block");
        let effects = NativeBlockEffects {
            credits: vec![NativeStxCredit {
                recipient: PrincipalData::parse("ST000000000000000000002AMW42H")
                    .expect("valid recipient"),
                amount: 1,
            }],
            liquid_supply_increase: 1,
        };
        let applied = ChainState::from_checkpoint(checkpoint, source, root)
            .expect("open checkpoint")
            .execute_nakamoto_block_with_effects(context, Some(source), &block, effects)
            .expect("execute native effects");

        assert_ne!(baseline.execution.state_root, applied.execution.state_root);
    }

    #[test]
    fn checks_strict_stx_postconditions_against_clarity_asset_accounting() {
        let transaction = decoded_transaction_with_postconditions(
            &token_transfer_payload(),
            2,
            &[stx_postcondition(1, 10)],
            0,
            0,
        );
        let origin = principal_from_address(
            StacksAddress::new(26, Hash160::from_bytes([0; 20])).expect("testnet address"),
        )
        .expect("origin");
        let mut assets = AssetMap::new();
        assets
            .add_stx_transfer(&origin, 10)
            .expect("record transfer");

        assert_eq!(
            check_postconditions(&transaction, &origin, &assets).expect("check postconditions"),
            None
        );

        let unchecked =
            decoded_transaction_with_postconditions(&token_transfer_payload(), 2, &[], 0, 0);
        assert!(
            check_postconditions(&unchecked, &origin, &assets)
                .expect("check postconditions")
                .is_some()
        );
    }

    #[test]
    fn failed_postcondition_rolls_back_contract_writes_and_consumes_nonce() {
        let mut chainstate = ChainState::new().expect("create chainstate");
        chainstate
            .vm
            .begin_block(None, [2; 32])
            .expect("begin block");

        let deployment = decoded_transaction(
            &versioned_contract_payload(
                "counter",
                "(define-data-var counter uint u0) (define-public (increment) (begin (var-set counter (+ (var-get counter) u1)) (ok (var-get counter)))) (define-read-only (read-counter) (var-get counter))",
            ),
            0,
            0,
        );
        let deployed = chainstate
            .execute_transaction(&deployment, &ExecutionCost::ZERO)
            .expect("deploy contract");

        let failed = decoded_transaction_with_postconditions(
            &contract_call_payload_without_arguments("counter", "increment"),
            2,
            &[stx_postcondition(1, 1)],
            1,
            0,
        );
        let failed = chainstate
            .execute_transaction(&failed, &deployed.result.cost)
            .expect("abort transaction");
        assert!(matches!(
            failed.status,
            TransactionStatus::PostConditionAborted(_)
        ));
        assert!(!failed.committed);

        let get = decoded_transaction(
            &contract_call_payload_without_arguments("counter", "read-counter"),
            2,
            0,
        );
        let read = chainstate
            .execute_transaction(&get, &failed.result.cost)
            .expect("read contract state");
        assert_eq!(read.result.value, Some(clarity::vm::Value::UInt(0)));
    }

    #[test]
    fn runtime_failure_rolls_back_contract_writes_and_consumes_nonce() {
        let mut chainstate = ChainState::new().expect("create chainstate");
        chainstate
            .vm
            .begin_block(None, [3; 32])
            .expect("begin block");

        let deployment = decoded_transaction(
            &versioned_contract_payload(
                "counter",
                "(define-data-var counter uint u0) (define-public (fail) (begin (var-set counter u1) (ok (/ u1 u0)))) (define-read-only (read-counter) (var-get counter))",
            ),
            0,
            0,
        );
        let deployed = chainstate
            .execute_transaction(&deployment, &ExecutionCost::ZERO)
            .expect("deploy contract");

        let failed = decoded_transaction(
            &contract_call_payload_without_arguments("counter", "fail"),
            1,
            0,
        );
        let failed = chainstate
            .execute_transaction(&failed, &deployed.result.cost)
            .expect("accept runtime failure");
        assert!(matches!(
            failed.status,
            TransactionStatus::RuntimeFailure(_)
        ));
        assert!(!failed.committed);
        assert_eq!(failed.result.value, Some(clarity::vm::Value::err_none()));

        let get = decoded_transaction(
            &contract_call_payload_without_arguments("counter", "read-counter"),
            2,
            0,
        );
        let read = chainstate
            .execute_transaction(&get, &failed.result.cost)
            .expect("read contract state");
        assert_eq!(read.result.value, Some(clarity::vm::Value::UInt(0)));
    }

    fn decoded_transaction(payload: &[u8], nonce: u64, fee: u64) -> Transaction {
        decoded_transaction_with_postconditions(payload, 1, &[], nonce, fee)
    }

    fn decoded_transaction_with_postconditions(
        payload: &[u8],
        post_condition_mode: u8,
        post_conditions: &[Vec<u8>],
        nonce: u64,
        fee: u64,
    ) -> Transaction {
        let mut bytes = vec![0x80];
        bytes.extend_from_slice(&0x8000_0000_u32.to_be_bytes());
        bytes.push(4);
        bytes.push(0);
        bytes.extend_from_slice(&[0; 20]);
        bytes.extend_from_slice(&nonce.to_be_bytes());
        bytes.extend_from_slice(&fee.to_be_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&[0; 65]);
        bytes.push(3);
        bytes.push(post_condition_mode);
        bytes.extend_from_slice(
            &u32::try_from(post_conditions.len())
                .expect("short post-condition list")
                .to_be_bytes(),
        );
        for post_condition in post_conditions {
            bytes.extend_from_slice(post_condition);
        }
        bytes.extend_from_slice(payload);
        Transaction::decode(&bytes).expect("decode transaction").0
    }

    fn stx_postcondition(condition: u8, amount: u64) -> Vec<u8> {
        let mut postcondition = vec![0, 1, condition];
        postcondition.extend_from_slice(&amount.to_be_bytes());
        postcondition
    }

    fn token_transfer_payload() -> Vec<u8> {
        let mut payload = vec![0, 5, 26];
        payload.extend_from_slice(&[0; 20]);
        payload.extend_from_slice(&0_u64.to_be_bytes());
        payload.extend_from_slice(&[0; 34]);
        payload
    }

    fn versioned_contract_payload(name: &str, source: &str) -> Vec<u8> {
        let mut payload = vec![6, 6, u8::try_from(name.len()).expect("short contract name")];
        payload.extend_from_slice(name.as_bytes());
        payload.extend_from_slice(
            &u32::try_from(source.len())
                .expect("short source")
                .to_be_bytes(),
        );
        payload.extend_from_slice(source.as_bytes());
        payload
    }

    fn contract_call_payload(contract: &str, function: &str, value: u128) -> Vec<u8> {
        let mut payload = vec![2, 26];
        payload.extend_from_slice(&[0; 20]);
        payload.push(u8::try_from(contract.len()).expect("short contract name"));
        payload.extend_from_slice(contract.as_bytes());
        payload.push(u8::try_from(function.len()).expect("short function name"));
        payload.extend_from_slice(function.as_bytes());
        payload.extend_from_slice(&1_u32.to_be_bytes());
        payload.push(1);
        payload.extend_from_slice(&value.to_be_bytes());
        payload
    }

    fn contract_call_payload_without_arguments(contract: &str, function: &str) -> Vec<u8> {
        let mut payload = vec![2, 26];
        payload.extend_from_slice(&[0; 20]);
        payload.push(u8::try_from(contract.len()).expect("short contract name"));
        payload.extend_from_slice(contract.as_bytes());
        payload.push(u8::try_from(function.len()).expect("short function name"));
        payload.extend_from_slice(function.as_bytes());
        payload.extend_from_slice(&0_u32.to_be_bytes());
        payload
    }
}
