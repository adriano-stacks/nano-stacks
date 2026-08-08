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

;; The shape the three mainnet contracts use: two trait references compared.
(define-public (same (a <ft>) (b <ft>)) (ok (is-eq a b)))
(define-public (different (a <ft>) (b <ft>)) (ok (not (is-eq a b))))

;; A trait against the principal it names, which has to agree with `contract-of`.
(define-public (matches-contract-of (a <ft>)) (ok (is-eq (contract-of a) (contract-of a))))
(define-read-only (named (a <ft>)) (contract-of a))
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
    let deployments = [(token_a(), TOKEN), (token_b(), TOKEN), (market(), MARKET)];
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
