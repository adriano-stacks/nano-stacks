//! What the one engine does when it cannot run something.
//!
//! `one_engine_in_the_artifact` proves the shipped binary has no second engine
//! to fall through to. That is the structural half. This is the behavioural
//! half: each of the three ways clarity-wasm can refuse a piece of work is
//! forced through the production boundary — `nano_vm::Vm`, the type
//! `nano-chainstate` executes every transaction with — and each has to refuse
//! without answering and without leaving anything behind.
//!
//! The three classes, and how each is forced. None needs a compiler bug, which
//! matters: a gate that could only be exercised while a divergence was open
//! would stop working the moment somebody fixed one.
//!
//! | class | forced by |
//! |---|---|
//! | compile refusal | a source naming a function that does not exist |
//! | module-load refusal | a `let` with 60,000 bindings — more wasm locals than wasmtime's validator accepts |
//! | runtime trap | `(- u0 u1)`, after a write |
//!
//! The module-load case took some finding. wasmtime's own limits are the only
//! way to make `clar2wasm` emit a module the runtime refuses without breaking
//! `clar2wasm`, and most are out of reach: a function's parameters are capped at
//! 256 by Clarity's *analyzer* long before wasm's limit of 1,000, and a contract
//! cannot be planted into state as bytes because its metadata is written once.
//! Locals are reachable, because a `let` binding becomes one and nothing between
//! the source and the validator counts them.
//!
//! ## Falling through would have changed the answer
//!
//! The `let` with 60,000 bindings is a positive control as well as a failure
//! case: the reference interpreter compiles nothing, so it **deploys and runs
//! that contract perfectly well**. Same source, same state, one engine refuses
//! and the other answers — which is exactly the shape of a compiler gap. nano
//! refusing it is nano declining to answer from the engine the chain does not
//! run on.

use clarity::vm::contexts::ContractContext;
use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier};
use clarity::vm::{ClarityVersion, Value};
use nano_primitives::Network;
use nano_vm::{ContractCallOutcome, MarfStore, Vm};

/// A source the compiler refuses: `no-such-word` resolves to nothing.
const UNRESOLVED: &str = "(define-public (f) (ok (no-such-word u1)))";

/// A source that writes and then aborts, so the write has something to be rolled
/// back from.
const TRAP: &str = "\
(define-data-var v uint u0)
(define-read-only (peek) (var-get v))
(define-public (f) (begin (var-set v u7) (ok (- u0 u1))))";

/// How many times each failure is repeated. A node retries a block it cannot
/// execute for as long as it runs, so "leaves nothing behind" has to hold on the
/// twentieth attempt as well as the first.
const ATTEMPTS: usize = 20;

/// A source that compiles and produces a module wasmtime will not load.
///
/// Every `let` binding is a wasm local, and wasmtime's validator accepts 50,000
/// of them. Nothing between Clarity's analyzer and the validator counts, so this
/// is a module the compiler emits and the runtime refuses — the one failure class
/// that is otherwise only reachable through a compiler bug.
fn too_many_locals() -> String {
    let bindings = (0..60_000_u32)
        .map(|index| format!("(a{index} u1)"))
        .collect::<Vec<_>>()
        .join(" ");
    format!("(define-public (f) (ok (let ({bindings}) a0)))")
}

fn contract(name: &str) -> QualifiedContractIdentifier {
    QualifiedContractIdentifier::parse(&format!("ST000000000000000000002AMW42H.{name}"))
        .expect("valid contract identifier")
}

fn sender(contract: &QualifiedContractIdentifier) -> PrincipalData {
    contract.issuer.clone().into()
}

/// A VM standing in one block, with nothing in it.
fn started() -> Vm {
    let mut vm = Vm::new(Network::TESTNET).expect("create VM");
    vm.begin_block(None, [9; 32]).expect("begin block");
    vm
}

fn pending(vm: &Vm) -> [u8; 32] {
    vm.pending_state_root().expect("the pending root").0
}

/// Put a contract into state whose source the compiler cannot build.
///
/// This is the state a node meets after a checkpoint import, or after any block
/// it did not execute itself: a contract the network accepted, sitting in the
/// MARF, whose module this build of the compiler will not produce. It is written
/// through the same three database writes a deployment makes, so what a call
/// finds is indistinguishable from a deployed contract — which is the point,
/// since a deployment through this boundary would have been refused.
fn plant(vm: &mut Vm, contract: &QualifiedContractIdentifier, source: &str) {
    let mut database = vm.clarity_db();
    database.begin();
    database
        .insert_contract_hash(contract, source)
        .expect("record the source");
    database
        .insert_contract(
            contract,
            ContractContext::new(contract.clone(), ClarityVersion::Clarity6).into(),
        )
        .expect("record the contract");
    database
        .set_contract_data_size(contract, 0)
        .expect("record the data size");
    database.commit().expect("commit the planted contract");
}

fn call(vm: &mut Vm, contract: &QualifiedContractIdentifier, function: &str) -> ContractCallOutcome {
    vm.execute_contract_call_outcome(
        sender(contract),
        None,
        contract.clone(),
        function,
        &[],
        &LimitedCostTracker::new_free(),
    )
    .unwrap_or_else(|error| panic!("the boundary reports {function} rather than raising: {error:?}"))
}

/// The failure a `RuntimeFailure` outcome carries, or a panic naming what came
/// back instead.
fn failure(outcome: ContractCallOutcome) -> String {
    match outcome {
        ContractCallOutcome::RuntimeFailure { error, .. } => error.to_string(),
        ContractCallOutcome::Success(result) | ContractCallOutcome::AbortedByResponse(result) => {
            panic!("the call answered {:?} instead of failing", result.value)
        }
    }
}

/// A deployment the compiler refuses is refused, repeatedly, and writes nothing.
#[test]
fn a_deployment_the_compiler_refuses_leaves_no_state() {
    for (name, source) in [
        ("unresolved", UNRESOLVED.to_owned()),
        ("unloadable", too_many_locals()),
    ] {
        let mut vm = started();
        let empty = pending(&vm);
        for attempt in 0..ATTEMPTS {
            let refused = vm
                .deploy_contract(
                    contract(name),
                    ClarityVersion::Clarity6,
                    &source,
                    LimitedCostTracker::new_free(),
                )
                .expect_err("the compiler refuses this source");
            let complaint = refused.to_string();
            assert!(
                nano_vm::is_contract_analysis_failure_message(&complaint),
                "attempt {attempt} on {name} failed for a reason the boundary does \
                 not recognize as the compiler's: {complaint}"
            );
            assert_eq!(
                hex::encode(pending(&vm)),
                hex::encode(empty),
                "attempt {attempt} on {name} left state behind"
            );
        }
        // And the contract is not in state under any guise: a deployment that
        // half-happened would leave a readable source or a contract analysis, and
        // the next block's call into it would find something.
        assert!(
            vm.contract_source(&contract(name)).is_err(),
            "{name} is readable from state after a refused deployment"
        );
    }
}

/// The module-load refusal is a real one, distinct from the compile refusal.
///
/// Both are the compiler's business and the boundary treats them the same way,
/// which is why they are easy to conflate — so this pins that the second source
/// gets past analysis and codegen and is stopped by the *runtime*, naming the
/// module. Otherwise a change that made `too_many_locals` fail analysis instead
/// would leave the load path untested and everything above still green.
#[test]
fn a_module_the_runtime_will_not_load_is_named_as_a_module() {
    let mut vm = started();
    let refused = vm
        .deploy_contract(
            contract("unloadable_named"),
            ClarityVersion::Clarity6,
            &too_many_locals(),
            LimitedCostTracker::new_free(),
        )
        .expect_err("the runtime refuses this module")
        .to_string();
    assert!(
        refused.contains("compiles to a module that will not load"),
        "this source no longer reaches the module-load check, so that class of \
         failure is no longer being forced: {refused}"
    );
    assert!(
        refused.contains("too many locals"),
        "the module-load refusal does not say what the runtime objected to: {refused}"
    );
}

/// A call the compiler cannot build answers nothing and writes nothing.
///
/// This is the shape a compiler gap actually takes on a running node: the
/// contract is already in state, the network ran it, and this build cannot
/// produce its module. Both refusal classes are forced, because a checkpointed
/// node meets both.
#[test]
fn a_call_the_compiler_cannot_build_answers_nothing_and_writes_nothing() {
    for (name, source) in [
        ("planted_unresolved", UNRESOLVED.to_owned()),
        ("planted_unloadable", too_many_locals()),
    ] {
        let mut vm = started();
        plant(&mut vm, &contract(name), &source);
        let planted = pending(&vm);
        for attempt in 0..ATTEMPTS {
            let complaint = failure(call(&mut vm, &contract(name), "f"));
            assert!(
                nano_vm::is_contract_analysis_failure_message(&complaint),
                "attempt {attempt} on {name} failed for a reason the boundary does \
                 not recognize as the compiler's: {complaint}"
            );
            assert!(
                complaint.contains(&contract(name).to_string()),
                "attempt {attempt} on {name} does not name the contract that could \
                 not be built: {complaint}"
            );
            assert_eq!(
                hex::encode(pending(&vm)),
                hex::encode(planted),
                "attempt {attempt} on {name} left state behind"
            );
        }
    }
}

/// A runtime trap rolls back the writes that ran before it.
#[test]
fn a_runtime_trap_rolls_back_the_writes_that_preceded_it() {
    let mut vm = started();
    let identifier = contract("trap");
    vm.deploy_contract(
        identifier.clone(),
        ClarityVersion::Clarity6,
        TRAP,
        LimitedCostTracker::new_free(),
    )
    .expect("the trapping contract itself deploys");
    let deployed = pending(&vm);

    for attempt in 0..ATTEMPTS {
        let complaint = failure(call(&mut vm, &identifier, "f"));
        assert!(
            complaint.contains("ArithmeticUnderflow"),
            "attempt {attempt} trapped for the wrong reason: {complaint}"
        );
        // Not the compiler's fault, and not reported as if it were: a trap is a
        // transaction that failed, and the boundary has to tell the two apart or
        // a compiler gap disappears into the ordinary failures.
        assert!(
            !nano_vm::is_contract_analysis_failure_message(&complaint),
            "attempt {attempt} reported a runtime trap as a compiler failure"
        );
        assert_eq!(
            vm.call_contract_values(&sender(&identifier), &identifier, "peek", &[])
                .expect("read the variable back"),
            Value::UInt(0),
            "attempt {attempt} kept the write the trapping transaction made"
        );
        assert_eq!(
            hex::encode(pending(&vm)),
            hex::encode(deployed),
            "attempt {attempt} left state behind"
        );
    }
}

/// The interpreter would have answered, and nano did not ask it.
///
/// The positive control for the whole file. `too_many_locals` is a contract the
/// reference interpreter deploys and runs without complaint — it compiles
/// nothing, so wasmtime's limit does not exist for it — and the production
/// boundary refuses. That is a real disagreement between the two engines,
/// manufactured rather than waited for, and nano's answer to it is "no".
///
/// Without this, every assertion above is consistent with a boundary that
/// refuses everything.
#[test]
fn the_engine_the_node_does_not_have_would_have_answered() {
    let source = too_many_locals();
    let identifier = contract("would_have_worked");

    // The oracle, on the same store type and the same block.
    let mut store = MarfStore::new(Network::TESTNET).expect("create store");
    store.begin(None, [9; 32]).expect("begin block");
    nano_oracle::deploy_contract(
        &mut store,
        identifier.clone(),
        ClarityVersion::Clarity6,
        &source,
        LimitedCostTracker::new_free(),
    )
    .expect("the interpreter deploys a contract with sixty thousand let bindings");
    let interpreted = nano_oracle::execute_contract_call_outcome(
        &mut store,
        sender(&identifier),
        None,
        identifier.clone(),
        "f",
        &[],
        LimitedCostTracker::new_free(),
    )
    .expect("the interpreter runs it");
    assert!(
        matches!(interpreted, ContractCallOutcome::Success(_)),
        "the interpreter did not succeed either, so this control says nothing: \
         {interpreted:?}"
    );

    // And the engine the node ships with refuses, on both paths into it.
    let mut vm = started();
    vm.deploy_contract(
        identifier.clone(),
        ClarityVersion::Clarity6,
        &source,
        LimitedCostTracker::new_free(),
    )
    .expect_err("the shipped engine refuses the deployment the interpreter accepted");
    plant(&mut vm, &identifier, &source);
    let complaint = failure(call(&mut vm, &identifier, "f"));
    assert!(
        nano_vm::is_contract_analysis_failure_message(&complaint),
        "the shipped engine did not refuse the call the interpreter answered: {complaint}"
    );
}

/// What a sealed root cannot tell you, written down because it decides which
/// gate catches a compiler gap.
///
/// A call the compiler cannot build is reported as a **failed transaction**, not
/// as a refusal to execute the block — deliberately, because a deployment naming
/// a function that does not exist is an ordinary failed mainnet transaction and
/// has to stay one. The consequence is that a compiler gap on an *already
/// deployed* contract, which can only ever be a gap, arrives at the block as a
/// transaction that aborted, and a transaction that aborted writes nothing. So
/// the sealed root is the same one an untouched block seals, and the same one a
/// legitimate `ArithmeticUnderflow` seals.
///
/// The state-root check therefore cannot distinguish a compiler gap from an
/// abort the network also made. **Receipts can**, which is why the mainnet
/// replay gate asserts them (`mainnet_accounting`, `event_observer`) and why
/// [[053]] does not accept a root-only replay as evidence. This test pins that
/// reasoning so it cannot be quietly forgotten, and it is the argument for
/// `nano-vm` eventually telling a refusal-at-a-call apart from a
/// refusal-at-a-deploy — recorded on task 060.
#[test]
fn a_compiler_refusal_at_a_call_is_invisible_in_the_sealed_root() {
    let untouched = {
        let vm = started();
        pending(&vm)
    };

    let refused = {
        let mut vm = started();
        let identifier = contract("invisible");
        plant(&mut vm, &identifier, UNRESOLVED);
        let before = pending(&vm);
        let _ = failure(call(&mut vm, &identifier, "f"));
        assert_eq!(
            hex::encode(pending(&vm)),
            hex::encode(before),
            "the refused call wrote something after all, which would make it \
             visible in the root and this test wrong"
        );
        before
    };

    // The planted contract is itself state, so the two roots differ by exactly
    // that and by nothing the failing call did. What is being asserted is the
    // second half: the call contributed nothing either way.
    assert_ne!(
        hex::encode(refused),
        hex::encode(untouched),
        "planting a contract changed no state, so this test is not measuring what \
         it thinks"
    );
}
