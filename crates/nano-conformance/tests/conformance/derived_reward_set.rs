//! The reward set nano derives from its own state, against the published one.
//!
//! `/v3/stacker_set` is the document a signer reads its own weight out of and a
//! node reads whose signatures to count. nano does not relay it: it walks the
//! pox-5 linked list in the state it executed and apportions the positions
//! itself. So there are three ways to be wrong that no other test can see — the
//! amounts, the per-slot threshold, and the single Bitcoin output a waterfall
//! cycle pays — and all three are fields of a document the captured chain
//! published for the same cycles.
//!
//! The payout address is the sharpest of them. It is not an address anyone
//! chose: it is the taproot output key of the sBTC deposit script for the
//! registry's current aggregate key, paying `.pox-5`. Thirty-two bytes derived
//! through two script builds, two leaf hashes, a branch hash and a tweak — one
//! byte wrong anywhere gives a different key with no error attached, and a miner
//! would then commit to an output the network does not pay.
//!
//! Nothing is replayed here. The set belongs to a *cycle*, and the checkpoint
//! already stands inside one, so the state as imported is the state that answers.

use std::{fs, path::PathBuf};

use nano_chainstate::{BitcoinBlockContext, ChainState, NakamotoBlock};
use serde_json::Value;

fn fixtures() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// The checkpoint's chain state, and the burn context of the first block above it.
///
/// The context names the reward cycle: everything the set is derived for comes
/// out of it, and it is the same context the follow path executes that block
/// under.
fn state_and_context() -> Option<(ChainState, BitcoinBlockContext)> {
    let fixtures = fixtures();
    let snapshots = nano_conformance::captured_bitcoin_snapshots(&fixtures)?;
    let context = nano_conformance::captured_block_paths(&fixtures)
        .into_iter()
        .find_map(|path| {
            let block = NakamotoBlock::decode(&fs::read(&path).ok()?).ok()?;
            snapshots.get(&block.header.consensus_hash.to_string()).copied()
        })?;
    let (chainstate, _) = nano_conformance::replay_chainstate(&fixtures).ok()?;
    Some((chainstate, context))
}

/// The `/v3/stacker_set` document the captured chain published for a cycle.
fn published(cycle: u64) -> Option<Value> {
    let path = fixtures().join(format!("stacker_set/cycle-{cycle}.json"));
    let document: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    Some(document["stacker_set"].clone())
}

#[test]
fn the_derived_reward_set_is_the_document_the_network_published() {
    let Some((mut chainstate, context)) = state_and_context() else {
        nano_conformance::skip_gate("the capture has no block above its checkpoint");
        return;
    };
    let cycle = nano_chainstate::signers::reward_cycle_at(context).expect("a reward cycle");
    let Some(expected) = published(cycle) else {
        nano_conformance::skip_gate("the capture published no reward set for its own cycle");
        return;
    };

    let derived = chainstate
        .derived_signer_set(context)
        .expect("the cycle has pox-5 positions to walk");
    assert_eq!(derived.reward_cycle, cycle);
    let registry = nano_conformance::captured_sbtc_registry(&fixtures());
    let payout = chainstate
        .sbtc_payout_address(registry.as_deref())
        .expect("the sBTC registry names an aggregate key");
    let served = nano_rpc::stacker_set_payload(
        &nano_rpc::derived_signers(&derived),
        derived.pox_ustx_threshold,
        Some(&payout),
    );

    // Field by field rather than as one blob, so a failure names which of the
    // three derivations is wrong instead of printing two documents.
    assert_eq!(
        served["sbtc_address"], expected["sbtc_address"],
        "the waterfall payout address"
    );
    assert_eq!(
        served["pox_ustx_threshold"], expected["pox_ustx_threshold"],
        "the per-slot stacking threshold"
    );
    assert_eq!(served["reward_set_version"], expected["reward_set_version"]);
    let signers = served["signers"].as_array().expect("the derived signers");
    assert_eq!(
        signers.len(),
        expected["signers"].as_array().expect("published signers").len(),
        "the number of signers"
    );
    assert!(!signers.is_empty(), "an empty signer set agrees with nothing");
    for (derived, published) in signers.iter().zip(
        expected["signers"]
            .as_array()
            .expect("published signers")
            .iter(),
    ) {
        // The published document is sorted by signing key, and so is nano's, so
        // the two line up entry for entry.
        assert_eq!(derived["signing_key"], published["signing_key"]);
        assert_eq!(
            derived["stacked_amt"], published["stacked_amt"],
            "what {} stacked", published["signing_key"]
        );
        assert_eq!(
            derived["weight"], published["weight"],
            "the weight apportioned to {}", published["signing_key"]
        );
    }
}

/// Every cycle the capture published a set for, one after another.
///
/// The test above checks the cycle the checkpoint stands in. This one checks that
/// a cycle nano has to *look up* — one whose positions were stacked for a cycle
/// other than the one being executed — derives the same way, which is what a node
/// serving `/v3/stacker_set/:cycle` for the cycle after the current one does.
#[test]
fn every_published_cycle_derives_the_same_signers() {
    let Some((mut chainstate, context)) = state_and_context() else {
        nano_conformance::skip_gate("the capture has no block above its checkpoint");
        return;
    };
    let cycle = nano_chainstate::signers::reward_cycle_at(context).expect("a reward cycle");
    let mut compared = 0;
    // Forward only: a cycle already past has had its positions unlocked, so the
    // state that answers for it is not the state this checkpoint holds.
    for ahead in cycle..cycle + 3 {
        let Some(expected) = published(ahead) else {
            continue;
        };
        let mut context = context;
        // The cycle a set is derived for is the context's, and nothing else in
        // the context moves the walk.
        context.height = context.first_height
            + ahead * u64::from(context.prepare_phase_length + context.reward_phase_length)
            + 1;
        let Ok(derived) = chainstate.derived_signer_set(context) else {
            continue;
        };
        let served = nano_rpc::stacker_set_payload(
            &nano_rpc::derived_signers(&derived),
            derived.pox_ustx_threshold,
            None,
        );
        assert_eq!(
            served["signers"], expected["signers"],
            "the signers of cycle {ahead}"
        );
        assert_eq!(
            served["pox_ustx_threshold"], expected["pox_ustx_threshold"],
            "the threshold of cycle {ahead}"
        );
        compared += 1;
    }
    assert!(compared > 0, "no published cycle was derived at all");
}
