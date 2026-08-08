//! `map` over a string walks its characters, in both engines.
//!
//! Four contracts mainnet deployed and accepted would not compile:
//! `SP2BRB6P0BK6T35DHTGXCV6MZ5TGRN5E0RKZ1T8B5.gated-pages` and three siblings,
//! each refusing with `Incompatible types for duck typing: buff 1 against
//! string-ascii 256`. `gated-pages` line 390 is the shape:
//!
//! ```clarity
//! (map airdrop-single-page recipients titles descriptions metadata-uri)
//! ```
//!
//! Three of those are lists; `metadata-uri` is a bare `(string-ascii 256)`, so
//! `map` walks its characters and hands each to a parameter declared
//! `(string-ascii 256)`.
//!
//! `SequenceElementType::Byte` means "read a byte at a time", which is right for
//! a buffer and a `string-ascii` alike — and its conversion to a `TypeSignature`
//! has to pick one, so it picked `(buff 1)`. Right for a load, wrong for a
//! widening. `fold` met the same ambiguity and worked around it by reading the
//! folded function's declared parameter; `map` had no such workaround.
//!
//! The element type is not clar2wasm's to invent, so it comes from clarity's own
//! `SequenceSubtype::unit_type` now.
//!
//! **The assertion is the answer, not that it compiles.** A wrong element type
//! that happens to lay out compatibly does not refuse — it computes, and nothing
//! downstream says so. That is what task 086 was. So every case here is
//! crosschecked against the reference interpreter.

use clarity::vm::ClarityVersion;
use clarity::vm::Value;
use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier};
use nano_primitives::Network;
use nano_vm::{MarfStore, Vm};
use stacks_common::codec::StacksMessageCodec;

const SOURCE: &str = "
;; The mainnet shape: a parameter wider than the character `map` feeds it.
(define-private (widen (c (string-ascii 256))) c)
(define-read-only (over-ascii (s (string-ascii 8))) (map widen s))

;; The same for a buffer, which is what the element type used to claim
;; everything was -- so a fix that made strings work by breaking buffers shows.
(define-private (widen-buff (b (buff 32))) b)
(define-read-only (over-buff (b (buff 8))) (map widen-buff b))

;; And UTF-8, whose element is a different width again.
(define-private (widen-utf8 (c (string-utf8 64))) c)
(define-read-only (over-utf8 (s (string-utf8 8))) (map widen-utf8 s))

;; A list beside them, which never had the ambiguity and must not gain one.
(define-private (widen-uint (n uint) ) (+ n u1))
(define-read-only (over-list (l (list 8 uint))) (map widen-uint l))

;; Two sequences at once, one a list and one a string -- `gated-pages` passes
;; four, three lists and a string.
(define-private (pair (n uint) (c (string-ascii 256))) { n: n, c: c })
(define-read-only (over-both (l (list 4 uint)) (s (string-ascii 4))) (map pair l s))
";

fn contract() -> QualifiedContractIdentifier {
    QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.mapping")
        .expect("a contract identifier")
}

fn encode(value: &Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    value.consensus_serialize(&mut bytes).expect("serialize");
    bytes
}

fn describe(outcome: Result<nano_vm::ContractCallOutcome, impl std::fmt::Debug>) -> String {
    match outcome {
        Ok(
            nano_vm::ContractCallOutcome::Success(result)
            | nano_vm::ContractCallOutcome::AbortedByResponse(result),
        ) => format!("{:?}", result.value),
        Ok(nano_vm::ContractCallOutcome::RuntimeFailure { error, .. }) => {
            format!("failed: {error:?}")
        }
        Err(error) => format!("{error:?}"),
    }
}

fn answers(function: &str, arguments: &[Vec<u8>]) -> (String, String) {
    let mut wasm = Vm::new(Network::TESTNET).expect("create the compiling VM");
    wasm.begin_block(None, [0x81; 32]).expect("begin");
    wasm.deploy_contract(
        contract(),
        ClarityVersion::Clarity4,
        SOURCE,
        LimitedCostTracker::new_free(),
    )
    .expect("deploy");

    let mut store = MarfStore::new(Network::TESTNET).expect("create the interpreter store");
    store.begin(None, [0x82; 32]).expect("begin");
    nano_oracle::deploy_contract(
        &mut store,
        contract(),
        ClarityVersion::Clarity4,
        SOURCE,
        LimitedCostTracker::new_free(),
    )
    .expect("deploy");

    let sender: PrincipalData = contract().issuer.into();
    let compiled = describe(wasm.execute_contract_call_outcome(
        sender.clone(),
        None,
        contract(),
        function,
        arguments,
        &LimitedCostTracker::new_free(),
    ));
    let interpreted = describe(nano_oracle::execute_contract_call_outcome(
        &mut store,
        sender,
        None,
        contract(),
        function,
        arguments,
        LimitedCostTracker::new_free(),
    ));
    (compiled, interpreted)
}

#[test]
fn both_engines_map_over_a_string_ascii() {
    let text = Value::string_ascii_from_bytes(b"abcd".to_vec()).expect("ascii");
    let (compiled, interpreted) = answers("over-ascii", &[encode(&text)]);
    assert_eq!(compiled, interpreted, "the engines disagree about `map` over a string");
    // Four characters back, each its own one-character string. A wrong element
    // type that laid out compatibly would still return *something*.
    assert!(
        compiled.matches('a').count() >= 1 && compiled.contains('d'),
        "`map` over \"abcd\" lost its characters: {compiled}"
    );
}

#[test]
fn both_engines_map_over_a_buffer_and_a_utf8_string() {
    let buffer = Value::buff_from(vec![1, 2, 3]).expect("a buff");
    let (compiled, interpreted) = answers("over-buff", &[encode(&buffer)]);
    assert_eq!(compiled, interpreted, "the engines disagree about `map` over a buffer");

    let text = Value::string_utf8_from_bytes(b"ab".to_vec()).expect("utf8");
    let (compiled, interpreted) = answers("over-utf8", &[encode(&text)]);
    assert_eq!(compiled, interpreted, "the engines disagree about `map` over a utf8 string");
}

#[test]
fn both_engines_map_over_a_list_and_over_a_list_beside_a_string() {
    let list = Value::cons_list_unsanitized(vec![Value::UInt(1), Value::UInt(2)])
        .expect("a list");
    let (compiled, interpreted) = answers("over-list", &[encode(&list)]);
    assert_eq!(compiled, interpreted, "the engines disagree about `map` over a list");

    // The mainnet shape: a list and a string in one `map`.
    let text = Value::string_ascii_from_bytes(b"xy".to_vec()).expect("ascii");
    let (compiled, interpreted) = answers("over-both", &[encode(&list), encode(&text)]);
    assert_eq!(
        compiled, interpreted,
        "the engines disagree about `map` over a list beside a string"
    );
    assert!(
        compiled.contains('x'),
        "`map` over a list and a string lost the string's characters: {compiled}"
    );
}
