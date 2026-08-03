//! A mainnet contract clarity-wasm compiles to a module that will not load.
//!
//! `SP2H674PRTZV6YW56K0FMR7GDGZE4ZC5HMYZ3CDEV.flea` is deliberately awkward: one
//! trait returning `(response (list 20 (response uint uint)) uint)` and forty
//! near-identical functions passing that trait along to one that calls it. The
//! compiler accepts it and wasmtime refuses the result — "expected i64, found
//! i32", which is a response's payload width against its indicator's.
//!
//! Neither shape reproduces it alone, and neither does forty of either grown
//! synthetically. Deploying and calling it in one session does not either — the
//! module built at deploy time is the one that runs, and it is fine.
//!
//! What the node does instead is *rebuild* it: a process that resumes has no
//! module for a contract deployed before it started, so it recompiles from the
//! stored source under the epoch the chain is running at. This deploys, closes
//! the state and reopens it to take that path — and flea's own module still
//! loads and runs, reaching the trait dispatch before it stops.
//!
//! So the module wasmtime refuses is not flea's. The transaction that fails
//! passes `SP2H674PRTZV6YW56K0FMR7GDGZE4ZC5HMYZ3CDEV.hilt` as the trait — 30 KB
//! importing six traits of its own, which fits the 74,624-byte offset in the
//! error far better than this contract does. That is where the next look goes,
//! and it needs the trait-defining contracts deployed alongside it.

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

/// Deploy it under one Clarity version, then call it from a cold process.
fn check_loads(version: ClarityVersion) {
    let contract = QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.flea")
        .expect("valid contract identifier");
    let directory = tempfile::tempdir().expect("a directory");
    let mut vm = Vm::open(Network::TESTNET, directory.path()).expect("open VM");
    vm.begin_block(None, [9; 32]).expect("begin block");

    vm.deploy_contract(
        contract.clone(),
        version,
        FLEA,
        LimitedCostTracker::new_free(),
    )
    .unwrap_or_else(|error| panic!("the contract deploys as {version}: {error}"));
    vm.seal_block().expect("seal the deploy");
    drop(vm);

    // A resumed process holds no module for it and has to rebuild one from the
    // source on disk, which is the compilation that goes wrong.
    let mut vm = Vm::open(Network::TESTNET, directory.path()).expect("reopen VM");
    vm.begin_block(Some([9; 32]), [10; 32]).expect("begin block");

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
    // The call fails for want of a real implementation of the trait — which it
    // can only reach by loading and running flea's module, which is the point.
    if let Err(error) = &outcome {
        assert!(
            !format!("{error:?}").contains("UnableToLoadModule"),
            "as {version}, the module wasmtime refuses is the fault: {error:?}"
        );
    }
}
