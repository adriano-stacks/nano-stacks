//! Ask stacks-core what a mainnet checkpoint's trie actually holds.
//!
//! nano's imported mainnet trie has no value for
//! `clarity-contract::<id>` for a contract deployed long before the
//! checkpoint, which stops execution at the first deployment referencing it.
//! Two things could produce that: the import dropped a key the source had, or
//! the source never had it under that name. They need opposite fixes, and
//! stacks-core reading its own checkpoint settles which.
//!
//! Point `NANO_MAINNET_MARF` at the checkpoint's `marf.sqlite` and
//! `NANO_MAINNET_BLOCK` at the state identifier it was taken at:
//!
//! ```text
//! NANO_MAINNET_MARF=/path/checkpoint-H/marf.sqlite \
//! NANO_MAINNET_BLOCK=<64 hex chars> \
//!   cargo test -p nano-conformance --test mainnet_checkpoint -- --nocapture
//! ```

use std::env;

use blockstack_lib::chainstate::stacks::index::marf::{MARF, MARFOpenOpts};
use stacks_common::types::chainstate::StacksBlockId;

/// The contract whose absence stopped a mainnet replay.
const CONTRACT: &str = "SP4SZE494VC2YC5JYG7AYFQ44F5Q4PYV7DVMDPBG.native-pool-v1";

/// How many checkpointed contracts nano's imported trie can still find.
///
/// A matching root proves the trie has the right shape, not that every key
/// written before the checkpoint can be walked to. Clarity reads a contract
/// through `clarity-contract::<id>` and its analysis loader swallows a failed
/// read, so an unreachable key surfaces as "unresolved contract" — which is
/// where a mainnet replay stopped. This counts them.
///
/// ```text
/// NANO_NODE_MARF=/path/state/chainstate/marf.sqlite \
/// NANO_NODE_CLARITY=/path/state/chainstate/clarity.sqlite \
/// NANO_MAINNET_BLOCK=<64 hex chars> \
///   cargo test -p nano-conformance --test mainnet_checkpoint -- --nocapture
/// ```
#[test]
fn every_checkpointed_contract_is_reachable_in_the_imported_trie() {
    let (Ok(marf), Ok(clarity), Ok(block)) = (
        env::var("NANO_NODE_MARF"),
        env::var("NANO_NODE_CLARITY"),
        env::var("NANO_MAINNET_BLOCK"),
    ) else {
        eprintln!("set NANO_NODE_MARF, NANO_NODE_CLARITY and NANO_MAINNET_BLOCK to run this");
        return;
    };
    let block: nano_marf::MarfBlockId =
        <[u8; 32]>::try_from(hex::decode(&block).expect("hexadecimal").as_slice())
            .expect("32 bytes");

    let connection = rusqlite::Connection::open_with_flags(
        &clarity,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open the side store");
    // A sample, not a sweep: walking a mainnet trie for every contract takes
    // longer than the answer is worth, and whether the fault is one contract
    // or all of them shows up in twenty.
    let mut statement = connection
        .prepare(
            "SELECT key FROM metadata_table WHERE key LIKE 'clr-meta::%::analysis' \
             ORDER BY key LIMIT 20",
        )
        .expect("query the side store");
    let contracts: Vec<String> = statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("read the side store")
        .filter_map(|key| {
            let key = key.ok()?;
            Some(
                key.strip_prefix("clr-meta::")?
                    .strip_suffix("::analysis")?
                    .to_owned(),
            )
        })
        .collect();
    // Plus the one a mainnet replay actually stopped on.
    let mut contracts = contracts;
    contracts.push(CONTRACT.to_owned());
    assert!(!contracts.is_empty(), "the side store names contracts");

    let trie = nano_marf::VersionedMarf::open(&marf).expect("open the trie");
    let missing: Vec<&String> = contracts
        .iter()
        .filter(|contract| {
            trie.get(block, format!("clarity-contract::{contract}").as_bytes())
                .is_none()
        })
        .collect();

    println!(
        "{} of {} checkpointed contracts are unreachable",
        missing.len(),
        contracts.len()
    );
    for contract in missing.iter().take(10) {
        println!("  unreachable: {contract}");
    }
    assert!(missing.is_empty(), "every checkpointed contract is reachable");
}

#[test]
fn stacks_core_finds_the_contract_nano_cannot() {
    let (Ok(path), Ok(block)) = (env::var("NANO_MAINNET_MARF"), env::var("NANO_MAINNET_BLOCK"))
    else {
        eprintln!("set NANO_MAINNET_MARF and NANO_MAINNET_BLOCK to run this");
        return;
    };
    let block = StacksBlockId::from_hex(&block).expect("the block identifier is hexadecimal");

    let mut marf: MARF<StacksBlockId> =
        MARF::from_path(&path, MARFOpenOpts::default()).expect("open the checkpoint");
    let key = format!("clarity-contract::{CONTRACT}");
    // `get_with_proof` is the public read that does not need a storage handle;
    // the proof is discarded, only presence is in question.
    let found = marf
        .get_with_proof(&block, &key)
        .expect("query the checkpoint")
        .is_some();

    // Printed rather than only asserted: whichever way it goes names the fix,
    // and a bare failure would not.
    println!(
        "stacks-core {} {key} at {block}",
        if found { "finds" } else { "does not find" }
    );
    assert!(
        found,
        "the source checkpoint has no {key}, so nano's import did not drop it \
         and the key nano looks up is the thing to question"
    );
}
