#![no_main]

use arbitrary::{Arbitrary as _, Unstructured};
use libfuzzer_sys::fuzz_target;
use nano_adversarial::clarity_refusal_differential;
use nano_adversarial_fuzz::{ClarityRefusalCase, clarity_refusal_bytes};

fuzz_target!(|input: &[u8]| {
    let mut input = Unstructured::new(input);
    if let Ok(case) = ClarityRefusalCase::arbitrary(&mut input) {
        let _ = clarity_refusal_differential(&clarity_refusal_bytes(case));
    }
});
