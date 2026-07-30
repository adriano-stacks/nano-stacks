use std::{
    cell::RefCell,
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use rusqlite::Connection;
use serde::Deserialize;

use crate::{
    ChildTarget, MarfBlockId, MarfError, MarfValue, TrieChild, TrieHash, TrieNode, TrieNodeId,
    VersionedMarf, internal_node_hash, leaf_hash, node_id_for_children, slots, state_root,
    storage::TrieStorage,
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
    let blobs = Blobs::open(&blob_path(sqlite_path)?)?;
    let records = read_records(sqlite_path, &blobs)?;
    let source_record = records
        .get(&source)
        .ok_or(CheckpointError::MissingBlock(source))?;
    if source_record.root != expected_root {
        return Err(CheckpointError::RootMismatch {
            expected: expected_root,
            actual: source_record.root,
        });
    }

    let mut chain = vec![source];
    while let Some(parent) = records
        .get(chain.last().expect("chain is never empty"))
        .ok_or(CheckpointError::MissingBlock(source))?
        .parent
    {
        chain.push(parent);
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
    records: &BTreeMap<MarfBlockId, Record>,
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
            .get(block)
            .ok_or(CheckpointError::MissingBlock(*block))?;
        storage.complete_block(*block, id, record.root, empty_root, None)?;
        identifiers.insert(record.block_id, id);
        parent = Some(id);
    }

    let source = *chain.last().ok_or(CheckpointError::InvalidCheckpoint(
        "checkpoint has no source block",
    ))?;
    let source_record = records
        .get(&source)
        .ok_or(CheckpointError::MissingBlock(source))?;
    let mut importer = Importer {
        records,
        blocks_by_id: index_by_id(records),
        blobs,
        storage,
        identifiers,
        next: BTreeMap::new(),
    };
    let (index, content) = importer.import_root(source_record)?;

    let ancestors = power_of_two_ancestors(chain, chain.len() - 1);
    let ancestor_roots = ancestors
        .iter()
        .map(|block| {
            records
                .get(block)
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
    parent: Option<MarfBlockId>,
    offset: usize,
    root: TrieHash,
}

fn read_records(
    path: &Path,
    blobs: &Blobs,
) -> Result<BTreeMap<MarfBlockId, Record>, CheckpointError> {
    let uri = format!("file:{}?immutable=1", path.display());
    let connection = Connection::open_with_flags(
        uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    let mut statement = connection.prepare(
        "SELECT block_id, block_hash, external_offset FROM marf_data \
         WHERE unconfirmed = 0 AND external_length > 0",
    )?;
    let mut rows = statement.query([])?;
    let mut raw_records = Vec::new();
    while let Some(row) = rows.next()? {
        let block_id: u32 = row.get(0)?;
        let block: String = row.get(1)?;
        let offset: u64 = row.get(2)?;
        raw_records.push((
            block_id,
            parse_hex(&block)?,
            usize::try_from(offset).map_err(|_| {
                CheckpointError::InvalidCheckpoint("blob offset exceeds address space")
            })?,
        ));
    }
    drop(rows);
    drop(statement);
    drop(connection);

    let mut records = BTreeMap::new();
    for (block_id, block, offset) in raw_records {
        let header = blobs.read(offset, ROOT_OFFSET + 32)?;
        let root = TrieHash::from_bytes(fixed(&header[ROOT_OFFSET..])?);
        records.insert(
            block,
            Record {
                block_id,
                parent: None,
                offset,
                root,
            },
        );
    }

    let blocks_by_id = index_by_id(&records);
    let known: std::collections::BTreeSet<_> = records.keys().copied().collect();
    for record in records.values_mut() {
        let parent = fixed(&blobs.read(record.offset, 32)?)?;
        record.parent = known.contains(&parent).then_some(parent).or_else(|| {
            blocks_by_id
                .get(&record.block_id.saturating_sub(1))
                .copied()
        });
    }
    Ok(records)
}

fn index_by_id(records: &BTreeMap<MarfBlockId, Record>) -> BTreeMap<u32, MarfBlockId> {
    records
        .iter()
        .map(|(block, record)| (record.block_id, *block))
        .collect()
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
    records: &'a BTreeMap<MarfBlockId, Record>,
    blocks_by_id: BTreeMap<u32, MarfBlockId>,
    blobs: &'a Blobs,
    storage: &'a TrieStorage,
    identifiers: BTreeMap<u32, u32>,
    next: BTreeMap<u32, u32>,
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
            .get(&block)
            .ok_or(CheckpointError::MissingBlock(block))?;
        let start = record
            .offset
            .checked_add(pointer)
            .ok_or(CheckpointError::InvalidCheckpoint("node offset overflow"))?;
        let window = self.blobs.window(start)?;
        let mut reader = Reader::new(&window);
        reader.take(32)?;
        let id = reader.byte()?;
        if id & !CONTROL_BITS == 6 {
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
        self.blocks_by_id.get(&id).copied().ok_or(
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

fn parse_hex<const LENGTH: usize>(value: &str) -> Result<[u8; LENGTH], CheckpointError> {
    let value = value.strip_prefix("0x").unwrap_or(value);
    if value.len() != LENGTH * 2 {
        return Err(CheckpointError::InvalidCheckpoint(
            "hash has the wrong length",
        ));
    }
    let mut bytes = [0; LENGTH];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let text = value
            .get(index * 2..index * 2 + 2)
            .ok_or(CheckpointError::InvalidCheckpoint("hash is truncated"))?;
        *byte = u8::from_str_radix(text, 16)
            .map_err(|_| CheckpointError::InvalidCheckpoint("hash is not hexadecimal"))?;
    }
    Ok(bytes)
}

fn fixed(bytes: &[u8]) -> Result<MarfBlockId, CheckpointError> {
    bytes
        .get(..32)
        .ok_or(CheckpointError::InvalidCheckpoint("blob is truncated"))?
        .try_into()
        .map_err(|_| CheckpointError::InvalidCheckpoint("blob is truncated"))
}
