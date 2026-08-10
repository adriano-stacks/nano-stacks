//! The same work through nano and stacks-core, timed.
//!
//! Correctness against stacks-core is what the conformance tests prove;
//! this measures cost on identical inputs, in one process, on one machine —
//! which is the only comparison that means anything. Dev-only like every
//! oracle in this crate: stacks-core stays out of the release graph.
//!
//! Two surfaces to begin with, the ones whose inputs already exist:
//!
//! - **codec**: decode and re-encode the captured mainnet Nakamoto blocks.
//! - **marf**: the lockstep workload — batch insert, seal, commit — over a
//!   fresh store per sample, so a sample measures sealing and not the page
//!   cache the previous sample warmed.

use std::{fs, hint::black_box, path::Path};

use criterion::{Criterion, criterion_group, criterion_main};

use nano_chainstate::NakamotoBlock;
use nano_marf::{MarfValue, VersionedMarf};

use blockstack_lib::chainstate::nakamoto::NakamotoBlock as CoreBlock;
use blockstack_lib::chainstate::stacks::index::ClarityMarfTrieId;
use blockstack_lib::chainstate::stacks::index::MARFValue as CoreMarfValue;
use blockstack_lib::chainstate::stacks::index::marf::{MARF, MARFOpenOpts};
use stacks_common::codec::StacksMessageCodec;
use stacks_common::types::chainstate::StacksBlockId;

/// Every captured mainnet block, as the consensus bytes both codecs read.
fn fixture_blocks() -> Vec<Vec<u8>> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/nakamoto/blocks");
    let mut paths: Vec<_> = fs::read_dir(&directory)
        .expect("the fixture blocks are checked in")
        .map(|entry| entry.expect("a directory entry").path())
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|path| fs::read(path).expect("read a fixture block"))
        .collect()
}

fn codec_decode(c: &mut Criterion) {
    let blocks = fixture_blocks();
    assert!(!blocks.is_empty(), "the fixture blocks are checked in");
    let mut group = c.benchmark_group("codec-decode");
    group.throughput(criterion::Throughput::Elements(blocks.len() as u64));
    group.bench_function("nano", |bencher| {
        bencher.iter(|| {
            for bytes in &blocks {
                black_box(NakamotoBlock::decode(bytes).expect("nano decodes the capture"));
            }
        });
    });
    group.bench_function("stacks-core", |bencher| {
        bencher.iter(|| {
            for bytes in &blocks {
                let mut cursor = std::io::Cursor::new(bytes.as_slice());
                black_box(
                    CoreBlock::consensus_deserialize(&mut cursor)
                        .expect("stacks-core decodes the capture"),
                );
            }
        });
    });
    group.finish();
}

fn codec_encode(c: &mut Criterion) {
    let blocks = fixture_blocks();
    let nano: Vec<NakamotoBlock> = blocks
        .iter()
        .map(|bytes| NakamotoBlock::decode(bytes).expect("nano decodes the capture"))
        .collect();
    let core: Vec<CoreBlock> = blocks
        .iter()
        .map(|bytes| {
            let mut cursor = std::io::Cursor::new(bytes.as_slice());
            CoreBlock::consensus_deserialize(&mut cursor).expect("stacks-core decodes the capture")
        })
        .collect();
    let mut group = c.benchmark_group("codec-encode");
    group.throughput(criterion::Throughput::Elements(blocks.len() as u64));
    group.bench_function("nano", |bencher| {
        bencher.iter(|| {
            for block in &nano {
                black_box(block.encode());
            }
        });
    });
    group.bench_function("stacks-core", |bencher| {
        bencher.iter(|| {
            for block in &core {
                black_box(block.serialize_to_vec());
            }
        });
    });
    group.finish();
}

/// A deterministic pseudo-random source, as the lockstep tests use: a benchmark
/// that varies its workload run to run compares nothing.
struct Rng(u64);

impl Rng {
    const fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

const fn block_id(n: u8) -> [u8; 32] {
    let mut bytes = [0; 32];
    bytes[0] = n;
    bytes
}

/// One block of the sealing workload: its identifier and what it writes.
type WorkloadBlock = ([u8; 32], Vec<(String, [u8; 40])>);

/// The lockstep workload: a chain of blocks whose sizes cross every trie node
/// promotion, each rewriting some of its ancestors' keys.
fn marf_workload() -> Vec<WorkloadBlock> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut all_keys: Vec<String> = Vec::new();
    [1usize, 3, 5, 17, 49, 260, 64, 64, 64, 64]
        .into_iter()
        .enumerate()
        .map(|(index, count)| {
            let mut pairs: Vec<(String, [u8; 40])> = (0..count)
                .map(|_| {
                    let key = format!("vm::SP000000000000000000002Q6VF78.c::19::{:x}", rng.next());
                    let mut value = [0u8; 40];
                    value[..8].copy_from_slice(&rng.next().to_be_bytes());
                    (key, value)
                })
                .collect();
            // Rewrite an ancestor's keys as well: copy-on-write is what a real
            // block mostly does, and it is the expensive path.
            pairs.extend(all_keys.iter().step_by(7).map(|key| {
                let mut value = [0u8; 40];
                value[..8].copy_from_slice(&rng.next().to_be_bytes());
                (key.clone(), value)
            }));
            all_keys.extend(pairs.iter().map(|(key, _)| key.clone()));
            (
                block_id(u8::try_from(index + 1).expect("a small index")),
                pairs,
            )
        })
        .collect()
}

fn marf_seal(c: &mut Criterion) {
    let workload = marf_workload();
    let blocks = workload.len() as u64;
    let mut group = c.benchmark_group("marf-seal");
    group.throughput(criterion::Throughput::Elements(blocks));
    group.sample_size(20);

    group.bench_function("nano", |bencher| {
        bencher.iter_batched(
            || {
                let directory = tempfile::tempdir().expect("a directory");
                let marf =
                    VersionedMarf::open(directory.path().join("marf.sqlite")).expect("opens");
                (directory, marf)
            },
            |(directory, mut marf)| {
                let mut parent: Option<[u8; 32]> = None;
                for (block, pairs) in &workload {
                    marf.begin(parent, *block).expect("nano begins");
                    for (key, value) in pairs {
                        marf.insert(key.as_bytes(), MarfValue::from_bytes(*value))
                            .expect("nano inserts");
                    }
                    black_box(marf.seal().expect("nano seals"));
                    parent = Some(*block);
                }
                drop(directory);
            },
            criterion::BatchSize::PerIteration,
        );
    });

    group.bench_function("stacks-core", |bencher| {
        bencher.iter_batched(
            || {
                let directory = tempfile::tempdir().expect("a directory");
                let marf = MARF::from_path(
                    directory
                        .path()
                        .join("marf.sqlite")
                        .to_str()
                        .expect("a path"),
                    MARFOpenOpts::default(),
                )
                .expect("open the stacks-core MARF");
                (directory, marf)
            },
            |(directory, mut marf)| {
                let mut parent = StacksBlockId::sentinel();
                for (block, pairs) in &workload {
                    let keys: Vec<String> = pairs.iter().map(|(key, _)| key.clone()).collect();
                    let values: Vec<CoreMarfValue> = pairs
                        .iter()
                        .map(|(_, value)| CoreMarfValue(*value))
                        .collect();
                    let mut transaction = marf.begin_tx().expect("stacks-core begins");
                    transaction
                        .begin(&parent, &StacksBlockId(*block))
                        .expect("stacks-core begins the block");
                    transaction
                        .insert_batch(&keys, values)
                        .expect("stacks-core inserts");
                    black_box(transaction.seal().expect("stacks-core seals"));
                    transaction.commit().expect("stacks-core commits");
                    parent = StacksBlockId(*block);
                }
                drop(directory);
            },
            criterion::BatchSize::PerIteration,
        );
    });

    group.finish();
}

criterion_group!(benches, codec_decode, codec_encode, marf_seal);
criterion_main!(benches);
