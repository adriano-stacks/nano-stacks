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
//!
//! The tenure rules below are the same shape. A tenure change and a coinbase are
//! how a block claims a tenure and takes the pay for it, and every constraint on
//! the pair — how many, in what order, ending where, naming which miner — is a
//! thing a block can lie about while computing a state that hangs together
//! perfectly. `is_wellformed_tenure_start_block`, `is_wellformed_tenure_extend_block`
//! and `check_tenure_tx` are stacks-core's names for them.

use std::{fs, path::Path};

use nano_chainstate::{ChainState, NakamotoBlock, ProblematicTransaction, TenureAccounting};
use nano_codec::{
    AnchorMode, TenureChangeCause, TenureChangePayload, Transaction, TransactionPayloadData,
    TransactionVersion,
};
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
        <[u8; 32]>::try_from(hex::decode(value).expect("hexadecimal").as_slice()).expect("32 bytes")
    };

    let directory = Box::leak(Box::new(tempfile::tempdir().expect("a directory")));
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

/// A chainstate on the checkpoint, and the first captured block that starts a
/// tenure — the one shape every rule below is about.
fn checkpoint_and_tenure_start() -> Option<(ChainState, NakamotoBlock)> {
    let (chainstate, _) = checkpoint_and_block();
    let block = nano_conformance::captured_block_paths(&fixtures())
        .into_iter()
        .find_map(|path| {
            let block = NakamotoBlock::decode(&fs::read(&path).ok()?).ok()?;
            nano_chainstate::starts_new_tenure(&block).then_some(block)
        })?;
    Some((chainstate, block))
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

/// Rebuild a block's tenure change with one field changed.
///
/// Signed by a key of the test's own, because a transaction carries the bytes it
/// was decoded from: editing a field and re-encoding leaves the origin's own
/// signature over the old payload, which `verify_authorization` refuses before
/// any of this is reached. Signing a new one is what a miner forging a tenure
/// change would have to do too.
///
/// A side effect worth naming: the new transaction is signed by a key that did
/// not sign the header, so such a block also fails the miner tie. Every test
/// below asserts *which* rejection it got, and the shape rules are checked before
/// that tie, so the assertions say which rule ran.
fn with_tenure_change(block: &NakamotoBlock, payload: TenureChangePayload) -> NakamotoBlock {
    let mut block = block.clone();
    let position = block
        .transactions
        .iter()
        .position(|transaction| {
            matches!(
                transaction.payload().data(),
                TransactionPayloadData::TenureChange(_)
            )
        })
        .expect("a tenure-start block carries a tenure change");
    block.transactions[position] = Transaction::sign_standard(
        TransactionVersion::Testnet,
        block.transactions[position].chain_id(),
        AnchorMode::OnChainOnly,
        &nano_crypto::StacksPrivateKey::from_seed(b"another miner"),
        0,
        0,
        TransactionPayloadData::TenureChange(payload),
    )
    .expect("the tenure change signs");
    block
}

/// The first captured tenure-start block authenticates as it stands.
///
/// Without this every rejection below could be rejecting for the wrong reason,
/// or the block could be failing something else entirely.
#[test]
fn a_real_tenure_start_block_authenticates() {
    let Some((chainstate, block)) = checkpoint_and_tenure_start() else {
        nano_conformance::skip_gate("the capture has no tenure-start block");
        return;
    };
    chainstate
        .authenticate_block(&block)
        .expect("a tenure the network accepted authenticates");
}

#[test]
fn a_block_with_no_transactions_is_rejected() {
    let (chainstate, mut block) = checkpoint_and_block();
    block.transactions.clear();
    let rejected = chainstate
        .authenticate_block(&block)
        .expect_err("a block with nothing in it is not a block");
    assert!(
        rejected.to_string().contains("no transactions"),
        "the rejection says so: {rejected}"
    );
}

/// A coinbase without a VRF proof is a 2.x coinbase: it decodes, and it belongs
/// to no 4.0 block. Nothing else would notice — it pays the same miner the same
/// amount — but the proof is what ties the tenure to the sortition it claims.
#[test]
fn a_coinbase_without_a_vrf_proof_is_rejected() {
    let Some((chainstate, mut block)) = checkpoint_and_tenure_start() else {
        nano_conformance::skip_gate("the capture has no tenure-start block");
        return;
    };
    let position = block
        .transactions
        .iter()
        .position(|transaction| {
            matches!(
                transaction.payload().data(),
                TransactionPayloadData::NakamotoCoinbase { .. }
            )
        })
        .expect("a tenure-start block carries a coinbase");
    block.transactions[position] = Transaction::sign_standard(
        TransactionVersion::Testnet,
        block.transactions[position].chain_id(),
        AnchorMode::OnChainOnly,
        &nano_crypto::StacksPrivateKey::from_seed(b"a miner"),
        0,
        0,
        TransactionPayloadData::Coinbase { payload: [0; 32] },
    )
    .expect("the coinbase signs");
    let rejected = chainstate
        .authenticate_block(&block)
        .expect_err("a coinbase with no VRF proof is rejected");
    assert!(
        rejected.to_string().contains("no VRF proof"),
        "the rejection names the proof: {rejected}"
    );
}

#[test]
fn a_second_coinbase_is_rejected() {
    let Some((chainstate, mut block)) = checkpoint_and_tenure_start() else {
        nano_conformance::skip_gate("the capture has no tenure-start block");
        return;
    };
    let coinbase = block.transactions[1].clone();
    block.transactions.push(coinbase);
    let rejected = chainstate
        .authenticate_block(&block)
        .expect_err("one tenure pays one coinbase");
    assert!(
        rejected.to_string().contains("2 coinbases"),
        "the rejection counts them: {rejected}"
    );
}

#[test]
fn a_coinbase_without_a_tenure_change_is_rejected() {
    let Some((chainstate, mut block)) = checkpoint_and_tenure_start() else {
        nano_conformance::skip_gate("the capture has no tenure-start block");
        return;
    };
    block.transactions.remove(0);
    let rejected = chainstate
        .authenticate_block(&block)
        .expect_err("a coinbase nothing authorized is rejected");
    assert!(
        rejected
            .to_string()
            .contains("coinbase without a tenure change"),
        "the rejection says which is missing: {rejected}"
    );
}

/// The change first, the coinbase second. Both are present and both are the
/// captured ones; only the order is wrong.
#[test]
fn a_tenure_start_with_its_transactions_swapped_is_rejected() {
    let Some((chainstate, mut block)) = checkpoint_and_tenure_start() else {
        nano_conformance::skip_gate("the capture has no tenure-start block");
        return;
    };
    block.transactions.swap(0, 1);
    let rejected = chainstate
        .authenticate_block(&block)
        .expect_err("a tenure start puts the change first and the coinbase second");
    assert!(
        rejected
            .to_string()
            .contains("the tenure change is transaction 1"),
        "the rejection names the positions: {rejected}"
    );
}

/// An extension is paid nothing, so a coinbase beside one is a payment for a
/// sortition that did not happen.
#[test]
fn an_extension_carrying_a_coinbase_is_rejected() {
    let Some((chainstate, block)) = checkpoint_and_tenure_start() else {
        nano_conformance::skip_gate("the capture has no tenure-start block");
        return;
    };
    let payload = tenure_change(&block);
    let block = with_tenure_change(
        &block,
        TenureChangePayload {
            cause: TenureChangeCause::Extended,
            previous_tenure_consensus_hash: payload.tenure_consensus_hash,
            ..payload
        },
    );
    let rejected = chainstate
        .authenticate_block(&block)
        .expect_err("an extension is not owed a coinbase");
    assert!(
        rejected.to_string().contains("Extended"),
        "the rejection names the cause: {rejected}"
    );
}

#[test]
fn a_tenure_change_that_does_not_end_at_the_parent_is_rejected() {
    let Some((chainstate, block)) = checkpoint_and_tenure_start() else {
        nano_conformance::skip_gate("the capture has no tenure-start block");
        return;
    };
    let payload = tenure_change(&block);
    let block = with_tenure_change(
        &block,
        TenureChangePayload {
            previous_tenure_end: nano_primitives::StacksBlockId::from_bytes([9; 32]),
            ..payload
        },
    );
    let rejected = chainstate
        .authenticate_block(&block)
        .expect_err("a tenure change confirms the block's own parent");
    assert!(
        rejected
            .to_string()
            .contains("does not end at this block's parent"),
        "the rejection names the parent: {rejected}"
    );
}

#[test]
fn a_tenure_change_naming_another_tenure_is_rejected() {
    let Some((chainstate, block)) = checkpoint_and_tenure_start() else {
        nano_conformance::skip_gate("the capture has no tenure-start block");
        return;
    };
    let payload = tenure_change(&block);
    let block = with_tenure_change(
        &block,
        TenureChangePayload {
            tenure_consensus_hash: nano_primitives::ConsensusHash::from_bytes([9; 20]),
            ..payload
        },
    );
    let rejected = chainstate
        .authenticate_block(&block)
        .expect_err("a tenure change names the tenure of the block it travels in");
    assert!(
        rejected.to_string().contains("other than the block's own"),
        "the rejection names the tenure: {rejected}"
    );
}

/// A block found in a new sortition cannot claim the tenure it is replacing as
/// its own previous tenure: that is what an extension says, and an extension is
/// not paid.
#[test]
fn a_block_found_claiming_its_own_tenure_as_the_previous_one_is_rejected() {
    let Some((chainstate, block)) = checkpoint_and_tenure_start() else {
        nano_conformance::skip_gate("the capture has no tenure-start block");
        return;
    };
    let payload = tenure_change(&block);
    let block = with_tenure_change(
        &block,
        TenureChangePayload {
            previous_tenure_consensus_hash: payload.tenure_consensus_hash,
            ..payload
        },
    );
    let rejected = chainstate
        .authenticate_block(&block)
        .expect_err("a block found starts a tenure distinct from the one before it");
    assert!(
        rejected
            .to_string()
            .contains("previous tenure is not the one its cause requires"),
        "the rejection names the previous tenure: {rejected}"
    );
}

/// The tenure change, unchanged in every field, signed by another key: exactly
/// what lifting one out of a competing miner's block produces.
#[test]
fn a_tenure_change_signed_by_another_miner_is_rejected() {
    let Some((chainstate, block)) = checkpoint_and_tenure_start() else {
        nano_conformance::skip_gate("the capture has no tenure-start block");
        return;
    };
    let payload = tenure_change(&block);
    let block = with_tenure_change(
        &block,
        TenureChangePayload {
            public_key_hash: nano_primitives::hash160(&[7; 33]),
            ..payload
        },
    );
    let rejected = chainstate
        .authenticate_block(&block)
        .expect_err("a tenure change names the miner that signed the block");
    assert!(
        rejected
            .to_string()
            .contains("not signed by the miner that signed the block"),
        "the rejection names the miner: {rejected}"
    );
}

/// Tampering with the header is the other side of the same rule: the signature
/// still recovers, to a key that is not the one the tenure change names.
#[test]
fn a_tampered_header_no_longer_matches_its_tenure_change() {
    let Some((chainstate, mut block)) = checkpoint_and_tenure_start() else {
        nano_conformance::skip_gate("the capture has no tenure-start block");
        return;
    };
    block.header.timestamp += 1;
    let rejected = chainstate
        .authenticate_block(&block)
        .expect_err("the miner signature covers the timestamp");
    assert!(
        rejected
            .to_string()
            .contains("not signed by the miner that signed the block"),
        "the rejection names the miner: {rejected}"
    );
}

/// A marker naming a transaction that is not there.
///
/// Markers say which transactions a replay skips, so one pointing at nothing
/// would have two nodes execute different transaction sets out of the same bytes.
#[test]
fn a_problematic_marker_out_of_bounds_is_rejected() {
    let (chainstate, mut block) = checkpoint_and_block();
    block.header.problematic_transactions = vec![ProblematicTransaction {
        index: u32::try_from(block.transactions.len()).expect("the count fits"),
        category: 1,
    }];
    let rejected = chainstate
        .authenticate_block(&block)
        .expect_err("a marker points at a transaction the block carries");
    assert!(
        rejected.to_string().contains("past the end of the block"),
        "the rejection says so: {rejected}"
    );
}

#[test]
fn problematic_markers_out_of_order_are_rejected() {
    let (chainstate, mut block) = checkpoint_and_block();
    block.header.problematic_transactions = vec![
        ProblematicTransaction {
            index: 0,
            category: 1,
        },
        ProblematicTransaction {
            index: 0,
            category: 1,
        },
    ];
    let rejected = chainstate
        .authenticate_block(&block)
        .expect_err("markers are strictly increasing, which also forbids repeats");
    assert!(
        rejected
            .to_string()
            .contains("does not follow the one before"),
        "the rejection says so: {rejected}"
    );
}

/// A miner may flag a transaction of its own choosing, but not the block's
/// structure: skipping a coinbase or a tenure change on replay would change what
/// the tenure is, not merely what it ran.
#[test]
fn a_problematic_marker_on_a_tenure_transaction_is_rejected() {
    let Some((chainstate, mut block)) = checkpoint_and_tenure_start() else {
        nano_conformance::skip_gate("the capture has no tenure-start block");
        return;
    };
    block.header.problematic_transactions = vec![ProblematicTransaction {
        index: 0,
        category: 1,
    }];
    let rejected = chainstate
        .authenticate_block(&block)
        .expect_err("the block's own structure cannot be skipped");
    assert!(
        rejected
            .to_string()
            .contains("points at a coinbase or tenure change"),
        "the rejection says so: {rejected}"
    );
}

/// More markers than the smallest transactions that could fill a block.
///
/// The cap is checked against `stackslib`'s own constant rather than against the
/// arithmetic being repeated here, which is the same arithmetic and would agree
/// with itself however wrong it was.
#[test]
fn more_problematic_markers_than_a_block_could_hold_are_rejected() {
    assert_eq!(
        nano_chainstate::MAX_PROBLEMATIC_TRANSACTION_MARKERS,
        blockstack_lib::chainstate::nakamoto::MAX_PROBLEMATIC_TX_MARKERS,
        "the cap is the largest block divided by the smallest transaction"
    );
    let (chainstate, mut block) = checkpoint_and_block();
    block.header.problematic_transactions = (0
        ..=nano_chainstate::MAX_PROBLEMATIC_TRANSACTION_MARKERS)
        .map(|index| ProblematicTransaction {
            index: u32::try_from(index).expect("the index fits"),
            category: 1,
        })
        .collect();
    let rejected = chainstate
        .authenticate_block(&block)
        .expect_err("a block cannot flag more transactions than it could hold");
    assert!(
        rejected.to_string().contains("exceed the cap"),
        "the rejection says so: {rejected}"
    );
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

/// The `pox_treatment` bit vector carries no rule in 4.0, and this says why.
///
/// It is a header field this validator deliberately does not check, which is the
/// kind of decision that rots quietly: someone reads the field, sees nothing
/// checking it, and either adds a rule the network does not have or assumes one
/// exists. So the reason is pinned against stacks-core rather than written down.
///
/// A waterfall cycle pays one sBTC output, so it has no reward addresses to
/// punish and `check_pox_bitvector` returns on the first line —
/// `rewarded_addresses()` is `None` for anything but a V0 set, and the miner fills
/// one bit "for deserialization compatibility". The reward set is one the captured
/// chain published for a cycle this suite replays, read by stacks-core's own
/// deserializer, so this is a statement about a chain rather than about a value
/// constructed to make the point.
#[test]
fn pox_treatment_is_not_consensus_under_a_waterfall_reward_set() {
    let document: serde_json::Value = serde_json::from_slice(
        &fs::read(fixtures().join("stacker_set/cycle-22.json")).expect("the published set"),
    )
    .expect("the published set parses");
    let set: blockstack_lib::chainstate::stacks::boot::RewardSet =
        serde_json::from_value(document["stacker_set"].clone())
            .expect("stacks-core reads its own published reward set");
    assert!(
        matches!(
            set,
            blockstack_lib::chainstate::stacks::boot::RewardSet::Waterfall(_)
        ),
        "an epoch 4.0 cycle pays the waterfall"
    );
    assert!(
        set.rewarded_addresses().is_none(),
        "so there are no reward addresses, and no bitvector to check against them"
    );
    assert_eq!(
        set.pox_treatment_bitvec_len(),
        1,
        "and the one bit a miner sends is there to deserialize, not to be believed"
    );
}
