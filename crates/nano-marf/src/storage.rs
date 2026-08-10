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

/// Nodes staged during a checkpoint import, before the B-tree is built once.
const STAGING_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS marf_node_staging (
    block INTEGER NOT NULL,
    idx INTEGER NOT NULL,
    hash BLOB NOT NULL,
    data BLOB NOT NULL
);
";

/// How many resident bytes of trie nodes stay cached per generation.
///
/// A byte bound rather than an entry count, because the entries are nothing
/// alike. Near mainnet height 8.7 million, 42% of the nodes recent blocks read
/// are wide internal nodes that decode to ~14 KB — 56 bytes per present child —
/// while the rest are leaves under 200 bytes. The previous bound of 1,000,000
/// *entries* per generation was tuned by block-execution time (0.78 s a block,
/// against 1.8 s at 250,000, because a MARF lookup walks back-pointers into
/// ancestor states and consecutive blocks share that ancestry), but its own
/// sizing note assumed nodes were small: at the tip's real mix it held ~12 GB
/// across the two generations, and the kernel OOM-killed a follower at 18.3 GB
/// after fifteen hours. The working set the cache exists for is precisely the
/// wide ancestor nodes, so the fix is to bound what they cost, not to stop
/// caching them.
const NODE_CACHE_BYTES: usize = 1_500_000_000;
/// Node hashes are 32 bytes each; this keeps the same ~1M entries per
/// generation the entry-count bound did.
const NODE_HASH_CACHE_BYTES: usize = 100_000_000;
/// Block records and id→hash entries are ~100 bytes; these keep the previous
/// 65,536 entries.
const BLOCK_CACHE_BYTES: usize = 12_000_000;
const HASH_CACHE_BYTES: usize = 8_000_000;
const SQLITE_MMAP_BYTES: i64 = 64 * 1024 * 1024 * 1024;
const SQLITE_PAGE_BYTES: i64 = 16 * 1024;

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

/// What a cached value costs to keep resident, beyond the table slot around it.
trait Weight {
    fn weight(&self) -> usize;
}

impl Weight for Arc<TrieNode> {
    fn weight(&self) -> usize {
        size_of::<TrieNode>()
            + match self.as_ref() {
                TrieNode::Leaf { path, .. } => path.capacity(),
                TrieNode::Internal { path, children } => {
                    path.capacity() + children.capacity() * size_of::<TrieChild>()
                }
            }
    }
}

impl Weight for TrieHash {
    fn weight(&self) -> usize {
        size_of::<Self>()
    }
}

impl Weight for BlockRecord {
    fn weight(&self) -> usize {
        size_of::<Self>()
    }
}

impl Weight for MarfBlockId {
    fn weight(&self) -> usize {
        size_of::<Self>()
    }
}

/// The key and hash-table slot around each entry, charged so a generation of
/// tiny values still has a bounded table.
const ENTRY_OVERHEAD: usize = 64;

/// A two-generation cache: the older generation is dropped wholesale once the
/// newer one fills its byte budget, which bounds residency without tracking
/// access order.
#[derive(Debug)]
struct Cache<K, V> {
    hot: HashMap<K, V>,
    cold: HashMap<K, V>,
    hot_bytes: usize,
    capacity: usize,
}

impl<K: Eq + Hash, V: Clone + Weight> Cache<K, V> {
    fn new(capacity: usize) -> Self {
        Self {
            hot: HashMap::new(),
            cold: HashMap::new(),
            hot_bytes: 0,
            capacity,
        }
    }

    fn get(&mut self, key: &K) -> Option<V> {
        if let Some(value) = self.hot.get(key) {
            return Some(value.clone());
        }
        // Moved, not copied: leaving the entry in the cold generation as well
        // held the whole hot working set resident twice.
        let (key, value) = self.cold.remove_entry(key)?;
        self.insert(key, value.clone());
        Some(value)
    }

    fn insert(&mut self, key: K, value: V) {
        if self.hot_bytes >= self.capacity {
            self.cold = std::mem::take(&mut self.hot);
            self.hot_bytes = 0;
        }
        let weight = ENTRY_OVERHEAD + value.weight();
        if self.hot.insert(key, value).is_none() {
            self.hot_bytes += weight;
        }
    }

    fn remove(&mut self, key: &K) {
        if let Some(value) = self.hot.remove(key) {
            self.hot_bytes -= ENTRY_OVERHEAD + value.weight();
        }
        self.cold.remove(key);
    }
}

/// The trie's backing database.
#[derive(Debug)]
pub struct TrieStorage {
    connection: Connection,
    nodes: RefCell<Cache<(u32, u32), Arc<TrieNode>>>,
    /// Node hashes, which are read with the fanout of the trie while sealing.
    node_hashes: RefCell<Cache<(u32, u32), TrieHash>>,
    blocks: RefCell<Cache<MarfBlockId, BlockRecord>>,
    hashes: RefCell<Cache<u32, MarfBlockId>>,
    /// Whether nodes are being staged for a checkpoint import.
    staging: std::cell::Cell<bool>,
}

impl TrieStorage {
    /// Open an ephemeral store, for tests and for states nothing will reopen.
    pub(crate) fn in_memory() -> Result<Self, MarfError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    /// Open, creating if absent, the store held in `path`.
    pub(crate) fn open(path: &Path) -> Result<Self, MarfError> {
        Self::open_with_journal(path, true)
    }

    /// Open a store that is already there, writing nothing to the filesystem.
    ///
    /// Not merely `SQLITE_OPEN_READ_ONLY`: a read-only connection to a WAL
    /// database still creates the `-shm` wal-index beside it, and a command that
    /// says it is reading must leave a state exactly as it found it. `immutable=1`
    /// takes no lock and builds no index, which is sound only because
    /// [`crate::refuse_uncommitted`] has already refused a database whose journal
    /// still holds frames — an immutable read of one of those would answer with
    /// pages nothing committed.
    pub(crate) fn open_existing(path: &Path) -> Result<Self, MarfError> {
        let connection = Connection::open_with_flags(
            crate::immutable_uri(path),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )?;
        connection.pragma_update(None, "mmap_size", SQLITE_MMAP_BYTES)?;
        connection.execute_batch(
            "PRAGMA cache_size = -2000000;
             PRAGMA temp_store = MEMORY;",
        )?;
        Ok(Self::with_caches(connection))
    }

    /// Open a store that is about to be written in one enormous transaction.
    ///
    /// A checkpoint import is a single write of the whole trie graph, and under
    /// WAL that grows a write-ahead log it can never checkpoint until the end —
    /// sixteen gigabytes of it for mainnet, after which every page lookup
    /// searches that log and throughput decays. A mainnet import fell from
    /// 60 MB/s to under 3.
    ///
    /// Journalling is therefore off for the duration, and the state is durable
    /// from the first block executed on top of it — the import's connection is
    /// dropped and the store reopened under WAL before anything executes.
    ///
    /// Which means an import that does not finish cannot roll back: the pages it
    /// wrote stay in the file and read as state. That is what
    /// `UnfinishedImport` is for; nothing may write here outside its mark.
    pub(crate) fn open_for_import(path: &Path) -> Result<Self, MarfError> {
        Self::open_with_journal(path, false)
    }

    fn open_with_journal(path: &Path, journal: bool) -> Result<Self, MarfError> {
        let connection = Connection::open(path)?;
        // Only a database created by this open takes the page size; an existing
        // file keeps its own. Nodes of one block sit adjacent under the
        // `(block, idx)` key, so a 16 KiB page turns four scattered reads into
        // one — measured on a mainnet ancestry walk, and it also rewrote a
        // 23 GB store into 14 GB.
        connection.pragma_update(None, "page_size", SQLITE_PAGE_BYTES)?;
        let mode = if journal { "WAL" } else { "OFF" };
        connection.query_row(&format!("PRAGMA journal_mode = {mode}"), [], |_| Ok(()))?;
        if !journal {
            connection.execute_batch("PRAGMA synchronous = OFF;")?;
        }
        // `marf_node` is a B-tree keyed by (block, idx), and a checkpoint
        // import writes it in trie order rather than key order: it hops
        // between blocks as it follows back-pointers, so every insert lands
        // somewhere else in the tree. Against SQLite's default two megabytes
        // of page cache that is one random read per node, which is where a
        // mainnet import spent its time — 159 MB/s of reads to write 29.
        //
        // `mmap_size`: an ancestry walk is serial pointer-chasing over a
        // multi-gigabyte file, ~80 µs a miss, and through `pread` every hit in
        // the OS page cache still costs a syscall. Mapping the file makes a
        // cached hit a memory access. Needs the raised `SQLITE_MAX_MMAP_SIZE`
        // in `.cargo/config.toml` — under the stock 2 GB ceiling only the
        // file's first two gigabytes would map.
        connection.pragma_update(None, "mmap_size", SQLITE_MMAP_BYTES)?;
        connection.execute_batch(
            "PRAGMA synchronous = NORMAL;
             PRAGMA cache_size = -2000000;
             PRAGMA temp_store = MEMORY;",
        )?;
        Self::from_connection(connection)
    }

    fn from_connection(connection: Connection) -> Result<Self, MarfError> {
        connection.execute_batch(SCHEMA)?;
        Ok(Self::with_caches(connection))
    }

    fn with_caches(connection: Connection) -> Self {
        Self {
            connection,
            nodes: RefCell::new(Cache::new(NODE_CACHE_BYTES)),
            node_hashes: RefCell::new(Cache::new(NODE_HASH_CACHE_BYTES)),
            blocks: RefCell::new(Cache::new(BLOCK_CACHE_BYTES)),
            hashes: RefCell::new(Cache::new(HASH_CACHE_BYTES)),
            staging: std::cell::Cell::new(false),
        }
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

    /// Delete every sealed state above a height, and the trie nodes they own.
    ///
    /// One transaction, so a kill in the middle of *this* leaves the store as it was.
    pub(crate) fn discard_above(&self, height: u32) -> Result<usize, MarfError> {
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let removed = (|| -> Result<usize, MarfError> {
            self.connection.execute(
                "DELETE FROM marf_node WHERE block IN (SELECT id FROM marf_block WHERE height > ?1)",
                params![height],
            )?;
            Ok(self
                .connection
                .execute("DELETE FROM marf_block WHERE height > ?1", params![height])?)
        })();
        match removed {
            Ok(removed) => {
                self.connection.execute_batch("COMMIT")?;
                // The caches address rows that are gone now, and a stale *hash* is
                // worse than a stale node -- it goes straight into a parent's
                // preimage and moves a root.
                self.forget();
                Ok(removed)
            }
            Err(error) => {
                self.connection.execute_batch("ROLLBACK")?;
                Err(error)
            }
        }
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

    /// A node's hash, cached for the same reason the node is.
    ///
    /// Sealing a block hashes every node on every path it touched, and each of
    /// those preimages needs *every sibling's* hash — so this is read with the
    /// fanout of the trie, thousands of times a block, and it was the one read on
    /// the path with no cache at all. A hash is immutable per `(block, index)`
    /// exactly as the node is: a state is addressed by the block that sealed it,
    /// so nothing ever rewrites one.
    pub(crate) fn node_hash(&self, block: u32, index: u32) -> Result<TrieHash, MarfError> {
        if let Some(hash) = self.node_hashes.borrow_mut().get(&(block, index)) {
            return Ok(hash);
        }
        let hash: Option<Vec<u8>> = self
            .connection
            .prepare_cached("SELECT hash FROM marf_node WHERE block = ?1 AND idx = ?2")?
            .query_row(params![block, index], |row| row.get(0))
            .optional()?;
        let hash = hash.ok_or_else(|| missing(&format!("trie node {block}/{index}")))?;
        let hash = TrieHash::from_bytes(block_id(&hash)?);
        self.node_hashes.borrow_mut().insert((block, index), hash);
        Ok(hash)
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
        self.connection
            .prepare_cached(
                "INSERT INTO marf_block (hash, parent, height, root, content, node, jumps) \
             VALUES (?1, ?2, ?3, ?4, ?4, NULL, ?5)",
            )?
            .execute(params![&hash[..], parent, height, &[0u8; 32][..], jumps])?;
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
            .prepare_cached(
                "UPDATE marf_block SET root = ?2, content = ?3, node = ?4 WHERE id = ?1",
            )?
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
        let statement = if self.staging.get() {
            "INSERT INTO marf_node_staging (block, idx, hash, data) VALUES (?1, ?2, ?3, ?4)"
        } else {
            "INSERT OR REPLACE INTO marf_node (block, idx, hash, data) VALUES (?1, ?2, ?3, ?4)"
        };
        self.connection.prepare_cached(statement)?.execute(params![
            block,
            index,
            &hash.as_bytes()[..],
            encode(node)?
        ])?;
        Ok(())
    }

    /// Stage a checkpoint import's nodes instead of writing them in place.
    ///
    /// `marf_node` is `WITHOUT ROWID` keyed by `(block, idx)`, so the table *is*
    /// the B-tree, and an import writes in trie order — hopping between blocks
    /// as it follows back-pointers. Every insert therefore lands somewhere
    /// random in a tree that grows to sixteen gigabytes, and there is no
    /// separate index to defer.
    ///
    /// Staging into a plain rowid table makes every write an append, and the
    /// B-tree is then built once, in order, by `finish_staged_import`.
    ///
    /// Safe because a checkpoint import never reads a node back: it follows the
    /// source's records and tracks what it has already imported in memory.
    pub(crate) fn begin_staged_import(&self) -> Result<(), MarfError> {
        self.connection.execute_batch(STAGING_SCHEMA)?;
        self.staging.set(true);
        Ok(())
    }

    /// Build the node B-tree from the staged rows, in key order, once.
    pub(crate) fn finish_staged_import(&self) -> Result<(), MarfError> {
        self.staging.set(false);
        // Sorting sixteen gigabytes cannot be resident, whatever the rest of
        // the import wants.
        self.connection.execute_batch(
            "PRAGMA temp_store = FILE;
             INSERT OR REPLACE INTO marf_node (block, idx, hash, data)
                 SELECT block, idx, hash, data FROM marf_node_staging
                 ORDER BY block, idx;
             DROP TABLE marf_node_staging;
             PRAGMA temp_store = MEMORY;",
        )?;
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
        *self.nodes.borrow_mut() = Cache::new(NODE_CACHE_BYTES);
        // Not optional: a rolled-back write leaves this addressing rows the
        // database no longer holds, and a stale *hash* is worse than a stale node
        // because it goes straight into a parent's preimage and moves a root.
        *self.node_hashes.borrow_mut() = Cache::new(NODE_HASH_CACHE_BYTES);
        *self.blocks.borrow_mut() = Cache::new(BLOCK_CACHE_BYTES);
        *self.hashes.borrow_mut() = Cache::new(HASH_CACHE_BYTES);
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
                    block: if flags & BACK_POINTER == 0 {
                        block
                    } else {
                        target
                    },
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
                let ChildTarget::Stored { block, index, kind } = child.target else {
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

#[cfg(test)]
mod tests {
    use super::{SQLITE_MMAP_BYTES, SQLITE_PAGE_BYTES, TrieStorage};

    #[test]
    fn a_durable_store_uses_wide_pages_and_maps_large_databases() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let store =
            TrieStorage::open(&directory.path().join("marf.sqlite")).expect("open a durable store");
        let page_bytes: i64 = store
            .connection
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .expect("read the page size");
        let mmap_bytes: i64 = store
            .connection
            .pragma_query_value(None, "mmap_size", |row| row.get(0))
            .expect("read the mmap size");

        assert_eq!(page_bytes, SQLITE_PAGE_BYTES);
        assert_eq!(mmap_bytes, SQLITE_MMAP_BYTES);
    }
}
