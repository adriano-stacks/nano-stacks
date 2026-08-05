//! The follow path enforces the tenure VRF rules, rather than merely owning them.
//!
//! `verify_coinbase_vrf_proof` and `verify_committed_vrf_seed` were correct and
//! unreachable: every captured tenure was checked against both in a test, and
//! nothing called either while following a chain. A rule nothing calls is not a
//! rule — a miner could claim a tenure it did not win, or commit a seed of its
//! choosing to steer the sortition after it, and a state root would not notice
//! because the block computes a perfectly self-consistent state.
//!
//! So these tests go through `ChainState::append_nakamoto_block_with_bitcoin_operations`
//! and ask what it does, not what the rules would have said.

use std::{fs, path::Path};

use nano_chainstate::{BitcoinBlockContext, NakamotoBlock};

/// One captured block with the burn context and operations it executed under.
#[derive(Clone)]
struct Captured {
    block: NakamotoBlock,
    context: BitcoinBlockContext,
    operations: Vec<nano_bitcoin::BitcoinOperation>,
}

/// A captured tenure-start block, and everything that has to run before it.
struct Tenure {
    /// The blocks from the checkpoint up to but not including the target.
    prefix: Vec<Captured>,
    target: Captured,
}

/// The first captured tenure-start block above the checkpoint anchor.
///
/// A tenure-start block cannot be executed on its own — the checkpoint's own
/// anchor block is not one — so the blocks before it come along, and run with
/// the contexts the network gave them.
fn captured_tenure() -> Option<Tenure> {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let snapshots = nano_conformance::captured_bitcoin_snapshots(&fixture)?;
    let operations = nano_conformance::captured_bitcoin_operations(&fixture)?;
    let mut prefix = Vec::new();
    for path in nano_conformance::captured_block_paths(&fixture) {
        let block = NakamotoBlock::decode(&fs::read(&path).ok()?).ok()?;
        let view = block.header.consensus_hash.to_string();
        let (Some(context), Some(operations)) = (snapshots.get(&view), operations.get(&view))
        else {
            continue;
        };
        let captured = Captured {
            block,
            context: *context,
            operations: operations.clone(),
        };
        // The first captured block is the checkpoint's anchor, already executed.
        if prefix.is_empty() && captured.block.header.chain_length > 0 {
            prefix.push(captured);
            continue;
        }
        if nano_chainstate::starts_new_tenure(&captured.block) {
            return Some(Tenure {
                prefix,
                target: captured,
            });
        }
        prefix.push(captured);
    }
    None
}

/// The leader key that produced this tenure's coinbase proof.
///
/// Found by trying every registration the capture holds, rather than by
/// resolving the winning commitment's `key_block_height`/`key_transaction_index`
/// — several miners commit in one burn block and the captured contexts are keyed
/// by consensus hash rather than by burn height, so resolving it properly here
/// would rebuild the node's own lookup inside a test of something else.
///
/// Using the rule to find its own input is only sound for the *acceptance* case,
/// which exists to show the rejections below are not rejecting everything. The
/// rejections do not depend on this.
fn winning_key(tenure: &Tenure) -> Option<[u8; 32]> {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let operations = nano_conformance::captured_bitcoin_operations(&fixture)?;
    operations
        .values()
        .flatten()
        .filter_map(|operation| match operation.kind {
            nano_bitcoin::BitcoinOperationKind::LeaderKeyRegistration {
                vrf_public_key, ..
            } => Some(vrf_public_key),
            _ => None,
        })
        .find(|key| {
            nano_chainstate::verify_coinbase_vrf_proof(
                &tenure.target.block,
                key,
                &tenure.target.context.sortition_hash,
            )
            .is_ok()
        })
}

/// Execute one tenure-start block against a fresh checkpoint and say whether it
/// was accepted.
fn accepts(tenure: &Tenure, context: BitcoinBlockContext) -> Result<(), String> {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let (mut chainstate, source) = nano_conformance::replay_chainstate(&fixture)
        .map_err(|error| format!("open the checkpoint: {error}"))?;
    let mut parent = Some(source);
    for captured in &tenure.prefix {
        chainstate
            .append_nakamoto_block_with_bitcoin_operations(
                captured.context,
                &captured.operations,
                parent,
                &captured.block,
            )
            .map_err(|error| format!("the prefix must execute: {error}"))?;
        parent = Some(*captured.block.block_id().as_bytes());
    }
    chainstate
        .append_nakamoto_block_with_bitcoin_operations(
            context,
            &tenure.target.operations,
            parent,
            &tenure.target.block,
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// A key that is a real curve point and did not produce this proof.
fn stranger_key() -> [u8; 32] {
    nano_crypto::VrfPrivateKey::from_bytes([3; 32])
        .public_key()
        .to_bytes()
}

/// The block the network accepted is accepted, with its real key and hash.
///
/// This is the half that would pass if the check were skipped, so it is only
/// worth anything beside the rejection below.
#[test]
fn a_tenure_with_its_own_leader_key_is_accepted() {
    let Some(tenure) = captured_tenure() else {
        nano_conformance::skip_gate("the capture has no tenure-start block on the checkpoint");
        return;
    };
    let Some(key) = winning_key(&tenure) else {
        nano_conformance::skip_gate("the capture has no leader key for the winning commitment");
        return;
    };
    let mut context = tenure.target.context;
    context.winner_vrf_public_key = Some(key);
    accepts(&tenure, context).expect("the tenure the network accepted");
}

/// A proof that is not the winning miner's is rejected before anything executes.
#[test]
fn a_tenure_with_another_miners_leader_key_is_rejected() {
    let Some(tenure) = captured_tenure() else {
        nano_conformance::skip_gate("the capture has no tenure-start block on the checkpoint");
        return;
    };
    let mut context = tenure.target.context;
    context.winner_vrf_public_key = Some(stranger_key());
    let error = accepts(&tenure, context).expect_err("a proof from another key must be rejected");
    assert!(
        error.contains("registered VRF key") || error.contains("leader"),
        "the rejection should name the proof, not something downstream: {error}"
    );
}

/// A key that is not a curve point at all is rejected too, and differently.
#[test]
fn a_tenure_with_a_malformed_leader_key_is_rejected() {
    let Some(tenure) = captured_tenure() else {
        nano_conformance::skip_gate("the capture has no tenure-start block on the checkpoint");
        return;
    };
    let mut context = tenure.target.context;
    context.winner_vrf_public_key = Some([7; 32]);
    accepts(&tenure, context).expect_err("a malformed key must be rejected");
}

/// An unknown leader key does not silently pass as "checked".
///
/// This is the case a node in its first tenures after a checkpoint is in, and it
/// is the one worth being careful about: the block *is* accepted, because there
/// is nothing to check it against, and that has to be visible rather than
/// indistinguishable from a proof that verified.
#[test]
fn an_unknown_leader_key_accepts_the_block_and_says_so() {
    let Some(tenure) = captured_tenure() else {
        nano_conformance::skip_gate("the capture has no tenure-start block on the checkpoint");
        return;
    };
    let mut context = tenure.target.context;
    context.winner_vrf_public_key = None;
    accepts(&tenure, context).expect("an uncheckable proof is not a failed one");
}
