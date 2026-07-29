use clar2wasm::tools::crosscheck;
use clarity::vm::types::OptionalData;
use clarity::vm::Value;
use proptest::proptest;

use crate::PropValue;

proptest! {
    #![proptest_config(super::runtime_config())]

    #[test]
    fn is_eq_one_argument_always_true(val in PropValue::any()) {
        crosscheck(
            &format!(r#"(is-eq {val})"#),
            Ok(Some(clarity::vm::Value::Bool(true)))
        );
    }
}

/// Principals of more than one contract unify into a union of callable
/// subtypes rather than a single callable, which the compiler once refused to
/// size, so `is-eq` failed to compile while the interpreter answered true.
#[test]
fn is_eq_over_a_list_of_contract_principals() {
    crosscheck(
        "(is-eq (list 'SH205N8RY76BDEA8Q0VP13GNS70M3CSA42KPRX0MB.A 'S720QWDM2GQYP70TPDH62VHCCTWZ8Q4RC6YFH8BW3.A))",
        Ok(Some(clarity::vm::Value::Bool(true))),
    );
}

proptest! {
    #![proptest_config(super::runtime_config())]

    #[test]
    fn is_eq_value_with_itself_always_true(val in PropValue::any()) {
        crosscheck(
            &format!(r#"(is-eq {val} {val})"#),
            Ok(Some(clarity::vm::Value::Bool(true)))
        );
    }
}

proptest! {
    #![proptest_config(super::runtime_config())]

    #[test]
    fn is_eq_value_with_itself_always_true_3(val in PropValue::any()) {
        crosscheck(
            &format!(r#"(is-eq {val} {val} {val})"#),
            Ok(Some(clarity::vm::Value::Bool(true)))
        );
    }
}

proptest! {
    #![proptest_config(super::runtime_config())]

    #[test]
    fn crosscheck_index_of(
        seq in PropValue::any_sequence(10usize),
        idx in (0usize..10)
    ) {
        let Value::Sequence(seq_data) = seq.clone().into() else { unreachable!() };
        let (item, first) = match seq_data.clone().element_at(idx).unwrap() {
            Some(item) =>
                match seq_data.contains(item.clone()).unwrap() {
                    Some(v) => (item, Value::UInt(v.try_into().unwrap())),
                    None => (item, Value::none())
                }
            None => (Value::none(), Value::none()),
        };

        let snippet = format!("(index-of {} {})", seq, PropValue(item));

        crosscheck(
            &snippet,
            Ok(Some(
                Value::Optional(
                    OptionalData {data: Some(Box::new(first))}
                )
            ))
        )
    }
}
