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

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Mutex,
};

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
    /// The staged parent graph does not form a finite branch.
    Incoherent,
}

impl std::fmt::Display for StagingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "staging storage: {error}"),
            Self::Block(error) => write!(formatter, "staged block: {error}"),
            Self::Poisoned => formatter.write_str("the staging store was poisoned"),
            Self::Incoherent => formatter.write_str("the staged block graph is incoherent"),
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
    selected: Mutex<Option<Vec<StagedLink>>>,
}

#[derive(Clone, Copy, Debug)]
struct StagedLink {
    block_id: StacksBlockId,
    parent: StacksBlockId,
    height: u64,
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
            selected: Mutex::new(None),
        })
    }

    fn invalidate_selection(&self) -> Result<(), StagingError> {
        *self.selected.lock().map_err(|_| StagingError::Poisoned)? = None;
        Ok(())
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
        self.invalidate_selection()?;
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

    /// The longest coherent staged branch, with block-id ordering as a stable tie-break.
    fn selected_branch(&self) -> Result<Vec<StagedLink>, StagingError> {
        let cached = {
            self.selected
                .lock()
                .map_err(|_| StagingError::Poisoned)?
                .clone()
        };
        if let Some(branch) = cached {
            return Ok(branch);
        }
        let (blocks, parents) = {
            let connection = self.connection()?;
            let mut statement =
                connection.prepare("SELECT block_id, parent_block_id, height FROM staged")?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, u64>(2)?,
                ))
            })?;
            let mut blocks = BTreeMap::new();
            let mut parents = BTreeSet::new();
            for row in rows {
                let (block_id, parent, height) = row?;
                let block_id = StacksBlockId::from_bytes(
                    block_id.try_into().map_err(|_| StagingError::Incoherent)?,
                );
                let parent = StacksBlockId::from_bytes(
                    parent.try_into().map_err(|_| StagingError::Incoherent)?,
                );
                parents.insert(parent);
                blocks.insert(
                    block_id,
                    StagedLink {
                        block_id,
                        parent,
                        height,
                    },
                );
            }
            drop(statement);
            drop(connection);
            (blocks, parents)
        };
        let Some(tip) = blocks
            .values()
            .filter(|candidate| !parents.contains(&candidate.block_id))
            .max_by_key(|block| (block.height, block.block_id))
            .copied()
        else {
            return if blocks.is_empty() {
                Ok(Vec::new())
            } else {
                Err(StagingError::Incoherent)
            };
        };

        let mut branch = vec![tip];
        while let Some(parent) = blocks.get(
            &branch
                .last()
                .expect("the selected branch starts with its tip")
                .parent,
        ) {
            if branch.iter().any(|block| block.block_id == parent.block_id) {
                return Err(StagingError::Incoherent);
            }
            branch.push(*parent);
        }
        branch.reverse();
        *self.selected.lock().map_err(|_| StagingError::Poisoned)? = Some(branch.clone());
        Ok(branch)
    }

    fn block(&self, block_id: StacksBlockId) -> Result<Option<NakamotoBlock>, StagingError> {
        self.connection()?
            .query_row(
                "SELECT bytes FROM staged WHERE block_id = ?1",
                params![block_id.as_bytes().as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
            .map(|bytes| NakamotoBlock::decode(&bytes).map_err(StagingError::Block))
            .transpose()
    }

    /// The parent of the lowest staged block, which is where a descent resumes,
    /// and that block's own height.
    ///
    /// Staging may retain siblings; the selected longest linked branch decides
    /// which lowest block and parent belong together.
    ///
    /// The height comes back with it because the parent alone cannot say whether
    /// there is anything left to fetch. A tenure arrives whole, so the answer that
    /// reached the executed tip staged blocks below it too, and the lowest of
    /// those points at a tenure this node has already sealed — which a caller
    /// would then ask for again on every round.
    pub fn descent_resumes_at(&self) -> Result<Option<(StacksBlockId, u64)>, StagingError> {
        Ok(self
            .selected_branch()?
            .first()
            .map(|block| (block.parent, block.height)))
    }

    /// The highest staged block, which is the furthest this node has *acquired*.
    ///
    /// Asked by the forward download schedule, and the distinction from the executed
    /// tip is what keeps it from re-downloading. A schedule anchored at the executed
    /// tip re-derives almost the same window every round — the tip moves by a tenure
    /// while the window is dozens long — so it would ask for the tenures it fetched
    /// last round and throw the answers on top of themselves. Anchored here it asks
    /// for what comes next.
    pub fn highest(&self) -> Result<Option<NakamotoBlock>, StagingError> {
        let Some(highest) = self.selected_branch()?.last().copied() else {
            return Ok(None);
        };
        self.block(highest.block_id)
    }

    /// The staged block whose parent is `parent`, if it is here.
    pub fn child_of(&self, parent: StacksBlockId) -> Result<Option<NakamotoBlock>, StagingError> {
        let selected = self
            .selected_branch()?
            .into_iter()
            .find(|block| block.parent == parent);
        selected.map_or(Ok(None), |block| self.block(block.block_id))
    }

    /// Forget a block, which is what sealing it makes right.
    pub fn remove(&self, block_id: StacksBlockId) -> Result<(), StagingError> {
        self.connection()?.execute(
            "DELETE FROM staged WHERE block_id = ?1",
            params![block_id.as_bytes().as_slice()],
        )?;
        {
            let mut selected = self.selected.lock().map_err(|_| StagingError::Poisoned)?;
            if let Some(branch) = selected.as_mut() {
                branch.retain(|block| block.block_id != block_id);
            }
            if selected.as_ref().is_some_and(Vec::is_empty) {
                *selected = None;
            }
        }
        Ok(())
    }

    /// Forget everything at or below a height, which sealing past it makes
    /// dead weight — and which a descent that overshot leaves behind.
    pub fn remove_to(&self, height: u64) -> Result<usize, StagingError> {
        let removed = self
            .connection()?
            .execute("DELETE FROM staged WHERE height <= ?1", params![height])?;
        {
            let mut selected = self.selected.lock().map_err(|_| StagingError::Poisoned)?;
            if let Some(branch) = selected.as_mut() {
                branch.retain(|block| block.height > height);
            }
            if selected.as_ref().is_some_and(Vec::is_empty) {
                *selected = None;
            }
        }
        Ok(removed)
    }

    /// Drop everything, which a fork or a corrupt descent calls for.
    pub fn clear(&self) -> Result<(), StagingError> {
        self.connection()?.execute("DELETE FROM staged", [])?;
        self.invalidate_selection()?;
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
    use nano_primitives::{ConsensusHash, StacksBlockId};
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
            Some((lowest.header.parent_block_id, lowest.header.chain_length))
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

    #[test]
    fn competing_siblings_select_one_coherent_longest_branch() {
        let directory = tempfile::tempdir().expect("a directory");
        let staging = Staging::open(&directory.path().join("staging.sqlite")).expect("open");
        let blocks = fixtures();
        let agreed = blocks[0].block_id();
        for block in &blocks[1..] {
            staging.put(block).expect("stage original branch");
        }

        let mut branch = blocks[1..].to_vec();
        branch[0].header.parent_block_id = agreed;
        branch[0].header.consensus_hash = ConsensusHash::from_bytes([0x96; 20]);
        for index in 1..branch.len() {
            branch[index].header.parent_block_id = branch[index - 1].block_id();
            branch[index].header.consensus_hash = ConsensusHash::from_bytes([0x96; 20]);
        }
        let mut extension = branch.last().expect("branch tip").clone();
        extension.header.parent_block_id = branch.last().expect("branch tip").block_id();
        extension.header.chain_length = extension.header.chain_length.saturating_add(1);
        branch.push(extension);
        for block in &branch {
            staging.put(block).expect("stage replacement branch");
        }

        assert_eq!(
            staging.descent_resumes_at().expect("resume"),
            Some((agreed, branch[0].header.chain_length))
        );
        assert_eq!(
            staging
                .highest()
                .expect("highest")
                .expect("selected tip")
                .block_id(),
            branch.last().expect("branch tip").block_id()
        );
        let mut parent = agreed;
        for expected in branch {
            let child = staging
                .child_of(parent)
                .expect("child")
                .expect("selected branch is linked");
            assert_eq!(child.block_id(), expected.block_id());
            parent = child.block_id();
        }
    }
}
