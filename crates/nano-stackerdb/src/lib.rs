#![forbid(unsafe_code)]

use std::fmt;

use nano_address::StacksAddress;
use nano_crypto::{CryptoError, MessageSignature, StacksPrivateKey};
use nano_primitives::{Hash160, Sha256Sum, hash160, sha512_256};

/// Maximum wire payload for a signer `StackerDB` chunk.
pub const MAX_CHUNK_SIZE: usize = 2 * 1024 * 1024;

/// Metadata authenticated by a `StackerDB` slot writer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlotMetadata {
    pub slot_id: u32,
    pub slot_version: u32,
    pub data_hash: Sha256Sum,
    pub signature: MessageSignature,
}

/// A signed `StackerDB` chunk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Chunk {
    pub slot_id: u32,
    pub slot_version: u32,
    pub signature: MessageSignature,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StackerDbError {
    ChunkTooLarge,
    Truncated,
    TrailingBytes,
    InvalidSignature,
    Crypto(CryptoError),
}

impl fmt::Display for StackerDbError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ChunkTooLarge => "StackerDB chunk exceeds the protocol limit",
            Self::Truncated => "truncated StackerDB chunk",
            Self::TrailingBytes => "StackerDB chunk has trailing bytes",
            Self::InvalidSignature => "StackerDB chunk signature is invalid",
            Self::Crypto(error) => {
                return write!(formatter, "StackerDB cryptographic error: {error}");
            }
        })
    }
}

impl std::error::Error for StackerDbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Crypto(error) => Some(error),
            Self::ChunkTooLarge
            | Self::Truncated
            | Self::TrailingBytes
            | Self::InvalidSignature => None,
        }
    }
}

impl From<CryptoError> for StackerDbError {
    fn from(error: CryptoError) -> Self {
        Self::Crypto(error)
    }
}

impl SlotMetadata {
    #[must_use]
    pub const fn unsigned(slot_id: u32, slot_version: u32, data_hash: Sha256Sum) -> Self {
        Self {
            slot_id,
            slot_version,
            data_hash,
            signature: MessageSignature::from_bytes([0; 65]),
        }
    }

    #[must_use]
    pub fn digest(&self) -> Sha256Sum {
        let mut bytes = Vec::with_capacity(40);
        bytes.extend_from_slice(&self.slot_id.to_be_bytes());
        bytes.extend_from_slice(&self.slot_version.to_be_bytes());
        bytes.extend_from_slice(self.data_hash.as_bytes());
        sha512_256(&bytes)
    }

    pub fn sign(&mut self, key: &StacksPrivateKey) {
        self.signature = key.sign(self.digest().as_bytes());
    }

    pub fn verify(&self, writer: Hash160) -> Result<bool, StackerDbError> {
        let public_key = self.signature.recover(self.digest().as_bytes())?;
        Ok(hash160(&public_key.to_bytes_compressed()) == writer)
    }
}

impl Chunk {
    #[must_use]
    pub const fn new(slot_id: u32, slot_version: u32, data: Vec<u8>) -> Self {
        Self {
            slot_id,
            slot_version,
            signature: MessageSignature::from_bytes([0; 65]),
            data,
        }
    }

    #[must_use]
    pub fn metadata(&self) -> SlotMetadata {
        SlotMetadata {
            slot_id: self.slot_id,
            slot_version: self.slot_version,
            data_hash: sha512_256(&self.data),
            signature: self.signature,
        }
    }

    pub fn sign(&mut self, key: &StacksPrivateKey) -> Result<(), StackerDbError> {
        if self.data.len() > MAX_CHUNK_SIZE {
            return Err(StackerDbError::ChunkTooLarge);
        }
        let mut metadata = self.metadata();
        metadata.sign(key);
        self.signature = metadata.signature;
        Ok(())
    }

    pub fn verify(&self, writer: Hash160) -> Result<bool, StackerDbError> {
        self.metadata().verify(writer)
    }

    pub fn encode(&self) -> Result<Vec<u8>, StackerDbError> {
        if self.data.len() > MAX_CHUNK_SIZE {
            return Err(StackerDbError::ChunkTooLarge);
        }
        let length = u32::try_from(self.data.len()).map_err(|_| StackerDbError::ChunkTooLarge)?;
        let mut bytes = Vec::with_capacity(77 + self.data.len());
        bytes.extend_from_slice(&self.slot_id.to_be_bytes());
        bytes.extend_from_slice(&self.slot_version.to_be_bytes());
        bytes.extend_from_slice(self.signature.as_bytes());
        bytes.extend_from_slice(&length.to_be_bytes());
        bytes.extend_from_slice(&self.data);
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, StackerDbError> {
        let mut reader = ChunkReader { bytes, offset: 0 };
        let slot_id = reader.u32()?;
        let slot_version = reader.u32()?;
        let signature = MessageSignature::from_bytes(reader.array()?);
        let length = usize::try_from(reader.u32()?).map_err(|_| StackerDbError::ChunkTooLarge)?;
        if length > MAX_CHUNK_SIZE {
            return Err(StackerDbError::ChunkTooLarge);
        }
        let data = reader.take(length)?.to_vec();
        if reader.offset != bytes.len() {
            return Err(StackerDbError::TrailingBytes);
        }
        Ok(Self {
            slot_id,
            slot_version,
            signature,
            data,
        })
    }
}

/// Construct the canonical endpoint for a chunk version.
#[must_use]
pub fn chunk_path(
    address: StacksAddress,
    contract: &str,
    slot: u32,
    version: Option<u32>,
) -> String {
    version.map_or_else(
        || format!("/v2/stackerdb/{address}/{contract}/{slot}"),
        |version| format!("/v2/stackerdb/{address}/{contract}/{slot}/{version}"),
    )
}

struct ChunkReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> ChunkReader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], StackerDbError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(StackerDbError::Truncated)?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or(StackerDbError::Truncated)?;
        self.offset = end;
        Ok(bytes)
    }

    fn array<const LENGTH: usize>(&mut self) -> Result<[u8; LENGTH], StackerDbError> {
        self.take(LENGTH)?
            .try_into()
            .map_err(|_| StackerDbError::Truncated)
    }

    fn u32(&mut self) -> Result<u32, StackerDbError> {
        Ok(u32::from_be_bytes(self.array()?))
    }
}

#[cfg(test)]
mod tests {
    use nano_crypto::StacksPrivateKey;
    use nano_primitives::hash160;

    use super::{Chunk, MAX_CHUNK_SIZE, StackerDbError};

    #[test]
    fn signed_chunks_round_trip_and_verify() {
        let key = StacksPrivateKey::from_seed(b"stackerdb");
        let writer = hash160(&key.public_key().to_bytes_compressed());
        let mut chunk = Chunk::new(1, 2, vec![3; 128]);
        chunk.sign(&key).expect("sign chunk");

        let encoded = chunk.encode().expect("encode chunk");
        let decoded = Chunk::decode(&encoded).expect("decode chunk");
        assert_eq!(decoded, chunk);
        assert!(decoded.verify(writer).expect("verify chunk"));
    }

    #[test]
    fn chunks_reject_oversized_payloads() {
        let chunk = Chunk::new(0, 0, vec![0; MAX_CHUNK_SIZE + 1]);
        assert!(matches!(chunk.encode(), Err(StackerDbError::ChunkTooLarge)));
    }
}
