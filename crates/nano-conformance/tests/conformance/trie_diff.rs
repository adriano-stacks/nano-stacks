//! Find the key a node is not writing, by comparing its trie with the network's.
//!
//! A state root that differs while every value the node writes agrees with the
//! chain means the node is missing a write. Guessing which one is hopeless; the
//! difference can be read instead.
//!
//! The parent's root already matches, so the ancestor skip-list is identical and
//! only the block's own content hash differs — which is the hash of its root
//! node, over that node's children. A MARF merkle proof for any key carries the
//! hashes of every sibling at every level, so a single proof against the block
//! yields all 255 of the network's root children. Whichever disagrees with the
//! node's names the first byte of the path that is wrong, and the same trick
//! recurses.
//!
//! Run it with a proof fetched from a peer and a state directory to compare:
//!
//! ```text
//! NANO_TRIE_PROOF=/tmp/proof.json NANO_TRIE_STATE=~/mainnet-node/state \
//!   NANO_TRIE_PARENT=<parent-block-id> NANO_TRIE_WRITES=/tmp/writes.log \
//!   cargo test -p nano-conformance --test trie_diff -- --nocapture
//! ```

use std::{collections::BTreeMap, env, fs, path::PathBuf};

use blockstack_lib::chainstate::stacks::index::TrieMerkleProofType;
use blockstack_lib::chainstate::stacks::index::TrieMerkleProof;
use stacks_common::codec::StacksMessageCodec;
use stacks_common::types::chainstate::StacksBlockId;

use nano_marf::{MarfValue, VersionedMarf};

/// The placeholder a block is executed under before it is sealed.
fn temporary_state_id() -> [u8; 32] {
    *nano_primitives::sha512_256(&[1; 52]).as_bytes()
}

fn setting(name: &str) -> Option<String> {
    env::var(name).ok()
}

/// The network's root children, taken from the deepest `Node256` in the proof.
///
/// A proof runs from the leaf up, so the root is the last trie node in it —
/// everything after that is the ancestor shunt.
fn network_root_children(proof: &[u8]) -> Option<Vec<[u8; 32]>> {
    let proof: TrieMerkleProof<StacksBlockId> =
        TrieMerkleProof::consensus_deserialize(&mut &proof[..]).ok()?;
    proof.0.iter().rev().find_map(|entry| match entry {
        TrieMerkleProofType::Node256((_, _, hashes)) => {
            Some(hashes.iter().map(|hash| hash.0).collect())
        }
        _ => None,
    })
}

/// The writes of one block, from a `NANO_TRACE_WRITES` log.
fn traced_writes(path: &str) -> Vec<(String, [u8; 40])> {
    let Ok(trace) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut pairs = Vec::new();
    let mut started = false;
    for line in trace.lines() {
        let Some((key, value)) = line
            .strip_prefix("write ")
            .and_then(|rest| rest.split_once(" = "))
        else {
            continue;
        };
        if key.ends_with("clarity_storage::block_time") {
            if started {
                break;
            }
            started = true;
        }
        if let Ok(bytes) = hex::decode(value)
            && let Ok(value) = <[u8; 40]>::try_from(bytes.as_slice())
        {
            pairs.push((key.to_owned(), value));
        }
    }
    pairs
}

#[test]
fn the_networks_root_children_name_the_missing_write() {
    let (Some(proof), Some(state), Some(parent), Some(writes)) = (
        setting("NANO_TRIE_PROOF"),
        setting("NANO_TRIE_STATE"),
        setting("NANO_TRIE_PARENT"),
        setting("NANO_TRIE_WRITES"),
    ) else {
        nano_conformance::skip_gate("NANO_TRIE_PROOF, NANO_TRIE_STATE, NANO_TRIE_PARENT and NANO_TRIE_WRITES are needed");
        return;
    };

    let document: serde_json::Value =
        serde_json::from_slice(&fs::read(&proof).expect("read the proof")).expect("parse the proof");
    let encoded = document
        .get("proof")
        .and_then(serde_json::Value::as_str)
        .expect("the answer carries a proof");
    let bytes = hex::decode(encoded.trim_start_matches("0x")).expect("the proof is hexadecimal");
    let theirs = network_root_children(&bytes).expect("the proof reaches a Node256 root");

    let parent = <[u8; 32]>::try_from(hex::decode(&parent).expect("hexadecimal").as_slice())
        .expect("32 bytes");
    let mut marf = VersionedMarf::open(PathBuf::from(&state).join("chainstate/marf.sqlite"))
        .expect("open the MARF");
    marf.begin(Some(parent), temporary_state_id())
        .expect("begin the block");
    for (key, value) in traced_writes(&writes) {
        marf.insert(key.as_bytes(), MarfValue::from_bytes(value))
            .expect("insert");
    }
    let ours: BTreeMap<u8, [u8; 32]> = marf
        .pending_root_children()
        .expect("the root has children")
        .into_iter()
        .map(|(character, hash)| (character, *hash.as_bytes()))
        .collect();
    marf.abort().expect("leave nothing behind");

    // The proof omits the child on its own path, so it carries 255 of 256 and
    // the one it leaves out cannot be compared from this proof alone.
    println!("the network's proof carries {} root children", theirs.len());
    println!("this node's root has {} children", ours.len());
    let mut differing = Vec::new();
    for (character, hash) in &ours {
        if !theirs.contains(hash) {
            differing.push(*character);
        }
    }
    println!(
        "root children of ours the network's proof does not contain: {differing:?}"
    );

    // The proof descends through one child and omits it, so that one always
    // looks different. Naming it leaves the real divergence.
    let key = setting("NANO_TRIE_KEY").unwrap_or_default();
    if !key.is_empty() {
        let path = nano_marf::key_path(key.as_bytes());
        println!("the proof's own key starts at {:#04x}", path.as_bytes()[0]);
    }

    // Which of this block's writes land under each root child: a child that
    // differs with no write under it is where the missing key is.
    let mut under: BTreeMap<u8, Vec<String>> = BTreeMap::new();
    for (key, _) in traced_writes(&writes) {
        let first = nano_marf::key_path(key.as_bytes()).as_bytes()[0];
        under.entry(first).or_default().push(key);
    }
    for character in &differing {
        match under.get(character) {
            Some(keys) => println!("{character:#04x}: written by {keys:?}"),
            None => println!("{character:#04x}: this block writes nothing under it"),
        }
    }
}
