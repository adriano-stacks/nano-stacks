#![no_main]

use arbitrary::{Arbitrary as _, Unstructured};
use libfuzzer_sys::fuzz_target;
use nano_adversarial::marf_operations;
use nano_adversarial_fuzz::{MarfCase, marf_bytes};

fuzz_target!(|input: &[u8]| {
    let mut input = Unstructured::new(input);
    if let Ok(case) = MarfCase::arbitrary(&mut input) {
        let _ = marf_operations(&marf_bytes(case));
    }
});
