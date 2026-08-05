//! The shape mainnet block 8,668,096 diverges on, in both engines.
//!
//! `SP1E0XBN9T4B10E9QMR7XMFJPMA19D77WY3KP2QKC.auto-alex-v3-endpoint-v2-02` has
//!
//! ```clarity
//! ok-value (match claimed-response claimed (ok (+ ok-value …)) err (err err))
//! ```
//!
//! — a `match` whose *error* branch binds the name `err`, which is also a native
//! function. The interpreter checks a binding's name where it binds it, so a
//! branch that is not taken never rejects, and the chain answers `rebase` with
//! `(ok u390)`. clar2wasm refused the contract at compile time instead, so
//! *every* call into it failed:
//!
//! ```text
//! compiler     failed: contract analysis failed: Internal error: Name already used ClarityName("err")
//! interpreter  (ok u390)
//! ```
//!
//! This is not the placeholder-layout family the blocks below it were: nothing
//! is laid out for the wrong type. It is a static rejection of something the
//! reference implementation only rejects dynamically, on the path that binds.
//!
//! The interpreter is the oracle and nothing else: clarity-wasm has to be the
//! engine that runs mainnet, so a disagreement is a compiler bug to fix.

use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::types::QualifiedContractIdentifier;
use clarity::vm::{ClarityVersion, Value};
use nano_primitives::Network;
use nano_vm::{MarfStore, Vm};

/// `auto-alex-v3-endpoint-v2-02`'s shape: a reserved name bound by one branch of
/// a `match`, and callers that take each branch.
const SHADOWED: &str = "
(define-private (claim (r (response uint uint)))
  (match r claimed (ok (+ claimed u1)) err (err err)))
(define-read-only (claim-ok) (claim (ok u389)))
(define-read-only (claim-err) (claim (err u7)))

;; The same on the `some` side, where the `none` branch binds nothing.
(define-private (unwrapped (o (optional uint)))
  (match o len (ok len) (ok u0)))
(define-read-only (unwrapped-none) (unwrapped none))
(define-read-only (unwrapped-some) (unwrapped (some u5)))
";

fn id(name: &str) -> QualifiedContractIdentifier {
    QualifiedContractIdentifier::parse(&format!("ST000000000000000000002AMW42H.{name}"))
        .expect("a contract identifier")
}

/// Both engines' answers for a call, described the same way so they compare.
fn both(function: &str) -> (String, String) {
    let mut wasm = Vm::new(Network::TESTNET).expect("create the compiling VM");
    wasm.begin_block(None, [0x41; 32]).expect("begin");
    wasm.deploy_contract(
        id("f"),
        ClarityVersion::Clarity3,
        SHADOWED,
        LimitedCostTracker::new_free(),
    )
    .expect("mainnet accepted this shape, so the compiler has to build it");

    let mut store = MarfStore::new(Network::TESTNET).expect("create the interpreter store");
    store.begin(None, [0x42; 32]).expect("begin");
    nano_oracle::deploy_contract(
        &mut store,
        id("f"),
        ClarityVersion::Clarity3,
        SHADOWED,
        LimitedCostTracker::new_free(),
    )
    .expect("deploy under the interpreter");

    let describe = |outcome: Result<nano_vm::ContractCallOutcome, _>| match outcome {
        Ok(
            nano_vm::ContractCallOutcome::Success(result)
            | nano_vm::ContractCallOutcome::AbortedByResponse(result),
        ) => format!("{:?}", result.value),
        Ok(nano_vm::ContractCallOutcome::RuntimeFailure { error, .. }) => {
            format!("failed: {error}")
        }
        Err(error) => format!("error: {error}"),
    };

    let compiled = describe(wasm.execute_contract_call_outcome(
        id("f").issuer.into(),
        None,
        id("f"),
        function,
        &[],
        &LimitedCostTracker::new_free(),
    ));
    let interpreted = describe(nano_oracle::execute_contract_call_outcome(
        &mut store,
        id("f").issuer.into(),
        None,
        id("f"),
        function,
        &[],
        LimitedCostTracker::new_free(),
    ));
    (compiled, interpreted)
}

/// The branch that does not bind the reserved name answers, as the chain's
/// `rebase` does.
#[test]
fn a_branch_that_does_not_bind_a_reserved_name_answers() {
    for (function, expected) in [
        ("claim-ok", Value::UInt(390)),
        ("unwrapped-none", Value::UInt(0)),
    ] {
        let (compiled, interpreted) = both(function);
        assert_eq!(compiled, interpreted, "{function}");
        assert!(
            compiled.contains(&format!("{expected:?}")),
            "{function} answered {compiled}, which does not carry {expected:?}"
        );
    }
}

/// The branch that binds it still fails, and fails the same way — the rule is
/// *when* the name is rejected, not whether.
#[test]
fn the_branch_that_binds_a_reserved_name_fails_in_both() {
    for function in ["claim-err", "unwrapped-some"] {
        let (compiled, interpreted) = both(function);
        assert!(
            compiled.starts_with("failed:") || compiled.starts_with("error:"),
            "{function} should have been rejected under the compiler: {compiled}"
        );
        assert!(
            interpreted.starts_with("failed:") || interpreted.starts_with("error:"),
            "{function} should have been rejected under the interpreter: {interpreted}"
        );
    }
}
