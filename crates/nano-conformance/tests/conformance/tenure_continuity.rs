//! A tenure change is checked against the chain this node executed.
//!
//! Two of its fields are claims about history rather than about the block: which
//! tenure it confirms, and how many blocks that tenure ran for. stacks-core
//! validates both against a tenure index (`check_nakamoto_tenure`, and
//! `get_nakamoto_tenure_length` for the count, which is the parent's own
//! `height_in_tenure`). nano answers them from the list of blocks it has
//! executed, which is the same question asked of the only history it has.
//!
//! Both are skipped rather than guessed when that list cannot answer — a parent
//! below the checkpoint, or a tenure that began before the retained window — so
//! the risk here is not a wrong rejection but a check that quietly never runs.
//! Hence the shape of these tests: a control that must be accepted, and two
//! mutations that must not be, on a chain deep enough for the list to answer.
//!
//! The blocks are *assembled* rather than edited. A transaction carries the bytes
//! it was decoded from, so changing a tenure change means signing a new one, and
//! a tenure change has to name the miner that signed the header it travels in —
//! so the honest way to produce one is to mine it, which is also the only way a
//! real competing miner could.

use std::{fs, path::Path, path::PathBuf};

use nano_bitcoin::BitcoinOperation;
use nano_chainstate::{BitcoinBlockContext, ChainState, NakamotoBlock};
use nano_codec::{
    AnchorMode, TenureChangePayload, Transaction, TransactionPayloadData, TransactionVersion,
};
use nano_conformance::{FixtureManifest, FixtureMode, replay_into};
use nano_crypto::StacksPrivateKey;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// A chain with a whole tenure behind it, and the next tenure to start on it.
struct Standing {
    chainstate: ChainState,
    parent: [u8; 32],
    block: NakamotoBlock,
    context: BitcoinBlockContext,
    operations: Vec<BitcoinOperation>,
}

/// Replay up to the *second* tenure-start block the capture holds.
///
/// The second rather than the first on purpose: the count rule can only be
/// checked when the tenure being ended began inside the executed list, and the
/// first tenure above a checkpoint began below it.
fn standing_on_a_whole_tenure() -> Option<Standing> {
    let fixtures = fixtures();
    let blocks = nano_conformance::captured_block_paths(&fixtures)
        .into_iter()
        .map(|path| NakamotoBlock::decode(&fs::read(&path).expect("read a block")))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let target = blocks
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(_, block)| nano_chainstate::starts_new_tenure(block))
        .nth(1)
        .map(|(index, _)| index)?;
    let block = blocks.get(target)?.clone();
    let view = block.header.consensus_hash.to_string();
    let context = *nano_conformance::captured_bitcoin_snapshots(&fixtures)?.get(&view)?;
    let operations = nano_conformance::captured_bitcoin_operations(&fixtures)?
        .get(&view)
        .cloned()
        .unwrap_or_default();

    let (mut chainstate, source) = nano_conformance::replay_chainstate(&fixtures).ok()?;
    let depth = replay_into(
        &mut chainstate,
        source,
        &fixtures,
        FixtureManifest {
            mode: FixtureMode::Captured,
            replay_blocks: u64::try_from(target).ok()?,
            // The captured `new_block` events, which carry the PoX unlock
            // heights a testnet capture does not put in its provenance.
            receipts: true,
        },
        0,
        &mut |_, _| {},
    );
    assert_eq!(
        usize::try_from(depth.completed).expect("the depth fits"),
        target,
        "the blocks before the tenure must all execute: {:?}",
        depth.first_divergence
    );
    Some(Standing {
        chainstate,
        parent: *blocks.get(target - 1)?.block_id().as_bytes(),
        block,
        context,
        operations,
    })
}

/// The tenure change a block carries.
fn tenure_change(block: &NakamotoBlock) -> TenureChangePayload {
    block
        .transactions
        .iter()
        .find_map(|transaction| match transaction.payload().data() {
            TransactionPayloadData::TenureChange(payload) => Some(payload.clone()),
            _ => None,
        })
        .expect("a tenure-start block carries a tenure change")
}

/// Mine the captured tenure again, with a tenure change of our own making.
///
/// The miner key is the test's, so the tenure change names it and the header is
/// signed by it: a block that could exist. Only the fields under test differ from
/// what the network's own tenure change said.
fn mine_tenure(standing: &mut Standing, payload: &TenureChangePayload) -> Result<(), String> {
    let miner = StacksPrivateKey::from_seed(b"a tenure of our own");
    let mut candidate = standing.block.clone();
    candidate.header.signer_signatures.clear();
    candidate.transactions[0] = Transaction::sign_standard(
        TransactionVersion::Testnet,
        candidate.transactions[0].chain_id(),
        AnchorMode::OnChainOnly,
        &miner,
        0,
        0,
        TransactionPayloadData::TenureChange(TenureChangePayload {
            public_key_hash: nano_primitives::hash160(&miner.public_key().to_bytes_compressed()),
            ..payload.clone()
        }),
    )
    .expect("the tenure change signs");
    candidate.header.transaction_merkle_root =
        nano_codec::transaction_merkle_root(&candidate.transactions);
    standing
        .chainstate
        .assemble_nakamoto_block_with_bitcoin_operations(
            standing.context,
            &standing.operations,
            Some(standing.parent),
            candidate,
            &miner,
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// The control, and an oracle in its own right.
///
/// The captured tenure change's `previous_tenure_blocks` is what the network
/// counted for that tenure. Accepting it says nano's own count over the blocks it
/// executed is the same number — so the two mutations below are rejections of
/// something real rather than of arithmetic nobody agrees with.
#[test]
fn the_tenure_the_network_counted_is_the_tenure_this_chain_executed() {
    let Some(mut standing) = standing_on_a_whole_tenure() else {
        nano_conformance::skip_gate("the capture holds fewer than two tenures");
        return;
    };
    let payload = tenure_change(&standing.block);
    mine_tenure(&mut standing, &payload).expect("the tenure the network started");
}

#[test]
fn a_tenure_change_miscounting_the_tenure_it_ends_is_rejected() {
    let Some(mut standing) = standing_on_a_whole_tenure() else {
        nano_conformance::skip_gate("the capture holds fewer than two tenures");
        return;
    };
    let payload = tenure_change(&standing.block);
    let claimed = payload.previous_tenure_blocks + 1;
    let error = mine_tenure(
        &mut standing,
        &TenureChangePayload {
            previous_tenure_blocks: claimed,
            ..payload
        },
    )
    .expect_err("a tenure change reports the blocks the tenure it ends actually ran");
    assert!(
        error.contains(&format!("reports {claimed} blocks")),
        "the rejection names both counts: {error}"
    );
}

#[test]
fn a_tenure_change_confirming_a_tenure_this_chain_never_executed_is_rejected() {
    let Some(mut standing) = standing_on_a_whole_tenure() else {
        nano_conformance::skip_gate("the capture holds fewer than two tenures");
        return;
    };
    let payload = tenure_change(&standing.block);
    let error = mine_tenure(
        &mut standing,
        &TenureChangePayload {
            previous_tenure_consensus_hash: nano_primitives::ConsensusHash::from_bytes([9; 20]),
            ..payload
        },
    )
    .expect_err("a tenure change confirms the tenure its parent block belongs to");
    assert!(
        error.contains("previous tenure this chain did not execute"),
        "the rejection names the tenure: {error}"
    );
}
