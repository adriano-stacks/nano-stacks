#![no_main]

use arbitrary::{Arbitrary as _, Unstructured};
use libfuzzer_sys::fuzz_target;
use nano_adversarial::transaction_and_block_codecs;
use nano_adversarial_fuzz::{MutationCase, mutate};

const SEEDS: &[&str] = &[
    include_str!("../../crates/nano-adversarial/corpus/codecs/transaction.hex"),
    include_str!("../../crates/nano-adversarial/corpus/codecs/nakamoto-block.hex"),
];

fuzz_target!(|input: &[u8]| {
    let mut input = Unstructured::new(input);
    if let Ok(case) = MutationCase::arbitrary(&mut input) {
        let _ = transaction_and_block_codecs(&mutate(case, SEEDS));
    }
});
