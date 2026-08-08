//! A principal a `let` binds and an allowance reads — mainnet block 8,708,126.
//!
//! `SP1A27KFY4XERQCCRCARCYD1CC5N7M6688BSYADJ7.v0-5-market::supply-collateral-add`
//! failed the whole block, not one receipt, with
//!
//! ```text
//! Internal(InvariantViolation("Expect(\"Internal(Expect(\\\"Unexpected principal data\\\"))\")"))
//! ```
//!
//! which `StandardPrincipalData::new` raises for exactly one reason: a version
//! byte of 32 or more. Something read a principal at the wrong offset.
//!
//! Two reductions guessed at the argument *shape* — a trait beside a two-kilobyte
//! buff, and the same trait dispatched rather than named — and both passed. The
//! deployed source says why: the payload is not a `(buff 2048)` at all but an
//! `(optional (list 3 (buff 8192)))`, and the offset never came from the arguments.
//! It came from the body:
//!
//! ```clarity
//! (let ((ft-address (contract-of ft)) (asset (try! (get-asset ft-address))) …)
//!   …
//!   (as-contract? ((with-ft ft-address "*" amount))
//!     (try! (vault-deposit asset-id amount min-shares account))))
//! ```
//!
//! `ft-address` is a `let`-bound principal read three times, and the third read is
//! inside an allowance list. The use-count pre-pass behind wasm-local reuse did not
//! look inside a list whose head is not a word, so it counted two: the second read
//! took the count to zero, `note_binding_read` handed the binding's locals back to
//! the pool, and entering the allowance borrowed them straight back. The allowance
//! then read its operands from whatever had landed in those slots.
//!
//! `be3ec64e` is the fix. Its cause is pinned in `clar2wasm`'s
//! `binding_uses_counts_a_principal_read_from_an_allowance`, which asserts the
//! count itself: three reads, not two. That test fails on the pre-fix revision
//! with `[2, 2]` against `[3, 2]`.
//!
//! This file pins the effect, and the bisect was run: with `be3ec64e`'s walk
//! reverted and nothing else changed, `an_allowance_reads_the_principal_its_let_bound`
//! fails because the compiler raises
//!
//! ```text
//! Internal(InvariantViolation("Runtime(invalid utf-8 sequence of 1 bytes from index 0)"))
//! ```
//!
//! where the interpreter returns the tuple. Reduced, the reused slot lands on the
//! allowance's asset-name string rather than on its principal, so the reduction
//! names a different field of the same wrong read than mainnet did — the read, the
//! cause and the fix are the one thing, and neither error can be raised by a
//! module that reads its operands from the slots they were put in. Restoring the
//! walk makes both tests pass.
//!
//! See `fixtures/mainnet/divergence/README.md` and task 086.

use clarity::vm::ClarityVersion;
use clarity::vm::Value;
use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::types::QualifiedContractIdentifier;
use nano_primitives::Network;
use nano_vm::{MarfStore, Vm};
use stacks_common::codec::StacksMessageCodec;

/// The token the market is handed, with `v0-5-market`'s own trait shape.
const TOKEN: &str = "
(define-trait ft-trait (
  (transfer (uint principal principal (optional (buff 34))) (response bool uint))
))
(define-fungible-token unit)
(define-public (transfer (amount uint) (from principal) (to principal) (memo (optional (buff 34))))
  (ok true))
";

/// `supply-collateral-add` reduced to the binding and the allowance that read it.
///
/// Kept faithful where it matters and cut everywhere else: `ft-address` is bound
/// from `(contract-of ft)`, read once in a later binding's value, once in the
/// `is-eq` that chooses the branch, and once inside `((with-ft ft-address "*"
/// amount))` — the same three reads, in the same order, as the deployed source.
/// The price-feed payload keeps its real type, `(optional (list 3 (buff 8192)))`,
/// so the argument lowering the earlier reductions suspected is still exercised.
///
/// `the-allowance-reads-it-first` returns the principal itself: that is the value
/// that was wrong on mainnet, and a test that only asked whether an error was
/// raised would pass on a module computing with a principal nobody put there.
const MARKET: &str = "
(use-trait ft-trait .token.ft-trait)
(define-constant WRAPPER 'ST000000000000000000002AMW42H.wrapper)

(define-read-only (get-asset (address principal))
  (if (is-eq address WRAPPER) (err u404) (ok u1)))

(define-public (supply-collateral-add
    (ft <ft-trait>)
    (amount uint)
    (min-shares uint)
    (price-feeds (optional (list 3 (buff 8192)))))
  (let ((ft-address (contract-of ft))
        (asset (try! (get-asset ft-address))))
    (try! (contract-call? ft transfer amount tx-sender current-contract none))
    (let ((shares (try! (if (is-eq ft-address WRAPPER)
                          (as-contract? ((with-stx amount)) amount)
                          (as-contract? ((with-ft ft-address \"*\" amount)) amount)))))
      (ok { asset: asset, shares: shares }))))

;; The allowance read first, so the ordering is not what carries it.
(define-public (the-allowance-reads-it-first (ft <ft-trait>) (amount uint))
  (let ((ft-address (contract-of ft))
        (shares (try! (as-contract? ((with-ft ft-address \"*\" amount)) amount))))
    (ok { named: ft-address, shares: shares })))
";

fn token() -> QualifiedContractIdentifier {
    QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.token")
        .expect("a contract identifier")
}

fn market() -> QualifiedContractIdentifier {
    QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.market")
        .expect("a contract identifier")
}

fn encode(value: &Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    value.consensus_serialize(&mut bytes).expect("serialize");
    bytes
}

/// The real call's arguments: the token as a trait reference, two uints, and a
/// price-feed payload of the deployed type.
fn arguments(feeds: usize, feed_bytes: usize) -> Vec<Vec<u8>> {
    let payload = if feeds == 0 {
        Value::none()
    } else {
        Value::some(
            Value::cons_list_unsanitized(
                (0..feeds)
                    .map(|index| {
                        Value::buff_from(vec![u8::try_from(index).unwrap_or(0); feed_bytes])
                            .expect("a buff")
                    })
                    .collect(),
            )
            .expect("a list"),
        )
        .expect("an optional")
    };
    vec![
        encode(&Value::Principal(token().into())),
        encode(&Value::UInt(46_413)),
        encode(&Value::UInt(45_924)),
        encode(&payload),
    ]
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

/// What each engine answers for one call into the market.
fn answers(function: &str, arguments: &[Vec<u8>]) -> (String, String) {
    let mut wasm = Vm::new(Network::TESTNET).expect("create the compiling VM");
    wasm.begin_block(None, [0x41; 32]).expect("begin");
    for (contract, source) in [(token(), TOKEN), (market(), MARKET)] {
        wasm.deploy_contract(
            contract,
            ClarityVersion::Clarity4,
            source,
            LimitedCostTracker::new_free(),
        )
        .expect("deploy");
    }

    let mut store = MarfStore::new(Network::TESTNET).expect("create the interpreter store");
    store.begin(None, [0x42; 32]).expect("begin");
    for (contract, source) in [(token(), TOKEN), (market(), MARKET)] {
        nano_oracle::deploy_contract(
            &mut store,
            contract,
            ClarityVersion::Clarity4,
            source,
            LimitedCostTracker::new_free(),
        )
        .expect("deploy");
    }

    let compiled = describe(wasm.execute_contract_call_outcome(
        market().issuer.into(),
        None,
        market(),
        function,
        arguments,
        &LimitedCostTracker::new_free(),
    ));
    let interpreted = describe(nano_oracle::execute_contract_call_outcome(
        &mut store,
        market().issuer.into(),
        None,
        market(),
        function,
        arguments,
        LimitedCostTracker::new_free(),
    ));
    (compiled, interpreted)
}

/// The captured call, at the payload sizes around the one mainnet sent.
///
/// The engines have to agree, and the answer has to *name the token* — a
/// principal read from a reused slot is a different principal, or no principal
/// at all.
#[test]
fn an_allowance_reads_the_principal_its_let_bound() {
    for (feeds, bytes) in [(0, 0), (1, 1), (1, 2_048), (2, 2_048), (3, 8_192)] {
        let (compiled, interpreted) = answers("supply-collateral-add", &arguments(feeds, bytes));
        assert_eq!(
            compiled, interpreted,
            "the engines agree on `supply-collateral-add` with {feeds} feed(s) of {bytes} bytes"
        );
        assert!(
            !compiled.contains("principal data") && !compiled.contains("Internal"),
            "the allowance read a principal from the wrong offset, with {feeds} feed(s) of \
             {bytes} bytes: {compiled}"
        );
    }
}

/// The same binding with the allowance reading it *first*, and the principal
/// itself as the answer.
///
/// The ordering is the other half of the pair: `supply-collateral-add` reads the
/// binding twice before the allowance, and this reads it there first. Asserting
/// the principal by value is what makes this more than an error check — a slot
/// handed back and borrowed again does not have to raise anything to be wrong,
/// and where the types line up it simply answers with a different contract.
#[test]
fn an_allowance_is_a_read_wherever_it_sits() {
    let function = "the-allowance-reads-it-first";
    let arguments = [
        encode(&Value::Principal(token().into())),
        encode(&Value::UInt(7)),
    ];
    let (compiled, interpreted) = answers(function, &arguments);
    assert_eq!(compiled, interpreted, "the engines agree on `{function}`");
    assert!(
        compiled.contains(&format!("{:?}", Value::Principal(token().into()))),
        "`{function}` named a principal that is not the token: {compiled}"
    );
}
