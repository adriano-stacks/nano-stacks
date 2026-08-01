//! The executed state the public RPC answers account and Clarity queries from.
//!
//! The node owns its chain state, so the RPC reaches it through this trait
//! rather than through a chainstate handle: a follower, a miner and a test
//! harness all serve the same routes from whatever state they hold.

use clarity::vm::{
    Value,
    costs::LimitedCostTracker,
    types::{PrincipalData, QualifiedContractIdentifier},
};
use nano_vm::{ContractCallOutcome, Vm};

/// What a node reports about one account.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AccountEntry {
    /// The STX this account can spend now.
    pub balance: u128,
    pub locked: u128,
    pub unlock_height: u64,
    pub nonce: u64,
}

/// A read-only contract call, with its arguments still in Clarity's
/// consensus serialization.
#[derive(Clone, Debug)]
pub struct ReadOnlyCall {
    pub sender: PrincipalData,
    pub sponsor: Option<PrincipalData>,
    pub contract: QualifiedContractIdentifier,
    pub function: String,
    pub arguments: Vec<Vec<u8>>,
}

/// Why a Clarity query could not be answered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChainAccessError {
    /// The state could not answer at all, which is a server-side failure.
    Unavailable(String),
    /// The call ran and failed, which the caller is told about verbatim.
    Failed(String),
    /// The call tried to write, so it is not a read-only call.
    NotReadOnly,
}

/// The executed Clarity state behind `/v2/accounts`, `/v2/contracts/call-read`
/// and transaction admission.
pub trait ChainAccess: Send {
    /// Read one account as of the state this node last executed.
    fn account(&mut self, principal: &PrincipalData) -> Result<AccountEntry, ChainAccessError>;

    /// Run a read-only contract call and return what it evaluated to.
    fn call_read_only(&mut self, call: &ReadOnlyCall) -> Result<Value, ChainAccessError>;
}

impl ChainAccess for Vm {
    fn account(&mut self, principal: &PrincipalData) -> Result<AccountEntry, ChainAccessError> {
        let nonce = self
            .account_nonce(principal)
            .map_err(|error| ChainAccessError::Unavailable(error.to_string()))?;
        // `stx-account` is the only view that reports the lock alongside the
        // spendable balance, and it applies the unlock schedule while it reads.
        let account = self
            .execute(
                &format!("(stx-account '{principal})"),
                LimitedCostTracker::new_free(),
            )
            .map_err(|error| ChainAccessError::Unavailable(error.to_string()))?
            .value;
        let Some(Value::Tuple(account)) = account else {
            return Err(ChainAccessError::Unavailable(
                "stx-account did not evaluate to a tuple".to_owned(),
            ));
        };
        let field = |name: &str| -> Result<u128, ChainAccessError> {
            match account.get(name) {
                Ok(Value::UInt(amount)) => Ok(*amount),
                _ => Err(ChainAccessError::Unavailable(format!(
                    "stx-account is missing {name}"
                ))),
            }
        };
        Ok(AccountEntry {
            balance: field("unlocked")?,
            locked: field("locked")?,
            unlock_height: u64::try_from(field("unlock-height")?).unwrap_or(u64::MAX),
            nonce,
        })
    }

    /// The Clarity database this path opens is dropped without committing, so
    /// a call that writes cannot reach the MARF; the write dimensions it
    /// reports are what tells the caller it was not a read-only call.
    fn call_read_only(&mut self, call: &ReadOnlyCall) -> Result<Value, ChainAccessError> {
        let outcome = self
            .execute_contract_call_outcome(
                call.sender.clone(),
                call.sponsor.clone(),
                call.contract.clone(),
                &call.function,
                &call.arguments,
                &LimitedCostTracker::new_free(),
            )
            .map_err(|error| ChainAccessError::Failed(error.to_string()))?;
        let result = match outcome {
            ContractCallOutcome::Success(result)
            | ContractCallOutcome::AbortedByResponse(result) => result,
            ContractCallOutcome::RuntimeFailure { error, .. } => {
                return Err(ChainAccessError::Failed(error.to_string()));
            }
        };
        if result.cost.write_count > 0 || result.cost.write_length > 0 {
            return Err(ChainAccessError::NotReadOnly);
        }
        result
            .value
            .ok_or_else(|| ChainAccessError::Failed("call returned no value".to_owned()))
    }
}

/// An executing chainstate answers from the state it has sealed, which is the
/// only source the public RPC should ever read.
impl ChainAccess for nano_chainstate::ChainState {
    fn account(&mut self, principal: &PrincipalData) -> Result<AccountEntry, ChainAccessError> {
        self.vm_mut().account(principal)
    }

    fn call_read_only(&mut self, call: &ReadOnlyCall) -> Result<Value, ChainAccessError> {
        self.vm_mut().call_read_only(call)
    }
}
