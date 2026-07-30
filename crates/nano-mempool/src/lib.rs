#![forbid(unsafe_code)]

//! The transactions a node has accepted and not yet seen mined.
//!
//! Admission mirrors stacks-core's (`chainstate/stacks/db/blocks.rs`,
//! `can_include_tx`, and `core/mempool.rs`, `try_add_tx`): the checks run in the
//! same order and refuse with the same reason codes, because those codes are
//! what a wallet reads back from `/v2/transactions`.
//!
//! What nano cannot check it does not pretend to: the Clarity-level refusals
//! (`NoSuchContract`, `NoSuchPublicFunction`, `BadFunctionArgument`,
//! `ContractAlreadyExists`) need a read-only VM over the tip, which the mempool
//! has no handle on. Those transactions are admitted here and dropped during
//! assembly instead, where they do run.

use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap, HashSet},
    fmt,
    hash::BuildHasher,
};

use nano_address::StacksAddress;
use nano_codec::{Principal, Transaction, TransactionPayloadData, TransactionVersion};
use nano_primitives::{Network, Sha256Sum};
use serde_json::{Value, json};

/// Nonces a transaction may run ahead of its account's before the mempool
/// refuses to hold it (`core/mempool.rs`, `MAXIMUM_MEMPOOL_TX_CHAINING`).
pub const MAXIMUM_MEMPOOL_TX_CHAINING: u64 = 25;

/// The smallest fee a transaction may pay (`MINIMUM_TX_FEE`).
pub const MINIMUM_TX_FEE: u64 = 1;

/// The smallest fee a transaction may pay per serialized byte
/// (`MINIMUM_TX_FEE_RATE_PER_BYTE`).
pub const MINIMUM_TX_FEE_RATE_PER_BYTE: u64 = 1;

/// How long an unmined transaction is held before it is collected
/// (`MEMPOOL_NAKAMOTO_MAX_TRANSACTION_AGE`).
pub const MAX_TRANSACTION_AGE_SECS: u64 = 256 * 10 * 60;

/// Serialized transaction bytes a block may carry (`MAX_BLOCK_LEN`).
pub const MAX_BLOCK_LEN: u64 = 2 * 1024 * 1024;

/// What a chain tip says about one account.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Account {
    /// The next nonce this account's transactions must carry.
    pub nonce: u64,
    /// What the account can spend now, when the tip can say.
    ///
    /// A tip that only knows nonces reports `None`, and the fee it cannot
    /// price is left to execution rather than guessed at.
    pub balance: Option<u128>,
}

/// The chain state a transaction is judged against.
pub trait ChainTip {
    /// The account state at this tip, which is the zero account when unknown.
    fn account(&self, address: &StacksAddress) -> Account;
}

impl<S: BuildHasher> ChainTip for HashMap<StacksAddress, Account, S> {
    fn account(&self, address: &StacksAddress) -> Account {
        self.get(address).copied().unwrap_or_default()
    }
}

/// An account nonce a transaction did not match.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NonceMismatch {
    pub expected: u64,
    pub actual: u64,
    pub principal: StacksAddress,
    pub is_origin: bool,
}

impl NonceMismatch {
    fn reason_data(&self) -> Value {
        json!({
            "expected": self.expected,
            "actual": self.actual,
            "principal": self.principal.to_string(),
            "is_origin": self.is_origin,
        })
    }
}

/// Why a node refused a transaction.
///
/// The reason codes are user-visible, so they are stacks-core's own
/// (`MemPoolRejection::into_json`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Rejection {
    BadTransactionVersion,
    SignatureValidation(String),
    FeeTooLow { expected: u64, actual: u64 },
    BadNonce(NonceMismatch),
    TooMuchChaining(NonceMismatch),
    NotEnoughFunds { expected: u128, actual: u128 },
    ConflictingNonceInMempool,
    TransferRecipientCannotEqualSender(StacksAddress),
    TransferAmountMustBePositive,
    BadAddressVersionByte,
    NoCoinbaseViaMempool,
    NoTenureChangeViaMempool,
    Other(String),
}

impl Rejection {
    /// The `reason` a rejected submission reports.
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::BadTransactionVersion => "BadTransactionVersion",
            Self::SignatureValidation(_) => "SignatureValidation",
            Self::FeeTooLow { .. } => "FeeTooLow",
            Self::BadNonce(_) => "BadNonce",
            Self::TooMuchChaining(_) => "TooMuchChaining",
            Self::NotEnoughFunds { .. } => "NotEnoughFunds",
            Self::ConflictingNonceInMempool => "ConflictingNonceInMempool",
            Self::TransferRecipientCannotEqualSender(_) => "TransferRecipientCannotEqualSender",
            Self::TransferAmountMustBePositive => "TransferAmountMustBePositive",
            Self::BadAddressVersionByte => "BadAddressVersionByte",
            Self::NoCoinbaseViaMempool => "NoCoinbaseViaMempool",
            Self::NoTenureChangeViaMempool => "NoTenureChangeViaMempool",
            Self::Other(_) => "ServerFailureOther",
        }
    }

    /// The `reason_data` accompanying the reason, when there is any.
    #[must_use]
    pub fn reason_data(&self) -> Option<Value> {
        match self {
            Self::BadTransactionVersion
            | Self::ConflictingNonceInMempool
            | Self::TransferAmountMustBePositive
            | Self::BadAddressVersionByte
            | Self::NoCoinbaseViaMempool
            | Self::NoTenureChangeViaMempool => None,
            Self::SignatureValidation(message) | Self::Other(message) => {
                Some(json!({ "message": message }))
            }
            Self::FeeTooLow { expected, actual } => Some(json!({
                "expected": expected,
                "actual": actual,
            })),
            Self::BadNonce(mismatch) => Some(mismatch.reason_data()),
            Self::TooMuchChaining(mismatch) => {
                let mut data = mismatch.reason_data();
                if let Some(object) = data.as_object_mut() {
                    object.insert(
                        "message".to_owned(),
                        Value::from("Nonce would exceed chaining limit in mempool"),
                    );
                }
                Some(data)
            }
            Self::NotEnoughFunds { expected, actual } => Some(json!({
                "expected": format!("0x{expected:032x}"),
                "actual": format!("0x{actual:032x}"),
            })),
            Self::TransferRecipientCannotEqualSender(recipient) => {
                Some(json!({ "recipient": recipient.to_string() }))
            }
        }
    }

    /// The body `/v2/transactions` answers a refused submission with.
    #[must_use]
    pub fn into_json(self, txid: Sha256Sum) -> Value {
        let mut body = json!({
            "txid": txid.to_string(),
            "error": "transaction rejected",
            "reason": self.reason(),
        });
        if let (Some(object), Some(data)) = (body.as_object_mut(), self.reason_data()) {
            object.insert("reason_data".to_owned(), data);
        }
        body
    }
}

impl fmt::Display for Rejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.reason())?;
        self.reason_data()
            .map_or(Ok(()), |data| write!(formatter, ": {data}"))
    }
}

impl std::error::Error for Rejection {}

/// What holding a submitted transaction did to the pool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Admission {
    /// The transaction is new to this node.
    Added,
    /// The transaction outbid the ones holding its account nonces.
    Replaced(Vec<Sha256Sum>),
    /// The node already held this transaction.
    AlreadyPresent,
}

/// One held transaction and the facts admission and ordering read off it.
#[derive(Clone, Debug)]
struct Entry {
    transaction: Transaction,
    txid: Sha256Sum,
    origin: StacksAddress,
    origin_nonce: u64,
    /// The sponsor and the nonce it spends, for a sponsored transaction.
    sponsor: Option<(StacksAddress, u64)>,
    /// The account paying the fee, which is the sponsor when there is one.
    payer: StacksAddress,
    fee: u64,
    length: u64,
    accepted_at: u64,
}

impl Entry {
    /// Whether a tip has confirmed this transaction or made it unusable.
    fn is_stale(&self, tip: &impl ChainTip, now: u64) -> bool {
        if now.saturating_sub(self.accepted_at) > MAX_TRANSACTION_AGE_SECS {
            return true;
        }
        if self.origin_nonce < tip.account(&self.origin).nonce {
            return true;
        }
        if let Some((sponsor, nonce)) = self.sponsor
            && nonce < tip.account(&sponsor).nonce
        {
            return true;
        }
        tip.account(&self.payer)
            .balance
            .is_some_and(|balance| balance < u128::from(self.fee))
    }
}

/// A candidate ordered so that the best fee rate is the greatest.
#[derive(Clone, Copy, Debug)]
struct Candidate<'a>(&'a Entry);

impl Ord for Candidate<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        // Comparing the cross products orders by fee per byte without dividing.
        let mine = u128::from(self.0.fee) * u128::from(other.0.length);
        let theirs = u128::from(other.0.fee) * u128::from(self.0.length);
        mine.cmp(&theirs)
            .then_with(|| other.0.txid.cmp(&self.0.txid))
    }
}

impl PartialOrd for Candidate<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Candidate<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Candidate<'_> {}

/// The transactions this node holds for the next block.
#[derive(Clone, Debug)]
pub struct Mempool {
    network: Network,
    entries: HashMap<Sha256Sum, Entry>,
    /// The transaction holding each origin nonce, which is the key a
    /// replacement bids against.
    origins: HashMap<(StacksAddress, u64), Sha256Sum>,
    sponsors: HashMap<(StacksAddress, u64), Sha256Sum>,
}

impl Mempool {
    #[must_use]
    pub fn new(network: Network) -> Self {
        Self {
            network,
            entries: HashMap::new(),
            origins: HashMap::new(),
            sponsors: HashMap::new(),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn contains(&self, txid: Sha256Sum) -> bool {
        self.entries.contains_key(&txid)
    }

    #[must_use]
    pub fn get(&self, txid: Sha256Sum) -> Option<&Transaction> {
        self.entries.get(&txid).map(|entry| &entry.transaction)
    }

    /// Every account a held transaction depends on, which is what a tip has to
    /// be able to answer for.
    #[must_use]
    pub fn addresses(&self) -> Vec<StacksAddress> {
        let mut seen = HashSet::new();
        for entry in self.entries.values() {
            seen.insert(entry.origin);
            if let Some((sponsor, _)) = entry.sponsor {
                seen.insert(sponsor);
            }
        }
        seen.into_iter().collect()
    }

    /// Hold a transaction, or say why this node will not.
    pub fn submit(
        &mut self,
        transaction: Transaction,
        tip: &impl ChainTip,
        now: u64,
    ) -> Result<Admission, Rejection> {
        let txid = transaction.txid();
        if self.entries.contains_key(&txid) {
            return Ok(Admission::AlreadyPresent);
        }
        let mut entry = self.judge(transaction, tip)?;
        entry.accepted_at = now;

        let replaced = self.outbid(&entry)?;
        for prior in &replaced {
            self.remove(*prior);
        }
        self.origins.insert((entry.origin, entry.origin_nonce), txid);
        if let Some(sponsor) = entry.sponsor {
            self.sponsors.insert(sponsor, txid);
        }
        self.entries.insert(txid, entry);
        Ok(if replaced.is_empty() {
            Admission::Added
        } else {
            Admission::Replaced(replaced)
        })
    }

    /// Forget one transaction, whatever became of it.
    pub fn remove(&mut self, txid: Sha256Sum) -> Option<Transaction> {
        let entry = self.entries.remove(&txid)?;
        self.origins.remove(&(entry.origin, entry.origin_nonce));
        if let Some(sponsor) = entry.sponsor {
            self.sponsors.remove(&sponsor);
        }
        Some(entry.transaction)
    }

    /// Drop what a new tip confirmed, what it invalidated, and what has aged
    /// out, and report the transactions that left.
    ///
    /// A mined transaction leaves because the tip's nonce moved past it, which
    /// also collects the ones a competing transaction on the same nonce
    /// displaced on chain.
    pub fn advance(&mut self, tip: &impl ChainTip, now: u64) -> Vec<Sha256Sum> {
        let mut dropped: Vec<_> = self
            .entries
            .values()
            .filter(|entry| entry.is_stale(tip, now))
            .map(|entry| entry.txid)
            .collect();
        dropped.sort_unstable();
        for txid in &dropped {
            self.remove(*txid);
        }
        dropped
    }

    /// The transactions to offer the next block, best fee rate first.
    ///
    /// Only a transaction at its account's next nonce can execute, so taking
    /// one advances that account and admits the one behind it: a wallet's
    /// chained transactions reach the same block, still in nonce order.
    ///
    /// The walk stops at the bytes a block may carry. The execution-cost limit
    /// is left to assembly, which is the first point the cost is known — the
    /// same split stacks-core makes between `iterate_candidates` and its miner.
    #[must_use]
    pub fn candidates(&self, tip: &impl ChainTip) -> Vec<Transaction> {
        let mut nonces: HashMap<StacksAddress, u64> = HashMap::new();
        let mut heap = BinaryHeap::new();
        for entry in self.entries.values() {
            let next = *nonces
                .entry(entry.origin)
                .or_insert_with(|| tip.account(&entry.origin).nonce);
            if entry.origin_nonce == next {
                heap.push(Candidate(entry));
            }
        }

        let mut selected = Vec::new();
        let mut budget = MAX_BLOCK_LEN;
        while let Some(Candidate(entry)) = heap.pop() {
            if let Some((sponsor, nonce)) = entry.sponsor {
                let next = *nonces
                    .entry(sponsor)
                    .or_insert_with(|| tip.account(&sponsor).nonce);
                if nonce != next {
                    continue;
                }
            }
            let Some(remaining) = budget.checked_sub(entry.length) else {
                continue;
            };
            budget = remaining;
            selected.push(entry.transaction.clone());

            if let Some((sponsor, _)) = entry.sponsor
                && sponsor != entry.origin
                && let Some(next) = nonces.get_mut(&sponsor)
            {
                *next = next.saturating_add(1);
            }
            let next = nonces
                .get_mut(&entry.origin)
                .expect("the origin nonce was read to admit this candidate");
            *next = next.saturating_add(1);
            if let Some(following) = self
                .origins
                .get(&(entry.origin, *next))
                .and_then(|txid| self.entries.get(txid))
            {
                heap.push(Candidate(following));
            }
        }
        selected
    }

    /// Run stacks-core's admission checks, in stacks-core's order.
    fn judge(&self, transaction: Transaction, tip: &impl ChainTip) -> Result<Entry, Rejection> {
        let mainnet = match transaction.version() {
            TransactionVersion::Mainnet => true,
            TransactionVersion::Testnet => false,
            TransactionVersion::Other(_) => return Err(Rejection::BadTransactionVersion),
        };
        if mainnet != self.network.is_mainnet() {
            return Err(Rejection::BadTransactionVersion);
        }
        let origin = transaction.auth().origin().account_address(mainnet);
        let sponsor = transaction
            .auth()
            .sponsor()
            .map(|sponsor| sponsor.account_address(mainnet));
        let payload = transaction.payload().data();
        semantic_checks(payload, origin, mainnet)?;

        if matches!(payload, TransactionPayloadData::PoisonMicroblock { .. }) {
            return Err(Rejection::Other(
                "PoisonMicroblock transactions not accepted via mempool".to_owned(),
            ));
        }
        transaction
            .verify_authorization()
            .map_err(|error| Rejection::SignatureValidation(error.to_string()))?;
        if transaction.chain_id() != self.network.chain_id() {
            return Err(Rejection::SignatureValidation(format!(
                "invalid chain ID {} (expected {})",
                transaction.chain_id(),
                self.network.chain_id()
            )));
        }

        let length = transaction.as_bytes().len() as u64;
        let fee = transaction.auth().payer().fee();
        if fee < MINIMUM_TX_FEE || fee / length < MINIMUM_TX_FEE_RATE_PER_BYTE {
            return Err(Rejection::FeeTooLow {
                expected: MINIMUM_TX_FEE.max(length * MINIMUM_TX_FEE_RATE_PER_BYTE),
                actual: fee,
            });
        }
        // Epoch 4.0 supports every payload the codec accepts, so the epoch
        // check stacks-core runs here can only pass for a node that runs
        // nothing else.

        let origin_nonce = transaction.auth().origin().nonce();
        let payer = sponsor.unwrap_or(origin);
        let payer_nonce = transaction.auth().payer().nonce();
        check_nonces(
            tip,
            NonceCheck {
                origin,
                origin_nonce,
                sponsor,
                payer_nonce,
            },
        )?;

        if !valid_address_version(mainnet, origin.version())
            || !valid_address_version(mainnet, payer.version())
        {
            return Err(Rejection::BadAddressVersionByte);
        }

        let balance = tip.account(&payer).balance;
        let transfer = matches!(payload, TransactionPayloadData::TokenTransfer { .. });
        if !transfer && let Some(balance) = balance.filter(|balance| *balance < u128::from(fee)) {
            return Err(Rejection::NotEnoughFunds {
                expected: u128::from(fee),
                actual: balance,
            });
        }
        payload_checks(payload, tip, mainnet, &Spend { origin, payer, fee })?;

        Ok(Entry {
            txid: transaction.txid(),
            transaction,
            origin,
            origin_nonce,
            sponsor: sponsor.map(|sponsor| (sponsor, payer_nonce)),
            payer,
            fee,
            length,
            accepted_at: 0,
        })
    }

    /// The transactions this one displaces, if it pays more than all of them.
    fn outbid(&self, entry: &Entry) -> Result<Vec<Sha256Sum>, Rejection> {
        let mut replaced: Vec<_> = self
            .origins
            .get(&(entry.origin, entry.origin_nonce))
            .into_iter()
            .chain(entry.sponsor.and_then(|sponsor| self.sponsors.get(&sponsor)))
            .copied()
            .collect();
        replaced.sort_unstable();
        replaced.dedup();
        for prior in &replaced {
            if entry.fee <= self.entries[prior].fee {
                return Err(Rejection::ConflictingNonceInMempool);
            }
        }
        Ok(replaced)
    }
}

/// The accounts a transaction spends from.
struct Spend {
    origin: StacksAddress,
    payer: StacksAddress,
    fee: u64,
}

/// The nonces a transaction claims.
#[derive(Clone, Copy)]
struct NonceCheck {
    origin: StacksAddress,
    origin_nonce: u64,
    sponsor: Option<StacksAddress>,
    payer_nonce: u64,
}

/// The checks that read the transaction alone (`can_admit_mempool_semantic`).
fn semantic_checks(
    payload: &TransactionPayloadData,
    origin: StacksAddress,
    mainnet: bool,
) -> Result<(), Rejection> {
    let TransactionPayloadData::TokenTransfer {
        recipient, amount, ..
    } = payload
    else {
        return Ok(());
    };
    if *recipient == Principal::Standard(origin) {
        return Err(Rejection::TransferRecipientCannotEqualSender(origin));
    }
    if *amount == 0 {
        return Err(Rejection::TransferAmountMustBePositive);
    }
    if !valid_address_version(mainnet, principal_version(recipient)) {
        return Err(Rejection::BadAddressVersionByte);
    }
    Ok(())
}

/// The nonce and chaining rules (`check_transaction_nonces` and the
/// `MAXIMUM_MEMPOOL_TX_CHAINING` fallback around it).
fn check_nonces(tip: &impl ChainTip, claimed: NonceCheck) -> Result<(), Rejection> {
    let expected_origin = tip.account(&claimed.origin).nonce;
    let payer = claimed.sponsor.unwrap_or(claimed.origin);
    let expected_payer = tip.account(&payer).nonce;

    // The sponsor's nonce is the one stacks-core reports when both are wrong.
    let mismatch = if claimed.sponsor.is_some() && claimed.payer_nonce != expected_payer {
        Some(NonceMismatch {
            expected: expected_payer,
            actual: claimed.payer_nonce,
            principal: payer,
            is_origin: false,
        })
    } else if claimed.origin_nonce != expected_origin {
        Some(NonceMismatch {
            expected: expected_origin,
            actual: claimed.origin_nonce,
            principal: claimed.origin,
            is_origin: true,
        })
    } else {
        None
    };
    let Some(mismatch) = mismatch else {
        return Ok(());
    };
    // A nonce the account has already spent can never be chained onto.
    if mismatch.actual < mismatch.expected {
        return Err(Rejection::BadNonce(mismatch));
    }
    let origin_max = expected_origin + 1 + MAXIMUM_MEMPOOL_TX_CHAINING;
    if origin_max < claimed.origin_nonce {
        return Err(Rejection::TooMuchChaining(NonceMismatch {
            expected: origin_max,
            actual: claimed.origin_nonce,
            principal: claimed.origin,
            is_origin: true,
        }));
    }
    let payer_max = expected_payer + 1 + MAXIMUM_MEMPOOL_TX_CHAINING;
    if claimed.sponsor.is_some() && payer_max < claimed.payer_nonce {
        return Err(Rejection::TooMuchChaining(NonceMismatch {
            expected: payer_max,
            actual: claimed.payer_nonce,
            principal: payer,
            is_origin: false,
        }));
    }
    Ok(())
}

/// The checks that depend on what the transaction does.
fn payload_checks(
    payload: &TransactionPayloadData,
    tip: &impl ChainTip,
    mainnet: bool,
    spend: &Spend,
) -> Result<(), Rejection> {
    match payload {
        TransactionPayloadData::TokenTransfer {
            recipient, amount, ..
        } => {
            if !valid_address_version(mainnet, principal_version(recipient)) {
                return Err(Rejection::BadAddressVersionByte);
            }
            let total = u128::from(*amount)
                + if spend.origin == spend.payer {
                    u128::from(spend.fee)
                } else {
                    0
                };
            if let Some(balance) = tip
                .account(&spend.origin)
                .balance
                .filter(|balance| *balance < total)
            {
                return Err(Rejection::NotEnoughFunds {
                    expected: total,
                    actual: balance,
                });
            }
            Ok(())
        }
        TransactionPayloadData::Coinbase { .. }
        | TransactionPayloadData::CoinbaseToAltRecipient { .. }
        | TransactionPayloadData::NakamotoCoinbase { .. } => Err(Rejection::NoCoinbaseViaMempool),
        TransactionPayloadData::TenureChange(_) => Err(Rejection::NoTenureChangeViaMempool),
        _ => Ok(()),
    }
}

const fn principal_version(principal: &Principal) -> u8 {
    match principal {
        Principal::Standard(address) | Principal::Contract { address, .. } => address.version(),
    }
}

/// Whether a version byte names an account on this network
/// (`is_valid_address_version`).
const fn valid_address_version(mainnet: bool, version: u8) -> bool {
    if mainnet {
        matches!(version, 22 | 20)
    } else {
        matches!(version, 26 | 21)
    }
}

#[cfg(test)]
mod tests {
    use nano_codec::{AnchorMode, TransactionPayloadData};
    use nano_crypto::StacksPrivateKey;

    use super::{
        Account, Admission, HashMap, MAXIMUM_MEMPOOL_TX_CHAINING, Mempool, Network, Principal,
        Rejection, StacksAddress, Transaction,
    };

    const NETWORK: Network = Network::TESTNET;

    fn key(seed: &[u8]) -> StacksPrivateKey {
        StacksPrivateKey::from_seed(seed)
    }

    fn address(key: &StacksPrivateKey) -> StacksAddress {
        StacksAddress::single_signature(
            nano_primitives::hash160(&key.public_key().to_bytes_compressed()),
            NETWORK.is_mainnet(),
        )
    }

    fn transfer(key: &StacksPrivateKey, nonce: u64, fee: u64, amount: u64) -> Transaction {
        Transaction::sign_standard(
            nano_codec::TransactionVersion::for_network(NETWORK),
            NETWORK.chain_id(),
            AnchorMode::OnChainOnly,
            key,
            nonce,
            fee,
            TransactionPayloadData::TokenTransfer {
                recipient: Principal::Standard(address(&self::key(b"recipient"))),
                amount,
                memo: [0; 34],
            },
        )
        .expect("sign a transfer")
    }

    fn tip(accounts: &[(StacksAddress, Account)]) -> HashMap<StacksAddress, Account> {
        accounts.iter().copied().collect()
    }

    #[test]
    fn a_signed_transfer_at_the_next_nonce_is_held() {
        let sender = key(b"sender");
        let mut mempool = Mempool::new(NETWORK);
        let transaction = transfer(&sender, 0, 400, 1);
        let txid = transaction.txid();
        assert_eq!(
            mempool.submit(transaction.clone(), &tip(&[]), 0),
            Ok(Admission::Added)
        );
        assert!(mempool.contains(txid));
        assert_eq!(
            mempool.submit(transaction, &tip(&[]), 0),
            Ok(Admission::AlreadyPresent)
        );
        assert_eq!(mempool.len(), 1);
    }

    #[test]
    fn a_higher_fee_on_the_same_nonce_replaces_the_transaction_holding_it() {
        let sender = key(b"sender");
        let mut mempool = Mempool::new(NETWORK);
        let cheap = transfer(&sender, 0, 400, 1);
        let cheap_txid = cheap.txid();
        mempool.submit(cheap, &tip(&[]), 0).expect("hold the cheap");

        let same_fee = transfer(&sender, 0, 400, 2);
        assert_eq!(
            mempool.submit(same_fee, &tip(&[]), 0),
            Err(Rejection::ConflictingNonceInMempool)
        );

        let richer = transfer(&sender, 0, 500, 1);
        let richer_txid = richer.txid();
        assert_eq!(
            mempool.submit(richer, &tip(&[]), 0),
            Ok(Admission::Replaced(vec![cheap_txid]))
        );
        assert!(!mempool.contains(cheap_txid));
        assert!(mempool.contains(richer_txid));
        assert_eq!(mempool.len(), 1);
    }

    #[test]
    fn a_nonce_the_account_has_spent_is_refused_and_one_too_far_ahead_too() {
        let sender = key(b"sender");
        let account = address(&sender);
        let mut mempool = Mempool::new(NETWORK);
        let tip = tip(&[(
            account,
            Account {
                nonce: 5,
                balance: None,
            },
        )]);
        assert!(matches!(
            mempool.submit(transfer(&sender, 4, 400, 1), &tip, 0),
            Err(Rejection::BadNonce(_))
        ));
        let too_far = 5 + MAXIMUM_MEMPOOL_TX_CHAINING + 2;
        assert!(matches!(
            mempool.submit(transfer(&sender, too_far, 400, 1), &tip, 0),
            Err(Rejection::TooMuchChaining(_))
        ));
        assert_eq!(
            mempool.submit(transfer(&sender, too_far - 1, 400, 1), &tip, 0),
            Ok(Admission::Added)
        );
    }

    #[test]
    fn a_fee_below_one_microstx_per_byte_is_refused() {
        let sender = key(b"sender");
        let mut mempool = Mempool::new(NETWORK);
        let transaction = transfer(&sender, 0, 1, 1);
        let expected = transaction.as_bytes().len() as u64;
        assert_eq!(
            mempool.submit(transaction, &tip(&[]), 0),
            Err(Rejection::FeeTooLow {
                expected,
                actual: 1
            })
        );
    }

    #[test]
    fn an_account_that_cannot_pay_the_fee_is_refused() {
        let sender = key(b"sender");
        let account = address(&sender);
        let mut mempool = Mempool::new(NETWORK);
        let tip = tip(&[(
            account,
            Account {
                nonce: 0,
                balance: Some(10),
            },
        )]);
        assert_eq!(
            mempool.submit(transfer(&sender, 0, 400, 1), &tip, 0),
            Err(Rejection::NotEnoughFunds {
                expected: 401,
                actual: 10
            })
        );
    }

    #[test]
    fn a_transfer_to_the_sender_or_of_nothing_is_refused() {
        let sender = key(b"sender");
        let account = address(&sender);
        let mut mempool = Mempool::new(NETWORK);
        let to_self = Transaction::sign_standard(
            nano_codec::TransactionVersion::for_network(NETWORK),
            NETWORK.chain_id(),
            AnchorMode::OnChainOnly,
            &sender,
            0,
            400,
            TransactionPayloadData::TokenTransfer {
                recipient: Principal::Standard(account),
                amount: 1,
                memo: [0; 34],
            },
        )
        .expect("sign a transfer");
        assert_eq!(
            mempool.submit(to_self, &tip(&[]), 0),
            Err(Rejection::TransferRecipientCannotEqualSender(account))
        );
        assert_eq!(
            mempool.submit(transfer(&sender, 0, 400, 0), &tip(&[]), 0),
            Err(Rejection::TransferAmountMustBePositive)
        );
    }

    #[test]
    fn a_transaction_for_another_chain_is_refused() {
        let sender = key(b"sender");
        let mut mempool = Mempool::new(NETWORK);
        let elsewhere = Transaction::sign_standard(
            nano_codec::TransactionVersion::Mainnet,
            NETWORK.chain_id(),
            AnchorMode::OnChainOnly,
            &sender,
            0,
            400,
            TransactionPayloadData::TokenTransfer {
                recipient: Principal::Standard(address(&key(b"recipient"))),
                amount: 1,
                memo: [0; 34],
            },
        )
        .expect("sign a transfer");
        assert_eq!(
            mempool.submit(elsewhere, &tip(&[]), 0),
            Err(Rejection::BadTransactionVersion)
        );

        let other_chain = Transaction::sign_standard(
            nano_codec::TransactionVersion::for_network(NETWORK),
            NETWORK.chain_id() ^ 1,
            AnchorMode::OnChainOnly,
            &sender,
            0,
            400,
            TransactionPayloadData::TokenTransfer {
                recipient: Principal::Standard(address(&key(b"recipient"))),
                amount: 1,
                memo: [0; 34],
            },
        )
        .expect("sign a transfer");
        assert!(matches!(
            mempool.submit(other_chain, &tip(&[]), 0),
            Err(Rejection::SignatureValidation(_))
        ));
    }

    #[test]
    fn a_tampered_signature_is_refused() {
        let sender = key(b"sender");
        let mut mempool = Mempool::new(NETWORK);
        let mut bytes = transfer(&sender, 0, 400, 1).encode();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        let (mutated, _) = Transaction::decode(&bytes).expect("decode the mutated transaction");
        assert!(matches!(
            mempool.submit(mutated, &tip(&[]), 0),
            Err(Rejection::SignatureValidation(_))
        ));
    }

    #[test]
    fn a_confirmed_nonce_and_an_aged_transaction_leave_the_pool() {
        let sender = key(b"sender");
        let account = address(&sender);
        let mut mempool = Mempool::new(NETWORK);
        let mined = transfer(&sender, 0, 400, 1);
        let mined_txid = mined.txid();
        mempool.submit(mined, &tip(&[]), 0).expect("hold the first");
        mempool
            .submit(transfer(&sender, 1, 400, 1), &tip(&[]), 0)
            .expect("hold the second");

        let advanced = tip(&[(
            account,
            Account {
                nonce: 1,
                balance: None,
            },
        )]);
        assert_eq!(mempool.advance(&advanced, 0), vec![mined_txid]);
        assert_eq!(mempool.len(), 1);

        let stale = super::MAX_TRANSACTION_AGE_SECS + 1;
        assert_eq!(mempool.advance(&advanced, stale).len(), 1);
        assert!(mempool.is_empty());
    }

    #[test]
    fn candidates_run_best_fee_rate_first_and_keep_each_account_in_nonce_order() {
        let poor = key(b"poor");
        let rich = key(b"rich");
        let mut mempool = Mempool::new(NETWORK);
        let cheap = transfer(&poor, 0, 400, 1);
        let dear_first = transfer(&rich, 0, 900, 1);
        let dear_second = transfer(&rich, 1, 10_000, 1);
        for transaction in [&cheap, &dear_first, &dear_second] {
            mempool
                .submit(transaction.clone(), &tip(&[]), 0)
                .expect("hold the transaction");
        }
        let candidates = mempool.candidates(&tip(&[]));
        assert_eq!(
            candidates.iter().map(Transaction::txid).collect::<Vec<_>>(),
            vec![dear_first.txid(), dear_second.txid(), cheap.txid()],
        );
    }

    #[test]
    fn a_nonce_gap_holds_back_the_transactions_behind_it() {
        let sender = key(b"sender");
        let mut mempool = Mempool::new(NETWORK);
        mempool
            .submit(transfer(&sender, 1, 400, 1), &tip(&[]), 0)
            .expect("hold a transaction one nonce ahead");
        assert!(mempool.candidates(&tip(&[])).is_empty());

        let first = transfer(&sender, 0, 400, 1);
        mempool
            .submit(first.clone(), &tip(&[]), 0)
            .expect("hold the transaction that closes the gap");
        assert_eq!(mempool.candidates(&tip(&[])).len(), 2);
        assert_eq!(mempool.candidates(&tip(&[]))[0].txid(), first.txid());
    }

    #[test]
    fn a_rejection_reports_the_reason_a_wallet_reads() {
        let mismatch = Rejection::BadNonce(super::NonceMismatch {
            expected: 3,
            actual: 1,
            principal: address(&key(b"sender")),
            is_origin: true,
        });
        assert_eq!(mismatch.reason(), "BadNonce");
        let body = mismatch.into_json(nano_primitives::sha512_256(b"txid"));
        assert_eq!(body["reason"], "BadNonce");
        assert_eq!(body["reason_data"]["expected"], 3);
        assert_eq!(body["error"], "transaction rejected");

        let funds = Rejection::NotEnoughFunds {
            expected: 1,
            actual: 0,
        };
        assert_eq!(
            funds.reason_data().expect("funds carry data")["expected"],
            "0x00000000000000000000000000000001"
        );
    }
}
