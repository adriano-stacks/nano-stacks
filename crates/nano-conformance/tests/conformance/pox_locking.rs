//! nano's `PoX` handler against stacks-core's, call for call.
//!
//! The lock a `pox-5` entry point describes is applied outside the contract, on
//! the contract-call boundary, and it is consensus: the balance, the unlock
//! height, the lock event and the error a bad response produces are all visible
//! in a state root or a receipt. nano owns that code now, so `pox-locking` is
//! what it is checked against.
//!
//! Every case runs the same response through both handlers against two
//! identically funded stores and compares what each left behind — the balance,
//! the events, and the error or its absence. A test that only asserted nano's
//! own numbers would be asserting the bug.

use clarity::{
    consts::CHAIN_ID_TESTNET,
    types::StacksEpochId,
    vm::{
        ClarityName, ContractName, Value,
        contexts::GlobalContext,
        costs::LimitedCostTracker,
        database::MemoryBackingStore,
        events::{STXEventType, StacksTransactionEvent},
        types::{PrincipalData, QualifiedContractIdentifier, StandardPrincipalData, TupleData},
    },
};

/// The funded account, and the amount it starts with.
const FUNDED: u128 = 10_000_000;

fn staker() -> PrincipalData {
    PrincipalData::parse("ST1J9R0VMA5GQTW65QVHW1KVSKD7MCGT27X37A551").expect("a principal")
}

fn boot(name: &str) -> QualifiedContractIdentifier {
    QualifiedContractIdentifier::new(
        StandardPrincipalData::transient(),
        ContractName::try_from(name.to_owned()).expect("a contract name"),
    )
}

/// The boot contract identifier both handlers resolve, for testnet.
fn pox(name: &str) -> QualifiedContractIdentifier {
    clarity::boot_util::boot_code_id(name, false)
}

/// A `pox-5` position response, in the shape the contract returns.
fn ok_response(fields: Vec<(&str, Value)>) -> Value {
    Value::okay(Value::Tuple(
        TupleData::from_data(
            fields
                .into_iter()
                .map(|(name, value)| {
                    (
                        ClarityName::try_from(name.to_owned()).expect("a field name"),
                        value,
                    )
                })
                .collect(),
        )
        .expect("a tuple"),
    ))
    .expect("a response")
}

fn stake_response(amount: u128, unlock_height: u64) -> Value {
    ok_response(vec![
        ("staker", Value::Principal(staker())),
        ("amount-ustx", Value::UInt(amount)),
        ("unlock-burn-height", Value::UInt(u128::from(unlock_height))),
        ("first-reward-cycle", Value::UInt(2)),
        ("unlock-cycle", Value::UInt(3)),
    ])
}

/// Either handler: the same signature, minus the arguments neither reads.
type Handler = dyn Fn(
    &mut GlobalContext,
    Option<&PrincipalData>,
    &QualifiedContractIdentifier,
    &str,
    &Value,
) -> Result<(), clarity::vm::errors::VmExecutionError>;

/// What a handler left behind: the account, and what it reported.
#[derive(Debug, Eq, PartialEq)]
struct Outcome {
    unlocked: u128,
    locked: u128,
    unlock_height: u64,
    events: Vec<String>,
    error: Option<String>,
}

/// Run one call through a handler and read the account back.
///
/// `prior` runs before the call under test, so a roll-over or an update can be
/// given an account that is already locked — through the same handler, so
/// neither implementation is handed the other's starting state.
fn run(
    handler: &Handler,
    prior: Option<(&str, Value)>,
    contract: &QualifiedContractIdentifier,
    function: &str,
    result: &Value,
) -> Outcome {
    let mut store = MemoryBackingStore::new();
    let database = store.as_clarity_db();
    let mut global = GlobalContext::new(
        false,
        CHAIN_ID_TESTNET,
        database,
        LimitedCostTracker::new_free(),
        StacksEpochId::Epoch40,
    );
    global.begin();
    {
        let mut snapshot = global
            .database
            .get_stx_balance_snapshot(&staker())
            .expect("a snapshot");
        snapshot.credit(FUNDED).expect("credit");
        snapshot.save().expect("save");
    }
    if let Some((prior_function, prior_result)) = prior {
        handler(
            &mut global,
            Some(&staker()),
            &pox("pox-5"),
            prior_function,
            &prior_result,
        )
        .expect("the prior call sets up the account");
    }
    let before = global
        .event_batches
        .last()
        .map_or(0, |(batch, _)| batch.events.len());
    let error = handler(&mut global, Some(&staker()), contract, function, result)
        .err()
        .map(|error| format!("{error:?}"));
    let events = global
        .event_batches
        .last()
        .map(|(batch, _)| {
            batch
                .events
                .iter()
                .skip(before)
                .map(describe)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let balance = global
        .database
        .get_stx_balance_snapshot(&staker())
        .expect("a snapshot")
        .canonical_balance_repr()
        .expect("a balance");
    Outcome {
        unlocked: balance.amount_unlocked(),
        locked: balance.amount_locked(),
        unlock_height: balance.unlock_height(),
        events,
        error,
    }
}

/// A lock event's consensus-visible fields, as a comparable string.
fn describe(event: &StacksTransactionEvent) -> String {
    match event {
        StacksTransactionEvent::STXEvent(STXEventType::STXLockEvent(data)) => format!(
            "lock {} until {} for {} by {}",
            data.locked_amount, data.unlock_height, data.locked_address, data.contract_identifier
        ),
        other => format!("{other:?}"),
    }
}

fn nano(
    global: &mut GlobalContext,
    sender: Option<&PrincipalData>,
    contract: &QualifiedContractIdentifier,
    function: &str,
    result: &Value,
) -> Result<(), clarity::vm::errors::VmExecutionError> {
    nano_vm::pox::handle_contract_call(global, sender, None, contract, function, &[], result)
}

fn reference(
    global: &mut GlobalContext,
    sender: Option<&PrincipalData>,
    contract: &QualifiedContractIdentifier,
    function: &str,
    result: &Value,
) -> Result<(), clarity::vm::errors::VmExecutionError> {
    pox_locking::handle_contract_call_special_cases(
        global,
        sender,
        None,
        contract,
        function,
        &[],
        result,
    )
}

/// Assert both handlers leave the same account, events and error.
fn same(
    case: &str,
    prior: Option<(&str, Value)>,
    contract: &QualifiedContractIdentifier,
    function: &str,
    result: &Value,
) {
    let ours = run(&nano, prior.clone(), contract, function, result);
    let theirs = run(&reference, prior, contract, function, result);
    // Error text is each implementation's own; the shape of the answer is not.
    let comparable = |outcome: &Outcome| {
        (
            outcome.unlocked,
            outcome.locked,
            outcome.unlock_height,
            outcome.events.clone(),
            outcome.error.is_some(),
        )
    };
    assert_eq!(
        comparable(&ours),
        comparable(&theirs),
        "{case}\n  nano      {ours:?}\n  reference {theirs:?}"
    );
}

#[test]
fn a_fresh_stake_locks_the_same_stx() {
    same(
        "stake",
        None,
        &pox("pox-5"),
        "stake",
        &stake_response(4_000_000, 5_000),
    );
}

#[test]
fn registering_for_a_bond_locks_the_same_stx() {
    same(
        "register-for-bond",
        None,
        &pox("pox-5"),
        "register-for-bond",
        &stake_response(4_000_000, 5_000),
    );
}

#[test]
fn a_roll_over_carries_the_lock_forward() {
    for (case, amount) in [("upward", 6_000_000), ("downward", 1_000_000)] {
        same(
            &format!("register-for-bond rolling {case}"),
            Some(("stake", stake_response(4_000_000, 5_000))),
            &pox("pox-5"),
            "register-for-bond",
            &stake_response(amount, 9_000),
        );
    }
}

#[test]
fn a_roll_over_that_does_not_move_the_unlock_forward_is_refused() {
    for height in [4_000, 5_000] {
        same(
            &format!("register-for-bond rolling to {height}"),
            Some(("stake", stake_response(4_000_000, 5_000))),
            &pox("pox-5"),
            "register-for-bond",
            &stake_response(4_000_000, height),
        );
    }
}

#[test]
fn a_stake_update_extends_and_raises_the_same_lock() {
    same(
        "stake-update",
        Some(("stake", stake_response(4_000_000, 5_000))),
        &pox("pox-5"),
        "stake-update",
        &stake_response(6_000_000, 9_000),
    );
}

#[test]
fn a_stake_update_that_lowers_the_lock_is_refused() {
    same(
        "stake-update lowering",
        Some(("stake", stake_response(4_000_000, 5_000))),
        &pox("pox-5"),
        "stake-update",
        &stake_response(1_000_000, 9_000),
    );
}

#[test]
fn a_stake_update_on_an_unlocked_account_is_refused() {
    same(
        "stake-update with no lock",
        None,
        &pox("pox-5"),
        "stake-update",
        &stake_response(4_000_000, 9_000),
    );
}

#[test]
fn an_unstake_brings_the_unlock_forward() {
    same(
        "unstake",
        Some(("stake", stake_response(4_000_000, 9_000))),
        &pox("pox-5"),
        "unstake",
        &stake_response(4_000_000, 5_000),
    );
}

#[test]
fn an_unstake_with_no_lock_is_refused() {
    same(
        "unstake with no lock",
        None,
        &pox("pox-5"),
        "unstake",
        &stake_response(4_000_000, 5_000),
    );
}

#[test]
fn a_stake_of_more_than_the_account_holds_is_refused() {
    same(
        "stake beyond the balance",
        None,
        &pox("pox-5"),
        "stake",
        &stake_response(FUNDED + 1, 5_000),
    );
}

/// A contract `(err …)` locks nothing and is not an error.
#[test]
fn a_contract_error_locks_nothing() {
    same(
        "stake returning (err u1)",
        None,
        &pox("pox-5"),
        "stake",
        &Value::error(Value::UInt(1)).expect("a response"),
    );
}

/// A response that is not the shape `pox-5` is typed to return has to stop the
/// transaction, because the alternative is applying a lock from a guess.
#[test]
fn a_malformed_response_is_refused() {
    let cases = [
        ("not a response", Value::UInt(1)),
        (
            "ok payload not a tuple",
            Value::okay(Value::UInt(1)).expect("a response"),
        ),
        (
            "err payload not a uint",
            Value::error(Value::Bool(false)).expect("a response"),
        ),
        (
            "missing staker",
            ok_response(vec![
                ("amount-ustx", Value::UInt(1)),
                ("unlock-burn-height", Value::UInt(5_000)),
            ]),
        ),
        (
            "missing amount",
            ok_response(vec![
                ("staker", Value::Principal(staker())),
                ("unlock-burn-height", Value::UInt(5_000)),
            ]),
        ),
        (
            "missing unlock height",
            ok_response(vec![
                ("staker", Value::Principal(staker())),
                ("amount-ustx", Value::UInt(1)),
            ]),
        ),
        (
            "staker is not a principal",
            ok_response(vec![
                ("staker", Value::UInt(1)),
                ("amount-ustx", Value::UInt(1)),
                ("unlock-burn-height", Value::UInt(5_000)),
            ]),
        ),
        (
            "unlock height overflows u64",
            ok_response(vec![
                ("staker", Value::Principal(staker())),
                ("amount-ustx", Value::UInt(1)),
                ("unlock-burn-height", Value::UInt(u128::from(u64::MAX) + 1)),
            ]),
        ),
        (
            "zero amount",
            ok_response(vec![
                ("staker", Value::Principal(staker())),
                ("amount-ustx", Value::UInt(0)),
                ("unlock-burn-height", Value::UInt(5_000)),
            ]),
        ),
        (
            "zero unlock height",
            ok_response(vec![
                ("staker", Value::Principal(staker())),
                ("amount-ustx", Value::UInt(1)),
                ("unlock-burn-height", Value::UInt(0)),
            ]),
        ),
    ];
    for (case, response) in cases {
        same(case, None, &pox("pox-5"), "stake", &response);
    }
}

/// A `pox-5` function with no lock side effect does nothing to the account.
#[test]
fn a_function_with_no_lock_effect_changes_nothing() {
    for function in [
        "get-pox-info",
        "unstake-sbtc",
        "update-bond-registration",
        "announce-l1-early-exit",
        "some-function-that-does-not-exist",
    ] {
        same(
            function,
            None,
            &pox("pox-5"),
            function,
            &Value::okay(Value::Bool(true)).expect("a response"),
        );
    }
}

/// Every earlier `PoX` contract is defunct in epoch 4.0: a read still answers, a
/// write does not, and which is which is each contract's own list.
#[test]
fn the_defunct_pox_contracts_answer_only_their_reads() {
    let writes = [
        "stack-stx",
        "delegate-stx",
        "delegate-stack-stx",
        "stack-extend",
        "stack-increase",
        "revoke-delegate-stx",
        "stack-aggregation-commit",
    ];
    let reads = ["get-pox-info", "get-stacker-info", "can-stack-stx"];
    // pox-2, pox-3 and pox-4 only: their gate is the epoch, which both
    // implementations read the same way in any environment. pox-1's is its v1
    // unlock height against the *current burn height*, and an in-memory store
    // reports height 0 — so the reference concludes pox-1 is still live and
    // tries to apply a v1 lock, while nano asserts what every real chain says.
    // Comparing them here would be comparing environments, not handlers.
    for contract in ["pox-2", "pox-3", "pox-4"] {
        for function in writes.into_iter().chain(reads) {
            same(
                &format!("{contract}::{function}"),
                None,
                &pox(contract),
                function,
                &stake_response(4_000_000, 5_000),
            );
        }
    }
    // So pox-1 is asserted directly: a write is refused, a read still answers.
    for function in writes {
        assert!(
            run(
                &nano,
                None,
                &pox("pox"),
                function,
                &stake_response(4_000_000, 5_000)
            )
            .error
            .is_some(),
            "pox::{function} writes and must be refused"
        );
    }
    for function in reads {
        assert!(
            run(
                &nano,
                None,
                &pox("pox"),
                function,
                &Value::okay(Value::Bool(true)).expect("a response")
            )
            .error
            .is_none(),
            "pox::{function} reads and must still answer"
        );
    }
    // `get-pox-rejection` is on pox-1's, pox-2's and pox-3's lists and not
    // pox-4's; `verify-signer-key-sig` is on pox-4's and no other. A single
    // shared list would get both wrong.
    for (function, answering) in [
        ("get-pox-rejection", ["pox", "pox-2", "pox-3"].as_slice()),
        ("verify-signer-key-sig", ["pox-4"].as_slice()),
        ("check-caller-allowed", ["pox-4"].as_slice()),
        ("get-total-pox-rejection", ["pox-2", "pox-3"].as_slice()),
    ] {
        for contract in ["pox", "pox-2", "pox-3", "pox-4"] {
            let outcome = run(
                &nano,
                None,
                &pox(contract),
                function,
                &Value::okay(Value::Bool(true)).expect("a response"),
            );
            assert_eq!(
                outcome.error.is_none(),
                answering.contains(&contract),
                "{contract}::{function} should {} answer",
                if answering.contains(&contract) {
                    "still"
                } else {
                    "not"
                }
            );
            if contract != "pox" {
                same(
                    &format!("{contract}::{function}"),
                    None,
                    &pox(contract),
                    function,
                    &Value::okay(Value::Bool(true)).expect("a response"),
                );
            }
        }
    }
}

/// A call into anything that is not a `PoX` contract passes straight through.
#[test]
fn a_call_into_another_contract_is_untouched() {
    same(
        "an unrelated contract",
        None,
        &boot("some-token"),
        "transfer",
        &stake_response(4_000_000, 5_000),
    );
}
