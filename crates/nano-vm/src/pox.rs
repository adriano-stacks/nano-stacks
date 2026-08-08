//! The native side of a `PoX` contract call: locks, unlocks and lock events.
//!
//! A `pox-5` entry point that stakes, bonds, updates or unstakes returns an
//! ordinary Clarity response, and the STX lock it describes is applied *outside*
//! the contract — by this handler, on the contract-call boundary. Without it a
//! `stake` writes the contract's maps and moves no STX, which is a state root
//! and a receipt the network does not have.
//!
//! Only epoch 4.0 exists here, which decides the whole shape:
//!
//! - `pox-5` is the live contract, so its four position-altering entry points
//!   need real lock semantics.
//! - `pox` through `pox-4` are all defunct. A read-only call still answers
//!   (`get-pox-info` is how a client asks what happened), and anything else is
//!   `DefunctPoxContract`. There is no epoch in which this node runs their lock
//!   code, so none of it is here.
//!
//! Ported from stacks-core's `pox-locking` crate rather than reasoned out: these
//! are consensus rules, and `pox_locking` is the differential oracle they are
//! checked against in `nano-conformance`.

use clarity::{
    boot_util::boot_code_id,
    types::StacksEpochId,
    vm::{
        Value,
        contexts::GlobalContext,
        costs::{cost_functions::ClarityCostFunction, runtime_cost},
        database::{ClarityDatabase, STXBalance},
        errors::{RuntimeError, VmExecutionError, VmInternalError},
        events::{STXEventType, STXLockEventData, StacksTransactionEvent},
        types::{PrincipalData, QualifiedContractIdentifier},
    },
};

const POX_1_NAME: &str = "pox";
const POX_2_NAME: &str = "pox-2";
const POX_3_NAME: &str = "pox-3";
const POX_4_NAME: &str = "pox-4";
const POX_5_NAME: &str = "pox-5";

/// Why a lock could not be applied.
///
/// Two of these are the network's answer to a user; the rest are invariants the
/// `pox-5` contract is supposed to have checked before it returned `ok`, so
/// reaching one means the contract and this handler disagree.
#[derive(Debug)]
#[allow(
    dead_code,
    reason = "the description is read through Debug in the reported error"
)]
enum LockingError {
    /// The account already holds a lock.
    AlreadyLocked,
    /// The Clarity database failed.
    Clarity(VmExecutionError),
    /// An update or roll-over was attempted on an account with no lock.
    NotLocked,
    /// The account cannot cover the amount to lock.
    InsufficientBalance,
    /// An update tried to lower the locked amount.
    InvalidIncrease,
    /// The contract asked to lock nothing.
    InvalidLockAmount,
    /// The contract asked for an unlock height of zero, or one that does not
    /// move a roll-over forward.
    InvalidUnlockHeight,
    /// `amount_locked + amount_unlocked` overflows.
    BalanceOverflow,
    /// The response was not the shape `pox-5` is typed to return. The string
    /// says which field, and is read only through `Debug` in the error the
    /// transaction sees, which is where somebody looking at a failure finds it.
    MalformedResponse(String),
}

impl From<VmExecutionError> for LockingError {
    fn from(error: VmExecutionError) -> Self {
        Self::Clarity(error)
    }
}

/// Lift a locking failure into the error the transaction sees.
///
/// `AlreadyLocked` has its own runtime variant because it is a real answer to a
/// user. The rest are invariant violations: the contract returned `ok` for
/// something it should have rejected, and a node that carried on would write a
/// lock the network does not have. Every variant is named rather than caught by
/// a wildcard, so adding one forces a decision here.
fn locking_error(error: LockingError, context: &str) -> VmExecutionError {
    match error {
        LockingError::AlreadyLocked => {
            VmExecutionError::Runtime(RuntimeError::PoxAlreadyLocked, None)
        }
        LockingError::Clarity(error) => error,
        error @ (LockingError::NotLocked
        | LockingError::InsufficientBalance
        | LockingError::InvalidIncrease
        | LockingError::InvalidLockAmount
        | LockingError::InvalidUnlockHeight
        | LockingError::BalanceOverflow
        | LockingError::MalformedResponse(_)) => VmExecutionError::Internal(
            VmInternalError::Expect(format!("{context}: pox-5 invariant violated: {error:?}")),
        ),
    }
}

/// What a `pox-5` position call answered.
enum StakeResult {
    /// A well-formed `(ok { staker, amount-ustx, unlock-burn-height, … })`.
    Locked {
        staker: PrincipalData,
        amount: u128,
        unlock_height: u64,
    },
    /// An `(err <uint>)` — an ordinary user-visible failure, which locks nothing.
    ContractError,
}

/// Read the three fields a lock needs out of a `pox-5` response.
///
/// A missing or wrongly typed field is *not* treated as a failed call: `pox-5`
/// is typed to return this shape, so a response that is not it means the
/// contract and this handler have parted company, and continuing would apply a
/// lock derived from a guess.
fn parse_stake_result(result: &Value) -> Result<StakeResult, LockingError> {
    let malformed = |what: &str| LockingError::MalformedResponse(what.to_owned());
    let response = result
        .clone()
        .expect_result()
        .map_err(|error| LockingError::MalformedResponse(format!("not a response: {error:?}")))?;
    let payload = match response {
        Ok(payload) => payload,
        Err(payload) => {
            // `pox-5` is `(response … uint)`, so an error payload that is not a
            // uint is as malformed as a wrong `ok`.
            payload.expect_u128().map_err(|error| {
                LockingError::MalformedResponse(format!("err payload not a uint: {error:?}"))
            })?;
            return Ok(StakeResult::ContractError);
        }
    };
    let tuple = payload
        .expect_tuple()
        .map_err(|error| LockingError::MalformedResponse(format!("ok not a tuple: {error:?}")))?;
    let staker = tuple
        .get("staker")
        .map_err(|_| malformed("missing 'staker'"))?
        .to_owned()
        .expect_principal()
        .map_err(|error| {
            LockingError::MalformedResponse(format!("'staker' is not a principal: {error:?}"))
        })?;
    let amount = tuple
        .get("amount-ustx")
        .map_err(|_| malformed("missing 'amount-ustx'"))?
        .to_owned()
        .expect_u128()
        .map_err(|error| {
            LockingError::MalformedResponse(format!("'amount-ustx' is not a uint: {error:?}"))
        })?;
    let height = tuple
        .get("unlock-burn-height")
        .map_err(|_| malformed("missing 'unlock-burn-height'"))?
        .to_owned()
        .expect_u128()
        .map_err(|error| {
            LockingError::MalformedResponse(format!(
                "'unlock-burn-height' is not a uint: {error:?}"
            ))
        })?;
    let unlock_height = u64::try_from(height).map_err(|_| {
        LockingError::MalformedResponse(format!("'unlock-burn-height' overflows u64: {height}"))
    })?;
    Ok(StakeResult::Locked {
        staker,
        amount,
        unlock_height,
    })
}

/// Take a fresh lock on an account that holds none. Leaves the nonce alone.
fn lock(
    database: &mut ClarityDatabase,
    principal: &PrincipalData,
    amount: u128,
    unlock_height: u64,
) -> Result<(), LockingError> {
    if unlock_height == 0 {
        return Err(LockingError::InvalidUnlockHeight);
    }
    if amount == 0 {
        return Err(LockingError::InvalidLockAmount);
    }
    let mut snapshot = database.get_stx_balance_snapshot(principal)?;
    if snapshot.has_locked_tokens()? {
        return Err(LockingError::AlreadyLocked);
    }
    if !snapshot.can_transfer(amount)? {
        return Err(LockingError::InsufficientBalance);
    }
    snapshot.lock_tokens_v5(amount, unlock_height)?;
    snapshot.save()?;
    Ok(())
}

/// Move an existing lock's unlock height, keeping the amount. This is `unstake`,
/// which brings the unlock forward to the next reward cycle.
fn reschedule(
    database: &mut ClarityDatabase,
    principal: &PrincipalData,
    unlock_height: u64,
) -> Result<(), LockingError> {
    if unlock_height == 0 {
        return Err(LockingError::InvalidUnlockHeight);
    }
    let mut snapshot = database.get_stx_balance_snapshot(principal)?;
    if !snapshot.has_locked_tokens()? {
        return Err(LockingError::NotLocked);
    }
    snapshot.update_unlock_v5(unlock_height)?;
    snapshot.save()?;
    Ok(())
}

/// Extend an existing lock, and raise the amount it holds. Never lowers it.
fn extend(
    database: &mut ClarityDatabase,
    principal: &PrincipalData,
    unlock_height: u64,
    new_total_locked: u128,
) -> Result<STXBalance, LockingError> {
    if unlock_height == 0 {
        return Err(LockingError::InvalidUnlockHeight);
    }
    if new_total_locked == 0 {
        return Err(LockingError::InvalidLockAmount);
    }
    let mut snapshot = database.get_stx_balance_snapshot(principal)?;
    if !snapshot.has_locked_tokens()? {
        return Err(LockingError::NotLocked);
    }
    // The unlock moves before the amount does, because the balance the amount is
    // checked against is the one the reschedule leaves behind.
    snapshot.update_unlock_v5(unlock_height)?;
    let balance = snapshot.canonical_balance_repr()?;
    let total = balance
        .amount_unlocked()
        .checked_add(balance.amount_locked())
        .ok_or(LockingError::BalanceOverflow)?;
    if total < new_total_locked {
        return Err(LockingError::InsufficientBalance);
    }
    if balance.amount_locked() > new_total_locked {
        return Err(LockingError::InvalidIncrease);
    }
    snapshot.increase_lock_v5(new_total_locked)?;
    let out = snapshot.canonical_balance_repr()?;
    snapshot.save()?;
    Ok(out)
}

/// Roll an existing lock into a new position: a new unlock height and a new
/// amount, which may be lower — anything freed returns to the spendable balance.
///
/// This is how a staker moves between positions without the lock ever lifting:
/// bond to bond, stake to bond, bond to stake. Which of those is legal is the
/// contract's business; a response of `ok` is taken as its word.
fn roll_over(
    database: &mut ClarityDatabase,
    principal: &PrincipalData,
    unlock_height: u64,
    new_total_locked: u128,
) -> Result<STXBalance, LockingError> {
    if new_total_locked == 0 {
        return Err(LockingError::InvalidLockAmount);
    }
    let mut snapshot = database.get_stx_balance_snapshot(principal)?;
    if !snapshot.has_locked_tokens()? {
        return Err(LockingError::NotLocked);
    }
    let balance = snapshot.canonical_balance_repr()?;
    let total = balance
        .amount_unlocked()
        .checked_add(balance.amount_locked())
        .ok_or(LockingError::BalanceOverflow)?;
    if total < new_total_locked {
        return Err(LockingError::InsufficientBalance);
    }
    // A roll-over has to move the unlock *forward*; one that did not would let a
    // position be re-registered without ever becoming spendable again.
    if unlock_height <= balance.unlock_height() {
        return Err(LockingError::InvalidUnlockHeight);
    }
    snapshot.set_lock_v5(new_total_locked, unlock_height)?;
    let out = snapshot.canonical_balance_repr()?;
    snapshot.save()?;
    Ok(out)
}

/// The event a successful lock reports, which is consensus-visible in a receipt.
fn lock_event(
    mainnet: bool,
    staker: PrincipalData,
    locked_amount: u128,
    unlock_height: u64,
) -> StacksTransactionEvent {
    StacksTransactionEvent::STXEvent(STXEventType::STXLockEvent(STXLockEventData {
        locked_amount,
        unlock_height,
        locked_address: staker,
        contract_identifier: boot_code_id(POX_5_NAME, mainnet),
    }))
}

/// Charge what a lock costs, which is what a transfer costs.
fn charge(global: &mut GlobalContext) -> Result<(), VmExecutionError> {
    runtime_cost(ClarityCostFunction::StxTransfer, &mut global.cost_track, 1)
        .map_err(VmExecutionError::from)
}

/// `stake` and `register-for-bond`: take a lock, or carry an existing one over.
fn handle_lockup(
    global: &mut GlobalContext,
    function: &str,
    value: &Value,
) -> Result<Option<StacksTransactionEvent>, VmExecutionError> {
    charge(global)?;
    let (staker, amount, unlock_height) = match parse_stake_result(value)
        .map_err(|error| locking_error(error, &format!("pox-5 {function}: bad response")))?
    {
        StakeResult::Locked {
            staker,
            amount,
            unlock_height,
        } => (staker, amount, unlock_height),
        StakeResult::ContractError => return Ok(None),
    };

    // A staker moving from one position to another already holds a lock, and
    // taking a fresh one would fail with `AlreadyLocked`. A first-time call has
    // none and takes one.
    let already_locked = global
        .database
        .get_stx_balance_snapshot(&staker)?
        .has_locked_tokens()?;
    let applied = if already_locked {
        roll_over(&mut global.database, &staker, unlock_height, amount).map(|_| ())
    } else {
        lock(&mut global.database, &staker, amount, unlock_height)
    };
    match applied {
        Ok(()) => {
            global.log_stacking(&staker, amount)?;
            Ok(Some(lock_event(
                global.mainnet,
                staker,
                amount,
                unlock_height,
            )))
        }
        Err(error) => Err(locking_error(
            error,
            &format!(
                "pox-5 {function}: failed to lock {amount} from {staker} until {unlock_height}"
            ),
        )),
    }
}

/// `stake-update`: extend and raise a lock the account already holds.
fn handle_update(
    global: &mut GlobalContext,
    function: &str,
    value: &Value,
) -> Result<Option<StacksTransactionEvent>, VmExecutionError> {
    charge(global)?;
    let (staker, amount, unlock_height) = match parse_stake_result(value)
        .map_err(|error| locking_error(error, &format!("pox-5 {function}: bad response")))?
    {
        StakeResult::Locked {
            staker,
            amount,
            unlock_height,
        } => (staker, amount, unlock_height),
        StakeResult::ContractError => return Ok(None),
    };
    match extend(&mut global.database, &staker, unlock_height, amount) {
        Ok(_) => {
            global.log_stacking(&staker, amount)?;
            Ok(Some(lock_event(
                global.mainnet,
                staker,
                amount,
                unlock_height,
            )))
        }
        Err(error) => Err(locking_error(
            error,
            &format!("pox-5 {function}: failed to extend lock from {staker} until {unlock_height}"),
        )),
    }
}

/// `unstake`: bring the unlock forward, keeping the amount.
///
/// No `log_stacking` here: nothing was newly staked, so the asset map has
/// nothing to record. The event still goes out, carrying the new unlock height.
fn handle_unstake(
    global: &mut GlobalContext,
    function: &str,
    value: &Value,
) -> Result<Option<StacksTransactionEvent>, VmExecutionError> {
    charge(global)?;
    let (staker, amount, unlock_height) = match parse_stake_result(value)
        .map_err(|error| locking_error(error, &format!("pox-5 {function}: bad response")))?
    {
        StakeResult::Locked {
            staker,
            amount,
            unlock_height,
        } => (staker, amount, unlock_height),
        StakeResult::ContractError => return Ok(None),
    };
    match reschedule(&mut global.database, &staker, unlock_height) {
        Ok(()) => Ok(Some(lock_event(
            global.mainnet,
            staker,
            amount,
            unlock_height,
        ))),
        Err(error) => Err(locking_error(
            error,
            &format!("pox-5 {function}: failed to unstake {staker} until {unlock_height}"),
        )),
    }
}

/// Whether a defunct `PoX` contract still answers this function.
///
/// A read-only call reads maps that are still there, and clients ask
/// `get-pox-info` of old contracts to find out what happened to a position. Only
/// a call that would *write* is refused. The lists are each contract's own, from
/// `pox-locking`'s `is_read_only`; they are not interchangeable, and a name
/// wrongly on one turns a rejection into a state change.
fn answers_when_defunct(contract: &str, function: &str) -> bool {
    const POX_1: [&str; 10] = [
        "get-pox-rejection",
        "is-pox-active",
        "get-stacker-info",
        "get-reward-set-size",
        "get-total-ustx-stacked",
        "get-reward-set-pox-address",
        "get-stacking-minimum",
        "can-stack-stx",
        "minimal-can-stack-stx",
        "get-pox-info",
    ];
    // pox-2 and pox-3 share a list.
    const POX_2_AND_3: [&str; 23] = [
        "get-pox-rejection",
        "is-pox-active",
        "burn-height-to-reward-cycle",
        "reward-cycle-to-burn-height",
        "current-pox-reward-cycle",
        "get-stacker-info",
        "get-check-delegation",
        "get-reward-set-size",
        "next-cycle-rejection-votes",
        "get-total-ustx-stacked",
        "get-reward-set-pox-address",
        "get-stacking-minimum",
        "check-pox-addr-version",
        "check-pox-addr-hashbytes",
        "check-pox-lock-period",
        "can-stack-stx",
        "minimal-can-stack-stx",
        "get-pox-info",
        "get-delegation-info",
        "get-allowance-contract-callers",
        "get-num-reward-set-pox-addresses",
        "get-partial-stacked-by-cycle",
        "get-total-pox-rejection",
    ];
    const POX_4: [&str; 22] = [
        "burn-height-to-reward-cycle",
        "reward-cycle-to-burn-height",
        "current-pox-reward-cycle",
        "get-stacker-info",
        "check-caller-allowed",
        "get-check-delegation",
        "get-reward-set-size",
        "get-total-ustx-stacked",
        "get-reward-set-pox-address",
        "get-stacking-minimum",
        "check-pox-addr-version",
        "check-pox-addr-hashbytes",
        "check-pox-lock-period",
        "can-stack-stx",
        "minimal-can-stack-stx",
        "get-signer-key-message-hash",
        "verify-signer-key-sig",
        "get-pox-info",
        "get-delegation-info",
        "get-allowance-contract-callers",
        "get-num-reward-set-pox-addresses",
        "get-partial-stacked-by-cycle",
    ];
    let allowed: &[&str] = match contract {
        POX_1_NAME => &POX_1,
        POX_2_NAME | POX_3_NAME => &POX_2_AND_3,
        POX_4_NAME => &POX_4,
        _ => &[],
    };
    allowed.contains(&function)
}

/// Apply the native effects of a `PoX` contract call, if it had any.
///
/// Installed as the Clarity backing store's contract-call special case, so it
/// runs after the contract body and before the call's writes are committed. A
/// call into anything but the five `PoX` contracts passes straight through.
///
/// # Errors
/// If a defunct contract is asked to do something that writes, or if `pox-5`
/// returned a lock this node cannot apply.
pub fn handle_contract_call(
    global: &mut GlobalContext,
    sender: Option<&PrincipalData>,
    _sponsor: Option<&PrincipalData>,
    contract: &QualifiedContractIdentifier,
    function: &str,
    _arguments: &[Value],
    result: &Value,
) -> Result<(), VmExecutionError> {
    let name = match contract {
        contract if *contract == boot_code_id(POX_5_NAME, global.mainnet) => POX_5_NAME,
        contract if *contract == boot_code_id(POX_4_NAME, global.mainnet) => POX_4_NAME,
        contract if *contract == boot_code_id(POX_3_NAME, global.mainnet) => POX_3_NAME,
        contract if *contract == boot_code_id(POX_2_NAME, global.mainnet) => POX_2_NAME,
        contract if *contract == boot_code_id(POX_1_NAME, global.mainnet) => POX_1_NAME,
        _ => return Ok(()),
    };
    if name != POX_5_NAME {
        // Every earlier PoX contract is defunct by epoch 4.0, so the only
        // question left is whether the call reads or writes.
        //
        // pox-2, pox-3 and pox-4 are defunct from epochs 2.2, 2.5 and 4.0
        // respectively, which this node is always past. pox-1's rule is
        // different: stacks-core compares its v1 unlock height against the
        // *current burn height*, which on any chain this node follows is long
        // past — mainnet's v1 unlock was in epoch 2.1, thousands of reward
        // cycles ago. Asserting that rather than reading it is deliberate: a
        // node with no pox-1 lock code has no useful answer for a chain where
        // pox-1 is live, and pretending otherwise would mean writing a lock this
        // module cannot compute.
        debug_assert!(global.epoch_id >= StacksEpochId::Epoch40);
        if answers_when_defunct(name, function) {
            return Ok(());
        }
        return Err(VmExecutionError::Runtime(
            RuntimeError::DefunctPoxContract,
            None,
        ));
    }

    let event = match function {
        "stake" | "register-for-bond" => handle_lockup(global, function, result)?,
        "stake-update" => handle_update(global, function, result)?,
        "unstake" => handle_unstake(global, function, result)?,
        _ => None,
    };

    // A position-altering action is recorded for the staker — always
    // `tx-sender` — so `Pox` post-conditions and `with-pox` allowances can
    // constrain it. Recorded whether or not the call succeeded, so an allowance
    // can gate even an attempt.
    if matches!(
        function,
        "unstake" | "unstake-sbtc" | "update-bond-registration" | "announce-l1-early-exit"
    ) && let Some(staker) = sender
    {
        global.log_pox_action(staker)?;
    }

    if let Some((batch, _)) = global.event_batches.last_mut()
        && let Some(event) = event
    {
        batch.events.push(event);
    }
    Ok(())
}
