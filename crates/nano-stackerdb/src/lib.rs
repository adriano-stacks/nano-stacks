#![forbid(unsafe_code)]

use std::fmt;

use nano_address::StacksAddress;
use nano_crypto::{CryptoError, MessageSignature, StacksPrivateKey};
use nano_primitives::{Hash160, Sha256Sum, hash160, sha512_256};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};

mod signer_message;

pub use signer_message::{
    BlockAcceptance, BlockProposal, BlockRejection, BlockResponse, CurrentMiner,
    LATEST_SIGNER_PROTOCOL_VERSION, SignerMessage, SignerMessageError, SignerMessageType,
    StateMachineUpdate,
};

/// A `StackerDB` contract address and name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackerDbContract {
    pub address: StacksAddress,
    pub name: String,
}

/// HTTP client for a node's `StackerDB` endpoints.
#[derive(Clone, Debug)]
pub struct StackerDbClient {
    client: Client,
    base_url: Url,
}

/// The current version of a `StackerDB` writer slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlotVersion {
    pub slot_id: u32,
    pub slot_version: u32,
}

/// Acknowledgement returned after uploading a `StackerDB` chunk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChunkAck {
    pub accepted: bool,
    pub reason: Option<String>,
    pub code: Option<u32>,
    pub metadata: Option<SlotVersion>,
}

/// Errors returned by the `StackerDB` HTTP API.
#[derive(Debug)]
pub enum StackerDbClientError {
    InvalidBaseUrl,
    Http(reqwest::Error),
}

impl fmt::Display for StackerDbClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBaseUrl => formatter.write_str("StackerDB base URL cannot be a base"),
            Self::Http(error) => write!(formatter, "StackerDB HTTP error: {error}"),
        }
    }
}

impl std::error::Error for StackerDbClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Http(error) => Some(error),
            Self::InvalidBaseUrl => None,
        }
    }
}

impl From<reqwest::Error> for StackerDbClientError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error)
    }
}

impl StackerDbClient {
    /// Construct a client for an HTTP node endpoint.
    pub fn new(base_url: Url) -> Result<Self, StackerDbClientError> {
        if base_url.cannot_be_a_base() {
            return Err(StackerDbClientError::InvalidBaseUrl);
        }
        Ok(Self {
            client: Client::new(),
            base_url,
        })
    }

    /// Return current slot versions for a contract.
    pub async fn slot_versions(
        &self,
        contract: &StackerDbContract,
    ) -> Result<Vec<SlotVersion>, StackerDbClientError> {
        let path = format!("v2/stackerdb/{}/{}", contract.address, contract.name);
        let slots: Vec<SlotVersionWire> = self.get_json(&path).await?;
        Ok(slots
            .into_iter()
            .map(|slot| SlotVersion {
                slot_id: slot.slot_id,
                slot_version: slot.slot_version,
            })
            .collect())
    }

    /// Fetch the latest bytes written to a slot, if that slot has content.
    pub async fn latest_chunk(
        &self,
        contract: &StackerDbContract,
        slot: u32,
    ) -> Result<Option<Vec<u8>>, StackerDbClientError> {
        let path = chunk_path(contract.address, &contract.name, slot, None);
        let url = self.url(&path)?;
        let response = self.client.get(url).send().await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(response.error_for_status()?.bytes().await?.to_vec()))
    }

    /// Upload one signed chunk and return the node's acknowledgement.
    pub async fn put_chunk(
        &self,
        contract: &StackerDbContract,
        chunk: &Chunk,
    ) -> Result<ChunkAck, StackerDbClientError> {
        let path = format!("v2/stackerdb/{}/{}/chunks", contract.address, contract.name);
        let acknowledgement: ChunkAckWire = self
            .client
            .post(self.url(&path)?)
            .json(&ChunkWire::from(chunk))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(ChunkAck {
            accepted: acknowledgement.accepted,
            reason: acknowledgement.reason,
            code: acknowledgement.code,
            metadata: acknowledgement.metadata.map(|metadata| SlotVersion {
                slot_id: metadata.slot_id,
                slot_version: metadata.slot_version,
            }),
        })
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
    ) -> Result<T, StackerDbClientError> {
        Ok(self
            .client
            .get(self.url(path)?)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    fn url(&self, path: &str) -> Result<Url, StackerDbClientError> {
        self.base_url
            .join(path.trim_start_matches('/'))
            .map_err(|_| StackerDbClientError::InvalidBaseUrl)
    }
}

#[derive(Deserialize)]
struct SlotVersionWire {
    slot_id: u32,
    slot_version: u32,
}

#[derive(Deserialize)]
struct ChunkAckWire {
    accepted: bool,
    reason: Option<String>,
    code: Option<u32>,
    metadata: Option<SlotVersionWire>,
}

#[derive(Serialize)]
struct ChunkWire {
    slot_id: u32,
    slot_version: u32,
    sig: String,
    data: String,
}

impl From<&Chunk> for ChunkWire {
    fn from(chunk: &Chunk) -> Self {
        Self {
            slot_id: chunk.slot_id,
            slot_version: chunk.slot_version,
            sig: hex::encode(chunk.signature.as_bytes()),
            data: hex::encode(&chunk.data),
        }
    }
}

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
    use std::str::FromStr;

    use nano_address::StacksAddress;
    use nano_crypto::StacksPrivateKey;
    use nano_primitives::{hash160, sha512_256};
    use reqwest::Url;

    use super::{
        BlockAcceptance, BlockResponse, Chunk, MAX_CHUNK_SIZE, SignerMessage, StackerDbClient,
        StackerDbContract, StackerDbError,
    };

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

    #[test]
    fn block_acceptance_messages_round_trip() {
        let key = StacksPrivateKey::from_seed(b"signer-message");
        let digest = sha512_256(b"proposed block");
        let response = BlockAcceptance::new(digest, key.sign(digest.as_bytes()));
        let message = SignerMessage::BlockResponse(BlockResponse::Accepted(response));

        let decoded = SignerMessage::decode(&message.encode().expect("encode message"))
            .expect("decode message");
        assert_eq!(decoded, message);
    }

    #[test]
    fn block_acceptance_rejects_unused_response_data() {
        let key = StacksPrivateKey::from_seed(b"signer-message");
        let digest = sha512_256(b"proposed block");
        let message = SignerMessage::BlockResponse(BlockResponse::Accepted(BlockAcceptance::new(
            digest,
            key.sign(digest.as_bytes()),
        )));
        let mut bytes = message.encode().expect("encode message");
        let server_version_length_offset = 2 + 32 + 65;
        let server_version_length = usize::try_from(u32::from_be_bytes(
            bytes[server_version_length_offset..server_version_length_offset + 4]
                .try_into()
                .expect("server version length"),
        ))
        .expect("u32 fits usize");
        let response_data_length_offset =
            server_version_length_offset + 4 + server_version_length + 1;
        let response_data_length = u32::from_be_bytes(
            bytes[response_data_length_offset..response_data_length_offset + 4]
                .try_into()
                .expect("response data length"),
        );
        bytes[response_data_length_offset..response_data_length_offset + 4]
            .copy_from_slice(&(response_data_length + 1).to_be_bytes());
        bytes.push(0);

        assert!(matches!(
            SignerMessage::decode(&bytes),
            Err(super::SignerMessageError::TrailingBytes)
        ));
    }

    /// A state machine update a stock signer published, byte for byte.
    ///
    /// Reading these is what lets nano agree with the reward set on who the
    /// current miner is, which a stock signer requires before it will validate
    /// any block at all.
    #[test]
    fn decodes_a_stock_signer_state_update() {
        let bytes = hex::decode(concat!(
            "060000000000000002000000000000000200000085801d35ade2d7479accc24b209",
            "303a8024c12118c0000000000000257018585d3e4653be56d6e68e7aa7ddcc5140c",
            "e8ddccf81dd571922684030c768c95a146e310707733674cdcb7b6b53482907c145",
            "8503f9a1d0dc948140beaa1c1ec183298cd437ddd5b897943f340a3c4576259f55c",
            "f32e583441960fad000000000000046d00000000"
        ))
        .expect("hexadecimal chunk");

        let SignerMessage::StateMachineUpdate(update) =
            SignerMessage::decode(&bytes).expect("decode the state update")
        else {
            panic!("the chunk is a state machine update");
        };
        assert_eq!(update.active_protocol_version, 2);
        assert_eq!(update.bitcoin_height, 599);
        assert!(matches!(
            update.current_miner,
            super::CurrentMiner::Active {
                parent_tenure_last_block_height: 1133,
                ..
            }
        ));
        assert_eq!(
            SignerMessage::StateMachineUpdate(update)
                .encode()
                .expect("re-encode the state update"),
            bytes
        );
    }

    /// The promise a signer publishes before it signs, which the rest of the
    /// reward set waits for before signing at all.
    #[test]
    fn block_pre_commits_round_trip() {
        let hash = nano_primitives::Sha256Sum::from_bytes([7; 32]);
        let bytes = SignerMessage::BlockPreCommit(hash)
            .encode()
            .expect("encode a pre-commit");

        assert_eq!(bytes[0], 7, "a pre-commit is payload type 7");
        assert_eq!(
            SignerMessage::decode(&bytes),
            Ok(SignerMessage::BlockPreCommit(hash))
        );
    }

    #[tokio::test]
    #[ignore = "requires a local Hacknet node on port 20443"]
    async fn hacknet_miners_stackerdb_is_readable() {
        let client =
            StackerDbClient::new(Url::parse("http://127.0.0.1:20443/").expect("valid Hacknet URL"))
                .expect("create client");
        let contract = StackerDbContract {
            address: StacksAddress::from_str("ST000000000000000000002AMW42H")
                .expect("system address"),
            name: "miners".to_owned(),
        };

        let slots = client.slot_versions(&contract).await.expect("list slots");
        assert!(slots.iter().any(|slot| slot.slot_id == 0));
        assert!(
            client
                .latest_chunk(&contract, 0)
                .await
                .expect("read proposal slot")
                .is_some()
        );
    }
}
