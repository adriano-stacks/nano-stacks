use clar2wasm::tools::TestEnvironment;
use clarity::types::StacksEpochId;
use clarity::vm::{ClarityVersion, Value};
use stacks_common::util::secp256r1::{Secp256r1PrivateKey, Secp256r1PublicKey};

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

#[test]
fn evaluates_secp256r1_verify_in_wasm() {
    let private_key = Secp256r1PrivateKey::from_seed(&[1; 32]);
    let public_key = Secp256r1PublicKey::from_private(&private_key);
    let message = [0x11; 32];
    let signature = private_key.sign_digest(&message).unwrap();
    let source = format!(
        "(define-read-only (verify) (secp256r1-verify 0x{} 0x{} 0x{}))",
        hex::encode(message),
        hex::encode(signature.0),
        hex::encode(public_key.to_bytes_compressed()),
    );
    let mut environment = TestEnvironment::new(StacksEpochId::Epoch40, ClarityVersion::Clarity6);
    assert_eq!(environment.evaluate(&source), Ok(None));
    assert_eq!(
        environment.call_contract("snippet", "verify", &[]),
        Ok(Value::Bool(true))
    );
}
