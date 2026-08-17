#![no_main]

use arbitrary::{Arbitrary as _, Unstructured};
use libfuzzer_sys::fuzz_target;
use nano_adversarial::checkpoint_import;
use nano_adversarial_fuzz::{CheckpointImportCase, checkpoint_import_bytes};

fuzz_target!(|input: &[u8]| {
    let mut input = Unstructured::new(input);
    if let Ok(case) = CheckpointImportCase::arbitrary(&mut input) {
        let _ = checkpoint_import(&checkpoint_import_bytes(case));
    }
});
