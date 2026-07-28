#![forbid(unsafe_code)]

use std::fmt;

use nano_address::StacksAddress;
use nano_crypto::{MessageSignature, StacksPrivateKey, StacksPublicKey, VrfProof};
use nano_primitives::{
    ConsensusHash, Hash160, Sha256Sum, StacksBlockId, hash160, sha256, sha512_256,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodecError {
    EndOfInput,
    InvalidAuth,
    InvalidCondition,
    InvalidField,
    InvalidKey,
    InvalidLength,
    InvalidTransaction,
    InvalidPayload,
    InvalidPostCondition,
    InvalidPrincipal,
    InvalidName,
    InvalidString,
    InvalidClarityValue,
}

impl fmt::Display for CodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EndOfInput => "unexpected end of input",
            Self::InvalidAuth => "invalid transaction authorization",
            Self::InvalidCondition => "invalid spending condition",
            Self::InvalidField => "invalid authorization field",
            Self::InvalidKey => "invalid public key encoding",
            Self::InvalidLength => "invalid encoded length",
            Self::InvalidTransaction => "invalid transaction",
            Self::InvalidPayload => "invalid transaction payload",
            Self::InvalidPostCondition => "invalid transaction post-condition",
            Self::InvalidPrincipal => "invalid principal",
            Self::InvalidName => "invalid Clarity name",
            Self::InvalidString => "invalid Stacks string",
            Self::InvalidClarityValue => "invalid Clarity value",
        })
    }
}

impl std::error::Error for CodecError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyEncoding {
    Compressed,
    Uncompressed,
}

impl KeyEncoding {
    const fn byte(self) -> u8 {
        match self {
            Self::Compressed => 0,
            Self::Uncompressed => 1,
        }
    }
    const fn parse(value: u8) -> Result<Self, CodecError> {
        match value {
            0 => Ok(Self::Compressed),
            1 => Ok(Self::Uncompressed),
            _ => Err(CodecError::InvalidKey),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SinglesigHashMode {
    P2pkh,
    P2wpkh,
}

impl SinglesigHashMode {
    const fn byte(self) -> u8 {
        match self {
            Self::P2pkh => 0,
            Self::P2wpkh => 2,
        }
    }
    const fn parse(value: u8) -> Result<Self, CodecError> {
        match value {
            0 => Ok(Self::P2pkh),
            2 => Ok(Self::P2wpkh),
            _ => Err(CodecError::InvalidCondition),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MultisigHashMode {
    P2sh,
    P2wsh,
}

impl MultisigHashMode {
    const fn byte(self) -> u8 {
        match self {
            Self::P2sh => 1,
            Self::P2wsh => 3,
        }
    }
    const fn parse(value: u8) -> Result<Self, CodecError> {
        match value {
            1 => Ok(Self::P2sh),
            3 => Ok(Self::P2wsh),
            _ => Err(CodecError::InvalidCondition),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OrderIndependentMultisigHashMode {
    P2sh,
    P2wsh,
}

impl OrderIndependentMultisigHashMode {
    const fn byte(self) -> u8 {
        match self {
            Self::P2sh => 5,
            Self::P2wsh => 7,
        }
    }
    const fn parse(value: u8) -> Result<Self, CodecError> {
        match value {
            5 => Ok(Self::P2sh),
            7 => Ok(Self::P2wsh),
            _ => Err(CodecError::InvalidCondition),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthField {
    PublicKey {
        encoding: KeyEncoding,
        bytes: Vec<u8>,
    },
    Signature {
        encoding: KeyEncoding,
        signature: MessageSignature,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SinglesigCondition {
    pub hash_mode: SinglesigHashMode,
    pub signer: Hash160,
    pub nonce: u64,
    pub fee: u64,
    pub key_encoding: KeyEncoding,
    pub signature: MessageSignature,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MultisigCondition {
    pub hash_mode: MultisigHashMode,
    pub signer: Hash160,
    pub nonce: u64,
    pub fee: u64,
    pub fields: Vec<AuthField>,
    pub signatures_required: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrderIndependentMultisigCondition {
    pub hash_mode: OrderIndependentMultisigHashMode,
    pub signer: Hash160,
    pub nonce: u64,
    pub fee: u64,
    pub fields: Vec<AuthField>,
    pub signatures_required: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpendingCondition {
    Singlesig(SinglesigCondition),
    Multisig(MultisigCondition),
    OrderIndependentMultisig(OrderIndependentMultisigCondition),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransactionAuth {
    Standard(SpendingCondition),
    Sponsored {
        origin: SpendingCondition,
        sponsor: SpendingCondition,
    },
}

impl TransactionAuth {
    pub fn decode(bytes: &[u8]) -> Result<(Self, usize), CodecError> {
        let mut reader = Reader::new(bytes);
        let auth = match reader.byte()? {
            4 => Self::Standard(SpendingCondition::decode(&mut reader)?),
            5 => Self::Sponsored {
                origin: SpendingCondition::decode(&mut reader)?,
                sponsor: SpendingCondition::decode(&mut reader)?,
            },
            _ => return Err(CodecError::InvalidAuth),
        };
        Ok((auth, reader.position()))
    }
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::default();
        match self {
            Self::Standard(condition) => {
                writer.byte(4);
                condition.encode(&mut writer);
            }
            Self::Sponsored { origin, sponsor } => {
                writer.byte(5);
                origin.encode(&mut writer);
                sponsor.encode(&mut writer);
            }
        }
        writer.finish()
    }

    #[must_use]
    pub const fn origin(&self) -> &SpendingCondition {
        match self {
            Self::Standard(origin) | Self::Sponsored { origin, .. } => origin,
        }
    }

    #[must_use]
    pub const fn sponsor(&self) -> Option<&SpendingCondition> {
        match self {
            Self::Standard(_) => None,
            Self::Sponsored { sponsor, .. } => Some(sponsor),
        }
    }

    #[must_use]
    pub const fn payer(&self) -> &SpendingCondition {
        match self {
            Self::Standard(origin) => origin,
            Self::Sponsored { sponsor, .. } => sponsor,
        }
    }

    const fn initial_sighash_auth(&self) -> Self {
        match self {
            Self::Standard(origin) => Self::Standard(origin.cleared()),
            Self::Sponsored { origin, .. } => Self::Sponsored {
                origin: origin.cleared(),
                sponsor: SpendingCondition::Singlesig(SinglesigCondition {
                    hash_mode: SinglesigHashMode::P2pkh,
                    signer: Hash160::from_bytes([0; 20]),
                    nonce: 0,
                    fee: 0,
                    key_encoding: KeyEncoding::Compressed,
                    signature: MessageSignature::from_bytes([0; 65]),
                }),
            },
        }
    }

    fn verify(&self, initial_sighash: Sha256Sum) -> Result<(), CodecError> {
        let origin_sighash = self.origin().verify(initial_sighash, ORIGIN_AUTH_FLAG)?;
        if let Some(sponsor) = self.sponsor() {
            sponsor.verify(origin_sighash, SPONSOR_AUTH_FLAG)?;
        }
        Ok(())
    }
}

/// A complete SIP-005 transaction, retained in its canonical consensus encoding.
///
/// Transaction payloads contain Clarity values.  Their typed interpretation belongs
/// to the VM boundary, but this codec validates their wire format before retaining
/// the exact bytes needed for hashing, signing, and forwarding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transaction {
    bytes: Vec<u8>,
    version: TransactionVersion,
    chain_id: u32,
    auth: TransactionAuth,
    anchor_mode: AnchorMode,
    post_condition_mode: PostConditionMode,
    post_conditions: Vec<PostCondition>,
    payload: TransactionPayload,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionVersion {
    Mainnet,
    Testnet,
    Other(u8),
}

impl TransactionVersion {
    const fn parse(value: u8) -> Self {
        match value {
            0x00 => Self::Mainnet,
            0x80 => Self::Testnet,
            other => Self::Other(other),
        }
    }

    const fn byte(self) -> u8 {
        match self {
            Self::Mainnet => 0,
            Self::Testnet => 0x80,
            Self::Other(value) => value,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnchorMode {
    OnChainOnly,
    OffChainOnly,
    Any,
}

impl AnchorMode {
    const fn parse(value: u8) -> Result<Self, CodecError> {
        match value {
            1 => Ok(Self::OnChainOnly),
            2 => Ok(Self::OffChainOnly),
            3 => Ok(Self::Any),
            _ => Err(CodecError::InvalidTransaction),
        }
    }

    const fn byte(self) -> u8 {
        match self {
            Self::OnChainOnly => 1,
            Self::OffChainOnly => 2,
            Self::Any => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PostConditionMode {
    Deny,
    Allow,
    Originator,
}

impl PostConditionMode {
    const fn parse(value: u8) -> Result<Self, CodecError> {
        match value {
            1 => Ok(Self::Allow),
            2 => Ok(Self::Deny),
            3 => Ok(Self::Originator),
            _ => Err(CodecError::InvalidTransaction),
        }
    }

    const fn byte(self) -> u8 {
        match self {
            Self::Allow => 1,
            Self::Deny => 2,
            Self::Originator => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionPayloadType {
    TokenTransfer,
    SmartContract,
    ContractCall,
    PoisonMicroblock,
    Coinbase,
    CoinbaseToAltRecipient,
    VersionedSmartContract,
    TenureChange,
    NakamotoCoinbase,
}

impl TransactionPayloadType {
    const fn parse(value: u8) -> Result<Self, CodecError> {
        match value {
            0 => Ok(Self::TokenTransfer),
            1 => Ok(Self::SmartContract),
            2 => Ok(Self::ContractCall),
            3 => Ok(Self::PoisonMicroblock),
            4 => Ok(Self::Coinbase),
            5 => Ok(Self::CoinbaseToAltRecipient),
            6 => Ok(Self::VersionedSmartContract),
            7 => Ok(Self::TenureChange),
            8 => Ok(Self::NakamotoCoinbase),
            _ => Err(CodecError::InvalidPayload),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Principal {
    Standard(StacksAddress),
    Contract {
        address: StacksAddress,
        contract_name: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PostConditionPrincipal {
    Origin,
    Standard(StacksAddress),
    Contract {
        address: StacksAddress,
        contract_name: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetInfo {
    pub address: StacksAddress,
    pub contract_name: String,
    pub asset_name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FungibleCondition {
    SentEqual,
    SentGreater,
    SentGreaterEqual,
    SentLess,
    SentLessEqual,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NonFungibleCondition {
    DoesNotSend,
    DoesSend,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PoxCondition {
    NotPerformed,
    MaybePerformed,
    Performed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PostConditionData {
    Stx {
        principal: PostConditionPrincipal,
        condition: FungibleCondition,
        amount: u64,
    },
    Fungible {
        principal: PostConditionPrincipal,
        asset: AssetInfo,
        condition: FungibleCondition,
        amount: u64,
    },
    NonFungible {
        principal: PostConditionPrincipal,
        asset: AssetInfo,
        asset_value: ClarityValue,
        condition: NonFungibleCondition,
    },
    Staking {
        principal: PostConditionPrincipal,
        condition: FungibleCondition,
        amount: u64,
    },
    Pox {
        principal: PostConditionPrincipal,
        condition: PoxCondition,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostCondition {
    data: PostConditionData,
    bytes: Vec<u8>,
}

impl PostCondition {
    #[must_use]
    pub const fn data(&self) -> &PostConditionData {
        &self.data
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClarityValue(Vec<u8>);

impl ClarityValue {
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClarityVersion {
    Clarity1,
    Clarity2,
    Clarity3,
    Clarity4,
    Clarity5,
    Clarity6,
}

impl ClarityVersion {
    const fn parse(value: u8) -> Result<Self, CodecError> {
        match value {
            1 => Ok(Self::Clarity1),
            2 => Ok(Self::Clarity2),
            3 => Ok(Self::Clarity3),
            4 => Ok(Self::Clarity4),
            5 => Ok(Self::Clarity5),
            6 => Ok(Self::Clarity6),
            _ => Err(CodecError::InvalidPayload),
        }
    }

    const fn byte(self) -> u8 {
        match self {
            Self::Clarity1 => 1,
            Self::Clarity2 => 2,
            Self::Clarity3 => 3,
            Self::Clarity4 => 4,
            Self::Clarity5 => 5,
            Self::Clarity6 => 6,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MicroblockHeader {
    pub sequence: u16,
    pub previous_block: StacksBlockId,
    bytes: [u8; 132],
}

impl MicroblockHeader {
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 132] {
        &self.bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TenureChangeCause {
    BlockFound,
    Extended,
    ExtendedRuntime,
    ExtendedReadCount,
    ExtendedReadLength,
    ExtendedWriteCount,
    ExtendedWriteLength,
}

impl TenureChangeCause {
    const fn parse(value: u8) -> Result<Self, CodecError> {
        match value {
            0 => Ok(Self::BlockFound),
            1 => Ok(Self::Extended),
            2 => Ok(Self::ExtendedRuntime),
            3 => Ok(Self::ExtendedReadCount),
            4 => Ok(Self::ExtendedReadLength),
            5 => Ok(Self::ExtendedWriteCount),
            6 => Ok(Self::ExtendedWriteLength),
            _ => Err(CodecError::InvalidPayload),
        }
    }

    const fn byte(self) -> u8 {
        match self {
            Self::BlockFound => 0,
            Self::Extended => 1,
            Self::ExtendedRuntime => 2,
            Self::ExtendedReadCount => 3,
            Self::ExtendedReadLength => 4,
            Self::ExtendedWriteCount => 5,
            Self::ExtendedWriteLength => 6,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TenureChangePayload {
    pub tenure_consensus_hash: ConsensusHash,
    pub previous_tenure_consensus_hash: ConsensusHash,
    pub bitcoin_view_consensus_hash: ConsensusHash,
    pub previous_tenure_end: StacksBlockId,
    pub previous_tenure_blocks: u32,
    pub cause: TenureChangeCause,
    pub public_key_hash: Hash160,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransactionPayloadData {
    TokenTransfer {
        recipient: Principal,
        amount: u64,
        memo: [u8; 34],
    },
    SmartContract {
        contract_name: String,
        source: String,
    },
    ContractCall {
        address: StacksAddress,
        contract_name: String,
        function_name: String,
        arguments: Vec<ClarityValue>,
    },
    PoisonMicroblock {
        first: MicroblockHeader,
        second: MicroblockHeader,
    },
    Coinbase {
        payload: [u8; 32],
    },
    CoinbaseToAltRecipient {
        payload: [u8; 32],
        recipient: Principal,
    },
    VersionedSmartContract {
        clarity_version: ClarityVersion,
        contract_name: String,
        source: String,
    },
    TenureChange(TenureChangePayload),
    NakamotoCoinbase {
        payload: [u8; 32],
        recipient: Option<Principal>,
        vrf_proof: [u8; 80],
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionPayload {
    data: TransactionPayloadData,
    bytes: Vec<u8>,
}

impl TransactionPayload {
    /// Encode a payload so it can be placed in a newly built transaction.
    #[must_use]
    pub fn new(data: TransactionPayloadData) -> Self {
        let mut writer = Writer::default();
        encode_payload(&mut writer, &data);
        Self {
            bytes: writer.finish(),
            data,
        }
    }

    #[must_use]
    pub const fn data(&self) -> &TransactionPayloadData {
        &self.data
    }

    #[must_use]
    pub const fn kind(&self) -> TransactionPayloadType {
        match &self.data {
            TransactionPayloadData::TokenTransfer { .. } => TransactionPayloadType::TokenTransfer,
            TransactionPayloadData::SmartContract { .. } => TransactionPayloadType::SmartContract,
            TransactionPayloadData::ContractCall { .. } => TransactionPayloadType::ContractCall,
            TransactionPayloadData::PoisonMicroblock { .. } => {
                TransactionPayloadType::PoisonMicroblock
            }
            TransactionPayloadData::Coinbase { .. } => TransactionPayloadType::Coinbase,
            TransactionPayloadData::CoinbaseToAltRecipient { .. } => {
                TransactionPayloadType::CoinbaseToAltRecipient
            }
            TransactionPayloadData::VersionedSmartContract { .. } => {
                TransactionPayloadType::VersionedSmartContract
            }
            TransactionPayloadData::TenureChange(_) => TransactionPayloadType::TenureChange,
            TransactionPayloadData::NakamotoCoinbase { .. } => {
                TransactionPayloadType::NakamotoCoinbase
            }
        }
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Transaction {
    pub fn decode(bytes: &[u8]) -> Result<(Self, usize), CodecError> {
        let mut reader = Reader::new(bytes);
        let version = TransactionVersion::parse(reader.byte()?);
        let chain_id = reader.u32()?;

        let auth_start = reader.position();
        let (auth, auth_length) = TransactionAuth::decode(&bytes[auth_start..])?;
        reader.take(auth_length)?;

        let anchor_mode = AnchorMode::parse(reader.byte()?)?;
        let post_condition_mode = PostConditionMode::parse(reader.byte()?)?;

        let post_condition_count = reader.u32()?;
        let post_condition_count =
            usize::try_from(post_condition_count).map_err(|_| CodecError::InvalidLength)?;
        if post_condition_count > reader.remaining() / 11 {
            return Err(CodecError::InvalidLength);
        }
        let mut post_conditions = Vec::with_capacity(post_condition_count);
        for _ in 0..post_condition_count {
            let start = reader.position();
            let data = read_post_condition(&mut reader)?;
            post_conditions.push(PostCondition {
                data,
                bytes: reader.bytes[start..reader.position()].to_vec(),
            });
        }
        let payload_start = reader.position();
        let data = read_payload(&mut reader)?;
        let payload = TransactionPayload {
            data,
            bytes: reader.bytes[payload_start..reader.position()].to_vec(),
        };

        let length = reader.position();
        Ok((
            Self {
                bytes: bytes[..length].to_vec(),
                version,
                chain_id,
                auth,
                anchor_mode,
                post_condition_mode,
                post_conditions,
                payload,
            },
            length,
        ))
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::default();
        writer.byte(self.version.byte());
        writer.u32(self.chain_id);
        writer.raw(&self.auth.encode());
        writer.byte(self.anchor_mode.byte());
        writer.byte(self.post_condition_mode.byte());
        writer
            .u32(u32::try_from(self.post_conditions.len()).expect("post-condition count fits u32"));
        for post_condition in &self.post_conditions {
            encode_post_condition(&mut writer, post_condition.data());
        }
        encode_payload(&mut writer, self.payload.data());
        writer.finish()
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    pub const fn version(&self) -> TransactionVersion {
        self.version
    }

    #[must_use]
    pub const fn chain_id(&self) -> u32 {
        self.chain_id
    }

    #[must_use]
    pub const fn auth(&self) -> &TransactionAuth {
        &self.auth
    }

    #[must_use]
    pub const fn anchor_mode(&self) -> AnchorMode {
        self.anchor_mode
    }

    #[must_use]
    pub const fn post_condition_mode(&self) -> PostConditionMode {
        self.post_condition_mode
    }

    #[must_use]
    pub fn post_condition_count(&self) -> usize {
        self.post_conditions.len()
    }

    #[must_use]
    pub fn post_conditions(&self) -> &[PostCondition] {
        &self.post_conditions
    }

    #[must_use]
    pub const fn payload(&self) -> &TransactionPayload {
        &self.payload
    }

    #[must_use]
    pub const fn payload_type(&self) -> TransactionPayloadType {
        self.payload.kind()
    }

    #[must_use]
    pub fn txid(&self) -> Sha256Sum {
        sha512_256(&self.encode())
    }

    /// Build a transaction authorized by one standard, compressed secp256k1 key.
    pub fn sign_standard(
        version: TransactionVersion,
        chain_id: u32,
        anchor_mode: AnchorMode,
        key: &StacksPrivateKey,
        nonce: u64,
        fee: u64,
        payload: TransactionPayloadData,
    ) -> Result<Self, CodecError> {
        let public_key = key.public_key().to_bytes_compressed().to_vec();
        let mut transaction = Self {
            bytes: Vec::new(),
            version,
            chain_id,
            auth: TransactionAuth::Standard(SpendingCondition::Singlesig(SinglesigCondition {
                hash_mode: SinglesigHashMode::P2pkh,
                signer: address_hash(AddressHashMode::P2pkh, 1, &[public_key])?,
                nonce,
                fee,
                key_encoding: KeyEncoding::Compressed,
                signature: MessageSignature::from_bytes([0; 65]),
            })),
            anchor_mode,
            post_condition_mode: PostConditionMode::Deny,
            post_conditions: Vec::new(),
            payload: TransactionPayload::new(payload),
        };
        let mut cleared = transaction.clone();
        cleared.auth = transaction.auth.initial_sighash_auth();
        let presign = presign_sighash(cleared.txid(), ORIGIN_AUTH_FLAG, fee, nonce);
        let signature = key.sign(presign.as_bytes());
        let TransactionAuth::Standard(SpendingCondition::Singlesig(condition)) =
            &mut transaction.auth
        else {
            unreachable!("the authorization was just built as a standard single signature")
        };
        condition.signature = signature;
        transaction.bytes = transaction.encode();
        transaction.verify_authorization()?;
        Ok(transaction)
    }

    /// Verify the origin and, when present, sponsor authorizations for this transaction.
    pub fn verify_authorization(&self) -> Result<(), CodecError> {
        let mut initial = self.clone();
        initial.auth = self.auth.initial_sighash_auth();
        self.auth.verify(initial.txid())
    }

    /// Return the origin address implied by the authorization and transaction network.
    #[must_use]
    pub fn origin_address(&self) -> Option<StacksAddress> {
        self.network_mainnet()
            .map(|mainnet| self.auth.origin().account_address(mainnet))
    }

    /// Return the sponsor address for a sponsored transaction.
    #[must_use]
    pub fn sponsor_address(&self) -> Option<StacksAddress> {
        self.network_mainnet().and_then(|mainnet| {
            self.auth
                .sponsor()
                .map(|sponsor| sponsor.account_address(mainnet))
        })
    }

    const fn network_mainnet(&self) -> Option<bool> {
        match self.version {
            TransactionVersion::Mainnet => Some(true),
            TransactionVersion::Testnet => Some(false),
            TransactionVersion::Other(_) => None,
        }
    }
}

/// Calculate the tagged Merkle root committed by a block header.
#[must_use]
pub fn transaction_merkle_root(transactions: &[Transaction]) -> Sha256Sum {
    if transactions.is_empty() {
        return Sha256Sum::default();
    }

    let mut level: Vec<_> = transactions
        .iter()
        .map(|transaction| tagged_hash(0, transaction.txid().as_bytes()))
        .collect();

    if level.len() % 2 != 0 {
        level.push(*level.last().expect("non-empty level"));
    }

    while level.len() > 1 {
        if level.len() % 2 != 0 {
            level.push(*level.last().expect("non-empty level"));
        }
        level = level
            .chunks_exact(2)
            .map(|pair| {
                let mut bytes = [0_u8; 64];
                bytes[..32].copy_from_slice(pair[0].as_bytes());
                bytes[32..].copy_from_slice(pair[1].as_bytes());
                tagged_hash(1, &bytes)
            })
            .collect();
    }

    level[0]
}

fn tagged_hash(tag: u8, data: &[u8]) -> Sha256Sum {
    let mut bytes = Vec::with_capacity(data.len() + 1);
    bytes.push(tag);
    bytes.extend_from_slice(data);
    sha512_256(&bytes)
}

fn read_payload(reader: &mut Reader<'_>) -> Result<TransactionPayloadData, CodecError> {
    match TransactionPayloadType::parse(reader.byte()?)? {
        TransactionPayloadType::TokenTransfer => Ok(TransactionPayloadData::TokenTransfer {
            recipient: read_principal(reader)?,
            amount: reader.u64()?,
            memo: reader.take(34)?.try_into().expect("fixed slice"),
        }),
        TransactionPayloadType::SmartContract => Ok(TransactionPayloadData::SmartContract {
            contract_name: read_name(reader)?,
            source: read_stacks_string(reader)?,
        }),
        TransactionPayloadType::ContractCall => {
            let address = read_address(reader)?;
            let contract_name = read_name(reader)?;
            let function_name = read_name(reader)?;
            let count = usize::try_from(reader.u32()?).map_err(|_| CodecError::InvalidLength)?;
            if count > reader.remaining() {
                return Err(CodecError::InvalidLength);
            }
            let mut arguments = Vec::with_capacity(count);
            for _ in 0..count {
                arguments.push(read_clarity_value(reader)?);
            }
            Ok(TransactionPayloadData::ContractCall {
                address,
                contract_name,
                function_name,
                arguments,
            })
        }
        TransactionPayloadType::PoisonMicroblock => {
            let first = read_microblock_header(reader)?;
            let second = read_microblock_header(reader)?;
            if first.bytes == second.bytes
                || (first.sequence != second.sequence
                    && first.previous_block != second.previous_block)
            {
                return Err(CodecError::InvalidPayload);
            }
            Ok(TransactionPayloadData::PoisonMicroblock { first, second })
        }
        TransactionPayloadType::Coinbase => Ok(TransactionPayloadData::Coinbase {
            payload: reader.take(32)?.try_into().expect("fixed slice"),
        }),
        TransactionPayloadType::CoinbaseToAltRecipient => {
            Ok(TransactionPayloadData::CoinbaseToAltRecipient {
                payload: reader.take(32)?.try_into().expect("fixed slice"),
                recipient: read_principal(reader)?,
            })
        }
        TransactionPayloadType::VersionedSmartContract => {
            Ok(TransactionPayloadData::VersionedSmartContract {
                clarity_version: ClarityVersion::parse(reader.byte()?)?,
                contract_name: read_name(reader)?,
                source: read_stacks_string(reader)?,
            })
        }
        TransactionPayloadType::TenureChange => {
            Ok(TransactionPayloadData::TenureChange(TenureChangePayload {
                tenure_consensus_hash: read_consensus_hash(reader)?,
                previous_tenure_consensus_hash: read_consensus_hash(reader)?,
                bitcoin_view_consensus_hash: read_consensus_hash(reader)?,
                previous_tenure_end: read_block_id(reader)?,
                previous_tenure_blocks: reader.u32()?,
                cause: TenureChangeCause::parse(reader.byte()?)?,
                public_key_hash: reader.hash160()?,
            }))
        }
        TransactionPayloadType::NakamotoCoinbase => {
            let payload = reader.take(32)?.try_into().expect("fixed slice");
            let recipient = match reader.byte()? {
                9 => None,
                10 => Some(read_principal(reader)?),
                _ => return Err(CodecError::InvalidPayload),
            };
            let vrf_proof: [u8; 80] = reader.take(80)?.try_into().expect("fixed slice");
            VrfProof::from_bytes(&vrf_proof).map_err(|_| CodecError::InvalidPayload)?;
            Ok(TransactionPayloadData::NakamotoCoinbase {
                payload,
                recipient,
                vrf_proof,
            })
        }
    }
}

fn read_microblock_header(reader: &mut Reader<'_>) -> Result<MicroblockHeader, CodecError> {
    let bytes: [u8; 132] = reader.take(132)?.try_into().expect("fixed slice");
    Ok(MicroblockHeader {
        sequence: u16::from_be_bytes(bytes[1..3].try_into().expect("fixed slice")),
        previous_block: StacksBlockId::from_bytes(bytes[3..35].try_into().expect("fixed slice")),
        bytes,
    })
}

fn read_post_condition(reader: &mut Reader<'_>) -> Result<PostConditionData, CodecError> {
    match reader.byte()? {
        0 => Ok(PostConditionData::Stx {
            principal: read_post_condition_principal(reader)?,
            condition: read_fungible_condition(reader)?,
            amount: reader.u64()?,
        }),
        1 => Ok(PostConditionData::Fungible {
            principal: read_post_condition_principal(reader)?,
            asset: read_asset_info(reader)?,
            condition: read_fungible_condition(reader)?,
            amount: reader.u64()?,
        }),
        2 => Ok(PostConditionData::NonFungible {
            principal: read_post_condition_principal(reader)?,
            asset: read_asset_info(reader)?,
            asset_value: read_clarity_value(reader)?,
            condition: match reader.byte()? {
                0x10 => NonFungibleCondition::DoesSend,
                0x11 => NonFungibleCondition::DoesNotSend,
                _ => return Err(CodecError::InvalidPostCondition),
            },
        }),
        3 => Ok(PostConditionData::Staking {
            principal: read_post_condition_principal(reader)?,
            condition: read_fungible_condition(reader)?,
            amount: reader.u64()?,
        }),
        4 => Ok(PostConditionData::Pox {
            principal: read_post_condition_principal(reader)?,
            condition: match reader.byte()? {
                0x30 => PoxCondition::NotPerformed,
                0x31 => PoxCondition::MaybePerformed,
                0x32 => PoxCondition::Performed,
                _ => return Err(CodecError::InvalidPostCondition),
            },
        }),
        _ => Err(CodecError::InvalidPostCondition),
    }
}

fn read_post_condition_principal(
    reader: &mut Reader<'_>,
) -> Result<PostConditionPrincipal, CodecError> {
    match reader.byte()? {
        1 => Ok(PostConditionPrincipal::Origin),
        2 => Ok(PostConditionPrincipal::Standard(read_address(reader)?)),
        3 => Ok(PostConditionPrincipal::Contract {
            address: read_address(reader)?,
            contract_name: read_name(reader)?,
        }),
        _ => Err(CodecError::InvalidPostCondition),
    }
}

fn read_asset_info(reader: &mut Reader<'_>) -> Result<AssetInfo, CodecError> {
    Ok(AssetInfo {
        address: read_address(reader)?,
        contract_name: read_name(reader)?,
        asset_name: read_name(reader)?,
    })
}

fn read_fungible_condition(reader: &mut Reader<'_>) -> Result<FungibleCondition, CodecError> {
    match reader.byte()? {
        1 => Ok(FungibleCondition::SentEqual),
        2 => Ok(FungibleCondition::SentGreater),
        3 => Ok(FungibleCondition::SentGreaterEqual),
        4 => Ok(FungibleCondition::SentLess),
        5 => Ok(FungibleCondition::SentLessEqual),
        _ => Err(CodecError::InvalidPostCondition),
    }
}

fn read_principal(reader: &mut Reader<'_>) -> Result<Principal, CodecError> {
    match reader.byte()? {
        5 => Ok(Principal::Standard(read_address(reader)?)),
        6 => Ok(Principal::Contract {
            address: read_address(reader)?,
            contract_name: read_name(reader)?,
        }),
        _ => Err(CodecError::InvalidPrincipal),
    }
}

fn read_address(reader: &mut Reader<'_>) -> Result<StacksAddress, CodecError> {
    let bytes = reader.take(21)?;
    let hash160 = Hash160::from_bytes(bytes[1..].try_into().expect("fixed slice"));
    StacksAddress::new(bytes[0], hash160).map_err(|_| CodecError::InvalidPrincipal)
}

fn read_consensus_hash(reader: &mut Reader<'_>) -> Result<ConsensusHash, CodecError> {
    Ok(ConsensusHash::from_bytes(
        reader.take(20)?.try_into().expect("fixed slice"),
    ))
}

fn read_block_id(reader: &mut Reader<'_>) -> Result<StacksBlockId, CodecError> {
    Ok(StacksBlockId::from_bytes(
        reader.take(32)?.try_into().expect("fixed slice"),
    ))
}

fn read_name(reader: &mut Reader<'_>) -> Result<String, CodecError> {
    let length = usize::from(reader.byte()?);
    let bytes = reader.take(length)?;
    if length == 0 || length > 128 || !bytes.iter().all(u8::is_ascii) {
        return Err(CodecError::InvalidName);
    }
    Ok(String::from_utf8(bytes.to_vec()).expect("ASCII is valid UTF-8"))
}

fn read_stacks_string(reader: &mut Reader<'_>) -> Result<String, CodecError> {
    let length = usize::try_from(reader.u32()?).map_err(|_| CodecError::InvalidLength)?;
    let bytes = reader.take(length)?;
    if !bytes
        .iter()
        .all(|byte| byte.is_ascii_graphic() || matches!(*byte, b' ' | b'\t' | b'\n'))
    {
        return Err(CodecError::InvalidString);
    }
    Ok(String::from_utf8(bytes.to_vec()).expect("ASCII is valid UTF-8"))
}

fn read_clarity_value(reader: &mut Reader<'_>) -> Result<ClarityValue, CodecError> {
    let start = reader.position();
    scan_clarity_value(reader, 0)?;
    Ok(ClarityValue(
        reader.bytes[start..reader.position()].to_vec(),
    ))
}

fn scan_clarity_value(reader: &mut Reader<'_>, depth: u8) -> Result<(), CodecError> {
    if depth > 32 {
        return Err(CodecError::InvalidClarityValue);
    }
    match reader.byte()? {
        0 | 1 => {
            reader.take(16)?;
        }
        2 | 13 | 14 => {
            let length = usize::try_from(reader.u32()?).map_err(|_| CodecError::InvalidLength)?;
            if length > 1024 * 1024 {
                return Err(CodecError::InvalidClarityValue);
            }
            reader.take(length)?;
        }
        3 | 4 | 9 => {}
        5 => {
            read_address(reader)?;
        }
        6 => {
            read_address(reader)?;
            read_name(reader)?;
        }
        7 | 8 | 10 => scan_clarity_value(reader, depth + 1)?,
        11 => {
            let length = reader.u32()?;
            if length > 1024 * 1024 {
                return Err(CodecError::InvalidClarityValue);
            }
            for _ in 0..length {
                scan_clarity_value(reader, depth + 1)?;
            }
        }
        12 => {
            let length = reader.u32()?;
            if length > 1024 * 1024 {
                return Err(CodecError::InvalidClarityValue);
            }
            for _ in 0..length {
                read_name(reader)?;
                scan_clarity_value(reader, depth + 1)?;
            }
        }
        _ => return Err(CodecError::InvalidClarityValue),
    }
    Ok(())
}

fn encode_post_condition(writer: &mut Writer, post_condition: &PostConditionData) {
    match post_condition {
        PostConditionData::Stx {
            principal,
            condition,
            amount,
        } => {
            writer.byte(0);
            encode_post_condition_principal(writer, principal);
            encode_fungible_condition(writer, *condition);
            writer.u64(*amount);
        }
        PostConditionData::Fungible {
            principal,
            asset,
            condition,
            amount,
        } => {
            writer.byte(1);
            encode_post_condition_principal(writer, principal);
            encode_asset_info(writer, asset);
            encode_fungible_condition(writer, *condition);
            writer.u64(*amount);
        }
        PostConditionData::NonFungible {
            principal,
            asset,
            asset_value,
            condition,
        } => {
            writer.byte(2);
            encode_post_condition_principal(writer, principal);
            encode_asset_info(writer, asset);
            writer.raw(asset_value.as_bytes());
            writer.byte(match condition {
                NonFungibleCondition::DoesSend => 0x10,
                NonFungibleCondition::DoesNotSend => 0x11,
            });
        }
        PostConditionData::Staking {
            principal,
            condition,
            amount,
        } => {
            writer.byte(3);
            encode_post_condition_principal(writer, principal);
            encode_fungible_condition(writer, *condition);
            writer.u64(*amount);
        }
        PostConditionData::Pox {
            principal,
            condition,
        } => {
            writer.byte(4);
            encode_post_condition_principal(writer, principal);
            writer.byte(match condition {
                PoxCondition::NotPerformed => 0x30,
                PoxCondition::MaybePerformed => 0x31,
                PoxCondition::Performed => 0x32,
            });
        }
    }
}

fn encode_post_condition_principal(writer: &mut Writer, principal: &PostConditionPrincipal) {
    match principal {
        PostConditionPrincipal::Origin => writer.byte(1),
        PostConditionPrincipal::Standard(address) => {
            writer.byte(2);
            writer.address(*address);
        }
        PostConditionPrincipal::Contract {
            address,
            contract_name,
        } => {
            writer.byte(3);
            writer.address(*address);
            writer.name(contract_name);
        }
    }
}

fn encode_asset_info(writer: &mut Writer, asset: &AssetInfo) {
    writer.address(asset.address);
    writer.name(&asset.contract_name);
    writer.name(&asset.asset_name);
}

fn encode_fungible_condition(writer: &mut Writer, condition: FungibleCondition) {
    writer.byte(match condition {
        FungibleCondition::SentEqual => 1,
        FungibleCondition::SentGreater => 2,
        FungibleCondition::SentGreaterEqual => 3,
        FungibleCondition::SentLess => 4,
        FungibleCondition::SentLessEqual => 5,
    });
}

fn encode_principal(writer: &mut Writer, principal: &Principal) {
    match principal {
        Principal::Standard(address) => {
            writer.byte(5);
            writer.address(*address);
        }
        Principal::Contract {
            address,
            contract_name,
        } => {
            writer.byte(6);
            writer.address(*address);
            writer.name(contract_name);
        }
    }
}

fn encode_payload(writer: &mut Writer, payload: &TransactionPayloadData) {
    match payload {
        TransactionPayloadData::TokenTransfer {
            recipient,
            amount,
            memo,
        } => {
            writer.byte(0);
            encode_principal(writer, recipient);
            writer.u64(*amount);
            writer.raw(memo);
        }
        TransactionPayloadData::SmartContract {
            contract_name,
            source,
        } => {
            writer.byte(1);
            writer.name(contract_name);
            writer.stacks_string(source);
        }
        TransactionPayloadData::ContractCall {
            address,
            contract_name,
            function_name,
            arguments,
        } => {
            writer.byte(2);
            writer.address(*address);
            writer.name(contract_name);
            writer.name(function_name);
            writer.u32(u32::try_from(arguments.len()).expect("argument count fits u32"));
            for argument in arguments {
                writer.raw(argument.as_bytes());
            }
        }
        TransactionPayloadData::PoisonMicroblock { first, second } => {
            writer.byte(3);
            writer.raw(first.as_bytes());
            writer.raw(second.as_bytes());
        }
        TransactionPayloadData::Coinbase { payload } => {
            writer.byte(4);
            writer.raw(payload);
        }
        TransactionPayloadData::CoinbaseToAltRecipient { payload, recipient } => {
            writer.byte(5);
            writer.raw(payload);
            encode_principal(writer, recipient);
        }
        TransactionPayloadData::VersionedSmartContract {
            clarity_version,
            contract_name,
            source,
        } => {
            writer.byte(6);
            writer.byte(clarity_version.byte());
            writer.name(contract_name);
            writer.stacks_string(source);
        }
        TransactionPayloadData::TenureChange(payload) => {
            writer.byte(7);
            writer.raw(payload.tenure_consensus_hash.as_bytes());
            writer.raw(payload.previous_tenure_consensus_hash.as_bytes());
            writer.raw(payload.bitcoin_view_consensus_hash.as_bytes());
            writer.raw(payload.previous_tenure_end.as_bytes());
            writer.u32(payload.previous_tenure_blocks);
            writer.byte(payload.cause.byte());
            writer.hash160(payload.public_key_hash);
        }
        TransactionPayloadData::NakamotoCoinbase {
            payload,
            recipient,
            vrf_proof,
        } => {
            writer.byte(8);
            writer.raw(payload);
            match recipient {
                Some(principal) => {
                    writer.byte(10);
                    encode_principal(writer, principal);
                }
                None => writer.byte(9),
            }
            writer.raw(vrf_proof);
        }
    }
}

impl SpendingCondition {
    #[must_use]
    pub const fn nonce(&self) -> u64 {
        match self {
            Self::Singlesig(condition) => condition.nonce,
            Self::Multisig(condition) => condition.nonce,
            Self::OrderIndependentMultisig(condition) => condition.nonce,
        }
    }

    #[must_use]
    pub const fn fee(&self) -> u64 {
        match self {
            Self::Singlesig(condition) => condition.fee,
            Self::Multisig(condition) => condition.fee,
            Self::OrderIndependentMultisig(condition) => condition.fee,
        }
    }

    #[must_use]
    pub fn account_address(&self, mainnet: bool) -> StacksAddress {
        let (signer, singlesig) = match self {
            Self::Singlesig(condition) => (condition.signer, Some(condition.hash_mode)),
            Self::Multisig(condition) => (condition.signer, None),
            Self::OrderIndependentMultisig(condition) => (condition.signer, None),
        };
        let version = match (mainnet, singlesig) {
            (true, Some(SinglesigHashMode::P2pkh)) => 22,
            (false, Some(SinglesigHashMode::P2pkh)) => 26,
            (true, _) => 20,
            (false, _) => 21,
        };

        StacksAddress::new(version, signer).expect("protocol address versions are valid")
    }

    const fn cleared(&self) -> Self {
        match self {
            Self::Singlesig(condition) => Self::Singlesig(SinglesigCondition {
                hash_mode: condition.hash_mode,
                signer: condition.signer,
                nonce: 0,
                fee: 0,
                key_encoding: condition.key_encoding,
                signature: MessageSignature::from_bytes([0; 65]),
            }),
            Self::Multisig(condition) => Self::Multisig(MultisigCondition {
                hash_mode: condition.hash_mode,
                signer: condition.signer,
                nonce: 0,
                fee: 0,
                fields: Vec::new(),
                signatures_required: condition.signatures_required,
            }),
            Self::OrderIndependentMultisig(condition) => {
                Self::OrderIndependentMultisig(OrderIndependentMultisigCondition {
                    hash_mode: condition.hash_mode,
                    signer: condition.signer,
                    nonce: 0,
                    fee: 0,
                    fields: Vec::new(),
                    signatures_required: condition.signatures_required,
                })
            }
        }
    }

    fn verify(&self, initial_sighash: Sha256Sum, auth_flag: u8) -> Result<Sha256Sum, CodecError> {
        match self {
            Self::Singlesig(condition) => {
                let (key, next_sighash) = recover_key(
                    initial_sighash,
                    auth_flag,
                    condition.fee,
                    condition.nonce,
                    condition.key_encoding,
                    &condition.signature,
                )?;
                let mode = match condition.hash_mode {
                    SinglesigHashMode::P2pkh => AddressHashMode::P2pkh,
                    SinglesigHashMode::P2wpkh => AddressHashMode::P2wpkh,
                };
                if address_hash(mode, 1, &[key])? != condition.signer {
                    return Err(CodecError::InvalidAuth);
                }
                Ok(next_sighash)
            }
            Self::Multisig(condition) => verify_multisig(
                initial_sighash,
                auth_flag,
                MultisigVerification {
                    signer: condition.signer,
                    fee: condition.fee,
                    nonce: condition.nonce,
                    fields: &condition.fields,
                    signatures_required: condition.signatures_required,
                    mode: match condition.hash_mode {
                        MultisigHashMode::P2sh => AddressHashMode::P2sh,
                        MultisigHashMode::P2wsh => AddressHashMode::P2wsh,
                    },
                    order_independent: false,
                },
            ),
            Self::OrderIndependentMultisig(condition) => verify_multisig(
                initial_sighash,
                auth_flag,
                MultisigVerification {
                    signer: condition.signer,
                    fee: condition.fee,
                    nonce: condition.nonce,
                    fields: &condition.fields,
                    signatures_required: condition.signatures_required,
                    mode: match condition.hash_mode {
                        OrderIndependentMultisigHashMode::P2sh => AddressHashMode::P2sh,
                        OrderIndependentMultisigHashMode::P2wsh => AddressHashMode::P2wsh,
                    },
                    order_independent: true,
                },
            ),
        }
    }

    fn decode(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        let mode = reader.byte()?;
        match mode {
            0 | 2 => {
                let hash_mode = SinglesigHashMode::parse(mode)?;
                let signer = reader.hash160()?;
                let nonce = reader.u64()?;
                let fee = reader.u64()?;
                let key_encoding = KeyEncoding::parse(reader.byte()?)?;
                if hash_mode == SinglesigHashMode::P2wpkh && key_encoding != KeyEncoding::Compressed
                {
                    return Err(CodecError::InvalidCondition);
                }
                Ok(Self::Singlesig(SinglesigCondition {
                    hash_mode,
                    signer,
                    nonce,
                    fee,
                    key_encoding,
                    signature: reader.signature()?,
                }))
            }
            1 | 3 => Ok(Self::Multisig(decode_multisig(
                reader,
                MultisigHashMode::parse(mode)?,
            )?)),
            5 | 7 => Ok(Self::OrderIndependentMultisig(decode_order_independent(
                reader,
                OrderIndependentMultisigHashMode::parse(mode)?,
            )?)),
            _ => Err(CodecError::InvalidCondition),
        }
    }
    fn encode(&self, writer: &mut Writer) {
        match self {
            Self::Singlesig(condition) => {
                writer.byte(condition.hash_mode.byte());
                writer.hash160(condition.signer);
                writer.u64(condition.nonce);
                writer.u64(condition.fee);
                writer.byte(condition.key_encoding.byte());
                writer.signature(condition.signature);
            }
            Self::Multisig(condition) => encode_fields(
                writer,
                condition.hash_mode.byte(),
                condition.signer,
                condition.nonce,
                condition.fee,
                &condition.fields,
                condition.signatures_required,
            ),
            Self::OrderIndependentMultisig(condition) => encode_fields(
                writer,
                condition.hash_mode.byte(),
                condition.signer,
                condition.nonce,
                condition.fee,
                &condition.fields,
                condition.signatures_required,
            ),
        }
    }
}

#[derive(Clone, Copy)]
enum AddressHashMode {
    P2pkh,
    P2sh,
    P2wpkh,
    P2wsh,
}

#[derive(Clone, Copy)]
struct MultisigVerification<'a> {
    signer: Hash160,
    fee: u64,
    nonce: u64,
    fields: &'a [AuthField],
    signatures_required: u16,
    mode: AddressHashMode,
    order_independent: bool,
}

fn recover_key(
    current: Sha256Sum,
    auth_flag: u8,
    fee: u64,
    nonce: u64,
    encoding: KeyEncoding,
    signature: &MessageSignature,
) -> Result<(Vec<u8>, Sha256Sum), CodecError> {
    if !signature.is_low_s().map_err(|_| CodecError::InvalidAuth)? {
        return Err(CodecError::InvalidAuth);
    }
    let presign = presign_sighash(current, auth_flag, fee, nonce);
    let key = signature
        .recover(presign.as_bytes())
        .map_err(|_| CodecError::InvalidAuth)?;
    let key = encoded_key(&key, encoding);
    let next = postsign_sighash(presign, encoding, signature);
    Ok((key, next))
}

fn verify_multisig(
    initial_sighash: Sha256Sum,
    auth_flag: u8,
    verification: MultisigVerification<'_>,
) -> Result<Sha256Sum, CodecError> {
    let mut keys = Vec::with_capacity(verification.fields.len());
    let mut current = initial_sighash;
    let mut signatures = 0_u16;
    for field in verification.fields {
        let key = match field {
            AuthField::PublicKey { encoding, bytes } => {
                let key =
                    StacksPublicKey::from_bytes(bytes).map_err(|_| CodecError::InvalidAuth)?;
                if encoded_key(&key, *encoding) != *bytes {
                    return Err(CodecError::InvalidAuth);
                }
                bytes.clone()
            }
            AuthField::Signature {
                encoding,
                signature,
            } => {
                let (key, next) = recover_key(
                    current,
                    auth_flag,
                    verification.fee,
                    verification.nonce,
                    *encoding,
                    signature,
                )?;
                if !verification.order_independent {
                    current = next;
                }
                signatures = signatures.checked_add(1).ok_or(CodecError::InvalidAuth)?;
                key
            }
        };
        keys.push(key);
    }
    let signatures_match = if verification.order_independent {
        signatures >= verification.signatures_required
    } else {
        signatures == verification.signatures_required
    };
    if !signatures_match
        || address_hash(verification.mode, verification.signatures_required, &keys)?
            != verification.signer
    {
        return Err(CodecError::InvalidAuth);
    }
    Ok(if verification.order_independent {
        initial_sighash
    } else {
        current
    })
}

fn encoded_key(key: &StacksPublicKey, encoding: KeyEncoding) -> Vec<u8> {
    match encoding {
        KeyEncoding::Compressed => key.to_bytes_compressed().to_vec(),
        KeyEncoding::Uncompressed => key.to_bytes_uncompressed().to_vec(),
    }
}

/// Authorization flags separating the origin and sponsor signature chains.
const ORIGIN_AUTH_FLAG: u8 = 0x04;
const SPONSOR_AUTH_FLAG: u8 = 0x05;

fn presign_sighash(current: Sha256Sum, auth_flag: u8, fee: u64, nonce: u64) -> Sha256Sum {
    let mut bytes = Vec::with_capacity(49);
    bytes.extend_from_slice(current.as_bytes());
    bytes.push(auth_flag);
    bytes.extend_from_slice(&fee.to_be_bytes());
    bytes.extend_from_slice(&nonce.to_be_bytes());
    sha512_256(&bytes)
}

fn postsign_sighash(
    current: Sha256Sum,
    encoding: KeyEncoding,
    signature: &MessageSignature,
) -> Sha256Sum {
    let mut bytes = Vec::with_capacity(98);
    bytes.extend_from_slice(current.as_bytes());
    bytes.push(encoding.byte());
    bytes.extend_from_slice(signature.as_bytes());
    sha512_256(&bytes)
}

fn address_hash(
    mode: AddressHashMode,
    signatures_required: u16,
    keys: &[Vec<u8>],
) -> Result<Hash160, CodecError> {
    let signatures_required = usize::from(signatures_required);
    if signatures_required == 0 || keys.len() < signatures_required || keys.len() > 16 {
        return Err(CodecError::InvalidAuth);
    }
    match mode {
        AddressHashMode::P2pkh => {
            if signatures_required != 1 || keys.len() != 1 {
                return Err(CodecError::InvalidAuth);
            }
            Ok(hash160(&keys[0]))
        }
        AddressHashMode::P2wpkh => {
            if signatures_required != 1 || keys.len() != 1 || keys[0].len() != 33 {
                return Err(CodecError::InvalidAuth);
            }
            let key_hash = hash160(&keys[0]);
            let mut script = vec![0, 20];
            script.extend_from_slice(key_hash.as_bytes());
            Ok(hash160(&script))
        }
        AddressHashMode::P2sh => Ok(hash160(&multisig_script(signatures_required, keys)?)),
        AddressHashMode::P2wsh => {
            if keys.iter().any(|key| key.len() != 33) {
                return Err(CodecError::InvalidAuth);
            }
            let script_hash = sha256(&multisig_script(signatures_required, keys)?);
            let mut script = vec![0, 32];
            script.extend_from_slice(script_hash.as_bytes());
            Ok(hash160(&script))
        }
    }
}

fn multisig_script(signatures_required: usize, keys: &[Vec<u8>]) -> Result<Vec<u8>, CodecError> {
    let required = script_small_int(signatures_required)?;
    let key_count = script_small_int(keys.len())?;
    let mut script = Vec::with_capacity(3 + keys.iter().map(Vec::len).sum::<usize>() + keys.len());
    script.push(required);
    for key in keys {
        let length = u8::try_from(key.len()).map_err(|_| CodecError::InvalidAuth)?;
        script.push(length);
        script.extend_from_slice(key);
    }
    script.extend_from_slice(&[key_count, 0xae]);
    Ok(script)
}

fn script_small_int(value: usize) -> Result<u8, CodecError> {
    u8::try_from(value)
        .ok()
        .filter(|value| (1..=16).contains(value))
        .map(|value| 0x50 + value)
        .ok_or(CodecError::InvalidAuth)
}

fn decode_multisig(
    reader: &mut Reader<'_>,
    hash_mode: MultisigHashMode,
) -> Result<MultisigCondition, CodecError> {
    let (signer, nonce, fee, fields, signatures_required) = decode_fields(reader)?;
    let signatures = fields
        .iter()
        .filter(|field| matches!(field, AuthField::Signature { .. }))
        .count();
    if signatures != usize::from(signatures_required) {
        return Err(CodecError::InvalidCondition);
    }
    if hash_mode == MultisigHashMode::P2wsh && has_uncompressed(&fields) {
        return Err(CodecError::InvalidCondition);
    }
    Ok(MultisigCondition {
        hash_mode,
        signer,
        nonce,
        fee,
        fields,
        signatures_required,
    })
}
fn decode_order_independent(
    reader: &mut Reader<'_>,
    hash_mode: OrderIndependentMultisigHashMode,
) -> Result<OrderIndependentMultisigCondition, CodecError> {
    let (signer, nonce, fee, fields, signatures_required) = decode_fields(reader)?;
    let signatures = fields
        .iter()
        .filter(|field| matches!(field, AuthField::Signature { .. }))
        .count();
    if signatures < usize::from(signatures_required)
        || (hash_mode == OrderIndependentMultisigHashMode::P2wsh && has_uncompressed(&fields))
    {
        return Err(CodecError::InvalidCondition);
    }
    Ok(OrderIndependentMultisigCondition {
        hash_mode,
        signer,
        nonce,
        fee,
        fields,
        signatures_required,
    })
}
fn decode_fields(
    reader: &mut Reader<'_>,
) -> Result<(Hash160, u64, u64, Vec<AuthField>, u16), CodecError> {
    let signer = reader.hash160()?;
    let nonce = reader.u64()?;
    let fee = reader.u64()?;
    let length = usize::try_from(reader.u32()?).map_err(|_| CodecError::InvalidLength)?;
    if length > reader.remaining() / 34 {
        return Err(CodecError::InvalidLength);
    }
    let mut fields = Vec::with_capacity(length);
    for _ in 0..length {
        fields.push(reader.field()?);
    }
    Ok((signer, nonce, fee, fields, reader.u16()?))
}
fn encode_fields(
    writer: &mut Writer,
    hash_mode: u8,
    signer: Hash160,
    nonce: u64,
    fee: u64,
    fields: &[AuthField],
    signatures_required: u16,
) {
    writer.byte(hash_mode);
    writer.hash160(signer);
    writer.u64(nonce);
    writer.u64(fee);
    writer.u32(u32::try_from(fields.len()).expect("field count fits u32"));
    for field in fields {
        writer.field(field);
    }
    writer.u16(signatures_required);
}
fn has_uncompressed(fields: &[AuthField]) -> bool {
    fields.iter().any(|field| {
        matches!(
            field,
            AuthField::PublicKey {
                encoding: KeyEncoding::Uncompressed,
                ..
            } | AuthField::Signature {
                encoding: KeyEncoding::Uncompressed,
                ..
            }
        )
    })
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }
    const fn position(&self) -> usize {
        self.offset
    }
    const fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }
    fn take(&mut self, length: usize) -> Result<&'a [u8], CodecError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(CodecError::InvalidLength)?;
        let result = self
            .bytes
            .get(self.offset..end)
            .ok_or(CodecError::EndOfInput)?;
        self.offset = end;
        Ok(result)
    }
    fn byte(&mut self) -> Result<u8, CodecError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, CodecError> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().expect("fixed slice"),
        ))
    }
    fn u32(&mut self) -> Result<u32, CodecError> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("fixed slice"),
        ))
    }
    fn u64(&mut self) -> Result<u64, CodecError> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("fixed slice"),
        ))
    }
    fn hash160(&mut self) -> Result<Hash160, CodecError> {
        Ok(Hash160::from_bytes(
            self.take(20)?.try_into().expect("fixed slice"),
        ))
    }
    fn signature(&mut self) -> Result<MessageSignature, CodecError> {
        Ok(MessageSignature::from_bytes(
            self.take(65)?.try_into().expect("fixed slice"),
        ))
    }
    fn field(&mut self) -> Result<AuthField, CodecError> {
        match self.byte()? {
            0 => Ok(AuthField::PublicKey {
                encoding: KeyEncoding::Compressed,
                bytes: self.take(33)?.to_vec(),
            }),
            1 => Ok(AuthField::PublicKey {
                encoding: KeyEncoding::Uncompressed,
                bytes: self.take(65)?.to_vec(),
            }),
            2 => Ok(AuthField::Signature {
                encoding: KeyEncoding::Compressed,
                signature: self.signature()?,
            }),
            3 => Ok(AuthField::Signature {
                encoding: KeyEncoding::Uncompressed,
                signature: self.signature()?,
            }),
            _ => Err(CodecError::InvalidField),
        }
    }
}

#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
}
impl Writer {
    fn finish(self) -> Vec<u8> {
        self.bytes
    }
    fn byte(&mut self, value: u8) {
        self.bytes.push(value);
    }
    fn raw(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }
    fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }
    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }
    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }
    fn hash160(&mut self, value: Hash160) {
        self.bytes.extend_from_slice(value.as_bytes());
    }
    fn address(&mut self, value: StacksAddress) {
        self.byte(value.version());
        self.hash160(value.hash160());
    }
    fn name(&mut self, value: &str) {
        self.byte(u8::try_from(value.len()).expect("name length fits u8"));
        self.raw(value.as_bytes());
    }
    fn stacks_string(&mut self, value: &str) {
        self.u32(u32::try_from(value.len()).expect("string length fits u32"));
        self.raw(value.as_bytes());
    }
    fn signature(&mut self, value: MessageSignature) {
        self.bytes.extend_from_slice(value.as_bytes());
    }
    fn field(&mut self, field: &AuthField) {
        match field {
            AuthField::PublicKey { encoding, bytes } => {
                self.byte(match encoding {
                    KeyEncoding::Compressed => 0,
                    KeyEncoding::Uncompressed => 1,
                });
                self.bytes.extend_from_slice(bytes);
            }
            AuthField::Signature {
                encoding,
                signature,
            } => {
                self.byte(match encoding {
                    KeyEncoding::Compressed => 2,
                    KeyEncoding::Uncompressed => 3,
                });
                self.signature(*signature);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CodecError, NonFungibleCondition, PostConditionData, PostConditionMode, Reader,
        Transaction, read_stacks_string,
    };

    #[test]
    fn stacks_strings_allow_tabs_and_newlines() {
        let mut reader = Reader::new(b"\0\0\0\x0aline\n\ttext");
        assert_eq!(
            read_stacks_string(&mut reader).expect("valid source string"),
            "line\n\ttext"
        );

        let mut reader = Reader::new(b"\0\0\0\x0aline\r\ttext");
        assert_eq!(
            read_stacks_string(&mut reader),
            Err(CodecError::InvalidString)
        );
    }

    #[test]
    fn sip040_post_condition_tags_match_the_protocol() {
        for (mode, tag, expected) in [
            (1, 0x10, NonFungibleCondition::DoesSend),
            (2, 0x11, NonFungibleCondition::DoesNotSend),
            (3, 0x10, NonFungibleCondition::DoesSend),
        ] {
            let bytes = encoded_post_condition_transaction(mode, tag);
            let (transaction, consumed) = Transaction::decode(&bytes).expect("decode transaction");
            assert_eq!(consumed, bytes.len());
            assert_eq!(
                transaction.post_condition_mode(),
                match mode {
                    1 => PostConditionMode::Allow,
                    2 => PostConditionMode::Deny,
                    3 => PostConditionMode::Originator,
                    _ => unreachable!(),
                }
            );
            assert!(matches!(
                transaction.post_conditions()[0].data(),
                PostConditionData::NonFungible { condition, .. } if *condition == expected
            ));
            assert_eq!(transaction.encode(), bytes);
        }
    }

    fn encoded_post_condition_transaction(mode: u8, nft_tag: u8) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(0x80);
        bytes.extend_from_slice(&0x8000_0000_u32.to_be_bytes());
        bytes.push(4);
        bytes.push(0);
        bytes.extend_from_slice(&[0; 20]);
        bytes.extend_from_slice(&[0; 8]);
        bytes.extend_from_slice(&[0; 8]);
        bytes.push(0);
        bytes.extend_from_slice(&[0; 65]);
        bytes.push(3);
        bytes.push(mode);
        bytes.extend_from_slice(&1_u32.to_be_bytes());
        bytes.push(2);
        bytes.push(1);
        bytes.push(26);
        bytes.extend_from_slice(&[0; 20]);
        bytes.extend_from_slice(&[1, b'a', 1, b'b']);
        bytes.push(0);
        bytes.extend_from_slice(&[0; 16]);
        bytes.push(nft_tag);
        bytes.push(0);
        bytes.push(5);
        bytes.push(26);
        bytes.extend_from_slice(&[0; 20]);
        bytes.extend_from_slice(&[0; 8]);
        bytes.extend_from_slice(&[0; 34]);
        bytes
    }
}
