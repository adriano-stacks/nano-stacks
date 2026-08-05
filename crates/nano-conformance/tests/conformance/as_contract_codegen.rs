//! An early return inside `as-contract` emits wasm that will not load.
//!
//! This is what stops a mainnet replay at block 8,665,780.
//! `SP2H674PRTZV6YW56K0FMR7GDGZE4ZC5HMYZ3CDEV.hilt` compiles without a
//! diagnostic and wasmtime refuses the module — "expected i64, found i32" — so
//! the only way to run the block is to answer the call from the interpreter.
//!
//! Reducing 30 KB of contract left one line. `as-contract` did not pass its own
//! type down to the expression it wraps, so the expression was laid out with the
//! type it was analysed with: `(ok u1)` on its own is `(response uint NoType)`,
//! and putting that where a `(response uint uint)` belongs writes an `i32` where
//! the error is two `i64`s. `begin` and `as-contract?` both already did this;
//! `as-contract` was the one that did not.
//!
//! A short return is what makes it visible. Without one the two layouts happen
//! to agree, which is why the same body without a `try!` compiles today.
//!
//! No traits, no folds and no other contracts are involved, though it took all
//! three to find it — mainnet reaches this shape through a `fold` over a list of
//! trait references.

use nano_primitives::Network;
use nano_vm::Vm;

use clarity::types::StacksEpochId;
use clarity::vm::ClarityVersion;
use clarity::vm::types::QualifiedContractIdentifier;

/// An early return inside `as-contract`, with an expression after it.
const EARLY_RETURN: &str =
    "(define-public (sr (a (response bool uint))) (as-contract (begin (try! a) (ok u1))))";

/// The same body without a short return, which compiled before the fix too.
const NO_EARLY_RETURN: &str =
    "(define-public (sr (a (response bool uint))) (as-contract (begin (is-ok a) (ok u1))))";

fn compiles(source: &str) -> Result<(), String> {
    let mut vm = Vm::new(Network::TESTNET).expect("create VM");
    vm.begin_block(None, [13; 32]).expect("begin block");
    let contract = QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.early")
        .expect("a contract identifier");
    vm.check_module(
        &contract,
        ClarityVersion::Clarity3,
        source,
        StacksEpochId::Epoch34,
    )
    .map_err(|error| format!("{error:?}"))
}

#[test]
fn an_early_return_inside_as_contract_compiles_to_a_module_that_loads() {
    compiles(NO_EARLY_RETURN).expect("a body without an early return compiles");
    compiles(EARLY_RETURN).expect("an early return followed by an expression compiles");
}

/// A contract principal written where the callee expects a trait.
///
/// The type checker leaves such an argument unannotated, and clarity-wasm
/// refused to compile the contract at all — "contract-call? argument must be
/// typed". On the wire it is a principal either way.
///
/// `SP2C2YFP12AJZB4MABJBAJ55XECVS7E4PMMZ89YZR.arkadiko-swap-v2-1` does this, and
/// a mainnet transaction calling into it wrote nothing where the network wrote
/// state, so block 8,665,782's root diverged with the transaction merely failing.
const CONTRACT_AS_TRAIT: &str = "
(define-trait mintable ((mint (uint principal) (response bool uint))))
(define-public (mint-through (target <mintable>) (amount uint))
  (contract-call? target mint amount tx-sender))
(define-public (mint-here (amount uint))
  (contract-call? .token mint amount tx-sender))
";

#[test]
fn a_contract_principal_where_a_trait_is_expected_compiles() {
    let mut vm = Vm::new(Network::TESTNET).expect("create VM");
    vm.begin_block(None, [17; 32]).expect("begin block");
    vm.deploy_contract(
        QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.token")
            .expect("a contract identifier"),
        ClarityVersion::Clarity2,
        "(define-public (mint (amount uint) (who principal)) (ok true))",
        clarity::vm::costs::LimitedCostTracker::new_free(),
    )
    .expect("the token deploys");

    let contract = QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.minter")
        .expect("a contract identifier");
    vm.check_module(
        &contract,
        ClarityVersion::Clarity2,
        CONTRACT_AS_TRAIT,
        StacksEpochId::Epoch34,
    )
    .expect("a contract principal passed where a trait is expected compiles");
}

/// A constant naming a contract is as static a call as a literal is.
///
/// clarity-wasm only recognised the literal form, so
/// `(contract-call? SOME_CONSTANT f)` was taken for a trait dispatch and refused
/// for not being one. `SP3EWCDA3V8HCP64CSETSNYXZ25WC4AJ95EC0ZEST.dlmm-adapter`
/// routes every swap through a `SWAP_ROUTER` constant and would not deploy.
const CONSTANT_TARGET: &str = "
(define-constant TARGET .token)
(define-public (through (amount uint))
  (contract-call? TARGET mint amount tx-sender))
";

#[test]
fn a_constant_naming_a_contract_is_a_static_call() {
    let mut vm = Vm::new(Network::TESTNET).expect("create VM");
    vm.begin_block(None, [19; 32]).expect("begin block");
    vm.deploy_contract(
        QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.token")
            .expect("a contract identifier"),
        ClarityVersion::Clarity2,
        "(define-public (mint (amount uint) (who principal)) (ok true))",
        clarity::vm::costs::LimitedCostTracker::new_free(),
    )
    .expect("the token deploys");

    let contract = QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.router")
        .expect("a contract identifier");
    vm.check_module(
        &contract,
        ClarityVersion::Clarity2,
        CONSTANT_TARGET,
        StacksEpochId::Epoch34,
    )
    .expect("a call through a constant compiles");
}

/// `merge` overriding an optional field with `none`.
///
/// `none` analyses as `(optional NoType)`, and clarity-wasm laid the overriding
/// tuple out with its own field types rather than the result's — writing an i32
/// where an `(optional uint)` is an indicator and two i64s. The module compiled
/// and would not load, so
/// `SPSX722NK9V3A8D3CVQT0CDY4EBQ3E9FSDDE61FT.governance-v1` would not deploy,
/// and the block that deploys it took four sibling deploys down with it.
const MERGE_NONE: &str = "
(define-map proposals (buff 32) { closed: bool, execute-at: (optional uint) })
(define-private (close (id (buff 32)))
  (let ((proposal (unwrap! (map-get? proposals id) (err u1))))
    (begin
      (map-set proposals id (merge proposal { execute-at: none }))
      (ok true))))
";

#[test]
fn merging_none_over_an_optional_field_compiles_to_a_module_that_loads() {
    let mut vm = Vm::new(Network::TESTNET).expect("create VM");
    vm.begin_block(None, [23; 32]).expect("begin block");
    let contract = QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.proposals")
        .expect("a contract identifier");
    vm.check_module(
        &contract,
        ClarityVersion::Clarity3,
        MERGE_NONE,
        StacksEpochId::Epoch34,
    )
    .expect("merging none over an optional field compiles");
}

