use std::{
    fs,
    path::{Path, PathBuf},
};

use nano_adversarial::{
    checkpoint_import, checkpoint_manifests, clarity_refusal_differential,
    clarity_result_and_cost_differential, clarity_stateful_receipt_differential, clarity_wasm_abi,
    marf_operations, p2p_frame_and_protocol, p2p_session_state, signer_and_stackerdb_codecs,
    transaction_and_block_codecs, transaction_and_block_differential,
};

const MAX_SEEDS_PER_TARGET: usize = 32;
const MAX_CORPUS_BYTES_PER_TARGET: u64 = 4 * 1024 * 1024;

struct Target {
    name: &'static str,
    run: fn(&[u8]) -> u8,
    required_coverage: u8,
}

#[test]
fn owned_corpora_stay_bounded_and_replay() {
    for target in [
        Target {
            name: "p2p",
            run: p2p_frame_and_protocol,
            required_coverage: 3,
        },
        Target {
            name: "p2p-session",
            run: p2p_session_state,
            required_coverage: 31,
        },
        Target {
            name: "codecs",
            run: transaction_and_block_codecs,
            required_coverage: 3,
        },
        Target {
            name: "codecs",
            run: transaction_and_block_differential,
            required_coverage: 3,
        },
        Target {
            name: "signer-stackerdb",
            run: signer_and_stackerdb_codecs,
            required_coverage: 3,
        },
        Target {
            name: "checkpoint",
            run: checkpoint_manifests,
            required_coverage: 1,
        },
        Target {
            name: "checkpoint-import",
            run: checkpoint_import,
            required_coverage: 7,
        },
        Target {
            name: "marf",
            run: marf_operations,
            required_coverage: 3,
        },
        Target {
            name: "clarity-wasm",
            run: clarity_wasm_abi,
            required_coverage: 1,
        },
        Target {
            name: "clarity-differential",
            run: clarity_result_and_cost_differential,
            required_coverage: 63,
        },
        Target {
            name: "clarity-refusal",
            run: clarity_refusal_differential,
            required_coverage: 63,
        },
        Target {
            name: "clarity-stateful",
            run: clarity_stateful_receipt_differential,
            required_coverage: 63,
        },
    ] {
        replay(&target);
    }
}

fn replay(target: &Target) {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("corpus")
        .join(target.name);
    let mut seeds: Vec<PathBuf> = fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
        .map(|entry| entry.expect("corpus directory entry").path())
        .filter(|path| path.is_file())
        .collect();
    seeds.sort();
    assert!(!seeds.is_empty(), "{} has no owned corpus", target.name);
    assert!(
        seeds.len() <= MAX_SEEDS_PER_TARGET,
        "{} exceeds its seed budget",
        target.name
    );

    let total_bytes: u64 = seeds
        .iter()
        .map(|path| fs::metadata(path).expect("seed metadata").len())
        .sum();
    assert!(
        total_bytes <= MAX_CORPUS_BYTES_PER_TARGET,
        "{} exceeds its byte budget",
        target.name
    );

    let mut coverage = 0;
    for seed in seeds {
        let bytes = fs::read(&seed).expect("read corpus seed");
        let input = if seed.extension().is_some_and(|extension| extension == "hex") {
            hex::decode(
                String::from_utf8(bytes)
                    .expect("hex seed is UTF-8")
                    .split_whitespace()
                    .collect::<String>(),
            )
            .expect("decode hex seed")
        } else {
            bytes
        };
        coverage |= (target.run)(&input);
    }
    assert_eq!(
        coverage & target.required_coverage,
        target.required_coverage,
        "{} corpus does not reach every required path",
        target.name
    );
}
