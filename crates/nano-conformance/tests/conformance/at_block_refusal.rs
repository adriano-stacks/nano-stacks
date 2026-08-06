//! `at-block` in epoch 4.0: refused, and refused before its argument.
//!
//! stacks-core checks `supports_at_block()` twice against two different epochs.
//! At analysis time it asks the epoch the contract was *deployed* in
//! (`type_checker/v2_1/natives/mod.rs`), so a contract published before 3.4 was
//! accepted with the word and its stored analysis keeps it accepted forever. At
//! run time it asks the epoch the chain is executing *now*
//! (`special_at_block`, `clarity/src/vm/functions/database.rs`), and
//! `supports_at_block()` is `< Epoch34` — so on mainnet today the first check
//! passes and the second fails. **881** contracts in the mainnet checkpoint at
//! 8,665,600 have an `(at-block` call site; a call reaching one of those lines
//! errors, and the error is in the receipt.
//!
//! This is the case clar2wasm's own crosscheck harness cannot express, and
//! [[066]] says why: `TestEnvironment` replaces a Clarity version that does not
//! match its epoch, so a snippet asking for "analysed as Clarity 2, executed at
//! 4.0" silently runs as Clarity 6, where both engines refuse `at-block` at
//! *analysis* for a different and correct reason. Here the state is planted
//! instead — the shape [[064]]'s deploy-epoch fixture already builds — so the
//! two engines can be asked the same question the chain answers.
//!
//! ## Two things the refusal has to get right, and only one of them is the error
//!
//! `special_at_block` refuses **before** `check_argument_count`, before
//! `runtime_cost(AtBlock)` and before it evaluates the block hash. All three are
//! observable and none of them is a state root:
//!
//! * the **error identity** is `RuntimeCheckErrorKind::AtBlockUnavailable`, not a
//!   generic runtime failure, and it lands in the receipt as `(err none)` with
//!   the transaction failed and the block accepted — `AtBlockUnavailable` is not
//!   `rejectable()`;
//! * the **cost** carries no `AtBlock` charge and none for the hash argument;
//! * and when the hash argument can return from the function on its own — which
//!   is exactly what `reserve-v1` does, `(unwrap! (get-block-info?
//!   id-header-hash block) (err ERR_BLOCK_INFO))` — evaluating it first replaces
//!   the refusal with an ordinary answer.
//!
//! The third is why the gate cannot live only in the `enter_at_block` host
//! function: a host call happens after its arguments. `SHORT_RETURNING_HASH`
//! below answered `(err u1)` under the compiler and `AtBlockUnavailable` under
//! the interpreter until the compiler stopped emitting the argument at all.
//!
//! The interpreter is the oracle and nothing else: clarity-wasm is the engine
//! that runs mainnet, so a disagreement is a compiler bug to fix.

use clarity::vm::{
    ClarityVersion, Value,
    analysis::{AnalysisDatabase, ContractAnalysis, run_analysis},
    ast::build_ast,
    contexts::OwnedEnvironment,
    contracts::Contract,
    costs::{ExecutionCost, LimitedCostTracker},
    database::{ClarityDatabase, ClaritySerializable, MemoryBackingStore},
    errors::{RuntimeCheckErrorKind, VmExecutionError},
    resource_limiter::ResourceLimiter,
    types::QualifiedContractIdentifier,
};
use nano_primitives::Network;
use nano_vm::{ContractCallOutcome, MarfStore, Vm};
use stacks_common::types::StacksEpochId;

/// The epoch the planted contracts were deployed in.
///
/// 3.3 rather than `reserve-v1`'s own 2.4 because it is the last epoch that
/// accepted `at-block`, so it is the narrowest possible gap to 4.0: nothing here
/// can pass by being five epochs of language rules away from the executing one.
/// It is also the epoch stacks-core's own `at-block-unavail` test deploys in.
const DEPLOYED_IN: StacksEpochId = StacksEpochId::Epoch33;
const DEPLOYED_VERSION: ClarityVersion = ClarityVersion::Clarity2;

/// stacks-core's own `at-block-unavail`, byte for byte from
/// `stackslib/src/chainstate/tests/runtime_analysis_tests.rs`
/// (`runtime_check_error_kind_at_block_unavailable_ccall`), which deploys it in
/// 3.3 and calls it in 3.4.
///
/// Copied rather than paraphrased because the cost of the call is a function of
/// the contract's size, so the same source is what makes the recorded snapshot
/// comparable at all.
const LITERAL_HASH: &str = "
        (define-public (trigger-error)
            (ok (at-block 0x0101010101010101010101010101010101010101010101010101010101010101
                    u1)))";

/// `reserve-v1`'s shape: the block hash is computed, and computing it can return
/// from the function without `at-block` ever being reached.
///
/// This is not a contrived shape. It is the one line of 118 that made
/// `reserve-v1` the contract [[064]] was found on, and the idiom every contract
/// that reads a past block uses, because `at-block` needs an
/// `id-header-hash` and `get-block-info?` is where one comes from.
const SHORT_RETURNING_HASH: &str = "\
(define-read-only (guarded (block uint))
  (at-block (unwrap! (get-block-info? id-header-hash block) (err u1)) (ok u7)))";

/// An `at-block` under an `if` that does not take it.
///
/// The refusal is evaluated where the expression is, not where the contract is:
/// a branch that is not taken raises nothing. Without this, "refuse `at-block`"
/// could be satisfied by refusing every call into any contract that mentions it,
/// which is the mistake 8,668,096 was.
const UNTAKEN_BRANCH: &str = "\
(define-read-only (maybe (flag bool))
  (if flag
    (at-block 0x0101010101010101010101010101010101010101010101010101010101010101 u1)
    u7))";

fn id() -> QualifiedContractIdentifier {
    QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.aged")
        .expect("a contract identifier")
}

/// What one engine did with a call, in the three terms a receipt carries.
#[derive(Debug)]
struct Answer {
    value: Option<Value>,
    failure: Option<VmExecutionError>,
    cost: ExecutionCost,
}

impl Answer {
    /// Whether this is the refusal stacks-core produces, by error identity
    /// rather than by message.
    const fn refused_at_block(&self) -> bool {
        matches!(
            &self.failure,
            Some(VmExecutionError::RuntimeCheck(
                RuntimeCheckErrorKind::AtBlockUnavailable
            ))
        )
    }

    /// The failure's identity, or the value, for a message that says what came
    /// back instead.
    fn describe(&self) -> String {
        match (&self.failure, &self.value) {
            (Some(failure), _) => format!("failed: {failure:?}"),
            (None, Some(value)) => format!("answered {value:?}"),
            (None, None) => "answered nothing".to_owned(),
        }
    }
}

fn answer(outcome: Result<ContractCallOutcome, VmExecutionError>) -> Answer {
    match outcome {
        Ok(
            ContractCallOutcome::Success(result) | ContractCallOutcome::AbortedByResponse(result),
        ) => Answer {
            value: result.value,
            failure: None,
            cost: result.cost,
        },
        Ok(ContractCallOutcome::RuntimeFailure { cost, error }) => Answer {
            value: None,
            failure: Some(error),
            cost,
        },
        // A refusal that does not even become a receipt: the block would stop.
        // Kept as an answer rather than a panic so a test can say which of the
        // two happened.
        Err(error) => Answer {
            value: None,
            failure: Some(error),
            cost: ExecutionCost::ZERO,
        },
    }
}

/// The contract analysis the reference implementation writes at 3.3.
///
/// Built by `run_analysis` itself, in a throwaway store, so the planted state is
/// not a hand-written stand-in for what the chain holds. It is also the
/// assertion that 3.3 *accepts* this source: an `expect` here would fire if the
/// analysis-time half of the pair were what refused.
fn analysed_when_deployed(contract: &QualifiedContractIdentifier, source: &str) -> ContractAnalysis {
    let mut backing = MemoryBackingStore::new();
    let expressions = build_ast(
        contract,
        source,
        &mut LimitedCostTracker::new_free(),
        DEPLOYED_VERSION,
        DEPLOYED_IN,
    )
    .expect("the source parses")
    .expressions;
    let mut analysis_db = backing.as_analysis_db();
    analysis_db
        .execute(|database| {
            run_analysis(
                contract,
                &expressions,
                database,
                false,
                LimitedCostTracker::new_free(),
                DEPLOYED_IN,
                DEPLOYED_VERSION,
                false,
                ResourceLimiter::unlimited(),
            )
            .map_err(|boxed| boxed.0)
        })
        .expect("epoch 3.3 accepts at-block, which is the whole premise here")
}

/// The contract definition the reference implementation writes at 3.3.
///
/// `initialize_versioned_contract` type-checks nothing — analysis is the
/// separate step above — so this is reachable for a source epoch 4.0 refuses,
/// and what comes out is the real definition: the functions a call looks up and
/// the bodies the interpreter evaluates. The throwaway store takes every side
/// effect of the deployment.
fn initialized_when_deployed(
    network: Network,
    contract: &QualifiedContractIdentifier,
    source: &str,
) -> Contract {
    let mut backing = MemoryBackingStore::new();
    let mut environment = OwnedEnvironment::new_free(
        network.is_mainnet(),
        network.chain_id(),
        backing.as_clarity_db(),
        DEPLOYED_IN,
    );
    environment
        .initialize_versioned_contract(contract.clone(), DEPLOYED_VERSION, source, None)
        .expect("the reference implementation initializes it");
    let (mut database, _) = environment
        .destruct()
        .expect("the throwaway environment is not nested");
    database.begin();
    let definition = database
        .get_contract(contract)
        .expect("the definition it just wrote");
    database
        .roll_back()
        .expect("nothing from the throwaway store is kept");
    definition
}

/// Put a contract into state as a chain that still had `at-block` left it.
///
/// A contract *containing* `at-block` cannot be deployed at epoch 4.0 by either
/// engine — analysis refuses the word whatever the Clarity version, which is the
/// other half of stacks-core's pair of checks and is correct — so the only way
/// to reach the runtime check is the way mainnet reaches it: a contract
/// published years ago whose stored analysis names the epoch that accepted it.
///
/// The four writes are a deployment's own, in a deployment's order: the source
/// and its hash, which is what a rebuild compiles; the definition, which is
/// where a call finds the function it asks for; the data size, which the call
/// charges `LoadContract` for; and the analysis, which [[064]] made the compiled
/// semantics a function of.
fn plant(database: &mut ClarityDatabase<'_>, network: Network, source: &str) {
    let contract = id();
    let analysis = analysed_when_deployed(&contract, source);
    let definition = initialized_when_deployed(network, &contract, source);
    database.begin();
    database
        .insert_contract_hash(&contract, source)
        .expect("record the source");
    database
        .insert_contract(&contract, definition)
        .expect("record the definition");
    database
        .set_contract_data_size(&contract, 0)
        .expect("record the data size");
    database
        .set_metadata(
            &contract,
            AnalysisDatabase::storage_key(),
            &analysis.serialize(),
        )
        .expect("record the analysis the chain wrote when it accepted this contract");
    database.commit().expect("commit the planted contract");
}

/// The cost tracker a transaction is executed with, built the way
/// `Vm::transaction_cost_tracker` builds it.
///
/// It matters that this is not `LimitedCostTracker::new_free()`: a free tracker
/// accumulates nothing, so a cost assertion over one is vacuous. From epoch 4.0
/// the cost functions are native Rust and cost-voting is retired
/// (`load_costs`), so no cost contract has to be in state for this to succeed —
/// which is what makes a real tracker available over an empty store at all.
fn tracker(store: &mut MarfStore) -> LimitedCostTracker {
    let mut database = store.as_clarity_db();
    database.begin();
    database
        .set_clarity_epoch_version(StacksEpochId::Epoch40)
        .expect("declare the executing epoch");
    let tracker = LimitedCostTracker::new_mid_block(
        Network::TESTNET.is_mainnet(),
        Network::TESTNET.chain_id(),
        nano_vm::EPOCH_4_BLOCK_LIMIT,
        &mut database,
        StacksEpochId::Epoch40,
    )
    .expect("epoch 4.0's costs are native, so an empty store carries them");
    database
        .roll_back()
        .expect("reading the cost schedule writes nothing");
    tracker
}

/// The call, through the engine the node ships.
fn compiled(source: &str, function: &str, arguments: &[Value]) -> Answer {
    let directory = tempfile::tempdir().expect("a directory");
    let mut vm = Vm::open(Network::TESTNET, directory.path()).expect("open a state");
    vm.begin_block(None, [0x41; 32]).expect("begin a block");
    plant(&mut vm.clarity_db(), Network::TESTNET, source);
    let cost_tracker = vm
        .transaction_cost_tracker()
        .expect("a consensus cost tracker");
    answer(vm.execute_contract_call_outcome(
        id().issuer.into(),
        None,
        id(),
        function,
        &serialized(arguments),
        &cost_tracker,
    ))
}

/// The same call, through the reference interpreter, which is only an oracle.
fn interpreted(source: &str, function: &str, arguments: &[Value]) -> Answer {
    let mut store = MarfStore::new(Network::TESTNET).expect("create a store");
    store.begin(None, [0x42; 32]).expect("begin a block");
    plant(&mut store.as_clarity_db(), Network::TESTNET, source);
    let cost_tracker = tracker(&mut store);
    answer(nano_oracle::execute_contract_call_outcome(
        &mut store,
        id().issuer.into(),
        None,
        id(),
        function,
        &serialized(arguments),
        cost_tracker,
    ))
}

fn serialized(arguments: &[Value]) -> Vec<Vec<u8>> {
    arguments
        .iter()
        .map(|argument| {
            argument
                .serialize_to_vec()
                .expect("a consensus-serializable argument")
        })
        .collect()
}

/// stacks-core's own cost for this refusal, from its own consensus snapshot.
///
/// `blockstack_lib__chainstate__tests__runtime_analysis_tests__runtime_check_error_kind_at_block_unavailable_ccall.snap`,
/// the `Epoch34` block that calls `at-block-unavail-Epoch3_3-Clarity2`:
/// `vm_error: Some(AtBlockUnavailable)`, the transaction failed with `(err none)`,
/// the block accepted, and this cost. A hardcoded vector lifted from the
/// reference implementation's tests, which is the cheapest oracle that can
/// falsify a cost — and it is not obtainable from the chain, since no public API
/// serves a historical receipt.
const STACKS_CORE_REFUSAL_COST: ExecutionCost = ExecutionCost {
    write_length: 0,
    write_count: 0,
    read_length: 159,
    read_count: 3,
    runtime: 275,
};

/// The literal-hash contract: the error identity and every cost dimension, in
/// both engines and against stacks-core's own number.
///
/// The cost is the pin on *where* the refusal happens rather than only on what it
/// says. `special_at_block` refuses before `runtime_cost(AtBlock)`, and
/// clar2wasm charged `AtBlock` in `AtBlock::traverse` before it ever reached the
/// host gate — so this assertion fails with that charge restored, while the error
/// identity above it does not.
///
/// All five dimensions are asserted against stacks-core's snapshot because they
/// *match*, which was not assumed: the two read dimensions and the runtime were
/// measured first and turned out identical, so the weaker "the engines agree with
/// each other" is not what this settles for. `read_length` is 159 because
/// `LoadContract` charges the contract's size, which is why the source above is
/// copied byte for byte rather than paraphrased.
#[test]
fn an_old_contract_refuses_at_block_and_charges_nothing_for_it() {
    let compiled = compiled(LITERAL_HASH, "trigger-error", &[]);
    let interpreted = interpreted(LITERAL_HASH, "trigger-error", &[]);

    for (engine, answer) in [("compiler", &compiled), ("interpreter", &interpreted)] {
        assert!(
            answer.refused_at_block(),
            "{engine} did not refuse at-block: {}",
            answer.describe()
        );
        assert_eq!(
            answer.cost, STACKS_CORE_REFUSAL_COST,
            "{engine} charges a refused at-block differently from stacks-core"
        );
    }
}

/// The `reserve-v1` shape: a hash argument that can return on its own.
///
/// This is the assertion the host-function gate alone cannot satisfy, and it
/// fails without the compiler's refusal — `(err u1)` under the compiler against
/// `AtBlockUnavailable` under the interpreter, a transaction the chain fails and
/// nano succeeded at.
#[test]
fn a_hash_argument_that_can_return_does_not_get_to_run() {
    let compiled = compiled(SHORT_RETURNING_HASH, "guarded", &[Value::UInt(1)]);
    let interpreted = interpreted(SHORT_RETURNING_HASH, "guarded", &[Value::UInt(1)]);

    for (engine, answer) in [("compiler", &compiled), ("interpreter", &interpreted)] {
        assert!(
            answer.refused_at_block(),
            "{engine} evaluated the hash argument instead of refusing: {}",
            answer.describe()
        );
    }
    // 121, 3, 284 in both: `read_length` is this contract's own size and the
    // runtime is nine more than the literal-hash contract's for the one extra
    // argument the call type-checks. Nothing here is charged for the
    // `get-block-info?` the argument would have done.
    assert_eq!(
        compiled.cost, interpreted.cost,
        "neither engine charges for an argument it never evaluated"
    );
    assert_eq!(
        (compiled.cost.write_length, compiled.cost.write_count),
        (0, 0),
        "a refusal writes nothing, which is why a state root cannot see it"
    );
}

/// A branch that does not run raises nothing, in either engine.
///
/// The refusal is a property of the expression, not of the contract, so a
/// contract that merely *mentions* `at-block` is still callable — which is what
/// keeps this from being 8,668,096's mistake in another word.
#[test]
fn a_branch_that_never_reaches_at_block_still_answers() {
    for (engine, answer) in [
        ("compiler", compiled(UNTAKEN_BRANCH, "maybe", &[Value::Bool(false)])),
        (
            "interpreter",
            interpreted(UNTAKEN_BRANCH, "maybe", &[Value::Bool(false)]),
        ),
    ] {
        assert_eq!(
            answer.value,
            Some(Value::UInt(7)),
            "{engine} did not answer the branch it took: {}",
            answer.describe()
        );
    }

    // And the branch that does reach it refuses, so the test above is not
    // passing because the word became harmless.
    for (engine, answer) in [
        ("compiler", compiled(UNTAKEN_BRANCH, "maybe", &[Value::Bool(true)])),
        (
            "interpreter",
            interpreted(UNTAKEN_BRANCH, "maybe", &[Value::Bool(true)]),
        ),
    ] {
        assert!(
            answer.refused_at_block(),
            "{engine} took the branch and did not refuse: {}",
            answer.describe()
        );
    }
}
