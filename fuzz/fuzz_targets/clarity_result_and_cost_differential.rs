#![no_main]

use arbitrary::{Arbitrary as _, Unstructured};
use libfuzzer_sys::fuzz_target;
use nano_adversarial::clarity_result_and_cost_differential;
use nano_adversarial_fuzz::{ClarityDifferentialCase, clarity_differential_bytes};

fuzz_target!(|input: &[u8]| {
    let mut input = Unstructured::new(input);
    if let Ok(case) = ClarityDifferentialCase::arbitrary(&mut input) {
        let _ = clarity_result_and_cost_differential(&clarity_differential_bytes(case));
    }
});
