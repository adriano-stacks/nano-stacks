//! Who `tx-sender` is inside `as-contract`, in both engines.
//!
//! Mainnet block 8,666,423 diverges because a wrapped-STX transfer answers
//! `(err u2)` under the compiler and succeeds under the interpreter. From
//! `stx-transfer?`, `u2` means the sender and the recipient are the same
//! principal — so one of the two was computed differently, and both come from
//! `tx-sender` around an `as-contract`.
//!
//! `as-contract` has already been wrong once here, in how it typed its body.
//! This asks the other question about it: who it says you are, including when a
//! contract enters it and calls another contract that looks again.

use nano_primitives::Network;
use nano_vm::{MarfStore, Vm};

use clarity::vm::ClarityVersion;
use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::Value;
use clarity::vm::types::QualifiedContractIdentifier;
use stacks_common::codec::StacksMessageCodec;

/// Reports who it is asked as, plainly and through another contract.
const INNER: &str = "
(define-trait named ((who () (response principal uint))))
(define-public (who) (ok tx-sender))
";

const OUTER: &str = "
(use-trait named .inner.named)
(define-read-only (direct) (as-contract tx-sender))
(define-public (through) (as-contract (contract-call? .inner who)))
(define-read-only (nested) (as-contract (as-contract tx-sender)))
(define-read-only (after) (as-contract (let ((a tx-sender)) a)))
(define-read-only (named-target (t <named>)) (contract-of t))
(define-read-only (named-in-contract (t <named>)) (as-contract (contract-of t)))
";

fn inner() -> QualifiedContractIdentifier {
    QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.inner")
        .expect("a contract identifier")
}

fn outer() -> QualifiedContractIdentifier {
    QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.outer")
        .expect("a contract identifier")
}

fn answers(function: &str) -> (String, String) {
    answers_with(function, &[])
}

/// The trait argument a routing contract passes when it names a pool.
fn trait_argument() -> Vec<u8> {
    let mut bytes = Vec::new();
    Value::Principal(inner().into())
        .consensus_serialize(&mut bytes)
        .expect("serialize");
    bytes
}

fn answers_with(function: &str, arguments: &[Vec<u8>]) -> (String, String) {
    let mut wasm = Vm::new(Network::TESTNET).expect("create the compiling VM");
    wasm.begin_block(None, [0x31; 32]).expect("begin");
    for (contract, source) in [(inner(), INNER), (outer(), OUTER)] {
        wasm.deploy_contract(
            contract,
            ClarityVersion::Clarity3,
            source,
            LimitedCostTracker::new_free(),
        )
        .expect("deploy");
    }

    let mut store = MarfStore::new(Network::TESTNET).expect("create the interpreter store");
    store.begin(None, [0x32; 32]).expect("begin");
    for (contract, source) in [(inner(), INNER), (outer(), OUTER)] {
        nano_vm::deploy_contract(
            &mut store,
            contract,
            ClarityVersion::Clarity3,
            source,
            LimitedCostTracker::new_free(),
        )
        .expect("deploy");
    }

    let describe = |outcome: Result<nano_vm::ContractCallOutcome, _>| match outcome {
        Ok(
            nano_vm::ContractCallOutcome::Success(result)
            | nano_vm::ContractCallOutcome::AbortedByResponse(result),
        ) => format!("{:?}", result.value),
        Ok(nano_vm::ContractCallOutcome::RuntimeFailure { error, .. }) => {
            format!("failed: {error:?}")
        }
        Err(error) => format!("{error:?}"),
    };

    let compiled = describe(wasm.execute_contract_call_outcome(
        outer().issuer.into(),
        None,
        outer(),
        function,
        arguments,
        &LimitedCostTracker::new_free(),
    ));
    let interpreted = describe(nano_vm::execute_contract_call_outcome(
        &mut store,
        outer().issuer.into(),
        None,
        outer(),
        function,
        arguments,
        LimitedCostTracker::new_free(),
    ));
    (compiled, interpreted)
}

#[test]
fn both_engines_agree_on_who_as_contract_makes_you() {
    for function in ["direct", "through", "nested", "after"] {
        let (compiled, interpreted) = answers(function);
        assert_eq!(
            compiled, interpreted,
            "the engines agree on who `{function}` is run as"
        );
    }
}

#[test]
fn both_engines_agree_on_which_contract_a_trait_names() {
    // `contract-of` is how a routing contract learns where to send tokens. If it
    // answers with the caller instead of the trait's target, a transfer goes
    // from a contract to itself — which is exactly `stx-transfer?`'s `(err u2)`,
    // and exactly what mainnet block 8,666,423 does under the compiler.
    for function in ["named-target", "named-in-contract"] {
        let (compiled, interpreted) = answers_with(function, &[trait_argument()]);
        assert_eq!(
            compiled, interpreted,
            "the engines agree on which contract `{function}` names"
        );
    }
}

