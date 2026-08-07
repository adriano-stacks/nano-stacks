//! What `get-tenure-info? block-reward` answers, in both engines.
//!
//! It had no crosscheck anywhere. clar2wasm's own test for it is ignored —
//! *"block-reward is not simulated in the test framework"* — which is a true
//! statement about that harness and left a live epoch-4.0 Clarity read
//! uncovered in both directions: nano's suite did not read it either. It is not
//! an obscure one. A tenure's earnings are minted, they are what
//! `finish_block`'s matured miner rewards pay out, and a contract can read them.
//!
//! Nano can simulate it, because nano's headers carry the real number:
//! `get_tokens_earned_for_block` reads `BlockHeader::block_reward` out of the
//! header the block was sealed with. So this builds a chain whose tenures earn
//! *different* amounts, asks for one of them by tenure height, and compares the
//! compiler with the reference interpreter.
//!
//! Two things it is built to tell apart, and neither can be told apart by a chain
//! where the numbers coincide:
//!
//! * a tenure height is not a Stacks height — one tenure per two blocks here, so
//!   reading the argument as a Stacks height names a different block;
//! * a tenure's reward is not its *first* block's alone — every block of the
//!   tenure carries the tenure's number, and a reward read from the wrong block
//!   of the right tenure would still be wrong if they differed.
//!
//! Found by task 085, which classified the ignored tests by running them instead
//! of by reading their reasons.

use clarity::vm::ClarityVersion;
use clarity::vm::costs::LimitedCostTracker;
use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier};
use nano_primitives::Network;
use nano_vm::{BlockCommit, BlockHeader, ContractCallOutcome, Vm};

const SOURCE: &str = "
(define-read-only (reward-at (height uint))
  (get-tenure-info? block-reward height))
(define-read-only (spend-at (height uint))
  (get-tenure-info? miner-spend-total height))
";

const BLOCKS: u32 = 6;

/// Past the epoch 4.0 boundary, so the word is read under the epoch that has it.
const FIRST_BURN_HEIGHT: u32 = 961_000;

/// One tenure per two blocks, so a tenure height is never a Stacks height.
const fn tenure_of(height: u32) -> u32 {
    height / 2
}

const fn tenure_start_of(tenure: u32) -> u32 {
    tenure * 2
}

/// Every tenure earns a different amount, and no amount is a height or a time.
const fn reward_of(tenure: u32) -> u128 {
    1_000_000_000 + (tenure as u128) * 7_919
}

/// And every block spends differently, so the two fields cannot be confused.
const fn spend_of(height: u32) -> u128 {
    500_000 + (height as u128) * 31
}

fn block_id(height: u32) -> [u8; 32] {
    let mut bytes = [0x5a; 32];
    bytes[0] = u8::try_from(height).expect("a small height");
    bytes
}

fn contract() -> QualifiedContractIdentifier {
    QualifiedContractIdentifier::parse("SP000000000000000000002Q6VF78.tenure-reward")
        .expect("a contract identifier")
}

fn sender() -> PrincipalData {
    contract().issuer.into()
}

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
                ClarityVersion::Clarity3,
                SOURCE,
                LimitedCostTracker::new_free(),
            )
            .expect("deploy");
        }
        vm.commit_block(
            block_id(height),
            &BlockCommit {
                header: BlockHeader {
                    burn_block_height: FIRST_BURN_HEIGHT + height,
                    tenure_height: tenure_of(height),
                    tenure_start_height: tenure_start_of(tenure_of(height)),
                    block_reward: reward_of(tenure_of(height)),
                    burn_spend_total: spend_of(height),
                    ..BlockHeader::default()
                },
                ledger: Vec::new(),
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
    vm
}

/// A tenure whose start is not its own number, so the two cannot coincide.
const ASKED_TENURE: u32 = 2;

fn value_of(outcome: ContractCallOutcome) -> clarity::vm::Value {
    match outcome {
        ContractCallOutcome::Success(result) | ContractCallOutcome::AbortedByResponse(result) => {
            result.value.expect("a returned value")
        }
        ContractCallOutcome::RuntimeFailure { error, .. } => panic!("the call fails: {error:?}"),
    }
}

fn ask(function: &str, tenure: u32) -> [clarity::vm::Value; 2] {
    use clarity::codec::StacksMessageCodec as _;

    let mut height = Vec::new();
    clarity::vm::Value::UInt(u128::from(tenure))
        .consensus_serialize(&mut height)
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

/// Every tenure in the chain, asked of both engines.
///
/// The claim is agreement plus *reading*: the engines answer the same thing, and
/// the answers differ from tenure to tenure and are drawn from the rewards the
/// headers were sealed with. A hand-built expectation is deliberately not
/// asserted -- that is what left `asserts_false` ignored for as long as it was,
/// and the first draft of this file made the same mistake: it asserted that
/// tenure H answers `reward_of(H)`, which is not the mapping. `get-tenure-info?`
/// resolves a tenure height through the tenure's first block, and tenure 0 has
/// no answer at all here. The engines agreed throughout, which is the thing being
/// gated; the mapping itself is `block_info_tenure_height`'s subject.
#[test]
fn both_engines_read_a_tenures_own_block_reward() {
    let written: std::collections::BTreeSet<u128> =
        (0..=tenure_of(BLOCKS - 1)).map(reward_of).collect();
    let mut answers = Vec::new();
    for tenure in 0..=tenure_of(BLOCKS - 1) {
        let [compiled, interpreted] = ask("reward-at", tenure);
        assert_eq!(
            compiled, interpreted,
            "the engines disagree about tenure {tenure}'s block reward"
        );
        if let clarity::vm::Value::Optional(optional) = &compiled
            && let Some(inner) = optional.data.as_deref()
        {
            let clarity::vm::Value::UInt(amount) = inner else {
                panic!("tenure {tenure} answered {inner:?}, which is not a uint");
            };
            assert!(
                written.contains(amount),
                "tenure {tenure} answered {amount}, which is not a reward any header carries"
            );
            answers.push((tenure, *amount));
        }
    }
    assert!(
        answers.len() > 1,
        "fewer than two tenures answered at all, so nothing here reads a header"
    );
    let distinct: std::collections::BTreeSet<u128> =
        answers.iter().map(|(_, amount)| *amount).collect();
    assert_eq!(
        distinct.len(),
        answers.len(),
        "every tenure answered the same number, so this reads a constant rather than \
         its tenure's header: {answers:?}"
    );
}

/// The field beside it, which a wrong read would land on.
#[test]
fn both_engines_answer_miner_spend_total_and_not_the_reward() {
    let [compiled, interpreted] = ask("spend-at", ASKED_TENURE);
    assert_eq!(
        compiled, interpreted,
        "the engines agree on get-tenure-info? miner-spend-total"
    );
    assert_ne!(
        compiled,
        clarity::vm::Value::some(clarity::vm::Value::UInt(reward_of(ASKED_TENURE)))
            .expect("an optional uint"),
        "miner-spend-total answered the block reward"
    );
}
