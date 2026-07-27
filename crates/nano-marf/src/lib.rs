#![forbid(unsafe_code)]

use nano_primitives::{TrieHash, sha512_256};

/// The 40-byte value stored in a MARF leaf.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MarfValue([u8; 40]);

impl MarfValue {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 40]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn from_value(value: &[u8]) -> Self {
        Self::from_value_hash(sha512_256(value))
    }

    #[must_use]
    pub const fn from_value_hash(hash: nano_primitives::Sha256Sum) -> Self {
        let mut bytes = [0; 40];
        let hash_bytes = hash.as_bytes();
        let mut index = 0;
        while index < hash_bytes.len() {
            bytes[index] = hash_bytes[index];
            index += 1;
        }
        Self(bytes)
    }

    #[must_use]
    pub const fn from_u32(value: u32) -> Self {
        let mut bytes = [0; 40];
        let value = value.to_le_bytes();
        bytes[0] = value[0];
        bytes[1] = value[1];
        bytes[2] = value[2];
        bytes[3] = value[3];
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 40] {
        &self.0
    }

    #[must_use]
    pub const fn value_hash(&self) -> TrieHash {
        let mut bytes = [0; 32];
        let mut index = 0;
        while index < bytes.len() {
            bytes[index] = self.0[index];
            index += 1;
        }
        TrieHash::from_bytes(bytes)
    }
}

impl From<u32> for MarfValue {
    fn from(value: u32) -> Self {
        Self::from_u32(value)
    }
}

/// Hash a logical key into the 32-byte trie path.
#[must_use]
pub fn key_path(key: &[u8]) -> TrieHash {
    TrieHash::from_data(key)
}

/// A state root calculated by the MARF.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StateRoot(pub [u8; 32]);

impl StateRoot {
    #[must_use]
    pub const fn empty() -> Self {
        Self([0; 32])
    }
}

#[cfg(test)]
mod tests {
    use super::{MarfValue, key_path};

    #[test]
    fn value_hashing_and_integer_encoding_are_canonical() {
        let value = MarfValue::from_value(b"hello");
        assert_eq!(
            value.value_hash().to_string(),
            "e30d87cfa2a75db545eac4d61baf970366a8357c7f72fa95b52d0accb698f13a"
        );
        assert_eq!(&value.as_bytes()[32..], &[0; 8]);
        assert_eq!(
            MarfValue::from_u32(0x1020_3040).as_bytes(),
            &[
                0x40, 0x30, 0x20, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
            ]
        );
    }

    #[test]
    fn empty_key_uses_the_consensus_empty_path() {
        assert_eq!(key_path(b""), nano_primitives::TrieHash::EMPTY);
    }
}
