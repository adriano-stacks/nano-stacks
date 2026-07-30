//! Check nano's block envelope against blocks mainnet actually accepted.
//!
//! The fixtures are five consecutive blocks from `/v3/blocks/:id` and the
//! reward set `/v3/stacker_set/140` published for the cycle they fall in, both
//! captured from `api.mainnet.hiro.so` running stacks-node 4.0.1 at burn height
//! 960,300 — after the epoch 4.0 boundary at 960,230.
//!
//! Neither needs a chainstate, which is what makes this possible where a full
//! replay of mainnet is not: the reward set is published, and the envelope is
//! self-contained. It proves nano derives the same signer signature hash the
//! network signed, recovers the same keys from it, orders them the same way,
//! and counts the same weight against the same threshold. It proves nothing
//! about execution.

use std::fs;
use std::path::{Path, PathBuf};

use nano_chainstate::{NakamotoBlock, Signer, SignerSet};
use nano_crypto::StacksPublicKey;

fn mainnet() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("mainnet")
}

fn reward_set() -> SignerSet {
    let bytes = fs::read(mainnet().join("stacker_set-140.json")).expect("read the reward set");
    let document: serde_json::Value = serde_json::from_slice(&bytes).expect("parse the reward set");
    let signers = document["stacker_set"]["signers"]
        .as_array()
        .expect("the reward set has signers")
        .iter()
        .map(|entry| {
            let key = entry["signing_key"]
                .as_str()
                .expect("a signer has a signing key");
            let bytes = hex::decode(key.trim_start_matches("0x")).expect("the key is hexadecimal");
            Signer {
                public_key: StacksPublicKey::from_bytes(&bytes).expect("the key is a public key"),
                weight: u32::try_from(entry["weight"].as_u64().expect("a signer has a weight"))
                    .expect("the weight fits"),
            }
        })
        .collect();
    SignerSet::new(signers).expect("the reward set is not empty")
}

fn blocks() -> Vec<(String, NakamotoBlock)> {
    let mut blocks = fs::read_dir(mainnet().join("blocks"))
        .expect("read the block directory")
        .map(|entry| {
            let path = entry.expect("read a block entry").path();
            let name = path
                .file_stem()
                .expect("a block file has a name")
                .to_string_lossy()
                .into_owned();
            let block = NakamotoBlock::decode(&fs::read(&path).expect("read a block"))
                .expect("decode a block");
            (name, block)
        })
        .collect::<Vec<_>>();
    blocks.sort_by_key(|(_, block)| block.header.chain_length);
    blocks
}

#[test]
fn mainnet_blocks_carry_the_weight_the_network_required() {
    let set = reward_set();
    let total: u32 = set.signers().iter().map(|signer| signer.weight).sum();
    let blocks = blocks();
    assert_eq!(blocks.len(), 5, "five blocks were captured");

    for (name, block) in &blocks {
        let header = &block.header;
        // `block_hash` is the signer signature hash: signatures are not
        // committed to, so the two are the same preimage.
        assert_eq!(
            header.block_hash().as_bytes(),
            header.signer_signature_hash().as_bytes(),
            "block {name}"
        );
        let weight = set
            .verify(header)
            .unwrap_or_else(|error| panic!("block {name} was accepted by mainnet: {error}"));
        assert!(
            u64::from(weight) * 10 >= u64::from(total) * 7,
            "block {name} carries seven tenths of the weight"
        );
    }
}

#[test]
fn a_block_id_is_the_hash_the_network_serves_it_under() {
    for (name, block) in blocks() {
        assert_eq!(
            hex::encode(block.block_id().as_bytes()),
            name,
            "the block answers to the identifier it was fetched by"
        );
    }
}
