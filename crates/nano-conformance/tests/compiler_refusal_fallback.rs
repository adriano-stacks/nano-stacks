//! A contract the compiler cannot build still deploys, because mainnet built it.
//!
//! Mainnet block 8,667,467 diverges on a deployment of
//! `SP1A27KFY4XERQCCRCARCYD1CC5N7M6688BSYADJ7.v0-egroup`, which clar2wasm
//! compiles into a module wasmtime refuses: "expected i64, found i32".
//!
//! The cause is a third instance of a known clarity-wasm typechecker
//! limitation. A tuple literal bound in a `let` types a bare `none` as
//! `(optional NoType)` — one wasm slot — and `fold` then reads it where the
//! accumulator's `(optional uint)` needs three. Passing the same tuple *inline*
//! works, because `fold` sets the expected type on the expression it is about
//! to lay out; a `let` has already stored the narrow one by then.
//! `words/tuples.rs` carries two workarounds for the same fault, one of them
//! written for this project.
//!
//! Fixing the compiler properly means resolving `NoType` placeholders from
//! usage, which is type unification and not a small change. But the chain does
//! not need it: mainnet runs the *interpreter*, so when the compiler refuses a
//! contract the interpreter decides. It deploys what is sound and rejects what
//! is not, and either way the block carries on rather than stopping on a
//! codegen bug.

use clarity::vm::ClarityVersion;
use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::types::QualifiedContractIdentifier;
use nano_primitives::Network;
use nano_vm::Vm;

/// The shape reduced from `v0-egroup`, by delta-debugging its 49 forms to four
/// and then by hand to two. The iterator is the identity: nothing about *it* is
/// wrong, which is what says the fault is in how `init` was stored.
const REFUSED: &str = "
(define-private (it (m uint) (acc {t: uint, r: (optional uint)})) acc)
(define-private (f (target uint) (masks (list 128 uint)))
  (let ((init { t: target, r: none }))
    (get r (fold it masks init))))
(define-read-only (g) (f u1 (list u1 u2)))
";

/// The same tuple passed inline, which is the case that already worked.
const ACCEPTED: &str = "
(define-private (it (m uint) (acc {t: uint, r: (optional uint)})) acc)
(define-private (f (target uint) (masks (list 128 uint)))
  (get r (fold it masks { t: target, r: none })))
(define-read-only (g) (f u1 (list u1 u2)))
";

/// Not a compiler problem: no parser accepts it, so no engine may deploy it.
const UNSOUND: &str = "(define-read-only (g) (unwrap-panic (map-get? absent u1)))";

fn deploys(name: &str, source: &str) -> Result<(), String> {
    let mut vm = Vm::new(Network::MAINNET).expect("create the VM");
    vm.begin_block(None, [0x31; 32]).expect("begin a block");
    let contract = QualifiedContractIdentifier::parse(&format!(
        "SP1A27KFY4XERQCCRCARCYD1CC5N7M6688BSYADJ7.{name}"
    ))
    .expect("a contract identifier");
    vm.deploy_contract(
        contract,
        ClarityVersion::Clarity3,
        source,
        LimitedCostTracker::new_free(),
    )
    .map(|_| ())
    .map_err(|error| error.to_string())
}

#[test]
fn a_contract_the_compiler_refuses_is_deployed_by_the_interpreter() {
    deploys("refused", REFUSED).expect("the interpreter deploys what the compiler refused");
}

#[test]
fn the_case_that_already_worked_still_does() {
    deploys("accepted", ACCEPTED).expect("an inline accumulator still compiles");
}

#[test]
fn an_unsound_contract_is_still_rejected() {
    // The fallback must not turn every bad deploy into a good one: a contract
    // neither engine can build has to stay refused, or a block accepts a
    // deployment mainnet rejected.
    let error = deploys("unsound", UNSOUND).expect_err("an unsound contract is refused");
    assert!(
        nano_vm::is_contract_analysis_failure_message(&error),
        "it is refused as an analysis failure, so the block carries on: {error}"
    );
}
