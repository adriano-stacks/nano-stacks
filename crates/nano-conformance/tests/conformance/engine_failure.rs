//! What the one engine does when it cannot run something.
//!
//! `one_engine_in_the_artifact` proves the shipped binary has no second engine
//! to fall through to. That is the structural half. This is the behavioural
//! half: each of the three ways clarity-wasm can refuse a piece of work is
//! forced through the production implementation. Compile and runtime failures
//! enter through `nano_vm::Vm`, the type `nano-chainstate` executes every
//! transaction with; malformed Wasm enters the same validator helper the VM
//! uses before instantiation.
//!
//! The three classes, and how each is forced. None needs a compiler bug, which
//! matters: a gate that could only be exercised while a divergence was open
//! would stop working the moment somebody fixed one.
//!
//! | class | forced by |
//! |---|---|
//! | compile refusal | a source naming a function that does not exist |
//! | module-load refusal | malformed Wasm bytes passed through nano-vm's production `loadable` boundary in its unit suite |
//! | runtime trap | `(- u0 u1)`, after a write |
//!
//! A valid Clarity program is deliberately not used to force module-load
//! failure. Keeping one would preserve a known compiler differential as a test
//! fixture and make fixing the compiler break the failure-path suite.

use clarity::vm::analysis::{AnalysisDatabase, ContractAnalysis};
use clarity::vm::contexts::ContractContext;
use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::database::ClaritySerializable;
use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier};
use clarity::vm::{ClarityVersion, Value};
use nano_primitives::Network;
use nano_vm::{ContractCallOutcome, Vm};
use stacks_common::types::StacksEpochId;

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
/// through the same four database writes a deployment makes, so what a call
/// finds is indistinguishable from a deployed contract — which is the point,
/// since a deployment through this boundary would have been refused.
///
/// The fourth is the contract analysis, and it is here because [[064]] made the
/// epoch a rebuild compiles under a fact read off the chain rather than one the
/// compiler picks by trying epochs. A contract with no stored analysis is now
/// refused outright, and rightly: stacks-core writes the analysis and the
/// contract in one transaction and a checkpoint copies the whole metadata table,
/// so half a deploy is a state no chain produces. The analysis planted here is
/// deliberately not this source's — an empty one naming epoch 4.0 — because what
/// a call needs from it is the epoch it was judged in, and the whole premise of
/// this file is that the source beside it cannot be built.
fn plant(vm: &mut Vm, contract: &QualifiedContractIdentifier, source: &str) {
    let analysis = ContractAnalysis::new(
        contract.clone(),
        Vec::new(),
        LimitedCostTracker::new_free(),
        StacksEpochId::Epoch40,
        ClarityVersion::Clarity6,
    );
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
    database
        .set_metadata(
            contract,
            AnalysisDatabase::storage_key(),
            &analysis.serialize(),
        )
        .expect("record the analysis the chain wrote when it accepted this contract");
    database.commit().expect("commit the planted contract");
}

fn call(
    vm: &mut Vm,
    contract: &QualifiedContractIdentifier,
    function: &str,
) -> ContractCallOutcome {
    vm.execute_contract_call_outcome(
        sender(contract),
        None,
        contract.clone(),
        function,
        &[],
        &LimitedCostTracker::new_free(),
    )
    .unwrap_or_else(|error| {
        panic!("the boundary reports {function} rather than raising: {error:?}")
    })
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
    let name = "unresolved";
    let mut vm = started();
    let empty = pending(&vm);
    for attempt in 0..ATTEMPTS {
        let refused = vm
            .deploy_contract(
                contract(name),
                ClarityVersion::Clarity6,
                UNRESOLVED,
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

/// A call the compiler cannot build answers nothing and writes nothing.
///
/// This is the shape a compiler gap actually takes on a running node: the
/// contract is already in state, the network ran it, and this build cannot
/// produce its module.
#[test]
fn a_call_the_compiler_cannot_build_answers_nothing_and_writes_nothing() {
    let name = "planted_unresolved";
    let mut vm = started();
    plant(&mut vm, &contract(name), UNRESOLVED);
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
