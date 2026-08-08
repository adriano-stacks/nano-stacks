//! The shape mainnet block 8,671,301 diverges on, in both engines.
//!
//! `SP3JNSEXAZP4BDSHV0DN3M8R3P0MY0EEBQQZX743X.xtrata-market-sponsored-stx-v1-1`
//! is an NFT marketplace. It holds escrowed inscriptions and hands one to a
//! buyer inside a Clarity 4 allowance:
//!
//! ```clarity
//! (as-contract? ((with-nft (contract-of nft-contract) "xtrata-inscription" (list token-id)))
//!   (contract-call? nft-contract transfer token-id CONTRACT-PRINCIPAL buyer))
//! ```
//!
//! The market defines no NFT of its own — the asset belongs to whichever
//! inscription core the listing named. clarity-wasm read the asset's key type
//! out of the *calling* contract's `meta_nft` to know how to decode the
//! identifier list, found nothing, and trapped: the chain answered `(ok true)`
//! and nano failed the whole block with
//! `Expect("NoSuchNFT(\"xtrata-inscription\")")`.
//!
//! The reference asks nothing of the asset. `check_allowance_with_nft` requires
//! only that the third argument is a list of at most `MAX_NFT_IDENTIFIERS`, and
//! `special_allowance` evaluates the three arguments into an `NftAllowance`
//! without consulting any contract — so an allowance may name an asset that
//! exists nowhere, and the allowance simply never matches anything. That is why
//! the type now comes from the compiler, which knows it, rather than from a
//! database read the reference never makes.
//!
//! The interpreter is the oracle and nothing else: clarity-wasm has to be the
//! engine that runs mainnet, so a disagreement is a compiler bug to fix.
//!
//! The evidence that the fix is the fix is the mainnet transaction itself, read
//! either side of it with `xtask call-both-tx` against the live state at
//! 8,671,300:
//!
//! ```text
//! before  compiler Internal(InvariantViolation(… Expect("NoSuchNFT("xtrata-inscription")")))
//!         interpreter Response { committed: true, data: Bool(true) }
//! after   both     Response { committed: true, data: Bool(true) }
//! ```
//!
//! and the chain says `(ok true)`. This file is the regression gate for it, in a
//! shape that needs no chainstate.

use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::types::QualifiedContractIdentifier;
use clarity::vm::{ClarityVersion, Value};
use nano_primitives::Network;
use nano_vm::{MarfStore, Vm};
use stacks_common::codec::StacksMessageCodec;

/// The inscription core: it owns the NFT the market never defines.
const CORE: &str = r"
(define-non-fungible-token xtrata-inscription uint)
(define-public (mint (id uint) (to principal))
  (nft-mint? xtrata-inscription id to))
(define-public (transfer (id uint) (from principal) (to principal))
  (nft-transfer? xtrata-inscription id from to))
(define-read-only (owner (id uint)) (nft-get-owner? xtrata-inscription id))
";

/// A second core with a differently-typed key, so the allowance's element type
/// cannot be guessed from "the one NFT anybody has".
const OTHER_CORE: &str = r"
(define-non-fungible-token xtrata-inscription (buff 32))
(define-public (mint (id (buff 32)) (to principal))
  (nft-mint? xtrata-inscription id to))
(define-public (transfer (id (buff 32)) (from principal) (to principal))
  (nft-transfer? xtrata-inscription id from to))
";

/// The market's shape: allowances over an asset it does not define.
///
/// `hold` escrows an inscription so `release` has something to move; that is
/// what makes the allowance's outcome observable rather than vacuous.
const MARKET: &str = r#"
(define-constant CORE 'ST000000000000000000002AMW42H.core)
(define-constant OTHER 'ST000000000000000000002AMW42H.other)
;; Spelled out rather than `(as-contract tx-sender)`: Clarity 4 replaced
;; `as-contract` with `as-contract?`, and a constant cannot open an allowance.
(define-constant SELF 'ST000000000000000000002AMW42H.market)

(define-public (hold (id uint))
  (contract-call? 'ST000000000000000000002AMW42H.core transfer id tx-sender SELF))

;; The mainnet shape: a named asset belonging to another contract.
(define-public (release (id uint) (to principal))
  (as-contract?
    ((with-nft CORE "xtrata-inscription" (list id)))
    (try! (contract-call? 'ST000000000000000000002AMW42H.core transfer id SELF to))))

;; Allowed for an identifier the transfer does not use, so the allowance is
;; what refuses it rather than the token.
(define-public (release-unallowed (id uint) (to principal))
  (as-contract?
    ((with-nft CORE "xtrata-inscription" (list (+ id u1000))))
    (try! (contract-call? 'ST000000000000000000002AMW42H.core transfer id SELF to))))

;; The wildcard, over the same foreign asset.
(define-public (release-wildcard (id uint) (to principal))
  (as-contract?
    ((with-nft CORE "*" (list id)))
    (try! (contract-call? 'ST000000000000000000002AMW42H.core transfer id SELF to))))

;; An allowance naming an asset whose key type is not this list's, in a contract
;; that is not the caller. Nothing here can supply `uint` for it, so this is the
;; case a lookup would answer wrongly rather than not at all.
(define-public (release-other-key (id uint) (to principal))
  (as-contract?
    ((with-nft OTHER "xtrata-inscription" (list 0x01)))
    (try! (contract-call? 'ST000000000000000000002AMW42H.core transfer id SELF to))))

;; And one naming an asset that exists in no contract at all, which the
;; reference accepts and which therefore allows nothing.
(define-public (release-unknown-asset (id uint) (to principal))
  (as-contract?
    ((with-nft CORE "no-such-inscription" (list id)))
    (try! (contract-call? 'ST000000000000000000002AMW42H.core transfer id SELF to))))
"#;

fn id(name: &str) -> QualifiedContractIdentifier {
    QualifiedContractIdentifier::parse(&format!("ST000000000000000000002AMW42H.{name}"))
        .expect("a contract identifier")
}

fn serialized(value: &Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    value.consensus_serialize(&mut bytes).expect("serialize");
    bytes
}

/// Mint an inscription, escrow it in the market, then run `function`, under both
/// engines. Answers each engine's rendering of the result.
fn both(function: &str, arguments: &[Vec<u8>]) -> (String, String) {
    let contracts = [
        (id("core"), CORE),
        (id("other"), OTHER_CORE),
        (id("market"), MARKET),
    ];
    let owner: Value = Value::Principal(id("market").issuer.into());
    let token = serialized(&Value::UInt(1));

    let describe = |outcome: Result<nano_vm::ContractCallOutcome, _>| match outcome {
        Ok(
            nano_vm::ContractCallOutcome::Success(result)
            | nano_vm::ContractCallOutcome::AbortedByResponse(result),
        ) => format!("{:?}", result.value),
        Ok(nano_vm::ContractCallOutcome::RuntimeFailure { error, .. }) => {
            format!("failed: {error}")
        }
        Err(error) => format!("error: {error}"),
    };

    let mut wasm = Vm::new(Network::TESTNET).expect("create the compiling VM");
    wasm.begin_block(None, [0x51; 32]).expect("begin");
    for (contract, source) in &contracts {
        wasm.deploy_contract(
            contract.clone(),
            ClarityVersion::Clarity4,
            source,
            LimitedCostTracker::new_free(),
        )
        .expect("deploy under the compiler");
    }
    let mut compiled_call = |contract: &str, name: &str, arguments: &[Vec<u8>]| {
        describe(wasm.execute_contract_call_outcome(
            id("market").issuer.into(),
            None,
            id(contract),
            name,
            arguments,
            &LimitedCostTracker::new_free(),
        ))
    };
    compiled_call("core", "mint", &[token.clone(), serialized(&owner)]);
    compiled_call("market", "hold", std::slice::from_ref(&token));
    let compiled = compiled_call("market", function, arguments);

    let mut store = MarfStore::new(Network::TESTNET).expect("create the interpreter store");
    store.begin(None, [0x52; 32]).expect("begin");
    for (contract, source) in &contracts {
        nano_oracle::deploy_contract(
            &mut store,
            contract.clone(),
            ClarityVersion::Clarity4,
            source,
            LimitedCostTracker::new_free(),
        )
        .expect("deploy under the interpreter");
    }
    let mut interpreted_call = |contract: &str, name: &str, arguments: &[Vec<u8>]| {
        describe(nano_oracle::execute_contract_call_outcome(
            &mut store,
            id("market").issuer.into(),
            None,
            id(contract),
            name,
            arguments,
            LimitedCostTracker::new_free(),
        ))
    };
    interpreted_call("core", "mint", &[token.clone(), serialized(&owner)]);
    interpreted_call("market", "hold", &[token]);
    let interpreted = interpreted_call("market", function, arguments);

    (compiled, interpreted)
}

/// An allowance over another contract's NFT, which is mainnet 8,671,301.
///
/// The market has no `define-non-fungible-token`, so reading the key type out of
/// the calling contract had nothing to find and refused a call the chain
/// accepted.
#[test]
fn an_allowance_over_another_contracts_nft_agrees() {
    let buyer = serialized(&Value::Principal(id("core").issuer.into()));
    let (compiled, interpreted) = both("release", &[serialized(&Value::UInt(1)), buyer]);
    assert_eq!(compiled, interpreted);
    assert!(
        compiled.contains("committed: true"),
        "the transfer the allowance permits succeeds: {compiled}"
    );
}

/// The allowance still has to bind: allowing a different identifier refuses.
///
/// Without this the test above would pass on an allowance that permitted
/// everything, which is the direction a wrong answer here is dangerous in.
#[test]
fn an_allowance_naming_another_identifier_refuses_in_both() {
    let buyer = serialized(&Value::Principal(id("core").issuer.into()));
    let (compiled, interpreted) = both("release-unallowed", &[serialized(&Value::UInt(1)), buyer]);
    assert_eq!(compiled, interpreted);
    assert!(
        !compiled.contains("committed: true"),
        "an identifier that was not allowed does not move: {compiled}"
    );
}

/// The wildcard, the differently-typed asset, and the asset that exists nowhere.
///
/// The last two are the cases a key-type lookup answers *wrongly* rather than
/// not at all — one would find a `(buff 32)` where the list holds `uint`s, and
/// one would find nothing in any contract, which the reference tolerates because
/// it never looks.
#[test]
fn the_allowance_shapes_no_definition_can_supply_a_type_for_agree() {
    let buyer = serialized(&Value::Principal(id("core").issuer.into()));
    for function in [
        "release-wildcard",
        "release-other-key",
        "release-unknown-asset",
    ] {
        let (compiled, interpreted) = both(function, &[serialized(&Value::UInt(1)), buyer.clone()]);
        assert_eq!(compiled, interpreted, "`{function}` agrees");
    }
}
