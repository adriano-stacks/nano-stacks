#![no_main]

use arbitrary::{Arbitrary as _, Unstructured};
use libfuzzer_sys::fuzz_target;
use nano_adversarial::clarity_stateful_receipt_differential;
use nano_adversarial_fuzz::{ClarityStatefulCase, clarity_stateful_bytes};

fuzz_target!(|input: &[u8]| {
    let mut input = Unstructured::new(input);
    if let Ok(case) = ClarityStatefulCase::arbitrary(&mut input) {
        let _ = clarity_stateful_receipt_differential(&clarity_stateful_bytes(case));
    }
});
