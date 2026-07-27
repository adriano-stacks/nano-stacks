#![forbid(unsafe_code)]

mod nakamoto;

pub use nakamoto::{
    NakamotoBlock, NakamotoBlockHeader, NakamotoCodecError, Signer, SignerSet, SignerSetError,
    TenureError,
};

use clarity::vm::ClarityVersion as VmClarityVersion;
use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::errors::{ClarityEvalError, VmExecutionError};
use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier};
use nano_codec::{ClarityVersion, Transaction, TransactionPayloadData};
use nano_primitives::{Sha256Sum, TrieHash};
use nano_sortition::SortitionSnapshot;
use nano_vm::{ExecutionResult, MarfStoreError, TransactionResult, Vm};
use std::path::Path;

/// M0 boundary that makes the final validation stage explicit.
#[derive(Clone, Debug, PartialEq)]
pub struct AppliedBlock {
    pub bitcoin_height: u64,
    pub execution: ExecutionResult,
    pub receipts: Vec<TransactionReceipt>,
}

/// A transaction result retained while applying a Nakamoto block.
#[derive(Clone, Debug, PartialEq)]
pub struct TransactionReceipt {
    pub txid: Sha256Sum,
    pub result: TransactionResult,
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
        let block_id = *block.block_id().as_bytes();
        self.vm.begin_block(parent, block_id)?;
        let receipts = block
            .transactions
            .iter()
            .map(|transaction| self.execute_transaction(transaction))
            .collect::<Result<Vec<_>, _>>()?;
        let state_root = self.vm.seal_block()?;
        let actual = TrieHash::from_bytes(state_root.0);
        if actual != block.header.state_index_root {
            return Err(ChainStateError::StateRootMismatch {
                expected: block.header.state_index_root,
                actual,
            });
        }
        Ok(AppliedBlock {
            bitcoin_height: snapshot.bitcoin_height,
            execution: ExecutionResult { state_root },
            receipts,
        })
    }

    fn execute_transaction(
        &mut self,
        transaction: &Transaction,
    ) -> Result<TransactionReceipt, ChainStateError> {
        let origin = transaction.origin_address().ok_or_else(|| {
            ChainStateError::InvalidTransaction("transaction has no recognized network".to_owned())
        })?;
        let sender = principal_from_address(origin)?;
        let sponsor = transaction
            .sponsor_address()
            .map(principal_from_address)
            .transpose()?;
        let result = match transaction.payload().data() {
            TransactionPayloadData::SmartContract {
                contract_name,
                source,
            } => self.vm.deploy_contract(
                contract_identifier(origin, contract_name)?,
                VmClarityVersion::Clarity6,
                source,
                LimitedCostTracker::new_free(),
            )?,
            TransactionPayloadData::VersionedSmartContract {
                clarity_version,
                contract_name,
                source,
            } => self.vm.deploy_contract(
                contract_identifier(origin, contract_name)?,
                clarity_version_to_vm(*clarity_version),
                source,
                LimitedCostTracker::new_free(),
            )?,
            TransactionPayloadData::ContractCall {
                address,
                contract_name,
                function_name,
                arguments,
            } => self.vm.execute_contract_call(
                sender,
                sponsor,
                contract_identifier(*address, contract_name)?,
                function_name,
                &arguments
                    .iter()
                    .map(|argument| argument.as_bytes().to_vec())
                    .collect::<Vec<_>>(),
                LimitedCostTracker::new_free(),
            )?,
            _ => return Err(ChainStateError::UnsupportedPayload),
        };
        Ok(TransactionReceipt {
            txid: transaction.txid(),
            result,
        })
    }
}

fn principal_from_address(
    address: nano_address::StacksAddress,
) -> Result<PrincipalData, ChainStateError> {
    PrincipalData::parse(&address.to_string())
        .map_err(|error| ChainStateError::InvalidTransaction(error.to_string()))
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
        receipts: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use nano_codec::Transaction;
    use nano_primitives::BitcoinHeaderHash;
    use nano_sortition::SortitionSnapshot;

    use super::ChainState;

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
        let deployment = decoded_transaction(&deployment_payload);
        let call = decoded_transaction(&call_payload);

        let deployed = chainstate
            .execute_transaction(&deployment)
            .expect("deploy decoded contract");
        let called = chainstate
            .execute_transaction(&call)
            .expect("call decoded contract");

        assert_eq!(deployed.result.value, None);
        assert_eq!(
            called.result.value,
            Some(clarity::vm::Value::okay(clarity::vm::Value::UInt(42)).expect("response"))
        );
        chainstate.vm.seal_block().expect("seal block");
    }

    fn decoded_transaction(payload: &[u8]) -> Transaction {
        let mut bytes = vec![0x80];
        bytes.extend_from_slice(&0x8000_0000_u32.to_be_bytes());
        bytes.push(4);
        bytes.push(0);
        bytes.extend_from_slice(&[0; 20]);
        bytes.extend_from_slice(&0_u64.to_be_bytes());
        bytes.extend_from_slice(&0_u64.to_be_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&[0; 65]);
        bytes.push(3);
        bytes.push(1);
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(payload);
        Transaction::decode(&bytes).expect("decode transaction").0
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
}
