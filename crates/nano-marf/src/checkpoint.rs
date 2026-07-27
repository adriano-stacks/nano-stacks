use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use rusqlite::Connection;

use crate::{
    MarfBlockId, MarfTrie, MarfValue, MarfVersion, TrieChild, TrieHash, TrieNode, TrieNodeId,
    VersionedMarf, state_root,
};

const ROOT_OFFSET: usize = 36;
const BACK_POINTER: u8 = 0x80;
const COMPRESSED: u8 = 0x40;
const CONTROL_BITS: u8 = BACK_POINTER | COMPRESSED | 0x20 | 0x10;

/// Errors raised while importing a stacks-core MARF checkpoint.
#[derive(Debug)]
pub enum CheckpointError {
    Io(std::io::Error),
    Sql(rusqlite::Error),
    InvalidCheckpoint(&'static str),
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
            Self::InvalidCheckpoint(reason) => write!(formatter, "invalid checkpoint: {reason}"),
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
            Self::InvalidCheckpoint(_)
            | Self::MissingBlock(_)
            | Self::RootMismatch { .. }
            | Self::UnsupportedPatch => None,
        }
    }
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

/// Import a raw stacks-core SQLite/blob MARF checkpoint at `source`.
///
/// The imported state keeps its original trie graph and back-pointer block identities. The
/// checkpoint's published root is checked before it is made available for extension.
pub fn import_checkpoint(
    sqlite_path: impl AsRef<Path>,
    source: MarfBlockId,
    expected_root: TrieHash,
) -> Result<VersionedMarf, CheckpointError> {
    let sqlite_path = sqlite_path.as_ref();
    let records = read_records(sqlite_path)?;
    let blobs = fs::read(blob_path(sqlite_path)?)?;
    let loader = Loader {
        records: &records,
        blobs: &blobs,
    };
    let source_record = records
        .get(&source)
        .ok_or(CheckpointError::MissingBlock(source))?;
    let trie = loader.load_root(source_record)?;
    let actual_root = source_record.root;
    if actual_root != expected_root {
        return Err(CheckpointError::RootMismatch {
            expected: expected_root,
            actual: actual_root,
        });
    }

    let ancestors = ancestor_roots(source_record.parent, &records)?;
    let calculated_root = state_root(trie.root_hash(), &ancestors);
    if calculated_root != expected_root {
        return Err(CheckpointError::RootMismatch {
            expected: expected_root,
            actual: calculated_root,
        });
    }

    let mut versions = BTreeMap::new();
    for (block, record) in &records {
        versions.insert(
            *block,
            MarfVersion {
                parent: record.parent,
                height: height(*block, &records)?,
                trie: MarfTrie::default(),
                root: record.root,
            },
        );
    }
    let version = versions
        .get_mut(&source)
        .ok_or(CheckpointError::MissingBlock(source))?;
    version.trie = trie;

    Ok(VersionedMarf {
        versions,
        active: None,
    })
}

#[derive(Clone)]
struct Record {
    block_id: u32,
    parent: Option<MarfBlockId>,
    offset: usize,
    root: TrieHash,
}

fn read_records(path: &Path) -> Result<BTreeMap<MarfBlockId, Record>, CheckpointError> {
    let connection = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
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
            parse_block(&block)?,
            usize::try_from(offset).map_err(|_| {
                CheckpointError::InvalidCheckpoint("blob offset exceeds address space")
            })?,
        ));
    }
    drop(rows);
    drop(statement);
    drop(connection);

    let blobs = fs::read(blob_path(path)?)?;
    let mut records = BTreeMap::new();
    for (block_id, block, offset) in raw_records {
        let parent = block_at(&blobs, offset, 32)?;
        let root = TrieHash::from_bytes(block_at(&blobs, offset + ROOT_OFFSET, 32)?);
        records.insert(
            block,
            Record {
                block_id,
                parent: None,
                offset,
                root,
            },
        );
        let _ = parent;
    }

    let blocks_by_id: BTreeMap<_, _> = records
        .iter()
        .map(|(block, record)| (record.block_id, *block))
        .collect();
    let known_blocks: Vec<_> = records.keys().copied().collect();
    for record in records.values_mut() {
        let parent = block_at(&blobs, record.offset, 32)?;
        record.parent = known_blocks
            .contains(&parent)
            .then_some(parent)
            .or_else(|| {
                blocks_by_id
                    .get(&record.block_id.saturating_sub(1))
                    .copied()
            });
    }
    Ok(records)
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

fn ancestor_roots(
    mut block: Option<MarfBlockId>,
    records: &BTreeMap<MarfBlockId, Record>,
) -> Result<Vec<TrieHash>, CheckpointError> {
    let mut roots = Vec::new();
    while let Some(current) = block {
        let record = records
            .get(&current)
            .ok_or(CheckpointError::MissingBlock(current))?;
        roots.push(record.root);
        block = record.parent;
    }
    Ok(roots)
}

fn height(
    block: MarfBlockId,
    records: &BTreeMap<MarfBlockId, Record>,
) -> Result<u32, CheckpointError> {
    let mut height = 0_u32;
    let mut cursor = records
        .get(&block)
        .ok_or(CheckpointError::MissingBlock(block))?
        .parent;
    while let Some(parent) = cursor {
        height = height
            .checked_add(1)
            .ok_or(CheckpointError::InvalidCheckpoint("height overflow"))?;
        cursor = records
            .get(&parent)
            .ok_or(CheckpointError::MissingBlock(parent))?
            .parent;
    }
    Ok(height)
}

struct Loader<'a> {
    records: &'a BTreeMap<MarfBlockId, Record>,
    blobs: &'a [u8],
}

impl Loader<'_> {
    fn load_root(&self, record: &Record) -> Result<MarfTrie, CheckpointError> {
        let node = self.load_node(record.block_id, ROOT_OFFSET, TrieNodeId::Node256 as u8)?;
        let TrieNode::Internal { path, children } = node else {
            return Err(CheckpointError::InvalidCheckpoint(
                "root is not an internal node",
            ));
        };
        if !path.is_empty() {
            return Err(CheckpointError::InvalidCheckpoint(
                "root has a compressed path",
            ));
        }
        Ok(MarfTrie {
            root_children: children,
        })
    }

    fn load_node(
        &self,
        block_id: u32,
        pointer: usize,
        expected_id: u8,
    ) -> Result<TrieNode, CheckpointError> {
        match self.decode_node(block_id, pointer, expected_id)? {
            DecodedNode::Leaf { path, value } => Ok(TrieNode::Leaf { path, value }),
            DecodedNode::Internal { path, pointers, .. } => {
                let mut children = Vec::new();
                for pointer in pointers {
                    if pointer.id == 0 {
                        continue;
                    }
                    let target_id = if pointer.id & BACK_POINTER != 0 {
                        pointer.back_block
                    } else {
                        block_id
                    };
                    let target_block = self.block_for_id(target_id)?;
                    let node = self.load_node(
                        target_id,
                        usize::try_from(pointer.offset).map_err(|_| {
                            CheckpointError::InvalidCheckpoint("node pointer exceeds address space")
                        })?,
                        pointer.id,
                    )?;
                    children.push(TrieChild {
                        character: pointer.character,
                        node: Box::new(node),
                        referenced_block: (pointer.id & BACK_POINTER != 0).then_some(target_block),
                    });
                }
                Ok(TrieNode::Internal { path, children })
            }
        }
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
        let bytes = self
            .blobs
            .get(start..)
            .ok_or(CheckpointError::InvalidCheckpoint(
                "node points outside blob file",
            ))?;
        let mut reader = Reader::new(bytes);
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
            let value = MarfValue::from_bytes(reader.take(40)?.try_into().expect("fixed slice"));
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
        self.records
            .iter()
            .find_map(|(block, record)| (record.block_id == id).then_some(*block))
            .ok_or(CheckpointError::InvalidCheckpoint(
                "node points to unknown block ID",
            ))
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

fn parse_block(value: &str) -> Result<MarfBlockId, CheckpointError> {
    if value.len() != 64 {
        return Err(CheckpointError::InvalidCheckpoint(
            "block hash is not 32 bytes",
        ));
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let text =
            value
                .get(index * 2..index * 2 + 2)
                .ok_or(CheckpointError::InvalidCheckpoint(
                    "block hash is truncated",
                ))?;
        *byte = u8::from_str_radix(text, 16)
            .map_err(|_| CheckpointError::InvalidCheckpoint("block hash is not hexadecimal"))?;
    }
    Ok(bytes)
}

fn block_at(bytes: &[u8], offset: usize, length: usize) -> Result<MarfBlockId, CheckpointError> {
    if length != 32 {
        return Err(CheckpointError::InvalidCheckpoint(
            "invalid fixed block length",
        ));
    }
    Ok(bytes
        .get(offset..offset + length)
        .ok_or(CheckpointError::InvalidCheckpoint("blob is truncated"))?
        .try_into()
        .expect("fixed slice"))
}
