//! A mainnet contract clarity-wasm compiles to a module that will not load.
//!
//! `SP2H674PRTZV6YW56K0FMR7GDGZE4ZC5HMYZ3CDEV.flea` is deliberately awkward: one
//! trait returning `(response (list 20 (response uint uint)) uint)` and forty
//! near-identical functions passing that trait along to one that calls it. The
//! compiler accepts it and wasmtime refuses the result — "expected i64, found
//! i32", which is a response's payload width against its indicator's.
//!
//! Neither shape reproduces it alone, and neither does forty of either grown
//! synthetically. Nor does the contract itself, under any Clarity version —
//! which this checks, and which narrows the trigger to the linking context: the
//! node compiles it beside the contracts it calls, and one of those modules is
//! what wasmtime refuses.
//!
//! So this is a guard rather than the reproduction: it pins that the contract
//! is fine on its own, so the next look goes to what it is linked with.

use nano_primitives::Network;
use nano_vm::Vm;

use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::types::QualifiedContractIdentifier;
use clarity::vm::{ClarityVersion, Value};
use stacks_common::codec::StacksMessageCodec;

/// The contract as mainnet holds it.
const FLEA: &str = include_str!("../fixtures/contracts/flea.clar");

#[test]
fn a_mainnet_contract_compiles_to_a_module_that_loads() {
    for version in [
        ClarityVersion::Clarity1,
        ClarityVersion::Clarity2,
        ClarityVersion::Clarity3,
        ClarityVersion::Clarity4,
        ClarityVersion::Clarity5,
        ClarityVersion::Clarity6,
    ] {
        check_loads(version);
    }
}

/// Deploy and call it under one Clarity version.
fn check_loads(version: ClarityVersion) {
    let contract = QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.flea")
        .expect("valid contract identifier");
    let mut vm = Vm::new(Network::TESTNET).expect("create VM");
    vm.begin_block(None, [9; 32]).expect("begin block");

    vm.deploy_contract(
        contract.clone(),
        version,
        FLEA,
        LimitedCostTracker::new_free(),
    )
    .unwrap_or_else(|error| panic!("the contract deploys as {version}: {error}"));

    // Deploying compiles it; calling it is what loads the module, which is
    // where wasmtime refuses.
    let mut buffer = Vec::new();
    Value::buff_from(vec![0])
        .expect("a buffer")
        .consensus_serialize(&mut buffer)
        .expect("serialize");
    let mut target = Vec::new();
    Value::Principal(contract.clone().into())
        .consensus_serialize(&mut target)
        .expect("serialize");

    let outcome = vm.execute_contract_call_outcome(
        contract.issuer.clone().into(),
        None,
        contract,
        "r",
        &[buffer, target],
        &LimitedCostTracker::new_free(),
    );
    // The call itself may fail for want of a real trait implementation; what
    // must not happen is the module failing to load.
    if let Err(error) = &outcome {
        assert!(
            !format!("{error:?}").contains("UnableToLoadModule"),
            "as {version}, the module wasmtime refuses is the fault: {error:?}"
        );
    }
}
