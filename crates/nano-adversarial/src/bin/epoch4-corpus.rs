use std::{collections::BTreeMap, fs, path::Path};

use nano_adversarial::{clarity_refusal_report, clarity_stateful_receipt_report};
use serde_json::{Value, json};

fn main() {
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus");
    let report = json!({
        "epoch": "4.0",
        "refusals": observations(&corpus.join("clarity-refusal"), clarity_refusal_report),
        "schema": 1,
        "stateful_receipts": observations(
            &corpus.join("clarity-stateful"),
            clarity_stateful_receipt_report,
        ),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&canonical(report)).expect("serialize architecture report")
    );
}

fn observations(directory: &Path, observe: fn(&[u8]) -> Option<Value>) -> Vec<Value> {
    let mut paths = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
        .map(|entry| entry.expect("corpus directory entry").path())
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let encoded = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            let input = hex::decode(encoded.split_whitespace().collect::<String>())
                .unwrap_or_else(|error| panic!("decode {}: {error}", path.display()));
            canonical(observe(&input).unwrap_or_else(|| panic!("observe {}", path.display())))
        })
        .collect()
}

fn canonical(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonical).collect()),
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, canonical(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        value => value,
    }
}
