//! Locate where nano's execution of a live chain first diverges from a node's.
//!
//! Replays a peer's canonical chain from a checkpoint exactly as a signer does,
//! stops at the first block whose committed state root does not match, and then
//! re-executes that block with the root check disabled so its writes can be
//! compared against the ones the node itself committed. MARF keys are hashes,
//! so the report names them by hashing the Clarity key strings the accounts and
//! contracts in that block would have written.
//!
//! ```text
//! NANO_CHECKPOINT=~/.cache/nano-stacks/hacknet/run/checkpoint \
//! NANO_STOCK_MARF=/tmp/stock/marf.sqlite \
//! NANO_BITCOIN_PASS_FILE=~/.cache/nano-stacks/hacknet/run/bitcoin-rpc.pass \
//!     cargo run -p nano-conformance --example replay-divergence
//! ```

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
};

use clarity::vm::database::{ClarityDatabase, StoreType};
use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier};
use nano_bitcoin::{BitcoinRpcSource, BitcoinSource};
use nano_chainstate::{ChainState, NakamotoBlock, TenureAccounting};
use nano_codec::{Principal, TransactionPayloadData};
use nano_marf::MarfValue;
use nano_node::CheckpointExecutor;
use nano_primitives::{TrieHash, sha512_256};
use nano_sync::{PoxInfo, SyncClient};
use reqwest::Url;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let checkpoint = PathBuf::from(
        env::var("NANO_CHECKPOINT").expect("NANO_CHECKPOINT must point at a checkpoint directory"),
    );
    let peer =
        Url::parse(&env::var("NANO_PEER").unwrap_or_else(|_| "http://127.0.0.1:20443/".to_owned()))
            .expect("NANO_PEER must be a URL");
    let client = SyncClient::new(peer).expect("create the sync client");
    let pox = client.pox_info().await.expect("read the PoX calendar");

    let anchor_height: u64 = checkpoint_value(&checkpoint, "anchor_bitcoin_height")
        .parse()
        .expect("checkpoint anchor height");
    let source = hex_array(&checkpoint_value(&checkpoint, "source_state_id"));
    let root = TrieHash::from_bytes(hex_array(&checkpoint_value(
        &checkpoint,
        "state_index_root",
    )));
    let anchor = NakamotoBlock::decode(&fs::read(checkpoint.join("anchor-block.bin")).unwrap())
        .expect("decode the anchor block");

    let mut context = pox.bitcoin_context();
    context.height = anchor_height;
    let mut executor = CheckpointExecutor::from_checkpoint_with_accounting(
        checkpoint.join("marf.sqlite"),
        source,
        root,
        anchor.clone(),
        context,
        bitcoin(),
        Some(
            TenureAccounting::from_json(
                &fs::read(checkpoint.join("native-effects.json")).expect("read tenure accounting"),
            )
            .expect("decode tenure accounting"),
        ),
    )
    .expect("import the checkpoint");

    // Replay the peer's canonical chain in order, exactly as a signer does.
    let mut pending = Vec::new();
    let mut block_id = client
        .tenure_info()
        .await
        .expect("read the tenure")
        .tip_block_id;
    while block_id != anchor.block_id() {
        let block = client.block(block_id).await.expect("download a block");
        block_id = block.header.parent_block_id;
        pending.push(block);
    }
    pending.reverse();
    println!(
        "replaying {} blocks from height {} to {}",
        pending.len(),
        anchor.header.chain_length + 1,
        pending.last().map_or(0, |block| block.header.chain_length)
    );

    let mut parent = anchor;
    for block in pending {
        let mut context = pox.bitcoin_context();
        context.height = bitcoin_height(&client, &block).await;
        match executor.apply(&block, context) {
            Ok(_) => parent = block,
            Err(error) => {
                println!(
                    "\nblock {} at height {} diverges: {error}",
                    block.block_id(),
                    block.header.chain_length
                );
                report(&mut executor, &parent, &block, context, &pox);
                return;
            }
        }
    }
    println!("no divergence: nano matched every canonical block up to the peer's tip");
}

/// Compare the writes nano and the node performed for one diverging block.
fn report(
    executor: &mut CheckpointExecutor<BitcoinRpcSource>,
    parent: &NakamotoBlock,
    block: &NakamotoBlock,
    context: nano_chainstate::BitcoinBlockContext,
    pox: &PoxInfo,
) {
    let operations = bitcoin()
        .block_at(context.height)
        .expect("read the tenure's Bitcoin block")
        .operations;
    executor
        .chainstate_mut()
        .execute_nakamoto_block_with_bitcoin_operations(
            context,
            &operations,
            Some(*parent.block_id().as_bytes()),
            block,
        )
        .expect("execute the diverging block without its root check");
    let nano = writes(executor.chainstate_mut(), parent, block);
    println!("nano wrote {} leaves", nano.len());

    let Ok(path) = env::var("NANO_STOCK_MARF") else {
        println!("set NANO_STOCK_MARF to a copy of the node's MARF to compare its writes");
        return;
    };
    // An import keeps the block's own trie, back pointers included, so the
    // node's writes are the difference between the two states it imports.
    let stock_state = imported(Path::new(&path), block);
    let stock = difference(
        &leaves(&stock_state, block),
        &leaves(&imported(Path::new(&path), parent), parent),
    )
    .into_iter()
    .collect::<BTreeMap<_, _>>();
    println!("the node wrote {} leaves", stock.len());

    // Identical leaves with a different root means the tries differ in shape,
    // which the content hash and the root's pointer array localize.
    let block_id = *block.block_id().as_bytes();
    println!(
        "content root: nano {:?} node {:?}",
        executor.chainstate_mut().state_content_root(block_id),
        stock_state.state_content_root(block_id)
    );
    descend(executor.chainstate_mut(), &stock_state, block_id);

    let names = key_names(block, pox);
    for (label, side) in [
        ("only nano wrote", difference(&nano, &stock)),
        ("only the node wrote", difference(&stock, &nano)),
    ] {
        println!("\n{label} ({} leaves):", side.len());
        for (key, value) in side {
            let name = names
                .get(&key)
                .map_or_else(|| "unrecognized key".to_owned(), Clone::clone);
            println!(
                "  {} = {}  [{name}]",
                hex::encode(key.as_bytes()),
                hex::encode(value.as_bytes()),
            );
        }
    }
}

/// Walk into the child whose hash differs until the diverging node is reached.
fn descend(nano: &ChainState, stock: &ChainState, block: [u8; 32]) {
    let mut prefix = Vec::new();
    loop {
        let (Some(left), Some(right)) = (
            nano.state_pointers_at(block, &prefix),
            stock.state_pointers_at(block, &prefix),
        ) else {
            println!(
                "the node at prefix {} exists on only one side",
                hex::encode(&prefix)
            );
            return;
        };
        let differing = left
            .iter()
            .zip(&right)
            .enumerate()
            .filter(|(_, (left, right))| left != right)
            .collect::<Vec<_>>();
        if differing.is_empty() {
            println!(
                "the node at prefix {} holds identical pointers and hashes",
                hex::encode(&prefix)
            );
            return;
        }
        println!("\nnode at prefix {}:", hex::encode(&prefix));
        for (slot, (left, right)) in &differing {
            println!("  slot {slot}");
            println!("    nano {:?} hash {}", left.0, left.1);
            println!("    node {:?} hash {}", right.0, right.1);
        }
        // A single slot whose pointer matches but whose hash differs means the
        // shapes agree here and the difference is inside that child.
        let [(_, (left, right))] = differing.as_slice() else {
            return;
        };
        if left.0 != right.0 || left.0.referenced_block.is_some() {
            return;
        }
        prefix.push(left.0.character);
    }
}

/// Import one block state of a node's own MARF.
fn imported(path: &Path, block: &NakamotoBlock) -> ChainState {
    ChainState::from_checkpoint(
        path,
        *block.block_id().as_bytes(),
        block.header.state_index_root,
    )
    .expect("import the node's state at a block")
}

/// Every leaf one block state holds.
fn leaves(state: &ChainState, block: &NakamotoBlock) -> BTreeMap<TrieHash, MarfValue> {
    state
        .state_leaves(*block.block_id().as_bytes())
        .unwrap_or_default()
        .into_iter()
        .collect()
}

/// The leaves a block added or changed relative to its parent.
fn writes(
    state: &ChainState,
    parent: &NakamotoBlock,
    block: &NakamotoBlock,
) -> BTreeMap<TrieHash, MarfValue> {
    let leaves = |state: &ChainState, block: &NakamotoBlock| {
        state
            .state_leaves(*block.block_id().as_bytes())
            .unwrap_or_default()
            .into_iter()
            .collect::<BTreeMap<_, _>>()
    };
    let before = leaves(state, parent);
    leaves(state, block)
        .into_iter()
        .filter(|(key, value)| before.get(key) != Some(value))
        .collect()
}

fn difference(
    left: &BTreeMap<TrieHash, MarfValue>,
    right: &BTreeMap<TrieHash, MarfValue>,
) -> Vec<(TrieHash, MarfValue)> {
    left.iter()
        .filter(|(key, value)| right.get(*key) != Some(*value))
        .map(|(key, value)| (*key, *value))
        .collect()
}

/// Clarity key strings the accounts and contracts in a block would write,
/// indexed by the MARF path they hash to.
fn key_names(block: &NakamotoBlock, pox: &PoxInfo) -> BTreeMap<TrieHash, String> {
    let mut principals = BTreeSet::new();
    for transaction in &block.transactions {
        if let Some(address) = transaction.origin_address() {
            principals.insert(address.to_string());
        }
        if let TransactionPayloadData::TokenTransfer { recipient, .. } =
            transaction.payload().data()
        {
            principals.insert(match recipient {
                Principal::Standard(address) => address.to_string(),
                Principal::Contract {
                    address,
                    contract_name,
                } => format!("{address}.{contract_name}"),
            });
        }
    }
    let mut names = BTreeMap::new();
    let mut record = |key: String, description: String| {
        names.insert(
            TrieHash::from_bytes(*sha512_256(key.as_bytes()).as_bytes()),
            description,
        );
    };
    for principal in &principals {
        let Ok(parsed) = PrincipalData::parse(principal) else {
            continue;
        };
        for (key, label) in [
            (
                ClarityDatabase::make_key_for_account_balance(&parsed),
                "balance",
            ),
            (
                ClarityDatabase::make_key_for_account_nonce(&parsed),
                "nonce",
            ),
            (
                ClarityDatabase::make_key_for_account_stx_locked(&parsed),
                "locked",
            ),
            (
                ClarityDatabase::make_key_for_account_unlock_height(&parsed),
                "unlock height",
            ),
        ] {
            record(key, format!("{principal} {label}"));
        }
    }
    for reserved in [
        "__MARF_BLOCK_HEIGHT_SELF".to_owned(),
        format!(
            "__MARF_BLOCK_HEIGHT_TO_HASH::{}",
            block.header.chain_length - 1
        ),
        format!("__MARF_BLOCK_HASH_TO_HEIGHT::{}", block.block_id()),
    ] {
        record(reserved.clone(), reserved);
    }
    for contract in ["pox-5", "signers", "lockup", "sip-031", "costs-4"] {
        let identifier = QualifiedContractIdentifier::parse(&format!(
            "ST000000000000000000002AMW42H.{contract}"
        ))
        .expect("a boot contract identifier");
        for variable in [
            "reward-cycle-total-stacked",
            "configured",
            "first-burnchain-block-height",
            "pox-prepare-cycle-length",
            "stacking-threshold-25",
        ] {
            record(
                ClarityDatabase::make_key_for_trip(&identifier, StoreType::Variable, variable),
                format!(".{contract} variable {variable}"),
            );
        }
    }
    let _ = pox;
    names
}

fn bitcoin() -> BitcoinRpcSource {
    let password = fs::read_to_string(
        env::var("NANO_BITCOIN_PASS_FILE").expect("NANO_BITCOIN_PASS_FILE must be set"),
    )
    .expect("read the Bitcoin RPC password");
    BitcoinRpcSource::new(
        &env::var("NANO_BITCOIN_RPC").unwrap_or_else(|_| "http://127.0.0.1:18443".to_owned()),
        env::var("NANO_BITCOIN_USER").unwrap_or_else(|_| "hacknet".to_owned()),
        password.trim_end(),
        *b"T3",
    )
    .expect("connect to Bitcoin Core")
}

async fn bitcoin_height(client: &SyncClient, block: &NakamotoBlock) -> u64 {
    client
        .sortition(block.header.consensus_hash)
        .await
        .expect("read a block's sortition")
        .bitcoin_height
}

fn checkpoint_value(directory: &Path, key: &str) -> String {
    let text = fs::read_to_string(directory.join("checkpoint.toml")).expect("read checkpoint.toml");
    text.lines()
        .find_map(|line| {
            let (name, value) = line.split_once('=')?;
            (name.trim() == key).then(|| value.trim().trim_matches('"').to_owned())
        })
        .unwrap_or_else(|| panic!("{key} is not in the checkpoint"))
}

fn hex_array<const N: usize>(value: &str) -> [u8; N] {
    hex::decode(value.trim())
        .expect("hexadecimal checkpoint value")
        .try_into()
        .expect("checkpoint value length")
}
