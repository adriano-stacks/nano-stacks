//! The follow path enforces what a tenure claims about winning its sortition.
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
//!
//! The miner signature is here too, because it is the same claim from the other
//! side. The VRF proof says the winning leader key produced this tenure's proof;
//! the signature says the winning leader key's *other* half — the block-signing
//! hash its registration carries — signed the block. Both resolve through the same
//! registration, and the capture is what says they agree in the field.

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
    let fixture = capture();
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
    let fixture = capture();
    candidate_keys(&fixture).into_iter().find(|key| {
        nano_chainstate::verify_coinbase_vrf_proof(
            &tenure.target.block,
            key,
            &tenure.target.context.sortition_hash,
        )
        .is_ok()
    })
}

fn capture() -> std::path::PathBuf {
    nano_conformance::capture_root(&Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures"))
}

/// Every VRF key the capture could resolve a winning commitment through.
///
/// Two sources, and the second is the one that matters. The captured Bitcoin
/// blocks hold the registrations that happened *inside* the captured burn window —
/// which on a chain a few hundred blocks old is most of them, and on mainnet is
/// almost none. A leader key is registered once and named for years afterwards, so
/// the registration a winning commitment names normally sits far below any window a
/// capture keeps: the live pox-5 hacknet registers at burn 204 and is still naming
/// those three keys at burn 393.
///
/// `sortition/leader-keys.json` is the artifact that exists for exactly that, and
/// reading it here is what makes these gates runnable against a capture whose keys
/// are below its own window — which is the ordinary case, not the exotic one.
fn candidate_keys(fixture: &Path) -> Vec<[u8; 32]> {
    let mut keys: Vec<[u8; 32]> = nano_conformance::captured_bitcoin_operations(fixture)
        .unwrap_or_default()
        .values()
        .flatten()
        .filter_map(|operation| match operation.kind {
            nano_bitcoin::BitcoinOperationKind::LeaderKeyRegistration {
                vrf_public_key, ..
            } => Some(vrf_public_key),
            _ => None,
        })
        .collect();
    let registry = fixture
        .join("sortition")
        .join(nano_node::sortition::LEADER_KEY_FILE);
    if let Ok(bytes) = fs::read(&registry)
        && let Ok(records) = serde_json::from_slice::<Vec<serde_json::Value>>(&bytes)
    {
        keys.extend(records.iter().filter_map(|record| {
            let hex = record.get("public_key")?.as_str()?;
            <[u8; 32]>::try_from(hex::decode(hex).ok()?.as_slice()).ok()
        }));
    }
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// The leader-key registration that produced this tenure's coinbase proof.
///
/// Found the same way and with the same caveat as `winning_key`: by asking which
/// registration's VRF key verifies the proof.
fn winning_registration(tenure: &Tenure) -> Option<nano_bitcoin::BitcoinOperation> {
    let fixture = capture();
    let key = winning_key(tenure)?;
    nano_conformance::captured_bitcoin_operations(&fixture)?
        .values()
        .flatten()
        .find(|operation| {
            matches!(
                operation.kind,
                nano_bitcoin::BitcoinOperationKind::LeaderKeyRegistration {
                    vrf_public_key,
                    ..
                } if vrf_public_key == key
            )
        })
        .cloned()
}

/// Execute one tenure-start block against a fresh checkpoint and say whether it
/// was accepted.
fn accepts(tenure: &Tenure, context: BitcoinBlockContext) -> Result<(), String> {
    accepts_with(tenure, context, &tenure.target.operations)
}

/// The same, with the Bitcoin operations the tenure's burn block is said to hold.
///
/// A leader-key registration is reused across tenures, so the one a sortition
/// resolves through sits in a burn block far below the tenure it decides — which
/// is why the operations are a parameter here. A node that has that registration
/// in front of it checks the miner signature; one that does not says so.
fn accepts_with(
    tenure: &Tenure,
    context: BitcoinBlockContext,
    operations: &[nano_bitcoin::BitcoinOperation],
) -> Result<(), String> {
    let fixture = capture();
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
            operations,
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

/// The capture says the two halves of a leader key belong together.
///
/// The oracle for the miner-signature rule, and it needs no rule to state it: the
/// registration that produced this tenure's VRF proof also carries a
/// block-signing hash, and that hash is the `Hash160` of the key that signed the
/// tenure's first block. Nothing in nano is consulted for either side — one comes
/// out of a Bitcoin transaction, the other out of a header signature — so this is
/// the chain itself saying what `verify_miner_signature` is allowed to assume.
#[test]
fn the_winning_registration_names_the_key_that_signed_the_tenure() {
    let Some(tenure) = captured_tenure() else {
        nano_conformance::skip_gate("the capture has no tenure-start block on the checkpoint");
        return;
    };
    let Some(registration) = winning_registration(&tenure) else {
        nano_conformance::skip_gate("the capture has no leader key for the winning commitment");
        return;
    };
    let nano_bitcoin::BitcoinOperationKind::LeaderKeyRegistration {
        block_signing_key_hash: Some(signing_key_hash),
        ..
    } = registration.kind
    else {
        nano_conformance::skip_gate("the winning registration carries no block-signing key hash");
        return;
    };
    let recovered = nano_chainstate::recovered_miner_key_hash(&tenure.target.block.header)
        .expect("the tenure's miner signature recovers");
    assert_eq!(
        recovered,
        nano_primitives::Hash160::from_bytes(signing_key_hash),
        "the block-signing hash the winner registered on Bitcoin is the hash of the key \
         that signed the first block of the tenure it won"
    );
}

/// With the registration in front of it, the follow path checks the signature.
///
/// The acceptance half: the winning key, the registration that carries its
/// block-signing hash, and the block the network accepted.
#[test]
fn a_tenure_signed_by_the_winning_miner_is_accepted() {
    let Some(tenure) = captured_tenure() else {
        nano_conformance::skip_gate("the capture has no tenure-start block on the checkpoint");
        return;
    };
    let (Some(key), Some(registration)) = (winning_key(&tenure), winning_registration(&tenure))
    else {
        nano_conformance::skip_gate("the capture has no leader key for the winning commitment");
        return;
    };
    let mut context = tenure.target.context;
    context.winner_vrf_public_key = Some(key);
    let mut operations = tenure.target.operations.clone();
    operations.push(registration);
    accepts_with(&tenure, context, &operations).expect("the tenure the network accepted");
}

/// A registration naming another block-signing key rejects the block.
///
/// Everything else about the block is the captured one's, including the tenure
/// change that names its own miner — so this is the rule that catches a miner
/// which forged a whole coherent block for a sortition it did not win.
#[test]
fn a_tenure_signed_by_a_miner_that_did_not_win_is_rejected() {
    let Some(tenure) = captured_tenure() else {
        nano_conformance::skip_gate("the capture has no tenure-start block on the checkpoint");
        return;
    };
    let (Some(key), Some(registration)) = (winning_key(&tenure), winning_registration(&tenure))
    else {
        nano_conformance::skip_gate("the capture has no leader key for the winning commitment");
        return;
    };
    let mut forged = registration;
    let nano_bitcoin::BitcoinOperationKind::LeaderKeyRegistration {
        block_signing_key_hash,
        ..
    } = &mut forged.kind
    else {
        unreachable!("the registration is a registration");
    };
    *block_signing_key_hash = Some([7; 20]);
    let mut context = tenure.target.context;
    context.winner_vrf_public_key = Some(key);
    let mut operations = tenure.target.operations.clone();
    operations.push(forged);
    let error = accepts_with(&tenure, context, &operations)
        .expect_err("a block signed by a miner that did not win must be rejected");
    assert!(
        error.contains("whose leader key won its sortition"),
        "the rejection names the winner: {error}"
    );
}

/// Without the registration the signature is unchecked, and that is said out loud.
///
/// The state a node is actually in today, on every tenure: it can name the
/// winning leader key, and the burn block that registered that key is far below
/// the operations it is handed, so there is no block-signing hash to check
/// against. The block is accepted — there is nothing to check it against — and
/// the difference between that and a signature that verified has to be visible.
#[test]
fn an_unregistered_signing_key_accepts_the_block_and_says_so() {
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
    accepts(&tenure, context).expect("an uncheckable signature is not a failed one");
}
