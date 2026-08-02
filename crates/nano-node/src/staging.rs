//! Blocks fetched from a peer but not yet executed.
//!
//! Nakamoto blocks name their parent, never their child, so reaching a peer's
//! tip from this node's own means walking backwards. Holding that walk in
//! memory makes catching up all-or-nothing: against mainnet the gap was twenty
//! thousand blocks, one rate limit anywhere in the descent discarded every
//! block fetched, and the next round started again from the peer's — by then
//! higher — tip. The executed height never moved.
//!
//! Staging makes the descent durable. Each block is written as it arrives, so a
//! round that ends early keeps what it fetched, a later round resumes from the
//! lowest block it has rather than from the tip, and a restart costs nothing.

use std::{path::Path, sync::Mutex};

use nano_chainstate::{NakamotoBlock, NakamotoCodecError};
use nano_primitives::StacksBlockId;
use rusqlite::{Connection, OptionalExtension, params};

/// Why a staged block could not be read or written.
#[derive(Debug)]
pub enum StagingError {
    Storage(rusqlite::Error),
    Block(NakamotoCodecError),
    /// A thread panicked while holding the store, so it cannot be trusted.
    Poisoned,
}

impl std::fmt::Display for StagingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "staging storage: {error}"),
            Self::Block(error) => write!(formatter, "staged block: {error}"),
            Self::Poisoned => formatter.write_str("the staging store was poisoned"),
        }
    }
}

impl std::error::Error for StagingError {}

impl From<rusqlite::Error> for StagingError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error)
    }
}

/// The blocks between this node's executed tip and a peer's, on disk.
///
/// The connection is guarded because a follower holds this store across the
/// awaits it spends fetching; every method locks for the length of one
/// statement and none of them await.
#[derive(Debug)]
pub struct Staging {
    connection: Mutex<Connection>,
}

impl Staging {
    /// The guarded connection, or the poisoning that made it unusable.
    fn connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StagingError> {
        self.connection.lock().map_err(|_| StagingError::Poisoned)
    }

    /// Open, creating the store when it is not there yet.
    pub fn open(path: &Path) -> Result<Self, StagingError> {
        let connection = Connection::open(path)?;
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS staged (
                 block_id BLOB PRIMARY KEY,
                 parent_block_id BLOB NOT NULL,
                 height INTEGER NOT NULL,
                 bytes BLOB NOT NULL
             ) WITHOUT ROWID;
             CREATE INDEX IF NOT EXISTS staged_parent ON staged (parent_block_id);
             CREATE INDEX IF NOT EXISTS staged_height ON staged (height);",
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Keep a block for later execution. Staging the same block twice is fine.
    pub fn put(&self, block: &NakamotoBlock) -> Result<(), StagingError> {
        self.connection()?.execute(
            "INSERT OR REPLACE INTO staged (block_id, parent_block_id, height, bytes)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                block.block_id().as_bytes().as_slice(),
                block.header.parent_block_id.as_bytes().as_slice(),
                block.header.chain_length,
                block.encode(),
            ],
        )?;
        Ok(())
    }

    /// Whether this block is already staged.
    pub fn holds(&self, block_id: StacksBlockId) -> Result<bool, StagingError> {
        Ok(self
            .connection()?
            .query_row(
                "SELECT 1 FROM staged WHERE block_id = ?1",
                params![block_id.as_bytes().as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .is_some())
    }

    /// The parent of the lowest staged block, which is where a descent resumes.
    ///
    /// Staged blocks descend from one chain, so the lowest is the furthest the
    /// walk has reached and its parent is the next block to ask for.
    pub fn descent_resumes_at(&self) -> Result<Option<StacksBlockId>, StagingError> {
        self.connection()?
            .query_row(
                "SELECT parent_block_id FROM staged ORDER BY height ASC LIMIT 1",
                [],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
            .map(|bytes| {
                Ok(StacksBlockId::from_bytes(
                    bytes.try_into().unwrap_or([0; 32]),
                ))
            })
            .transpose()
    }

    /// The staged block whose parent is `parent`, if it is here.
    pub fn child_of(&self, parent: StacksBlockId) -> Result<Option<NakamotoBlock>, StagingError> {
        let bytes = self
            .connection()?
            .query_row(
                "SELECT bytes FROM staged WHERE parent_block_id = ?1",
                params![parent.as_bytes().as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        bytes
            .map(|bytes| NakamotoBlock::decode(&bytes).map_err(StagingError::Block))
            .transpose()
    }

    /// Forget a block, which is what sealing it makes right.
    pub fn remove(&self, block_id: StacksBlockId) -> Result<(), StagingError> {
        self.connection()?.execute(
            "DELETE FROM staged WHERE block_id = ?1",
            params![block_id.as_bytes().as_slice()],
        )?;
        Ok(())
    }

    /// Forget everything at or below a height, which sealing past it makes
    /// dead weight — and which a descent that overshot leaves behind.
    pub fn remove_to(&self, height: u64) -> Result<usize, StagingError> {
        Ok(self
            .connection()?
            .execute("DELETE FROM staged WHERE height <= ?1", params![height])?)
    }

    /// Drop everything, which a fork or a corrupt descent calls for.
    pub fn clear(&self) -> Result<(), StagingError> {
        self.connection()?.execute("DELETE FROM staged", [])?;
        Ok(())
    }

    /// How many blocks are waiting.
    pub fn len(&self) -> Result<u64, StagingError> {
        Ok(self
            .connection()?
            .query_row("SELECT count(*) FROM staged", [], |row| row.get(0))?)
    }

    /// Whether nothing is waiting.
    pub fn is_empty(&self) -> Result<bool, StagingError> {
        Ok(self.len()? == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::Staging;
    use nano_chainstate::NakamotoBlock;
    use nano_primitives::StacksBlockId;
    use std::{fs, path::Path};

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
            .take(4)
            .map(|path| {
                NakamotoBlock::decode(&fs::read(path).expect("read a block")).expect("decode")
            })
            .collect()
    }

    #[test]
    fn a_staged_block_is_found_by_its_parent_and_survives_reopening() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("staging.sqlite");
        let blocks = fixtures();

        {
            let staging = Staging::open(&path).expect("open");
            assert!(staging.is_empty().expect("count"));
            for block in &blocks {
                staging.put(block).expect("stage");
            }
            assert_eq!(staging.len().expect("count"), blocks.len() as u64);
        }

        // The descent is on disk, which is the whole point: a process that
        // stops mid-catch-up does not have to fetch any of it again.
        let staging = Staging::open(&path).expect("reopen");
        assert_eq!(staging.len().expect("count"), blocks.len() as u64);
        for block in &blocks {
            assert!(staging.holds(block.block_id()).expect("holds"));
            let child = staging
                .child_of(block.header.parent_block_id)
                .expect("child")
                .expect("the staged block is its parent's child");
            assert_eq!(child.block_id(), block.block_id());
        }
    }

    #[test]
    fn a_descent_resumes_below_the_lowest_block_it_reached() {
        let directory = tempfile::tempdir().expect("a directory");
        let staging = Staging::open(&directory.path().join("staging.sqlite")).expect("open");
        let blocks = fixtures();
        assert_eq!(staging.descent_resumes_at().expect("resume"), None);

        // Staged out of order on purpose: what matters is the lowest height,
        // not the order the peer answered in.
        for block in blocks.iter().rev() {
            staging.put(block).expect("stage");
        }
        let lowest = blocks
            .iter()
            .min_by_key(|block| block.header.chain_length)
            .expect("a lowest block");
        assert_eq!(
            staging.descent_resumes_at().expect("resume"),
            Some(lowest.header.parent_block_id)
        );

        for block in &blocks {
            staging.remove(block.block_id()).expect("remove");
        }
        assert!(staging.is_empty().expect("count"));
    }

    #[test]
    fn an_unrelated_parent_has_no_staged_child() {
        let directory = tempfile::tempdir().expect("a directory");
        let staging = Staging::open(&directory.path().join("staging.sqlite")).expect("open");
        staging.put(&fixtures()[0]).expect("stage");
        assert!(
            staging
                .child_of(StacksBlockId::from_bytes([0xab; 32]))
                .expect("child")
                .is_none()
        );
    }
}
