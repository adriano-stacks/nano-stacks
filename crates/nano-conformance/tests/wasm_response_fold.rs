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
    nano_vm::deploy_contract(
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
    let interpreted = describe(nano_vm::execute_contract_call_outcome(
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
