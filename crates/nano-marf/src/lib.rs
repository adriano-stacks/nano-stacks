use std::{fmt, path::Path, sync::Arc};

use nano_primitives::{TrieHash, sha512_256};

mod checkpoint;
mod provenance;
mod storage;

pub use checkpoint::{CheckpointError, import_checkpoint, import_checkpoint_into, import_pcs};
pub use provenance::{
    CheckpointAttestation, CheckpointManifest, CheckpointProvenance, UnfinishedImport,
};
use storage::{BlockRecord, TrieStorage};

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

    const fn from_byte(byte: u8) -> Result<Self, MarfError> {
        match byte {
            1 => Ok(Self::Leaf),
            2 => Ok(Self::Node4),
            3 => Ok(Self::Node16),
            4 => Ok(Self::Node48),
            5 => Ok(Self::Node256),
            _ => Err(MarfError::InvalidPointerCount),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TriePointer {
    pub id: u8,
    pub character: u8,
    pub referenced_block: Option<[u8; 32]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MarfError {
    InvalidPath,
    InvalidPointerCount,
    InvalidBackPointer,
    UnknownVersion,
    VersionAlreadyExists,
    WriteInProgress,
    WriteNotBegun,
    HeightOverflow,
    Storage(String),
}

impl fmt::Display for MarfError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath => formatter.write_str("trie path exceeds 32 bytes"),
            Self::InvalidPointerCount => {
                formatter.write_str("trie node has the wrong number of child pointers")
            }
            Self::InvalidBackPointer => {
                formatter.write_str("trie pointer has an invalid referenced block")
            }
            Self::UnknownVersion => formatter.write_str("MARF version does not exist"),
            Self::VersionAlreadyExists => formatter.write_str("MARF version already exists"),
            Self::WriteInProgress => formatter.write_str("MARF version write is already in progress"),
            Self::WriteNotBegun => formatter.write_str("MARF version write has not begun"),
            Self::HeightOverflow => formatter.write_str("MARF version height overflowed"),
            Self::Storage(reason) => write!(formatter, "MARF storage error: {reason}"),
        }
    }
}

impl std::error::Error for MarfError {}

impl From<rusqlite::Error> for MarfError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

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
///
/// `ancestor_roots` holds the roots of the states at back-distances 1, 2, 4, 8,
/// … from the state being sealed, which is the skip list stacks-core walks.
#[must_use]
pub fn state_root(content: TrieHash, ancestor_roots: &[TrieHash]) -> TrieHash {
    if ancestor_roots.is_empty() {
        return content;
    }
    let mut bytes = Vec::with_capacity(32 * (ancestor_roots.len().saturating_add(1)));
    bytes.extend_from_slice(content.as_bytes());
    for root in ancestor_roots {
        bytes.extend_from_slice(root.as_bytes());
    }
    TrieHash::from_bytes(*sha512_256(&bytes).as_bytes())
}

/// The writes one state makes, layered over the durable trie of its ancestors.
///
/// Nothing inherited is copied: an unchanged child stays a reference into the
/// block that owns it, exactly as the MARF's copy-on-write back-pointers
/// express it.
#[derive(Debug)]
pub struct MarfTrie {
    storage: TrieStorage,
    root_children: Vec<TrieChild>,
}

impl Default for MarfTrie {
    fn default() -> Self {
        Self {
            storage: TrieStorage::in_memory().expect("opens in-memory trie storage"),
            root_children: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum TrieNode {
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
pub(crate) struct TrieChild {
    pub character: u8,
    /// The ancestor a back-pointer names in the consensus preimage.
    pub referenced_block: Option<MarfBlockId>,
    pub target: ChildTarget,
}

#[derive(Clone, Debug)]
pub(crate) enum ChildTarget {
    Memory(Arc<TrieNode>),
    Stored {
        block: u32,
        index: u32,
        kind: TrieNodeId,
    },
}

impl TrieChild {
    fn local(character: u8, node: TrieNode) -> Self {
        Self {
            character,
            referenced_block: None,
            target: ChildTarget::Memory(Arc::new(node)),
        }
    }

    fn node(&self, storage: &TrieStorage) -> Result<Arc<TrieNode>, MarfError> {
        match &self.target {
            ChildTarget::Memory(node) => Ok(Arc::clone(node)),
            ChildTarget::Stored { block, index, .. } => storage.node(*block, *index),
        }
    }

    fn kind(&self) -> TrieNodeId {
        match &self.target {
            ChildTarget::Memory(node) => node.node_id(),
            ChildTarget::Stored { kind, .. } => *kind,
        }
    }

    fn hash(&self, storage: &TrieStorage) -> Result<TrieHash, MarfError> {
        if let Some(block) = self.referenced_block {
            return Ok(TrieHash::from_bytes(block));
        }
        match &self.target {
            ChildTarget::Memory(node) => node.hash(storage),
            ChildTarget::Stored { block, index, .. } => storage.node_hash(*block, *index),
        }
    }

    /// Take ownership of this child for the state being written, turning the
    /// children it inherits into back-pointers to the block that owns them.
    fn owned(&mut self, storage: &TrieStorage) -> Result<&mut Arc<TrieNode>, MarfError> {
        if let ChildTarget::Stored { block, index, .. } = self.target {
            let owner = match self.referenced_block {
                Some(block) => block,
                None => storage.block_hash(block)?,
            };
            let mut node = (*storage.node(block, index)?).clone();
            node.prepare_for_copy(owner);
            self.target = ChildTarget::Memory(Arc::new(node));
            self.referenced_block = None;
        } else if let Some(block) = self.referenced_block.take() {
            let ChildTarget::Memory(node) = &mut self.target else {
                unreachable!("the stored case is handled above");
            };
            Arc::make_mut(node).prepare_for_copy(block);
        }
        let ChildTarget::Memory(node) = &mut self.target else {
            unreachable!("the child was just materialized");
        };
        Ok(node)
    }

    fn insert(
        &mut self,
        storage: &TrieStorage,
        path: &[u8],
        value: MarfValue,
    ) -> Result<(), MarfError> {
        let node = self.owned(storage)?;
        Arc::make_mut(node).insert(storage, path, value)
    }
}

impl MarfTrie {
    pub fn insert(&mut self, key: &[u8], value: MarfValue) {
        self.insert_path(*key_path(key).as_bytes(), value);
    }

    pub fn insert_path(&mut self, path: [u8; 32], value: MarfValue) {
        insert_under_root(&self.storage, &mut self.root_children, path, value)
            .expect("trie storage");
    }

    #[must_use]
    pub fn get(&self, key: &[u8]) -> Option<MarfValue> {
        self.get_path(*key_path(key).as_bytes())
    }

    #[must_use]
    pub fn get_path(&self, path: [u8; 32]) -> Option<MarfValue> {
        find_path(&self.storage, &self.root_children, path).expect("trie storage")
    }

    #[must_use]
    pub fn root_hash(&self) -> TrieHash {
        hash_children(&self.storage, TrieNodeId::Node256, &[], &self.root_children)
            .expect("trie storage")
    }

    /// Return every stored path and value in deterministic path order.
    #[must_use]
    pub fn leaves(&self) -> Vec<(TrieHash, MarfValue)> {
        collect_leaves(&self.storage, &self.root_children).expect("trie storage")
    }

    /// Return the root pointers in their consensus serialization order.
    #[must_use]
    pub fn root_pointers(&self) -> Vec<TriePointer> {
        self.pointers_at(&[])
            .unwrap_or_default()
            .into_iter()
            .map(|(pointer, _)| pointer)
            .collect()
    }

    /// Return the pointers and child hashes of the node reached by a path prefix.
    ///
    /// Two states that hold the same leaves under different roots differ in the
    /// shape of some node, and descending into the child whose hash differs is
    /// how that node is found.
    #[must_use]
    pub fn pointers_at(&self, prefix: &[u8]) -> Option<Vec<(TriePointer, TrieHash)>> {
        pointers_under_root(&self.storage, &self.root_children, prefix).expect("trie storage")
    }
}

/// Take over the sealed state `parent`, turning every child it owns into a
/// back-pointer to it.
fn extend_root(
    storage: &TrieStorage,
    parent: &BlockRecord,
) -> Result<Vec<TrieChild>, MarfError> {
    let mut children = match parent.node {
        Some(index) => match &*storage.node(parent.id, index)? {
            TrieNode::Internal { children, .. } => children.clone(),
            TrieNode::Leaf { .. } => {
                return Err(MarfError::Storage("root state is a leaf".to_owned()));
            }
        },
        None => Vec::new(),
    };
    prepare_root_for_copy(&mut children, parent.hash);
    Ok(children)
}

fn prepare_root_for_copy(children: &mut [TrieChild], block: MarfBlockId) {
    for child in children {
        if child.referenced_block.is_none() {
            child.referenced_block = Some(block);
        }
    }
}

fn insert_under_root(
    storage: &TrieStorage,
    root_children: &mut Vec<TrieChild>,
    path: [u8; 32],
    value: MarfValue,
) -> Result<(), MarfError> {
    let root_character = path[0];
    let suffix = &path[1..];
    let Some(child) = root_children
        .iter_mut()
        .find(|child| child.character == root_character)
    else {
        root_children.push(TrieChild::local(
            root_character,
            TrieNode::Leaf {
                path: suffix.to_vec(),
                value,
            },
        ));
        return Ok(());
    };
    child.insert(storage, suffix, value)
}

/// Write every node a state owns, returning its root node and content hash.
fn persist_root(
    storage: &TrieStorage,
    block: u32,
    root_children: &[TrieChild],
    next: &mut u32,
) -> Result<(u32, TrieHash), MarfError> {
    let (children, hashes) = persist_children(storage, block, root_children, next)?;
    let (pointers, child_hashes) = slots(TrieNodeId::Node256, &children, &hashes);
    let hash = internal_node_hash(TrieNodeId::Node256, &pointers, &[], &child_hashes)?;
    let index = *next;
    *next = next.checked_add(1).ok_or(MarfError::HeightOverflow)?;
    let node = Arc::new(TrieNode::Internal {
        path: Vec::new(),
        children,
    });
    storage.insert_node(block, index, hash, &node)?;
    storage.remember(block, index, node);
    Ok((index, hash))
}

impl TrieNode {
    fn collect_leaves(
        &self,
        storage: &TrieStorage,
        path: &mut Vec<u8>,
        leaves: &mut Vec<(TrieHash, MarfValue)>,
    ) -> Result<(), MarfError> {
        match self {
            Self::Leaf {
                path: suffix,
                value,
            } => {
                path.extend_from_slice(suffix);
                let mut bytes = [0; 32];
                bytes.copy_from_slice(path);
                leaves.push((TrieHash::from_bytes(bytes), *value));
                path.truncate(path.len() - suffix.len());
            }
            Self::Internal {
                path: prefix,
                children,
            } => {
                path.extend_from_slice(prefix);
                for child in children {
                    path.push(child.character);
                    child.node(storage)?.collect_leaves(storage, path, leaves)?;
                    path.pop();
                }
                path.truncate(path.len() - prefix.len());
            }
        }
        Ok(())
    }

    fn get(&self, storage: &TrieStorage, path: &[u8]) -> Result<Option<MarfValue>, MarfError> {
        match self {
            Self::Leaf {
                path: leaf_path,
                value,
            } => Ok((leaf_path == path).then_some(*value)),
            Self::Internal {
                path: node_path,
                children,
            } => {
                let Some(remaining) = path.strip_prefix(node_path.as_slice()) else {
                    return Ok(None);
                };
                let Some((character, remaining)) = remaining.split_first() else {
                    return Ok(None);
                };
                let Some(child) = children.iter().find(|child| child.character == *character)
                else {
                    return Ok(None);
                };
                child.node(storage)?.get(storage, remaining)
            }
        }
    }

    fn insert(
        &mut self,
        storage: &TrieStorage,
        path: &[u8],
        value: MarfValue,
    ) -> Result<(), MarfError> {
        match self {
            Self::Leaf {
                path: leaf_path,
                value: leaf_value,
            } => {
                if leaf_path == path {
                    *leaf_value = value;
                    return Ok(());
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
                Ok(())
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
                    // Splicing a node's compressed path packs the new leaf into
                    // the first slot and the node it displaced into the second,
                    // the opposite of the order a split leaf produces.
                    *self = Self::Internal {
                        path: node_path[..shared].to_vec(),
                        children: vec![
                            TrieChild::local(new_character, new_leaf),
                            TrieChild::local(old_character, old_node),
                        ],
                    };
                    return Ok(());
                }

                let child_character = path[node_path.len()];
                let suffix = &path[node_path.len() + 1..];
                let Some(child) = children
                    .iter_mut()
                    .find(|child| child.character == child_character)
                else {
                    children.push(TrieChild::local(
                        child_character,
                        Self::Leaf {
                            path: suffix.to_vec(),
                            value,
                        },
                    ));
                    return Ok(());
                };
                child.insert(storage, suffix, value)
            }
        }
    }

    fn hash(&self, storage: &TrieStorage) -> Result<TrieHash, MarfError> {
        match self {
            Self::Leaf { path, value } => leaf_hash(path, *value),
            Self::Internal { path, children } => {
                hash_children(storage, node_id_for_children(children.len()), path, children)
            }
        }
    }

    const fn node_id(&self) -> TrieNodeId {
        match self {
            Self::Leaf { .. } => TrieNodeId::Leaf,
            Self::Internal { children, .. } => node_id_for_children(children.len()),
        }
    }

    /// Descend a path prefix and return the node it reaches.
    fn pointers_at(
        &self,
        storage: &TrieStorage,
        prefix: &[u8],
    ) -> Result<Option<Vec<(TriePointer, TrieHash)>>, MarfError> {
        let Self::Internal { path, children } = self else {
            return Ok(None);
        };
        let Some(rest) = prefix.strip_prefix(path.as_slice()) else {
            return Ok(None);
        };
        let Some((character, rest)) = rest.split_first() else {
            return pointers_and_hashes(storage, node_id_for_children(children.len()), children)
                .map(Some);
        };
        let Some(child) = children.iter().find(|child| child.character == *character) else {
            return Ok(None);
        };
        child.node(storage)?.pointers_at(storage, rest)
    }

    fn prepare_for_copy(&mut self, block: MarfBlockId) {
        if let Self::Internal { children, .. } = self {
            for child in children {
                if child.referenced_block.is_none() {
                    child.referenced_block = Some(block);
                }
            }
        }
    }
}

fn find_path(
    storage: &TrieStorage,
    root_children: &[TrieChild],
    path: [u8; 32],
) -> Result<Option<MarfValue>, MarfError> {
    let Some(child) = root_children.iter().find(|child| child.character == path[0]) else {
        return Ok(None);
    };
    child.node(storage)?.get(storage, &path[1..])
}

fn collect_leaves(
    storage: &TrieStorage,
    root_children: &[TrieChild],
) -> Result<Vec<(TrieHash, MarfValue)>, MarfError> {
    let mut leaves = Vec::new();
    let mut path = Vec::with_capacity(32);
    for child in root_children {
        path.push(child.character);
        child
            .node(storage)?
            .collect_leaves(storage, &mut path, &mut leaves)?;
        path.pop();
    }
    leaves.sort_unstable_by_key(|(path, _)| *path);
    Ok(leaves)
}

fn pointers_under_root(
    storage: &TrieStorage,
    root_children: &[TrieChild],
    prefix: &[u8],
) -> Result<Option<Vec<(TriePointer, TrieHash)>>, MarfError> {
    let Some((character, rest)) = prefix.split_first() else {
        return pointers_and_hashes(storage, TrieNodeId::Node256, root_children).map(Some);
    };
    let Some(child) = root_children
        .iter()
        .find(|child| child.character == *character)
    else {
        return Ok(None);
    };
    child.node(storage)?.pointers_at(storage, rest)
}

fn hash_children(
    storage: &TrieStorage,
    id: TrieNodeId,
    path: &[u8],
    children: &[TrieChild],
) -> Result<TrieHash, MarfError> {
    let (pointers, hashes): (Vec<_>, Vec<_>) = pointers_and_hashes(storage, id, children)?
        .into_iter()
        .unzip();
    internal_node_hash(id, &pointers, path, &hashes)
}

fn pointers_and_hashes(
    storage: &TrieStorage,
    id: TrieNodeId,
    children: &[TrieChild],
) -> Result<Vec<(TriePointer, TrieHash)>, MarfError> {
    let hashes = children
        .iter()
        .map(|child| child.hash(storage))
        .collect::<Result<Vec<_>, _>>()?;
    let (pointers, hashes) = slots(id, children, &hashes);
    Ok(pointers.into_iter().zip(hashes).collect())
}

/// One node's pointer slots and the hash each slot contributes.
///
/// Node256 indexes its slots by character; the smaller layouts pack theirs in
/// insertion order, which is part of the consensus preimage.
fn slots(
    id: TrieNodeId,
    children: &[TrieChild],
    hashes: &[TrieHash],
) -> (Vec<TriePointer>, Vec<TrieHash>) {
    let mut pointers = vec![
        TriePointer {
            id: 0,
            character: 0,
            referenced_block: None,
        };
        id.pointer_count()
    ];
    let mut slot_hashes = vec![TrieHash::EMPTY; id.pointer_count()];
    for ((index, child), hash) in children.iter().enumerate().zip(hashes) {
        let index = if id == TrieNodeId::Node256 {
            usize::from(child.character)
        } else {
            index
        };
        pointers[index] = TriePointer {
            id: child.kind() as u8 | u8::from(child.referenced_block.is_some()) << 7,
            character: child.character,
            referenced_block: child.referenced_block,
        };
        slot_hashes[index] = *hash;
    }
    (pointers, slot_hashes)
}

fn persist_children(
    storage: &TrieStorage,
    block: u32,
    children: &[TrieChild],
    next: &mut u32,
) -> Result<(Vec<TrieChild>, Vec<TrieHash>), MarfError> {
    let mut persisted = Vec::with_capacity(children.len());
    let mut hashes = Vec::with_capacity(children.len());
    for child in children {
        match &child.target {
            ChildTarget::Memory(node) => {
                if child.referenced_block.is_some() {
                    return Err(MarfError::InvalidBackPointer);
                }
                let (index, hash, kind) = persist_node(storage, block, node, next)?;
                persisted.push(TrieChild {
                    character: child.character,
                    referenced_block: None,
                    target: ChildTarget::Stored { block, index, kind },
                });
                hashes.push(hash);
            }
            ChildTarget::Stored {
                block: owner,
                index,
                ..
            } => {
                hashes.push(match child.referenced_block {
                    Some(referenced) => TrieHash::from_bytes(referenced),
                    None => storage.node_hash(*owner, *index)?,
                });
                persisted.push(child.clone());
            }
        }
    }
    Ok((persisted, hashes))
}

fn persist_node(
    storage: &TrieStorage,
    block: u32,
    node: &Arc<TrieNode>,
    next: &mut u32,
) -> Result<(u32, TrieHash, TrieNodeId), MarfError> {
    let (hash, record, id) = match &**node {
        TrieNode::Leaf { path, value } => (
            leaf_hash(path, *value)?,
            Arc::clone(node),
            TrieNodeId::Leaf,
        ),
        TrieNode::Internal { path, children } => {
            let (children, hashes) = persist_children(storage, block, children, next)?;
            let id = node_id_for_children(children.len());
            let (pointers, child_hashes) = slots(id, &children, &hashes);
            (
                internal_node_hash(id, &pointers, path, &child_hashes)?,
                Arc::new(TrieNode::Internal {
                    path: path.clone(),
                    children,
                }),
                id,
            )
        }
    };
    let index = *next;
    *next = next.checked_add(1).ok_or(MarfError::HeightOverflow)?;
    storage.insert_node(block, index, hash, &record)?;
    storage.remember(block, index, record);
    Ok((index, hash, id))
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

/// A copy-on-write MARF keyed by block identifier, held on disk.
#[derive(Debug)]
pub struct VersionedMarf {
    storage: TrieStorage,
    active: Option<ActiveVersion>,
}

impl Default for VersionedMarf {
    fn default() -> Self {
        Self::from_storage(TrieStorage::in_memory().expect("opens in-memory trie storage"))
    }
}

#[derive(Clone, Debug)]
struct ActiveVersion {
    block: MarfBlockId,
    parent: Option<MarfBlockId>,
    height: u32,
    root_children: Vec<TrieChild>,
}

/// The unsealed state, kept so a failed Clarity transaction can put it back.
///
/// Copying it shares every node it did not itself write, so a snapshot costs
/// the block's root pointers and nothing more.
#[derive(Clone, Debug)]
pub struct MarfSnapshot(Option<ActiveVersion>);

impl VersionedMarf {
    /// Open, creating if absent, the MARF held in `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MarfError> {
        Ok(Self::from_storage(TrieStorage::open(path.as_ref())?))
    }

    pub(crate) const fn from_storage(storage: TrieStorage) -> Self {
        Self {
            storage,
            active: None,
        }
    }

    /// Copy the unsealed state so it can be restored after a rollback.
    #[must_use]
    pub fn snapshot(&self) -> MarfSnapshot {
        MarfSnapshot(self.active.clone())
    }

    /// Put back the unsealed state a snapshot captured.
    pub fn restore(&mut self, snapshot: MarfSnapshot) {
        self.active = snapshot.0;
    }

    /// The deepest sealed state, which is where a reopened MARF resumes.
    #[must_use]
    pub fn tip(&self) -> Option<MarfBlockId> {
        self.storage.tip().expect("trie storage")
    }

    /// Whether a sealed state exists.
    #[must_use]
    pub fn contains(&self, block: MarfBlockId) -> bool {
        self.storage
            .block(block)
            .expect("trie storage")
            .is_some()
    }

    /// The state currently being written.
    #[must_use]
    pub fn active_block(&self) -> Option<MarfBlockId> {
        self.active.as_ref().map(|active| active.block)
    }

    /// The height of the state currently being written.
    ///
    /// The height keys `begin` wrote are derived from it, so a recorder that
    /// computed one of its own could record keys the trie does not hold.
    #[must_use]
    pub fn active_height(&self) -> Option<u32> {
        self.active.as_ref().map(|active| active.height)
    }

    /// Start a new state from an existing parent, or from an empty genesis state.
    pub fn begin(
        &mut self,
        parent: Option<MarfBlockId>,
        block: MarfBlockId,
    ) -> Result<(), MarfError> {
        if self.active.is_some() {
            return Err(MarfError::WriteInProgress);
        }
        if self.storage.block(block)?.is_some() {
            return Err(MarfError::VersionAlreadyExists);
        }

        let (mut root_children, height) = match parent {
            Some(parent) => {
                let record = self
                    .storage
                    .block(parent)?
                    .ok_or(MarfError::UnknownVersion)?;
                (
                    extend_root(&self.storage, &record)?,
                    record.height.checked_add(1).ok_or(MarfError::HeightOverflow)?,
                )
            }
            None => (Vec::new(), 0),
        };
        insert_metadata(&self.storage, &mut root_children, parent, block, height)?;
        self.active = Some(ActiveVersion {
            block,
            parent,
            height,
            root_children,
        });
        Ok(())
    }

    /// Write a raw path into the active state.
    pub fn insert_path(&mut self, path: [u8; 32], value: MarfValue) -> Result<(), MarfError> {
        let active = self.active.as_mut().ok_or(MarfError::WriteNotBegun)?;
        insert_under_root(&self.storage, &mut active.root_children, path, value)
    }

    /// Write a logical key into the active state.
    pub fn insert(&mut self, key: &[u8], value: MarfValue) -> Result<(), MarfError> {
        self.insert_path(*key_path(key).as_bytes(), value)
    }

    /// Seal the active state and return its history-dependent root.
    pub fn seal(&mut self) -> Result<TrieHash, MarfError> {
        let block = self.active.as_ref().ok_or(MarfError::WriteNotBegun)?.block;
        self.seal_to(block)
    }

    /// Return the root that would be produced by sealing the active state.
    /// The character and hash of every child of the root being written.
    ///
    /// A state root that differs while every value agrees means the root node's
    /// children differ, and which one is a fact to be read rather than guessed:
    /// the network's own merkle proofs carry the same hashes, so comparing them
    /// names the first byte of the path that is wrong.
    pub fn pending_root_children(&self) -> Result<Vec<(u8, TrieHash)>, MarfError> {
        let active = self.active.as_ref().ok_or(MarfError::WriteNotBegun)?;
        active
            .root_children
            .iter()
            .map(|child| Ok((child.character, child.hash(&self.storage)?)))
            .collect()
    }

    pub fn pending_root(&self) -> Result<TrieHash, MarfError> {
        let active = self.active.as_ref().ok_or(MarfError::WriteNotBegun)?;
        Ok(state_root(
            hash_children(&self.storage, TrieNodeId::Node256, &[], &active.root_children)?,
            &self.ancestor_roots(active.parent, active.height)?,
        ))
    }

    /// Seal the active state while registering it under its committed block ID.
    ///
    /// Block execution uses a stable temporary ID so the MARF's height keys do
    /// not depend on a header that includes the state root being calculated.
    pub fn seal_to(&mut self, block: MarfBlockId) -> Result<TrieHash, MarfError> {
        if self.storage.block(block)?.is_some() {
            return Err(MarfError::VersionAlreadyExists);
        }
        let active = self.active.as_ref().ok_or(MarfError::WriteNotBegun)?;
        let root = match self.write_sealed(block, active) {
            Ok(root) => root,
            // A half-written state leaves the node cache addressing rows the
            // rollback took away, so nothing cached may outlive the failure.
            Err(error) => {
                self.storage.forget();
                return Err(error);
            }
        };
        self.active = None;
        Ok(root)
    }

    fn write_sealed(
        &self,
        block: MarfBlockId,
        active: &ActiveVersion,
    ) -> Result<TrieHash, MarfError> {
        let parent = active
            .parent
            .and_then(|parent| self.record(parent))
            .map(|record| record.id);
        let jumps = self.jumps(active.parent, active.height)?;
        let ancestor_roots = self.ancestor_roots_for(&jumps)?;

        let transaction = self.storage.transaction()?;
        let id = self
            .storage
            .reserve_block(block, parent, active.height, &jumps)?;
        let mut next = 0;
        let (node, content) = persist_root(&self.storage, id, &active.root_children, &mut next)?;
        let root = state_root(content, &ancestor_roots);
        self.storage
            .complete_block(block, id, root, content, Some(node))?;
        transaction.commit()?;
        Ok(root)
    }

    /// Discard the unsealed state currently being written.
    pub fn abort(&mut self) -> Result<(), MarfError> {
        self.active.take().ok_or(MarfError::WriteNotBegun)?;
        Ok(())
    }

    /// Read a logical key from a sealed state.
    #[must_use]
    pub fn get(&self, block: MarfBlockId, key: &[u8]) -> Option<MarfValue> {
        self.get_path(block, *key_path(key).as_bytes())
    }

    /// Read a raw path from a sealed state.
    #[must_use]
    pub fn get_path(&self, block: MarfBlockId, path: [u8; 32]) -> Option<MarfValue> {
        self.sealed_root(block)
            .expect("trie storage")
            .and_then(|root| root.get(&self.storage, &path).expect("trie storage"))
    }

    /// Read a logical key from the state being written.
    #[must_use]
    pub fn get_active(&self, key: &[u8]) -> Option<MarfValue> {
        self.get_active_path(*key_path(key).as_bytes())
    }

    /// Read a raw path from the state being written.
    #[must_use]
    pub fn get_active_path(&self, path: [u8; 32]) -> Option<MarfValue> {
        let active = self.active.as_ref()?;
        find_path(&self.storage, &active.root_children, path).expect("trie storage")
    }

    /// Return a sealed state root.
    #[must_use]
    pub fn root(&self, block: MarfBlockId) -> Option<TrieHash> {
        self.record(block).map(|record| record.root)
    }

    /// Return the content hash before ancestry is incorporated into the state root.
    #[must_use]
    pub fn content_root(&self, block: MarfBlockId) -> Option<TrieHash> {
        self.record(block).map(|record| record.content)
    }

    /// Return all leaves stored for a sealed state.
    #[must_use]
    pub fn leaves(&self, block: MarfBlockId) -> Option<Vec<(TrieHash, MarfValue)>> {
        let root = self.sealed_root(block).expect("trie storage")?;
        let TrieNode::Internal { children, .. } = &*root else {
            return None;
        };
        Some(collect_leaves(&self.storage, children).expect("trie storage"))
    }

    /// Return the root pointers for a sealed state.
    #[must_use]
    pub fn root_pointers(&self, block: MarfBlockId) -> Option<Vec<TriePointer>> {
        Some(
            self.pointers_at(block, &[])?
                .into_iter()
                .map(|(pointer, _)| pointer)
                .collect(),
        )
    }

    /// Return the pointers and child hashes a sealed state holds under a prefix.
    #[must_use]
    pub fn pointers_at(
        &self,
        block: MarfBlockId,
        prefix: &[u8],
    ) -> Option<Vec<(TriePointer, TrieHash)>> {
        let root = self.sealed_root(block).expect("trie storage")?;
        let TrieNode::Internal { children, .. } = &*root else {
            return None;
        };
        pointers_under_root(&self.storage, children, prefix).expect("trie storage")
    }

    /// Return a sealed state's parent block, if the state exists.
    #[must_use]
    pub fn parent(&self, block: MarfBlockId) -> Option<Option<MarfBlockId>> {
        let record = self.record(block)?;
        Some(
            record
                .parent
                .map(|parent| self.storage.block_hash(parent).expect("trie storage")),
        )
    }

    /// Return a sealed state's height.
    #[must_use]
    pub fn height(&self, block: MarfBlockId) -> Option<u32> {
        self.record(block).map(|record| record.height)
    }

    /// Find an ancestor at `height` from a sealed state.
    ///
    /// The walk descends the state's power-of-two ancestor table, so it costs a
    /// logarithmic number of hops rather than one per intervening block.
    #[must_use]
    pub fn block_at_height(&self, block: MarfBlockId, height: u32) -> Option<MarfBlockId> {
        let mut record = self.record(block)?;
        while record.height > height {
            let distance = record.height - height;
            let jumps = self.storage.jumps(record.hash).expect("trie storage");
            let step = jumps.get(distance.ilog2() as usize)?;
            record = self.record(*step)?;
        }
        (record.height == height).then_some(record.hash)
    }

    fn record(&self, block: MarfBlockId) -> Option<BlockRecord> {
        self.storage.block(block).expect("trie storage")
    }

    fn sealed_root(&self, block: MarfBlockId) -> Result<Option<Arc<TrieNode>>, MarfError> {
        let Some(record) = self.storage.block(block)? else {
            return Ok(None);
        };
        let Some(index) = record.node else {
            return Ok(None);
        };
        self.storage.node(record.id, index).map(Some)
    }

    /// The ancestors at back-distances 1, 2, 4, … from a state about to be sealed.
    fn jumps(
        &self,
        parent: Option<MarfBlockId>,
        height: u32,
    ) -> Result<Vec<MarfBlockId>, MarfError> {
        let Some(parent) = parent else {
            return Ok(Vec::new());
        };
        let mut jumps = vec![parent];
        let mut step = 1_usize;
        while (1_u64 << step) <= u64::from(height) {
            let previous = jumps[step - 1];
            let ancestor = *self
                .storage
                .jumps(previous)?
                .get(step - 1)
                .ok_or(MarfError::UnknownVersion)?;
            jumps.push(ancestor);
            step += 1;
        }
        Ok(jumps)
    }

    fn ancestor_roots(
        &self,
        parent: Option<MarfBlockId>,
        height: u32,
    ) -> Result<Vec<TrieHash>, MarfError> {
        let jumps = self.jumps(parent, height)?;
        self.ancestor_roots_for(&jumps)
    }

    fn ancestor_roots_for(&self, jumps: &[MarfBlockId]) -> Result<Vec<TrieHash>, MarfError> {
        jumps
            .iter()
            .map(|block| {
                self.storage
                    .block(*block)?
                    .map(|record| record.root)
                    .ok_or(MarfError::UnknownVersion)
            })
            .collect()
    }
}

const BLOCK_HASH_TO_HEIGHT_KEY: &str = "__MARF_BLOCK_HASH_TO_HEIGHT";
const BLOCK_HEIGHT_TO_HASH_KEY: &str = "__MARF_BLOCK_HEIGHT_TO_HASH";
const OWN_BLOCK_HEIGHT_KEY: &str = "__MARF_BLOCK_HEIGHT_SELF";

/// The MARF's own height keys for a state, in the order `begin` writes them.
///
/// Ordinary leaves, and part of the root — so their order is consensus like any
/// other write's, and a journal that recorded a block's writes without them
/// would seal a different trie. Public for exactly that reason: the recorder
/// asks the MARF what it wrote rather than restating the rule beside it.
#[must_use]
pub fn height_keys(
    parent: Option<MarfBlockId>,
    block: MarfBlockId,
    height: u32,
) -> Vec<(String, MarfValue)> {
    let mut entries = vec![
        (
            OWN_BLOCK_HEIGHT_KEY.to_owned(),
            MarfValue::from_u32(height),
        ),
        (
            format!("{BLOCK_HEIGHT_TO_HASH_KEY}::{height}"),
            MarfValue::from_block_id(block),
        ),
        (
            format!("{BLOCK_HASH_TO_HEIGHT_KEY}::{}", block_hex(block)),
            MarfValue::from_u32(height),
        ),
    ];
    if let Some(parent) = parent {
        let previous_height = height
            .checked_sub(1)
            .expect("parent implies non-genesis height");
        entries.push((
            format!("{BLOCK_HEIGHT_TO_HASH_KEY}::{previous_height}"),
            MarfValue::from_block_id(parent),
        ));
        entries.push((
            format!("{BLOCK_HASH_TO_HEIGHT_KEY}::{}", block_hex(parent)),
            MarfValue::from_u32(previous_height),
        ));
    }
    entries
}

fn insert_metadata(
    storage: &TrieStorage,
    root_children: &mut Vec<TrieChild>,
    parent: Option<MarfBlockId>,
    block: MarfBlockId,
    height: u32,
) -> Result<(), MarfError> {
    for (key, value) in height_keys(parent, block, height) {
        insert_under_root(
            storage,
            root_children,
            *key_path(key.as_bytes()).as_bytes(),
            value,
        )?;
    }
    Ok(())
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
    fn state_root_folds_the_power_of_two_ancestors_it_is_given() {
        let content = key_path(b"content");
        let ancestors = [key_path(b"one"), key_path(b"two"), key_path(b"four")];
        let root = state_root(content, &ancestors);
        let expected = nano_primitives::sha512_256(
            &[
                content.as_bytes().as_slice(),
                ancestors[0].as_bytes().as_slice(),
                ancestors[1].as_bytes().as_slice(),
                ancestors[2].as_bytes().as_slice(),
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
        super::prepare_root_for_copy(&mut trie.root_children, first);
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
        assert_eq!(trie.tip(), Some(third));
        assert_eq!(trie.block_at_height(third, 0), Some(first));
        assert_eq!(trie.block_at_height(third, 1), Some(second));
    }

    #[test]
    fn abort_discards_an_unsealed_version() {
        let block = [1; 32];
        let replacement = [2; 32];
        let path = [7; 32];
        let mut trie = VersionedMarf::default();

        trie.begin(None, block).expect("starts active state");
        trie.insert_path(path, MarfValue::from_u32(1))
            .expect("writes active state");
        trie.abort().expect("discards active state");

        assert_eq!(trie.root(block), None);
        trie.begin(None, replacement)
            .expect("starts replacement state");
        trie.seal().expect("seals replacement state");
    }

    #[test]
    fn a_reopened_marf_extends_the_state_it_left_on_disk() {
        let directory = std::env::temp_dir().join(format!(
            "nano-marf-reopen-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&directory).expect("create directory");
        let path = directory.join("marf.sqlite");
        let first = [1; 32];
        let second = [2; 32];
        let key = b"durable";

        let expected = {
            let mut marf = VersionedMarf::open(&path).expect("open MARF");
            marf.begin(None, first).expect("begin");
            marf.insert(key, MarfValue::from_value(b"one")).expect("insert");
            marf.seal().expect("seal");
            marf.begin(Some(first), second).expect("begin");
            marf.insert(key, MarfValue::from_value(b"two")).expect("insert");
            marf.seal().expect("seal")
        };

        let reopened = VersionedMarf::open(&path).expect("reopen MARF");
        assert_eq!(reopened.tip(), Some(second));
        assert_eq!(reopened.root(second), Some(expected));
        assert_eq!(
            reopened.get(second, key),
            Some(MarfValue::from_value(b"two"))
        );
        assert_eq!(
            reopened.get(first, key),
            Some(MarfValue::from_value(b"one"))
        );
        std::fs::remove_dir_all(&directory).expect("remove directory");
    }
}
