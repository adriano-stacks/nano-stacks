pub mod sbtc;

use std::{collections::HashMap, fmt};

use bitcoin::{
    Amount, Block, Transaction, TxIn, TxOut,
    absolute::LockTime,
    consensus::deserialize,
    hashes::Hash,
    script::{Instruction, PushBytesBuf, Script, ScriptBuf},
    transaction::Version as TransactionVersion,
};
use bitcoincore_rpc::{Auth, Client, RpcApi};
use nano_address::PoxAddress;

/// A Bitcoin block accepted by the HTTP/RPC ingest boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BitcoinBlock {
    pub height: u64,
    pub hash: [u8; 32],
    pub operations: Vec<BitcoinOperation>,
}

const PRE_STX_WINDOW_BLOCKS: u64 = 6;

/// `PreStx` outputs available to later Bitcoin blocks.
#[derive(Clone, Debug, Default)]
pub struct PreStxCache {
    senders: HashMap<[u8; 32], (nano_address::StacksAddress, u64)>,
}

impl PreStxCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn retain_window(&mut self, height: u64) {
        self.senders.retain(|_, (_, seen_height)| {
            height.saturating_sub(*seen_height) <= PRE_STX_WINDOW_BLOCKS
        });
    }

    /// Drop the pairings contributed at or above a Bitcoin height.
    ///
    /// A reorganized Bitcoin block takes its `PreStx` outputs with it, so the
    /// operations they would have authenticated are no longer paired.
    pub fn invalidate_from(&mut self, height: u64) {
        self.senders
            .retain(|_, (_, seen_height)| *seen_height < height);
    }
}

/// The source boundary for Bitcoin input.
pub trait BitcoinSource {
    type Error;

    fn block_at(&mut self, height: u64) -> Result<BitcoinBlock, Self::Error>;

    /// One block's header hash, without decoding its transactions.
    fn block_hash_at(&self, height: u64) -> Result<[u8; 32], Self::Error>;

    /// Forget everything read at or above a height, after a reorganization.
    ///
    /// Both real sources walk forward incrementally and carry a `PreStx` window
    /// six blocks wide, and neither is sound over a chain that moved underneath
    /// them: an operation authorised by a `PreStx` output in a block Bitcoin no
    /// longer holds is not an operation at all. A caller that has located the
    /// fork point against its own snapshots calls this with the first height that
    /// no longer holds, and the next read walks forward from there.
    ///
    /// Provided rather than required, because a source that keeps nothing —
    /// a fixture, a recorded window — has nothing to forget, and making it
    /// mandatory would put an empty body in every one of them. Until this existed
    /// the node could not reach either source's own `invalidate_from` through the
    /// trait it holds them behind, which is why [[049]] recorded the reorganization
    /// path as unwired.
    fn invalidate_from(&mut self, _height: u64) {}
}

/// Bitcoin Core RPC-backed protocol-operation source.
#[derive(Debug)]
pub struct BitcoinRpcSource {
    client: Client,
    magic: [u8; 2],
    pre_stx: PreStxCache,
    last_height: Option<u64>,
    last_block: Option<BitcoinBlock>,
}

#[derive(Debug)]
pub enum BitcoinRpcSourceError {
    Rpc(bitcoincore_rpc::Error),
    /// A REST source could not be read.
    Rest(String),
    Parse(BitcoinParseError),
    /// Bitcoin no longer holds the block this source read at that height.
    ///
    /// The fork can be deeper: this is only the shallowest height known to have
    /// changed. Callers locate the fork point against their own snapshot
    /// history and call [`BitcoinRpcSource::invalidate_from`] with it.
    Reorganized {
        height: u64,
    },
}

impl std::fmt::Display for BitcoinRpcSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc(error) => error.fmt(formatter),
            Self::Rest(error) => write!(formatter, "burnchain REST source: {error}"),
            Self::Parse(error) => error.fmt(formatter),
            Self::Reorganized { height } => write!(
                formatter,
                "Bitcoin block at height {height} is no longer canonical"
            ),
        }
    }
}

impl std::error::Error for BitcoinRpcSourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Rpc(error) => Some(error),
            Self::Parse(error) => Some(error),
            Self::Rest(_) | Self::Reorganized { .. } => None,
        }
    }
}

impl From<bitcoincore_rpc::Error> for BitcoinRpcSourceError {
    fn from(error: bitcoincore_rpc::Error) -> Self {
        Self::Rpc(error)
    }
}

impl From<BitcoinParseError> for BitcoinRpcSourceError {
    fn from(error: BitcoinParseError) -> Self {
        Self::Parse(error)
    }
}

impl BitcoinRpcSource {
    /// Connect to Bitcoin Core with explicit RPC credentials.
    pub fn new(
        endpoint: &str,
        username: impl Into<String>,
        password: impl Into<String>,
        magic: [u8; 2],
    ) -> Result<Self, BitcoinRpcSourceError> {
        Ok(Self {
            client: Client::new(endpoint, Auth::UserPass(username.into(), password.into()))?,
            magic,
            pre_stx: PreStxCache::new(),
            last_height: None,
            last_block: None,
        })
    }

    /// Read one Bitcoin block's header hash without decoding its transactions.
    pub fn block_hash_at(&self, height: u64) -> Result<[u8; 32], BitcoinRpcSourceError> {
        Ok(bitcoin_hash_bytes(
            self.client.get_block_hash(height)?.to_byte_array(),
        ))
    }

    /// Forget every block read at or above a Bitcoin height.
    ///
    /// Called with the first height a reorganization invalidated, this leaves
    /// the source primed with the surviving chain's `PreStx` outputs only, so
    /// the next read walks forward from the fork point.
    pub fn invalidate_from(&mut self, height: u64) {
        self.pre_stx.invalidate_from(height);
        self.last_block = self.last_block.take().filter(|block| block.height < height);
        self.last_height = self.last_height.and_then(|last| {
            if last < height {
                Some(last)
            } else {
                height.checked_sub(1)
            }
        });
    }

    /// Report a reorganization rather than folding it into the next read.
    ///
    /// The `PreStx` window and the incremental walk are only sound over a chain
    /// that has not moved underneath them.
    fn check_last_read(&mut self) -> Result<(), BitcoinRpcSourceError> {
        let Some((height, hash)) = self
            .last_block
            .as_ref()
            .map(|block| (block.height, block.hash))
        else {
            return Ok(());
        };
        if self.block_hash_at(height)? == hash {
            return Ok(());
        }
        self.invalidate_from(height);
        Err(BitcoinRpcSourceError::Reorganized { height })
    }

    /// Decode the protocol operations in a Bitcoin block, retaining required prior `PreStx` outputs.
    pub fn block_at(&mut self, height: u64) -> Result<BitcoinBlock, BitcoinRpcSourceError> {
        self.check_last_read()?;
        if let Some(block) = self
            .last_block
            .as_ref()
            .filter(|block| block.height == height)
        {
            return Ok(block.clone());
        }
        if self.last_height.is_some_and(|last| height < last) {
            self.pre_stx = PreStxCache::new();
        }
        let first_height = self.last_height.filter(|last| *last < height).map_or_else(
            || height.saturating_sub(PRE_STX_WINDOW_BLOCKS),
            |last| last + 1,
        );
        let mut current = None;
        for current_height in first_height..=height {
            let hash = self.client.get_block_hash(current_height)?;
            let raw = bitcoin::consensus::serialize(&self.client.get_block(&hash)?);
            let block =
                decode_block_with_pre_stx(current_height, &raw, self.magic, &mut self.pre_stx)?;
            current = Some(block);
        }
        self.last_height = Some(height);
        let block = current.ok_or(BitcoinParseError::InvalidBlock)?;
        self.last_block = Some(block.clone());
        Ok(block)
    }
}

/// An Esplora-backed source, for a node with no Bitcoin RPC of its own.
///
/// Esplora serves `/block/<hash>/raw`, which is the same bytes `getblock
/// <hash> 0` returns, and `/block-height/<n>`, which is `getblockhash`. Those
/// two are all a follower reads the burnchain for, so a node can follow a
/// public chain without carrying several hundred gigabytes of it.
#[derive(Debug)]
pub struct BitcoinRestSource {
    base: String,
    magic: [u8; 2],
    pre_stx: PreStxCache,
    last_height: Option<u64>,
    last_block: Option<BitcoinBlock>,
}

impl BitcoinRestSource {
    pub fn new(base: &str, magic: [u8; 2]) -> Result<Self, BitcoinRpcSourceError> {
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            magic,
            pre_stx: PreStxCache::new(),
            last_height: None,
            last_block: None,
        })
    }

    /// A plain synchronous GET.
    ///
    /// Deliberately not `reqwest::blocking`, which carries its own runtime:
    /// building one inside the node's async context and dropping it there
    /// panics tokio, which is how the first live node died.
    fn get(&self, path: &str) -> Result<Vec<u8>, BitcoinRpcSourceError> {
        use std::io::Read as _;
        let mut body = Vec::new();
        let mut reader = ureq::get(&format!("{}/{path}", self.base))
            .call()
            .map_err(|error| BitcoinRpcSourceError::Rest(error.to_string()))?
            .into_body()
            .into_reader();
        reader
            .read_to_end(&mut body)
            .map_err(|error| BitcoinRpcSourceError::Rest(error.to_string()))?;
        Ok(body)
    }

    /// The hash Esplora reports at a height, in the byte order nano uses.
    ///
    /// Esplora answers with the hash as it is displayed, which is already the
    /// order a `BitcoinBlock` records — reversing it again would compare a
    /// block against its own mirror image and call every height a
    /// reorganization.
    pub fn block_hash_at(&self, height: u64) -> Result<[u8; 32], BitcoinRpcSourceError> {
        let text = String::from_utf8(self.get(&format!("block-height/{height}"))?)
            .map_err(|error| BitcoinRpcSourceError::Rest(error.to_string()))?;
        decode_block_hash(text.trim())
            .ok_or_else(|| BitcoinRpcSourceError::Rest(format!("unreadable block hash {text:?}")))
    }

    pub fn invalidate_from(&mut self, height: u64) {
        self.pre_stx.invalidate_from(height);
        self.last_block = self.last_block.take().filter(|block| block.height < height);
        self.last_height = self.last_height.and_then(|last| {
            (last < height).then_some(last)
        });
    }

    fn check_last_read(&mut self) -> Result<(), BitcoinRpcSourceError> {
        let Some((height, hash)) = self
            .last_block
            .as_ref()
            .map(|block| (block.height, block.hash))
        else {
            return Ok(());
        };
        if self.block_hash_at(height)? == hash {
            return Ok(());
        }
        self.invalidate_from(height);
        Err(BitcoinRpcSourceError::Reorganized { height })
    }

    pub fn block_at(&mut self, height: u64) -> Result<BitcoinBlock, BitcoinRpcSourceError> {
        self.check_last_read()?;
        if let Some(block) = self
            .last_block
            .as_ref()
            .filter(|block| block.height == height)
        {
            return Ok(block.clone());
        }
        if self.last_height.is_some_and(|last| height < last) {
            self.pre_stx = PreStxCache::new();
        }
        // A `PreStx` output authorises an operation up to six blocks later, so
        // a source that starts mid-chain reads that window first.
        let first_height = self.last_height.filter(|last| *last < height).map_or_else(
            || height.saturating_sub(PRE_STX_WINDOW_BLOCKS),
            |last| last + 1,
        );
        let mut current = None;
        for current_height in first_height..=height {
            let hash = String::from_utf8(self.get(&format!("block-height/{current_height}"))?)
                .map_err(|error| BitcoinRpcSourceError::Rest(error.to_string()))?;
            let raw = self.get(&format!("block/{}/raw", hash.trim()))?;
            let block =
                decode_block_with_pre_stx(current_height, &raw, self.magic, &mut self.pre_stx)?;
            current = Some(block);
        }
        self.last_height = Some(height);
        let block = current.ok_or(BitcoinParseError::InvalidBlock)?;
        self.last_block = Some(block.clone());
        Ok(block)
    }
}

impl BitcoinSource for BitcoinRestSource {
    type Error = BitcoinRpcSourceError;

    fn block_at(&mut self, height: u64) -> Result<BitcoinBlock, Self::Error> {
        Self::block_at(self, height)
    }

    fn block_hash_at(&self, height: u64) -> Result<[u8; 32], Self::Error> {
        Self::block_hash_at(self, height)
    }

    fn invalidate_from(&mut self, height: u64) {
        Self::invalidate_from(self, height);
    }
}

/// Decode a big-endian block hash as Esplora prints it.
fn decode_block_hash(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(bytes)
}

impl BitcoinSource for BitcoinRpcSource {
    type Error = BitcoinRpcSourceError;

    fn block_at(&mut self, height: u64) -> Result<BitcoinBlock, Self::Error> {
        Self::block_at(self, height)
    }

    fn block_hash_at(&self, height: u64) -> Result<[u8; 32], Self::Error> {
        Self::block_hash_at(self, height)
    }

    fn invalidate_from(&mut self, height: u64) {
        Self::invalidate_from(self, height);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BitcoinOperation {
    pub txid: [u8; 32],
    pub transaction_index: u32,
    pub inputs: Vec<BitcoinInput>,
    pub outputs: Vec<BitcoinOutput>,
    pub kind: BitcoinOperationKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BitcoinInput {
    pub txid: [u8; 32],
    pub output_index: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BitcoinOutput {
    pub amount_sats: u64,
    pub recipient: PoxAddress,
}

/// The canonical payload for a Bitcoin leader-block commitment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LeaderBlockCommitment {
    pub block_header_hash: [u8; 32],
    pub new_seed: [u8; 32],
    pub parent_block_height: u32,
    pub parent_transaction_index: u16,
    pub key_block_height: u32,
    pub key_transaction_index: u16,
    pub memo: u8,
    pub parent_modulus: u8,
}

/// The canonical payload for a Bitcoin leader-key registration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeaderKeyRegistration {
    pub consensus_hash: [u8; 20],
    pub vrf_public_key: [u8; 32],
    pub block_signing_key_hash: [u8; 20],
    pub memo: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeaderKeyRegistrationError {
    InvalidVrfPublicKey,
    MemoTooLarge,
}

impl fmt::Display for LeaderKeyRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidVrfPublicKey => "invalid leader-key VRF public key",
            Self::MemoTooLarge => "leader-key registration memo exceeds five bytes",
        })
    }
}

impl std::error::Error for LeaderKeyRegistrationError {}

impl LeaderKeyRegistration {
    /// Encode the protocol payload following the leader-key opcode.
    pub fn encode(&self) -> Result<Vec<u8>, LeaderKeyRegistrationError> {
        if self.memo.len() > 5 {
            return Err(LeaderKeyRegistrationError::MemoTooLarge);
        }
        nano_crypto::VrfPublicKey::from_bytes(self.vrf_public_key)
            .map_err(|_| LeaderKeyRegistrationError::InvalidVrfPublicKey)?;

        let mut bytes = Vec::with_capacity(72 + self.memo.len());
        bytes.extend_from_slice(&self.consensus_hash);
        bytes.extend_from_slice(&self.vrf_public_key);
        bytes.extend_from_slice(&self.block_signing_key_hash);
        bytes.extend_from_slice(&self.memo);
        Ok(bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeaderCommitmentError {
    MemoTooLarge,
    InvalidParentModulus,
}

impl fmt::Display for LeaderCommitmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MemoTooLarge => "leader commitment memo exceeds five bits",
            Self::InvalidParentModulus => "leader commitment parent modulus exceeds four",
        })
    }
}

impl std::error::Error for LeaderCommitmentError {}

impl LeaderBlockCommitment {
    /// Encode the protocol payload following the leader-commit opcode.
    pub fn encode(self) -> Result<[u8; 77], LeaderCommitmentError> {
        if self.memo > 0b1_1111 {
            return Err(LeaderCommitmentError::MemoTooLarge);
        }
        if self.parent_modulus > 4 {
            return Err(LeaderCommitmentError::InvalidParentModulus);
        }
        let mut bytes = [0; 77];
        bytes[..32].copy_from_slice(&self.block_header_hash);
        bytes[32..64].copy_from_slice(&self.new_seed);
        bytes[64..68].copy_from_slice(&self.parent_block_height.to_be_bytes());
        bytes[68..70].copy_from_slice(&self.parent_transaction_index.to_be_bytes());
        bytes[70..74].copy_from_slice(&self.key_block_height.to_be_bytes());
        bytes[74..76].copy_from_slice(&self.key_transaction_index.to_be_bytes());
        bytes[76] = (self.memo << 3) | self.parent_modulus;
        Ok(bytes)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LeaderCommitmentTransactionError {
    ZeroCommitmentAmount,
    ZeroChangeAmount,
    InvalidProtocolPayload,
    Commitment(LeaderCommitmentError),
}

impl fmt::Display for LeaderCommitmentTransactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroCommitmentAmount => {
                formatter.write_str("leader commitment amount must be greater than zero")
            }
            Self::ZeroChangeAmount => {
                formatter.write_str("change amount must be greater than zero")
            }
            Self::InvalidProtocolPayload => {
                formatter.write_str("invalid leader commitment payload")
            }
            Self::Commitment(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for LeaderCommitmentTransactionError {}

impl From<LeaderCommitmentError> for LeaderCommitmentTransactionError {
    fn from(error: LeaderCommitmentError) -> Self {
        Self::Commitment(error)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LeaderKeyRegistrationTransactionError {
    InvalidProtocolPayload,
    Registration(LeaderKeyRegistrationError),
}

impl fmt::Display for LeaderKeyRegistrationTransactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidProtocolPayload => {
                formatter.write_str("invalid leader-key registration payload")
            }
            Self::Registration(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for LeaderKeyRegistrationTransactionError {}

impl From<LeaderKeyRegistrationError> for LeaderKeyRegistrationTransactionError {
    fn from(error: LeaderKeyRegistrationError) -> Self {
        Self::Registration(error)
    }
}

/// Construct an unsigned leader-key registration transaction.
pub fn build_leader_key_registration_transaction(
    magic: [u8; 2],
    registration: &LeaderKeyRegistration,
    inputs: Vec<TxIn>,
) -> Result<Transaction, LeaderKeyRegistrationTransactionError> {
    let mut protocol_payload = Vec::with_capacity(80);
    protocol_payload.extend_from_slice(&magic);
    protocol_payload.push(b'^');
    protocol_payload.extend_from_slice(&registration.encode()?);
    let protocol_payload = PushBytesBuf::try_from(protocol_payload)
        .map_err(|_| LeaderKeyRegistrationTransactionError::InvalidProtocolPayload)?;

    Ok(Transaction {
        version: TransactionVersion::TWO,
        lock_time: LockTime::ZERO,
        input: inputs,
        output: vec![TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::new_op_return(protocol_payload),
        }],
    })
}

/// Construct an unsigned waterfall leader-commitment transaction.
///
/// The payout is always the first spendable output. An optional change output
/// follows it, so wallet funding cannot alter the protocol output ordering.
/// Callers that fund the returned transaction through Bitcoin Core may leave
/// `inputs` empty.
pub fn build_leader_commitment_transaction(
    magic: [u8; 2],
    commitment: LeaderBlockCommitment,
    inputs: Vec<TxIn>,
    sbtc_address: &PoxAddress,
    commitment_amount: Amount,
    change: Option<(&PoxAddress, Amount)>,
) -> Result<Transaction, LeaderCommitmentTransactionError> {
    if commitment_amount == Amount::ZERO {
        return Err(LeaderCommitmentTransactionError::ZeroCommitmentAmount);
    }
    if change.is_some_and(|(_, amount)| amount == Amount::ZERO) {
        return Err(LeaderCommitmentTransactionError::ZeroChangeAmount);
    }

    let mut protocol_payload = Vec::with_capacity(80);
    protocol_payload.extend_from_slice(&magic);
    protocol_payload.push(b'[');
    protocol_payload.extend_from_slice(&commitment.encode()?);
    let protocol_payload = PushBytesBuf::try_from(protocol_payload)
        .map_err(|_| LeaderCommitmentTransactionError::InvalidProtocolPayload)?;

    let mut output = vec![
        TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::new_op_return(protocol_payload),
        },
        TxOut {
            value: commitment_amount,
            script_pubkey: sbtc_address.script_pubkey(),
        },
    ];
    if let Some((address, amount)) = change {
        output.push(TxOut {
            value: amount,
            script_pubkey: address.script_pubkey(),
        });
    }

    Ok(Transaction {
        version: TransactionVersion::TWO,
        lock_time: LockTime::ZERO,
        input: inputs,
        output,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BitcoinOperationKind {
    LeaderBlockCommit {
        block_header_hash: [u8; 32],
        new_seed: [u8; 32],
        parent_block_height: u32,
        parent_transaction_index: u16,
        key_block_height: u32,
        key_transaction_index: u16,
        memo: u8,
        parent_modulus: u8,
    },
    LeaderKeyRegistration {
        consensus_hash: [u8; 20],
        vrf_public_key: [u8; 32],
        block_signing_key_hash: Option<[u8; 20]>,
        memo: Vec<u8>,
    },
    PreStx {
        sender: nano_address::StacksAddress,
    },
    StackStx {
        sender: nano_address::StacksAddress,
        reward_address: PoxAddress,
        amount: u128,
        cycles: u8,
        signer_key: Option<[u8; 33]>,
        max_amount: Option<u128>,
        authorization_id: Option<u32>,
    },
    TransferStx {
        sender: nano_address::StacksAddress,
        recipient: nano_address::StacksAddress,
        amount: u128,
        memo: Vec<u8>,
    },
    DelegateStx {
        sender: nano_address::StacksAddress,
        delegate: nano_address::StacksAddress,
        amount: u128,
        reward_address: Option<PoxAddress>,
        reward_address_output: Option<u32>,
        until_bitcoin_height: Option<u64>,
    },
    VoteForAggregateKey {
        sender: nano_address::StacksAddress,
        signer_index: u16,
        aggregate_key: [u8; 33],
        round: u32,
        reward_cycle: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BitcoinParseError {
    InvalidBlock,
    TooManyTransactions,
}

impl fmt::Display for BitcoinParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidBlock => "invalid Bitcoin block",
            Self::TooManyTransactions => "Bitcoin block has too many transactions",
        })
    }
}

impl std::error::Error for BitcoinParseError {}

/// Decode a Bitcoin block and classify protocol operations in output zero.
pub fn decode_block(
    height: u64,
    bytes: &[u8],
    magic: [u8; 2],
) -> Result<BitcoinBlock, BitcoinParseError> {
    decode_block_with_pre_stx(height, bytes, magic, &mut PreStxCache::new())
}

/// Decode a Bitcoin block while retaining `PreStx` outputs needed by later blocks.
pub fn decode_block_with_pre_stx(
    height: u64,
    bytes: &[u8],
    magic: [u8; 2],
    pre_stx_cache: &mut PreStxCache,
) -> Result<BitcoinBlock, BitcoinParseError> {
    pre_stx_cache.retain_window(height);
    let block: Block = deserialize(bytes).map_err(|_| BitcoinParseError::InvalidBlock)?;
    let mut operations = Vec::new();
    for (index, transaction) in block.txdata.iter().enumerate() {
        let index = u32::try_from(index).map_err(|_| BitcoinParseError::TooManyTransactions)?;
        let Some((opcode, payload)) = transaction
            .output
            .first()
            .and_then(|output| protocol_payload(output.script_pubkey.as_script(), magic))
        else {
            continue;
        };
        let Some(outputs) = transaction
            .output
            .iter()
            .skip(1)
            .map(|output| {
                PoxAddress::from_script_pubkey(output.script_pubkey.as_bytes(), false)
                    .ok()
                    .map(|recipient| BitcoinOutput {
                        amount_sats: output.value.to_sat(),
                        recipient,
                    })
            })
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        if transaction.input.is_empty() || outputs.is_empty() {
            continue;
        }
        let inputs: Vec<_> = transaction
            .input
            .iter()
            .map(|input| BitcoinInput {
                txid: bitcoin_hash_bytes(input.previous_output.txid.to_byte_array()),
                output_index: input.previous_output.vout,
            })
            .collect();
        let sender = inputs
            .first()
            .filter(|input| input.output_index == 1)
            .and_then(|input| pre_stx_cache.senders.get(&input.txid))
            .map(|(sender, _)| *sender);
        let Some(kind) = parse_operation(opcode, payload, &outputs, sender) else {
            continue;
        };
        let txid = bitcoin_hash_bytes(transaction.compute_txid().to_byte_array());
        if let BitcoinOperationKind::PreStx { sender } = &kind {
            pre_stx_cache.senders.insert(txid, (*sender, height));
        }
        operations.push(BitcoinOperation {
            txid,
            transaction_index: index,
            inputs,
            outputs,
            kind,
        });
    }
    Ok(BitcoinBlock {
        height,
        hash: bitcoin_hash_bytes(block.block_hash().to_byte_array()),
        operations,
    })
}

const fn bitcoin_hash_bytes(mut bytes: [u8; 32]) -> [u8; 32] {
    bytes.reverse();
    bytes
}

fn protocol_payload(script: &Script, magic: [u8; 2]) -> Option<(u8, &[u8])> {
    let mut instructions = script.instructions_minimal();
    let Instruction::Op(op_return) = instructions.next()?.ok()? else {
        return None;
    };
    if op_return.to_u8() != 0x6a {
        return None;
    }
    let Instruction::PushBytes(data) = instructions.next()?.ok()? else {
        return None;
    };
    if instructions.next().is_some() {
        return None;
    }
    let data = data.as_bytes();
    (data.starts_with(&magic) && data.len() > magic.len()).then(|| (data[2], &data[3..]))
}

fn parse_operation(
    opcode: u8,
    data: &[u8],
    outputs: &[BitcoinOutput],
    sender: Option<nano_address::StacksAddress>,
) -> Option<BitcoinOperationKind> {
    match opcode {
        b'[' => parse_leader_block_commit(data),
        b'^' => parse_leader_key_registration(data),
        b'p' => Some(BitcoinOperationKind::PreStx {
            sender: outputs.first()?.recipient.as_stacks_address()?,
        }),
        b'x' => parse_stack_stx(data, outputs, sender?),
        b'$' => parse_transfer_stx(data, outputs, sender?),
        b'#' => parse_delegate_stx(data, outputs, sender?),
        b'v' => parse_vote_for_aggregate_key(data, sender?),
        _ => None,
    }
}

fn parse_leader_block_commit(data: &[u8]) -> Option<BitcoinOperationKind> {
    let block_header_hash = array(data.get(..32)?)?;
    let new_seed = array(data.get(32..64)?)?;
    let parent_block_height = u32::from_be_bytes(array(data.get(64..68)?)?);
    let parent_transaction_index = u16::from_be_bytes(array(data.get(68..70)?)?);
    let key_block_height = u32::from_be_bytes(array(data.get(70..74)?)?);
    let key_transaction_index = u16::from_be_bytes(array(data.get(74..76)?)?);
    let flags = *data.get(76)?;
    Some(BitcoinOperationKind::LeaderBlockCommit {
        block_header_hash,
        new_seed,
        parent_block_height,
        parent_transaction_index,
        key_block_height,
        key_transaction_index,
        memo: flags >> 3,
        parent_modulus: (flags & 0b111) % 5,
    })
}

fn parse_leader_key_registration(data: &[u8]) -> Option<BitcoinOperationKind> {
    let consensus_hash = array(data.get(..20)?)?;
    let vrf_public_key = array(data.get(20..52)?)?;
    nano_crypto::VrfPublicKey::from_bytes(vrf_public_key).ok()?;
    let memo = data.get(52..)?.to_vec();
    let block_signing_key_hash = memo.get(..20).and_then(array);
    Some(BitcoinOperationKind::LeaderKeyRegistration {
        consensus_hash,
        vrf_public_key,
        block_signing_key_hash,
        memo,
    })
}

fn parse_stack_stx(
    data: &[u8],
    outputs: &[BitcoinOutput],
    sender: nano_address::StacksAddress,
) -> Option<BitcoinOperationKind> {
    let amount = u128::from_be_bytes(array(data.get(..16)?)?);
    let cycles = *data.get(16)?;
    let signer_key = data.get(17..50).and_then(array);
    let max_amount = data.get(50..66).and_then(array).map(u128::from_be_bytes);
    let authorization_id = data.get(66..70).and_then(array).map(u32::from_be_bytes);
    Some(BitcoinOperationKind::StackStx {
        sender,
        reward_address: outputs.first()?.recipient.clone(),
        amount,
        cycles,
        signer_key,
        max_amount,
        authorization_id,
    })
}

fn parse_transfer_stx(
    data: &[u8],
    outputs: &[BitcoinOutput],
    sender: nano_address::StacksAddress,
) -> Option<BitcoinOperationKind> {
    if !(16..=77).contains(&data.len()) {
        return None;
    }
    Some(BitcoinOperationKind::TransferStx {
        sender,
        recipient: outputs.first()?.recipient.as_stacks_address()?,
        amount: u128::from_be_bytes(array(data.get(..16)?)?),
        memo: data.get(16..)?.to_vec(),
    })
}

fn parse_delegate_stx(
    data: &[u8],
    outputs: &[BitcoinOutput],
    sender: nano_address::StacksAddress,
) -> Option<BitcoinOperationKind> {
    let amount = u128::from_be_bytes(array(data.get(..16)?)?);
    let reward_address_output = match *data.get(16)? {
        0 => None,
        1 => Some(u32::from_be_bytes(array(data.get(17..21)?)?)),
        _ => return None,
    };
    let until_bitcoin_height = match *data.get(21)? {
        0 => None,
        1 => Some(u64::from_be_bytes(array(data.get(22..30)?)?)),
        _ => return None,
    };
    Some(BitcoinOperationKind::DelegateStx {
        sender,
        delegate: outputs.first()?.recipient.as_stacks_address()?,
        amount,
        reward_address: reward_address_output
            .and_then(|index| outputs.get(usize::try_from(index).ok()?).cloned())
            .map(|output| output.recipient),
        reward_address_output,
        until_bitcoin_height,
    })
}

fn parse_vote_for_aggregate_key(
    data: &[u8],
    sender: nano_address::StacksAddress,
) -> Option<BitcoinOperationKind> {
    if data.len() != 47 {
        return None;
    }
    Some(BitcoinOperationKind::VoteForAggregateKey {
        sender,
        signer_index: u16::from_be_bytes(array(data.get(..2)?)?),
        aggregate_key: array(data.get(2..35)?)?,
        round: u32::from_be_bytes(array(data.get(35..39)?)?),
        reward_cycle: u64::from_be_bytes(array(data.get(39..47)?)?),
    })
}

fn array<const N: usize>(bytes: &[u8]) -> Option<[u8; N]> {
    bytes.try_into().ok()
}

#[cfg(test)]
mod tests {
    /// A block's recorded hash and the hash a source reports for its height
    /// have to be the same bytes, or every read looks like a reorganization.
    ///
    /// They came from opposite ends: a `BitcoinBlock` stores the displayed
    /// order, Bitcoin Core's RPC hands back the internal order and is reversed,
    /// and Esplora hands back the displayed order and was reversed too. Against
    /// mainnet that made every block "no longer canonical" and stopped
    /// execution at the first one.
    #[test]
    fn a_block_hash_reads_the_same_from_either_source() {
        use bitcoin::hashes::Hash;

        // Mainnet block 960,231, as both sources present it.
        let displayed = "00000000000000000000e5a2b2a4dfa4d4f70e4e1e46e0e33f3e0c6a6f6a2e59";
        let hash = bitcoin::BlockHash::from_byte_array(super::bitcoin_hash_bytes(
            super::decode_block_hash(displayed).expect("the display hash decodes"),
        ));

        // What the RPC source does with what Bitcoin Core returns.
        let from_rpc = super::bitcoin_hash_bytes(hash.to_byte_array());
        // What the REST source does with what Esplora returns.
        let from_rest = super::decode_block_hash(displayed).expect("the display hash decodes");

        assert_eq!(from_rpc, from_rest, "the two sources agree on one block");
    }

    use std::{fs, path::Path};

    use bitcoin::{
        Amount, Block, BlockHash, CompactTarget, OutPoint, ScriptBuf, Transaction, TxIn,
        TxMerkleNode, TxOut,
        absolute::LockTime,
        block::{Header, Version as BlockVersion},
        consensus::serialize,
        hashes::Hash,
        transaction::Version as TransactionVersion,
    };

    use super::{
        BitcoinBlock, BitcoinOperationKind, BitcoinRpcSource, LeaderBlockCommitment,
        LeaderCommitmentTransactionError, LeaderKeyRegistration, LeaderKeyRegistrationError,
        PreStxCache, build_leader_commitment_transaction,
        build_leader_key_registration_transaction, decode_block, decode_block_with_pre_stx,
        parse_leader_block_commit, parse_leader_key_registration, protocol_payload,
    };

    #[test]
    fn captured_bitcoin_blocks_decode_with_hacknet_magic() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../nano-conformance/fixtures/bitcoin/blocks");
        let mut operation_count = 0;
        for entry in fs::read_dir(directory).expect("read fixture directory") {
            let path = entry.expect("fixture entry").path();
            let hex = fs::read_to_string(&path).expect("read fixture block");
            let bytes = hex::decode(hex.trim()).expect("decode fixture hex");
            let block = decode_block(0, &bytes, *b"T3")
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            assert_ne!(block.hash, [0; 32]);
            operation_count += block.operations.len();
        }
        assert!(operation_count > 0);
    }

    #[test]
    fn rpc_source_rewinds_to_the_fork_point_of_a_reorganization() {
        let mut source =
            BitcoinRpcSource::new("http://127.0.0.1:18443", "user", "password", *b"T3")
                .expect("create RPC source");
        let sender =
            nano_address::StacksAddress::new(26, nano_primitives::Hash160::from_bytes([0x24; 20]))
                .expect("valid Stacks address");
        source.last_height = Some(123);
        source.last_block = Some(BitcoinBlock {
            height: 123,
            hash: [0x42; 32],
            operations: Vec::new(),
        });
        source.pre_stx.senders.insert([0x01; 32], (sender, 120));
        source.pre_stx.senders.insert([0x02; 32], (sender, 122));

        source.invalidate_from(122);

        assert_eq!(source.last_height, Some(121));
        assert!(source.last_block.is_none());
        assert_eq!(
            source.pre_stx.senders.keys().collect::<Vec<_>>(),
            [&[0x01; 32]]
        );
    }

    #[test]
    fn prestx_pairings_of_a_reorganized_block_are_dropped() {
        let mut cache = PreStxCache::new();
        let pre_stx = transaction(
            vec![TxIn::default()],
            vec![protocol_output(b'p', &[]), p2pkh_output(0x24)],
        );
        let spend = TxIn {
            previous_output: OutPoint::new(pre_stx.compute_txid(), 1),
            ..TxIn::default()
        };
        decode_block_with_pre_stx(100, &block_bytes(vec![pre_stx]), *b"T3", &mut cache)
            .expect("valid Bitcoin block");
        cache.invalidate_from(100);

        let transfer = transaction(
            vec![spend],
            vec![protocol_output(b'$', &[0; 16]), p2pkh_output(0x42)],
        );
        let block =
            decode_block_with_pre_stx(101, &block_bytes(vec![transfer]), *b"T3", &mut cache)
                .expect("valid Bitcoin block");

        assert!(block.operations.is_empty());
    }

    #[test]
    fn prestx_cache_expires_after_six_bitcoin_blocks() {
        let mut cache = PreStxCache::new();
        let sender =
            nano_address::StacksAddress::new(26, nano_primitives::Hash160::from_bytes([0x24; 20]))
                .expect("valid Stacks address");
        cache.senders.insert([0x42; 32], (sender, 100));

        cache.retain_window(106);
        assert_eq!(cache.senders.len(), 1);
        cache.retain_window(107);
        assert!(cache.senders.is_empty());
    }

    #[test]
    fn prestx_sender_is_resolved_from_the_second_output() {
        let pre_stx = transaction(
            vec![TxIn::default()],
            vec![protocol_output(b'p', &[]), p2pkh_output(0x24)],
        );
        let transfer = transaction(
            vec![TxIn {
                previous_output: OutPoint::new(pre_stx.compute_txid(), 1),
                ..TxIn::default()
            }],
            vec![protocol_output(b'$', &[0; 16]), p2pkh_output(0x42)],
        );

        let block = decode_block(100, &block_bytes(vec![pre_stx, transfer]), *b"T3")
            .expect("valid Bitcoin block");
        assert_eq!(block.operations.len(), 2);
        match (&block.operations[0].kind, &block.operations[1].kind) {
            (
                BitcoinOperationKind::PreStx { sender },
                BitcoinOperationKind::TransferStx {
                    sender: transfer_sender,
                    ..
                },
            ) => assert_eq!(sender, transfer_sender),
            operations => panic!("unexpected operations: {operations:?}"),
        }
    }

    #[test]
    fn operations_require_an_input_and_a_decodable_output() {
        let transaction = transaction(vec![], vec![protocol_output(b'p', &[]), p2pkh_output(0x24)]);
        let block = decode_block(100, &block_bytes(vec![transaction]), *b"T3")
            .expect("valid Bitcoin block");
        assert!(block.operations.is_empty());
    }

    #[test]
    fn leader_key_registration_requires_a_valid_vrf_key() {
        assert!(parse_leader_key_registration(&[0; 52]).is_none());
    }

    #[test]
    fn leader_key_registration_round_trips_through_the_parser() {
        let registration = LeaderKeyRegistration {
            consensus_hash: [1; 20],
            vrf_public_key: nano_crypto::VrfPrivateKey::from_bytes([2; 32])
                .public_key()
                .to_bytes(),
            block_signing_key_hash: [3; 20],
            memo: vec![4; 5],
        };
        let payload = registration.encode().expect("encode registration");
        let Some(BitcoinOperationKind::LeaderKeyRegistration {
            consensus_hash,
            vrf_public_key,
            block_signing_key_hash,
            memo,
        }) = parse_leader_key_registration(&payload)
        else {
            panic!("parse leader-key registration");
        };
        assert_eq!(consensus_hash, registration.consensus_hash);
        assert_eq!(vrf_public_key, registration.vrf_public_key);
        assert_eq!(
            block_signing_key_hash,
            Some(registration.block_signing_key_hash)
        );
        assert_eq!(
            memo,
            [registration.block_signing_key_hash.to_vec(), vec![4; 5]].concat()
        );
    }

    #[test]
    fn leader_key_registration_uses_a_single_protocol_output() {
        let registration = LeaderKeyRegistration {
            consensus_hash: [1; 20],
            vrf_public_key: nano_crypto::VrfPrivateKey::from_bytes([2; 32])
                .public_key()
                .to_bytes(),
            block_signing_key_hash: [3; 20],
            memo: Vec::new(),
        };
        let transaction =
            build_leader_key_registration_transaction(*b"T3", &registration, Vec::new())
                .expect("build registration");
        assert_eq!(transaction.output.len(), 1);
        let (opcode, payload) =
            protocol_payload(transaction.output[0].script_pubkey.as_script(), *b"T3")
                .expect("protocol output");
        assert_eq!(opcode, b'^');
        assert_eq!(payload, registration.encode().expect("encode registration"));
    }

    #[test]
    fn leader_key_registration_rejects_a_memo_that_exceeds_op_return_capacity() {
        let registration = LeaderKeyRegistration {
            consensus_hash: [1; 20],
            vrf_public_key: nano_crypto::VrfPrivateKey::from_bytes([2; 32])
                .public_key()
                .to_bytes(),
            block_signing_key_hash: [3; 20],
            memo: vec![4; 6],
        };
        assert_eq!(
            registration.encode(),
            Err(LeaderKeyRegistrationError::MemoTooLarge)
        );
    }

    #[test]
    fn leader_commitment_payload_round_trips_through_the_parser() {
        let commitment = LeaderBlockCommitment {
            block_header_hash: [1; 32],
            new_seed: [2; 32],
            parent_block_height: 3,
            parent_transaction_index: 4,
            key_block_height: 5,
            key_transaction_index: 6,
            memo: 7,
            parent_modulus: 4,
        };
        let payload = commitment.encode().expect("encode commitment");
        let Some(BitcoinOperationKind::LeaderBlockCommit {
            block_header_hash,
            new_seed,
            parent_block_height,
            parent_transaction_index,
            key_block_height,
            key_transaction_index,
            memo,
            parent_modulus,
        }) = parse_leader_block_commit(&payload)
        else {
            panic!("parse leader commitment");
        };
        assert_eq!(block_header_hash, [1; 32]);
        assert_eq!(new_seed, [2; 32]);
        assert_eq!(parent_block_height, 3);
        assert_eq!(parent_transaction_index, 4);
        assert_eq!(key_block_height, 5);
        assert_eq!(key_transaction_index, 6);
        assert_eq!(memo, 7);
        assert_eq!(parent_modulus, 4);
    }

    #[test]
    fn waterfall_commitment_places_the_payout_before_change() {
        let commitment = LeaderBlockCommitment {
            block_header_hash: [1; 32],
            new_seed: [2; 32],
            parent_block_height: 3,
            parent_transaction_index: 4,
            key_block_height: 5,
            key_transaction_index: 6,
            memo: 7,
            parent_modulus: 4,
        };
        let payout = p2tr_address(0x42);
        let change = p2wpkh_address(0x24);
        let transaction = build_leader_commitment_transaction(
            *b"T3",
            commitment,
            vec![TxIn::default()],
            &payout,
            Amount::from_sat(12_345),
            Some((&change, Amount::from_sat(54_321))),
        )
        .expect("build leader commitment");

        assert_eq!(transaction.output.len(), 3);
        assert_eq!(transaction.output[1].value, Amount::from_sat(12_345));
        assert_eq!(transaction.output[1].script_pubkey, payout.script_pubkey());
        assert_eq!(transaction.output[2].value, Amount::from_sat(54_321));
        assert_eq!(transaction.output[2].script_pubkey, change.script_pubkey());
        let (opcode, payload) =
            super::protocol_payload(transaction.output[0].script_pubkey.as_script(), *b"T3")
                .expect("protocol payload");
        assert_eq!(opcode, b'[');
        assert_eq!(payload, commitment.encode().expect("commitment payload"));
    }

    #[test]
    fn waterfall_commitment_allows_wallet_funding_and_rejects_zero_amounts() {
        let commitment = LeaderBlockCommitment {
            block_header_hash: [1; 32],
            new_seed: [2; 32],
            parent_block_height: 3,
            parent_transaction_index: 4,
            key_block_height: 5,
            key_transaction_index: 6,
            memo: 7,
            parent_modulus: 4,
        };
        let payout = p2tr_address(0x42);
        let template = build_leader_commitment_transaction(
            *b"T3",
            commitment,
            vec![],
            &payout,
            Amount::from_sat(1),
            None,
        )
        .expect("build fundable transaction template");
        assert!(template.input.is_empty());
        assert_eq!(
            build_leader_commitment_transaction(
                *b"T3",
                commitment,
                vec![TxIn::default()],
                &payout,
                Amount::ZERO,
                None,
            ),
            Err(LeaderCommitmentTransactionError::ZeroCommitmentAmount)
        );
    }

    fn transaction(input: Vec<TxIn>, output: Vec<TxOut>) -> Transaction {
        Transaction {
            version: TransactionVersion::TWO,
            lock_time: LockTime::ZERO,
            input,
            output,
        }
    }

    fn protocol_output(opcode: u8, payload: &[u8]) -> TxOut {
        let mut data = Vec::with_capacity(payload.len() + 3);
        data.extend_from_slice(b"T3");
        data.push(opcode);
        data.extend_from_slice(payload);
        let length = u8::try_from(data.len()).expect("test packet fits direct push");
        TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::from_bytes([vec![0x6a, length], data].concat()),
        }
    }

    fn p2pkh_output(byte: u8) -> TxOut {
        TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::from_bytes(
                [vec![0x76, 0xa9, 0x14], vec![byte; 20], vec![0x88, 0xac]].concat(),
            ),
        }
    }

    fn p2tr_address(byte: u8) -> nano_address::PoxAddress {
        nano_address::PoxAddress::Addr32 {
            mainnet: false,
            address_type: nano_address::PoxAddressType32::P2tr,
            bytes: [byte; 32],
        }
    }

    fn p2wpkh_address(byte: u8) -> nano_address::PoxAddress {
        nano_address::PoxAddress::Addr20 {
            mainnet: false,
            address_type: nano_address::PoxAddressType20::P2wpkh,
            bytes: [byte; 20],
        }
    }

    fn block_bytes(transactions: Vec<Transaction>) -> Vec<u8> {
        serialize(&Block {
            header: Header {
                version: BlockVersion::ONE,
                prev_blockhash: BlockHash::all_zeros(),
                merkle_root: TxMerkleNode::all_zeros(),
                time: 0,
                bits: CompactTarget::from_consensus(0),
                nonce: 0,
            },
            txdata: transactions,
        })
    }
}

#[cfg(test)]
mod rest_tests {
    use super::BitcoinRestSource;

    /// Reads a real mainnet burn block over Esplora.
    ///
    /// Ignored by default: it needs the network. It is the check that a node
    /// with no Bitcoin of its own can still see the burnchain.
    #[test]
    #[ignore = "reads mempool.space"]
    fn reads_a_mainnet_burn_block() {
        let mut source = BitcoinRestSource::new("https://mempool.space/api", *b"X2")
            .expect("build the source");
        // A burn block just past the epoch 4.0 boundary.
        let block = source.block_at(960_240).expect("read the block");
        assert_eq!(block.height, 960_240);
        assert!(
            !block.hash.iter().all(|byte| *byte == 0),
            "the block carries its hash"
        );
    }
}
