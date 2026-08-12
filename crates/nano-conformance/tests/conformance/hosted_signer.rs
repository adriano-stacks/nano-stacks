//! What a stock `stacks-signer`, a transaction submitter and an event observer
//! get out of nano when nano is the only node they talk to.
//!
//! `hacknet_replacement` proves nano can sign for a network. This proves the
//! other direction, which is the one that says nano's RPC is the real thing: a
//! stock signer keeps no chain state and knows no peers, so every proposal it
//! reads, every verdict it acts on and every chunk it writes has to come out of
//! nano. Hacknet needs all three signatures for a block, so a chain that keeps
//! advancing with one signer hosted on nano is that signer having done all of it
//! through nano — and this test names which parts, off the chain and off nano's
//! own replicas rather than off a log line.
//!
//! Driven by `hacknet/harness.sh verify-hosted` against a live network.

use std::{
    collections::BTreeMap,
    env, fs,
    path::PathBuf,
    time::{Duration, Instant},
};

use nano_address::StacksAddress;
use nano_codec::{AnchorMode, Principal, Transaction, TransactionPayloadData, TransactionVersion};
use nano_crypto::{StacksPrivateKey, StacksPublicKey};
use nano_primitives::{Network, Sha256Sum, hash160};
use nano_stackerdb::{BlockAcceptance, BlockResponse, SignerMessage};
use nano_sync::SyncClient;
use reqwest::Url;
use serde_json::Value;

/// Hacknet is a testnet, and the chain identifier follows from that.
const NETWORK: Network = Network::TESTNET;

/// Where the hosted run put everything this test reads.
struct Hosted {
    /// nano's own RPC, which is the signer's only node.
    nano: Url,
    /// A participant nano did not replace, for the canonical chain.
    peer: Url,
    /// The key the hosted stock signer holds.
    signer_key: StacksPublicKey,
    /// Where nano's events were recorded, and where the stock nodes' were.
    nano_events: PathBuf,
    stock_events: PathBuf,
    /// A funded key, for the transaction a submitter posts.
    funded: Option<StacksPrivateKey>,
}

impl Hosted {
    /// Read the run out of the environment, or say what is missing.
    fn from_env() -> Option<Self> {
        let url = |name: &str| {
            env::var(name)
                .ok()
                .map(|value| Url::parse(&value).unwrap_or_else(|_| panic!("{name} must be a URL")))
        };
        let key = env::var("NANO_HOSTED_SIGNER_PUBLIC_KEY").ok()?;
        Some(Self {
            nano: url("NANO_HOSTED_RPC")?,
            peer: url("NANO_HACKNET_PEER")?,
            signer_key: StacksPublicKey::from_bytes(
                &hex::decode(key.trim()).expect("the hosted signer key must be hexadecimal"),
            )
            .expect("the hosted signer key must be a public key"),
            nano_events: PathBuf::from(env::var("NANO_EVENT_DIR").ok()?),
            stock_events: PathBuf::from(env::var("NANO_STOCK_EVENT_DIR").ok()?),
            funded: env::var("NANO_FUNDED_KEY")
                .ok()
                .map(|key| private_key(key.trim())),
        })
    }
}

/// A Stacks private key as the tooling writes it: 32 bytes, and a trailing
/// `01` meaning the public key is used compressed, which nano always does.
fn private_key(hex: &str) -> StacksPrivateKey {
    let bytes = hex::decode(hex.strip_suffix("01").unwrap_or(hex))
        .expect("a private key must be hexadecimal");
    StacksPrivateKey::from_bytes(
        <[u8; 32]>::try_from(bytes.as_slice()).expect("a private key is 32 bytes"),
    )
    .expect("a private key must be on the curve")
}

fn hosted() -> Option<Hosted> {
    let run = Hosted::from_env();
    if run.is_none() {
        nano_conformance::skip_gate(
            "a hosted run needs NANO_HOSTED_RPC, NANO_HACKNET_PEER, \
             NANO_HOSTED_SIGNER_PUBLIC_KEY, NANO_EVENT_DIR and NANO_STOCK_EVENT_DIR: \
             run hacknet/harness.sh verify-hosted",
        );
    }
    run
}

const fn require_accepted_response(accepted: Option<BlockAcceptance>) -> BlockAcceptance {
    accepted.expect(
        "the hosted signer accepted no block through nano after the live gate ran: rejected \
         responses are evidence of refusal, not acceptance",
    )
}

#[test]
#[should_panic(expected = "the hosted signer accepted no block through nano")]
fn a_live_hosted_signer_run_without_an_acceptance_is_not_success() {
    let _ = require_accepted_response(None);
}

/// The reward cycle nano says it is in, read from nano rather than from a peer.
///
/// From `/v2/pox` and not `/v3/tenures/info`, because the second answers only for
/// a tip the followed view still reaches and the first answers from the executed
/// tip alone — and it is the route the hosted signer reads it from too.
async fn active_cycle(nano: &Url) -> u64 {
    let url = nano.join("v2/pox").expect("the PoX URL");
    let pox: Value = reqwest::get(url)
        .await
        .expect("nano answers for its PoX calendar")
        .json()
        .await
        .expect("nano's PoX calendar decodes");
    pox["current_cycle"]["id"]
        .as_u64()
        .expect("the PoX calendar names the current cycle")
}

/// A stock signer holds a slot in nano's replica and answers proposals there.
///
/// Three things at once, and the order is the argument: nano derived the reward
/// set from its own state, so the slot exists; the chunk in that slot decodes to
/// a `BlockResponse` signed by the hosted signer, so the signer got a proposal
/// out of nano and a verdict it was willing to act on; and the block that
/// response names is canonical with that very signature in its header, so nano
/// carried the answer to the miner counting it.
#[tokio::test]
#[ignore = "requires a Hacknet network with one participant's signer hosted on nano"]
async fn a_stock_signer_answers_proposals_through_nano() {
    let Some(run) = hosted() else { return };
    let nano = SyncClient::new(run.nano.clone()).expect("a client for nano");
    let peer = SyncClient::new(run.peer.clone()).expect("a client for the peer");
    let cycle = active_cycle(&run.nano).await;

    let set = nano
        .stacker_set(cycle)
        .await
        .expect("nano serves the reward set it derived")
        .signer_set;
    let expected = run.signer_key.to_bytes_compressed();
    let slot = set
        .signers()
        .iter()
        .position(|signer| signer.public_key.to_bytes_compressed() == expected)
        .expect("the hosted signer holds a slot in the set nano derived");
    println!(
        "nano derived cycle {cycle} from its own state and gives the hosted signer slot {slot} \
         of {}",
        set.signers().len()
    );

    // What nano *took over its own API*, and not what it holds: nano also pulls
    // its peer's chunks into the same replica, and the hosted signer shares its
    // key with the stock signer it replaced — so a chunk read out of the replica
    // could be either one's. A `stackerdb_chunks` event is dispatched only where a
    // chunk was POSTed to nano, and nothing but the hosted signer POSTs to nano,
    // so the events are the only evidence that distinguishes them.
    let posted = chunks_nano_took(&run.nano_events);
    assert!(
        !posted.is_empty(),
        "nano took no chunk from the signer it hosts: {} holds the events",
        run.nano_events.join("stackerdb_chunks").display()
    );
    let mut kinds: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut accepted = None;
    for (contract, chunk) in &posted {
        let Ok(message) = SignerMessage::decode(&chunk.data) else {
            continue;
        };
        let kind = match &message {
            SignerMessage::BlockProposal(_) => "block proposal",
            SignerMessage::BlockPushed(_) => "pushed block",
            SignerMessage::StateMachineUpdate(_) => "state machine update",
            SignerMessage::BlockPreCommit(_) => "block pre-commit",
            SignerMessage::BlockResponse(response) => match response {
                BlockResponse::Accepted(acceptance) => {
                    accepted = Some(acceptance.clone());
                    "accepted block response"
                }
                BlockResponse::Rejected(rejection) => {
                    println!(
                        "the hosted signer rejected block {} through nano: {}",
                        rejection.signer_signature_hash, rejection.reason
                    );
                    "rejected block response"
                }
            },
        };
        *kinds.entry(kind).or_default() += 1;
        // Every one of them is authenticated against the writer nano assigned the
        // slot, which is the hosted signer's own key.
        assert!(
            chunk.verify(hash160(&expected)).unwrap_or(false),
            "a chunk nano took for {contract} was not signed by the signer it hosts"
        );
    }
    println!(
        "nano took {} chunks from the signer it hosts: {kinds:?}",
        posted.len()
    );

    let acceptance = require_accepted_response(accepted);
    let recovered = acceptance
        .signature
        .recover(acceptance.signer_signature_hash.as_bytes())
        .expect("the acceptance carries a recoverable signature");
    assert_eq!(
        recovered.to_bytes_compressed(),
        expected,
        "the acceptance nano took was not written by the signer it hosts"
    );
    println!(
        "the hosted signer accepted block {} through nano, reporting itself as {}",
        acceptance.signer_signature_hash, acceptance.server_version
    );
    // And the network counted it: the signature is in the header of a block the
    // canonical chain carries, which only happens if nano passed the chunk on.
    assert!(
        canonical_block_with(&peer, acceptance.signer_signature_hash).await,
        "block {} never became canonical, so nano did not carry the response to the miner",
        acceptance.signer_signature_hash
    );
}

/// Every chunk nano took over its own `/v2/stackerdb` route, from the events it
/// dispatched for them.
fn chunks_nano_took(events: &std::path::Path) -> Vec<(String, nano_stackerdb::Chunk)> {
    let Ok(entries) = fs::read_dir(events.join("stackerdb_chunks")) else {
        return Vec::new();
    };
    let mut taken = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let Ok(payload) = fs::read(entry.path()) else {
            continue;
        };
        let Ok(payload) = serde_json::from_slice::<Value>(&payload) else {
            continue;
        };
        let contract = payload["contract_id"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        for slot in payload["modified_slots"].as_array().unwrap_or(&Vec::new()) {
            let signature: Option<[u8; 65]> = hex::decode(
                slot["sig"]
                    .as_str()
                    .unwrap_or_default()
                    .trim_start_matches("0x"),
            )
            .ok()
            .and_then(|bytes| bytes.try_into().ok());
            let (Some(signature), Some(data)) = (
                signature,
                hex::decode(slot["data"].as_str().unwrap_or_default()).ok(),
            ) else {
                continue;
            };
            taken.push((
                contract.clone(),
                nano_stackerdb::Chunk {
                    slot_id: u32::try_from(slot["slot_id"].as_u64().unwrap_or_default())
                        .unwrap_or_default(),
                    slot_version: u32::try_from(slot["slot_version"].as_u64().unwrap_or_default())
                        .unwrap_or_default(),
                    signature: nano_crypto::MessageSignature::from_bytes(signature),
                    data,
                },
            ));
        }
    }
    taken
}

/// Walk the canonical chain back looking for a block, and the signature in it.
async fn canonical_block_with(peer: &SyncClient, digest: Sha256Sum) -> bool {
    let deadline = Instant::now() + Duration::from_mins(5);
    loop {
        let tip = peer.tenure_info().await.expect("the peer reports its tip");
        let mut block_id = tip.tip_block_id;
        for _ in 0..200 {
            let Ok(block) = peer.block(block_id).await else {
                break;
            };
            if block.header.signer_signature_hash() == digest {
                return true;
            }
            if block.header.chain_length == 0 {
                break;
            }
            block_id = block.header.parent_block_id;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// A transaction posted to nano is admitted by nano and reaches the network.
///
/// Admission is nano's own answer and mining is the network's answer. Both are
/// required: a follower that accepts a transaction but never relays it has not
/// completed the client journey.
#[tokio::test]
#[ignore = "requires a Hacknet network with one participant's signer hosted on nano"]
async fn a_transaction_posted_to_nano_is_admitted_and_reported() {
    let Some(run) = hosted() else { return };
    let Some(funded) = run.funded.clone() else {
        nano_conformance::skip_gate("a submitted transaction needs NANO_FUNDED_KEY");
        return;
    };
    let nano = SyncClient::new(run.nano.clone()).expect("a client for nano");
    let sender = StacksAddress::single_signature(
        hash160(&funded.public_key().to_bytes_compressed()),
        NETWORK.is_mainnet(),
    );
    let nonce = nano
        .account_nonce(sender)
        .await
        .expect("nano serves the sender's account");
    let transaction = Transaction::sign_standard(
        TransactionVersion::Testnet,
        NETWORK.chain_id(),
        AnchorMode::OnChainOnly,
        &funded,
        nonce,
        // Above Hacknet's minimum, and small enough not to matter.
        2_000,
        // Not to itself: a self-transfer is refused by the pool on both sides,
        // which would test the refusal rather than the admission.
        TransactionPayloadData::TokenTransfer {
            recipient: Principal::Standard(StacksAddress::single_signature(
                nano_primitives::Hash160::from_bytes([9; 20]),
                NETWORK.is_mainnet(),
            )),
            amount: 1,
            memo: [0; 34],
        },
    )
    .expect("sign the transfer");
    let txid = transaction.txid().to_string();

    let posted = reqwest::Client::new()
        .post(run.nano.join("v2/transactions").expect("the submit URL"))
        .header("content-type", "application/octet-stream")
        .body(transaction.encode())
        .send()
        .await
        .expect("post the transaction to nano");
    let status = posted.status();
    let body = posted.text().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "nano refused the transaction it should have admitted: {status} {body}"
    );
    assert!(
        body.contains(&txid),
        "nano answered {body} for the transaction {txid} it admitted"
    );
    println!("nano admitted and relayed {txid}, answering {status}");

    let mined = mined_by_the_network(&run, &txid).await;
    assert!(
        mined,
        "the network did not mine {txid} or nano's observer received no receipt for it"
    );
    println!("the network mined {txid} and nano's observer received its receipt");
}

/// Whether the indexer and nano's own observer both have the transaction.
async fn mined_by_the_network(run: &Hosted, txid: &str) -> bool {
    let api = env::var("NANO_HACKNET_API").unwrap_or_else(|_| "http://127.0.0.1:3999/".to_owned());
    let Ok(url) = Url::parse(&api).and_then(|api| api.join(&format!("extended/v1/tx/0x{txid}")))
    else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_mins(3);
    while Instant::now() < deadline {
        if let Ok(response) = reqwest::get(url.clone()).await
            && let Ok(body) = response.json::<Value>().await
            && body.get("tx_status").and_then(Value::as_str) == Some("success")
            && observer_has_transaction(&run.nano_events, txid)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    false
}

fn observer_has_transaction(events: &std::path::Path, txid: &str) -> bool {
    let Ok(entries) = fs::read_dir(events.join("new_block")) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let Ok(payload) = fs::read(entry.path()) else {
            return false;
        };
        let Ok(payload) = serde_json::from_slice::<Value>(&payload) else {
            return false;
        };
        payload["transactions"]
            .as_array()
            .is_some_and(|transactions| {
                transactions.iter().any(|receipt| {
                    receipt["txid"]
                        .as_str()
                        .is_some_and(|observed| observed.trim_start_matches("0x") == txid)
                })
            })
    })
}

/// An observer on nano is told the same things stacks-core tells its own.
///
/// Every event kind the node is supposed to announce has to have arrived, and for
/// a block both nodes executed the receipts have to agree — transaction for
/// transaction, on status, result and cost. A payload that merely arrives says
/// nothing about whether it describes the same execution.
#[test]
#[ignore = "requires a Hacknet network with one participant's signer hosted on nano"]
fn an_observer_on_nano_is_told_what_stacks_core_tells_its_own() {
    let Some(run) = hosted() else { return };
    for event in ["new_block", "new_burn_block", "stackerdb_chunks"] {
        let directory = run.nano_events.join(event);
        let count = fs::read_dir(&directory)
            .map(|entries| entries.filter_map(Result::ok).count())
            .unwrap_or_default();
        assert!(
            count > 0,
            "nano's observer received no {event}: {}",
            directory.display()
        );
        println!("nano's observer received {count} {event} events");
    }

    let ours = blocks_by_hash(&run.nano_events.join("new_block"));
    let theirs = blocks_by_hash(&run.stock_events.join("new_block"));
    let shared: Vec<&String> = ours
        .keys()
        .filter(|hash| theirs.contains_key(*hash))
        .collect();
    assert!(
        !shared.is_empty(),
        "nano and stacks-core announced no block in common: {} of ours, {} of theirs",
        ours.len(),
        theirs.len()
    );
    for hash in &shared {
        compare_receipts(&ours[*hash], &theirs[*hash], hash);
    }
    println!(
        "the receipts agree for all {} blocks both observers were told about",
        shared.len()
    );
}

/// Every `new_block` a directory holds, by the block hash it names.
fn blocks_by_hash(directory: &PathBuf) -> BTreeMap<String, Value> {
    let Ok(entries) = fs::read_dir(directory) else {
        return BTreeMap::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let payload: Value = serde_json::from_slice(&fs::read(entry.path()).ok()?).ok()?;
            let hash = payload.get("block_hash")?.as_str()?.to_owned();
            Some((hash, payload))
        })
        .collect()
}

/// The transaction receipts of one block, as the two observers were told them.
fn compare_receipts(ours: &Value, theirs: &Value, hash: &str) {
    for field in [
        "block_height",
        "index_block_hash",
        "parent_index_block_hash",
    ] {
        assert_eq!(
            ours.get(field),
            theirs.get(field),
            "{field} differs for block {hash}"
        );
    }
    let receipts = |payload: &Value| {
        payload["transactions"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .map(|receipt| {
                (
                    receipt["txid"].as_str().unwrap_or_default().to_owned(),
                    receipt["status"].clone(),
                    receipt["raw_result"].clone(),
                    receipt["execution_cost"].clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        receipts(ours),
        receipts(theirs),
        "the receipts nano announced for block {hash} are not the ones stacks-core announced"
    );
}
