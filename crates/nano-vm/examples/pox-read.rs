//! Evaluate a PoX-5 read-only function against a captured block's state.

use std::{env, path::Path};

use clarity::vm::{Value, types::PrincipalData};
use nano_primitives::TrieHash;
use nano_vm::{BitcoinBlockContext, Vm};

fn main() {
    let mut arguments = env::args().skip(1);
    let block: [u8; 32] = hex::decode(arguments.next().expect("block id"))
        .expect("hex block id")
        .try_into()
        .expect("32-byte block id");
    let root: [u8; 32] = hex::decode(arguments.next().expect("state root"))
        .expect("hex state root")
        .try_into()
        .expect("32-byte state root");
    let height: u64 = arguments
        .next()
        .expect("bitcoin height")
        .parse()
        .expect("height");
    let function = arguments.next().expect("function name");
    let staker = arguments.next();

    let checkpoint = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../nano-conformance/fixtures/chainstate/checkpoint-H/marf.sqlite");
    let mut vm = Vm::from_checkpoint(checkpoint, block, TrieHash::from_bytes(root))
        .expect("open the captured state");
    vm.begin_block_execution(
        Some(block),
        [0x7f; 32],
        BitcoinBlockContext {
            first_height: 0,
            prepare_phase_length: 5,
            reward_phase_length: 15,
            pox_5_activation_height: 262,
            ..BitcoinBlockContext::at_height(height)
        },
    )
    .expect("begin a probe block");
    let pox = clarity::boot_util::boot_code_id("pox-5", false);
    let sender = PrincipalData::Standard(pox.issuer.clone());
    let args = staker
        .map(|staker| {
            vec![Value::Principal(
                PrincipalData::parse(&staker).expect("principal"),
            )]
        })
        .unwrap_or_default();
    println!(
        "{function} = {:?}",
        vm.call_contract_values(sender, &pox, &function, &args)
    );
}
