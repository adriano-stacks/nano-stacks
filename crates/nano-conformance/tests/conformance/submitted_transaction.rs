//! A transaction posted to the public RPC, mined by this node, and executed.
//!
//! `/v2/transactions` was checkable in three unrelated pieces: the route decodes
//! and admits (a `nano-rpc` unit test against a fabricated account), the mempool
//! judges (`mempool.rs`, against stacks-core's own admission), and blocks execute
//! (the replay). What none of them touched is the *path*: a transaction arriving
//! over HTTP, entering the pool a miner reads, being selected, being executed in a
//! block this node assembled, and appearing in the `new_block` an observer is
//! sent. That path is what "the route is live" has to mean, and until now nothing
//! offline walked it — the workspace had no test that mined a transaction at all.
//!
//! The transaction is a **captured one**, posted verbatim. That settles the
//! problem that made this hard: a valid transaction needs a funded account at the
//! right nonce, and a fixture cannot forge one without a private key the capture
//! does not carry. A transaction the network itself accepted at this height is
//! valid by construction — and it comes with an oracle, because the capture
//! records the receipt stacks-core published for it.
//!
//! So the chain is replayed up to just below the block that carried it, the block
//! is dropped, and nano builds its own in that place out of its own mempool. The
//! state root is nano's and is not compared to anything: this is about a
//! transaction's journey, not about a root. What is compared is the receipt.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use nano_chainstate::{ChainState, NakamotoBlock, TransactionStatus, starts_new_tenure};
use nano_codec::Transaction;
use nano_conformance::{FixtureManifest, FixtureMode};
use nano_mempool::{Account, Mempool};
use nano_primitives::Sha256Sum;
use nano_rpc::{ChainAccess, RpcState};
use tokio::sync::Mutex;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// Whether this is a transaction a mempool is allowed to carry at all.
///
/// A coinbase and a tenure change are the miner's own and are refused by name
/// (`NoCoinbaseViaMempool`, `NoTenureChangeViaMempool`), so a block whose only
/// transactions are those is no use here.
const fn comes_via_the_mempool(transaction: &Transaction) -> bool {
    !matches!(
        transaction.payload().data(),
        nano_codec::TransactionPayloadData::NakamotoCoinbase { .. }
            | nano_codec::TransactionPayloadData::Coinbase { .. }
            | nano_codec::TransactionPayloadData::CoinbaseToAltRecipient { .. }
            | nano_codec::TransactionPayloadData::TenureChange { .. }
    )
}

/// The first captured block that a miner could have built out of its mempool.
///
/// Mid-tenure on purpose: a tenure-start block carries a tenure change and a
/// coinbase that a miner has to put there itself, and this test is about the
/// transactions it does *not* invent. Answers the block's position in the capture,
/// so the replay can stop just below it.
fn first_mempool_block() -> Option<(usize, NakamotoBlock)> {
    nano_conformance::captured_block_paths(&fixtures())
        .into_iter()
        .enumerate()
        .find_map(|(position, path)| {
            let block = NakamotoBlock::decode(&fs::read(&path).ok()?).ok()?;
            let usable = !starts_new_tenure(&block)
                && block.transactions.iter().any(comes_via_the_mempool)
                && block.transactions.iter().all(comes_via_the_mempool);
            usable.then_some((position, block))
        })
}

/// The receipt the network published for a transaction, out of the capture.
fn published_receipt(block: &NakamotoBlock, txid: Sha256Sum) -> Option<serde_json::Value> {
    let name = format!(
        "{:08}-{}.json",
        block.header.chain_length,
        block.header.block_hash()
    );
    let event: serde_json::Value =
        serde_json::from_slice(&fs::read(fixtures().join("events/new_block").join(name)).ok()?)
            .ok()?;
    event["transactions"]
        .as_array()?
        .iter()
        .find(|transaction| {
            transaction["txid"].as_str().map(|id| id.trim_start_matches("0x"))
                == Some(&txid.to_string())
        })
        .cloned()
}

/// Post a transaction to `/v2/transactions` over a real socket.
///
/// A served listener rather than the router in process, because "posted to the
/// public RPC" is the claim and a client is what makes it.
async fn post_to_the_rpc(state: RpcState, transaction: &Transaction) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the RPC");
    let address = listener.local_addr().expect("an address");
    let served = tokio::spawn(async move { nano_rpc::serve(listener, state).await });
    let answer = reqwest::Client::new()
        .post(format!("http://{address}/v2/transactions"))
        .header("content-type", "application/octet-stream")
        .body(transaction.encode())
        .send()
        .await
        .expect("the route answers");
    assert_eq!(answer.status(), reqwest::StatusCode::OK);
    assert_eq!(
        answer.json::<serde_json::Value>().await.ok(),
        Some(serde_json::json!(transaction.txid().to_string())),
        "the route did not answer with the transaction identifier"
    );
    served.abort();
}

/// The accounts a mempool has to be able to ask about, read out of the state.
fn tip_accounts(chainstate: &mut ChainState, transaction: &Transaction) -> HashMap<nano_address::StacksAddress, Account> {
    let mut accounts = HashMap::new();
    if let Some(address) = transaction.origin_address()
        && let Ok(principal) = clarity::vm::types::PrincipalData::parse(&address.to_string())
        && let Ok(entry) = chainstate.account(&principal)
    {
        accounts.insert(
            address,
            Account {
                nonce: entry.nonce,
                balance: Some(entry.balance),
            },
        );
    }
    accounts
}

/// The chain replayed to just below the block being replaced, so the state the
/// transaction is judged and executed against is the state the network judged it
/// against.
fn replay_below(fixtures: &Path, position: usize) -> Option<ChainState> {
    let Ok((mut chainstate, source)) = nano_conformance::replay_chainstate(fixtures) else {
        nano_conformance::skip_gate("the captured checkpoint does not open");
        return None;
    };
    let depth = nano_conformance::replay_into(
        &mut chainstate,
        source,
        fixtures,
        FixtureManifest {
            mode: FixtureMode::Captured,
            replay_blocks: position as u64,
            receipts: true,
        },
        0,
        &mut |_, _| {},
    );
    assert_eq!(
        depth.completed, position as u64,
        "the replay stopped early: {:?}",
        depth.first_divergence
    );
    Some(chainstate)
}

#[tokio::test]
async fn a_transaction_posted_to_the_rpc_is_mined_and_executed_by_this_node() {
    let fixtures = fixtures();
    let Some((position, dropped)) = first_mempool_block() else {
        nano_conformance::skip_gate("the capture holds no mid-tenure block of mempool transactions");
        return;
    };
    let Some(chainstate) = replay_below(&fixtures, position) else {
        return;
    };
    let transaction = dropped
        .transactions
        .iter()
        .find(|transaction| comes_via_the_mempool(transaction))
        .expect("the block carries a mempool transaction")
        .clone();
    let txid = transaction.txid();
    let published = published_receipt(&dropped, txid).expect("the capture published its receipt");

    // One chainstate, two roles, the way the node holds it: the RPC reads
    // accounts through it and the miner builds on it, under one lock.
    let shared = Arc::new(Mutex::new(chainstate));
    let mempool = Arc::new(Mutex::new(Mempool::new(nano_conformance::captured_network(
        &fixtures,
    ))));
    let state = RpcState::new(nano_conformance::captured_network(&fixtures))
        .with_chain(shared.clone() as Arc<Mutex<dyn ChainAccess>>)
        .with_mempool(mempool.clone());

    post_to_the_rpc(state, &transaction).await;
    assert!(
        mempool.lock().await.contains(txid),
        "the route answered but the pool the miner reads does not hold it"
    );

    // Selected: by the pool, against the accounts at this node's own tip. This is
    // the step that makes admission mean anything — a transaction held and never
    // offered to a block is a black hole with a 200.
    let mut chainstate = shared.lock().await;
    let accounts = tip_accounts(&mut chainstate, &transaction);
    let selected = mempool.lock().await.candidates(&accounts);
    assert!(
        selected.iter().any(|candidate| candidate.txid() == txid),
        "the pool did not offer the transaction to a block"
    );

    // Mined: nano's own block, in the place of the one that was dropped, with the
    // transaction coming from the pool rather than from the capture.
    let snapshots = nano_conformance::captured_bitcoin_snapshots(&fixtures)
        .expect("the captured burn contexts");
    let context = *snapshots
        .get(&dropped.header.consensus_hash.to_string())
        .expect("the burn context of the dropped block");
    let mut candidate = dropped.clone();
    candidate.transactions.clear();
    let parent = chainstate.tip();
    // A mid-tenure candidate is born empty and is filled by the pool, so the
    // emptiness that matters is the *assembled* block's. Both halves, because
    // this is exactly what was wrong: the check ran before the pool had its turn,
    // so every block a miner could build out of its mempool was refused for
    // carrying nothing — and moving the check has to leave the block a miner
    // should not publish still refused.
    let miner = nano_crypto::StacksPrivateKey::from_seed(b"nano miner");
    let nothing_offered = chainstate
        .assemble_nakamoto_block_selecting(context, &[], parent, candidate.clone(), &[], &miner);
    assert!(
        nothing_offered
            .err()
            .is_some_and(|error| error.to_string().contains("carries no transactions")),
        "a block whose pool offered nothing admissible was assembled anyway"
    );
    assert_eq!(
        parent.map(nano_primitives::StacksBlockId::from_bytes),
        Some(dropped.header.parent_block_id),
        "the replay did not stop on the parent of the block being replaced"
    );
    let (built, applied) = chainstate
        .assemble_nakamoto_block_selecting(
            context,
            &[],
            parent,
            candidate,
            &selected,
            &miner,
        )
        .expect("the block assembles");
    drop(chainstate);

    assert!(
        built.transactions.iter().any(|held| held.txid() == txid),
        "the assembled block does not carry the transaction the pool offered"
    );

    // Executed: nano's receipt for it, against the one the network published.
    let receipt = applied
        .receipts
        .iter()
        .find(|receipt| receipt.txid == txid)
        .expect("the block that carries it has a receipt for it");
    assert_eq!(
        receipt.status,
        TransactionStatus::Success,
        "the network's receipt said {}",
        published["status"]
    );
    assert_eq!(published["status"], serde_json::json!("success"));

    // Emitted: the payload an observer is sent names it, with the result and the
    // cost stacks-core published for the same execution.
    let payload = nano_rpc::new_block_payload(&built, &applied, &nano_rpc::BlockEventContext::default());
    let emitted = payload["transactions"]
        .as_array()
        .expect("the payload lists its transactions")
        .iter()
        .find(|entry| entry["txid"] == serde_json::json!(format!("0x{txid}")))
        .expect("the payload names the transaction this node mined")
        .clone();
    assert_eq!(emitted["status"], published["status"]);
    assert_eq!(emitted["raw_result"], published["raw_result"]);
    assert_eq!(
        emitted["execution_cost"], published["execution_cost"],
        "nano and stacks-core disagree about what the transaction cost"
    );
}
