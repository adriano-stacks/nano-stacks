//! A `let`-bound tuple carrying a placeholder is laid out for the use, not the
//! binding.
//!
//! Mainnet block 8,667,467 deploys
//! `SP1A27KFY4XERQCCRCARCYD1CC5N7M6688BSYADJ7.v0-egroup`, which clar2wasm built
//! into a module wasmtime refused: "expected i64, found i32".
//!
//! Delta-debugging its 49 top-level forms down to two names it. A `let` stores
//! a binding laid out for the type its *value* analysed as, and
//! `{ t: target, r: none }` analyses `none` as `(optional NoType)` — an
//! indicator and one `i32`, where `(optional uint)` is an indicator and two
//! `i64`s. `fold` then sets its accumulator's type on the expression it is
//! about to read, and reads a value two slots short.
//!
//! The `let` cannot know: the type it needs comes from a use it has not reached
//! yet. So the widening happens where both types are in hand — at the read —
//! and the placeholder slot is dropped for zeros the indicator already says are
//! absent.
//!
//! Passing the same tuple *inline* always worked, because `fold` sets the type
//! on the tuple literal itself before it is laid out. An empty `(list)` in the
//! same position always worked too: a sequence is an offset and a length
//! whatever it holds, so there is nothing to widen.

use clarity::vm::ClarityVersion;
use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::types::QualifiedContractIdentifier;
use nano_primitives::Network;
use nano_vm::Vm;

/// The shape reduced from `v0-egroup`, by delta-debugging its 49 forms to four
/// and then by hand to two. The iterator is the identity: nothing about *it* is
/// wrong, which is what says the fault was in how `init` was stored.
const LET_BOUND: &str = "
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
fn a_let_bound_placeholder_is_widened_at_the_use() {
    deploys("widened", LET_BOUND).expect("clarity-wasm builds a module that loads");
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
