#![no_main]

use arbitrary::{Arbitrary as _, Unstructured};
use libfuzzer_sys::fuzz_target;
use nano_adversarial::clarity_wasm_abi;
use nano_adversarial_fuzz::{ClarityCase, clarity_source};

fuzz_target!(|input: &[u8]| {
    let mut input = Unstructured::new(input);
    if let Ok(case) = ClarityCase::arbitrary(&mut input) {
        let _ = clarity_wasm_abi(clarity_source(case).as_bytes());
    }
});
