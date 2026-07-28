use clar2wasm::tools::TestEnvironment;
use clarity::types::StacksEpochId;
use clarity::vm::types::ResponseData;
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

#[test]
fn evaluates_clarity6_crypto_words_in_wasm() {
    let source = "
        (define-read-only (ed) (ed25519-verify 0x00 0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000 0x0000000000000000000000000000000000000000000000000000000000000000))
        (define-read-only (uncompress) (secp256k1-decompress? 0x03adb8de4bfb65db2cfd6120d55c6526ae9c52e675db7e47308636534ba7786110))";
    let mut environment = TestEnvironment::new(StacksEpochId::Epoch40, ClarityVersion::Clarity6);
    assert_eq!(environment.evaluate(source), Ok(None));
    assert_eq!(
        environment.call_contract("snippet", "ed", &[]),
        Ok(Value::Bool(false))
    );
    assert!(matches!(
        environment.call_contract("snippet", "uncompress", &[]),
        Ok(Value::Response(ResponseData {
            committed: true,
            ..
        }))
    ));
}

#[test]
fn evaluates_clarity6_bitcoin_words_in_wasm() {
    let source = "
        (define-read-only (proof)
            (verify-merkle-proof
                0x1111111111111111111111111111111111111111111111111111111111111111
                0x1111111111111111111111111111111111111111111111111111111111111111
                u0 u1 (list)))
        (define-read-only (output)
            (get-bitcoin-tx-output?
                0x01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff4d04ffff001d0104455468652054696d65732030332f4a616e2f32303039204368616e63656c6c6f72206f6e206272696e6b206f66207365636f6e64206261696c6f757420666f722062616e6b73ffffffff0100f2052a01000000434104678afdb0fe5548271967f1a67130b7105cd6a828e03909a67962e0ea1f61deb649f6bc3f4cef38c4f35504e51ec112de5c384df7ba0b8d578a4c702b6bf11d5fac00000000
                u0))";
    let mut environment = TestEnvironment::new(StacksEpochId::Epoch40, ClarityVersion::Clarity6);
    assert_eq!(environment.evaluate(source), Ok(None));
    assert_eq!(
        environment.call_contract("snippet", "proof", &[]),
        Ok(Value::Bool(true))
    );
    let output = environment.call_contract("snippet", "output", &[]);
    assert!(
        matches!(
            output,
            Ok(Value::Response(ResponseData {
                committed: true,
                ..
            }))
        ),
        "{output:?}"
    );
}
