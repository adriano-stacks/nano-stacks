//! The shape mainnet block 8,668,161 diverges on, in both engines.
//!
//! `SP2H674PRTZV6YW56K0FMR7GDGZE4ZC5HMYZ3CDEV.hilt::ss` is reached through
//! `.loto::ri`, a router whose only body is `(contract-call? t ss b)` on a
//! trait reference. Under clarity-wasm one element of its answer comes back
//! `(err u2)` where the chain and the interpreter say `(err u9)` — and `u2`
//! from `stx-transfer?` means sender and recipient were the same principal, so
//! a principal is being computed wrongly somewhere in here.
//!
//! `ss` is
//!
//! ```clarity
//! (ok (map sr (get s (fold sl (unwrap-panic (slice? kft u0 …)) { b: b, s: (list) }))))
//! ```
//!
//! which puts two known-awkward shapes together: a tuple literal whose `(list)`
//! has no element type until the fold's accumulator gives it one, and a fold
//! over a list of *trait references*. `v0-egroup` at 8,667,467 was the same
//! family with `none` instead of `(list)`.
//!
//! The interpreter is the oracle here and nothing else: clarity-wasm has to be
//! the engine that runs mainnet, so a disagreement is a compiler bug to fix.

use clarity::vm::ClarityVersion;
use clarity::vm::Value;
use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::types::QualifiedContractIdentifier;
use nano_primitives::Network;
use nano_vm::{MarfStore, Vm};
use stacks_common::codec::StacksMessageCodec;

/// What the folded-over list holds, and what it is asked for.
const TARGET: &str = "
(define-trait ft ((who () (response principal uint))))
(define-public (who) (ok tx-sender))
";

/// `hilt`'s shape: an empty list in a tuple literal used as a fold accumulator,
/// folded over trait references, then mapped.
const ROUTER: &str = "
(use-trait ft .a.ft)
(define-private (step (ai <ft>) (it { n: uint, s: (list 4 principal) }))
  { n: (+ (get n it) u1), s: (unwrap-panic (as-max-len? (append (get s it) (contract-of ai)) u4)) })
(define-private (name (p principal)) p)
(define-read-only (collect (ts (list 4 <ft>)))
  (get s (fold step ts { n: u0, s: (list) })))
(define-read-only (counted (ts (list 4 <ft>)))
  (get n (fold step ts { n: u0, s: (list) })))
(define-read-only (mapped (ts (list 4 <ft>)))
  (map name (get s (fold step ts { n: u0, s: (list) }))))

(define-read-only (after-arg)
  (let ((before tx-sender))
    (let ((inside (as-contract tx-sender)))
      { before: before, inside: inside, after: tx-sender })))
(define-read-only (two-args)
  { a: (as-contract tx-sender), b: tx-sender })
(define-read-only (bound-then-as-contract)
  (let ((v3 tx-sender) (other (as-contract tx-sender)))
    { v3: v3, other: other, same: (is-eq v3 other) }))

;; `hilt::sr` returns `(err u9)` from an `asserts!` *inside* `as-contract`, and
;; `map` then runs it again. If unwinding out of `as-contract` that way leaves
;; the sender switched, the next call believes it is the contract.
(define-private (leave-early (i uint))
  (if (is-eq i u0)
      (as-contract (begin (asserts! false (err u9)) (ok tx-sender)))
      (ok tx-sender)))
(define-read-only (early-return-then-read) (map leave-early (list u0 u1)))
(define-private (failing) (if true (err u9) (ok tx-sender)))
(define-private (try-early (i uint))
  (if (is-eq i u0)
      (as-contract (begin (try! (failing)) (ok tx-sender)))
      (ok tx-sender)))
(define-read-only (try-out-of-as-contract) (map try-early (list u0 u1)))
";

fn id(name: &str) -> QualifiedContractIdentifier {
    QualifiedContractIdentifier::parse(&format!("ST000000000000000000002AMW42H.{name}"))
        .expect("a contract identifier")
}

fn serialized(value: &Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    value.consensus_serialize(&mut bytes).expect("serialize");
    bytes
}

/// A list of trait references, as a router passes them.
fn targets() -> Vec<u8> {
    serialized(
        &Value::cons_list_unsanitized(vec![
            Value::Principal(id("a").into()),
            Value::Principal(id("b").into()),
            Value::Principal(id("a").into()),
        ])
        .expect("a list"),
    )
}

fn both(function: &str, arguments: &[Vec<u8>]) -> (String, String) {
    let contracts = [(id("a"), TARGET), (id("b"), TARGET), (id("r"), ROUTER)];

    let mut wasm = Vm::new(Network::TESTNET).expect("create the compiling VM");
    wasm.begin_block(None, [0x41; 32]).expect("begin");
    for (contract, source) in &contracts {
        wasm.deploy_contract(
            contract.clone(),
            ClarityVersion::Clarity3,
            source,
            LimitedCostTracker::new_free(),
        )
        .expect("deploy under the compiler");
    }

    let mut store = MarfStore::new(Network::TESTNET).expect("create the interpreter store");
    store.begin(None, [0x42; 32]).expect("begin");
    for (contract, source) in &contracts {
        nano_vm::deploy_contract(
            &mut store,
            contract.clone(),
            ClarityVersion::Clarity3,
            source,
            LimitedCostTracker::new_free(),
        )
        .expect("deploy under the interpreter");
    }

    let describe = |outcome: Result<nano_vm::ContractCallOutcome, _>| match outcome {
        Ok(
            nano_vm::ContractCallOutcome::Success(result)
            | nano_vm::ContractCallOutcome::AbortedByResponse(result),
        ) => format!("{:?}", result.value),
        Ok(nano_vm::ContractCallOutcome::RuntimeFailure { error, .. }) => format!("failed: {error}"),
        Err(error) => format!("error: {error}"),
    };

    let compiled = describe(wasm.execute_contract_call_outcome(
        id("r").issuer.into(),
        None,
        id("r"),
        function,
        arguments,
        &LimitedCostTracker::new_free(),
    ));
    let interpreted = describe(nano_vm::execute_contract_call_outcome(
        &mut store,
        id("r").issuer.into(),
        None,
        id("r"),
        function,
        arguments,
        LimitedCostTracker::new_free(),
    ));
    (compiled, interpreted)
}

/// `sr` binds `(v3 tx-sender)` and then hands `(as-contract tx-sender)` to a
/// transfer whose sender is `v3`. `(err u2)` is that token reporting sender and
/// recipient equal — so under the compiler those two principals came out the
/// same, and the question is whether `as-contract` restores the sender it
/// changed when it appears as an argument beside a reader of `tx-sender`.
#[test]
fn as_contract_beside_a_reader_of_tx_sender_agrees() {
    for function in ["after_arg", "two_args", "bound_then_as_contract"] {
        let (compiled, interpreted) = both(&function.replace('_', "-"), &[]);
        assert_eq!(
            compiled, interpreted,
            "`{function}` sees the same principals in both engines"
        );
    }
}

/// Unwinding out of `as-contract` must put `tx-sender` back.
///
/// This is the shape `hilt::sr` actually takes: it enters `as-contract`, hits
/// `(asserts! (>= v5 v2) (err u9))`, and returns from inside it — and `map`
/// then calls it a second time. `sr` on either chunk *alone* agrees between the
/// engines; only the two in sequence disagree, which points at what the first
/// call leaves behind rather than at what either computes.
#[test]
fn returning_out_of_as_contract_restores_the_sender() {
    for function in ["early-return-then-read", "try-out-of-as-contract"] {
        let (compiled, interpreted) = both(function, &[]);
        assert_eq!(
            compiled, interpreted,
            "`{function}`: the call after an early return out of `as-contract` \
             sees the same sender in both engines"
        );
    }
}

#[test]
fn an_empty_list_in_a_fold_accumulator_collects_the_same_principals() {
    let (compiled, interpreted) = both("collect", &[targets()]);
    assert_eq!(
        compiled, interpreted,
        "both engines fold trait references into an initially empty list alike"
    );
}

#[test]
fn the_other_field_of_the_accumulator_survives() {
    let (compiled, interpreted) = both("counted", &[targets()]);
    assert_eq!(
        compiled, interpreted,
        "the tuple's other field is not disturbed by the empty list beside it"
    );
}

#[test]
fn mapping_over_the_folded_list_agrees() {
    // `hilt::ss` maps over the fold's result, which is where its wrong
    // principal becomes visible to the caller.
    let (compiled, interpreted) = both("mapped", &[targets()]);
    assert_eq!(
        compiled, interpreted,
        "mapping over the folded list gives the same principals"
    );
}
