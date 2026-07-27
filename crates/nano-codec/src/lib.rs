#![forbid(unsafe_code)]

use std::fmt;

use nano_crypto::MessageSignature;
use nano_primitives::{Hash160, Sha256Sum, sha512_256};

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
}

impl TransactionVersion {
    const fn parse(value: u8) -> Result<Self, CodecError> {
        match value {
            0x00 => Ok(Self::Mainnet),
            0x80 => Ok(Self::Testnet),
            _ => Err(CodecError::InvalidTransaction),
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PostConditionMode {
    Deny,
    Allow,
}

impl PostConditionMode {
    const fn parse(value: u8) -> Result<Self, CodecError> {
        match value {
            1 => Ok(Self::Deny),
            2 => Ok(Self::Allow),
            _ => Err(CodecError::InvalidTransaction),
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PostConditionType {
    Stx,
    Fungible,
    NonFungible,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostCondition {
    kind: PostConditionType,
    bytes: Vec<u8>,
}

impl PostCondition {
    #[must_use]
    pub const fn kind(&self) -> PostConditionType {
        self.kind
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionPayload {
    kind: TransactionPayloadType,
    bytes: Vec<u8>,
}

impl TransactionPayload {
    #[must_use]
    pub const fn kind(&self) -> TransactionPayloadType {
        self.kind
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Transaction {
    pub fn decode(bytes: &[u8]) -> Result<(Self, usize), CodecError> {
        let mut reader = Reader::new(bytes);
        let version = TransactionVersion::parse(reader.byte()?)?;
        let chain_id = reader.u32()?;

        let auth_start = reader.position();
        let (auth, auth_length) = TransactionAuth::decode(&bytes[auth_start..])?;
        reader.take(auth_length)?;

        let anchor_mode = AnchorMode::parse(reader.byte()?)?;
        let post_condition_mode = PostConditionMode::parse(reader.byte()?)?;

        let post_condition_count = reader.u32()?;
        let mut post_conditions = Vec::with_capacity(
            usize::try_from(post_condition_count).map_err(|_| CodecError::InvalidLength)?,
        );
        for _ in 0..post_condition_count {
            let start = reader.position();
            let kind = scan_post_condition(&mut reader)?;
            post_conditions.push(PostCondition {
                kind,
                bytes: reader.bytes[start..reader.position()].to_vec(),
            });
        }
        let payload_start = reader.position();
        let payload_type = scan_payload(&mut reader)?;
        let payload = TransactionPayload {
            kind: payload_type,
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
        self.bytes.clone()
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
        sha512_256(&self.bytes)
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

fn scan_payload(reader: &mut Reader<'_>) -> Result<TransactionPayloadType, CodecError> {
    let payload_type = TransactionPayloadType::parse(reader.byte()?)?;
    match payload_type {
        TransactionPayloadType::TokenTransfer => {
            scan_principal(reader)?;
            reader.u64()?;
            reader.take(34)?;
        }
        TransactionPayloadType::SmartContract => {
            scan_name(reader)?;
            scan_stacks_string(reader)?;
        }
        TransactionPayloadType::ContractCall => {
            reader.take(21)?;
            scan_name(reader)?;
            scan_name(reader)?;
            let arguments = reader.u32()?;
            for _ in 0..arguments {
                scan_clarity_value(reader, 0)?;
            }
        }
        TransactionPayloadType::PoisonMicroblock => {
            let first = scan_microblock_header(reader)?;
            let second = scan_microblock_header(reader)?;
            if first == second
                || (first.sequence != second.sequence && first.previous != second.previous)
            {
                return Err(CodecError::InvalidPayload);
            }
        }
        TransactionPayloadType::Coinbase => {
            reader.take(32)?;
        }
        TransactionPayloadType::CoinbaseToAltRecipient => {
            reader.take(32)?;
            let value_start = reader.position();
            scan_clarity_value(reader, 0)?;
            if reader.bytes[value_start] != 5 && reader.bytes[value_start] != 6 {
                return Err(CodecError::InvalidPayload);
            }
        }
        TransactionPayloadType::VersionedSmartContract => {
            match reader.byte()? {
                1..=6 => {}
                _ => return Err(CodecError::InvalidPayload),
            }
            scan_name(reader)?;
            scan_stacks_string(reader)?;
        }
        TransactionPayloadType::TenureChange => {
            reader.take(20 * 3 + 32)?;
            reader.u32()?;
            if reader.byte()? > 6 {
                return Err(CodecError::InvalidPayload);
            }
            reader.take(20)?;
        }
        TransactionPayloadType::NakamotoCoinbase => {
            reader.take(32)?;
            let value_start = reader.position();
            scan_clarity_value(reader, 0)?;
            let principal = &reader.bytes[value_start..reader.position()];
            if !matches!(principal, [9] | [10, 5 | 6, ..]) {
                return Err(CodecError::InvalidPayload);
            }
            reader.take(80)?;
        }
    }
    Ok(payload_type)
}

#[derive(Eq, PartialEq)]
struct MicroblockIdentity {
    sequence: u16,
    previous: [u8; 32],
    bytes: [u8; 132],
}

fn scan_microblock_header(reader: &mut Reader<'_>) -> Result<MicroblockIdentity, CodecError> {
    let bytes: [u8; 132] = reader.take(132)?.try_into().expect("fixed slice");
    Ok(MicroblockIdentity {
        sequence: u16::from_be_bytes(bytes[1..3].try_into().expect("fixed slice")),
        previous: bytes[3..35].try_into().expect("fixed slice"),
        bytes,
    })
}

fn scan_post_condition(reader: &mut Reader<'_>) -> Result<PostConditionType, CodecError> {
    let kind = match reader.byte()? {
        0 => {
            scan_post_condition_principal(reader)?;
            scan_fungible_condition(reader)?;
            reader.u64()?;
            PostConditionType::Stx
        }
        1 => {
            scan_post_condition_principal(reader)?;
            scan_asset_info(reader)?;
            scan_fungible_condition(reader)?;
            reader.u64()?;
            PostConditionType::Fungible
        }
        2 => {
            scan_post_condition_principal(reader)?;
            scan_asset_info(reader)?;
            scan_clarity_value(reader, 0)?;
            match reader.byte()? {
                0x10 | 0x11 => {}
                _ => return Err(CodecError::InvalidPostCondition),
            }
            PostConditionType::NonFungible
        }
        _ => return Err(CodecError::InvalidPostCondition),
    };
    Ok(kind)
}

fn scan_post_condition_principal(reader: &mut Reader<'_>) -> Result<(), CodecError> {
    match reader.byte()? {
        1 => Ok(()),
        2 => reader.take(21).map(|_| ()),
        3 => {
            reader.take(21)?;
            scan_name(reader)
        }
        _ => Err(CodecError::InvalidPostCondition),
    }
}

fn scan_asset_info(reader: &mut Reader<'_>) -> Result<(), CodecError> {
    reader.take(21)?;
    scan_name(reader)?;
    scan_name(reader)
}

fn scan_fungible_condition(reader: &mut Reader<'_>) -> Result<(), CodecError> {
    match reader.byte()? {
        1..=5 => Ok(()),
        _ => Err(CodecError::InvalidPostCondition),
    }
}

fn scan_principal(reader: &mut Reader<'_>) -> Result<(), CodecError> {
    match reader.byte()? {
        5 => reader.take(21).map(|_| ()),
        6 => {
            reader.take(21)?;
            scan_name(reader)
        }
        _ => Err(CodecError::InvalidPrincipal),
    }
}

fn scan_name(reader: &mut Reader<'_>) -> Result<(), CodecError> {
    let length = usize::from(reader.byte()?);
    if length == 0 || length > 128 || !reader.take(length)?.iter().all(u8::is_ascii) {
        return Err(CodecError::InvalidName);
    }
    Ok(())
}

fn scan_stacks_string(reader: &mut Reader<'_>) -> Result<(), CodecError> {
    let length = usize::try_from(reader.u32()?).map_err(|_| CodecError::InvalidLength)?;
    if !reader
        .take(length)?
        .iter()
        .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
    {
        return Err(CodecError::InvalidString);
    }
    Ok(())
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
            reader.take(21)?;
        }
        6 => {
            reader.take(21)?;
            scan_name(reader)?;
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
                scan_name(reader)?;
                scan_clarity_value(reader, depth + 1)?;
            }
        }
        _ => return Err(CodecError::InvalidClarityValue),
    }
    Ok(())
}

impl SpendingCondition {
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
