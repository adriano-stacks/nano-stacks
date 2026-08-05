//! What a Hacknet network must keep doing while nano replaces one participant.
//!
//! The reward set Hacknet stacks gives each of its three signers equal weight,
//! and the approval threshold is seven tenths of the total, so no block is
//! accepted without every signer. A network that keeps producing blocks after
//! nano takes one participant over is therefore already proof that nano's
//! signature counted; the rest of this test names what those blocks contained,
//! so a passing run also shows transactions, contract deploys and calls,
//! tenure changes, sortitions and a PoX-5 cycle rollover being processed.
//!
//! Driven by `hacknet/harness.sh verify` against a live network:
//!
//! ```text
//! NANO_SIGNER_PUBLIC_KEY=<hex> cargo test -p nano-conformance \
//!     --test hacknet_replacement -- --ignored --nocapture
//! ```

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    time::{Duration, Instant},
};

use nano_chainstate::{NakamotoBlock, SignerSet};
use nano_codec::TransactionPayloadType;
use nano_crypto::StacksPublicKey;
use nano_sync::{SyncClient, TenureInfo};
use reqwest::Url;

/// Everything the run is expected to observe, and where to observe it.
struct Expectation {
    peer: Url,
    api: Url,
    nano_key: StacksPublicKey,
    /// Key nano mines with, when the participant it replaced also mines.
    nano_miner_key: Option<StacksPublicKey>,
    blocks: u64,
    cycles: u64,
    timeout: Duration,
    /// Seconds a frozen Stacks tip is tolerated before the run is a failure.
    stall: Duration,
}

impl Expectation {
    fn from_env() -> Self {
        let variable =
            |name: &str, fallback: &str| env::var(name).unwrap_or_else(|_| fallback.to_owned());
        let number = |name: &str, fallback: u64| {
            variable(name, &fallback.to_string())
                .parse()
                .unwrap_or_else(|_| panic!("{name} must be a number"))
        };
        let key = env::var("NANO_SIGNER_PUBLIC_KEY")
            .expect("NANO_SIGNER_PUBLIC_KEY must hold the replaced participant's signing key");
        Self {
            peer: Url::parse(&variable("NANO_HACKNET_PEER", "http://127.0.0.1:20443/"))
                .expect("NANO_HACKNET_PEER must be a URL"),
            api: Url::parse(&variable("NANO_HACKNET_API", "http://127.0.0.1:3999/"))
                .expect("NANO_HACKNET_API must be a URL"),
            nano_key: StacksPublicKey::from_bytes(
                &hex::decode(key.trim()).expect("NANO_SIGNER_PUBLIC_KEY must be hexadecimal"),
            )
            .expect("NANO_SIGNER_PUBLIC_KEY must be a public key"),
            nano_miner_key: env::var("NANO_MINER_PUBLIC_KEY").ok().map(|key| {
                StacksPublicKey::from_bytes(
                    &hex::decode(key.trim()).expect("NANO_MINER_PUBLIC_KEY must be hexadecimal"),
                )
                .expect("NANO_MINER_PUBLIC_KEY must be a public key")
            }),
            blocks: number("NANO_HACKNET_BLOCKS", 20),
            cycles: number("NANO_HACKNET_CYCLES", 1),
            timeout: Duration::from_secs(number("NANO_HACKNET_TIMEOUT_SECS", 1800)),
            stall: Duration::from_secs(number("NANO_HACKNET_STALL_SECS", 300)),
        }
    }
}

/// What the canonical chain gained while nano was the replaced participant.
#[derive(Default)]
struct Observed {
    blocks: Vec<NakamotoBlock>,
    payloads: BTreeMap<&'static str, Vec<String>>,
    miners: BTreeSet<String>,
    sortitions: u64,
}

#[tokio::test]
#[ignore = "requires a Hacknet network with one participant replaced by nano"]
async fn hacknet_keeps_working_with_a_nano_participant() {
    let expectation = Expectation::from_env();
    let client = SyncClient::new(expectation.peer.clone()).expect("create the sync client");
    let pox = client.pox_info().await.expect("read the PoX calendar");
    assert!(
        pox.pox_5_activation_height
            .is_some_and(|height| u64::from(height) <= pox.bitcoin_height),
        "PoX-5 is not active yet: {pox:?}"
    );

    let start = client
        .tenure_info()
        .await
        .expect("read the starting tenure");
    println!(
        "starting at Stacks height {} in reward cycle {}",
        start.tip_height, start.reward_cycle
    );
    let end = advance(&client, &expectation, &start).await;
    let observed = collect(&client, &start, &end).await;

    println!(
        "observed {} canonical blocks across cycles {}..={}",
        observed.blocks.len(),
        start.reward_cycle,
        end.reward_cycle
    );
    assert_signed_by_nano(&client, &expectation, &observed).await;
    assert_mined_by_nano(&expectation, &observed);
    assert_processed_everything(&expectation, &observed).await;
    assert_pox_5_cycle_holds_nano(&client, &expectation, &end).await;
}

/// Follow the peer until it has produced the expected blocks and cycles.
async fn advance(client: &SyncClient, expectation: &Expectation, start: &TenureInfo) -> TenureInfo {
    let deadline = Instant::now() + expectation.timeout;
    let mut progress = Instant::now();
    let mut highest = start.tip_height;
    loop {
        let tenure = client.tenure_info().await.expect("read the tenure");
        if tenure.tip_height > highest {
            highest = tenure.tip_height;
            progress = Instant::now();
        }
        let grown = tenure.tip_height.saturating_sub(start.tip_height);
        let cycles = tenure.reward_cycle.saturating_sub(start.reward_cycle);
        if grown >= expectation.blocks && cycles >= expectation.cycles {
            return tenure;
        }
        assert!(
            progress.elapsed() < expectation.stall,
            "the Stacks tip froze at height {highest} for {:?}: a replaced participant stalled the network",
            progress.elapsed()
        );
        assert!(
            Instant::now() < deadline,
            "only {grown} of {} blocks and {cycles} of {} cycles arrived before the timeout",
            expectation.blocks,
            expectation.cycles
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Walk the canonical chain back from the final tip to where the run started.
async fn collect(client: &SyncClient, start: &TenureInfo, end: &TenureInfo) -> Observed {
    let mut observed = Observed::default();
    let mut block_id = end.tip_block_id;
    while block_id != start.tip_block_id {
        let block = client.block(block_id).await.expect("download a block");
        block_id = block.header.parent_block_id;
        observed.blocks.push(block);
    }
    observed.blocks.reverse();

    let mut consensus_hashes = BTreeSet::new();
    for block in &observed.blocks {
        consensus_hashes.insert(block.header.consensus_hash);
        for transaction in &block.transactions {
            let kind = match transaction.payload_type() {
                TransactionPayloadType::TokenTransfer => "transfer",
                TransactionPayloadType::SmartContract
                | TransactionPayloadType::VersionedSmartContract => "deploy",
                TransactionPayloadType::ContractCall => "call",
                TransactionPayloadType::TenureChange => "tenure change",
                TransactionPayloadType::NakamotoCoinbase => "coinbase",
                _ => continue,
            };
            observed
                .payloads
                .entry(kind)
                .or_default()
                .push(transaction.txid().to_string());
        }
    }
    for consensus_hash in consensus_hashes {
        let sortition = client
            .sortition(consensus_hash)
            .await
            .expect("read the sortition of an observed tenure");
        if sortition.was_sortition {
            observed.sortitions += 1;
            if let Some(hash) = sortition.miner_public_key_hash {
                observed.miners.insert(hash.to_string());
            }
        }
    }
    observed
}

/// Every observed block carries nano's signature and a threshold signer set.
async fn assert_signed_by_nano(
    client: &SyncClient,
    expectation: &Expectation,
    observed: &Observed,
) {
    assert!(
        !observed.blocks.is_empty(),
        "no canonical blocks were observed"
    );
    let mut sets: BTreeMap<u64, SignerSet> = BTreeMap::new();
    let pox = client.pox_info().await.expect("read the PoX calendar");
    for block in &observed.blocks {
        let sortition = client
            .sortition(block.header.consensus_hash)
            .await
            .expect("read the sortition of an observed block");
        let cycle = pox.reward_cycle(sortition.bitcoin_height);
        if let std::collections::btree_map::Entry::Vacant(entry) = sets.entry(cycle) {
            entry.insert(
                client
                    .stacker_set(cycle)
                    .await
                    .expect("read the reward set of an observed cycle")
                    .signer_set,
            );
        }
        let set = &sets[&cycle];
        let weight = set.verify(&block.header).unwrap_or_else(|error| {
            panic!(
                "block {} carries an invalid signer set: {error}",
                block.block_id()
            )
        });
        let digest = block.header.signer_signature_hash();
        assert!(
            block
                .header
                .signer_signatures
                .iter()
                .any(|signature| signature.recover(digest.as_bytes()).as_ref()
                    == Ok(&expectation.nano_key)),
            "block {} at height {} was accepted without nano's signature",
            block.block_id(),
            block.header.chain_length
        );
        assert!(
            weight >= set.approval_threshold().expect("a weighted reward set"),
            "block {} was accepted below the approval threshold",
            block.block_id()
        );
    }
    println!(
        "every one of the {} blocks carries nano's signature",
        observed.blocks.len()
    );
}

/// Blocks nano mined itself, when it replaced a participant that also mines.
///
/// A miner signature only counts once the block carrying it is canonical, which
/// is what walking the chain back from the tip already established.
fn assert_mined_by_nano(expectation: &Expectation, observed: &Observed) {
    let Some(expected) = &expectation.nano_miner_key else {
        println!("no mining key was given, so only nano's signature was checked");
        return;
    };
    let mined = observed
        .blocks
        .iter()
        .filter(|block| {
            block
                .header
                .miner_signature
                .recover(block.header.miner_signature_hash().as_bytes())
                .as_ref()
                == Ok(expected)
        })
        .collect::<Vec<_>>();
    let heights = mined
        .iter()
        .map(|block| block.header.chain_length)
        .collect::<Vec<_>>();
    assert!(
        !mined.is_empty(),
        "nano mined none of the {} canonical blocks",
        observed.blocks.len()
    );
    println!(
        "nano mined {} of the {} canonical blocks, at heights {heights:?}",
        mined.len(),
        observed.blocks.len()
    );
}

/// The observed window contains each kind of work the network must keep doing.
async fn assert_processed_everything(expectation: &Expectation, observed: &Observed) {
    for kind in ["transfer", "deploy", "call", "tenure change", "coinbase"] {
        let transactions = observed
            .payloads
            .get(kind)
            .unwrap_or_else(|| panic!("no {kind} transaction was processed while nano signed"));
        let confirmed = confirm(expectation, &transactions[0]).await;
        println!(
            "{} {kind} transactions, including {} which the network reports as {confirmed}",
            transactions.len(),
            transactions[0]
        );
        assert_eq!(
            confirmed, "success",
            "the first {kind} transaction did not succeed"
        );
    }
    assert!(
        observed.sortitions > 0,
        "no observed tenure came from a Bitcoin sortition"
    );
    assert!(
        observed.miners.len() > 1,
        "only one miner won a sortition while nano signed: {:?}",
        observed.miners
    );
    println!(
        "{} sortitions across {} distinct miners",
        observed.sortitions,
        observed.miners.len()
    );
}

/// Ask the indexer for the receipt status a transaction settled with.
async fn confirm(expectation: &Expectation, txid: &str) -> String {
    let url = expectation
        .api
        .join(&format!("extended/v1/tx/0x{txid}"))
        .expect("build the transaction URL");
    let response: serde_json::Value = reqwest::get(url)
        .await
        .expect("query the transaction")
        .json()
        .await
        .expect("decode the transaction");
    response
        .get("tx_status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("missing")
        .to_owned()
}

/// The cycle the run ended in is a PoX-5 cycle that still holds nano's key.
async fn assert_pox_5_cycle_holds_nano(
    client: &SyncClient,
    expectation: &Expectation,
    end: &TenureInfo,
) {
    let set = client
        .stacker_set(end.reward_cycle)
        .await
        .expect("read the reward set of the final cycle");
    let expected = expectation.nano_key.to_bytes_compressed();
    let entry = set
        .signer_set
        .signers()
        .iter()
        .find(|signer| signer.public_key.to_bytes_compressed() == expected)
        .expect("nano's key is not in the reward set of the final cycle");
    assert!(entry.weight > 0, "nano holds no voting weight");
    assert!(
        set.sbtc_address.is_some(),
        "the final cycle is not a waterfall reward set: {set:?}"
    );
    println!(
        "reward cycle {} pays a waterfall set in which nano holds weight {} of {}",
        end.reward_cycle,
        entry.weight,
        set.signer_set.weights().iter().sum::<u32>()
    );
}
