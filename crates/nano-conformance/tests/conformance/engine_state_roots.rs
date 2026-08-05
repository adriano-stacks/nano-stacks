//! Whether the two execution engines seal the same state root.
//!
//! nano can answer a call clarity-wasm refuses by asking the interpreter, which
//! is how a mainnet replay gets past a compiler bug. That is only sound if the
//! two write the same keys in the same order: a MARF packs a node's pointers in
//! the order its keys were first written, so two runs reaching identical values
//! by different routes seal different roots — and a root is consensus.
//!
//! So this asks the question directly rather than assuming either answer. Same
//! contract, same call, same starting state, one run through each engine, and
//! the sealed roots compared.

use nano_primitives::Network;
use nano_vm::{MarfStore, Vm};

use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::types::QualifiedContractIdentifier;
use clarity::vm::{ClarityVersion, Value};
use stacks_common::codec::StacksMessageCodec;

/// Several writes of different shapes, in an order the two engines could
/// plausibly disagree about: a map and two variables, touched under a branch.
const WRITER: &str = r"
(define-data-var a uint u0)
(define-data-var b uint u0)
(define-map m uint uint)
(define-public (go (n uint))
  (begin
    (map-set m n (+ n u1))
    (var-set a (+ (var-get a) n))
    (map-set m (+ n u1) n)
    (var-set b (+ (var-get b) (var-get a)))
    (ok (var-get b))))
";

fn contract() -> QualifiedContractIdentifier {
    QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.writer")
        .expect("valid contract identifier")
}

fn argument(value: u128) -> Vec<u8> {
    let mut encoded = Vec::new();
    Value::UInt(value)
        .consensus_serialize(&mut encoded)
        .expect("serialize");
    encoded
}

/// The block both runs are executed in, so a root difference is a write
/// difference rather than a difference of where the writes landed.
const BLOCK: [u8; 32] = [7; 32];

/// Deploy the contract and make the call through the compiler.
fn compiled() -> ([u8; 32], Value) {
    let mut vm = Vm::new(Network::TESTNET).expect("create VM");
    vm.begin_block(None, BLOCK).expect("begin block");
    vm.deploy_contract(
        contract(),
        ClarityVersion::Clarity6,
        WRITER,
        LimitedCostTracker::new_free(),
    )
    .expect("the contract deploys");
    let outcome = vm
        .execute_contract_call_outcome(
            contract().issuer.into(),
            None,
            contract(),
            "go",
            &[argument(3)],
            &LimitedCostTracker::new_free(),
        )
        .expect("the call runs");
    (vm.seal_block().expect("seal").0, value_of(outcome))
}

/// The same, through the interpreter.
fn interpreted() -> ([u8; 32], Value) {
    let mut store = MarfStore::new(Network::TESTNET).expect("create store");
    store.begin(None, BLOCK).expect("begin block");
    nano_oracle::deploy_contract(
        &mut store,
        contract(),
        ClarityVersion::Clarity6,
        WRITER,
        LimitedCostTracker::new_free(),
    )
    .expect("the contract deploys");
    let outcome = nano_oracle::execute_contract_call_outcome(
        &mut store,
        contract().issuer.into(),
        None,
        contract(),
        "go",
        &[argument(3)],
        LimitedCostTracker::new_free(),
    )
    .expect("the call runs");
    (store.seal().expect("seal").0, value_of(outcome))
}

fn value_of(outcome: nano_vm::ContractCallOutcome) -> Value {
    match outcome {
        nano_vm::ContractCallOutcome::Success(result)
        | nano_vm::ContractCallOutcome::AbortedByResponse(result) => {
            result.value.expect("the call returns a value")
        }
        nano_vm::ContractCallOutcome::RuntimeFailure { error, .. } => {
            panic!("the call fails: {error:?}")
        }
    }
}

#[test]
fn the_interpreter_and_the_compiler_seal_the_same_state_root() {
    let (compiled_root, compiled_value) = compiled();
    let (interpreted_root, interpreted_value) = interpreted();

    assert_eq!(
        compiled_value, interpreted_value,
        "the two engines return the same value"
    );
    assert_eq!(
        hex::encode(compiled_root),
        hex::encode(interpreted_root),
        "the two engines seal the same state root, so answering from the interpreter when the \
         compiler refuses a call keeps a replay on the chain"
    );
}
