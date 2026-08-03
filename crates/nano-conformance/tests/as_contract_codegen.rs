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
