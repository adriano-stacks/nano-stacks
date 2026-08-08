//! Show what a captured block changed in stacks-core's state.
//!
//! Comparing a block's committed leaves against its parent's isolates the
//! writes that block performed, which is how a replay divergence is located.
//! MARF keys are hashed, so a reported key is identified by hashing candidate
//! Clarity key strings and matching.

use std::{collections::BTreeSet, env, fs, path::Path};

use nano_chainstate::{ChainState, NakamotoBlock};
use nano_conformance::captured_network;

fn main() {
    let target: u64 = env::args()
        .nth(1)
        .expect("usage: captured-leaves <stacks height>")
        .parse()
        .expect("stacks height");
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let checkpoint = fixtures.join("chainstate/checkpoint-H/marf.sqlite");
    let mut paths = fs::read_dir(fixtures.join("nakamoto/blocks"))
        .expect("read captured blocks")
        .map(|entry| entry.expect("captured block entry").path())
        .collect::<Vec<_>>();
    paths.sort();
    let blocks = paths
        .iter()
        .map(|path| {
            NakamotoBlock::decode(&fs::read(path).expect("read block")).expect("decode block")
        })
        .collect::<Vec<_>>();
    let leaves = |block: &NakamotoBlock| {
        let block_id = *block.block_id().as_bytes();
        ChainState::from_checkpoint(
            captured_network(&fixtures),
            &checkpoint,
            block_id,
            block.header.state_index_root,
        )
        .expect("open the captured block state")
        .state_leaves(block_id)
        .expect("captured leaves")
        .into_iter()
        .map(|(key, value)| (hex::encode(key.as_bytes()), hex::encode(value.as_bytes())))
        .collect::<BTreeSet<_>>()
    };
    let index = blocks
        .iter()
        .position(|block| block.header.chain_length == target)
        .expect("captured target block");
    let parent = leaves(&blocks[index - 1]);
    let child = leaves(&blocks[index]);
    println!("added or changed by block {target}:");
    for entry in child.difference(&parent) {
        println!("  {} = {}", entry.0, entry.1);
    }
}
