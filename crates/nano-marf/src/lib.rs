#![forbid(unsafe_code)]

use std::fmt;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TrieNodeId {
    Leaf = 1,
    Node4 = 2,
    Node16 = 3,
    Node48 = 4,
    Node256 = 5,
}

impl TrieNodeId {
    const fn pointer_count(self) -> usize {
        match self {
            Self::Leaf => 0,
            Self::Node4 => 4,
            Self::Node16 => 16,
            Self::Node48 => 48,
            Self::Node256 => 256,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TriePointer {
    pub id: u8,
    pub character: u8,
    pub referenced_block: Option<[u8; 32]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MarfError {
    InvalidPath,
    InvalidPointerCount,
    InvalidBackPointer,
}

impl fmt::Display for MarfError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidPath => "trie path exceeds 32 bytes",
            Self::InvalidPointerCount => "trie node has the wrong number of child pointers",
            Self::InvalidBackPointer => "trie pointer has an invalid referenced block",
        })
    }
}

impl std::error::Error for MarfError {}

/// Hash an internal node's consensus preimage.
pub fn internal_node_hash(
    id: TrieNodeId,
    pointers: &[TriePointer],
    path: &[u8],
    child_hashes: &[TrieHash],
) -> Result<TrieHash, MarfError> {
    if path.len() > 32 {
        return Err(MarfError::InvalidPath);
    }
    let pointers_match_node = pointers.len() == id.pointer_count();
    let hashes_match_pointers = child_hashes.len() == pointers.len();
    if !(pointers_match_node && hashes_match_pointers) {
        return Err(MarfError::InvalidPointerCount);
    }

    let mut bytes =
        Vec::with_capacity(1 + pointers.len() * 34 + 1 + path.len() + child_hashes.len() * 32);
    bytes.push(id as u8);
    for pointer in pointers {
        bytes.push(pointer.id & 0x8f);
        bytes.push(pointer.character);
        match (pointer.id & 0x80 != 0, pointer.referenced_block) {
            (true, Some(block)) => bytes.extend_from_slice(&block),
            (false, None) => bytes.extend_from_slice(&[0; 32]),
            _ => return Err(MarfError::InvalidBackPointer),
        }
    }
    bytes.push(u8::try_from(path.len()).expect("validated path length"));
    bytes.extend_from_slice(path);
    for child_hash in child_hashes {
        bytes.extend_from_slice(child_hash.as_bytes());
    }
    Ok(TrieHash::from_bytes(*sha512_256(&bytes).as_bytes()))
}

/// Hash a MARF leaf's path suffix and fixed-width value.
pub fn leaf_hash(path: &[u8], value: MarfValue) -> Result<TrieHash, MarfError> {
    if path.len() > 32 {
        return Err(MarfError::InvalidPath);
    }
    let mut bytes = Vec::with_capacity(42 + path.len());
    bytes.push(TrieNodeId::Leaf as u8);
    bytes.push(u8::try_from(path.len()).expect("validated path length"));
    bytes.extend_from_slice(path);
    bytes.extend_from_slice(value.as_bytes());
    Ok(TrieHash::from_bytes(*sha512_256(&bytes).as_bytes()))
}

/// Fold a trie content hash into the MARF's power-of-two ancestor history.
#[must_use]
pub fn state_root(content: TrieHash, ancestor_roots: &[TrieHash]) -> TrieHash {
    let mut bytes = Vec::with_capacity(32 * (ancestor_roots.len().saturating_add(1)));
    bytes.extend_from_slice(content.as_bytes());
    let mut distance = 1_usize;
    while distance <= ancestor_roots.len() {
        bytes.extend_from_slice(ancestor_roots[distance - 1].as_bytes());
        distance = distance.saturating_mul(2);
    }
    TrieHash::from_bytes(*sha512_256(&bytes).as_bytes())
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
    use super::{
        MarfError, MarfValue, TrieNodeId, internal_node_hash, key_path, leaf_hash, state_root,
    };

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

    #[test]
    fn internal_node_requires_every_fixed_pointer_slot() {
        assert_eq!(
            internal_node_hash(TrieNodeId::Node4, &[], b"", &[]),
            Err(MarfError::InvalidPointerCount)
        );
    }

    #[test]
    fn leaf_paths_are_limited_to_a_single_hash_suffix() {
        assert_eq!(
            leaf_hash(&[0; 33], MarfValue::from_u32(1)),
            Err(MarfError::InvalidPath)
        );
    }

    #[test]
    fn state_root_selects_power_of_two_ancestors() {
        let content = key_path(b"content");
        let ancestors = [
            key_path(b"one"),
            key_path(b"two"),
            key_path(b"three"),
            key_path(b"four"),
        ];
        let root = state_root(content, &ancestors);
        let expected = nano_primitives::sha512_256(
            &[
                content.as_bytes().as_slice(),
                ancestors[0].as_bytes().as_slice(),
                ancestors[1].as_bytes().as_slice(),
                ancestors[3].as_bytes().as_slice(),
            ]
            .concat(),
        );
        assert_eq!(root.as_bytes(), expected.as_bytes());
    }
}
