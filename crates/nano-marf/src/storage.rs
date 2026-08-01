//! Durable storage for trie nodes and block records.
//!
//! A sealed MARF state lives entirely in `SQLite`: its nodes are addressed by
//! `(block, index)` so a back-pointer resolves in one hop, and each block row
//! carries the power-of-two ancestor table the state root folds over.

use std::{cell::RefCell, collections::HashMap, hash::Hash, path::Path, sync::Arc};

use nano_primitives::TrieHash;
use rusqlite::{Connection, OptionalExtension, params};

use crate::{ChildTarget, MarfBlockId, MarfError, MarfValue, TrieChild, TrieNode, TrieNodeId};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS marf_block (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    hash BLOB NOT NULL UNIQUE,
    parent INTEGER,
    height INTEGER NOT NULL,
    root BLOB NOT NULL,
    content BLOB NOT NULL,
    node INTEGER,
    jumps BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS marf_block_height ON marf_block(height);
CREATE TABLE IF NOT EXISTS marf_node (
    block INTEGER NOT NULL,
    idx INTEGER NOT NULL,
    hash BLOB NOT NULL,
    data BLOB NOT NULL,
    PRIMARY KEY (block, idx)
) WITHOUT ROWID;
";

/// How many trie nodes and block records stay resident per cache generation.
const NODE_CACHE: usize = 20_000;
const BLOCK_CACHE: usize = 4_096;

const LEAF_RECORD: u8 = 0;
const INTERNAL_RECORD: u8 = 1;
const BACK_POINTER: u8 = 0x80;

/// One sealed MARF state, without the nodes it owns.
#[derive(Clone, Copy, Debug)]
pub struct BlockRecord {
    pub id: u32,
    pub hash: MarfBlockId,
    pub parent: Option<u32>,
    pub height: u32,
    pub root: TrieHash,
    pub content: TrieHash,
    /// The index of this state's root node, absent for a state imported only
    /// as an ancestor of a checkpoint.
    pub node: Option<u32>,
}

/// A two-generation cache: the older generation is dropped wholesale once the
/// newer one fills, which bounds residency without tracking access order.
#[derive(Debug)]
struct Cache<K, V> {
    hot: HashMap<K, V>,
    cold: HashMap<K, V>,
    capacity: usize,
}

impl<K: Clone + Eq + Hash, V: Clone> Cache<K, V> {
    fn new(capacity: usize) -> Self {
        Self {
            hot: HashMap::new(),
            cold: HashMap::new(),
            capacity,
        }
    }

    fn get(&mut self, key: &K) -> Option<V> {
        if let Some(value) = self.hot.get(key) {
            return Some(value.clone());
        }
        let value = self.cold.get(key)?.clone();
        self.insert(key.clone(), value.clone());
        Some(value)
    }

    fn insert(&mut self, key: K, value: V) {
        if self.hot.len() >= self.capacity {
            self.cold = std::mem::take(&mut self.hot);
        }
        self.hot.insert(key, value);
    }

    fn remove(&mut self, key: &K) {
        self.hot.remove(key);
        self.cold.remove(key);
    }
}

/// The trie's backing database.
#[derive(Debug)]
pub struct TrieStorage {
    connection: Connection,
    nodes: RefCell<Cache<(u32, u32), Arc<TrieNode>>>,
    blocks: RefCell<Cache<MarfBlockId, BlockRecord>>,
    hashes: RefCell<Cache<u32, MarfBlockId>>,
}

impl TrieStorage {
    /// Open an ephemeral store, for tests and for states nothing will reopen.
    pub(crate) fn in_memory() -> Result<Self, MarfError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    /// Open, creating if absent, the store held in `path`.
    pub(crate) fn open(path: &Path) -> Result<Self, MarfError> {
        let connection = Connection::open(path)?;
        connection.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        // `marf_node` is a B-tree keyed by (block, idx), and a checkpoint
        // import writes it in trie order rather than key order: it hops
        // between blocks as it follows back-pointers, so every insert lands
        // somewhere else in the tree. Against SQLite's default two megabytes
        // of page cache that is one random read per node, which is where a
        // mainnet import spent its time — 159 MB/s of reads to write 29.
        connection.execute_batch(
            "PRAGMA synchronous = NORMAL;
             PRAGMA cache_size = -2000000;
             PRAGMA temp_store = MEMORY;",
        )?;
        Self::from_connection(connection)
    }

    fn from_connection(connection: Connection) -> Result<Self, MarfError> {
        connection.execute_batch(SCHEMA)?;
        Ok(Self {
            connection,
            nodes: RefCell::new(Cache::new(NODE_CACHE)),
            blocks: RefCell::new(Cache::new(BLOCK_CACHE)),
            hashes: RefCell::new(Cache::new(BLOCK_CACHE)),
        })
    }

    /// The sealed state with the greatest height, which is where a reopened
    /// store resumes.
    pub(crate) fn tip(&self) -> Result<Option<MarfBlockId>, MarfError> {
        let hash: Option<Vec<u8>> = self
            .connection
            .prepare_cached("SELECT hash FROM marf_block ORDER BY height DESC, id DESC LIMIT 1")?
            .query_row([], |row| row.get(0))
            .optional()?;
        hash.map(|hash| block_id(&hash)).transpose()
    }

    pub(crate) fn block(&self, hash: MarfBlockId) -> Result<Option<BlockRecord>, MarfError> {
        if let Some(record) = self.blocks.borrow_mut().get(&hash) {
            return Ok(Some(record));
        }
        let record = self
            .connection
            .prepare_cached(
                "SELECT id, parent, height, root, content, node FROM marf_block WHERE hash = ?1",
            )?
            .query_row(params![&hash[..]], |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    row.get::<_, Option<u32>>(1)?,
                    row.get::<_, u32>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, Option<u32>>(5)?,
                ))
            })
            .optional()?;
        let Some((id, parent, height, root, content, node)) = record else {
            return Ok(None);
        };
        let record = BlockRecord {
            id,
            hash,
            parent,
            height,
            root: TrieHash::from_bytes(block_id(&root)?),
            content: TrieHash::from_bytes(block_id(&content)?),
            node,
        };
        self.blocks.borrow_mut().insert(hash, record);
        self.hashes.borrow_mut().insert(id, hash);
        Ok(Some(record))
    }

    pub(crate) fn block_hash(&self, id: u32) -> Result<MarfBlockId, MarfError> {
        if let Some(hash) = self.hashes.borrow_mut().get(&id) {
            return Ok(hash);
        }
        let hash: Option<Vec<u8>> = self
            .connection
            .prepare_cached("SELECT hash FROM marf_block WHERE id = ?1")?
            .query_row(params![id], |row| row.get(0))
            .optional()?;
        let hash = block_id(&hash.ok_or_else(|| missing(&format!("block {id}")))?)?;
        self.hashes.borrow_mut().insert(id, hash);
        Ok(hash)
    }

    /// The state's ancestors at back-distances 1, 2, 4, 8, …
    pub(crate) fn jumps(&self, hash: MarfBlockId) -> Result<Vec<MarfBlockId>, MarfError> {
        let jumps: Option<Vec<u8>> = self
            .connection
            .prepare_cached("SELECT jumps FROM marf_block WHERE hash = ?1")?
            .query_row(params![&hash[..]], |row| row.get(0))
            .optional()?;
        let jumps = jumps.ok_or_else(|| missing("block"))?;
        jumps.chunks(32).map(block_id).collect()
    }

    pub(crate) fn node(&self, block: u32, index: u32) -> Result<Arc<TrieNode>, MarfError> {
        if let Some(node) = self.nodes.borrow_mut().get(&(block, index)) {
            return Ok(node);
        }
        let data: Option<Vec<u8>> = self
            .connection
            .prepare_cached("SELECT data FROM marf_node WHERE block = ?1 AND idx = ?2")?
            .query_row(params![block, index], |row| row.get(0))
            .optional()?;
        let data = data.ok_or_else(|| missing(&format!("trie node {block}/{index}")))?;
        let node = Arc::new(self.decode(block, &data)?);
        self.nodes
            .borrow_mut()
            .insert((block, index), Arc::clone(&node));
        Ok(node)
    }

    pub(crate) fn node_hash(&self, block: u32, index: u32) -> Result<TrieHash, MarfError> {
        let hash: Option<Vec<u8>> = self
            .connection
            .prepare_cached("SELECT hash FROM marf_node WHERE block = ?1 AND idx = ?2")?
            .query_row(params![block, index], |row| row.get(0))
            .optional()?;
        let hash = hash.ok_or_else(|| missing(&format!("trie node {block}/{index}")))?;
        Ok(TrieHash::from_bytes(block_id(&hash)?))
    }

    /// Reserve a state's row before its nodes are written, so the nodes can
    /// carry its identifier.
    pub(crate) fn reserve_block(
        &self,
        hash: MarfBlockId,
        parent: Option<u32>,
        height: u32,
        jumps: &[MarfBlockId],
    ) -> Result<u32, MarfError> {
        let jumps: Vec<u8> = jumps.iter().flatten().copied().collect();
        self.connection.prepare_cached(
            "INSERT INTO marf_block (hash, parent, height, root, content, node, jumps) \
             VALUES (?1, ?2, ?3, ?4, ?4, NULL, ?5)",
        )?.execute(params![&hash[..], parent, height, &[0u8; 32][..], jumps])?;
        u32::try_from(self.connection.last_insert_rowid())
            .map_err(|_| MarfError::Storage("block identifier overflowed".to_owned()))
    }

    /// Complete a reserved state once its nodes and root are known.
    pub(crate) fn complete_block(
        &self,
        hash: MarfBlockId,
        id: u32,
        root: TrieHash,
        content: TrieHash,
        node: Option<u32>,
    ) -> Result<(), MarfError> {
        self.connection
            .prepare_cached("UPDATE marf_block SET root = ?2, content = ?3, node = ?4 WHERE id = ?1")?
            .execute(params![
                id,
                &root.as_bytes()[..],
                &content.as_bytes()[..],
                node
            ])?;
        self.blocks.borrow_mut().remove(&hash);
        Ok(())
    }

    pub(crate) fn insert_node(
        &self,
        block: u32,
        index: u32,
        hash: TrieHash,
        node: &TrieNode,
    ) -> Result<(), MarfError> {
        self.connection
            .prepare_cached("INSERT OR REPLACE INTO marf_node (block, idx, hash, data) VALUES (?1, ?2, ?3, ?4)")?
            .execute(params![block, index, &hash.as_bytes()[..], encode(node)?])?;
        Ok(())
    }

    /// Cache a node the caller has just written, which is where the next block
    /// will read it from.
    pub(crate) fn remember(&self, block: u32, index: u32, node: Arc<TrieNode>) {
        self.nodes.borrow_mut().insert((block, index), node);
    }

    pub(crate) fn transaction(&self) -> Result<rusqlite::Transaction<'_>, MarfError> {
        Ok(self.connection.unchecked_transaction()?)
    }

    /// Drop everything cached, for when a rolled-back write leaves the cache
    /// addressing rows the database no longer holds.
    pub(crate) fn forget(&self) {
        *self.nodes.borrow_mut() = Cache::new(NODE_CACHE);
        *self.blocks.borrow_mut() = Cache::new(BLOCK_CACHE);
        *self.hashes.borrow_mut() = Cache::new(BLOCK_CACHE);
    }

    fn decode(&self, block: u32, bytes: &[u8]) -> Result<TrieNode, MarfError> {
        let mut reader = Reader::new(bytes);
        let kind = reader.byte()?;
        let path = reader.path()?;
        if kind == LEAF_RECORD {
            let value = MarfValue::from_bytes(
                reader
                    .take(40)?
                    .try_into()
                    .map_err(|_| corrupt("leaf value"))?,
            );
            return Ok(TrieNode::Leaf { path, value });
        }
        if kind != INTERNAL_RECORD {
            return Err(corrupt("node record kind"));
        }
        let count = usize::from(u16::from_le_bytes(
            reader
                .take(2)?
                .try_into()
                .map_err(|_| corrupt("child count"))?,
        ));
        let mut children = Vec::with_capacity(count);
        for _ in 0..count {
            let flags = reader.byte()?;
            let character = reader.byte()?;
            let target = u32::from_le_bytes(
                reader
                    .take(4)?
                    .try_into()
                    .map_err(|_| corrupt("child block"))?,
            );
            let index = u32::from_le_bytes(
                reader
                    .take(4)?
                    .try_into()
                    .map_err(|_| corrupt("child index"))?,
            );
            let kind = TrieNodeId::from_byte(flags & !BACK_POINTER)?;
            let referenced_block = if flags & BACK_POINTER == 0 {
                None
            } else {
                Some(self.block_hash(target)?)
            };
            children.push(TrieChild {
                character,
                referenced_block,
                target: ChildTarget::Stored {
                    block: if flags & BACK_POINTER == 0 { block } else { target },
                    index,
                    kind,
                },
            });
        }
        Ok(TrieNode::Internal { path, children })
    }
}

fn encode(node: &TrieNode) -> Result<Vec<u8>, MarfError> {
    match node {
        TrieNode::Leaf { path, value } => {
            let mut bytes = Vec::with_capacity(42 + path.len());
            bytes.push(LEAF_RECORD);
            bytes.push(path_length(path)?);
            bytes.extend_from_slice(path);
            bytes.extend_from_slice(value.as_bytes());
            Ok(bytes)
        }
        TrieNode::Internal { path, children } => {
            let mut bytes = Vec::with_capacity(4 + path.len() + children.len() * 10);
            bytes.push(INTERNAL_RECORD);
            bytes.push(path_length(path)?);
            bytes.extend_from_slice(path);
            let count =
                u16::try_from(children.len()).map_err(|_| corrupt("node has too many children"))?;
            bytes.extend_from_slice(&count.to_le_bytes());
            for child in children {
                let ChildTarget::Stored {
                    block,
                    index,
                    kind,
                } = child.target
                else {
                    return Err(MarfError::Storage(
                        "trie child was not persisted before its parent".to_owned(),
                    ));
                };
                bytes.push(kind as u8 | u8::from(child.referenced_block.is_some()) << 7);
                bytes.push(child.character);
                bytes.extend_from_slice(&block.to_le_bytes());
                bytes.extend_from_slice(&index.to_le_bytes());
            }
            Ok(bytes)
        }
    }
}

fn path_length(path: &[u8]) -> Result<u8, MarfError> {
    u8::try_from(path.len()).map_err(|_| MarfError::InvalidPath)
}

fn block_id(bytes: &[u8]) -> Result<MarfBlockId, MarfError> {
    bytes.try_into().map_err(|_| corrupt("32-byte identifier"))
}

fn corrupt(what: &str) -> MarfError {
    MarfError::Storage(format!("trie storage holds an invalid {what}"))
}

fn missing(what: &str) -> MarfError {
    MarfError::Storage(format!("trie storage is missing {what}"))
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn byte(&mut self) -> Result<u8, MarfError> {
        Ok(self.take(1)?[0])
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], MarfError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| corrupt("node record"))?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| corrupt("node record"))?;
        self.position = end;
        Ok(bytes)
    }

    fn path(&mut self) -> Result<Vec<u8>, MarfError> {
        let length = usize::from(self.byte()?);
        if length > 32 {
            return Err(MarfError::InvalidPath);
        }
        Ok(self.take(length)?.to_vec())
    }
}
