#![forbid(unsafe_code)]

use std::fmt;

use curve25519_dalek::{
    constants::ED25519_BASEPOINT_POINT,
    edwards::{CompressedEdwardsY, EdwardsPoint},
    scalar::{Scalar, clamp_integer},
};
use ed25519_dalek::{SigningKey, VerifyingKey};
use nano_primitives::sha256;
use secp256k1::{
    Message, PublicKey, Secp256k1, SecretKey,
    ecdsa::{RecoverableSignature, RecoveryId},
};
use sha2::{Digest, Sha512};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessageSignature([u8; 65]);

impl MessageSignature {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 65]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 65] {
        &self.0
    }

    pub fn recover(&self, digest: &[u8; 32]) -> Result<StacksPublicKey, CryptoError> {
        let message = Message::from_digest(*digest);
        let signature = self.recoverable_signature()?;
        Secp256k1::new()
            .recover_ecdsa(message, &signature)
            .map(StacksPublicKey)
            .map_err(|_| CryptoError::InvalidSignature)
    }

    pub fn is_low_s(&self) -> Result<bool, CryptoError> {
        let standard = self.recoverable_signature()?.to_standard();
        let mut normalized = standard;
        normalized.normalize_s();
        Ok(normalized == standard)
    }

    fn recoverable_signature(&self) -> Result<RecoverableSignature, CryptoError> {
        let recovery_id = RecoveryId::try_from(i32::from(self.0[0]))
            .map_err(|_| CryptoError::InvalidRecoveryId)?;
        RecoverableSignature::from_compact(&self.0[1..], recovery_id)
            .map_err(|_| CryptoError::InvalidSignature)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StacksPublicKey(PublicKey);

impl StacksPublicKey {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CryptoError> {
        PublicKey::from_slice(bytes)
            .map(Self)
            .map_err(|_| CryptoError::InvalidPublicKey)
    }

    #[must_use]
    pub fn to_bytes_compressed(&self) -> [u8; 33] {
        self.0.serialize()
    }

    #[must_use]
    pub fn to_bytes_uncompressed(&self) -> [u8; 65] {
        self.0.serialize_uncompressed()
    }

    pub fn verify_transaction(
        &self,
        digest: &[u8; 32],
        signature: &MessageSignature,
    ) -> Result<(), CryptoError> {
        if !signature.is_low_s()? {
            return Err(CryptoError::HighS);
        }
        if signature.recover(digest)? != *self {
            return Err(CryptoError::SignatureMismatch);
        }
        Ok(())
    }

    pub fn verify_signer(
        &self,
        digest: &[u8; 32],
        signature: &MessageSignature,
    ) -> Result<(), CryptoError> {
        if signature.recover(digest)? != *self {
            return Err(CryptoError::SignatureMismatch);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct StacksPrivateKey(SecretKey);

impl StacksPrivateKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self, CryptoError> {
        SecretKey::from_byte_array(bytes)
            .map(Self)
            .map_err(|_| CryptoError::InvalidPrivateKey)
    }

    #[must_use]
    pub fn from_seed(seed: &[u8]) -> Self {
        let mut candidate = seed.to_vec();
        loop {
            if let Ok(bytes) = <[u8; 32]>::try_from(candidate.as_slice())
                && let Ok(key) = Self::from_bytes(bytes)
            {
                return key;
            }
            candidate = sha256(&candidate).as_bytes().to_vec();
        }
    }

    #[must_use]
    pub fn public_key(&self) -> StacksPublicKey {
        StacksPublicKey(PublicKey::from_secret_key(&Secp256k1::new(), &self.0))
    }

    #[must_use]
    pub fn sign(&self, digest: &[u8; 32]) -> MessageSignature {
        let message = Message::from_digest(*digest);
        let signature = Secp256k1::new().sign_ecdsa_recoverable(message, &self.0);
        let (recovery_id, compact) = signature.serialize_compact();
        let mut bytes = [0; 65];
        bytes[0] = u8::try_from(i32::from(recovery_id)).expect("recovery IDs are in 0..4");
        bytes[1..].copy_from_slice(&compact);
        MessageSignature(bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CryptoError {
    InvalidDigest,
    InvalidRecoveryId,
    InvalidSignature,
    InvalidPublicKey,
    InvalidPrivateKey,
    HighS,
    SignatureMismatch,
}

impl fmt::Display for CryptoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidDigest => "invalid 32-byte digest",
            Self::InvalidRecoveryId => "invalid recovery ID",
            Self::InvalidSignature => "invalid recoverable signature",
            Self::InvalidPublicKey => "invalid secp256k1 public key",
            Self::InvalidPrivateKey => "invalid secp256k1 private key",
            Self::HighS => "high-S signatures are invalid",
            Self::SignatureMismatch => "signature does not recover the expected public key",
        })
    }
}

impl std::error::Error for CryptoError {}

const VRF_SUITE: u8 = 0x03;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VrfPrivateKey(SigningKey);

impl VrfPrivateKey {
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(SigningKey::from_bytes(&bytes))
    }

    #[must_use]
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    #[must_use]
    pub fn public_key(&self) -> VrfPublicKey {
        VrfPublicKey(self.0.verifying_key())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VrfPublicKey(VerifyingKey);

impl VrfPublicKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self, VrfError> {
        let point = CompressedEdwardsY(bytes)
            .decompress()
            .ok_or(VrfError::InvalidPublicKey)?;
        if point.is_small_order() {
            return Err(VrfError::InvalidPublicKey);
        }
        VerifyingKey::from_bytes(&bytes)
            .map(Self)
            .map_err(|_| VrfError::InvalidPublicKey)
    }

    #[must_use]
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VrfProof {
    gamma: EdwardsPoint,
    challenge: Scalar,
    response: Scalar,
}

impl VrfProof {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, VrfError> {
        let bytes: &[u8; 80] = bytes.try_into().map_err(|_| VrfError::InvalidProof)?;
        let gamma = CompressedEdwardsY(bytes[..32].try_into().expect("fixed-size slice"))
            .decompress()
            .filter(|point| !point.is_small_order())
            .ok_or(VrfError::InvalidProof)?;
        let mut challenge = [0; 32];
        challenge[..16].copy_from_slice(&bytes[32..48]);
        let mut response = [0; 32];
        response.copy_from_slice(&bytes[48..]);
        let challenge = Option::<Scalar>::from(Scalar::from_canonical_bytes(challenge))
            .ok_or(VrfError::InvalidProof)?;
        let response = Option::<Scalar>::from(Scalar::from_canonical_bytes(response))
            .ok_or(VrfError::InvalidProof)?;
        Ok(Self {
            gamma,
            challenge,
            response,
        })
    }

    #[must_use]
    pub fn to_bytes(&self) -> [u8; 80] {
        let challenge = self.challenge.to_bytes();
        debug_assert!(challenge[16..].iter().all(|byte| *byte == 0));
        let mut bytes = [0; 80];
        bytes[..32].copy_from_slice(&self.gamma.compress().to_bytes());
        bytes[32..48].copy_from_slice(&challenge[..16]);
        bytes[48..].copy_from_slice(&self.response.to_bytes());
        bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VrfError {
    InvalidPublicKey,
    InvalidProof,
    InvalidChallenge,
}

impl fmt::Display for VrfError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidPublicKey => "invalid VRF public key",
            Self::InvalidProof => "invalid VRF proof",
            Self::InvalidChallenge => "invalid VRF challenge",
        })
    }
}

impl std::error::Error for VrfError {}

pub struct Vrf;

impl Vrf {
    pub fn prove(private_key: &VrfPrivateKey, message: &[u8]) -> Result<VrfProof, VrfError> {
        let (public_key, scalar, nonce_prefix) = Self::expand_private_key(private_key);
        let hash_point = Self::hash_to_curve(&public_key, message);
        let gamma = scalar * hash_point;
        let nonce = Self::nonce(&nonce_prefix, &hash_point);
        let challenge = Self::challenge(
            &hash_point,
            &gamma,
            &(nonce * ED25519_BASEPOINT_POINT),
            &(nonce * hash_point),
        )?;
        Ok(VrfProof {
            gamma,
            challenge,
            response: nonce + challenge * scalar,
        })
    }

    pub fn verify(
        public_key: &VrfPublicKey,
        proof: &VrfProof,
        message: &[u8],
    ) -> Result<bool, VrfError> {
        if proof.gamma.is_small_order() {
            return Err(VrfError::InvalidPublicKey);
        }
        let hash_point = Self::hash_to_curve(public_key, message);
        let public_point = CompressedEdwardsY(public_key.to_bytes())
            .decompress()
            .ok_or(VrfError::InvalidPublicKey)?;
        let first = proof.response * ED25519_BASEPOINT_POINT - proof.challenge * public_point;
        let second = proof.response * hash_point - proof.challenge * proof.gamma;
        Ok(Self::challenge(&hash_point, &proof.gamma, &first, &second)? == proof.challenge)
    }

    fn hash_to_curve(public_key: &VrfPublicKey, message: &[u8]) -> EdwardsPoint {
        let mut counter = 0_u64;
        loop {
            let mut hasher = Sha512::new();
            hasher.update([VRF_SUITE, 0x01]);
            hasher.update(public_key.to_bytes());
            hasher.update(message);
            if counter == 0 {
                hasher.update([0]);
            } else {
                for (index, byte) in counter.to_le_bytes().iter().enumerate() {
                    if counter > 1_u64 << (index * 8) {
                        hasher.update([*byte]);
                    }
                }
            }
            if let Some(point) =
                CompressedEdwardsY(hasher.finalize()[..32].try_into().expect("digest prefix"))
                    .decompress()
            {
                return point.mul_by_cofactor();
            }
            counter = counter.checked_add(1).expect("hash-to-curve exhausted u64");
        }
    }

    fn challenge(
        first: &EdwardsPoint,
        second: &EdwardsPoint,
        third: &EdwardsPoint,
        fourth: &EdwardsPoint,
    ) -> Result<Scalar, VrfError> {
        let mut hasher = Sha512::new();
        hasher.update([VRF_SUITE, 0x02]);
        for point in [first, second, third, fourth] {
            hasher.update(point.compress().to_bytes());
        }
        let mut bytes = [0; 32];
        bytes[..16].copy_from_slice(&hasher.finalize()[..16]);
        Option::<Scalar>::from(Scalar::from_canonical_bytes(bytes))
            .ok_or(VrfError::InvalidChallenge)
    }

    fn expand_private_key(private_key: &VrfPrivateKey) -> (VrfPublicKey, Scalar, [u8; 32]) {
        let digest = Sha512::digest(private_key.to_bytes());
        let mut scalar_bytes = [0; 32];
        scalar_bytes.copy_from_slice(&digest[..32]);
        scalar_bytes[0] &= 0xf8;
        scalar_bytes[31] &= 0x7f;
        scalar_bytes[31] |= 0x40;
        let mut nonce_prefix = [0; 32];
        nonce_prefix.copy_from_slice(&digest[32..]);
        (
            private_key.public_key(),
            Scalar::from_bytes_mod_order(clamp_integer(scalar_bytes)),
            nonce_prefix,
        )
    }

    fn nonce(prefix: &[u8; 32], hash_point: &EdwardsPoint) -> Scalar {
        let mut hasher = Sha512::new();
        hasher.update(prefix);
        hasher.update(hash_point.compress().to_bytes());
        Scalar::from_bytes_mod_order_wide(&hasher.finalize().into())
    }
}

#[cfg(test)]
mod tests {
    use super::StacksPrivateKey;

    #[test]
    fn signs_and_recovers() {
        let private = StacksPrivateKey::from_seed(b"nano-stacks");
        let digest = [7; 32];
        let signature = private.sign(&digest);
        let public = private.public_key();
        assert!(signature.is_low_s().expect("valid signature"));
        assert_eq!(signature.recover(&digest).expect("recovers"), public);
        public
            .verify_transaction(&digest, &signature)
            .expect("valid transaction signature");
    }
}
