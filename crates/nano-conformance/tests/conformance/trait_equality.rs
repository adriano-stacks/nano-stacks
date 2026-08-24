//! `is-eq` over two trait references, in both engines.
//!
//! Three contracts mainnet deployed and accepted would not compile at all:
//! `SPXWGJQ101N1C1FYHK64TGTHN4793CHVKTJAT7VQ.amm-swap003` and two `.pool`s, each
//! refusing with `Not implemented: equality over CallableType(Trait(..))`.
//! `wasm_equal`'s principal arm covered `CallableSubtype::Principal` and not
//! `CallableSubtype::Trait`, though the two are the same thing at run time —
//! `wasm_generator` says so where it lowers one, "a public function receives a
//! trait argument as a bare principal", and `contract-of` is the read of it.
//!
//! Which is why compiling is not the assertion here. A trait reference *is* a
//! principal in linear memory, so the bug had two possible endings: refuse to
//! compile, or compare the wrong bytes and answer confidently. Task 086 is what
//! the second one looks like on mainnet — a principal read at the wrong offset,
//! in a module that loaded. So this asks the reference interpreter what the
//! answer is and requires the compiler to agree, over pairs that are equal, pairs
//! that are not, and a trait against a bare principal naming the same contract.
//!
//! Found by task 073's sweep over every contract in the imported mainnet state;
//! classified and split out as task 093.

use clarity::vm::ClarityVersion;
use clarity::vm::Value;
use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier};
use nano_primitives::Network;
use nano_vm::{MarfStore, Vm};
use stacks_common::codec::StacksMessageCodec;

/// Two contracts implementing one trait, so a comparison has something to say.
const TOKEN: &str = "
(define-trait ft ((transfer (uint) (response bool uint))))
(define-public (transfer (amount uint)) (ok true))
";

const MARKET: &str = "
(use-trait ft .token-a.ft)

(define-map registrations
  { staker: principal, signer-manager: principal }
  uint)

(define-data-var registration-head (optional {
  staker: principal,
  signer-manager: principal,
}) none)

(define-data-var registration-tail (optional {
  staker: principal,
  signer-manager: principal,
}) none)

(define-map registration-links
  { staker: principal, signer-manager: principal }
  {
    prev: (optional { staker: principal, signer-manager: principal }),
    next: (optional { staker: principal, signer-manager: principal }),
  })

(define-private (append-registration (key {
    staker: principal,
    signer-manager: principal,
  }))
  (let ((old-tail (var-get registration-tail)))
    (map-set registration-links key { prev: old-tail, next: none })
    (match old-tail
      tail-key
      (match (map-get? registration-links tail-key)
        tail-links
        (map-set registration-links tail-key
          (merge tail-links { next: (some key) }))
        false)
      (var-set registration-head (some key)))
    (var-set registration-tail (some key))))

(define-map bond-memberships
  principal
  { signer: principal, bond-index: uint })

(define-map active-stakes
  principal
  { signer: principal, first-reward-cycle: uint })

(map-set active-stakes tx-sender {
  signer: .token-a,
  first-reward-cycle: u1,
})

(define-read-only (position (staker principal))
  (match (map-get? bond-memberships staker)
    membership
    (some {
      signer: (get signer membership),
      first-reward-cycle: u1,
      bond-index: (some (get bond-index membership)),
    })
    (match (map-get? active-stakes staker)
      info
      (some {
        signer: (get signer info),
        first-reward-cycle: (get first-reward-cycle info),
        bond-index: none,
      })
      none)))

;; The shape the three mainnet contracts use: two trait references compared.
(define-public (same (a <ft>) (b <ft>)) (ok (is-eq a b)))
(define-public (different (a <ft>) (b <ft>)) (ok (not (is-eq a b))))

;; A trait against the principal it names, which has to agree with `contract-of`.
(define-public (matches-contract-of (a <ft>)) (ok (is-eq (contract-of a) (contract-of a))))
(define-read-only (named (a <ft>)) (contract-of a))

;; Mainnet 8,815,026: a Clarity 4 entrypoint keeps the principal named by a
;; trait beside a local optional position, then uses it as a map key and in a
;; printed tuple. The production call has exactly these five argument shapes.
(define-public (register
    (staker principal)
    (manager <ft>)
    (start-reward-cycle uint)
    (one-per-cycle bool)
    (fee uint))
  (let (
      (price u10000)
      (num-claims (/ fee price))
      (signer (contract-of manager))
      (key { staker: staker, signer-manager: signer })
      (current (unwrap! (position staker) (err u1))))
    (asserts! (is-eq signer (get signer current)) (err u2))
    (map-set registrations key num-claims)
    (append-registration key)
    (print {
      staker: staker,
      manager: signer,
      start-reward-cycle: start-reward-cycle,
      one-per-cycle: one-per-cycle,
      num-claims: num-claims,
    })
    (ok num-claims)))
";

fn token_a() -> QualifiedContractIdentifier {
    QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.token-a")
        .expect("a contract identifier")
}

fn token_b() -> QualifiedContractIdentifier {
    QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.token-b")
        .expect("a contract identifier")
}

fn market() -> QualifiedContractIdentifier {
    QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.market")
        .expect("a contract identifier")
}

fn encode(contract: &QualifiedContractIdentifier) -> Vec<u8> {
    let mut bytes = Vec::new();
    Value::Principal(contract.clone().into())
        .consensus_serialize(&mut bytes)
        .expect("serialize");
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

/// What each engine answers, for one call.
fn answers(function: &str, arguments: &[Vec<u8>]) -> (String, String) {
    answers_with_market(MARKET, function, arguments)
}

fn answers_with_market(
    market_source: &str,
    function: &str,
    arguments: &[Vec<u8>],
) -> (String, String) {
    let deployments = [
        (token_a(), TOKEN),
        (token_b(), TOKEN),
        (market(), market_source),
    ];
    let mut wasm = Vm::new(Network::TESTNET).expect("create the compiling VM");
    wasm.begin_block(None, [0x71; 32]).expect("begin");
    for (contract, source) in deployments.clone() {
        wasm.deploy_contract(
            contract,
            ClarityVersion::Clarity4,
            source,
            LimitedCostTracker::new_free(),
        )
        .expect("deploy");
    }

    let mut store = MarfStore::new(Network::TESTNET).expect("create the interpreter store");
    store.begin(None, [0x72; 32]).expect("begin");
    for (contract, source) in deployments {
        nano_oracle::deploy_contract(
            &mut store,
            contract,
            ClarityVersion::Clarity4,
            source,
            LimitedCostTracker::new_free(),
        )
        .expect("deploy");
    }

    let sender: PrincipalData = market().issuer.into();
    let compiled = describe(wasm.execute_contract_call_outcome(
        sender.clone(),
        None,
        market(),
        function,
        arguments,
        &LimitedCostTracker::new_free(),
    ));
    let interpreted = describe(nano_oracle::execute_contract_call_outcome(
        &mut store,
        sender,
        None,
        market(),
        function,
        arguments,
        LimitedCostTracker::new_free(),
    ));
    (compiled, interpreted)
}

/// Two trait references naming the same contract are equal, in both engines.
#[test]
fn both_engines_agree_two_traits_naming_one_contract_are_equal() {
    let (compiled, interpreted) = answers("same", &[encode(&token_a()), encode(&token_a())]);
    assert_eq!(
        compiled, interpreted,
        "the engines disagree about `is-eq` over traits"
    );
    assert!(
        compiled.contains("true"),
        "two references to the same contract compared unequal: {compiled}"
    );
}

/// And two naming different contracts are not — which is the half a comparison
/// of the wrong bytes would still get right by accident if it compared nothing.
#[test]
fn both_engines_agree_two_traits_naming_different_contracts_differ() {
    let (compiled, interpreted) = answers("same", &[encode(&token_a()), encode(&token_b())]);
    assert_eq!(
        compiled, interpreted,
        "the engines disagree about `is-eq` over traits"
    );
    assert!(
        compiled.contains("false"),
        "two references to different contracts compared equal: {compiled}"
    );

    let (negated, negated_interpreted) =
        answers("different", &[encode(&token_a()), encode(&token_b())]);
    assert_eq!(
        negated, negated_interpreted,
        "the engines disagree about `not is-eq`"
    );
    assert!(
        negated.contains("true"),
        "`not is-eq` disagreed with `is-eq`: {negated}"
    );
}

/// The comparison and `contract-of` are about the same principal.
///
/// A trait reference is a principal in linear memory, so a comparison reading the
/// wrong offset would not necessarily fail — it would answer. Task 086 is what
/// that looks like on mainnet. Tying the two together is what makes this more
/// than "it compiles".
#[test]
fn a_traits_equality_and_its_contract_of_name_the_same_contract() {
    let (compiled, interpreted) = answers("matches-contract-of", &[encode(&token_a())]);
    assert_eq!(compiled, interpreted, "the engines disagree");
    assert!(
        compiled.contains("true"),
        "a trait is not equal to itself: {compiled}"
    );

    let (named, named_interpreted) = answers("named", &[encode(&token_a())]);
    assert_eq!(
        named, named_interpreted,
        "the engines disagree about `contract-of`"
    );
    assert!(
        named.contains("token-a"),
        "`contract-of` named something other than the trait's contract: {named}"
    );
}

#[test]
fn a_clarity_four_trait_principal_survives_the_registration_shape() {
    let staker = Value::Principal(market().issuer.into())
        .serialize_to_vec()
        .expect("serialize staker");
    let start = Value::UInt(141)
        .serialize_to_vec()
        .expect("serialize start");
    let once = Value::Bool(false)
        .serialize_to_vec()
        .expect("serialize bool");
    let fee = Value::UInt(20_000)
        .serialize_to_vec()
        .expect("serialize fee");
    // The mainnet failure named a *value offset* — 159218 — and the argument it
    // could not read was an unmaterialised `(0, 0)`. Where a value lands is a
    // function of how much static data precedes it, so the shape is checked at
    // several offsets rather than the one this reduction happened to produce.
    for padding in [0, 1, 64, 4_096] {
        let market = format!(
            "(define-constant module-padding \"{}\")\n{MARKET}",
            "x".repeat(padding)
        );
        let (compiled, interpreted) = answers_with_market(
            &market,
            "register",
            &[
                staker.clone(),
                encode(&token_a()),
                start.clone(),
                once.clone(),
                fee.clone(),
            ],
        );
        assert_eq!(
            compiled, interpreted,
            "the engines disagree with {padding} bytes of static data"
        );
        assert!(
            compiled.contains("UInt(2)"),
            "the {padding}-byte registration shape did not return two claims: {compiled}"
        );
    }
}
