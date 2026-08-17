#![no_main]

use arbitrary::{Arbitrary as _, Unstructured};
use libfuzzer_sys::fuzz_target;
use nano_adversarial::signer_and_stackerdb_codecs;
use nano_adversarial_fuzz::{MutationCase, mutate};

const SEEDS: &[&str] = &[
    include_str!("../../crates/nano-adversarial/corpus/signer-stackerdb/signer-state-update.hex"),
    include_str!("../../crates/nano-adversarial/corpus/signer-stackerdb/chunk.hex"),
];

fuzz_target!(|input: &[u8]| {
    let mut input = Unstructured::new(input);
    if let Ok(case) = MutationCase::arbitrary(&mut input) {
        let _ = signer_and_stackerdb_codecs(&mutate(case, SEEDS));
    }
});
