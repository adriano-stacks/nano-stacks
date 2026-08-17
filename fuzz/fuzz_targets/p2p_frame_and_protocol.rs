#![no_main]

use arbitrary::{Arbitrary as _, Unstructured};
use libfuzzer_sys::fuzz_target;
use nano_adversarial::p2p_frame_and_protocol;
use nano_adversarial_fuzz::{P2pCase, p2p_bytes};

fuzz_target!(|input: &[u8]| {
    let mut input = Unstructured::new(input);
    if let Ok(case) = P2pCase::arbitrary(&mut input) {
        let _ = p2p_frame_and_protocol(&p2p_bytes(case));
    }
});
