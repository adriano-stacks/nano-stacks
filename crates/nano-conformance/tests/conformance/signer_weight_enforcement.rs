//! The follow path refuses a block the reward set did not approve.
//!
//! Signer signatures are the only thing that says a Nakamoto block is the
//! network's rather than one node's opinion, and they are invisible to every
//! other check: they are not in the block hash — `signer_signature_hash` is the
//! preimage *without* them — so removing one, reordering two or forging a third
//! changes no identifier, no Merkle root and no state root. A node that does not
//! weigh them follows whatever it is handed.
//!
//! What made this checkable is where the set comes from. `.signers` holds it,
//! written by whichever node reached the prepare phase, under a state root the
//! network agreed with — so reading it back is not trusting a peer, and it works
//! for a cycle stacked before the state was exported, which no walk of pox-5
//! positions can do.
//!
//! These tests mutate a real captured block one field at a time and ask what
//! `append_nakamoto_block_with_bitcoin_operations` *does*, because a rule that
//! only exists as a function is not a rule.

use std::{collections::BTreeMap, fs, path::Path, path::PathBuf};

use nano_chainstate::{BitcoinBlockContext, ChainState, NakamotoBlock};
use nano_crypto::{MessageSignature, StacksPrivateKey};
use nano_primitives::{Hash160, hash160};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// The first captured block above the checkpoint, with the burn context and
/// operations the network gave it.
///
/// One block is enough for every rejection here: the reward set is the cycle's,
/// not the block's, so the first block of the capture is checked against the
/// same set as the last.
fn first_captured_block() -> Option<(
    NakamotoBlock,
    BitcoinBlockContext,
    Vec<nano_bitcoin::BitcoinOperation>,
)> {
    let fixtures = fixtures();
    let snapshots = nano_conformance::captured_bitcoin_snapshots(&fixtures)?;
    let operations = nano_conformance::captured_bitcoin_operations(&fixtures)?;
    nano_conformance::captured_block_paths(&fixtures)
        .into_iter()
        .find_map(|path| {
            let block = NakamotoBlock::decode(&fs::read(&path).ok()?).ok()?;
            if block.header.chain_length == 0 {
                return None;
            }
            let view = block.header.consensus_hash.to_string();
            Some((
                block,
                *snapshots.get(&view)?,
                operations.get(&view).cloned().unwrap_or_default(),
            ))
        })
}

/// Execute one captured block on a fresh checkpoint and say what happened.
fn execute(
    block: &NakamotoBlock,
    context: BitcoinBlockContext,
    operations: &[nano_bitcoin::BitcoinOperation],
) -> Result<(), String> {
    let (mut chainstate, source) = nano_conformance::replay_chainstate(&fixtures())
        .map_err(|error| format!("open the checkpoint: {error}"))?;
    chainstate
        .append_nakamoto_block_with_bitcoin_operations(context, operations, Some(source), block)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// A chainstate standing on the captured checkpoint.
fn checkpoint() -> Option<ChainState> {
    nano_conformance::replay_chainstate(&fixtures())
        .ok()
        .map(|(chainstate, _)| chainstate)
}

/// The reward set the network published for a cycle, by signing-key hash.
fn published_signer_set(root: &Path, cycle: u64) -> Option<BTreeMap<Hash160, u32>> {
    let document: serde_json::Value = serde_json::from_slice(
        &fs::read(root.join(format!("stacker_set/cycle-{cycle}.json"))).ok()?,
    )
    .ok()?;
    Some(
        document["stacker_set"]["signers"]
            .as_array()?
            .iter()
            .map(|entry| {
                let key = entry["signing_key"].as_str().expect("a signing key");
                let key = hex::decode(key.trim_start_matches("0x")).expect("hexadecimal");
                let weight = u32::try_from(entry["weight"].as_u64().expect("a weight"))
                    .expect("the weight fits");
                (hash160(&key), weight)
            })
            .collect(),
    )
}

/// What nano read out of `.signers`, in the same shape.
fn recorded_signer_set(
    chainstate: &mut ChainState,
    context: BitcoinBlockContext,
) -> BTreeMap<Hash160, u32> {
    chainstate
        .recorded_signer_set(context)
        .expect("the cycle has a recorded signer set")
        .entries()
        .iter()
        .copied()
        .collect()
}

/// The set nano reads out of state is the one the network published.
///
/// The strong half of this file: `verify` passing says the signatures map into
/// *some* set nano believes in, and would keep saying so if that set were a
/// superset with the wrong weights. `/v3/stacker_set/:cycle` is the network's own
/// answer for the same cycle, captured from the chain rather than derived here,
/// so agreeing with it is agreeing about who the signers are and what each is
/// worth.
#[test]
fn the_recorded_signer_set_is_the_one_the_network_published() {
    let Some((block, context, _)) = first_captured_block() else {
        nano_conformance::skip_gate("the capture has no block above its checkpoint");
        return;
    };
    let cycle = nano_chainstate::reward_cycle_at(context).expect("the block falls in a cycle");
    let Some(published) = published_signer_set(&fixtures(), cycle) else {
        nano_conformance::skip_gate("the capture published no reward set for the replayed cycle");
        return;
    };
    let mut chainstate = checkpoint().expect("the captured checkpoint opens");
    assert_eq!(
        recorded_signer_set(&mut chainstate, context),
        published,
        "block {} at burn {} falls in cycle {cycle}",
        block.header.chain_length,
        context.height
    );
}

/// The set the node would *write* is the set it reads back.
///
/// Reading `.signers` instead of walking pox-5 every block is only sound because
/// the two answer the same thing — the node wrote those entries from that walk.
/// If they part company, one of them is wrong and the recorded one is the one
/// with a state root behind it, so this failing is a signal about the derivation
/// rather than about the check.
#[test]
fn the_derived_and_recorded_signer_sets_agree() {
    let Some((_, context, _)) = first_captured_block() else {
        nano_conformance::skip_gate("the capture has no block above its checkpoint");
        return;
    };
    let mut chainstate = checkpoint().expect("the captured checkpoint opens");
    let recorded = recorded_signer_set(&mut chainstate, context);
    let derived = chainstate
        .derived_signer_set(context)
        .expect("the cycle has pox-5 positions")
        .signers
        .signing_weights()
        .expect("the derived set is well formed")
        .entries()
        .iter()
        .copied()
        .collect::<BTreeMap<_, _>>();
    assert_eq!(derived, recorded);
}

/// The block the network accepted is accepted, which is what makes the
/// rejections below mean anything.
#[test]
fn a_block_the_reward_set_signed_is_accepted() {
    let Some((block, context, operations)) = first_captured_block() else {
        nano_conformance::skip_gate("the capture has no block above its checkpoint");
        return;
    };
    execute(&block, context, &operations).expect("the block the network accepted");
}

/// A missing set is an incomplete checkpoint, not a signature check that passed.
#[test]
fn a_block_without_an_authenticated_signer_set_is_rejected() {
    let Some((block, context, operations)) = first_captured_block() else {
        nano_conformance::skip_gate("the capture has no block above its checkpoint");
        return;
    };
    let mut reader = checkpoint().expect("the captured checkpoint opens");
    let cycle_length =
        u64::from(context.prepare_phase_length) + u64::from(context.reward_phase_length);
    let missing = (1..=1_000)
        .find_map(|cycles| {
            let mut candidate = context;
            candidate.move_to_burn_block(
                context
                    .height
                    .saturating_add(cycle_length.saturating_mul(cycles)),
            );
            reader
                .recorded_signer_set(candidate)
                .is_err()
                .then_some(candidate)
        })
        .expect("a cycle beyond the checkpoint has no recorded signer set");
    let error = execute(&block, missing, &operations)
        .expect_err("a missing signer set must refuse the followed block");
    assert!(
        error.contains("no authenticated signer set"),
        "the refusal names the absent checkpoint evidence: {error}"
    );
}

/// Drop the last signature: the block is otherwise untouched, including its
/// identifier and its state root, and it must still be refused.
#[test]
fn a_block_below_the_weight_threshold_is_rejected() {
    let Some((block, context, operations)) = first_captured_block() else {
        nano_conformance::skip_gate("the capture has no block above its checkpoint");
        return;
    };
    let mut chainstate = checkpoint().expect("the captured checkpoint opens");
    let set = chainstate
        .recorded_signer_set(context)
        .expect("the cycle has a recorded signer set");
    let threshold = set.approval_threshold().expect("a threshold");
    let mut short = block.clone();
    // Removing signatures from the end until the weight cannot reach the
    // threshold: how many that takes depends on the capture's weights, so it is
    // computed rather than assumed.
    while short.header.signer_signatures.pop().is_some_and(|_| {
        set.verify(&short.header)
            .is_ok_and(|weight| weight >= threshold)
    }) {}
    assert_ne!(
        short.header.signer_signatures, block.header.signer_signatures,
        "the capture's block must carry signatures to remove"
    );
    let error = execute(&short, context, &operations)
        .expect_err("a block short of threshold weight must be refused");
    assert!(
        error.contains("below approval threshold"),
        "the rejection names the weight: {error}"
    );
}

/// Swap two signatures. Every signer is still present and the weight is
/// unchanged; only the order is wrong, which is the rule 4.0 added
/// (`enforces_strict_signature_order`) and the one a threshold check alone
/// would miss.
#[test]
fn signatures_out_of_reward_set_order_are_rejected() {
    let Some((block, context, operations)) = first_captured_block() else {
        nano_conformance::skip_gate("the capture has no block above its checkpoint");
        return;
    };
    let mut swapped = block;
    if swapped.header.signer_signatures.len() < 2 {
        nano_conformance::skip_gate("the captured block carries one signature, so it has no order");
        return;
    }
    swapped.header.signer_signatures.swap(0, 1);
    let error = execute(&swapped, context, &operations)
        .expect_err("signatures out of reward-set order must be refused");
    assert!(
        error.contains("out of reward-set order"),
        "the rejection names the order: {error}"
    );
}

/// Replace a signature with one from a key that stacked nothing.
///
/// It recovers perfectly and belongs to nobody in the set, which is the case a
/// check that only counted signatures would wave through.
#[test]
fn a_signature_from_outside_the_reward_set_is_rejected() {
    let Some((block, context, operations)) = first_captured_block() else {
        nano_conformance::skip_gate("the capture has no block above its checkpoint");
        return;
    };
    let mut forged = block;
    let stranger = StacksPrivateKey::from_seed(b"not a signer");
    let digest = forged.header.signer_signature_hash();
    let signature = stranger.sign(digest.as_bytes());
    // Recovering it back is the point: this is a valid signature over the real
    // block, from a key the cycle never stacked for.
    assert_eq!(
        signature
            .recover(digest.as_bytes())
            .expect("the forged signature recovers")
            .to_bytes_compressed(),
        stranger.public_key().to_bytes_compressed()
    );
    *forged
        .header
        .signer_signatures
        .first_mut()
        .expect("the captured block carries signatures") = signature;
    let error = execute(&forged, context, &operations)
        .expect_err("a signature from outside the reward set must be refused");
    assert!(
        error.contains("unknown or signatures are out of reward-set order"),
        "the rejection names the signer: {error}"
    );
}

/// A signature that recovers to no key at all is refused, and differently.
#[test]
fn a_signature_that_recovers_to_nothing_is_rejected() {
    let Some((block, context, operations)) = first_captured_block() else {
        nano_conformance::skip_gate("the capture has no block above its checkpoint");
        return;
    };
    let mut malformed = block;
    *malformed
        .header
        .signer_signatures
        .first_mut()
        .expect("the captured block carries signatures") = MessageSignature::from_bytes([0xff; 65]);
    let error = execute(&malformed, context, &operations)
        .expect_err("a signature that recovers to nothing must be refused");
    assert!(
        error.contains("invalid signer signature"),
        "the rejection names the signature: {error}"
    );
}

/// The mainnet state a node has imported carries mainnet's own signer set.
///
/// This is the question task 050 could not answer before: nothing is stacked in
/// pox-5 for mainnet's cycle 140, because it was stacked in pox-4 below the
/// checkpoint, so a walk of pox-5 positions finds no set and the check could not
/// run. The `.signers` entries for that cycle came across with the state, and
/// this puts them against `/v3/stacker_set/140` as the network published it.
///
/// Point `NANO_MAINNET_STATE` at a state directory a node has imported into and
/// `NANO_MAINNET_CAPTURE` at the capture that published the reward set.
#[test]
fn the_mainnet_state_carries_the_signer_set_mainnet_published() {
    let (Some(state), Some(capture)) = (
        std::env::var_os("NANO_MAINNET_STATE").map(PathBuf::from),
        std::env::var_os("NANO_MAINNET_CAPTURE").map(PathBuf::from),
    ) else {
        nano_conformance::skip_gate(
            "NANO_MAINNET_STATE and NANO_MAINNET_CAPTURE name a state directory and a capture",
        );
        return;
    };
    // Read-only, because this is somebody's real mainnet state and the gate only
    // reads it. `ChainState::open` would create the directory if the path were
    // wrong, adopt a network, append an `engine_identity` row and leave a WAL --
    // on an operator's 33 GB directory. See task 087.
    let mut chainstate =
        ChainState::open_existing(&state).expect("the mainnet state opens for reading");
    // The cycle the capture published, taken from the file rather than computed:
    // this test is about the set, and the calendar has its own tests.
    let cycle = fs::read_dir(capture.join("stacker_set"))
        .expect("the capture publishes a reward set")
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            name.strip_prefix("cycle-")?
                .strip_suffix(".json")?
                .parse::<u64>()
                .ok()
        })
        .max()
        .expect("a published cycle");
    let published = published_signer_set(&capture, cycle).expect("the published reward set reads");
    let context = mainnet_context(&capture);
    assert_eq!(
        nano_chainstate::reward_cycle_at(context),
        Some(cycle),
        "the burn height the checkpoint stands at has to fall in the published cycle"
    );
    let Some(recorded) = recorded_set_or_skip(&mut chainstate, context) else {
        return;
    };
    assert_eq!(
        recorded
            .entries()
            .iter()
            .copied()
            .collect::<BTreeMap<_, _>>(),
        published
    );
}

/// Mainnet's own blocks pass the check against mainnet's own state.
///
/// The one that matters for turning this into a rejection rather than a report.
/// `mainnet_envelope` proves those five blocks carry threshold weight against the
/// reward set `/v3/stacker_set/140` published, and the test above proves the set
/// in the imported state is that same set — but composing two green tests is an
/// argument, not a measurement, and the whole point of this task is that a rule
/// which refuses a block the network accepted is the worst outcome available. So
/// The set a mainnet state records for the cycle it stands in, or why it has none.
///
/// **Mainnet cycle 140 has none, and cannot.** It was stacked under pox-4, before the
/// state nano imports, so the block that would have written its `.signers` entries is
/// below the checkpoint. Every block replayed from the epoch 4.0 boundary so far is
/// inside that cycle — 141 opens at burn 962,150 — so a state that has not crossed
/// into it can only report the absence, which `ChainState::check_signer_signatures`
/// does deliberately rather than refusing every block of the chain the network is on.
///
/// So this gate is unprovable until a replay crosses into a pox-5-stacked cycle, and
/// it says that rather than failing on an assertion that reads like nano's fault.
/// `NANO_REQUIRE_MAINNET` still turns it into a failure, which is right: a release
/// run may not claim this gate green while the chain it stands on cannot answer it.
fn recorded_set_or_skip(
    chainstate: &mut ChainState,
    context: BitcoinBlockContext,
) -> Option<nano_chainstate::SignerWeights> {
    match chainstate.recorded_signer_set(context) {
        Ok(set) => Some(set),
        Err(error) => {
            nano_conformance::skip_gate(&format!(
                "the mainnet state stands in a cycle with no recorded signer set ({error}): \
                 cycle 140 was stacked under pox-4, below the checkpoint, so this needs a \
                 replay that has crossed into cycle 141 at burn 962,150"
            ));
            None
        }
    }
}

/// this is the measurement: the set out of state, the blocks off the wire, the
/// same `verify` the follow path calls.
#[test]
fn mainnet_blocks_pass_the_check_against_mainnet_state() {
    let (Some(state), Some(capture)) = (
        std::env::var_os("NANO_MAINNET_STATE").map(PathBuf::from),
        std::env::var_os("NANO_MAINNET_CAPTURE").map(PathBuf::from),
    ) else {
        nano_conformance::skip_gate(
            "NANO_MAINNET_STATE and NANO_MAINNET_CAPTURE name a state directory and a capture",
        );
        return;
    };
    // Read-only, because this is somebody's real mainnet state and the gate only
    // reads it. `ChainState::open` would create the directory if the path were
    // wrong, adopt a network, append an `engine_identity` row and leave a WAL --
    // on an operator's 33 GB directory. See task 087.
    let mut chainstate =
        ChainState::open_existing(&state).expect("the mainnet state opens for reading");
    let Some(set) = recorded_set_or_skip(&mut chainstate, mainnet_context(&capture)) else {
        return;
    };
    let threshold = set.approval_threshold().expect("a threshold");
    // Both samples: the five blocks in the tree, and every block the capture
    // holds. They are the same cycle, and the capture is the larger sample.
    let mut checked = 0;
    for directory in [
        fixtures().join("mainnet/blocks"),
        capture.join("nakamoto/blocks"),
    ] {
        for entry in fs::read_dir(&directory).expect("the captured mainnet blocks") {
            let path = entry.expect("a block entry").path();
            let block = NakamotoBlock::decode(&fs::read(&path).expect("read a block"))
                .expect("a captured mainnet block decodes");
            let weight = set.verify(&block.header).unwrap_or_else(|error| {
                panic!(
                    "mainnet block {} at height {} was accepted by mainnet: {error}",
                    path.display(),
                    block.header.chain_length
                )
            });
            assert!(
                weight >= threshold,
                "block {} carries {weight} of the {threshold} its cycle requires",
                block.header.chain_length
            );
            checked += 1;
        }
    }
    assert!(checked > 5, "the capture holds no mainnet blocks to check");
}

/// Every block the mainnet capture holds passes the complete validator.
///
/// The acceptance criterion of the task, for the half that needs no state: the
/// header version, the transactions' network, chain and anchoring, the tenure and
/// coinbase shape, the tenure change's miner and the problematic-transaction
/// markers, over a hundred consecutive blocks mainnet accepted. The rules that
/// read executed state — the signer weight above, the tenure counts, the VRF —
/// are checked where they are checkable.
#[test]
fn every_captured_mainnet_block_authenticates() {
    let Some(capture) = std::env::var_os("NANO_MAINNET_CAPTURE").map(PathBuf::from) else {
        nano_conformance::skip_gate("NANO_MAINNET_CAPTURE must name a capture directory");
        return;
    };
    // An empty chainstate is enough: authentication reads the chain's identity and
    // the block, and nothing else.
    let chainstate =
        ChainState::new(nano_primitives::Network::MAINNET).expect("an empty mainnet chainstate");
    let mut checked = 0;
    for entry in fs::read_dir(capture.join("nakamoto/blocks")).expect("the captured blocks") {
        let path = entry.expect("a block entry").path();
        let block = NakamotoBlock::decode(&fs::read(&path).expect("read a block"))
            .expect("a captured mainnet block decodes");
        chainstate
            .authenticate_block(&block)
            .unwrap_or_else(|error| {
                panic!(
                    "mainnet block {} was accepted by mainnet: {error}",
                    block.header.chain_length
                )
            });
        checked += 1;
    }
    assert!(checked > 0, "the capture holds no blocks to authenticate");
}

/// The stacking calendar and burn height a mainnet capture records.
///
/// Read from the capture rather than written down here: these are the same
/// `/v2/pox` constants every `BitcoinBlockContext` the node builds is made of,
/// and a transcribed one would put the reward cycle somewhere the chain does not.
fn mainnet_context(capture: &Path) -> BitcoinBlockContext {
    let number = |document: &str, name: &str| -> u64 {
        document
            .lines()
            .find_map(|line| {
                line.strip_prefix(&format!("{name} = "))?
                    .trim()
                    .parse()
                    .ok()
            })
            .unwrap_or_else(|| panic!("the capture records {name}"))
    };
    let provenance = fs::read_to_string(capture.join("provenance.toml"))
        .expect("the capture records its provenance");
    let checkpoint = fs::read_to_string(capture.join("chainstate/checkpoint-H/checkpoint.toml"))
        .expect("the capture records its checkpoint");
    let mut context = BitcoinBlockContext::at_height(number(&checkpoint, "first_bitcoin_height"));
    context.first_height = number(&provenance, "pox_first_bitcoin_height");
    context.prepare_phase_length = u32::try_from(number(&provenance, "pox_prepare_phase_length"))
        .expect("the prepare phase fits");
    context.reward_phase_length = u32::try_from(number(&provenance, "pox_reward_phase_length"))
        .expect("the reward phase fits");
    context
}
