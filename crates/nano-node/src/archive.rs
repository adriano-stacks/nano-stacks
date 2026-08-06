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
use nano_primitives::StacksBlockId;
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
    /// A thread panicked while holding the store, so it cannot be trusted.
    Poisoned,
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "executed block storage: {error}"),
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
    pub fn len(&self) -> Result<u64, ArchiveError> {
        Ok(self
            .connection()?
            .query_row("SELECT count(*) FROM executed", [], |row| row.get(0))?)
    }

    fn stored(&self, block_id: StacksBlockId) -> Result<Option<(Vec<u8>, Vec<u8>, u64)>, ArchiveError> {
        Ok(self
            .connection()?
            .query_row(
                "SELECT bytes, consensus_hash, height FROM executed WHERE block_id = ?1",
                params![block_id.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?)
    }

    /// The blocks of one tenure from `height` up, lowest first.
    fn tenure_from(
        &self,
        consensus_hash: &[u8],
        height: u64,
    ) -> Result<Vec<(StacksBlockId, Vec<u8>)>, ArchiveError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT block_id, bytes FROM executed
             WHERE consensus_hash = ?1 AND height >= ?2 ORDER BY height ASC",
        )?;
        let rows = statement.query_map(params![consensus_hash, height], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        let mut blocks = Vec::new();
        for row in rows {
            let (id, bytes) = row?;
            blocks.push((
                StacksBlockId::from_bytes(id.try_into().unwrap_or([0; 32])),
                bytes,
            ));
        }
        Ok(blocks)
    }
}

/// Reading fails only where the store itself is broken, and a route that cannot
/// read one block is not a route that should refuse every other request. So a
/// failure reads as "this node does not have it", said once where it happened.
impl nano_rpc::ExecutedBlocks for Archive {
    fn block(&self, block_id: StacksBlockId) -> Option<Vec<u8>> {
        match self.stored(block_id) {
            Ok(stored) => stored.map(|(bytes, _, _)| bytes),
            Err(error) => {
                eprintln!("cannot read the executed block {block_id}: {error}");
                None
            }
        }
    }

    fn tenure(&self, start_block_id: StacksBlockId, stop: Option<StacksBlockId>) -> Vec<Vec<u8>> {
        let Ok(Some((_, consensus_hash, height))) = self.stored(start_block_id) else {
            return Vec::new();
        };
        match self.tenure_from(&consensus_hash, height) {
            Ok(blocks) => blocks
                .into_iter()
                .take_while(|(id, _)| Some(*id) != stop)
                .map(|(_, bytes)| bytes)
                .collect(),
            Err(error) => {
                eprintln!("cannot read the tenure starting at {start_block_id}: {error}");
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use nano_chainstate::NakamotoBlock;
    use nano_primitives::StacksBlockId;
    use nano_rpc::ExecutedBlocks;

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

    /// A tenure comes back whole, in height order, and stops where asked.
    #[test]
    fn a_tenure_is_served_from_its_first_block_and_stops_where_asked() {
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
        for consensus_hash in blocks
            .iter()
            .map(|block| block.header.consensus_hash)
            .collect::<std::collections::BTreeSet<_>>()
        {
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
            tenure.len() > 1,
            "the fixture window holds no tenure of more than one block, so this proves nothing"
        );
        let first = tenure[0];

        let served = archive.tenure(first.block_id(), None);
        assert_eq!(
            served,
            tenure
                .iter()
                .map(|block| block.encode())
                .collect::<Vec<_>>()
        );

        // Stopping before a block a caller already has is what the peer protocol
        // asks for, and it stops *before* rather than after.
        let stop = tenure[1];
        assert_eq!(
            archive.tenure(first.block_id(), Some(stop.block_id())),
            vec![first.encode()]
        );
        assert!(archive.tenure(StacksBlockId::from_bytes([9; 32]), None).is_empty());
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
        assert_eq!(archive.len().expect("count"), 4);
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
