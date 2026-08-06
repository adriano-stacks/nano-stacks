use clarity::vm::types::{SequenceSubtype, StringSubtype, TypeSignature};
use clarity_types::ClarityName;

use super::{SimpleWord, Word};
use crate::cost::WordCharge;
use crate::wasm_generator::GeneratorError;

#[derive(Debug)]
pub struct StringToInt;

impl Word for StringToInt {
    fn name(&self) -> clarity::vm::ClarityName {
        ClarityName::from_literal("string-to-int?")
    }
}

impl SimpleWord for StringToInt {
    fn visit(
        &self,
        generator: &mut crate::wasm_generator::WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        arg_types: &[TypeSignature],
        _return_type: &TypeSignature,
    ) -> Result<(), crate::wasm_generator::GeneratorError> {
        self.charge(generator, builder, 0)?;

        let func_prefix = match &arg_types[0] {
            TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::ASCII(_))) => {
                "string"
            }
            TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(_))) => {
                "utf8"
            }
            _ => {
                return Err(GeneratorError::TypeError(
                    "impossible type for string-to-int?".to_owned(),
                ))
            }
        };

        let func = generator.func_by_name(&format!("stdlib.{func_prefix}-to-int"));
        builder.call(func);

        Ok(())
    }
}

#[derive(Debug)]
pub struct StringToUint;

impl Word for StringToUint {
    fn name(&self) -> clarity::vm::ClarityName {
        ClarityName::from_literal("string-to-uint?")
    }
}

impl SimpleWord for StringToUint {
    fn visit(
        &self,
        generator: &mut crate::wasm_generator::WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        arg_types: &[TypeSignature],
        _return_type: &TypeSignature,
    ) -> Result<(), crate::wasm_generator::GeneratorError> {
        self.charge(generator, builder, 0)?;

        let func_prefix = match arg_types[0] {
            TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::ASCII(_))) => {
                "string"
            }
            TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(_))) => {
                "utf8"
            }
            _ => {
                return Err(GeneratorError::TypeError(
                    "impossible type for string-to-int?".to_owned(),
                ))
            }
        };

        let func = generator.func_by_name(&format!("stdlib.{func_prefix}-to-uint"));

        builder.call(func);

        Ok(())
    }
}

#[derive(Debug)]
pub struct IntToAscii;

impl Word for IntToAscii {
    fn name(&self) -> clarity::vm::ClarityName {
        ClarityName::from_literal("int-to-ascii")
    }
}

impl SimpleWord for IntToAscii {
    fn visit(
        &self,
        generator: &mut crate::wasm_generator::WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        arg_types: &[TypeSignature],
        return_type: &TypeSignature,
    ) -> Result<(), crate::wasm_generator::GeneratorError> {
        self.charge(generator, builder, 0)?;

        let type_prefix = match arg_types[0] {
            TypeSignature::IntType => "int",
            TypeSignature::UIntType => "uint",
            _ => {
                return Err(GeneratorError::TypeError(
                    "invalid type for int-to-ascii".to_owned(),
                ));
            }
        };

        let (result_offset, _) =
            generator.create_call_stack_local(builder, return_type, false, true);
        builder.local_get(result_offset);

        let func = generator.func_by_name(&format!("stdlib.{type_prefix}-to-string"));

        builder.call(func);

        Ok(())
    }
}

#[derive(Debug)]
pub struct IntToUtf8;

impl Word for IntToUtf8 {
    fn name(&self) -> clarity::vm::ClarityName {
        ClarityName::from_literal("int-to-utf8")
    }
}

impl SimpleWord for IntToUtf8 {
    fn visit(
        &self,
        generator: &mut crate::wasm_generator::WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        arg_types: &[TypeSignature],
        return_type: &TypeSignature,
    ) -> Result<(), GeneratorError> {
        self.charge(generator, builder, 0)?;

        let type_prefix = match arg_types[0] {
            TypeSignature::IntType => "int",
            TypeSignature::UIntType => "uint",
            _ => {
                return Err(GeneratorError::TypeError(
                    "invalid type for int-to-utf8".to_owned(),
                ));
            }
        };

        let (result_offset, _) =
            generator.create_call_stack_local(builder, return_type, false, true);
        builder.local_get(result_offset);

        let func = generator.func_by_name(&format!("stdlib.{type_prefix}-to-utf8"));

        builder.call(func);

        Ok(())
    }
}

#[cfg(not(feature = "test-clarity-v1"))]
#[cfg(test)]
mod tests {
    #[cfg(test)]
    mod clarity_v2_v3 {
        use clarity::vm::types::{ASCIIData, CharType, SequenceData, UTF8Data};
        use clarity::vm::Value;

        use crate::tools::crosscheck;

        #[test]
        fn valid_string_to_int() {
            crosscheck(
                r#"(string-to-int? "1234567")"#,
                Ok(Some(Value::some(Value::Int(1234567)).unwrap())),
            )
        }

        #[test]
        fn valid_negative_string_to_int() {
            crosscheck(
                r#"(string-to-int? "-1234567")"#,
                Ok(Some(Value::some(Value::Int(-1234567)).unwrap())),
            )
        }

        #[test]
        fn invalid_string_to_int() {
            crosscheck(r#"(string-to-int? "0xabcd")"#, Ok(Some(Value::none())))
        }

        #[test]
        fn valid_string_to_uint() {
            crosscheck(
                r#"(string-to-uint? "98765")"#,
                Ok(Some(Value::some(Value::UInt(98765)).unwrap())),
            )
        }

        #[test]
        fn invalid_string_to_uint() {
            crosscheck(r#"(string-to-uint? "0xabcd")"#, Ok(Some(Value::none())))
        }

        #[test]
        fn valid_utf8_to_int() {
            crosscheck(
                r#"(string-to-int? u"1234567")"#,
                Ok(Some(Value::some(Value::Int(1234567)).unwrap())),
            )
        }

        #[test]
        fn valid_negative_utf8_to_int() {
            crosscheck(
                r#"(string-to-int? u"-1234567")"#,
                Ok(Some(Value::some(Value::Int(-1234567)).unwrap())),
            )
        }

        #[test]
        fn invalid_utf8_to_int() {
            crosscheck(r#"(string-to-int? u"0xabcd")"#, Ok(Some(Value::none())));
        }

        #[test]
        fn valid_utf8_to_uint() {
            crosscheck(
                r#"(string-to-uint? u"98765")"#,
                Ok(Some(Value::some(Value::UInt(98765)).unwrap())),
            )
        }

        #[test]
        fn invalid_utf8_to_uint() {
            crosscheck(r#"(string-to-uint? u"0xabcd")"#, Ok(Some(Value::none())))
        }

        #[test]
        fn uint_to_string() {
            crosscheck(
                r#"(int-to-ascii u42)"#,
                Ok(Some(Value::Sequence(SequenceData::String(
                    CharType::ASCII(ASCIIData {
                        data: "42".bytes().collect(),
                    }),
                )))),
            )
        }

        #[test]
        fn positive_int_to_string() {
            crosscheck(
                r#"(int-to-ascii 2048)"#,
                Ok(Some(Value::Sequence(SequenceData::String(
                    CharType::ASCII(ASCIIData {
                        data: "2048".bytes().collect(),
                    }),
                )))),
            )
        }

        #[test]
        fn negative_int_to_string() {
            crosscheck(
                r#"(int-to-ascii -2048)"#,
                Ok(Some(Value::Sequence(SequenceData::String(
                    CharType::ASCII(ASCIIData {
                        data: "-2048".bytes().collect(),
                    }),
                )))),
            )
        }

        #[test]
        fn uint_to_utf8() {
            crosscheck(
                r#"(int-to-utf8 u42)"#,
                Ok(Some(Value::Sequence(SequenceData::String(CharType::UTF8(
                    UTF8Data {
                        data: "42".bytes().map(|b| vec![b]).collect(),
                    },
                ))))),
            )
        }

        #[test]
        fn positive_int_to_utf8() {
            crosscheck(
                r#"(int-to-utf8 2048)"#,
                Ok(Some(Value::Sequence(SequenceData::String(CharType::UTF8(
                    UTF8Data {
                        data: "2048".bytes().map(|b| vec![b]).collect(),
                    },
                ))))),
            );
        }

        /// `string-to-int?` and `string-to-uint?` are Rust's `from_str`, which
        /// strips one optional leading sign before it looks at a digit.
        ///
        /// The compiled path recognised only `-`, and only for the signed form,
        /// so it answered `none` where the reference answers a number. Stripping
        /// the sign before the 39-digit budget matters too: with the sign the
        /// reference still accepts u128::MAX, at 40 characters.
        #[test]
        fn a_leading_plus_parses_as_the_reference_does() {
            for (snippet, expected) in [
                (r#"(string-to-uint? "+5")"#, Value::some(Value::UInt(5))),
                (r#"(string-to-uint? "+0")"#, Value::some(Value::UInt(0))),
                (
                    r#"(string-to-uint? "+340282366920938463463374607431768211455")"#,
                    Value::some(Value::UInt(u128::MAX)),
                ),
                (r#"(string-to-int? "+5")"#, Value::some(Value::Int(5))),
                (
                    r#"(string-to-int? "+170141183460469231731687303715884105727")"#,
                    Value::some(Value::Int(i128::MAX)),
                ),
                (r#"(string-to-int? u"+5")"#, Value::some(Value::Int(5))),
                (r#"(string-to-uint? u"+5")"#, Value::some(Value::UInt(5))),
            ] {
                crosscheck(snippet, Ok(Some(expected.unwrap())));
            }

            // A sign is stripped once, and what follows has to be digits.
            for snippet in [
                r#"(string-to-uint? "+")"#,
                r#"(string-to-uint? "++5")"#,
                r#"(string-to-uint? "-5")"#,
                r#"(string-to-uint? "5+")"#,
                r#"(string-to-int? "+")"#,
                r#"(string-to-int? "-+5")"#,
                r#"(string-to-int? "+-5")"#,
                r#"(string-to-int? u"-+5")"#,
                r#"(string-to-int? u"+-5")"#,
            ] {
                crosscheck(snippet, Ok(Some(Value::none())));
            }
        }

        /// Thirty-nine digits fit a u128's *width* but can still overflow it.
        ///
        /// The digit loop stops as soon as the result reaches u128::MAX/10 and
        /// the tail then admitted any last digit under 6 — so 38 nines followed
        /// by a 5 came back as u128::MAX where the reference answers `none`. The
        /// exact top of the range has to keep working, which is the second half
        /// of this table.
        #[test]
        fn thirty_nine_digits_that_overflow_are_none() {
            for snippet in [
                r#"(string-to-uint? "999999999999999999999999999999999999995")"#,
                r#"(string-to-uint? "340282366920938463463374607431768211456")"#,
                r#"(string-to-uint? "999999999999999999999999999999999999999")"#,
                r#"(string-to-uint? u"999999999999999999999999999999999999995")"#,
                r#"(string-to-int? "999999999999999999999999999999999999995")"#,
            ] {
                crosscheck(snippet, Ok(Some(Value::none())));
            }

            for (snippet, expected) in [
                (
                    r#"(string-to-uint? "340282366920938463463374607431768211455")"#,
                    Value::some(Value::UInt(u128::MAX)),
                ),
                (
                    r#"(string-to-uint? "340282366920938463463374607431768211450")"#,
                    Value::some(Value::UInt(u128::MAX - 5)),
                ),
                (
                    r#"(string-to-uint? u"340282366920938463463374607431768211455")"#,
                    Value::some(Value::UInt(u128::MAX)),
                ),
                (
                    r#"(string-to-int? "-170141183460469231731687303715884105728")"#,
                    Value::some(Value::Int(i128::MIN)),
                ),
                (
                    r#"(string-to-int? u"-170141183460469231731687303715884105728")"#,
                    Value::some(Value::Int(i128::MIN)),
                ),
            ] {
                crosscheck(snippet, Ok(Some(expected.unwrap())));
            }
        }

        #[test]
        fn negative_int_to_utf8() {
            crosscheck(
                r#"(int-to-utf8 -2048)"#,
                Ok(Some(Value::Sequence(SequenceData::String(CharType::UTF8(
                    UTF8Data {
                        data: "-2048".bytes().map(|b| vec![b]).collect(),
                    },
                ))))),
            )
        }
    }
}
