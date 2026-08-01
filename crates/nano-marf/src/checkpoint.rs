use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OptionalExtension as _};
use serde::Deserialize;

use crate::{
    CheckpointManifest, ChildTarget, MarfBlockId, MarfError, MarfValue, TrieChild, TrieHash,
    TrieNode, TrieNodeId, VersionedMarf, internal_node_hash, leaf_hash, node_id_for_children, slots,
    state_root, storage::TrieStorage,
};

const ROOT_OFFSET: usize = 36;
const BACK_POINTER: u8 = 0x80;
const COMPRESSED: u8 = 0x40;
const CONTROL_BITS: u8 = BACK_POINTER | COMPRESSED | 0x20 | 0x10;

/// How much of the blob file one node can occupy. A Node256 with uncompressed
/// pointers is under 3 KiB, so nothing straddles a window of this size.
const WINDOW: usize = 8192;

/// Errors raised while importing a stacks-core MARF checkpoint.
#[derive(Debug)]
pub enum CheckpointError {
    Io(std::io::Error),
    Sql(rusqlite::Error),
    Storage(MarfError),
    InvalidCheckpoint(&'static str),
    InvalidManifest(String),
    MissingBlock(MarfBlockId),
    RootMismatch {
        expected: TrieHash,
        actual: TrieHash,
    },
    DeclaredRootMismatch {
        declared: TrieHash,
        published: TrieHash,
    },
    ProvenanceMismatch {
        recorded: Box<CheckpointManifest>,
        configured: Box<CheckpointManifest>,
    },
    UnsupportedPatch,
}

impl std::fmt::Display for CheckpointError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "checkpoint I/O error: {error}"),
            Self::Sql(error) => write!(formatter, "checkpoint SQLite error: {error}"),
            Self::Storage(error) => write!(formatter, "checkpoint storage error: {error}"),
            Self::InvalidCheckpoint(reason) => write!(formatter, "invalid checkpoint: {reason}"),
            Self::InvalidManifest(reason) => write!(formatter, "invalid PCS manifest: {reason}"),
            Self::MissingBlock(block) => write!(
                formatter,
                "checkpoint references missing block {block:02x?}"
            ),
            Self::RootMismatch { expected, actual } => {
                write!(
                    formatter,
                    "checkpoint root mismatch: expected {expected}, got {actual}"
                )
            }
            Self::DeclaredRootMismatch {
                declared,
                published,
            } => write!(
                formatter,
                "declared checkpoint root {declared} is not the root {published} the checkpoint publishes"
            ),
            Self::ProvenanceMismatch {
                recorded,
                configured,
            } => write!(
                formatter,
                "state directory was imported from checkpoint {:02x?} (root {} at height {}), not {:02x?} (root {} at height {})",
                recorded.source_state_id,
                recorded.state_index_root,
                recorded.stacks_height,
                configured.source_state_id,
                configured.state_index_root,
                configured.stacks_height
            ),
            Self::UnsupportedPatch => {
                formatter.write_str("checkpoint contains unsupported trie patch")
            }
        }
    }
}

impl std::error::Error for CheckpointError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sql(error) => Some(error),
            Self::Storage(error) => Some(error),
            Self::InvalidCheckpoint(_)
            | Self::InvalidManifest(_)
            | Self::MissingBlock(_)
            | Self::RootMismatch { .. }
            | Self::DeclaredRootMismatch { .. }
            | Self::ProvenanceMismatch { .. }
            | Self::UnsupportedPatch => None,
        }
    }
}

#[derive(Deserialize)]
struct PcsManifest {
    snapshot: PcsSnapshot,
    roots: PcsRoots,
}

#[derive(Deserialize)]
struct PcsSnapshot {
    block_hash: String,
}

#[derive(Deserialize)]
struct PcsRoots {
    clarity_archival_marf_root_hash: Option<String>,
}

impl From<std::io::Error> for CheckpointError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for CheckpointError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error)
    }
}

impl From<MarfError> for CheckpointError {
    fn from(error: MarfError) -> Self {
        Self::Storage(error)
    }
}

/// Import a raw stacks-core SQLite/blob MARF checkpoint at `source` into memory.
pub fn import_checkpoint(
    sqlite_path: impl AsRef<Path>,
    source: MarfBlockId,
    expected_root: TrieHash,
) -> Result<VersionedMarf, CheckpointError> {
    import_into(
        TrieStorage::in_memory()?,
        sqlite_path.as_ref(),
        source,
        expected_root,
    )
}

/// Import a checkpoint into the durable MARF held at `marf_path`.
///
/// The trie node graph streams straight from the checkpoint's blob file into
/// storage, so nothing larger than one node is ever resident.
pub fn import_checkpoint_into(
    marf_path: impl AsRef<Path>,
    sqlite_path: impl AsRef<Path>,
    source: MarfBlockId,
    expected_root: TrieHash,
) -> Result<VersionedMarf, CheckpointError> {
    import_into(
        TrieStorage::open(marf_path.as_ref())?,
        sqlite_path.as_ref(),
        source,
        expected_root,
    )
}

fn import_into(
    storage: TrieStorage,
    sqlite_path: &Path,
    source: MarfBlockId,
    expected_root: TrieHash,
) -> Result<VersionedMarf, CheckpointError> {
    if let Some(manifest) = CheckpointManifest::beside(sqlite_path)? {
        manifest.check_declared_root(source, expected_root)?;
    }
    let blobs = Blobs::open(&blob_path(sqlite_path)?)?;
    let records = Records::open(sqlite_path, &blobs)?;
    let source_record = records
        .by_hash(source)?
        .ok_or(CheckpointError::MissingBlock(source))?;
    if source_record.root != expected_root {
        return Err(CheckpointError::RootMismatch {
            expected: expected_root,
            actual: source_record.root,
        });
    }

    let mut chain = vec![source];
    let mut current = source_record;
    while let Some(parent) = records.parent_of(&current)? {
        chain.push(parent);
        current = records
            .by_hash(parent)?
            .ok_or(CheckpointError::MissingBlock(parent))?;
    }
    chain.reverse();

    match import_chain(&storage, &records, &blobs, &chain, expected_root) {
        Ok(()) => Ok(VersionedMarf::from_storage(storage)),
        Err(error) => {
            storage.forget();
            Err(error)
        }
    }
}

fn import_chain(
    storage: &TrieStorage,
    records: &Records<'_>,
    blobs: &Blobs,
    chain: &[MarfBlockId],
    expected_root: TrieHash,
) -> Result<(), CheckpointError> {
    let transaction = storage.transaction()?;
    let empty_root = internal_node_hash(
        TrieNodeId::Node256,
        &vec![
            crate::TriePointer {
                id: 0,
                character: 0,
                referenced_block: None,
            };
            256
        ],
        &[],
        &vec![TrieHash::EMPTY; 256],
    )?;

    let mut identifiers = BTreeMap::new();
    let mut parent = None;
    for (height, block) in chain.iter().enumerate() {
        let jumps = power_of_two_ancestors(chain, height);
        let height = u32::try_from(height)
            .map_err(|_| CheckpointError::InvalidCheckpoint("checkpoint height overflow"))?;
        let id = storage.reserve_block(*block, parent, height, &jumps)?;
        let record = records
            .by_hash(*block)?
            .ok_or(CheckpointError::MissingBlock(*block))?;
        storage.complete_block(*block, id, record.root, empty_root, None)?;
        identifiers.insert(record.block_id, id);
        parent = Some(id);
    }

    let source = *chain.last().ok_or(CheckpointError::InvalidCheckpoint(
        "checkpoint has no source block",
    ))?;
    let source_record = records
        .by_hash(source)?
        .ok_or(CheckpointError::MissingBlock(source))?;
    let mut importer = Importer {
        records,
        blobs,
        storage,
        identifiers,
        next: BTreeMap::new(),
        imported: HashMap::new(),
    };
    let (index, content) = importer.import_root(&source_record)?;

    let ancestors = power_of_two_ancestors(chain, chain.len() - 1);
    let ancestor_roots = ancestors
        .iter()
        .map(|block| {
            records
                .by_hash(*block)?
                .map(|record| record.root)
                .ok_or(CheckpointError::MissingBlock(*block))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let calculated_root = state_root(content, &ancestor_roots);
    if calculated_root != expected_root {
        return Err(CheckpointError::RootMismatch {
            expected: expected_root,
            actual: calculated_root,
        });
    }

    let id = *importer
        .identifiers
        .get(&source_record.block_id)
        .ok_or(CheckpointError::MissingBlock(source))?;
    storage.complete_block(source, id, expected_root, content, Some(index))?;
    transaction.commit()?;
    Ok(())
}

/// The linear chain's ancestors at back-distances 1, 2, 4, … from `height`.
fn power_of_two_ancestors(chain: &[MarfBlockId], height: usize) -> Vec<MarfBlockId> {
    let mut ancestors = Vec::new();
    let mut step = 0;
    while (1_usize << step) <= height {
        ancestors.push(chain[height - (1 << step)]);
        step += 1;
    }
    ancestors
}

/// Import the Clarity MARF from a standard marf-squash PCS directory.
pub fn import_pcs(root: impl AsRef<Path>) -> Result<VersionedMarf, CheckpointError> {
    let pcs_root = root.as_ref();
    let (source, expected_root) = pcs_state(pcs_root)?;
    import_checkpoint(
        pcs_root.join("chainstate/vm/clarity/marf.sqlite"),
        source,
        expected_root,
    )
}

fn pcs_state(pcs_root: &Path) -> Result<(MarfBlockId, TrieHash), CheckpointError> {
    let manifest = fs::read_to_string(pcs_root.join("PCS_manifest.toml"))?;
    let manifest: PcsManifest = toml::from_str(&manifest)
        .map_err(|error| CheckpointError::InvalidManifest(error.to_string()))?;
    let source = parse_hex(&manifest.snapshot.block_hash)?;
    let expected_root = manifest
        .roots
        .clarity_archival_marf_root_hash
        .as_deref()
        .ok_or(CheckpointError::InvalidCheckpoint(
            "PCS manifest has no Clarity archival MARF root",
        ))?;
    Ok((source, TrieHash::from_bytes(parse_hex(expected_root)?)))
}

#[derive(Clone, Copy)]
struct Record {
    block_id: u32,
    offset: usize,
    root: TrieHash,
}

/// The checkpoint's `marf_data` rows, read on demand.
///
/// A mainnet chainstate holds nearly nine million of them. Loading every
/// record — and the two indexes the walk needs beside it — cost more memory
/// than the machine had, and the import was killed part-way. `block_id` is the
/// table's primary key and `block_hash` is unique and indexed, so each lookup
/// is a log-time query rather than a map that has to be built first.
struct Records<'a> {
    connection: Connection,
    blobs: &'a Blobs,
    /// Records already read, because the import asks for the same ones
    /// constantly: every node it decodes belongs to a block, and the blocks a
    /// trie reaches through back-pointers repeat. Reading them lazily is what
    /// keeps the walk small; reading them lazily *and* repeatedly is what made
    /// it slow, at a hundred thousand reads a second to write sixteen
    /// megabytes.
    by_hash: RefCell<HashMap<MarfBlockId, Option<Record>>>,
    by_id: RefCell<HashMap<u32, Option<MarfBlockId>>>,
}

impl<'a> Records<'a> {
    fn open(path: &Path, blobs: &'a Blobs) -> Result<Self, CheckpointError> {
        let uri = format!("file:{}?immutable=1", path.display());
        let connection = Connection::open_with_flags(
            uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )?;
        Ok(Self {
            connection,
            blobs,
            by_hash: RefCell::new(HashMap::new()),
            by_id: RefCell::new(HashMap::new()),
        })
    }

    /// The record a block hash names, if the checkpoint holds its trie.
    fn by_hash(&self, block: MarfBlockId) -> Result<Option<Record>, CheckpointError> {
        if let Some(known) = self.by_hash.borrow().get(&block) {
            return Ok(*known);
        }
        let found = self.read_by_hash(block)?;
        self.by_hash.borrow_mut().insert(block, found);
        Ok(found)
    }

    fn read_by_hash(&self, block: MarfBlockId) -> Result<Option<Record>, CheckpointError> {
        let hash = hex_of(&block);
        let row = self
            .connection
            .query_row(
                "SELECT block_id, external_offset FROM marf_data \
                 WHERE block_hash = ?1 AND unconfirmed = 0 AND external_length > 0",
                [&hash],
                |row| Ok((row.get::<_, u32>(0)?, row.get::<_, u64>(1)?)),
            )
            .optional()?;
        let Some((block_id, offset)) = row else {
            return Ok(None);
        };
        self.record(block_id, offset).map(Some)
    }

    /// The block a `block_id` names, which is how a back-pointer refers to one.
    fn hash_for_id(&self, block_id: u32) -> Result<Option<MarfBlockId>, CheckpointError> {
        if let Some(known) = self.by_id.borrow().get(&block_id) {
            return Ok(*known);
        }
        let found = self.read_hash_for_id(block_id)?;
        self.by_id.borrow_mut().insert(block_id, found);
        Ok(found)
    }

    fn read_hash_for_id(&self, block_id: u32) -> Result<Option<MarfBlockId>, CheckpointError> {
        let row = self
            .connection
            .query_row(
                "SELECT block_hash FROM marf_data \
                 WHERE block_id = ?1 AND unconfirmed = 0 AND external_length > 0",
                [block_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        row.map(|hash| parse_hex(&hash)).transpose()
    }

    fn record(&self, block_id: u32, offset: u64) -> Result<Record, CheckpointError> {
        let offset = usize::try_from(offset).map_err(|_| {
            CheckpointError::InvalidCheckpoint("blob offset exceeds address space")
        })?;
        let header = self.blobs.read(offset, ROOT_OFFSET + 32)?;
        let root = TrieHash::from_bytes(fixed(&header[ROOT_OFFSET..])?);
        Ok(Record {
            block_id,
            offset,
            root,
        })
    }

    /// A block's parent: the one its blob names, or the one before it by id
    /// when that name is not a block this checkpoint carries.
    fn parent_of(&self, record: &Record) -> Result<Option<MarfBlockId>, CheckpointError> {
        let named: MarfBlockId = fixed(&self.blobs.read(record.offset, 32)?)?;
        if self.by_hash(named)?.is_some() {
            return Ok(Some(named));
        }
        self.hash_for_id(record.block_id.saturating_sub(1))
    }
}

fn hex_of(block: &MarfBlockId) -> String {
    use std::fmt::Write as _;
    block.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

fn blob_path(sqlite_path: &Path) -> Result<PathBuf, CheckpointError> {
    let name = sqlite_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(CheckpointError::InvalidCheckpoint(
            "SQLite path has no UTF-8 filename",
        ))?;
    Ok(sqlite_path.with_file_name(format!("{name}.blobs")))
}

/// The checkpoint's blob file, read a node at a time.
struct Blobs {
    file: RefCell<File>,
    length: u64,
}

impl Blobs {
    fn open(path: &Path) -> Result<Self, CheckpointError> {
        let file = File::open(path)?;
        let length = file.metadata()?.len();
        Ok(Self {
            file: RefCell::new(file),
            length,
        })
    }

    fn read(&self, offset: usize, length: usize) -> Result<Vec<u8>, CheckpointError> {
        let end = u64::try_from(offset.saturating_add(length))
            .map_err(|_| CheckpointError::InvalidCheckpoint("blob offset exceeds address space"))?;
        if end > self.length {
            return Err(CheckpointError::InvalidCheckpoint("blob is truncated"));
        }
        let mut bytes = vec![0; length];
        let mut file = self.file.borrow_mut();
        file.seek(SeekFrom::Start(offset as u64))?;
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    /// Read enough bytes from `offset` to hold a node of that kind.
    ///
    /// A pointer says what it points at, so the read can be the size that kind
    /// needs rather than the size the largest kind needs. Most nodes are
    /// leaves or small branches of a hundred-odd bytes, and reading eight
    /// kilobytes for each of them is where a mainnet import spent its time:
    /// 159 MB/s of reads to write 29.
    fn window_for(&self, offset: usize, id: u8) -> Result<Vec<u8>, CheckpointError> {
        let length = match id & !CONTROL_BITS {
            // Leaf, Node4: a path of at most 32 and either a 40-byte value or
            // four ten-byte pointers.
            1 | 2 => 256,
            // Node16.
            3 => 512,
            // Node48 carries a 256-byte index beside its pointers.
            4 => 1024,
            // Node256.
            5 => 4096,
            // A patch names a base node and its differences, and how many
            // there are is not known until it is read.
            _ => WINDOW,
        };
        self.read_upto(offset, length)
    }

    /// Read up to `length` bytes, stopping at the end of the file.
    fn read_upto(&self, offset: usize, length: usize) -> Result<Vec<u8>, CheckpointError> {
        let offset64 = u64::try_from(offset)
            .map_err(|_| CheckpointError::InvalidCheckpoint("blob offset exceeds address space"))?;
        let available = usize::try_from(self.length.saturating_sub(offset64)).unwrap_or(length);
        self.read(offset, length.min(available))
    }

    /// Read enough bytes from `offset` to hold any single node.
    fn window(&self, offset: usize) -> Result<Vec<u8>, CheckpointError> {
        let offset64 = u64::try_from(offset)
            .map_err(|_| CheckpointError::InvalidCheckpoint("blob offset exceeds address space"))?;
        let length = usize::try_from(self.length.saturating_sub(offset64))
            .unwrap_or(WINDOW)
            .min(WINDOW);
        self.read(offset, length)
    }
}

struct Importer<'a> {
    records: &'a Records<'a>,
    blobs: &'a Blobs,
    storage: &'a TrieStorage,
    identifiers: BTreeMap<u32, u32>,
    next: BTreeMap<u32, u32>,
    /// Nodes already imported, by where they live in the checkpoint.
    ///
    /// A back-pointer names a node in an ancestor block, and on a chain with
    /// millions of blocks of copy-on-write the same ancestor node is reached
    /// again and again. Importing it once per path is most of the work: a
    /// mainnet import wrote for hours without finishing. Re-importing is only
    /// wasteful rather than wrong — both copies hash the same — so a miss here
    /// costs time, not correctness.
    imported: HashMap<(u32, u32), (u32, TrieHash, TrieNodeId)>,
}

impl Importer<'_> {
    /// Import a checkpoint's root, which always hashes as a Node256.
    fn import_root(&mut self, record: &Record) -> Result<(u32, TrieHash), CheckpointError> {
        let DecodedNode::Internal { path, pointers, .. } =
            self.decode_node(record.block_id, ROOT_OFFSET, TrieNodeId::Node256 as u8)?
        else {
            return Err(CheckpointError::InvalidCheckpoint(
                "root is not an internal node",
            ));
        };
        if !path.is_empty() {
            return Err(CheckpointError::InvalidCheckpoint(
                "root has a compressed path",
            ));
        }
        let (children, hashes) = self.import_children(record.block_id, &pointers)?;
        let (slot_pointers, slot_hashes) = slots(TrieNodeId::Node256, &children, &hashes);
        let hash = internal_node_hash(TrieNodeId::Node256, &slot_pointers, &[], &slot_hashes)?;
        let index = self.write(record.block_id, hash, &TrieNode::Internal { path, children })?;
        Ok((index, hash))
    }

    fn import_node(
        &mut self,
        block_id: u32,
        pointer: usize,
        expected_id: u8,
    ) -> Result<(u32, TrieHash, TrieNodeId), CheckpointError> {
        let key = (block_id, u32::try_from(pointer).unwrap_or(u32::MAX));
        if let Some(already) = self.imported.get(&key) {
            return Ok(*already);
        }
        let imported = self.import_fresh(block_id, pointer, expected_id)?;
        self.imported.insert(key, imported);
        Ok(imported)
    }

    fn import_fresh(
        &mut self,
        block_id: u32,
        pointer: usize,
        expected_id: u8,
    ) -> Result<(u32, TrieHash, TrieNodeId), CheckpointError> {
        match self.decode_node(block_id, pointer, expected_id)? {
            DecodedNode::Leaf { path, value } => {
                let hash = leaf_hash(&path, value)?;
                let index = self.write(block_id, hash, &TrieNode::Leaf { path, value })?;
                Ok((index, hash, TrieNodeId::Leaf))
            }
            DecodedNode::Internal { path, pointers, .. } => {
                let (children, hashes) = self.import_children(block_id, &pointers)?;
                let id = node_id_for_children(children.len());
                let (slot_pointers, slot_hashes) = slots(id, &children, &hashes);
                let hash = internal_node_hash(id, &slot_pointers, &path, &slot_hashes)?;
                let index = self.write(block_id, hash, &TrieNode::Internal { path, children })?;
                Ok((index, hash, id))
            }
        }
    }

    fn import_children(
        &mut self,
        block_id: u32,
        pointers: &[Pointer],
    ) -> Result<(Vec<TrieChild>, Vec<TrieHash>), CheckpointError> {
        let mut children = Vec::new();
        let mut hashes = Vec::new();
        for pointer in pointers {
            if pointer.id == 0 {
                continue;
            }
            let back_pointer = pointer.id & BACK_POINTER != 0;
            let target_id = if back_pointer {
                pointer.back_block
            } else {
                block_id
            };
            let offset = usize::try_from(pointer.offset).map_err(|_| {
                CheckpointError::InvalidCheckpoint("node pointer exceeds address space")
            })?;
            let (index, hash, kind) = self.import_node(target_id, offset, pointer.id)?;
            let referenced_block = back_pointer
                .then(|| self.block_for_id(target_id))
                .transpose()?;
            hashes.push(referenced_block.map_or(hash, TrieHash::from_bytes));
            children.push(TrieChild {
                character: pointer.character,
                referenced_block,
                target: ChildTarget::Stored {
                    block: self.identifier(target_id)?,
                    index,
                    kind,
                },
            });
        }
        Ok((children, hashes))
    }

    fn write(
        &mut self,
        block_id: u32,
        hash: TrieHash,
        node: &TrieNode,
    ) -> Result<u32, CheckpointError> {
        let block = self.identifier(block_id)?;
        let index = self.next.entry(block).or_default();
        let assigned = *index;
        *index = index
            .checked_add(1)
            .ok_or(CheckpointError::InvalidCheckpoint("too many trie nodes"))?;
        self.storage.insert_node(block, assigned, hash, node)?;
        Ok(assigned)
    }

    fn identifier(&self, block_id: u32) -> Result<u32, CheckpointError> {
        self.identifiers.get(&block_id).copied().ok_or(
            CheckpointError::InvalidCheckpoint("node points to unknown block ID"),
        )
    }

    fn decode_node(
        &self,
        block_id: u32,
        pointer: usize,
        expected_id: u8,
    ) -> Result<DecodedNode, CheckpointError> {
        let block = self.block_for_id(block_id)?;
        let record = self
            .records
            .by_hash(block)?
            .ok_or(CheckpointError::MissingBlock(block))?;
        let start = record
            .offset
            .checked_add(pointer)
            .ok_or(CheckpointError::InvalidCheckpoint("node offset overflow"))?;
        let window = self.blobs.window_for(start, expected_id)?;
        let mut reader = Reader::new(&window);
        reader.take(32)?;
        let id = reader.byte()?;
        if id & !CONTROL_BITS == 6 {
            // A patch is not the kind the pointer promised, so it needs the
            // read the pointer did not size for.
            let window = self.blobs.window(start)?;
            let mut reader = Reader::new(&window);
            reader.take(32)?;
            reader.byte()?;
            return self.decode_patch(block_id, &mut reader);
        }
        if id & !CONTROL_BITS != expected_id & !CONTROL_BITS {
            return Err(CheckpointError::InvalidCheckpoint(
                "node ID differs from pointer ID",
            ));
        }
        if id & !CONTROL_BITS == TrieNodeId::Leaf as u8 {
            let path = reader.path()?;
            let value = MarfValue::from_bytes(
                reader
                    .take(40)?
                    .try_into()
                    .map_err(|_| CheckpointError::InvalidCheckpoint("leaf value is truncated"))?,
            );
            return Ok(DecodedNode::Leaf { path, value });
        }

        let pointers = reader.pointers(id)?;
        if id & !CONTROL_BITS == TrieNodeId::Node48 as u8 {
            reader.take(256)?;
        }
        let path = reader.path()?;
        Ok(DecodedNode::Internal { id, path, pointers })
    }

    fn decode_patch(
        &self,
        block_id: u32,
        reader: &mut Reader<'_>,
    ) -> Result<DecodedNode, CheckpointError> {
        let base = reader.pointer(true)?;
        let diff_count = usize::from(reader.byte()?).saturating_add(1);
        let mut differences = Vec::with_capacity(diff_count);
        for _ in 0..diff_count {
            differences.push(reader.pointer(true)?);
        }
        let base_block_id = if base.id & BACK_POINTER != 0 {
            base.back_block
        } else {
            block_id
        };
        let DecodedNode::Internal {
            id,
            path,
            mut pointers,
        } = self.decode_node(
            base_block_id,
            usize::try_from(base.offset).map_err(|_| {
                CheckpointError::InvalidCheckpoint("patch pointer exceeds address space")
            })?,
            base.id,
        )?
        else {
            return Err(CheckpointError::InvalidCheckpoint("patch targets a leaf"));
        };

        for pointer in &mut pointers {
            if pointer.id != 0 && pointer.id & BACK_POINTER == 0 {
                pointer.id |= BACK_POINTER;
                pointer.back_block = base.back_block;
            }
        }
        for difference in differences {
            replace_pointer(id, &mut pointers, difference)?;
        }
        for pointer in &mut pointers {
            if pointer.id != 0 && pointer.id & BACK_POINTER == 0 {
                pointer.id |= BACK_POINTER;
                pointer.back_block = block_id;
            }
            if pointer.id & BACK_POINTER != 0 && pointer.back_block == block_id {
                pointer.id &= !BACK_POINTER;
                pointer.back_block = 0;
            }
        }
        Ok(DecodedNode::Internal { id, path, pointers })
    }

    fn block_for_id(&self, id: u32) -> Result<MarfBlockId, CheckpointError> {
        self.records.hash_for_id(id)?.ok_or(
            CheckpointError::InvalidCheckpoint("node points to unknown block ID"),
        )
    }
}

#[derive(Clone, Copy)]
struct Pointer {
    id: u8,
    character: u8,
    offset: u32,
    back_block: u32,
}

enum DecodedNode {
    Leaf {
        path: Vec<u8>,
        value: MarfValue,
    },
    Internal {
        id: u8,
        path: Vec<u8>,
        pointers: Vec<Pointer>,
    },
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn byte(&mut self) -> Result<u8, CheckpointError> {
        Ok(self.take(1)?[0])
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], CheckpointError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or(CheckpointError::InvalidCheckpoint("node length overflow"))?;
        let result = self
            .bytes
            .get(self.position..end)
            .ok_or(CheckpointError::InvalidCheckpoint("unexpected end of node"))?;
        self.position = end;
        Ok(result)
    }

    fn path(&mut self) -> Result<Vec<u8>, CheckpointError> {
        let length = usize::from(self.byte()?);
        if length > 32 {
            return Err(CheckpointError::InvalidCheckpoint(
                "trie path exceeds 32 bytes",
            ));
        }
        Ok(self.take(length)?.to_vec())
    }

    fn pointers(&mut self, node_id: u8) -> Result<Vec<Pointer>, CheckpointError> {
        let count = pointer_count(node_id)?;
        if node_id & COMPRESSED == 0 {
            return (0..count).map(|_| self.pointer(false)).collect();
        }

        let first = self.byte()?;
        if first != 0xff {
            self.position = self.position.checked_sub(1).expect("read one byte");
            return (0..count).map(|_| self.pointer(true)).collect();
        }
        let bitmap_size = count.div_ceil(8);
        let bitmap = self.take(bitmap_size)?.to_vec();
        let mut pointers = Vec::with_capacity(count);
        for index in 0..count {
            let bit = bitmap[index / 8] & (1 << (index % 8));
            pointers.push(if bit == 0 {
                Pointer {
                    id: 0,
                    character: 0,
                    offset: 0,
                    back_block: 0,
                }
            } else {
                self.pointer(true)?
            });
        }
        Ok(pointers)
    }

    fn pointer(&mut self, compressed: bool) -> Result<Pointer, CheckpointError> {
        let id = self.byte()? & !COMPRESSED;
        let character = self.byte()?;
        let offset = u32::from_be_bytes(self.take(4)?.try_into().expect("fixed slice"));
        let back_block = if compressed && id & BACK_POINTER == 0 {
            0
        } else {
            u32::from_be_bytes(self.take(4)?.try_into().expect("fixed slice"))
        };
        Ok(Pointer {
            id,
            character,
            offset,
            back_block,
        })
    }
}

const fn pointer_count(id: u8) -> Result<usize, CheckpointError> {
    match id & !CONTROL_BITS {
        2 => Ok(4),
        3 => Ok(16),
        4 => Ok(48),
        5 => Ok(256),
        _ => Err(CheckpointError::InvalidCheckpoint(
            "invalid internal node ID",
        )),
    }
}

fn replace_pointer(
    id: u8,
    pointers: &mut [Pointer],
    replacement: Pointer,
) -> Result<(), CheckpointError> {
    if id & !CONTROL_BITS == TrieNodeId::Node256 as u8 {
        pointers[usize::from(replacement.character)] = replacement;
        return Ok(());
    }
    if let Some(pointer) = pointers
        .iter_mut()
        .find(|pointer| pointer.id != 0 && pointer.character == replacement.character)
    {
        *pointer = replacement;
        return Ok(());
    }
    let pointer = pointers
        .iter_mut()
        .find(|pointer| pointer.id == 0)
        .ok_or(CheckpointError::InvalidCheckpoint("patch overfills node"))?;
    *pointer = replacement;
    Ok(())
}

pub fn parse_hex<const LENGTH: usize>(value: &str) -> Result<[u8; LENGTH], CheckpointError> {
    let mut bytes = [0; LENGTH];
    hex::decode_to_slice(value.strip_prefix("0x").unwrap_or(value), &mut bytes)
        .map_err(|_| CheckpointError::InvalidCheckpoint("value is not a 32-byte hash"))?;
    Ok(bytes)
}

fn fixed(bytes: &[u8]) -> Result<MarfBlockId, CheckpointError> {
    bytes
        .get(..32)
        .ok_or(CheckpointError::InvalidCheckpoint("blob is truncated"))?
        .try_into()
        .map_err(|_| CheckpointError::InvalidCheckpoint("blob is truncated"))
}
