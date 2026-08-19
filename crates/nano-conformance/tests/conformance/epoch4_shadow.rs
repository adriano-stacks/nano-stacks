//! The decision boundary answers identically in and out of process.
//!
//! Task 140's firewall moves chainstate write authority behind a supervised
//! executor process. Before any authority moves, this gate proves the move
//! changes nothing: two identical checkpoint imports of the captured chain,
//! one driven through the in-process `epoch4_consensus::judge` path and one
//! through the spawned executor binary over its serialized line protocol,
//! must produce byte-identical decision records for every captured block —
//! verdict, sealed root, all five cost dimensions and the bounded receipt
//! commitment — across the capture's reward-cycle boundaries.
//!
//! A tampered candidate is fed to both sides mid-chain and must be refused
//! with the same typed identity on both, and the honest block must still be
//! accepted afterwards on both — a refusal is a decision, and it commits
//! nothing either side of the pipe.
//!
//! Offline, from fixtures, with no environment variable: this gate cannot
//! skip itself. Both sides take their inputs from the same
//! [`epoch4_consensus::DecisionRequest`], so what is compared is exactly what
//! the wire carries.

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::Path;
use std::process::{Command, Stdio};

use epoch4_consensus::{
    DecisionRecord, DecisionRequest, RefusalKind, Verdict, host::STAND_SCHEMA, judge,
};
use nano_chainstate::{BitcoinBlockContext, ChainState, NakamotoBlock};
use nano_conformance::{FixtureManifest, FixtureMode, replay_into};

use crate::follow_path::{
    MovableBurnchain, captured_burnchain, captured_chain, derived_chain, fixture_boundary_blocks,
    fixtures, pox,
};

/// A chainstate standing on the capture's first complete tenure boundary.
///
/// The prefix below it has tenure parents under the checkpoint, which only the
/// unauthenticated fixture replay can apply; everything above it is what the
/// shadow comparison judges through the production authenticated path.
fn standing(directory: &Path) -> (ChainState, Vec<NakamotoBlock>, usize) {
    let (mut chainstate, source) = crate::restart::open(directory);
    let chain = captured_chain();
    let boundary = fixture_boundary_blocks(&chain);
    let blocks = u64::try_from(boundary).expect("the prefix fits");
    let replay = replay_into(
        &mut chainstate,
        source,
        &fixtures(),
        FixtureManifest {
            mode: FixtureMode::Captured,
            replay_blocks: blocks,
            receipts: true,
        },
        0,
        &mut |_, _| {},
    );
    assert_eq!(
        replay.completed, blocks,
        "the fixture prefix did not replay: {replay:?}"
    );
    (chainstate, chain, boundary)
}

/// Everything a request needs, from the capture and a locally derived chain.
struct CapturedInputs {
    snapshots: BTreeMap<String, BitcoinBlockContext>,
    operations: BTreeMap<String, Vec<nano_bitcoin::BitcoinOperation>>,
    tracker: nano_node::sortition::SortitionTracker,
    unlocks: nano_sync::PoxInfo,
}

impl CapturedInputs {
    fn read(last_burn_height: u64, sortition_directory: &Path) -> Self {
        let seed = nano_node::CheckpointManifest::load(fixtures().join("chainstate/checkpoint-H"))
            .expect("read the checkpoint manifest")
            .first_bitcoin_height;
        let burnchain = MovableBurnchain::new(captured_burnchain());
        Self {
            snapshots: nano_conformance::captured_bitcoin_snapshots(&fixtures())
                .expect("the captured snapshots read"),
            operations: nano_conformance::captured_bitcoin_operations(&fixtures())
                .expect("the captured operations read"),
            tracker: derived_chain(seed, last_burn_height, &burnchain, sortition_directory),
            unlocks: pox(),
        }
    }

    /// The context a block executes under, exactly as the replay derives it,
    /// plus the winner keys the authenticated path validates against — from
    /// this node's own derived sortition chain, never from a peer's answer.
    fn request(
        &self,
        block: &NakamotoBlock,
        bitcoin_view: &mut String,
        parent: [u8; 32],
    ) -> DecisionRequest {
        if let Some(view) = block.bitcoin_view_consensus_hash() {
            *bitcoin_view = view.to_string();
        } else if bitcoin_view.is_empty() {
            *bitcoin_view = block.header.consensus_hash.to_string();
        }
        let mut context = *self
            .snapshots
            .get(bitcoin_view.as_str())
            .expect("the block's burn view is captured");
        let tenure_hash = block.header.consensus_hash.to_string();
        if let Some(tenure) = self.snapshots.get(&tenure_hash) {
            let view = context.height;
            context.move_to_burn_block(tenure.height);
            context.extend_view_to(view);
        }
        context.v1_unlock_height = self.unlocks.v1_unlock_height.expect("v1 unlock");
        context.v2_unlock_height = self.unlocks.v2_unlock_height.expect("v2 unlock");
        context.v3_unlock_height = self.unlocks.v3_unlock_height.expect("v3 unlock");
        context.pox_5_activation_height = self.unlocks.pox_5_activation_height.expect("activation");
        let sortition = self
            .tracker
            .snapshot_at(context.tenure_burn_height())
            .expect("the derived chain reaches the tenure");
        context.sortition_hash = *sortition.sortition_hash.as_bytes();
        context.winner_vrf_public_key = Some(
            sortition
                .winner_vrf_public_key
                .expect("the derived sortition resolves the winning VRF key"),
        );
        context.winner_signing_key_hash = Some(
            sortition
                .winner_signing_key_hash
                .expect("the derived sortition resolves the winning signing key"),
        );
        let operations = self
            .operations
            .get(&tenure_hash)
            .expect("the tenure's Bitcoin operations are captured")
            .clone();
        DecisionRequest::new(block, context, operations, Some(parent))
    }
}

/// The spawned executor, one line in and one line out.
struct Shadow {
    child: std::process::Child,
    input: std::process::ChildStdin,
    output: BufReader<std::process::ChildStdout>,
}

impl Shadow {
    fn spawn(directory: &Path, stand: &NakamotoBlock) -> Self {
        let network = nano_conformance::captured_network(&fixtures());
        let mut child = Command::new(env!("CARGO_BIN_EXE_epoch4-shadow-executor"))
            .arg(directory)
            .arg(format!("chain-id:{}", network.chain_id()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn the shadow executor");
        let mut input = child.stdin.take().expect("the executor's stdin");
        let mut output = BufReader::new(child.stdout.take().expect("the executor's stdout"));
        writeln!(
            input,
            "{}",
            serde_json::json!({
                "schema": STAND_SCHEMA,
                "block": hex::encode(stand.encode()),
            })
        )
        .expect("write the stand");
        let mut ready = String::new();
        output.read_line(&mut ready).expect("read the ready line");
        let ready: serde_json::Value = serde_json::from_str(&ready).expect("the ready line parses");
        assert_eq!(
            ready["tip"],
            stand.block_id().to_string(),
            "the executor stands where the state directory does"
        );
        Self {
            child,
            input,
            output,
        }
    }

    fn decide(&mut self, request: &DecisionRequest) -> DecisionRecord {
        writeln!(
            self.input,
            "{}",
            serde_json::to_string(request).expect("the request serializes")
        )
        .expect("write the request");
        let mut line = String::new();
        self.output.read_line(&mut line).expect("read the record");
        serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("the record parses: {error}: {line}"))
    }

    fn finish(mut self) {
        drop(self.input);
        let status = self.child.wait().expect("the executor exits");
        assert!(status.success(), "the executor exited cleanly: {status:?}");
    }
}

/// Every captured block decides identically in and out of process, and a
/// tampered candidate is refused identically without costing either side the
/// honest block that follows it.
#[test]
fn the_decision_boundary_answers_identically_in_and_out_of_process() {
    let in_process = tempfile::tempdir().expect("a directory");
    let out_of_process = tempfile::tempdir().expect("a directory");
    let sortitions = tempfile::tempdir().expect("a directory");

    let (mut chainstate, chain, boundary) = standing(in_process.path());
    {
        let (shadow_state, _, shadow_boundary) = standing(out_of_process.path());
        assert_eq!(boundary, shadow_boundary);
        drop(shadow_state);
    }

    let rows = crate::follow_path::snapshots();
    let last_burn = crate::follow_path::burn_height_of(&rows, chain.last().expect("a tip"));
    let inputs = CapturedInputs::read(last_burn, &sortitions.path().join("sortition"));

    let mut tip = chain[boundary - 1].clone();
    let mut shadow = Shadow::spawn(out_of_process.path(), &tip);

    // Both sides must refuse a forged candidate identically, and the refusal
    // must cost neither of them the honest block that follows. Mid-chain, so
    // state exists on both sides of it.
    let tampered_at = boundary + (chain.len() - boundary) / 2;

    let mut bitcoin_view = String::new();
    let mut accepted = 0_usize;
    for (index, block) in chain.iter().enumerate().skip(boundary) {
        let parent = *tip.block_id().as_bytes();
        let request = inputs.request(block, &mut bitcoin_view, parent);

        if index == tampered_at {
            let mut forged = block.clone();
            forged.header.timestamp = forged.header.timestamp.wrapping_add(1);
            let forged_request = DecisionRequest::new(
                &forged,
                BitcoinBlockContext::try_from(request.context).expect("the context decodes"),
                request.operations.clone(),
                Some(parent),
            );
            let opened = forged_request.open().expect("the forged request opens");
            let ours = judge(&mut chainstate, &opened, &tip, None);
            let theirs = shadow.decide(&forged_request);
            assert_eq!(
                ours.record, theirs,
                "a forged candidate is refused identically at height {}",
                forged.header.chain_length
            );
            assert!(
                matches!(
                    ours.record.verdict,
                    Verdict::Refused {
                        kind: RefusalKind::MinerAuthentication,
                        ..
                    }
                ),
                "a re-timed block's signatures recover to nobody: {:?}",
                ours.record.verdict
            );
            assert!(ours.applied.is_none(), "a refusal commits nothing");
        }

        let opened = request.open().expect("the request opens");
        let ours = judge(&mut chainstate, &opened, &tip, None);
        let theirs = shadow.decide(&request);
        assert_eq!(
            ours.record, theirs,
            "the decision records part at height {}",
            block.header.chain_length
        );
        assert!(
            matches!(ours.record.verdict, Verdict::Accepted),
            "the captured block at height {} was refused: {:?}",
            block.header.chain_length,
            ours.record.verdict
        );
        assert!(
            ours.record.receipts.is_some(),
            "an accepted record carries its receipt commitment"
        );
        tip = block.clone();
        accepted += 1;
    }
    assert_eq!(
        accepted,
        chain.len() - boundary,
        "every captured block above the boundary was decided"
    );

    shadow.finish();
}
