//! What a block claims about itself, checked before any of it runs.
//!
//! A state root only says a block computes what its header commits to. It says
//! nothing about whether the block belongs to this chain: a transaction with
//! another network's version byte or chain identifier is not a transaction here,
//! and one anchored off-chain names microblocks, which 4.0 does not have.
//!
//! A root would catch none of them, because a node that executes them computes a
//! perfectly self-consistent state for a chain nobody else is on. So each is
//! rejected before execution begins, and each gets its own test — a validator
//! nothing exercises is a validator that quietly stops validating.

use std::{fs, path::Path};

use nano_chainstate::{ChainState, NakamotoBlock, TenureAccounting};
use nano_codec::Transaction;
use nano_conformance::{FixtureManifest, FixtureMode, replay_into};

fn fixtures() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// Open a chainstate over the captured checkpoint and return one real block.
fn checkpoint_and_block() -> (ChainState, NakamotoBlock) {
    let fixtures = fixtures();
    let checkpoint = fixtures.join("chainstate/checkpoint-H");
    let manifest = fs::read_to_string(checkpoint.join("checkpoint.toml"))
        .expect("read the checkpoint manifest");
    let field = |name: &str| -> String {
        manifest
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name} = "))?.strip_prefix('"'))
            .and_then(|value| value.strip_suffix('"'))
            .unwrap_or_else(|| panic!("the checkpoint names {name}"))
            .to_owned()
    };
    let decode = |value: &str| -> [u8; 32] {
        <[u8; 32]>::try_from(hex::decode(value).expect("hexadecimal").as_slice())
            .expect("32 bytes")
    };

    let directory = Box::leak(Box::new(
        tempfile::tempdir().expect("a directory"),
    ));
    let source = decode(&field("source_state_id"));
    let mut chainstate = ChainState::open_from_checkpoint(
        nano_primitives::Network::TESTNET,
        directory.path(),
        checkpoint.join("marf.sqlite"),
        source,
        nano_primitives::TrieHash::from_bytes(decode(&field("published_state_index_root"))),
    )
    .expect("open the checkpoint");
    if let Some(accounting) = fs::read(checkpoint.join("native-effects.json"))
        .ok()
        .and_then(|contents| TenureAccounting::from_json(&contents).ok())
    {
        *chainstate.accounting_mut() = accounting;
    }

    let mut captured = None;
    replay_into(
        &mut chainstate,
        source,
        &fixtures,
        FixtureManifest {
            mode: FixtureMode::Captured,
            replay_blocks: 1,
            receipts: true,
        },
        0,
        &mut |block, _| captured = Some(block.clone()),
    );
    let block = captured.expect("the capture holds a block");
    (chainstate, block)
}

#[test]
fn a_real_block_authenticates() {
    let (chainstate, block) = checkpoint_and_block();
    chainstate
        .authenticate_block(&block)
        .expect("a block the network accepted authenticates");
}

#[test]
fn a_header_version_from_another_epoch_is_rejected() {
    let (chainstate, mut block) = checkpoint_and_block();
    // The shadow flag is the top bit; the epoch's version is what is below it.
    block.header.version = 0;
    let rejected = chainstate
        .authenticate_block(&block)
        .expect_err("a version that is not epoch 4.0's is rejected");
    assert!(
        rejected.to_string().contains("version"),
        "the rejection names the version: {rejected}"
    );
}

#[test]
fn the_shadow_flag_does_not_change_the_version() {
    let (chainstate, mut block) = checkpoint_and_block();
    block.header.version |= 0x80;
    chainstate
        .authenticate_block(&block)
        .expect("the shadow flag sits above the version and does not change it");
}

/// Re-decode a block's first transaction with one byte changed.
///
/// A transaction holds the bytes it was decoded from, so the only honest way to
/// give it another version or chain is to change those bytes and decode again —
/// which is also exactly what arriving from a peer looks like.
fn with_mutated_transaction(block: &NakamotoBlock, at: usize, byte: u8) -> NakamotoBlock {
    let mut block = block.clone();
    let mut bytes = block.transactions[0].encode();
    bytes[at] = byte;
    let (transaction, _) = Transaction::decode(&bytes).expect("the mutated transaction decodes");
    block.transactions[0] = transaction;
    block
}

#[test]
fn a_transaction_from_another_network_is_rejected() {
    let (chainstate, block) = checkpoint_and_block();
    // Byte zero is the transaction version: 0x00 mainnet, 0x80 testnet.
    let block = with_mutated_transaction(&block, 0, 0x00);
    let rejected = chainstate
        .authenticate_block(&block)
        .expect_err("a mainnet transaction is rejected on a testnet chain");
    assert!(
        rejected.to_string().contains("another network"),
        "the rejection says which: {rejected}"
    );
}

#[test]
fn a_transaction_naming_another_chain_is_rejected() {
    let (chainstate, block) = checkpoint_and_block();
    // Bytes one to four are the chain identifier, big-endian.
    let block = with_mutated_transaction(&block, 4, 0xff);
    let rejected = chainstate
        .authenticate_block(&block)
        .expect_err("a transaction for another chain is rejected");
    assert!(
        rejected.to_string().contains("names chain"),
        "the rejection says which: {rejected}"
    );
}

