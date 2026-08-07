//! Who `tx-sender` is inside `as-contract`, in both engines.
//!
//! Mainnet block 8,666,423 diverges because a wrapped-STX transfer answers
//! `(err u2)` under the compiler and succeeds under the interpreter. From
//! `stx-transfer?`, `u2` means the sender and the recipient are the same
//! principal — so one of the two was computed differently, and both come from
//! `tx-sender` around an `as-contract`.
//!
//! `as-contract` has already been wrong once here, in how it typed its body.
//! This asks the other questions: who it says you are, which contract a trait
//! names, and which one comes back out of a list — the three ways a routing
//! contract works out where to send tokens.
//!
//! All ten cases agree, so the wrong principal is computed some other way. What
//! the file is for is that the next hypothesis has somewhere to go, and the ones
//! already ruled out stay ruled out.

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
(define-public (as-me) (ok (as-contract tx-sender)))
(define-public (as-me-via (t <named>)) (ok (as-contract (contract-of t))))
";

const OUTER: &str = "
(use-trait named .inner.named)
(define-read-only (direct) (as-contract tx-sender))
(define-public (through) (as-contract (contract-call? .inner who)))
(define-read-only (nested) (as-contract (as-contract tx-sender)))
(define-read-only (after) (as-contract (let ((a tx-sender)) a)))
(define-read-only (named-target (t <named>)) (contract-of t))
(define-read-only (named-in-contract (t <named>)) (as-contract (contract-of t)))
(define-private (name-it (t <named>)) (contract-of t))
(define-read-only (from-list (ts (list 4 <named>)) (at uint))
  (name-it (unwrap-panic (element-at? ts at))))

;; The mainnet shape: a trait reference standing beside a large buff, which is
;; what moves everything after it in memory. `v0-5-market::supply-collateral-add`
;; takes exactly this -- a token trait, two uints and a two-kilobyte price-feed
;; payload -- and nano failed the block it was in with `Unexpected principal
;; data`, which is a version byte of 32 or more and so a principal read at the
;; wrong offset. See `fixtures/mainnet/divergence`.
(define-read-only (named-beside-a-buff (t <named>) (a uint) (b uint) (payload (buff 2048)))
  (contract-of t))

(define-public (inner-as-me) (contract-call? .inner as-me))
(define-public (inner-as-me-entered) (as-contract (contract-call? .inner as-me)))
(define-public (inner-who-entered) (as-contract (contract-call? .inner who)))
";

fn inner() -> QualifiedContractIdentifier {
    QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.inner")
        .expect("a contract identifier")
}

fn other() -> QualifiedContractIdentifier {
    QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.other")
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
    for (contract, source) in [(inner(), INNER), (other(), INNER), (outer(), OUTER)] {
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
    for (contract, source) in [(inner(), INNER), (other(), INNER), (outer(), OUTER)] {
        nano_oracle::deploy_contract(
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
    let interpreted = describe(nano_oracle::execute_contract_call_outcome(
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
    // A contract that enters `as-contract` after being called by *another*
    // contract must become itself, not its caller. That is how a routing
    // contract names where to send tokens, and naming the caller instead sends
    // a transfer from a contract to itself.
    for function in [
        "direct",
        "through",
        "nested",
        "after",
        "inner-as-me",
        "inner-as-me-entered",
        "inner-who-entered",
    ] {
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

/// A list of trait references, indexed — how a routing contract picks a pool.
///
/// `SP2H674PRTZV6YW56K0FMR7GDGZE4ZC5HMYZ3CDEV.hilt` reaches every pool this way,
/// and the shape has already produced two clarity-wasm bugs on its own. If the
/// wrong element comes back, the pool a transfer names is wrong — which is what
/// mainnet block 8,666,423 does under the compiler.
#[test]
fn both_engines_agree_on_a_trait_taken_from_a_list() {
    let mut list = Vec::new();
    Value::cons_list_unsanitized(vec![
        Value::Principal(inner().into()),
        Value::Principal(other().into()),
        Value::Principal(inner().into()),
        Value::Principal(other().into()),
    ])
    .expect("a list")
    .consensus_serialize(&mut list)
    .expect("serialize");

    for at in 0..4u128 {
        let mut index = Vec::new();
        Value::UInt(at)
            .consensus_serialize(&mut index)
            .expect("serialize");
        let (compiled, interpreted) = answers_with("from-list", &[list.clone(), index]);
        assert_eq!(
            compiled, interpreted,
            "the engines agree on element {at} of a trait list"
        );
    }
}

/// A trait reference read correctly when a large buff shares the call.
///
/// The minimized shape of mainnet 8,708,126. That block calls
/// `v0-5-market::supply-collateral-add` with a token trait, two uints and a
/// two-kilobyte buff, and nano fails the *block* with `Unexpected principal data` --
/// which `StandardPrincipalData::new` raises for one reason, a version byte of 32 or
/// more, so something read a principal at the wrong offset.
///
/// A buff is what moves everything after it in memory, which is why it is the
/// argument under suspicion rather than the trait. The buff grows across the sizes
/// either side of the real one, because an offset wrong by a length is a bug that
/// appears at a size rather than at a shape.
#[test]
fn a_trait_beside_a_large_buff_still_names_its_contract() {
    for size in [0usize, 1, 64, 1024, 2000, 2048] {
        let payload = Value::buff_from(vec![0xab; size]).expect("a buff");
        let arguments = vec![
            trait_argument(),
            Value::UInt(46_413).serialize_to_vec().expect("a uint"),
            Value::UInt(45_924).serialize_to_vec().expect("a uint"),
            payload.serialize_to_vec().expect("the payload"),
        ];
        let (compiled, interpreted) = answers_with("named-beside-a-buff", &arguments);
        assert_eq!(
            compiled, interpreted,
            "a {size}-byte buff beside a trait reference changed which contract it names"
        );
    }
}
