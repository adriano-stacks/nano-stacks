//! `block-height` is the tenure height from epoch 3.0, in both engines.
//!
//! The interpreter switched when Nakamoto arrived — `vm::variables`,
//! `NativeVariables::BlockHeight` — so that the value keeps incrementing at
//! roughly its old pace. clarity-wasm did not: its host function returned the
//! Stacks block height whatever the epoch.
//!
//! That is consensus-visible the moment a contract *stores* it. Mainnet block
//! 8,665,780 does: `univ2-core`'s `pools` map records `block-height` alongside a
//! pool's reserves, and the network wrote 251,323 where nano wrote 8,665,780 —
//! a state root that differed while every balance, nonce and reserve agreed.

use nano_primitives::Network;
use nano_vm::{MarfStore, Vm};

use clarity::vm::ClarityVersion;
use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::types::QualifiedContractIdentifier;

/// Stores what the keyword answers, which is what makes it consensus.
const SOURCE: &str = "
(define-data-var seen uint u0)
(define-public (remember) (begin (var-set seen block-height) (ok (var-get seen))))
";

fn contract() -> QualifiedContractIdentifier {
    QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.heights")
        .expect("a contract identifier")
}

#[test]
fn the_compiler_and_the_interpreter_answer_block_height_alike() {
    let mut wasm = Vm::new(Network::TESTNET).expect("create the compiling VM");
    wasm.begin_block(None, [0x41; 32]).expect("begin");
    wasm.deploy_contract(
        contract(),
        ClarityVersion::Clarity2,
        SOURCE,
        LimitedCostTracker::new_free(),
    )
    .expect("deploy");

    let mut store = MarfStore::new(Network::TESTNET).expect("create the interpreter store");
    store.begin(None, [0x42; 32]).expect("begin");
    nano_oracle::deploy_contract(
        &mut store,
        contract(),
        ClarityVersion::Clarity2,
        SOURCE,
        LimitedCostTracker::new_free(),
    )
    .expect("deploy");

    let sender = contract().issuer.into();
    let compiled = wasm
        .execute_contract_call_outcome(
            sender,
            None,
            contract(),
            "remember",
            &[],
            &LimitedCostTracker::new_free(),
        )
        .expect("the compiled call runs");
    let interpreted = nano_oracle::execute_contract_call_outcome(
        &mut store,
        contract().issuer.into(),
        None,
        contract(),
        "remember",
        &[],
        LimitedCostTracker::new_free(),
    )
    .expect("the interpreted call runs");

    let value = |outcome: nano_vm::ContractCallOutcome| match outcome {
        nano_vm::ContractCallOutcome::Success(result)
        | nano_vm::ContractCallOutcome::AbortedByResponse(result) => result.value,
        nano_vm::ContractCallOutcome::RuntimeFailure { error, .. } => {
            panic!("the call fails: {error:?}")
        }
    };
    assert_eq!(
        value(compiled),
        value(interpreted),
        "both engines answer block-height the same"
    );
}
