#![forbid(unsafe_code)]

use std::fmt;

use nano_crypto::MessageSignature;
use nano_primitives::Hash160;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodecError {
    EndOfInput,
    InvalidAuth,
    InvalidCondition,
    InvalidField,
    InvalidKey,
    InvalidLength,
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
