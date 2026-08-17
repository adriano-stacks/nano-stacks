#![no_main]

use arbitrary::{Arbitrary as _, Unstructured};
use libfuzzer_sys::fuzz_target;
use nano_adversarial::p2p_session_state;
use nano_adversarial_fuzz::{SessionCase, session_bytes};

fuzz_target!(|input: &[u8]| {
    let mut input = Unstructured::new(input);
    if let Ok(case) = SessionCase::arbitrary(&mut input) {
        let _ = p2p_session_state(&session_bytes(case));
    }
});
