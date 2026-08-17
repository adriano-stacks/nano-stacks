//! The blocks this node kept because it executed them.
//!
//! Staging makes the *descent* durable and drops each block the moment it seals,
//! which left a node unable to serve a block it had itself executed: `/v3/blocks`
//! and `/v3/tenures` answered out of the peer view bounded at the executed tip,
//! so a follower 36,876 blocks behind mainnet answered `404` for its own tip. It
//! had the header and the state; the bytes were gone.
//!
//! So sealing a block writes it here as well as forgetting it there. Nothing in
//! this store is consensus — the state root already agreed with the network
//! before a block reached it — which is why a write that fails is reported and
//! stepped over rather than ending the round.
//!
//! Bounded, because a node that keeps every block it ever executed grows without
//! limit and no route asks that of it: what a peer, a signer or a downstream node
//! reads is recent history. [`ARCHIVE_BLOCKS`] is the window, and the oldest
//! blocks fall out of it as newer ones arrive.

use std::{path::Path, sync::Mutex};

use nano_chainstate::NakamotoBlock;
use nano_primitives::{ConsensusHash, StacksBlockId};
use nano_rpc::ExecutedTenure;
use rusqlite::{Connection, OptionalExtension, params};

/// How many executed blocks are kept.
///
/// A signer's reorganization check reaches ten tenures back and a peer catching
/// up asks for whole tenures, so the window has to cover more than a reward
/// cycle's worth of blocks rather than a handful. At mainnet's rate this is a few
/// days; at 30 KB a block it is a couple of gigabytes at the very worst, and
/// mainnet blocks average a fraction of that.
pub const ARCHIVE_BLOCKS: u64 = 20_000;

/// Why an executed block could not be kept or read back.
#[derive(Debug)]
pub enum ArchiveError {
    Storage(rusqlite::Error),
    /// A row this process wrote no longer contains a consensus hash.
    MalformedConsensusHash(usize),
    /// A row this process wrote no longer contains a block identifier.
    MalformedBlockId(usize),
    /// A row this process wrote no longer contains the block it names.
    MalformedBlock(StacksBlockId),
    /// A thread panicked while holding the store, so it cannot be trusted.
    Poisoned,
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "executed block storage: {error}"),
            Self::MalformedConsensusHash(length) => {
                write!(
                    formatter,
                    "executed block storage holds a {length}-byte consensus hash"
                )
            }
            Self::MalformedBlockId(length) => {
                write!(
                    formatter,
                    "executed block storage holds a {length}-byte block identifier"
                )
            }
            Self::MalformedBlock(block_id) => {
                write!(
                    formatter,
                    "executed block storage cannot decode block {block_id}"
                )
            }
            Self::Poisoned => formatter.write_str("the executed block store was poisoned"),
        }
    }
}

impl std::error::Error for ArchiveError {}

impl From<rusqlite::Error> for ArchiveError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error)
    }
}

/// A bounded window of the blocks this node executed, on disk.
#[derive(Debug)]
pub struct Archive {
    connection: Mutex<Connection>,
    kept: u64,
}

impl Archive {
    fn connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>, ArchiveError> {
        self.connection.lock().map_err(|_| ArchiveError::Poisoned)
    }

    /// Open, creating the store when it is not there yet.
    pub fn open(path: &Path) -> Result<Self, ArchiveError> {
        Self::with_window(path, ARCHIVE_BLOCKS)
    }

    /// Open keeping a window of your own, which is how the bound is testable.
    pub fn with_window(path: &Path, kept: u64) -> Result<Self, ArchiveError> {
        let connection = Connection::open(path)?;
        connection.execute_batch(
            // `NORMAL` for the same reason staging uses it: losing the last few
            // blocks of this store to a power cut costs a node nothing it cannot
            // fetch again, and it is not what a state root is checked against.
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS executed (
                 block_id BLOB PRIMARY KEY,
                 consensus_hash BLOB NOT NULL,
                 height INTEGER NOT NULL,
                 bytes BLOB NOT NULL
             ) WITHOUT ROWID;
             CREATE INDEX IF NOT EXISTS executed_tenure
                 ON executed (consensus_hash, height);
             CREATE INDEX IF NOT EXISTS executed_height ON executed (height);",
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
            kept: kept.max(1),
        })
    }

    /// Keep a block this node executed, and forget anything past the window.
    ///
    /// The tenure is keyed by consensus hash rather than by walking parents,
    /// because that is the question `/v3/tenures/:id` asks: every block of a
    /// tenure carries the consensus hash of the sortition that elected it.
    pub fn keep(&self, block: &NakamotoBlock) -> Result<(), ArchiveError> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT OR REPLACE INTO executed (block_id, consensus_hash, height, bytes)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                block.block_id().as_bytes().as_slice(),
                block.header.consensus_hash.as_bytes().as_slice(),
                block.header.chain_length,
                block.encode(),
            ],
        )?;
        // Pruned by height rather than by count so that a fork's retracted blocks
        // — which sit at heights the chain has since re-executed — go out with the
        // window they belong to instead of surviving it.
        connection.execute(
            "DELETE FROM executed WHERE height <= ?1 - ?2",
            params![block.header.chain_length, self.kept],
        )?;
        drop(connection);
        Ok(())
    }

    /// Forget every block at or above a height, which a fork switch calls for.
    ///
    /// A retracted block is one this node no longer claims to have executed, and
    /// serving it would be answering for a chain it has left.
    pub fn retract_from(&self, height: u64) -> Result<usize, ArchiveError> {
        Ok(self
            .connection()?
            .execute("DELETE FROM executed WHERE height >= ?1", params![height])?)
    }

    /// How many blocks are kept.
    pub fn kept(&self) -> Result<u64, ArchiveError> {
        Ok(self
            .connection()?
            .query_row("SELECT count(*) FROM executed", [], |row| row.get(0))?)
    }

    /// The tenures this archive can serve, newest first.
    pub fn executed_tenures(&self) -> Result<Vec<ConsensusHash>, ArchiveError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT consensus_hash FROM executed
             GROUP BY consensus_hash ORDER BY max(height) DESC",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        let mut tenures = Vec::new();
        for row in rows {
            let bytes = row?;
            let length = bytes.len();
            let bytes = bytes
                .try_into()
                .map_err(|_| ArchiveError::MalformedConsensusHash(length))?;
            tenures.push(ConsensusHash::from_bytes(bytes));
        }
        drop(statement);
        drop(connection);
        Ok(tenures)
    }

    /// The block this node executed at a Stacks height, when it kept exactly one.
    ///
    /// Asked by name rather than served over [`nano_rpc::ExecutedBlocks`], because
    /// no route asks a height: this is how the node reads back a tenure-start block
    /// a hundred tenures below its tip to name the rewards it matured, and the
    /// alternative was carrying that provenance in the ledger it serializes with
    /// every block.
    ///
    /// Nothing when two blocks share the height. A retracted fork's block sits at a
    /// height the chain has since re-executed and `retract_from` is what normally
    /// takes it back, so a height with two under it is one where that did not
    /// happen — and naming either of them would be a guess.
    #[must_use]
    pub fn block_at_height(&self, height: u64) -> Option<NakamotoBlock> {
        let kept = match self.at_height(height) {
            Ok(kept) => kept,
            Err(error) => {
                eprintln!("cannot read the executed block at height {height}: {error}");
                return None;
            }
        };
        let [bytes] = kept.as_slice() else {
            return None;
        };
        NakamotoBlock::decode(bytes).ok()
    }

    /// The blocks kept at one height, at most two of them: the count is the whole
    /// question, so reading a third would be reading for nothing.
    fn at_height(&self, height: u64) -> Result<Vec<Vec<u8>>, ArchiveError> {
        let connection = self.connection()?;
        let mut statement =
            connection.prepare("SELECT bytes FROM executed WHERE height = ?1 LIMIT 2")?;
        let rows = statement.query_map(params![height], |row| row.get::<_, Vec<u8>>(0))?;
        let mut blocks = Vec::new();
        for row in rows {
            blocks.push(row?);
        }
        drop(statement);
        drop(connection);
        Ok(blocks)
    }

    fn stored(&self, block_id: StacksBlockId) -> Result<Option<StoredBlock>, ArchiveError> {
        Ok(self
            .connection()?
            .query_row(
                "SELECT bytes, consensus_hash, height FROM executed WHERE block_id = ?1",
                params![block_id.as_bytes().as_slice()],
                |row| {
                    Ok(StoredBlock {
                        bytes: row.get(0)?,
                        consensus_hash: row.get(1)?,
                        height: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// The named block and its ancestors in the same tenure, newest first.
    fn tenure_from(
        &self,
        start_block_id: StacksBlockId,
        consensus_hash: &[u8],
        height: u64,
        stop: Option<StacksBlockId>,
        max_bytes: usize,
    ) -> Result<ExecutedTenure, ArchiveError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT block_id, bytes FROM executed
             WHERE consensus_hash = ?1 AND height <= ?2 ORDER BY height DESC",
        )?;
        let rows = statement.query_map(params![consensus_hash, height], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        let mut bytes_out = Vec::new();
        let mut expected = start_block_id;
        for row in rows {
            let (id, bytes) = row?;
            let length = id.len();
            let id = StacksBlockId::from_bytes(
                id.try_into()
                    .map_err(|_| ArchiveError::MalformedBlockId(length))?,
            );
            if id != expected {
                continue;
            }
            if !bytes_out.is_empty() && Some(id) == stop {
                break;
            }
            if bytes.len() > max_bytes.saturating_sub(bytes_out.len()) {
                return Ok(ExecutedTenure::TooLarge);
            }
            let block =
                NakamotoBlock::decode(&bytes).map_err(|_| ArchiveError::MalformedBlock(id))?;
            expected = block.header.parent_block_id;
            bytes_out.extend(bytes);
        }
        drop(statement);
        drop(connection);
        Ok(if bytes_out.is_empty() {
            ExecutedTenure::Missing
        } else {
            ExecutedTenure::Found(bytes_out)
        })
    }
}

/// One block as the archive holds it: its bytes, the tenure it belongs to and
/// the height it was executed at.
struct StoredBlock {
    bytes: Vec<u8>,
    consensus_hash: Vec<u8>,
    height: u64,
}

/// Reading fails only where the store itself is broken, and a route that cannot
/// read one block is not a route that should refuse every other request. So a
/// failure reads as "this node does not have it", said once where it happened.
impl nano_rpc::ExecutedBlocks for Archive {
    fn block(&self, block_id: StacksBlockId) -> Option<Vec<u8>> {
        match self.stored(block_id) {
            Ok(stored) => stored.map(|stored| stored.bytes),
            Err(error) => {
                eprintln!("cannot read the executed block {block_id}: {error}");
                None
            }
        }
    }

    fn tenure_start(&self, block_id: StacksBlockId) -> Option<StacksBlockId> {
        let stored = self.stored(block_id).ok().flatten()?;
        let found = self.connection().ok()?.query_row(
            "SELECT block_id FROM executed WHERE consensus_hash = ?1 ORDER BY height ASC LIMIT 1",
            params![stored.consensus_hash],
            |row| row.get::<_, Vec<u8>>(0),
        );
        match found {
            Ok(bytes) => <[u8; 32]>::try_from(bytes.as_slice())
                .ok()
                .map(StacksBlockId::from_bytes),
            Err(error) => {
                eprintln!("cannot read the tenure start of {block_id}: {error}");
                None
            }
        }
    }

    fn tenure_tip(&self, consensus_hash: &[u8; 20]) -> Option<Vec<u8>> {
        let found = self.connection().ok()?.query_row(
            "SELECT bytes FROM executed WHERE consensus_hash = ?1 ORDER BY height DESC LIMIT 1",
            params![consensus_hash.as_slice()],
            |row| row.get::<_, Vec<u8>>(0),
        );
        match found {
            Ok(bytes) => Some(bytes),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(error) => {
                eprintln!(
                    "cannot read the last block of tenure {}: {error}",
                    hex::encode(consensus_hash)
                );
                None
            }
        }
    }

    fn tenure(
        &self,
        start_block_id: StacksBlockId,
        stop: Option<StacksBlockId>,
        max_bytes: usize,
    ) -> ExecutedTenure {
        let Ok(Some(start)) = self.stored(start_block_id) else {
            return ExecutedTenure::Missing;
        };
        match self.tenure_from(
            start_block_id,
            &start.consensus_hash,
            start.height,
            stop,
            max_bytes,
        ) {
            Ok(blocks) => blocks,
            Err(error) => {
                eprintln!("cannot read the tenure starting at {start_block_id}: {error}");
                ExecutedTenure::Missing
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use nano_chainstate::NakamotoBlock;
    use nano_primitives::{ConsensusHash, StacksBlockId};
    use nano_rpc::{ExecutedBlocks, ExecutedTenure};

    use super::Archive;

    /// Captured blocks, lowest first, which is the order they were executed in.
    fn fixtures() -> Vec<NakamotoBlock> {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../nano-conformance/fixtures/nakamoto/blocks");
        let mut paths = fs::read_dir(directory)
            .expect("read fixture blocks")
            .map(|entry| entry.expect("fixture block").path())
            .collect::<Vec<_>>();
        paths.sort();
        paths
            .into_iter()
            .take(12)
            .map(|path| {
                NakamotoBlock::decode(&fs::read(path).expect("read a block")).expect("decode")
            })
            .collect()
    }

    /// A node serves the blocks it executed, from its own store, after a restart.
    ///
    /// The restart is the point. Before this store existed the routes answered out
    /// of a peer view, so a node that had executed a block and been restarted
    /// could not serve it — which is what a follower far behind mainnet does for
    /// its whole catch-up.
    #[test]
    fn an_executed_block_is_served_by_its_identifier_after_reopening() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("archive.sqlite");
        let blocks = fixtures();
        {
            let archive = Archive::open(&path).expect("open");
            for block in &blocks {
                archive.keep(block).expect("keep");
            }
        }

        let archive = Archive::open(&path).expect("reopen");
        for block in &blocks {
            assert_eq!(
                archive.block(block.block_id()).expect("the block is kept"),
                block.encode(),
                "block {} at height {}",
                block.block_id(),
                block.header.chain_length
            );
        }
        assert!(archive.block(StacksBlockId::from_bytes([9; 32])).is_none());
    }

    #[test]
    fn every_tenure_with_archived_blocks_is_advertisable() {
        let directory = tempfile::tempdir().expect("a directory");
        let archive = Archive::open(&directory.path().join("archive.sqlite")).expect("open");
        let mut blocks = fixtures().into_iter().take(3).collect::<Vec<_>>();
        let earlier = ConsensusHash::from_bytes([1; 20]);
        let later = ConsensusHash::from_bytes([2; 20]);
        blocks[0].header.consensus_hash = earlier;
        blocks[1].header.consensus_hash = later;
        blocks[2].header.consensus_hash = earlier;
        for block in &blocks {
            archive.keep(block).expect("keep");
        }

        assert_eq!(
            archive.executed_tenures().expect("read tenures"),
            vec![earlier, later],
            "a tenure remains advertisable while any of its blocks remain serveable"
        );
    }

    /// A tenure comes back from the cursor through its ancestors and stops where asked.
    #[test]
    fn a_tenure_is_served_from_its_cursor_and_stops_where_asked() {
        let directory = tempfile::tempdir().expect("a directory");
        let archive = Archive::open(&directory.path().join("archive.sqlite")).expect("open");
        let blocks = fixtures();
        for block in &blocks {
            archive.keep(block).expect("keep");
        }
        // The longest tenure the window holds whole: a tenure is every block
        // carrying the consensus hash of the sortition that elected it, and a
        // one-block tenure would prove nothing about ordering or stopping.
        let mut tenures: Vec<Vec<&NakamotoBlock>> = Vec::new();
        let consensus_hashes: std::collections::BTreeSet<_> = blocks
            .iter()
            .map(|block| block.header.consensus_hash)
            .collect();
        for consensus_hash in consensus_hashes {
            tenures.push(
                blocks
                    .iter()
                    .filter(|block| block.header.consensus_hash == consensus_hash)
                    .collect(),
            );
        }
        let tenure = tenures
            .into_iter()
            .max_by_key(Vec::len)
            .expect("the window holds a tenure");
        assert!(
            tenure.len() > 2,
            "the fixture window holds no tenure long enough to test a stop cursor"
        );
        let cursor = tenure.last().expect("a non-empty tenure");

        let expected = tenure
            .iter()
            .rev()
            .flat_map(|block| block.encode())
            .collect::<Vec<_>>();
        let served = archive.tenure(cursor.block_id(), None, usize::MAX);
        assert_eq!(served, ExecutedTenure::Found(expected.clone()));

        // Stopping before a block a caller already has is what the peer protocol
        // asks for, and it stops *before* rather than after.
        let stop = tenure[1];
        assert_eq!(
            archive.tenure(cursor.block_id(), Some(stop.block_id()), usize::MAX),
            ExecutedTenure::Found(
                tenure[2..]
                    .iter()
                    .rev()
                    .flat_map(|block| block.encode())
                    .collect()
            )
        );
        assert_eq!(
            archive.tenure(StacksBlockId::from_bytes([9; 32]), None, usize::MAX),
            ExecutedTenure::Missing
        );

        assert_eq!(
            archive.tenure(cursor.block_id(), None, expected.len() - 1),
            ExecutedTenure::TooLarge
        );
        assert_eq!(
            archive.tenure(cursor.block_id(), None, expected.len()),
            ExecutedTenure::Found(expected)
        );
    }

    /// A block is found by its height, and an ambiguous height answers nothing.
    ///
    /// The height lookup is how the node reads a tenure-start block back to name
    /// the rewards it matured, and it is only sound while one block holds the
    /// height. A retracted fork's block sits at a height the chain has since
    /// re-executed, so this is the case where naming a block would name the wrong
    /// one — and the second block here is exactly that: the same height, a
    /// different identifier.
    #[test]
    fn a_block_is_found_by_its_height_unless_two_blocks_share_it() {
        let directory = tempfile::tempdir().expect("a directory");
        let archive = Archive::open(&directory.path().join("archive.sqlite")).expect("open");
        let blocks = fixtures();
        for block in &blocks {
            archive.keep(block).expect("keep");
        }

        for block in &blocks {
            let found = archive
                .block_at_height(block.header.chain_length)
                .expect("the block is kept");
            assert_eq!(found.block_id(), block.block_id());
        }
        assert!(archive.block_at_height(0).is_none());

        // The same height under a second identifier, which is what a fork the chain
        // has left leaves behind when nothing retracted it.
        let mut forked = blocks[0].clone();
        forked.header.timestamp += 1;
        assert_ne!(forked.block_id(), blocks[0].block_id());
        archive.keep(&forked).expect("keep the fork's block too");
        assert!(
            archive
                .block_at_height(forked.header.chain_length)
                .is_none(),
            "an ambiguous height named one of two blocks"
        );
        // By identifier both are still there: that question has one answer each.
        assert!(archive.block(forked.block_id()).is_some());
        assert!(archive.block(blocks[0].block_id()).is_some());
    }

    /// The window is a bound: the oldest blocks leave as newer ones arrive, and a
    /// fork switch takes back what it retracted.
    #[test]
    fn the_window_is_bounded_and_a_retraction_gives_blocks_back() {
        let directory = tempfile::tempdir().expect("a directory");
        let archive =
            Archive::with_window(&directory.path().join("archive.sqlite"), 4).expect("open");
        let blocks = fixtures();
        for block in &blocks {
            archive.keep(block).expect("keep");
        }
        assert_eq!(archive.kept().expect("count"), 4);
        let newest = blocks.last().expect("a newest block");
        assert!(archive.block(newest.block_id()).is_some());
        assert!(
            archive.block(blocks[0].block_id()).is_none(),
            "the oldest block should have left the window"
        );

        // A chain that gave blocks back must not keep answering for them.
        archive
            .retract_from(newest.header.chain_length)
            .expect("retract");
        assert!(archive.block(newest.block_id()).is_none());
    }
}
