use clarity::vm::{ClarityName, SymbolicExpression};

use super::{ComplexWord, Word};
use crate::check_args;
use crate::cost::WordCharge;
use crate::wasm_generator::{GeneratorError, WasmGenerator};
use crate::wasm_utils::ArgumentCountCheck;

#[derive(Debug)]
pub struct Verify;

impl Word for Verify {
    fn name(&self) -> ClarityName {
        ClarityName::from_literal("secp256r1-verify")
    }
}

impl ComplexWord for Verify {
    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        check_args!(generator, builder, 3, args.len(), ArgumentCountCheck::Exact);
        self.charge(generator, builder, 0)?;
        for argument in args {
            generator.traverse_expr(builder, argument)?;
        }
        builder.call(generator.func_by_name("stdlib.secp256r1_verify"));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clarity::types::StacksEpochId;
    use clarity::util::secp256r1::{Secp256r1PrivateKey, Secp256r1PublicKey};
    use clarity::vm::errors::{RuntimeCheckErrorKind, VmExecutionError};
    use clarity::vm::types::{SequenceSubtype, TypeSignature};
    use clarity::vm::{ClarityVersion, Value};

    use crate::tools::{crosscheck_with_epoch_and_version, evaluate};

    const CONFIGS: [(StacksEpochId, ClarityVersion); 3] = [
        (StacksEpochId::Epoch33, ClarityVersion::Clarity4),
        (StacksEpochId::Epoch34, ClarityVersion::Clarity5),
        (StacksEpochId::Epoch40, ClarityVersion::Clarity6),
    ];

    fn snippet(message: &[u8], signature: &[u8], public_key: &[u8]) -> String {
        format!(
            "(secp256r1-verify 0x{} 0x{} 0x{})",
            hex::encode(message),
            hex::encode(signature),
            hex::encode(public_key)
        )
    }

    fn crosscheck_all(
        source: &str,
        expected: impl Fn() -> Result<Option<Value>, VmExecutionError>,
    ) {
        for (epoch, version) in CONFIGS {
            crosscheck_with_epoch_and_version(source, expected(), epoch, version);
        }
    }

    #[test]
    fn rejects_the_wrong_arity() {
        for source in [
            "(secp256r1-verify 0x00 0x00)",
            "(secp256r1-verify 0x00 0x00 0x00 0x00)",
        ] {
            assert!(evaluate(source).is_err());
        }
    }

    #[test]
    fn message_length_must_be_32() {
        let message = vec![0xab; 31];
        let source = snippet(&message, &[0; 64], &[0; 33]);
        crosscheck_all(&source, || {
            Err(RuntimeCheckErrorKind::TypeValueError(
                Box::new(TypeSignature::BUFFER_32),
                Value::buff_from(message.clone())
                    .expect("a short message is still a valid buffer")
                    .to_error_string(),
            )
            .into())
        });
    }

    #[test]
    fn signature_length_other_than_64_is_false() {
        let source = snippet(&[0; 32], &[0; 63], &[0; 33]);
        crosscheck_all(&source, || Ok(Some(Value::Bool(false))));
    }

    #[test]
    fn public_key_length_must_be_33() {
        let public_key = vec![0xcd; 32];
        let source = snippet(&[0; 32], &[0; 64], &public_key);
        crosscheck_all(&source, || {
            Err(RuntimeCheckErrorKind::TypeValueError(
                Box::new(TypeSignature::SequenceType(SequenceSubtype::BufferType(
                    33_u32.try_into().expect("33 is a valid buffer bound"),
                ))),
                Value::buff_from(public_key.clone())
                    .expect("a short public key is still a valid buffer")
                    .to_error_string(),
            )
            .into())
        });
    }

    #[test]
    fn hashing_scheme_follows_the_clarity_version() {
        let private_key = Secp256r1PrivateKey::from_seed(&[1; 32]);
        let public_key = Secp256r1PublicKey::from_private(&private_key);
        let other_public_key =
            Secp256r1PublicKey::from_private(&Secp256r1PrivateKey::from_seed(&[2; 32]));
        let message = [0x11; 32];

        for (epoch, version) in CONFIGS {
            let (valid, wrong_scheme) = if version.uses_secp256r1_double_hashing() {
                (
                    private_key.sign(&message),
                    private_key.sign_digest(&message),
                )
            } else {
                (
                    private_key.sign_digest(&message),
                    private_key.sign(&message),
                )
            };
            let valid = valid.expect("the test key signs");
            let wrong_scheme = wrong_scheme.expect("the test key signs");

            for (source, expected) in [
                (
                    snippet(&message, &valid.0, &public_key.to_bytes_compressed()),
                    true,
                ),
                (
                    snippet(&[0x22; 32], &valid.0, &public_key.to_bytes_compressed()),
                    false,
                ),
                (
                    snippet(&message, &valid.0, &other_public_key.to_bytes_compressed()),
                    false,
                ),
                (
                    snippet(&message, &wrong_scheme.0, &public_key.to_bytes_compressed()),
                    false,
                ),
            ] {
                crosscheck_with_epoch_and_version(
                    &source,
                    Ok(Some(Value::Bool(expected))),
                    epoch,
                    version,
                );
            }
        }
    }
}
