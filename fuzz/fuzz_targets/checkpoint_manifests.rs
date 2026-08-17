#![no_main]

use arbitrary::{Arbitrary as _, Unstructured};
use libfuzzer_sys::fuzz_target;
use nano_adversarial::checkpoint_manifests;
use nano_adversarial_fuzz::{MutationCase, mutate_bytes};

const SEEDS: &[&[u8]] = &[include_bytes!(
    "../../crates/nano-adversarial/corpus/checkpoint/published-sample.case"
)];

fuzz_target!(|input: &[u8]| {
    let mut input = Unstructured::new(input);
    if let Ok(case) = MutationCase::arbitrary(&mut input) {
        let _ = checkpoint_manifests(&mutate_bytes(case, SEEDS));
    }
});
