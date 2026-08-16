//! The interpreter's answers, and where they must match the compiler's.
//!
//! These tests were `nano-vm`'s. They moved here with the interpreter: a crate
//! the node is built from cannot name an interpreter entry point, and a test in
//! that crate is exactly such a name. What they check has not changed.

use clarity::vm::{
    ClarityVersion, Value,
    costs::LimitedCostTracker,
    types::{PrincipalData, QualifiedContractIdentifier},
};
use nano_oracle::{
    deploy_contract, evaluate, evaluate_in_store, execute_contract_call,
    execute_contract_call_outcome,
};
use nano_primitives::Network;
use nano_vm::{ContractCallOutcome, MarfStore, Vm};

#[test]
fn evaluates_clarity_six_programs() {
    let value =
        evaluate(Network::TESTNET, "(+ u20 u22)").expect("Clarity 6 program should evaluate");

    assert_eq!(value, Some(Value::UInt(42)));
}

#[test]
fn supports_epoch_four_clarity_six_words() {
    let concatenated = evaluate(Network::TESTNET, "(concat 0x01 0x02 0x03)")
        .expect("variadic concat should evaluate");
    let parsed_bitcoin = evaluate(Network::TESTNET, "(get-bitcoin-tx-output? 0x00 u0)")
        .expect("bitcoin transaction parser should evaluate");

    assert_eq!(
        concatenated,
        Some(Value::buff_from(vec![1, 2, 3]).expect("valid buffer"))
    );
    assert_eq!(parsed_bitcoin, Some(Value::err_uint(1)));
}

#[test]
fn rejects_invalid_programs() {
    assert!(evaluate(Network::TESTNET, "(unknown-word u1)").is_err());
}

#[test]
fn evaluates_against_an_active_marf_store() {
    let mut store = MarfStore::new(Network::TESTNET).expect("create MARF store");
    store.begin(None, [1; 32]).expect("begin state");

    let value = evaluate_in_store(&mut store, "(+ u20 u22)").expect("evaluate against MARF store");

    assert_eq!(value, Some(Value::UInt(42)));
    store.seal().expect("seal state");
}

#[test]
fn persists_clarity_data_variables_in_the_marf_store() {
    let mut store = MarfStore::new(Network::TESTNET).expect("create MARF store");
    let block = [1; 32];
    store.begin(None, block).expect("begin state");

    let value = evaluate_in_store(
        &mut store,
        "(define-data-var counter uint u1) (var-set counter u2) (var-get counter)",
    )
    .expect("evaluate persistent data variable");

    assert_eq!(value, Some(Value::UInt(2)));
    store.seal().expect("seal state");
    assert!(store.root(block).expect("read the sealed root").is_some());
}

/// Reading a length out of a buffer, which decides how much work follows.
///
/// Wormhole reads its signature count from one byte of the VAA and slices a
/// nineteen-element list down to it. A count that comes out too large makes
/// that slice answer `none`, `unwrap-panic` fail, and the recovery loop run
/// longer on the way — which is the shape of the divergence at 8,665,719.
#[test]
fn a_length_read_from_a_buffer_is_what_the_bytes_say() {
    for (program, expected) in [
        // One byte at an offset, as `read-uint-8` does it.
        (
            "(buff-to-uint-be (unwrap-panic (slice? 0x00000000000d u5 u6)))",
            "u13",
        ),
        (
            "(buff-to-uint-be (unwrap-panic (slice? 0x0000000000ff u5 u6)))",
            "u255",
        ),
        // Four bytes, as `read-uint-32` does it.
        (
            "(buff-to-uint-be (unwrap-panic (slice? 0x0000000007ff u1 u5)))",
            "u7",
        ),
        // A buffer slice at its exact end is still in range.
        ("(unwrap-panic (slice? 0x0102 u0 u2))", "0x0102"),
        // And one starting at the end is not, the same as for a list.
        ("(slice? 0x0102 u2 u2)", "none"),
    ] {
        let value = evaluate(Network::TESTNET, program)
            .expect("the program evaluates")
            .expect("the program returns a value");
        assert_eq!(format!("{value}"), expected, "{program}");
    }
}

/// `slice?` with a bound the compiler cannot see, which is the shape a
/// VAA check uses.
///
/// Wormhole slices a nineteen-element list down to a signature count read
/// out of the message at run time, so the bound is a value rather than a
/// literal — and a compiler may treat the two differently.
#[test]
fn slice_over_a_list_with_a_runtime_bound() {
    for (program, expected) in [
        (
            "(let ((n (unwrap-panic (slice? 0x0002 u1 u2)))) \
               (slice? (list u1 u2 u3) u0 (buff-to-uint-be n)))",
            "(some (u1 u2))",
        ),
        (
            "(let ((n (unwrap-panic (slice? 0x0000 u1 u2)))) \
               (slice? (list u1 u2 u3) u0 (buff-to-uint-be n)))",
            "(some ())",
        ),
        (
            "(let ((n (unwrap-panic (slice? 0x0009 u1 u2)))) \
               (slice? (list u1 u2 u3) u0 (buff-to-uint-be n)))",
            "none",
        ),
    ] {
        let value = evaluate(Network::TESTNET, program)
            .expect("the program evaluates")
            .expect("the program returns a value");
        assert_eq!(format!("{value}"), expected, "{program}");
    }
}

/// `map` across two lists, which is how signatures meet their hashes.
///
/// Wormhole recovers a key per signature with
/// `(map recover-public-key signatures vaa-body-hash-list)`, so a two-list
/// map that pairs wrongly or stops at the wrong length recovers the wrong
/// keys from good signatures.
#[test]
fn map_across_two_lists_pairs_them_in_order() {
    for (program, expected) in [
        (
            "(map + (list u1 u2 u3) (list u10 u20 u30))",
            "(u11 u22 u33)",
        ),
        // The shorter list decides how far it goes.
        ("(map + (list u1 u2 u3) (list u10 u20))", "(u11 u22)"),
        ("(map + (list u1) (list u10 u20 u30))", "(u11)"),
    ] {
        let value = evaluate(Network::TESTNET, program)
            .expect("map evaluates")
            .expect("map returns a value");
        assert_eq!(format!("{value}"), expected, "{program}");
    }
}

/// `slice?` over a list, which a VAA check unwraps without a fallback.
///
/// Wormhole's core contract slices a nineteen-element list down to the
/// number of signatures it has and `unwrap-panic`s the result, so a `slice?`
/// answering `none` fails the whole verification and reads as an unwrap of
/// an error far from the word that was wrong. The bounds are the subtle
/// part, and they are stacks-core's rather than the obvious ones.
#[test]
fn slice_over_a_list_answers_for_every_range() {
    for (range, expected) in [
        ("u0 u2", Some("(u1 u2)")),
        ("u0 u3", Some("(u1 u2 u3)")),
        ("u1 u3", Some("(u2 u3)")),
        ("u0 u0", Some("()")),
        // `left >= len` is out of bounds even when the range is empty,
        // which is stacks-core's check and not an obvious one.
        ("u3 u3", None),
        ("u2 u1", None),
        ("u0 u4", None),
    ] {
        let value = evaluate(
            Network::TESTNET,
            &format!("(slice? (list u1 u2 u3) {range})"),
        )
        .expect("slice? evaluates")
        .expect("slice? returns a value");
        let shown = format!("{value}");
        match expected {
            Some(items) => assert_eq!(shown, format!("(some {items})"), "slice? {range}"),
            None => assert_eq!(shown, "none", "slice? {range}"),
        }
    }
}

/// The crypto words a signature-verifying contract stands on.
///
/// A mainnet market reaches a wormhole guardian-set check on its way
/// through `borrow`, and a recovery or a hash that differs makes the whole
/// verification fail — which reads as an unwrap of an error, nowhere near
/// the word that was wrong.
#[test]
fn the_signature_words_agree_with_their_known_vectors() {
    // keccak256 of the empty buffer, the canonical vector.
    assert_eq!(
        evaluate(Network::TESTNET, "(keccak256 0x)").expect("keccak256 evaluates"),
        Some(
            Value::buff_from(
                hex::decode("c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470")
                    .expect("a hash")
            )
            .expect("a buffer")
        )
    );

    // A recovery, which is what a guardian check actually does.
    let recovered = evaluate(
        Network::TESTNET,
        "(secp256k1-recover? \
         0xde5b9eb9e7c5592930eb2e30a01369c36586d872082ed8181ee83d2a0ec20f04 \
         0x8738487ebe69b93d8e51583be8eee50bb4213fc49c767d329632730cc193b873\
         554428fc936ca3569afc15f1c9365f6591d6251a89fee9c9ac661116824d3a1301)",
    )
    .expect("secp256k1-recover? evaluates");
    assert_eq!(
        recovered,
        Some(
            Value::okay(
                Value::buff_from(
                    hex::decode(
                        "03adb8de4bfb65db2cfd6120d55c6526ae9c52e675db7e47308636534ba7786110"
                    )
                    .expect("a key")
                )
                .expect("a buffer")
            )
            .expect("ok")
        )
    );

    // sha256 of the empty buffer, for contrast: a contract hashing a
    // message the wrong way recovers the wrong key from a good signature.
    assert_eq!(
        evaluate(Network::TESTNET, "(sha256 0x)").expect("sha256 evaluates"),
        Some(
            Value::buff_from(
                hex::decode("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
                    .expect("a hash")
            )
            .expect("a buffer")
        )
    );
}

fn assert_successful_wasm_call_matches_interpreter(
    wasm: &mut Vm,
    store: &mut MarfStore,
    sender: PrincipalData,
    contract: QualifiedContractIdentifier,
    function: &str,
    arguments: &[Vec<u8>],
) {
    let wasm_result = wasm
        .execute_contract_call(
            sender.clone(),
            None,
            contract.clone(),
            function,
            arguments,
            &LimitedCostTracker::new_free(),
        )
        .expect("call WASM contract");
    let interpreter_result = execute_contract_call(
        store,
        sender,
        None,
        contract,
        function,
        arguments,
        LimitedCostTracker::new_free(),
    )
    .expect("call interpreter contract");

    assert_eq!(wasm_result.value, interpreter_result.value);
    assert_eq!(wasm_result.cost, interpreter_result.cost);
    assert_eq!(wasm_result.assets, interpreter_result.assets);
    assert_eq!(wasm_result.events, interpreter_result.events);
}

fn assert_wasm_failure_matches_interpreter(
    wasm: &mut Vm,
    store: &mut MarfStore,
    sender: PrincipalData,
    contract: QualifiedContractIdentifier,
    function: &str,
    arguments: &[Vec<u8>],
) {
    let wasm_failure = wasm
        .execute_contract_call_outcome(
            sender.clone(),
            None,
            contract.clone(),
            function,
            arguments,
            &LimitedCostTracker::new_free(),
        )
        .expect("execute WASM failure");
    let interpreter_failure = execute_contract_call_outcome(
        store,
        sender,
        None,
        contract,
        function,
        arguments,
        LimitedCostTracker::new_free(),
    )
    .expect("execute interpreter failure");
    let (
        ContractCallOutcome::RuntimeFailure {
            cost: wasm_cost,
            error: wasm_error,
        },
        ContractCallOutcome::RuntimeFailure {
            cost: interpreter_cost,
            error: interpreter_error,
        },
    ) = (wasm_failure, interpreter_failure)
    else {
        panic!("{function} should fail at runtime")
    };
    assert_eq!(wasm_cost, interpreter_cost);
    assert_eq!(wasm_error.to_string(), interpreter_error.to_string());
}

#[test]
fn wasm_calls_match_the_clarity_six_interpreter() {
    let contract = QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.crosscheck")
        .expect("valid contract identifier");
    let source = "
        (define-public (describe (value (optional int)) (items (list 3 int)))
            (let ((count (len items)) (number (default-to 0 value)))
                (ok (tuple (count count) (number number)))))
        (define-public (must-have (value (optional int)))
            (ok (unwrap-panic value)))
    ";
    let arguments = [
        Value::some(Value::Int(7))
            .expect("valid optional")
            .serialize_to_vec()
            .expect("serialize optional"),
        Value::cons_list_unsanitized(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
            .expect("valid list")
            .serialize_to_vec()
            .expect("serialize list"),
    ];
    let sender: PrincipalData = contract.issuer.clone().into();

    let mut wasm = Vm::new(Network::TESTNET).expect("create WASM VM");
    wasm.begin_block(None, [0x71; 32])
        .expect("begin WASM block");
    wasm.deploy_contract(
        contract.clone(),
        ClarityVersion::Clarity6,
        source,
        LimitedCostTracker::new_free(),
    )
    .expect("deploy WASM contract");
    let mut store = MarfStore::new(Network::TESTNET).expect("create interpreter store");
    store
        .begin(None, [0x72; 32])
        .expect("begin interpreter block");
    deploy_contract(
        &mut store,
        contract.clone(),
        ClarityVersion::Clarity6,
        source,
        LimitedCostTracker::new_free(),
    )
    .expect("deploy interpreter contract");
    assert_successful_wasm_call_matches_interpreter(
        &mut wasm,
        &mut store,
        sender,
        contract.clone(),
        "describe",
        &arguments,
    );

    let none = Value::none()
        .serialize_to_vec()
        .expect("serialize optional none");
    assert_wasm_failure_matches_interpreter(
        &mut wasm,
        &mut store,
        contract.issuer.clone().into(),
        contract,
        "must-have",
        std::slice::from_ref(&none),
    );
}

/// Parsing a contract's functions needs nothing else to be present.
///
/// This is what heals a contract the compiler stored with stub bodies when its
/// dependencies are not in this node's state: deploying it beside them is
/// impossible, parsing it is not.
mod rebuild_tests {
    use clarity::vm::ClarityVersion;
    use clarity::vm::types::QualifiedContractIdentifier;

    use nano_oracle::defined_functions;

    /// Names a contract this node does not hold, so it cannot be deployed.
    const SOURCE: &str = "
(define-constant target 'ST000000000000000000002AMW42H.absent)
(define-public (pay (amount uint) (to principal))
  (stx-transfer? amount tx-sender to))
(define-read-only (double (n uint)) (* n u2))
(define-private (helper) (ok true))
(define-data-var counter uint u0)
";

    fn contract() -> QualifiedContractIdentifier {
        QualifiedContractIdentifier::parse("ST000000000000000000002AMW42H.rebuilt")
            .expect("a contract identifier")
    }

    #[test]
    fn every_define_type_is_rebuilt_with_its_real_body() {
        let functions = defined_functions(&contract(), ClarityVersion::Clarity3, SOURCE);
        for name in ["pay", "double", "helper"] {
            assert!(
                functions.contains_key(name),
                "`{name}` was rebuilt from the source"
            );
        }
        assert_eq!(functions.len(), 3, "only the functions, not the data var");

        // The whole point: a real body, not the compiler's `(int 0)` stub.
        let body = format!("{:?}", functions["double"]);
        assert!(
            body.contains('*') || body.contains("Multiply") || body.contains('n'),
            "`double` carries its real body: {body}"
        );
    }

    #[test]
    fn arguments_keep_their_declared_types() {
        let functions = defined_functions(&contract(), ClarityVersion::Clarity3, SOURCE);
        let pay = format!("{:?}", functions["pay"]);
        assert!(
            pay.contains("UInt") && pay.contains("Principal"),
            "`pay` keeps `(uint, principal)`: {pay}"
        );
    }

    #[test]
    fn unparseable_source_yields_nothing_rather_than_a_wrong_contract() {
        let functions = defined_functions(&contract(), ClarityVersion::Clarity3, "(this is not");
        assert!(
            functions.is_empty(),
            "a source that will not parse heals nothing"
        );
    }
}
