#![forbid(unsafe_code)]

use std::{collections::BTreeMap, fmt};

use nano_primitives::{TrieHash, sha512_256};

mod checkpoint;

pub use checkpoint::{CheckpointError, import_checkpoint, import_pcs};

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
    pub const fn from_block_id(block: [u8; 32]) -> Self {
        let mut bytes = [0; 40];
        let mut index = 0;
        while index < block.len() {
            bytes[index] = block[index];
            index += 1;
        }
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
    UnknownVersion,
    VersionAlreadyExists,
    WriteInProgress,
    WriteNotBegun,
    HeightOverflow,
}

impl fmt::Display for MarfError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidPath => "trie path exceeds 32 bytes",
            Self::InvalidPointerCount => "trie node has the wrong number of child pointers",
            Self::InvalidBackPointer => "trie pointer has an invalid referenced block",
            Self::UnknownVersion => "MARF version does not exist",
            Self::VersionAlreadyExists => "MARF version already exists",
            Self::WriteInProgress => "MARF version write is already in progress",
            Self::WriteNotBegun => "MARF version write has not begun",
            Self::HeightOverflow => "MARF version height overflowed",
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
    if ancestor_roots.is_empty() {
        return content;
    }
    let mut bytes = Vec::with_capacity(32 * (ancestor_roots.len().saturating_add(1)));
    bytes.extend_from_slice(content.as_bytes());
    let mut distance = 1_usize;
    while distance <= ancestor_roots.len() {
        bytes.extend_from_slice(ancestor_roots[distance - 1].as_bytes());
        distance = distance.saturating_mul(2);
    }
    TrieHash::from_bytes(*sha512_256(&bytes).as_bytes())
}

/// An in-memory, path-compressed trie using MARF's consensus node layouts.
#[derive(Clone, Debug, Default)]
pub struct MarfTrie {
    root_children: Vec<TrieChild>,
}

#[derive(Clone, Debug)]
enum TrieNode {
    Leaf {
        path: Vec<u8>,
        value: MarfValue,
    },
    Internal {
        path: Vec<u8>,
        children: Vec<TrieChild>,
    },
}

#[derive(Clone, Debug)]
struct TrieChild {
    character: u8,
    node: Box<TrieNode>,
    referenced_block: Option<[u8; 32]>,
}

impl TrieChild {
    fn local(character: u8, node: TrieNode) -> Self {
        Self {
            character,
            node: Box::new(node),
            referenced_block: None,
        }
    }

    fn insert(&mut self, path: &[u8], value: MarfValue) {
        if let Some(block) = self.referenced_block.take() {
            self.node.prepare_for_copy(block);
        }
        self.node.insert(path, value);
    }
}

impl MarfTrie {
    pub fn insert(&mut self, key: &[u8], value: MarfValue) {
        self.insert_path(*key_path(key).as_bytes(), value);
    }

    pub fn insert_path(&mut self, path: [u8; 32], value: MarfValue) {
        let root_character = path[0];
        let suffix = &path[1..];
        if let Some(child) = self
            .root_children
            .iter_mut()
            .find(|child| child.character == root_character)
        {
            child.insert(suffix, value);
        } else {
            self.root_children.push(TrieChild::local(
                root_character,
                TrieNode::Leaf {
                    path: suffix.to_vec(),
                    value,
                },
            ));
        }
    }

    #[must_use]
    pub fn get(&self, key: &[u8]) -> Option<MarfValue> {
        self.get_path(*key_path(key).as_bytes())
    }

    #[must_use]
    pub fn get_path(&self, path: [u8; 32]) -> Option<MarfValue> {
        self.root_children
            .iter()
            .find(|child| child.character == path[0])
            .and_then(|child| child.node.get(&path[1..]))
    }

    #[must_use]
    pub fn root_hash(&self) -> TrieHash {
        hash_children(TrieNodeId::Node256, &[], &self.root_children)
    }

    fn prepare_root_for_copy(&mut self, block: [u8; 32]) {
        for child in &mut self.root_children {
            if child.referenced_block.is_none() {
                child.referenced_block = Some(block);
            }
        }
    }
}

impl TrieNode {
    fn get(&self, path: &[u8]) -> Option<MarfValue> {
        match self {
            Self::Leaf {
                path: leaf_path,
                value,
            } => (leaf_path == path).then_some(*value),
            Self::Internal {
                path: node_path,
                children,
            } => path
                .strip_prefix(node_path.as_slice())
                .and_then(|remaining| remaining.split_first())
                .and_then(|(character, remaining)| {
                    children
                        .iter()
                        .find(|child| child.character == *character)
                        .and_then(|child| child.node.get(remaining))
                }),
        }
    }

    fn insert(&mut self, path: &[u8], value: MarfValue) {
        match self {
            Self::Leaf {
                path: leaf_path,
                value: leaf_value,
            } => {
                if leaf_path == path {
                    *leaf_value = value;
                    return;
                }

                let shared = shared_prefix(leaf_path, path);
                let old_character = leaf_path[shared];
                let new_character = path[shared];
                let old_leaf = Self::Leaf {
                    path: leaf_path[shared + 1..].to_vec(),
                    value: *leaf_value,
                };
                let new_leaf = Self::Leaf {
                    path: path[shared + 1..].to_vec(),
                    value,
                };
                *self = Self::Internal {
                    path: leaf_path[..shared].to_vec(),
                    children: vec![
                        TrieChild::local(old_character, old_leaf),
                        TrieChild::local(new_character, new_leaf),
                    ],
                };
            }
            Self::Internal {
                path: node_path,
                children,
            } => {
                let shared = shared_prefix(node_path, path);
                if shared < node_path.len() {
                    let old_character = node_path[shared];
                    let new_character = path[shared];
                    let old_node = Self::Internal {
                        path: node_path[shared + 1..].to_vec(),
                        children: std::mem::take(children),
                    };
                    let new_leaf = Self::Leaf {
                        path: path[shared + 1..].to_vec(),
                        value,
                    };
                    *self = Self::Internal {
                        path: node_path[..shared].to_vec(),
                        children: vec![
                            TrieChild::local(old_character, old_node),
                            TrieChild::local(new_character, new_leaf),
                        ],
                    };
                    return;
                }

                let child_character = path[node_path.len()];
                let suffix = &path[node_path.len() + 1..];
                if let Some(child) = children
                    .iter_mut()
                    .find(|child| child.character == child_character)
                {
                    child.insert(suffix, value);
                } else {
                    children.push(TrieChild::local(
                        child_character,
                        Self::Leaf {
                            path: suffix.to_vec(),
                            value,
                        },
                    ));
                }
            }
        }
    }

    fn hash(&self) -> TrieHash {
        match self {
            Self::Leaf { path, value } => leaf_hash(path, *value).expect("leaf paths are bounded"),
            Self::Internal { path, children } => {
                let node_id = node_id_for_children(children.len());
                hash_children(node_id, path, children)
            }
        }
    }

    fn node_id(&self) -> TrieNodeId {
        match self {
            Self::Leaf { .. } => TrieNodeId::Leaf,
            Self::Internal { children, .. } => node_id_for_children(children.len()),
        }
    }

    fn prepare_for_copy(&mut self, block: [u8; 32]) {
        if let Self::Internal { children, .. } = self {
            for child in children {
                if child.referenced_block.is_none() {
                    child.referenced_block = Some(block);
                }
            }
        }
    }
}

fn hash_children(id: TrieNodeId, path: &[u8], children: &[TrieChild]) -> TrieHash {
    let mut pointers = vec![
        TriePointer {
            id: 0,
            character: 0,
            referenced_block: None,
        };
        id.pointer_count()
    ];
    let mut hashes = vec![TrieHash::EMPTY; id.pointer_count()];
    if id == TrieNodeId::Node256 {
        for child in children {
            let index = usize::from(child.character);
            pointers[index] = TriePointer {
                id: child.node.node_id() as u8 | u8::from(child.referenced_block.is_some()) << 7,
                character: child.character,
                referenced_block: child.referenced_block,
            };
            hashes[index] = child
                .referenced_block
                .map_or_else(|| child.node.hash(), TrieHash::from_bytes);
        }
    } else {
        for (index, child) in children.iter().enumerate() {
            pointers[index] = TriePointer {
                id: child.node.node_id() as u8 | u8::from(child.referenced_block.is_some()) << 7,
                character: child.character,
                referenced_block: child.referenced_block,
            };
            hashes[index] = child
                .referenced_block
                .map_or_else(|| child.node.hash(), TrieHash::from_bytes);
        }
    }
    internal_node_hash(id, &pointers, path, &hashes).expect("internally valid node layout")
}

const fn node_id_for_children(children: usize) -> TrieNodeId {
    match children {
        0 | 1 => panic!("an internal trie node needs at least two children"),
        2..=4 => TrieNodeId::Node4,
        5..=16 => TrieNodeId::Node16,
        17..=48 => TrieNodeId::Node48,
        _ => TrieNodeId::Node256,
    }
}

fn shared_prefix(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
}

/// An immutable MARF state identifier.
pub type MarfBlockId = [u8; 32];

/// A copy-on-write MARF keyed by block identifier.
#[derive(Clone, Debug, Default)]
pub struct VersionedMarf {
    versions: BTreeMap<MarfBlockId, MarfVersion>,
    active: Option<ActiveVersion>,
}

#[derive(Clone, Debug)]
struct MarfVersion {
    parent: Option<MarfBlockId>,
    height: u32,
    trie: MarfTrie,
    root: TrieHash,
}

#[derive(Clone, Debug)]
struct ActiveVersion {
    block: MarfBlockId,
    parent: Option<MarfBlockId>,
    height: u32,
    trie: MarfTrie,
}

impl VersionedMarf {
    /// Start a new state from an existing parent, or from an empty genesis state.
    pub fn begin(
        &mut self,
        parent: Option<MarfBlockId>,
        block: MarfBlockId,
    ) -> Result<(), MarfError> {
        if self.active.is_some() {
            return Err(MarfError::WriteInProgress);
        }
        if self.versions.contains_key(&block) {
            return Err(MarfError::VersionAlreadyExists);
        }

        let (mut trie, height) = match parent {
            Some(parent) => {
                let version = self
                    .versions
                    .get(&parent)
                    .ok_or(MarfError::UnknownVersion)?;
                let mut trie = version.trie.clone();
                trie.prepare_root_for_copy(parent);
                (
                    trie,
                    version
                        .height
                        .checked_add(1)
                        .ok_or(MarfError::HeightOverflow)?,
                )
            }
            None => (MarfTrie::default(), 0),
        };
        insert_metadata(&mut trie, parent, block, height);
        self.active = Some(ActiveVersion {
            block,
            parent,
            height,
            trie,
        });
        Ok(())
    }

    /// Write a raw path into the active state.
    pub fn insert_path(&mut self, path: [u8; 32], value: MarfValue) -> Result<(), MarfError> {
        self.active
            .as_mut()
            .ok_or(MarfError::WriteNotBegun)?
            .trie
            .insert_path(path, value);
        Ok(())
    }

    /// Write a logical key into the active state.
    pub fn insert(&mut self, key: &[u8], value: MarfValue) -> Result<(), MarfError> {
        self.insert_path(*key_path(key).as_bytes(), value)
    }

    /// Seal the active state and return its history-dependent root.
    pub fn seal(&mut self) -> Result<TrieHash, MarfError> {
        let active = self.active.take().ok_or(MarfError::WriteNotBegun)?;
        let root = state_root(
            active.trie.root_hash(),
            &self.ancestor_roots(active.parent)?,
        );
        self.versions.insert(
            active.block,
            MarfVersion {
                parent: active.parent,
                height: active.height,
                trie: active.trie,
                root,
            },
        );
        Ok(root)
    }

    /// Read a logical key from a sealed state.
    #[must_use]
    pub fn get(&self, block: MarfBlockId, key: &[u8]) -> Option<MarfValue> {
        self.versions.get(&block)?.trie.get(key)
    }

    /// Read a raw path from a sealed state.
    #[must_use]
    pub fn get_path(&self, block: MarfBlockId, path: [u8; 32]) -> Option<MarfValue> {
        self.versions.get(&block)?.trie.get_path(path)
    }

    /// Return a sealed state root.
    #[must_use]
    pub fn root(&self, block: MarfBlockId) -> Option<TrieHash> {
        self.versions.get(&block).map(|version| version.root)
    }

    /// Return a sealed state's parent block, if the state exists.
    #[must_use]
    pub fn parent(&self, block: MarfBlockId) -> Option<Option<MarfBlockId>> {
        self.versions.get(&block).map(|version| version.parent)
    }

    /// Return a sealed state's height.
    #[must_use]
    pub fn height(&self, block: MarfBlockId) -> Option<u32> {
        self.versions.get(&block).map(|version| version.height)
    }

    /// Find an ancestor at `height` from a sealed state.
    #[must_use]
    pub fn block_at_height(&self, mut block: MarfBlockId, height: u32) -> Option<MarfBlockId> {
        loop {
            let version = self.versions.get(&block)?;
            if version.height == height {
                return Some(block);
            }
            block = version.parent?;
        }
    }

    fn ancestor_roots(&self, parent: Option<MarfBlockId>) -> Result<Vec<TrieHash>, MarfError> {
        let mut roots = Vec::new();
        let mut cursor = parent;
        while let Some(block) = cursor {
            let version = self.versions.get(&block).ok_or(MarfError::UnknownVersion)?;
            roots.push(version.root);
            cursor = version.parent;
        }
        Ok(roots)
    }
}

const BLOCK_HASH_TO_HEIGHT_KEY: &str = "__MARF_BLOCK_HASH_TO_HEIGHT";
const BLOCK_HEIGHT_TO_HASH_KEY: &str = "__MARF_BLOCK_HEIGHT_TO_HASH";
const OWN_BLOCK_HEIGHT_KEY: &str = "__MARF_BLOCK_HEIGHT_SELF";

fn insert_metadata(
    trie: &mut MarfTrie,
    parent: Option<MarfBlockId>,
    block: MarfBlockId,
    height: u32,
) {
    trie.insert(OWN_BLOCK_HEIGHT_KEY.as_bytes(), MarfValue::from_u32(height));
    trie.insert(
        format!("{BLOCK_HEIGHT_TO_HASH_KEY}::{height}").as_bytes(),
        MarfValue::from_block_id(block),
    );
    trie.insert(
        format!("{BLOCK_HASH_TO_HEIGHT_KEY}::{}", block_hex(block)).as_bytes(),
        MarfValue::from_u32(height),
    );
    if let Some(parent) = parent {
        let previous_height = height
            .checked_sub(1)
            .expect("parent implies non-genesis height");
        trie.insert(
            format!("{BLOCK_HEIGHT_TO_HASH_KEY}::{previous_height}").as_bytes(),
            MarfValue::from_block_id(parent),
        );
        trie.insert(
            format!("{BLOCK_HASH_TO_HEIGHT_KEY}::{}", block_hex(parent)).as_bytes(),
            MarfValue::from_u32(previous_height),
        );
    }
}

fn block_hex(block: MarfBlockId) -> String {
    let mut hex = String::with_capacity(64);
    for byte in block {
        use fmt::Write;
        write!(&mut hex, "{byte:02x}").expect("writing to a string cannot fail");
    }
    hex
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
        BLOCK_HASH_TO_HEIGHT_KEY, BLOCK_HEIGHT_TO_HASH_KEY, MarfError, MarfTrie, MarfValue,
        OWN_BLOCK_HEIGHT_KEY, TrieHash, TrieNodeId, TriePointer, VersionedMarf, block_hex,
        internal_node_hash, key_path, leaf_hash, state_root,
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
        assert_eq!(state_root(content, &[]), content);
    }

    #[test]
    fn trie_overwrites_existing_paths_and_promotes_child_layouts() {
        let mut trie = MarfTrie::default();
        for index in 0_u8..=48 {
            let mut path = [0; 32];
            path[0] = index;
            trie.insert_path(path, MarfValue::from_u32(u32::from(index)));
        }
        let root_before_overwrite = trie.root_hash();
        let mut overwritten_path = [0; 32];
        overwritten_path[0] = 9;
        trie.insert_path(overwritten_path, MarfValue::from_u32(100));
        let overwritten_root = trie.root_hash();
        assert_ne!(overwritten_root, root_before_overwrite);
        trie.insert_path(overwritten_path, MarfValue::from_u32(100));
        assert_eq!(trie.root_hash(), overwritten_root);
        assert_eq!(
            trie.get_path(overwritten_path),
            Some(MarfValue::from_u32(100))
        );
        assert_eq!(trie.get_path([0xff; 32]), None);
    }

    #[test]
    fn copied_root_uses_block_ids_for_unchanged_children() {
        let first = [1; 32];
        let path = [7; 32];
        let first_value = MarfValue::from_u32(1);
        let mut trie = MarfTrie::default();
        trie.insert_path(path, first_value);
        trie.prepare_root_for_copy(first);
        let mut pointers = vec![
            TriePointer {
                id: 0,
                character: 0,
                referenced_block: None,
            };
            256
        ];
        pointers[usize::from(path[0])] = TriePointer {
            id: TrieNodeId::Leaf as u8 | 0x80,
            character: path[0],
            referenced_block: Some(first),
        };
        let mut child_hashes = vec![TrieHash::EMPTY; 256];
        child_hashes[usize::from(path[0])] = TrieHash::from_bytes(first);
        let content = internal_node_hash(TrieNodeId::Node256, &pointers, &[], &child_hashes)
            .expect("hashes copied root");
        assert_eq!(trie.root_hash(), content);
    }

    #[test]
    fn versioned_trie_records_consensus_height_metadata() {
        let first = [1; 32];
        let second = [2; 32];
        let third = [3; 32];
        let path = [7; 32];
        let first_value = MarfValue::from_u32(1);
        let replacement = MarfValue::from_u32(2);
        let mut trie = VersionedMarf::default();

        trie.begin(None, first).expect("starts genesis state");
        trie.insert_path(path, first_value)
            .expect("writes genesis state");
        let first_root = trie.seal().expect("seals genesis state");

        trie.begin(Some(first), second)
            .expect("extends first state");
        let second_root = trie.seal().expect("seals unchanged state");

        trie.begin(Some(second), third)
            .expect("extends second state");
        trie.insert_path(path, replacement)
            .expect("overwrites copied leaf");
        let third_root = trie.seal().expect("seals updated state");
        assert_eq!(trie.get_path(first, path), Some(first_value));
        assert_eq!(trie.get_path(third, path), Some(replacement));
        assert_eq!(
            trie.get(third, OWN_BLOCK_HEIGHT_KEY.as_bytes()),
            Some(2.into())
        );
        assert_eq!(
            trie.get(third, format!("{BLOCK_HEIGHT_TO_HASH_KEY}::0").as_bytes()),
            Some(MarfValue::from_block_id(first))
        );
        assert_eq!(
            trie.get(
                third,
                format!("{BLOCK_HASH_TO_HEIGHT_KEY}::{}", block_hex(second)).as_bytes()
            ),
            Some(1.into())
        );
        assert_eq!(trie.root(first), Some(first_root));
        assert_eq!(trie.root(second), Some(second_root));
        assert_ne!(third_root, second_root);
    }
}
