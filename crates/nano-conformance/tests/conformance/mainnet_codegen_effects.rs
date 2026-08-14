//! Full transaction surfaces for the three code-generation reductions in task 093.

use clarity::{
    types::StacksEpochId,
    vm::{
        ClarityVersion, Value,
        costs::{ExecutionCost, LimitedCostTracker},
        types::QualifiedContractIdentifier,
    },
};
use nano_primitives::Network;
use nano_vm::{ContractCallOutcome, EPOCH_4_BLOCK_LIMIT, MarfStore, TransactionResult, Vm};

const TOKEN: &str = r"
(define-trait ft ((transfer (uint) (response bool uint))))
(define-public (transfer (amount uint)) (ok true))
";

const TRAIT_EQUALITY: &str = r#"
(use-trait ft .token.ft)
(define-data-var stored bool false)
(define-public (run (left <ft>) (right <ft>))
  (let ((answer (is-eq left right)))
    (begin
      (var-set stored answer)
      (print { reduction: "trait-equality", answer: answer })
      (ok answer))))
(define-read-only (read-stored) (var-get stored))
"#;

const MAP_OVER_STRING: &str = r#"
(define-data-var stored (list 8 (string-ascii 256)) (list))
(define-private (widen (character (string-ascii 256))) character)
(define-public (run (text (string-ascii 8)))
  (let ((answer (map widen text)))
    (begin
      (var-set stored answer)
      (print { reduction: "map-string", answer: answer })
      (ok answer))))
(define-read-only (read-stored) (var-get stored))
"#;

const APPEND_WIDE_RESULT: &str = r#"
(define-data-var stored (list 2 { kept: uint }) (list))
(define-private (wide) { kept: u2, extra: u3 })
(define-public (run)
  (let ((answer (append (list { kept: u1 }) (wide))))
    (begin
      (var-set stored answer)
      (print { reduction: "append-wide", answer: answer })
      (ok answer))))
(define-read-only (read-stored) (var-get stored))
"#;

#[derive(Debug, PartialEq)]
struct Observation {
    transaction: TransactionResult,
    stored: Value,
    root: [u8; 32],
}

fn contract(name: &str) -> QualifiedContractIdentifier {
    QualifiedContractIdentifier::parse(&format!("ST000000000000000000002AMW42H.{name}"))
        .expect("a contract identifier")
}

fn encoded(arguments: &[Value]) -> Vec<Vec<u8>> {
    arguments
        .iter()
        .map(|argument| {
            argument
                .serialize_to_vec()
                .expect("a consensus-serializable argument")
        })
        .collect()
}

fn transaction(outcome: ContractCallOutcome) -> TransactionResult {
    match outcome {
        ContractCallOutcome::Success(result) => *result,
        ContractCallOutcome::AbortedByResponse(result) => {
            panic!("the public call aborted: {:?}", result.value)
        }
        ContractCallOutcome::RuntimeFailure { error, .. } => {
            panic!("the public call failed: {error:?}")
        }
    }
}

fn value(outcome: ContractCallOutcome) -> Value {
    transaction(outcome)
        .value
        .expect("the read-only call returns a value")
}

fn tracker(store: &mut MarfStore) -> LimitedCostTracker {
    let mut database = store.as_clarity_db();
    database.begin();
    database
        .set_clarity_epoch_version(StacksEpochId::Epoch40)
        .expect("declare the executing epoch");
    let tracker = LimitedCostTracker::new_mid_block(
        Network::TESTNET.is_mainnet(),
        Network::TESTNET.chain_id(),
        EPOCH_4_BLOCK_LIMIT,
        &mut database,
        StacksEpochId::Epoch40,
    )
    .expect("epoch 4 costs are native");
    database
        .roll_back()
        .expect("reading the cost schedule writes nothing");
    tracker
}

fn compiled(source: &str, dependencies: &[(&str, &str)], arguments: &[Value]) -> Observation {
    let directory = tempfile::tempdir().expect("a compiled state directory");
    let mut vm = Vm::open(Network::TESTNET, directory.path()).expect("open the compiling VM");
    vm.begin_block(None, [0x93; 32]).expect("begin a block");
    for (name, dependency) in dependencies {
        vm.deploy_contract(
            contract(name),
            ClarityVersion::Clarity4,
            dependency,
            LimitedCostTracker::new_free(),
        )
        .expect("deploy a dependency");
    }
    vm.deploy_contract(
        contract("reduction"),
        ClarityVersion::Clarity4,
        source,
        LimitedCostTracker::new_free(),
    )
    .expect("deploy the reduction");

    let cost = vm
        .transaction_cost_tracker()
        .expect("a consensus cost tracker");
    let transaction = transaction(
        vm.execute_contract_call_outcome(
            contract("reduction").issuer.into(),
            None,
            contract("reduction"),
            "run",
            &encoded(arguments),
            &cost,
        )
        .expect("execute the reduction"),
    );
    let cost = vm
        .transaction_cost_tracker()
        .expect("a consensus cost tracker");
    let stored = value(
        vm.execute_contract_call_outcome(
            contract("reduction").issuer.into(),
            None,
            contract("reduction"),
            "read-stored",
            &[],
            &cost,
        )
        .expect("read the committed value"),
    );
    let root = vm.seal_block().expect("seal the compiled state").0;
    Observation {
        transaction,
        stored,
        root,
    }
}

fn interpreted(source: &str, dependencies: &[(&str, &str)], arguments: &[Value]) -> Observation {
    let directory = tempfile::tempdir().expect("an interpreter state directory");
    let mut store =
        MarfStore::open(Network::TESTNET, directory.path()).expect("open the interpreter store");
    store.begin(None, [0x93; 32]).expect("begin a block");
    for (name, dependency) in dependencies {
        nano_oracle::deploy_contract(
            &mut store,
            contract(name),
            ClarityVersion::Clarity4,
            dependency,
            LimitedCostTracker::new_free(),
        )
        .expect("deploy a dependency");
    }
    nano_oracle::deploy_contract(
        &mut store,
        contract("reduction"),
        ClarityVersion::Clarity4,
        source,
        LimitedCostTracker::new_free(),
    )
    .expect("deploy the reduction");

    let cost = tracker(&mut store);
    let transaction = transaction(
        nano_oracle::execute_contract_call_outcome(
            &mut store,
            contract("reduction").issuer.into(),
            None,
            contract("reduction"),
            "run",
            &encoded(arguments),
            cost,
        )
        .expect("execute the reduction"),
    );
    let cost = tracker(&mut store);
    let stored = value(
        nano_oracle::execute_contract_call_outcome(
            &mut store,
            contract("reduction").issuer.into(),
            None,
            contract("reduction"),
            "read-stored",
            &[],
            cost,
        )
        .expect("read the committed value"),
    );
    let root = store.seal().expect("seal the interpreted state").0;
    Observation {
        transaction,
        stored,
        root,
    }
}

fn response_value(transaction: &TransactionResult) -> Value {
    let Some(Value::Response(response)) = &transaction.value else {
        panic!(
            "the public call did not return a response: {:?}",
            transaction.value
        );
    };
    assert!(
        response.committed,
        "the public call returned an error response"
    );
    response.data.as_ref().clone()
}

fn assert_surfaces(source: &str, dependencies: &[(&str, &str)], arguments: &[Value]) {
    let compiled = compiled(source, dependencies, arguments);
    let interpreted = interpreted(source, dependencies, arguments);

    assert_ne!(
        compiled.transaction.cost,
        ExecutionCost::ZERO,
        "a free tracker would make the cost comparison vacuous"
    );
    assert_eq!(
        compiled.transaction.events.len(),
        1,
        "the reduction must expose one ordered print event"
    );
    assert_eq!(
        compiled.stored,
        response_value(&compiled.transaction),
        "the value returned by the transaction was not committed"
    );
    assert_eq!(
        compiled, interpreted,
        "clarity-wasm and the reference interpreter disagree on a transaction surface"
    );
}

#[test]
fn trait_equality_matches_result_cost_events_assets_and_writes() {
    let token = Value::Principal(contract("token").into());
    assert_surfaces(TRAIT_EQUALITY, &[("token", TOKEN)], &[token.clone(), token]);
}

#[test]
fn map_over_a_string_matches_result_cost_events_assets_and_writes() {
    let text = Value::string_ascii_from_bytes(b"abcd".to_vec()).expect("an ASCII string");
    assert_surfaces(MAP_OVER_STRING, &[], &[text]);
}

#[test]
fn append_wide_function_result_matches_result_cost_events_assets_and_writes() {
    assert_surfaces(APPEND_WIDE_RESULT, &[], &[]);
}

fn assert_deployment_surfaces(name: &str, source: &str) {
    let compiled_directory = tempfile::tempdir().expect("a compiled state directory");
    let mut vm = Vm::open(Network::TESTNET, compiled_directory.path()).expect("open the VM");
    vm.begin_block(None, [0x94; 32]).expect("begin a block");
    let cost = vm
        .transaction_cost_tracker()
        .expect("a consensus cost tracker");
    let compiled = vm
        .deploy_contract(contract(name), ClarityVersion::Clarity2, source, cost)
        .expect("deploy with clarity-wasm");
    let compiled_root = vm.seal_block().expect("seal the compiled state").0;

    let interpreted_directory = tempfile::tempdir().expect("an interpreter state directory");
    let mut store = MarfStore::open(Network::TESTNET, interpreted_directory.path())
        .expect("open the interpreter store");
    store.begin(None, [0x94; 32]).expect("begin a block");
    let cost = tracker(&mut store);
    let interpreted = nano_oracle::deploy_contract(
        &mut store,
        contract(name),
        ClarityVersion::Clarity2,
        source,
        cost,
    )
    .expect("deploy with the interpreter");
    let interpreted_root = store.seal().expect("seal the interpreted state").0;

    assert!(
        interpreted.cost != ExecutionCost::ZERO,
        "a free tracker would make the cost comparison vacuous"
    );
    assert_eq!(compiled, interpreted, "deployment receipts disagree");
    assert_eq!(compiled_root, interpreted_root, "deployment roots disagree");
}

#[test]
fn contract_deployment_matches_result_cost_events_assets_and_writes() {
    assert_deployment_surfaces("deployment", "(define-data-var stored uint u0)");
}

#[test]
fn every_contract_definition_matches_deployment_cost_and_writes() {
    const SOURCE: &str = r"
        (define-trait reader
            ((read ((optional uint)) (response (list 2 uint) uint))))
        (define-constant stored-constant u1)
        (define-data-var stored-var (optional uint) (some u1))
        (define-map stored-map { key: uint } { value: (optional uint) })
        (define-fungible-token token u100)
        (define-non-fungible-token collectible (buff 4))
        (define-private (identity (value (optional (list 2 uint))))
            (default-to (list) value))
    ";

    assert_deployment_surfaces("definitions", SOURCE);
}
