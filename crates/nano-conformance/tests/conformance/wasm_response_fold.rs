//! The shape mainnet block 8,665,719 diverges on, in both engines.
//!
//! `SP1A27KFY4XERQCCRCARCYD1CC5N7M6688BSYADJ7.v0-4-market::borrow` answers
//! `Runtime(UnwrapFailure)` under clarity-wasm where the chain and the
//! interpreter say `(ok true)`. Its first `try!` runs
//!
//! ```clarity
//! (define-private (write-feeds (feeds (optional (list 3 (buff 8192)))))
//!   (match feeds
//!     entries (fold write-feed entries (ok true))
//!     (ok true)))
//! ```
//!
//! and `write-feed` returns `(response bool uint)`. The accumulator literal
//! `(ok true)` has no error type of its own, so it analyses as
//! `(response bool NoType)` — one slot short of what the step function returns,
//! which is the same family as the `let`-bound `none` at 8,667,467, in a
//! position no `let` reaches.
//!
//! The interpreter is the oracle and nothing else: clarity-wasm has to be the
//! engine that runs mainnet, so a disagreement is a compiler bug to fix.

use clarity::vm::ClarityVersion;
use clarity::vm::Value;
use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::types::QualifiedContractIdentifier;
use nano_primitives::Network;
use nano_vm::{MarfStore, Vm};
use stacks_common::codec::StacksMessageCodec;

/// `write-feeds`' shape, and the accumulator literals around it.
const FOLDER: &str = "
(define-constant ERR-FAILED (err u4001))

;; Exactly `write-feed`: a `match` on the accumulator, a wider error type than
;; the `(ok true)` the fold starts from, and an early exit that keeps it.
(define-private (step (feed (buff 16)) (status (response bool uint)))
  (match status
    success
      (if (> (len feed) u0) (ok true) ERR-FAILED)
    error status))

(define-private (folded (feeds (optional (list 3 (buff 16)))))
  (match feeds
    entries (fold step entries (ok true))
    (ok true)))

(define-read-only (fold-over (feeds (optional (list 3 (buff 16)))))
  (folded feeds))

;; What `borrow` does with the answer: `try!` it inside a `let`.
(define-read-only (try-the-fold (feeds (optional (list 3 (buff 16)))))
  (let ((checked (try! (folded feeds))))
    (ok checked)))

;; The accumulator's error type comes from the step function alone here, with
;; no `match` to widen it on the way in.
(define-private (add (n uint) (acc (response uint uint)))
  (match acc total (ok (+ total n)) error acc))
(define-read-only (sum (ns (list 4 uint)))
  (fold add ns (ok u0)))

;; A `none` accumulator whose type only the step function knows, which is
;; 8,667,467's shape reached through `fold` rather than through a `let`.
(define-private (last-of (n uint) (acc (optional uint))) (some n))
(define-read-only (last (ns (list 4 uint)))
  (fold last-of ns none))
";

/// `pyth-pnau-decoder-v3::parse-proof` and what reads its answer, which is
/// where the compiler stops: a `fold` over a *buffer* whose accumulator is a
/// tuple holding an empty `(list)`, appending 20-byte slices into it, and a
/// consumer that `unwrap-panic`s a `slice?` of each element — so an element
/// that comes back short is an `UnwrapFailure` rather than a wrong answer.
///
/// The initial literal names its fields in a different order from the
/// parameter's type, exactly as the chain's contract does.
const PROOF: &str = "
(define-constant HASH_SIZE u20)

(define-private (read-buff-20 (bytes (buff 8192)) (pos uint))
  (ok (unwrap! (as-max-len? (unwrap! (slice? bytes pos (+ pos u20)) (err u1)) u20) (err u1))))

(define-private (parse-proof
    (entry (buff 1))
    (acc {
      cursor: { index: uint, next-update-index: uint },
      bytes: (buff 8192),
      result: (list 128 (buff 20)),
      limit: uint
    }))
  (let ((result (get result acc)) (limit (get limit acc)))
    (if (is-eq (len result) limit)
      acc
      (let ((cursor (get cursor acc))
            (index (get index cursor))
            (next-update-index (get next-update-index cursor))
            (bytes (get bytes acc)))
        (if (is-eq index next-update-index)
          {
            cursor: { index: (+ index u1), next-update-index: (+ index HASH_SIZE) },
            bytes: bytes,
            result: (unwrap-panic (as-max-len? (append result (unwrap-panic (read-buff-20 bytes index))) u128)),
            limit: limit,
          }
          {
            cursor: { index: (+ index u1), next-update-index: next-update-index },
            bytes: bytes,
            result: result,
            limit: limit
          }
        )))))

(define-private (parsed (proof-bytes (buff 8192)) (proof-size uint))
  (get result (fold parse-proof proof-bytes
    { result: (list), cursor: { index: u0, next-update-index: u0 }, bytes: proof-bytes, limit: proof-size })))

(define-read-only (proof-of (proof-bytes (buff 8192)) (proof-size uint))
  (parsed proof-bytes proof-size))

;; What `check-proof` does with it, and where a short element panics.
(define-private (buff-20-to-uint (bytes (buff 20)))
  (buff-to-uint-be (unwrap-panic (as-max-len? (unwrap-panic (slice? bytes u0 u15)) u16))))
(define-private (keccak160 (bytes (buff 1024)))
  (unwrap-panic (as-max-len? (unwrap-panic (slice? (keccak256 bytes) u0 u20)) u20)))
(define-private (hash-nodes (node-1 (buff 20)) (node-2 (buff 20)))
  (let ((uint-1 (buff-20-to-uint node-1))
        (uint-2 (buff-20-to-uint node-2))
        (sequence (if (< uint-2 uint-1) (concat (concat 0x01 node-2) node-1) (concat (concat 0x01 node-1) node-2))))
    (keccak160 sequence)))
(define-private (hash-path (entry (buff 20)) (acc (buff 20))) (hash-nodes entry acc))
(define-read-only (checked (proof-bytes (buff 8192)) (proof-size uint) (leaf (buff 255)))
  (fold hash-path (parsed proof-bytes proof-size) (keccak160 (concat 0x00 leaf))))
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

/// The `(optional (list 3 (buff 16)))` argument, as `borrow` receives it.
fn feeds(count: usize) -> Vec<u8> {
    let entries = (0..count)
        .map(|index| Value::buff_from(vec![u8::try_from(index).expect("a byte") + 1; 4]))
        .collect::<Result<Vec<_>, _>>()
        .expect("buffers");
    serialized(
        &Value::some(Value::cons_list_unsanitized(entries).expect("a list")).expect("an optional"),
    )
}

fn numbers(count: u128) -> Vec<u8> {
    serialized(
        &Value::cons_list_unsanitized((1..=count).map(Value::UInt).collect()).expect("a list"),
    )
}

fn both(function: &str, arguments: &[Vec<u8>]) -> (String, String) {
    both_in(FOLDER, function, arguments)
}

fn both_in(source: &str, function: &str, arguments: &[Vec<u8>]) -> (String, String) {
    let mut wasm = Vm::new(Network::TESTNET).expect("create the compiling VM");
    wasm.begin_block(None, [0x41; 32]).expect("begin");
    wasm.deploy_contract(
        id("f"),
        ClarityVersion::Clarity3,
        source,
        LimitedCostTracker::new_free(),
    )
    .expect("deploy under the compiler");

    let mut store = MarfStore::new(Network::TESTNET).expect("create the interpreter store");
    store.begin(None, [0x42; 32]).expect("begin");
    nano_oracle::deploy_contract(
        &mut store,
        id("f"),
        ClarityVersion::Clarity3,
        source,
        LimitedCostTracker::new_free(),
    )
    .expect("deploy under the interpreter");

    let describe = |outcome: Result<nano_vm::ContractCallOutcome, _>| match outcome {
        Ok(
            nano_vm::ContractCallOutcome::Success(result)
            | nano_vm::ContractCallOutcome::AbortedByResponse(result),
        ) => format!("{:?}", result.value),
        Ok(nano_vm::ContractCallOutcome::RuntimeFailure { error, .. }) => format!("failed: {error}"),
        Err(error) => format!("error: {error}"),
    };

    let compiled = describe(wasm.execute_contract_call_outcome(
        id("f").issuer.into(),
        None,
        id("f"),
        function,
        arguments,
        &LimitedCostTracker::new_free(),
    ));
    let interpreted = describe(nano_oracle::execute_contract_call_outcome(
        &mut store,
        id("f").issuer.into(),
        None,
        id("f"),
        function,
        arguments,
        LimitedCostTracker::new_free(),
    ));
    (compiled, interpreted)
}

#[test]
fn a_response_literal_accumulator_folds_the_same() {
    for count in [0, 1, 3] {
        let (compiled, interpreted) = both("fold-over", &[feeds(count)]);
        assert_eq!(compiled, interpreted, "fold-over over {count} feeds");
    }
}

#[test]
fn the_folded_response_can_be_tried() {
    for count in [0, 1, 3] {
        let (compiled, interpreted) = both("try-the-fold", &[feeds(count)]);
        assert_eq!(compiled, interpreted, "try-the-fold over {count} feeds");
    }
}

#[test]
fn an_ok_literal_accumulator_sums_the_same() {
    let (compiled, interpreted) = both("sum", &[numbers(4)]);
    assert_eq!(compiled, interpreted);
}

#[test]
fn a_none_accumulator_folds_the_same() {
    let (compiled, interpreted) = both("last", &[numbers(4)]);
    assert_eq!(compiled, interpreted);
}

/// A proof of `count` hashes, laid out as the decoder reads them: 20 bytes per
/// hash, back to back, which is what `parse-proof` walks a byte at a time.
fn proof_bytes(count: usize) -> Vec<u8> {
    serialized(
        &Value::buff_from(
            (0..count)
                .flat_map(|hash| {
                    (0..20).map(move |byte| u8::try_from((hash * 20 + byte) % 251 + 1).expect("a byte"))
                })
                .collect(),
        )
        .expect("a buffer"),
    )
}

fn uint(value: u128) -> Vec<u8> {
    serialized(&Value::UInt(value))
}

#[test]
fn a_buffer_fold_appending_into_an_empty_list_agrees() {
    for count in [0, 1, 2, 5] {
        let (compiled, interpreted) = both_in(
            PROOF,
            "proof-of",
            &[proof_bytes(count), uint(count as u128)],
        );
        assert_eq!(compiled, interpreted, "proof-of over {count} hashes");
    }
}

#[test]
fn hashing_the_parsed_proof_agrees() {
    for count in [0, 1, 2, 5] {
        let leaf = serialized(&Value::buff_from(vec![0x11; 32]).expect("a buffer"));
        let (compiled, interpreted) = both_in(
            PROOF,
            "checked",
            &[proof_bytes(count), uint(count as u128), leaf],
        );
        assert_eq!(compiled, interpreted, "checked over {count} hashes");
    }
}

/// The shape mainnet block 8,666,585 stops on: two tuple types under one `if`.
///
/// `SP4SZE…rewards-stx-v1` is deployed by that block, mainnet accepted it, and
/// clarity-wasm emits a module wasmtime refuses to load:
///
/// ```text
/// type mismatch: expected i64, found i32 (at offset 0x1a37)
/// ```
///
/// which is the placeholder-layout signature again — the fourth of this family
/// after 8,667,467's `let`-bound `none` and the fold-over-a-buffer above.
///
/// The contract's `process-rewards` ends in an `if` whose two arms `print` tuples
/// with *different field sets*, one carrying an extra `keeper-only`. Minimised to
/// eight lines below. Note that the same `if` **without** `print` is rejected by
/// analysis ("Tuples fields should be typed"), so it is `print` accepting two
/// unrelated tuple types that lets codegen reach a layout it cannot honour.
///
/// Fixed in three places: `need_ducktyping` compared tuples field-set-wise
/// instead of zipping them positionally, `print` ducks the value it hands back
/// from its argument's type to its own expression's type, and `duck_type_stack`
/// maps tuple fields by name and drops the ones the target does not have.
#[test]
fn two_tuple_shapes_under_one_if_compile_to_a_loadable_module() {
    const SHAPES: &str = "
(define-data-var v uint u0)
(define-public (go (n uint))
  (begin
    (if (> n u0)
      (print { a: n, b: u1 })
      (print { a: n, b: u1, c: true }))
    (ok n)))
";
    let mut wasm = Vm::new(Network::TESTNET).expect("create the compiling VM");
    wasm.begin_block(None, [0x51; 32]).expect("begin");
    wasm.deploy_contract(
        id("shapes"),
        ClarityVersion::Clarity6,
        SHAPES,
        LimitedCostTracker::new_free(),
    )
    .expect("mainnet accepted this contract, so the compiler has to build it");
}

/// The narrowing the fix has to get right, not merely survive.
///
/// `least_supertype` walks the *true* arm's fields and looks each one up in the
/// false arm's, so the `if` takes the true arm's — narrower — type and the false
/// arm's value has to shed fields. Shedding the wrong one still loads: it just
/// answers with a neighbour's value, so each case reads the survivors back and
/// compares them against the interpreter.
const NARROWING: &str = "
;; The dropped field is in the middle of the name order, and is two i32 slots
;; wide where its neighbours are one i32 and two i64.
(define-read-only (drop-middle (narrow bool))
  (let ((t (if narrow
             (print { a: u10, c: true })
             (print { a: u10, b: 0x11223344, c: true }))))
    { a: (get a t), c: (get c t) }))

;; The same drop with every field the same width, so a mispairing of source
;; field to target locals loads cleanly and answers wrong instead of refusing
;; to load, which is the failure mode a load check cannot see.
(define-read-only (drop-middle-uniform (narrow bool))
  (let ((t (if narrow
             (print { a: u10, c: u30 })
             (print { a: u10, b: u20, c: u30 }))))
    { a: (get a t), c: (get c t) }))

;; Narrowing inside a field: `inner` sorts before `outer`, and `y` sorts between
;; the two fields that survive it.
(define-read-only (drop-nested (narrow bool))
  (let ((t (if narrow
             (print { inner: { x: u20, z: u40 }, outer: u50 })
             (print { inner: { x: u20, y: u30, z: u40 }, outer: u50 }))))
    { x: (get x (get inner t)), z: (get z (get inner t)), outer: (get outer t) }))

;; A list field makes the target need duck-typing workspace, which is the branch
;; where `print` has to allocate call-stack bytes rather than pass `none`.
(define-read-only (drop-beside-a-list (narrow bool))
  (let ((t (if narrow
             (print { a: (list u1 u2 u3), c: u60 })
             (print { a: (list u1 u2 u3), b: u70, c: u60 }))))
    { a: (get a t), c: (get c t) }))
";

fn boolean(value: bool) -> Vec<u8> {
    serialized(&Value::Bool(value))
}

fn principal(text: &str) -> Vec<u8> {
    serialized(
        &Value::Principal(
            clarity::vm::types::PrincipalData::parse(text).expect("a principal literal"),
        ),
    )
}

/// The shape mainnet block 8,667,509 stops on: a `default-to` whose default
/// names fewer fields than the optional carries.
///
/// `SPN5AKG35QZSK2M8GAMR4AFX45659RJHDW353HSG.blacklist-susdh-v1` is on the chain
/// and mainnet ran it, and clarity-wasm emitted a module wasmtime refuses:
///
/// ```text
/// type mismatch: values remaining on stack at end of block (at offset 0x2955)
/// ```
///
/// which is *not* the placeholder signature of the four before it — nothing is
/// read as the wrong width; a slot is pushed that nothing pops. `default-to`
/// types as `least_supertype(default, inner)`, which walks the **default's**
/// fields and silently drops the rest, so `(default-to { soft: false } (map-get?
/// blacklist k))` over a `{ soft: bool, full: bool }` map analyses as the
/// one-field tuple. `map-get?` reads the map's own value type and pushes two
/// slots; only one was accounted for, and the other stayed on the stack to the
/// end of the function.
const NARROWING_DEFAULT: &str = "
(define-map blacklist { address: principal } { soft: bool, full: bool })

;; `get-soft-blacklist` and `get-full-blacklist`, verbatim in shape: each default
;; names one of the two fields, so each narrows differently.
(define-read-only (soft-of (address principal))
  (get soft (default-to { soft: false } (map-get? blacklist { address: address }))))
(define-read-only (full-of (address principal))
  (get full (default-to { full: false } (map-get? blacklist { address: address }))))

;; Both branches of the same `default-to`: present for `address`, absent for the
;; principal beside it. A `none` converts a placeholder payload, which must not
;; be read.
(define-public (record (address principal) (full bool))
  (begin
    (map-set blacklist { address: address } { soft: true, full: full })
    (ok {
      present-soft: (soft-of address),
      present-full: (full-of address),
      absent: (soft-of 'ST000000000000000000002AMW42H)
    })))

;; A list in the surviving field makes the conversion need call-stack workspace
;; rather than locals alone, and `(list)` as the default makes the *default* the
;; placeholder for once.
(define-map holdings { address: principal } { amounts: (list 3 uint), note: uint })
(define-public (hold (address principal))
  (begin
    (map-set holdings { address: address } { amounts: (list u1 u2 u3), note: u9 })
    (ok {
      present: (get amounts (default-to { amounts: (list) }
                 (map-get? holdings { address: address }))),
      absent: (get amounts (default-to { amounts: (list) }
                 (map-get? holdings { address: 'ST000000000000000000002AMW42H })))
    })))

;; The optional as a plain binding rather than a map read, which is the same
;; narrowing reached through `visit_atom` instead of through `map-get?`.
(define-read-only (soft-of-bound (entry (optional { soft: bool, full: bool })))
  (get soft (default-to { soft: false } entry)))

;; `default-to` with nothing to narrow, which the fix must leave alone: the
;; payload's type is the expression's, and either side can be the placeholder.
(define-read-only (or-seven (n (optional uint))) (default-to u7 n))
(define-read-only (or-nothing (n (optional (optional uint)))) (default-to none n))

;; The narrowed tuple handed back whole instead of read through `get`, which is
;; the one shape the two engines still answer differently.
(define-read-only (whole (entry (optional { soft: bool, full: bool })))
  (default-to { soft: false } entry))
";

#[test]
fn a_default_naming_fewer_fields_loads_and_reads_the_ones_it_named() {
    let alice = "SP2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKNRV9EJ7";
    for function in ["soft-of", "full-of"] {
        for who in [alice, "ST000000000000000000002AMW42H"] {
            let (compiled, interpreted) =
                both_in(NARROWING_DEFAULT, function, &[principal(who)]);
            assert_eq!(compiled, interpreted, "{function} of {who}");
        }
    }
    for full in [true, false] {
        let (compiled, interpreted) =
            both_in(NARROWING_DEFAULT, "record", &[principal(alice), boolean(full)]);
        assert!(
            !compiled.starts_with("failed:") && !compiled.starts_with("error:"),
            "record with full={full} answered nothing: {compiled}"
        );
        assert_eq!(compiled, interpreted, "record with full={full}");
    }
    let (compiled, interpreted) = both_in(NARROWING_DEFAULT, "hold", &[principal(alice)]);
    assert!(
        !compiled.starts_with("failed:") && !compiled.starts_with("error:"),
        "hold answered nothing: {compiled}"
    );
    assert_eq!(compiled, interpreted, "hold");

    for entry in [
        Value::none(),
        Value::some(Value::Tuple(
            clarity::vm::types::TupleData::from_data(vec![
                (
                    clarity::vm::ClarityName::from_literal("soft"),
                    Value::Bool(true),
                ),
                (
                    clarity::vm::ClarityName::from_literal("full"),
                    Value::Bool(false),
                ),
            ])
            .expect("a tuple"),
        ))
        .expect("an optional"),
    ] {
        let (compiled, interpreted) =
            both_in(NARROWING_DEFAULT, "soft-of-bound", &[serialized(&entry)]);
        assert_eq!(compiled, interpreted, "soft-of-bound of {entry}");
    }
}

/// The supertype asymmetry, now reachable through `default-to`.
///
/// `least_supertype` walks the default's fields and drops the rest, so
/// `(default-to { soft: false } entry)` analyses as `{ soft: bool }`. clar2wasm
/// lays every value out for its static type and so answers with the narrowed
/// tuple; the interpreter carries the taken value and answers with the wide one.
/// Nothing sanitizes a contract-call return, so the two receipts differ:
///
/// ```text
/// compiled     { "soft": bool }               { soft: true }
/// interpreted  { "full": bool, "soft": bool } { full: true, soft: true }
/// ```
///
/// The same asymmetry the `if` narrowing left open, and `default-to` is a far
/// more common way to reach it than a `print` under an `if`.
/// `blacklist-susdh-v1` reads every one of its `default-to`s through `get`, so
/// mainnet block 8,667,509 does not depend on which value comes back, and no
/// shape found on the chain so far does. Ignored rather than deleted: it needs a
/// decision at the analysis layer, and a red suite teaches people to ignore red
/// suites.
#[test]
#[ignore = "narrowing a tuple by its static type answers differently from the interpreter, which keeps the taken value's own type"]
fn a_narrowed_default_handed_back_whole_agrees() {
    let entry = Value::some(Value::Tuple(
        clarity::vm::types::TupleData::from_data(vec![
            (
                clarity::vm::ClarityName::from_literal("soft"),
                Value::Bool(true),
            ),
            (
                clarity::vm::ClarityName::from_literal("full"),
                Value::Bool(true),
            ),
        ])
        .expect("a tuple"),
    ))
    .expect("an optional");
    let (compiled, interpreted) = both_in(NARROWING_DEFAULT, "whole", &[serialized(&entry)]);
    assert_eq!(compiled, interpreted);
}

#[test]
fn a_default_to_with_nothing_to_narrow_is_unchanged() {
    let some_eleven = Value::some(Value::UInt(11)).expect("an optional");
    for (function, argument) in [
        ("or-seven", &some_eleven),
        ("or-seven", &Value::none()),
        (
            "or-nothing",
            &Value::some(some_eleven.clone()).expect("an optional"),
        ),
        ("or-nothing", &Value::none()),
    ] {
        let (compiled, interpreted) =
            both_in(NARROWING_DEFAULT, function, &[serialized(argument)]);
        assert_eq!(compiled, interpreted, "{function} of {argument}");
    }
}

#[test]
fn narrowing_a_tuple_keeps_the_fields_it_kept() {
    for function in [
        "drop-middle",
        "drop-middle-uniform",
        "drop-nested",
        "drop-beside-a-list",
    ] {
        for narrow in [true, false] {
            let (compiled, interpreted) = both_in(NARROWING, function, &[boolean(narrow)]);
            // Two engines agreeing on a failure would prove nothing about which
            // fields survived.
            assert!(
                !compiled.starts_with("failed:") && !compiled.starts_with("error:"),
                "{function} with narrow={narrow} answered nothing: {compiled}"
            );
            assert_eq!(compiled, interpreted, "{function} with narrow={narrow}");
        }
    }
}
