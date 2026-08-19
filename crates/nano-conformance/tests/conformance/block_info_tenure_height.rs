//! `get-block-info?` reads a tenure height from epoch 3.0, and reads burn time.
//!
//! Two things about the pre-Nakamoto word, both of which clarity-wasm had wrong
//! and the interpreter has right:
//!
//! * the height a Clarity 1 or Clarity 2 contract passes it is a **tenure**
//!   height from epoch 3.0 on, resolved through the tenure's first Stacks block
//!   — the same switch `block-height` made, so that `(get-block-info? … (- block-height u1))`
//!   keeps meaning what it meant. clarity-wasm passed the number straight to the
//!   Stacks-height reads, which on mainnet names a block from a year and a half
//!   earlier;
//! * `time` is the **burn** block time of that block, not the Nakamoto header's
//!   own timestamp. `get-stacks-block-info? time` is the one that reads the
//!   timestamp, and clarity-wasm called it for both.
//!
//! Mainnet block 8,706,194 is where it stopped a replay. Two calls to
//! `age009-token-lock::get-tokens-many` compare
//! `(unwrap-panic (get-block-info? time (- block-height u1)))` against a vesting
//! timestamp forty times over; against a time twenty months stale, twenty of the
//! forty took the other branch, returned `ERR-BLOCK-HEIGHT-NOT-REACHED` and did
//! half the writes. Both engines called the transaction a success, so only the
//! value, the write count and the root moved.
//!
//! The chain here is the smallest one that can tell any of that apart: tenure
//! heights that advance at half the rate of Stacks heights, so a tenure height
//! and a Stacks height are never the same number, and a burn time that is never a
//! Stacks timestamp.

use clarity::vm::ClarityVersion;
use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier};
use nano_primitives::Network;
use nano_vm::{BlockCommit, BlockHeader, ContractCallOutcome, Vm};

/// Reads a block-info property at a height the caller supplies.
///
/// Clarity 2, because that is the version the word survives in and the version
/// the height translation applies to.
const SOURCE: &str = "
(define-read-only (time-at (height uint))
  (get-block-info? time height))
(define-read-only (burn-hash-at (height uint))
  (get-block-info? burnchain-header-hash height))
";

/// Blocks in the chain built below, the last of which runs the call.
const BLOCKS: u32 = 5;

/// Where epoch 4.0 starts on mainnet: the burn heights have to be past it or
/// every block reads as epoch 2.x and no translation is due.
const FIRST_BURN_HEIGHT: u32 = 961_000;

/// The tenure a Stacks height belongs to: one tenure per two blocks.
const fn tenure_of(height: u32) -> u32 {
    height / 2
}

/// The Stacks height a tenure's first block sits at.
const fn tenure_start_of(tenure: u32) -> u32 {
    tenure * 2
}

/// The burn block time of the block at a Stacks height.
const fn burn_time_of(height: u32) -> u64 {
    1_700_000_000 + (height as u64) * 600
}

/// The Nakamoto timestamp of the block at a Stacks height, never a burn time.
const fn stacks_time_of(height: u32) -> u64 {
    burn_time_of(height) + 13
}

fn block_id(height: u32) -> [u8; 32] {
    let mut bytes = [0x77; 32];
    bytes[0] = u8::try_from(height).expect("a small height");
    bytes
}

fn burn_header_of(height: u32) -> [u8; 32] {
    let mut bytes = [0x11; 32];
    bytes[0] = u8::try_from(height).expect("a small height");
    bytes
}

fn contract() -> QualifiedContractIdentifier {
    QualifiedContractIdentifier::parse("SP000000000000000000002Q6VF78.block-info")
        .expect("a contract identifier")
}

fn sender() -> PrincipalData {
    contract().issuer.into()
}

/// Build the chain and leave a block open on top of it, ready to run a call.
///
/// The contract is deployed in the last sealed block rather than the open one so
/// that the call runs against a committed deployment, as a transaction does.
fn chain_with_a_block_open() -> Vm {
    let mut vm = Vm::new(Network::MAINNET).expect("create the VM");
    for height in 0..BLOCKS {
        let parent = height.checked_sub(1).map(block_id);
        vm.begin_block_at_bitcoin_height(
            parent,
            block_id(height),
            u64::from(FIRST_BURN_HEIGHT + height),
        )
        .expect("begin a block");
        if height == BLOCKS - 1 {
            vm.deploy_contract(
                contract(),
                ClarityVersion::Clarity2,
                SOURCE,
                LimitedCostTracker::new_free(),
            )
            .expect("deploy");
        }
        vm.commit_block(
            block_id(height),
            &BlockCommit {
                header: BlockHeader {
                    burn_header_hash: burn_header_of(height),
                    burn_block_height: FIRST_BURN_HEIGHT + height,
                    burn_block_time: burn_time_of(height),
                    stacks_block_time: stacks_time_of(height),
                    tenure_height: tenure_of(height),
                    tenure_start_height: tenure_start_of(tenure_of(height)),
                    ..BlockHeader::default()
                },
                ledger: Vec::new(),
                decision: None,
            },
        )
        .expect("seal it");
    }
    vm.begin_block_at_bitcoin_height(
        Some(block_id(BLOCKS - 1)),
        block_id(BLOCKS),
        u64::from(FIRST_BURN_HEIGHT + BLOCKS),
    )
    .expect("begin the open block");
    // No `set_tenure_height`: Clarity's stored tenure height is what
    // `block-height` reads, and this contract takes the height as an argument
    // instead -- which is the point, since the defect was in translating an
    // argument. The tenure heights that matter here are the ones the block
    // headers record, which `get_block_height_for_tenure_height` resolves
    // through, and `commit_block` writes them below.
    vm
}

/// The tenure height asked about, and the Stacks height it resolves to.
///
/// Deliberately a tenure whose start is *not* its own number: the whole defect
/// was reading the number as a Stacks height, and a tenure that starts at its own
/// height cannot tell the two apart.
const ASKED_TENURE: u32 = 1;
const ANSWERING_HEIGHT: u32 = tenure_start_of(ASKED_TENURE);

fn value_of(outcome: ContractCallOutcome) -> clarity::vm::Value {
    match outcome {
        ContractCallOutcome::Success(result) | ContractCallOutcome::AbortedByResponse(result) => {
            result.value.expect("a returned value")
        }
        ContractCallOutcome::RuntimeFailure { error, .. } => panic!("the call fails: {error:?}"),
    }
}

fn ask(function: &str) -> [clarity::vm::Value; 2] {
    let height = clarity::vm::Value::UInt(u128::from(ASKED_TENURE))
        .serialize_to_vec()
        .expect("serialize the height");
    let mut vm = chain_with_a_block_open();
    let compiled = vm
        .execute_contract_call_outcome(
            sender(),
            None,
            contract(),
            function,
            std::slice::from_ref(&height),
            &LimitedCostTracker::new_free(),
        )
        .expect("the compiled call runs");
    let interpreted = nano_oracle::interpret_contract_call(
        &mut vm,
        nano_oracle::ContractCall {
            sender: sender(),
            sponsor: None,
            contract: contract(),
            function,
            arguments: &[height],
        },
        LimitedCostTracker::new_free(),
    )
    .expect("the interpreted call runs");
    [value_of(compiled), value_of(interpreted)]
}

#[test]
fn get_block_info_time_is_the_tenure_start_burn_time_in_both_engines() {
    let [compiled, interpreted] = ask("time-at");
    assert_eq!(
        compiled, interpreted,
        "both engines answer get-block-info? time the same"
    );
    assert_eq!(
        compiled,
        clarity::vm::Value::some(clarity::vm::Value::UInt(u128::from(burn_time_of(
            ANSWERING_HEIGHT
        ))))
        .expect("an optional uint"),
        "the answer is the burn time of the block tenure {ASKED_TENURE} started at"
    );
}

#[test]
fn get_block_info_resolves_a_tenure_height_in_both_engines() {
    let [compiled, interpreted] = ask("burn-hash-at");
    assert_eq!(
        compiled, interpreted,
        "both engines answer get-block-info? burnchain-header-hash the same"
    );
    assert_eq!(
        compiled,
        clarity::vm::Value::some(
            clarity::vm::Value::buff_from(burn_header_of(ANSWERING_HEIGHT).to_vec())
                .expect("a 32-byte buffer")
        )
        .expect("an optional buffer"),
        "the answer names the burn block of the tenure, not of the Stacks height"
    );
}
