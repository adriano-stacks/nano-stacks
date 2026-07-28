use clar2wasm::tools::TestEnvironment;
use clarity::types::StacksEpochId;
use clarity::vm::{ClarityVersion, Value};

#[test]
fn evaluates_a_clarity6_contract_at_epoch40() {
    let mut environment = TestEnvironment::new(StacksEpochId::Epoch40, ClarityVersion::Clarity6);
    assert_eq!(
        environment.evaluate("(define-read-only (double (value uint)) (+ value value))"),
        Ok(None)
    );
    assert_eq!(
        environment.call_contract("snippet", "double", &[Value::UInt(3)]),
        Ok(Value::UInt(6))
    );
}

#[test]
fn dispatches_contract_calls_through_wasm() {
    let mut environment = TestEnvironment::new(StacksEpochId::Epoch40, ClarityVersion::Clarity6);
    assert_eq!(
        environment.init_contract_with_snippet(
            "first",
            "(define-read-only (double (value uint)) (+ value value))",
        ),
        Ok(None)
    );
    assert_eq!(
        environment.init_contract_with_snippet(
            "second",
            "(define-read-only (quad (value uint)) (contract-call? .first double (+ value value)))",
        ),
        Ok(None)
    );
    assert_eq!(
        environment.call_contract("second", "quad", &[Value::UInt(3)]),
        Ok(Value::UInt(12))
    );
}
