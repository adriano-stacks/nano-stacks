//! What nano's mempool must accept, refuse and order the way stacks-core does.
//!
//! The refusals a node hands back are user-visible, so they are checked against
//! stacks-core's own admission path (`StacksChainState::will_admit_mempool_tx`)
//! rather than against a transcription of it, and the ordering is checked
//! against the transactions six hundred captured blocks actually carried.

use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use blockstack_lib::chainstate::stacks::db::StacksChainState;
use blockstack_lib::chainstate::stacks::db::blocks::{
    MINIMUM_TX_FEE, MINIMUM_TX_FEE_RATE_PER_BYTE,
};
use blockstack_lib::chainstate::stacks::db::testing::TestChainstateBuilder;
use blockstack_lib::chainstate::stacks::{MAX_BLOCK_LEN, StacksTransaction};
use blockstack_lib::core::mempool::{
    MAXIMUM_MEMPOOL_TX_CHAINING, MEMPOOL_NAKAMOTO_MAX_TRANSACTION_AGE,
};
use clarity::vm::database::NULL_BURN_STATE_DB;
use nano_address::StacksAddress;
use nano_chainstate::NakamotoBlock;
use nano_codec::{AnchorMode, Principal, Transaction, TransactionPayloadData, TransactionVersion};
use nano_conformance::captured_network;
use nano_crypto::StacksPrivateKey;
use nano_mempool::{Account, ChainTip, Mempool, Rejection};
use nano_primitives::{Network, hash160};
use serde_json::Value;
use stacks_common::codec::StacksMessageCodec;
use stacks_common::consts::{FIRST_BURNCHAIN_CONSENSUS_HASH, FIRST_STACKS_BLOCK_HASH};
use stacks_common::types::chainstate::StacksAddress as ReferenceStacksAddress;
use stacks_common::util::hash::Hash160 as ReferenceHash160;

const NETWORK: Network = Network::TESTNET;

/// Enough to pay every fee these tests charge and make every transfer they make.
const FUNDED: u64 = 1_000_000;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn key(seed: &[u8]) -> StacksPrivateKey {
    StacksPrivateKey::from_seed(seed)
}

fn address(key: &StacksPrivateKey) -> StacksAddress {
    StacksAddress::single_signature(
        hash160(&key.public_key().to_bytes_compressed()),
        NETWORK.is_mainnet(),
    )
}

fn reference_address(address: StacksAddress) -> ReferenceStacksAddress {
    ReferenceStacksAddress::new(
        address.version(),
        ReferenceHash160(*address.hash160().as_bytes()),
    )
    .expect("protocol address versions are valid")
}

fn sign(
    key: &StacksPrivateKey,
    version: TransactionVersion,
    nonce: u64,
    fee: u64,
    payload: TransactionPayloadData,
) -> Transaction {
    Transaction::sign_standard(
        version,
        NETWORK.chain_id(),
        AnchorMode::OnChainOnly,
        key,
        nonce,
        fee,
        payload,
    )
    .expect("sign the transaction")
}

const fn transfer_to(recipient: StacksAddress, amount: u64) -> TransactionPayloadData {
    TransactionPayloadData::TokenTransfer {
        recipient: Principal::Standard(recipient),
        amount,
        memo: [0; 34],
    }
}

const fn account(nonce: u64, balance: u128) -> Account {
    Account {
        nonce,
        balance: Some(balance),
    }
}

/// The refusal without its free-form message, which is prose on both sides and
/// carries none of the contract a wallet reads.
fn shape(mut body: Value) -> Value {
    if let Some(data) = body.get_mut("reason_data").and_then(Value::as_object_mut) {
        data.remove("message");
    }
    body
}

fn reference_rejection(
    chainstate: &mut StacksChainState,
    transaction: &Transaction,
) -> Option<Value> {
    let bytes = transaction.encode();
    let reference = StacksTransaction::consensus_deserialize(&mut Cursor::new(&bytes))
        .expect("stacks-core decodes what nano signed");
    let txid = reference.txid();
    chainstate
        .will_admit_mempool_tx(
            &NULL_BURN_STATE_DB,
            &FIRST_BURNCHAIN_CONSENSUS_HASH,
            &FIRST_STACKS_BLOCK_HASH,
            &reference,
            bytes.len() as u64,
        )
        .err()
        .map(|rejection| shape(rejection.into_json(&txid)))
}

fn nano_rejection(
    tip: &HashMap<StacksAddress, Account>,
    transaction: &Transaction,
) -> Option<Value> {
    // A pool of its own per transaction keeps the replacement rule, which
    // stacks-core applies in a later step, out of the comparison.
    Mempool::new(NETWORK)
        .submit(transaction.clone(), tip, 0)
        .err()
        .map(|rejection| shape(rejection.into_json(transaction.txid())))
}

fn captured_blocks() -> Vec<NakamotoBlock> {
    let mut paths: Vec<_> = fs::read_dir(fixtures().join("nakamoto/blocks"))
        .expect("read fixture blocks")
        .map(|entry| entry.expect("fixture entry").path())
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|path| {
            NakamotoBlock::decode(&fs::read(path).expect("read fixture block"))
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        })
        .collect()
}

#[test]
fn mempool_limits_match_stacks_core() {
    assert_eq!(
        nano_mempool::MAXIMUM_MEMPOOL_TX_CHAINING,
        MAXIMUM_MEMPOOL_TX_CHAINING
    );
    assert_eq!(nano_mempool::MINIMUM_TX_FEE, MINIMUM_TX_FEE);
    assert_eq!(
        nano_mempool::MINIMUM_TX_FEE_RATE_PER_BYTE,
        MINIMUM_TX_FEE_RATE_PER_BYTE
    );
    assert_eq!(
        nano_mempool::MAX_TRANSACTION_AGE_SECS,
        MEMPOOL_NAKAMOTO_MAX_TRANSACTION_AGE.as_secs()
    );
    assert_eq!(nano_mempool::MAX_BLOCK_LEN, u64::from(MAX_BLOCK_LEN));
}

#[test]
#[allow(clippy::too_many_lines)]
fn admission_refuses_what_stacks_core_refuses_for_the_same_reason() {
    let sender = key(b"conformance mempool sender");
    let pauper = key(b"conformance mempool pauper");
    let recipient = key(b"conformance mempool recipient");
    let sender_address = address(&sender);
    let pauper_address = address(&pauper);
    let recipient_address = address(&recipient);

    let mut chainstate = TestChainstateBuilder::new_testnet("nano_conformance_mempool")
        .with_balances(vec![(reference_address(sender_address), FUNDED)])
        .build();
    let tip: HashMap<_, _> = [
        (sender_address, account(0, u128::from(FUNDED))),
        (pauper_address, account(0, 0)),
    ]
    .into_iter()
    .collect();

    let testnet = TransactionVersion::for_network(NETWORK);
    let mut tampered = sign(&sender, testnet, 0, 200, transfer_to(recipient_address, 1)).encode();
    let last = tampered.len() - 1;
    tampered[last] ^= 1;

    let cases = vec![
        (
            "a funded transfer at the next nonce",
            sign(&sender, testnet, 0, 200, transfer_to(recipient_address, 1)),
        ),
        (
            "the last nonce chaining allows",
            sign(
                &sender,
                testnet,
                1 + MAXIMUM_MEMPOOL_TX_CHAINING,
                200,
                transfer_to(recipient_address, 1),
            ),
        ),
        (
            "one nonce past what chaining allows",
            sign(
                &sender,
                testnet,
                2 + MAXIMUM_MEMPOOL_TX_CHAINING,
                200,
                transfer_to(recipient_address, 1),
            ),
        ),
        (
            "a fee below one microSTX per byte",
            sign(&sender, testnet, 0, 1, transfer_to(recipient_address, 1)),
        ),
        (
            "a transfer to the sender",
            sign(&sender, testnet, 0, 200, transfer_to(sender_address, 1)),
        ),
        (
            "a transfer of nothing",
            sign(&sender, testnet, 0, 200, transfer_to(recipient_address, 0)),
        ),
        (
            "a transfer carrying the mainnet version byte",
            sign(
                &sender,
                TransactionVersion::Mainnet,
                0,
                200,
                transfer_to(recipient_address, 1),
            ),
        ),
        (
            "a transfer from an account with nothing in it",
            sign(&pauper, testnet, 0, 200, transfer_to(recipient_address, 1)),
        ),
        (
            "a contract deploy from an account with nothing in it",
            sign(
                &pauper,
                testnet,
                0,
                200,
                TransactionPayloadData::SmartContract {
                    contract_name: "nothing".to_owned(),
                    source: "(define-read-only (nothing) u1)".to_owned(),
                },
            ),
        ),
        (
            "a transaction whose signature was altered",
            Transaction::decode(&tampered)
                .expect("the altered transaction still decodes")
                .0,
        ),
        (
            "a coinbase",
            sign(
                &sender,
                testnet,
                0,
                200,
                TransactionPayloadData::Coinbase { payload: [0; 32] },
            ),
        ),
    ];

    for (description, transaction) in cases {
        assert_eq!(
            nano_rejection(&tip, &transaction),
            reference_rejection(&mut chainstate, &transaction),
            "{description}",
        );
    }
}

/// stacks-core reads the tip's Clarity state while admitting and nano does not,
/// so a call to a contract that was never deployed is refused there and held
/// here until assembly runs it and leaves it out of the block.
#[test]
fn a_call_to_a_missing_contract_is_the_one_refusal_nano_defers() {
    let sender = key(b"conformance mempool caller");
    let sender_address = address(&sender);
    let mut chainstate = TestChainstateBuilder::new_testnet("nano_conformance_mempool_call")
        .with_balances(vec![(reference_address(sender_address), FUNDED)])
        .build();
    let tip: HashMap<_, _> =
        std::iter::once((sender_address, account(0, u128::from(FUNDED)))).collect();

    let call = sign(
        &sender,
        TransactionVersion::for_network(NETWORK),
        0,
        200,
        TransactionPayloadData::ContractCall {
            address: sender_address,
            contract_name: "never-deployed".to_owned(),
            function_name: "nothing".to_owned(),
            arguments: Vec::new(),
        },
    );
    assert_eq!(
        reference_rejection(&mut chainstate, &call).expect("stacks-core refuses the call")["reason"],
        "NoSuchContract"
    );
    assert_eq!(nano_rejection(&tip, &call), None);
}

/// A captured transaction is one the network accepted, so the only thing that
/// may turn it away here is a term the mempool does not offer that consensus
/// does: a coinbase or tenure change, which no node takes by submission, and a
/// fee a miner was willing to carry for nothing.
#[test]
fn every_captured_transaction_is_held_or_refused_on_a_mempool_only_term() {
    let network = captured_network(&fixtures());
    let mut held = 0_usize;
    for block in captured_blocks() {
        for transaction in &block.transactions {
            let origin = transaction
                .origin_address()
                .expect("a captured transaction names its origin");
            let tip: HashMap<_, _> = std::iter::once((
                origin,
                account(transaction.auth().origin().nonce(), u128::MAX),
            ))
            .collect();
            match Mempool::new(network).submit(transaction.clone(), &tip, 0) {
                Ok(_) => held += 1,
                Err(Rejection::NoCoinbaseViaMempool | Rejection::NoTenureChangeViaMempool) => {}
                Err(Rejection::FeeTooLow { actual, expected }) => {
                    assert_eq!(actual, transaction.auth().payer().fee());
                    assert!(actual < expected);
                }
                Err(rejection) => panic!("{}: {rejection}", transaction.txid()),
            }
        }
    }
    assert!(held > 0, "the captured blocks carry no user transactions");
}

#[test]
fn a_block_is_filled_from_the_captured_transactions_by_fee_rate_and_in_nonce_order() {
    let network = captured_network(&fixtures());
    let mut mempool = Mempool::new(network);
    let mut tip: HashMap<StacksAddress, Account> = HashMap::new();
    for block in captured_blocks() {
        for transaction in &block.transactions {
            let Some(origin) = transaction.origin_address() else {
                continue;
            };
            let nonce = transaction.auth().origin().nonce();
            // A transaction could only have reached a pool while its account
            // was on the nonce it carries, so the tip holds the earliest one.
            let known = tip
                .entry(origin)
                .or_insert_with(|| account(nonce, u128::MAX));
            known.nonce = known.nonce.min(nonce);
            let _ = mempool.submit(transaction.clone(), &tip, 0);
        }
    }
    assert!(!mempool.is_empty(), "the captured blocks held nothing");

    let candidates = mempool.candidates(&tip);
    assert!(!candidates.is_empty());

    // Every account runs from the nonce the tip is on, one nonce at a time.
    let mut expected: HashMap<StacksAddress, u64> = HashMap::new();
    for transaction in &candidates {
        let origin = transaction.origin_address().expect("a named origin");
        let next = expected
            .entry(origin)
            .or_insert_with(|| tip.account(&origin).nonce);
        assert_eq!(transaction.auth().origin().nonce(), *next);
        *next = next.saturating_add(1);
    }

    // The first candidate pays the best rate of everything that was ready to
    // run before it, which is what ordering by fee rate means here.
    let ready = |transaction: &Transaction| {
        let origin = transaction.origin_address().expect("a named origin");
        transaction.auth().origin().nonce() == tip.account(&origin).nonce
    };
    let rate = |transaction: &Transaction| {
        u128::from(transaction.auth().payer().fee()) * 1_000_000
            / transaction.as_bytes().len() as u128
    };
    let best = candidates
        .iter()
        .filter(|transaction| ready(transaction))
        .map(rate)
        .max()
        .expect("at least one transaction is ready to run");
    assert_eq!(rate(&candidates[0]), best);
}
