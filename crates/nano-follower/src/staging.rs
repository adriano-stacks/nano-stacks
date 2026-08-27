//! Blocks fetched from a peer but not yet executed.
//!
//! Nakamoto blocks name their parent, never their child, so reaching a peer's
//! tip from this node's own means walking backwards. Holding that walk in
//! memory makes catching up all-or-nothing: against mainnet the gap was twenty
//! thousand blocks, one rate limit anywhere in the descent discarded every
//! block fetched, and the next round started again from the peer's — by then
//! higher — tip. The executed height never moved.
//!
//! Downloads make the descent durable, but remain separate from executable
//! staging until local chainstate authenticates their exact representation.
//! A round that ends early still resumes from the lowest block it fetched.

use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    path::Path,
    sync::Mutex,
};

use nano_chainstate::{AuthenticatedBlock, NakamotoBlock, NakamotoCodecError};
use nano_primitives::{Sha256Sum, StacksBlockId, sha512_256};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

/// Why a staged block could not be read or written.
#[derive(Debug)]
pub enum StagingError {
    Storage(rusqlite::Error),
    Block(NakamotoCodecError),
    /// Two rows claim one block identifier but disagree outside signer signatures.
    RepresentationConflict(StacksBlockId),
    /// A store created by a newer binary cannot be interpreted safely.
    UnsupportedSchema(i64),
    /// A legacy executable row could not be moved behind authentication.
    LegacyRow(String),
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
            Self::RepresentationConflict(block_id) => write!(
                formatter,
                "staged block {block_id} has conflicting core representations"
            ),
            Self::UnsupportedSchema(version) => {
                write!(formatter, "unsupported staging schema version {version}")
            }
            Self::LegacyRow(error) => write!(
                formatter,
                "legacy staged row cannot be quarantined before execution: {error}"
            ),
            Self::Poisoned => formatter.write_str("the staging store was poisoned"),
            Self::Incoherent => formatter.write_str("the staged block graph is incoherent"),
        }
    }
}

/// What keeping a block changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StagingInsert {
    Inserted,
    Identical,
    /// The same block core arrived with another signer certificate.
    AdditionalCertificate,
}

impl std::error::Error for StagingError {}

impl From<rusqlite::Error> for StagingError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error)
    }
}

/// Downloaded and locally authenticated blocks waiting above the executed tip.
///
/// The connection is guarded because a follower holds this store across the
/// awaits it spends fetching; every method locks for the length of one
/// statement and none of them await.
#[derive(Debug)]
pub struct Staging {
    connection: Mutex<Connection>,
    selected: Mutex<Option<Vec<StagedLink>>>,
    quarantined: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StagedLink {
    block_id: StacksBlockId,
    parent: StacksBlockId,
    height: u64,
}

const SCHEMA_VERSION: i64 = 2;

impl Staging {
    /// The guarded connection, or the poisoning that made it unusable.
    fn connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StagingError> {
        self.connection.lock().map_err(|_| StagingError::Poisoned)
    }

    /// Open, creating the store when it is not there yet.
    pub fn open(path: &Path) -> Result<Self, StagingError> {
        let mut connection = Connection::open(path)?;
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 30000;",
        )?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version =
            transaction.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?;
        if !matches!(version, 0 | SCHEMA_VERSION) {
            return Err(StagingError::UnsupportedSchema(version));
        }
        let had_staged = table_exists(&transaction, "staged")?;
        if version == 0 {
            if had_staged {
                migrate_legacy_staging(&transaction)?;
            } else {
                create_schema(&transaction)?;
                transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
        }
        validate_schema(&transaction)?;
        let quarantined = quarantined_rows(&transaction)?;
        transaction.commit()?;
        Ok(Self {
            connection: Mutex::new(connection),
            selected: Mutex::new(None),
            quarantined,
        })
    }

    /// Legacy rows retained in quarantine outside executable staging.
    #[must_use]
    pub const fn quarantined_rows(&self) -> u64 {
        self.quarantined
    }

    fn invalidate_selection(&self) -> Result<(), StagingError> {
        *self.selected.lock().map_err(|_| StagingError::Poisoned)? = None;
        Ok(())
    }

    /// Keep the cached branch when this block's arrival cannot have changed it.
    ///
    /// Re-selecting reads every staged and downloaded row and rebuilds the branch
    /// from the tip down, which on a deep catch-up is a six-figure row scan. The
    /// execution loop pays it for a block it already holds: `put` of the
    /// authenticated form of a block the branch already names changes bytes and
    /// nothing about the topology.
    fn keep_selection_unless_new(&self, link: StagedLink) -> Result<(), StagingError> {
        let mut selected = self.selected.lock().map_err(|_| StagingError::Poisoned)?;
        let unchanged = selected
            .as_ref()
            .is_some_and(|branch| branch.contains(&link));
        if !unchanged {
            *selected = None;
        }
        drop(selected);
        Ok(())
    }

    /// Drop the branch's lowest block from the cache after it is executed.
    ///
    /// Equivalent to re-selecting: the tip a re-selection would pick is
    /// unchanged, and the walk down from it stops where the parent is no longer
    /// held — which is exactly this element removed. Any other removal
    /// invalidates, because it can change which tip wins or cut the branch in
    /// the middle.
    fn forget_selected_head(&self, block_id: StacksBlockId) -> Result<(), StagingError> {
        let mut selected = self.selected.lock().map_err(|_| StagingError::Poisoned)?;
        match selected.as_mut() {
            Some(branch) if branch.first().is_some_and(|head| head.block_id == block_id) => {
                branch.remove(0);
                if branch.is_empty() {
                    *selected = None;
                }
            }
            _ => *selected = None,
        }
        drop(selected);
        Ok(())
    }

    /// Keep an authenticated block for later execution without letting another
    /// signer representation overwrite its core.
    pub fn put(&self, block: &AuthenticatedBlock) -> Result<StagingInsert, StagingError> {
        let block = block.block();
        let block_id = block.block_id();
        let bytes = block.encode();
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::check_stored_representations(&transaction, block_id, block)?;
        let existing = transaction
            .query_row(
                "SELECT bytes FROM staged WHERE block_id = ?1",
                params![block_id.as_bytes().as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        let outcome = if let Some(existing_bytes) = existing {
            match bytes.cmp(&existing_bytes) {
                std::cmp::Ordering::Equal => StagingInsert::Identical,
                std::cmp::Ordering::Less => {
                    transaction.execute(
                        "UPDATE staged SET parent_block_id = ?2, height = ?3, bytes = ?4
                         WHERE block_id = ?1",
                        params![
                            block_id.as_bytes().as_slice(),
                            block.header.parent_block_id.as_bytes().as_slice(),
                            block.header.chain_length,
                            bytes,
                        ],
                    )?;
                    StagingInsert::AdditionalCertificate
                }
                std::cmp::Ordering::Greater => StagingInsert::AdditionalCertificate,
            }
        } else {
            transaction.execute(
                "INSERT INTO staged (block_id, parent_block_id, height, bytes)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    block_id.as_bytes().as_slice(),
                    block.header.parent_block_id.as_bytes().as_slice(),
                    block.header.chain_length,
                    bytes,
                ],
            )?;
            StagingInsert::Inserted
        };
        transaction.commit()?;
        self.keep_selection_unless_new(StagedLink {
            block_id,
            parent: block.header.parent_block_id,
            height: block.header.chain_length,
        })?;
        drop(connection);
        Ok(outcome)
    }

    /// Keep an untrusted download outside executable staging until chainstate
    /// authenticates its exact representation.
    pub fn download(&self, block: &NakamotoBlock) -> Result<StagingInsert, StagingError> {
        let block_id = block.block_id();
        let bytes = block.encode();
        let representation_id = sha512_256(&bytes);
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let had_representation = Self::check_stored_representations(&transaction, block_id, block)?;
        let existing = transaction
            .query_row(
                "SELECT bytes FROM downloaded WHERE representation_id = ?1",
                params![representation_id.as_bytes().as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        let outcome = if let Some(existing) = existing {
            if existing != bytes {
                return Err(StagingError::RepresentationConflict(block_id));
            }
            StagingInsert::Identical
        } else {
            transaction.execute(
                "INSERT INTO downloaded
                 (representation_id, block_id, parent_block_id, height, bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    representation_id.as_bytes().as_slice(),
                    block_id.as_bytes().as_slice(),
                    block.header.parent_block_id.as_bytes().as_slice(),
                    block.header.chain_length,
                    bytes,
                ],
            )?;
            if had_representation {
                StagingInsert::AdditionalCertificate
            } else {
                StagingInsert::Inserted
            }
        };
        transaction.commit()?;
        self.keep_selection_unless_new(StagedLink {
            block_id,
            parent: block.header.parent_block_id,
            height: block.header.chain_length,
        })?;
        drop(connection);
        Ok(outcome)
    }

    fn check_stored_representations(
        transaction: &Transaction<'_>,
        block_id: StacksBlockId,
        block: &NakamotoBlock,
    ) -> Result<bool, StagingError> {
        let mut statement = transaction.prepare(
            "SELECT parent_block_id, height, bytes FROM staged WHERE block_id = ?1
             UNION ALL
             SELECT parent_block_id, height, bytes FROM downloaded WHERE block_id = ?1",
        )?;
        let rows = statement.query_map(params![block_id.as_bytes().as_slice()], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        let mut found = false;
        let expected_core = block_core_digest(block);
        for row in rows {
            let (stored_parent, stored_height, bytes) = row?;
            let stored = NakamotoBlock::decode(&bytes).map_err(StagingError::Block)?;
            if stored_parent != stored.header.parent_block_id.as_bytes().as_slice()
                || stored_height != stored.header.chain_length
                || block_core_digest(&stored) != expected_core
            {
                return Err(StagingError::RepresentationConflict(block_id));
            }
            found = true;
        }
        Ok(found)
    }

    /// Whether any representation of this block has already been acquired.
    pub fn has_representation(&self, block_id: StacksBlockId) -> Result<bool, StagingError> {
        Ok(self
            .connection()?
            .query_row(
                "SELECT 1 FROM staged WHERE block_id = ?1
                 UNION ALL
                 SELECT 1 FROM downloaded WHERE block_id = ?1
                 LIMIT 1",
                params![block_id.as_bytes().as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .is_some())
    }

    /// Whether a locally authenticated representation can enter execution.
    pub fn is_authenticated(&self, block_id: StacksBlockId) -> Result<bool, StagingError> {
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
        // The database lock precedes the cache lock on reads and writes, so a
        // committed mutation cannot race a stale branch back into the cache.
        let connection = self.connection()?;
        let mut selected = self.selected.lock().map_err(|_| StagingError::Poisoned)?;
        if let Some(branch) = selected.as_ref() {
            return Ok(branch.clone());
        }
        let mut statement = connection.prepare(
            "SELECT block_id, parent_block_id, height FROM staged
             UNION
             SELECT block_id, parent_block_id, height FROM downloaded",
        )?;
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
            let parent =
                StacksBlockId::from_bytes(parent.try_into().map_err(|_| StagingError::Incoherent)?);
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
        let mut visited = BTreeSet::from([tip.block_id]);
        while let Some(parent) = blocks.get(
            &branch
                .last()
                .expect("the selected branch starts with its tip")
                .parent,
        ) {
            if !visited.insert(parent.block_id) {
                return Err(StagingError::Incoherent);
            }
            branch.push(*parent);
        }
        branch.reverse();
        *selected = Some(branch.clone());
        drop(selected);
        drop(connection);
        Ok(branch)
    }

    fn blocks(&self, block_id: StacksBlockId) -> Result<Vec<NakamotoBlock>, StagingError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT bytes FROM (
                 SELECT bytes, 0 AS priority FROM staged WHERE block_id = ?1
                 UNION ALL
                 SELECT bytes, 1 AS priority FROM downloaded WHERE block_id = ?1
             ) GROUP BY bytes ORDER BY MIN(priority), bytes",
        )?;
        let rows = statement.query_map(params![block_id.as_bytes().as_slice()], |row| {
            row.get::<_, Vec<u8>>(0)
        })?;
        let blocks = rows
            .map(|row| {
                let bytes = row?;
                NakamotoBlock::decode(&bytes).map_err(StagingError::Block)
            })
            .collect();
        drop(statement);
        drop(connection);
        blocks
    }

    fn block(&self, block_id: StacksBlockId) -> Result<Option<NakamotoBlock>, StagingError> {
        Ok(self.blocks(block_id)?.into_iter().next())
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
        Ok(self.child_representations(parent)?.into_iter().next())
    }

    /// Every representation of the selected child, authenticated rows first.
    pub fn child_representations(
        &self,
        parent: StacksBlockId,
    ) -> Result<Vec<NakamotoBlock>, StagingError> {
        let selected = self
            .selected_branch()?
            .into_iter()
            .find(|block| block.parent == parent);
        selected.map_or_else(|| Ok(Vec::new()), |block| self.blocks(block.block_id))
    }

    /// Forget a block, which is what sealing it makes right.
    pub fn remove(&self, block_id: StacksBlockId) -> Result<(), StagingError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM staged WHERE block_id = ?1",
            params![block_id.as_bytes().as_slice()],
        )?;
        transaction.execute(
            "DELETE FROM downloaded WHERE block_id = ?1",
            params![block_id.as_bytes().as_slice()],
        )?;
        transaction.commit()?;
        self.forget_selected_head(block_id)?;
        drop(connection);
        Ok(())
    }

    /// Forget several blocks at once, which the per-round trim needs.
    ///
    /// One transaction and one selection invalidation rather than a pair of each
    /// per block: the trim names every block the executed chain has overtaken,
    /// which is bounded by how far a retraction reaches and so is hundreds of
    /// blocks on every round.
    pub fn remove_all(&self, block_ids: &[StacksBlockId]) -> Result<(), StagingError> {
        if block_ids.is_empty() {
            return Ok(());
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for block_id in block_ids {
            transaction.execute(
                "DELETE FROM staged WHERE block_id = ?1",
                params![block_id.as_bytes().as_slice()],
            )?;
            transaction.execute(
                "DELETE FROM downloaded WHERE block_id = ?1",
                params![block_id.as_bytes().as_slice()],
            )?;
        }
        transaction.commit()?;
        self.invalidate_selection()?;
        drop(connection);
        Ok(())
    }

    /// Forget a rejected block and every staged block that descends from it.
    pub fn remove_branch(&self, root: StacksBlockId) -> Result<usize, StagingError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let block_ids = {
            let mut statement = transaction.prepare(
                "WITH RECURSIVE
                 links(block_id, parent_block_id) AS (
                     SELECT block_id, parent_block_id FROM staged
                     UNION
                     SELECT block_id, parent_block_id FROM downloaded
                 ),
                 branch(block_id) AS (
                     SELECT block_id FROM links WHERE block_id = ?1
                     UNION
                     SELECT links.block_id
                     FROM links JOIN branch ON links.parent_block_id = branch.block_id
                 )
                 SELECT block_id FROM branch",
            )?;
            statement
                .query_map(params![root.as_bytes().as_slice()], |row| {
                    row.get::<_, Vec<u8>>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for block_id in &block_ids {
            transaction.execute("DELETE FROM staged WHERE block_id = ?1", params![block_id])?;
            transaction.execute(
                "DELETE FROM downloaded WHERE block_id = ?1",
                params![block_id],
            )?;
        }
        transaction.commit()?;
        self.invalidate_selection()?;
        drop(connection);
        Ok(block_ids.len())
    }

    /// Forget everything at or below a height, which sealing past it makes
    /// dead weight — and which a descent that overshot leaves behind.
    pub fn remove_to(&self, height: u64) -> Result<usize, StagingError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let removed = transaction.query_row(
            "SELECT count(*) FROM (
                 SELECT block_id FROM staged WHERE height <= ?1
                 UNION
                 SELECT block_id FROM downloaded WHERE height <= ?1
             )",
            params![height],
            |row| row.get::<_, usize>(0),
        )?;
        transaction.execute("DELETE FROM staged WHERE height <= ?1", params![height])?;
        transaction.execute("DELETE FROM downloaded WHERE height <= ?1", params![height])?;
        transaction.commit()?;
        self.invalidate_selection()?;
        drop(connection);
        Ok(removed)
    }

    /// Drop everything, which a fork or a corrupt descent calls for.
    pub fn clear(&self) -> Result<(), StagingError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute("DELETE FROM staged", [])?;
        transaction.execute("DELETE FROM downloaded", [])?;
        transaction.commit()?;
        self.invalidate_selection()?;
        drop(connection);
        Ok(())
    }

    /// How many blocks are waiting.
    pub fn len(&self) -> Result<u64, StagingError> {
        Ok(self.connection()?.query_row(
            "SELECT count(*) FROM (
                 SELECT block_id FROM staged
                 UNION
                 SELECT block_id FROM downloaded
             )",
            [],
            |row| row.get(0),
        )?)
    }

    /// Whether nothing is waiting.
    pub fn is_empty(&self) -> Result<bool, StagingError> {
        Ok(self.len()? == 0)
    }
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, StagingError> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![table],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some())
}

fn create_schema(connection: &Connection) -> Result<(), StagingError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS staged (
             block_id BLOB PRIMARY KEY,
             parent_block_id BLOB NOT NULL,
             height INTEGER NOT NULL,
             bytes BLOB NOT NULL
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS staged_parent ON staged (parent_block_id);
         CREATE INDEX IF NOT EXISTS staged_height ON staged (height);
         CREATE TABLE IF NOT EXISTS downloaded (
             representation_id BLOB PRIMARY KEY,
             block_id BLOB NOT NULL,
             parent_block_id BLOB NOT NULL,
             height INTEGER NOT NULL,
             bytes BLOB NOT NULL
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS downloaded_block ON downloaded (block_id);
         CREATE INDEX IF NOT EXISTS downloaded_parent ON downloaded (parent_block_id);
         CREATE INDEX IF NOT EXISTS downloaded_height ON downloaded (height);",
    )?;
    Ok(())
}

fn migrate_legacy_staging(connection: &Connection) -> Result<(), StagingError> {
    if table_exists(connection, "quarantined_staged_v1")? {
        return Err(StagingError::LegacyRow(
            "quarantined_staged_v1 already exists while staged still uses version 1".to_owned(),
        ));
    }
    connection.execute_batch(
        "ALTER TABLE staged RENAME TO quarantined_staged_v1;
         DROP INDEX IF EXISTS staged_parent;
         DROP INDEX IF EXISTS staged_height;",
    )?;
    create_schema(connection)?;
    connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

fn quarantined_rows(connection: &Connection) -> Result<u64, StagingError> {
    if !table_exists(connection, "quarantined_staged_v1")? {
        return Ok(0);
    }
    Ok(
        connection.query_row("SELECT count(*) FROM quarantined_staged_v1", [], |row| {
            row.get(0)
        })?,
    )
}

fn validate_schema(connection: &Connection) -> Result<(), StagingError> {
    let mut cores = BTreeMap::<StacksBlockId, Sha256Sum>::new();
    for table in ["staged", "downloaded"] {
        let query = format!("SELECT block_id, parent_block_id, height, bytes FROM {table}");
        let mut statement = connection.prepare(&query)?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })?;
        for row in rows {
            let (stored_id, stored_parent, stored_height, bytes) = row?;
            let block = NakamotoBlock::decode(&bytes).map_err(|error| {
                StagingError::LegacyRow(format!("{table} contains an undecodable block: {error}"))
            })?;
            let block_id = block.block_id();
            if stored_id.as_slice() != block_id.as_bytes().as_slice()
                || stored_parent.as_slice() != block.header.parent_block_id.as_bytes().as_slice()
                || stored_height != block.header.chain_length
            {
                return Err(StagingError::LegacyRow(format!(
                    "{table} row {} disagrees with its encoded block",
                    hex::encode(stored_id)
                )));
            }
            if table == "staged" && block.header.signer_signatures.is_empty() {
                return Err(StagingError::LegacyRow(format!(
                    "authenticated staging row {block_id} has no signer certificate"
                )));
            }
            let core = block_core_digest(&block);
            match cores.entry(block_id) {
                Entry::Vacant(entry) => {
                    entry.insert(core);
                }
                Entry::Occupied(entry) if entry.get() != &core => {
                    return Err(StagingError::RepresentationConflict(block_id));
                }
                Entry::Occupied(_) => {}
            }
        }
    }
    let mut statement = connection.prepare("SELECT representation_id, bytes FROM downloaded")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    for row in rows {
        let (stored, bytes) = row?;
        if stored.as_slice() != sha512_256(&bytes).as_bytes().as_slice() {
            return Err(StagingError::LegacyRow(
                "a downloaded representation identifier does not hash its bytes".to_owned(),
            ));
        }
    }
    Ok(())
}

fn block_core(block: &NakamotoBlock) -> Vec<u8> {
    let mut core = block.clone();
    core.header.signer_signatures.clear();
    core.encode()
}

fn block_core_digest(block: &NakamotoBlock) -> Sha256Sum {
    sha512_256(&block_core(block))
}

#[cfg(test)]
mod tests {
    use super::{Staging, StagingError, StagingInsert};
    use nano_chainstate::{NakamotoBlock, Signer, SignerSet};
    use nano_crypto::StacksPrivateKey;
    use nano_primitives::{ConsensusHash, StacksBlockId};
    use rusqlite::{Connection, params};
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

    fn independent_certificates() -> [NakamotoBlock; 2] {
        let mut first = fixtures().remove(0);
        let mut second = first.clone();
        let first_signer = StacksPrivateKey::from_seed(b"first independent signer");
        let second_signer = StacksPrivateKey::from_seed(b"second independent signer");
        let third_signer = StacksPrivateKey::from_seed(b"third independent signer");
        let set = SignerSet::new(vec![
            Signer {
                public_key: first_signer.public_key(),
                weight: 4,
            },
            Signer {
                public_key: second_signer.public_key(),
                weight: 3,
            },
            Signer {
                public_key: third_signer.public_key(),
                weight: 3,
            },
        ])
        .expect("signer set");
        let digest = first.header.signer_signature_hash();
        first.header.signer_signatures = vec![
            first_signer.sign(digest.as_bytes()),
            second_signer.sign(digest.as_bytes()),
        ];
        second.header.signer_signatures = vec![
            first_signer.sign(digest.as_bytes()),
            third_signer.sign(digest.as_bytes()),
        ];
        set.verify(&first.header).expect("first threshold subset");
        set.verify(&second.header).expect("second threshold subset");
        [first, second]
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
                staging.download(block).expect("stage");
            }
            assert_eq!(staging.len().expect("count"), blocks.len() as u64);
        }

        // The descent is on disk, which is the whole point: a process that
        // stops mid-catch-up does not have to fetch any of it again.
        let staging = Staging::open(&path).expect("reopen");
        assert_eq!(staging.len().expect("count"), blocks.len() as u64);
        for block in &blocks {
            assert!(
                staging
                    .has_representation(block.block_id())
                    .expect("contains")
            );
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
            staging.download(block).expect("stage");
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

    /// The cached branch says what a fresh selection would say, block by block.
    ///
    /// The execution loop asks for the branch, puts the authenticated form of the
    /// block it just took, executes it and removes it — three cache operations a
    /// block, and re-selecting on each reads every staged and downloaded row. The
    /// two the loop performs update the cache in place instead, and this is the
    /// proof they are equivalent: every step is compared against a store reopened
    /// from the same file, which has no cache at all.
    #[test]
    fn the_cached_branch_matches_a_reselection_at_every_step() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("staging.sqlite");
        let staging = Staging::open(&path).expect("open");
        let blocks = fixtures();
        for block in &blocks {
            staging.download(block).expect("download");
        }

        let reselected = |parent| {
            Staging::open(&path)
                .expect("reopen")
                .child_representations(parent)
                .expect("children")
                .into_iter()
                .map(|block| block.block_id())
                .collect::<Vec<_>>()
        };
        let cached = |parent| {
            staging
                .child_representations(parent)
                .expect("children")
                .into_iter()
                .map(|block| block.block_id())
                .collect::<Vec<_>>()
        };

        let mut parent = blocks[0].header.parent_block_id;
        for block in &blocks {
            assert_eq!(cached(parent), reselected(parent), "before the put");
            assert_eq!(cached(parent), vec![block.block_id()]);
            staging.remove(block.block_id()).expect("remove");
            parent = block.block_id();
            assert_eq!(cached(parent), reselected(parent), "after the remove");
        }
        assert!(staging.is_empty().expect("count"));
    }

    /// A block the branch does not hold still re-selects.
    #[test]
    fn a_block_the_branch_does_not_hold_reselects() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("staging.sqlite");
        let staging = Staging::open(&path).expect("open");
        let blocks = fixtures();
        staging.download(&blocks[0]).expect("download the first");
        // Warm the cache on a branch of one.
        assert_eq!(
            staging
                .child_representations(blocks[0].header.parent_block_id)
                .expect("children")
                .len(),
            1
        );
        // The second block extends it, and the cache must notice.
        staging.download(&blocks[1]).expect("download the second");
        assert_eq!(
            staging
                .child_representations(blocks[0].block_id())
                .expect("children")
                .into_iter()
                .map(|block| block.block_id())
                .collect::<Vec<_>>(),
            vec![blocks[1].block_id()]
        );
    }

    #[test]
    fn an_unrelated_parent_has_no_staged_child() {
        let directory = tempfile::tempdir().expect("a directory");
        let staging = Staging::open(&directory.path().join("staging.sqlite")).expect("open");
        staging.download(&fixtures()[0]).expect("stage");
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
            staging.download(block).expect("stage original branch");
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
            staging.download(block).expect("stage replacement branch");
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

    #[test]
    fn rejecting_a_block_prunes_only_its_descendant_branch() {
        let directory = tempfile::tempdir().expect("a directory");
        let staging = Staging::open(&directory.path().join("staging.sqlite")).expect("open");
        let blocks = fixtures();
        let rejected = blocks[1..].to_vec();
        let mut sibling = blocks[1..].to_vec();
        for index in 0..sibling.len() {
            sibling[index].header.consensus_hash = ConsensusHash::from_bytes([0x97; 20]);
            sibling[index].header.parent_block_id = if index == 0 {
                blocks[0].block_id()
            } else {
                sibling[index - 1].block_id()
            };
        }
        for block in rejected.iter().chain(&sibling) {
            staging.download(block).expect("stage competing branch");
        }

        assert_eq!(
            staging
                .remove_branch(rejected[0].block_id())
                .expect("prune rejected branch"),
            rejected.len()
        );
        for block in &rejected {
            assert!(
                !staging
                    .has_representation(block.block_id())
                    .expect("inspect rejected branch")
            );
        }
        for block in &sibling {
            assert!(
                staging
                    .has_representation(block.block_id())
                    .expect("inspect sibling branch")
            );
        }
    }

    #[test]
    fn unsigned_and_finalized_downloads_remain_separate_until_authentication() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("staging.sqlite");
        let staging = Staging::open(&path).expect("open");
        let signed = fixtures().remove(0);
        assert!(!signed.header.signer_signatures.is_empty());
        let mut unsigned = signed.clone();
        unsigned.header.signer_signatures.clear();
        let mut expected = vec![unsigned.clone(), signed.clone()];
        expected.sort_by_key(NakamotoBlock::encode);

        assert_eq!(
            staging
                .download(&unsigned)
                .expect("stage proposal representation"),
            StagingInsert::Inserted
        );
        drop(staging);
        let staging = Staging::open(&path).expect("restart after unsigned download");
        assert_eq!(
            staging
                .download(&signed)
                .expect("stage finalized representation"),
            StagingInsert::AdditionalCertificate
        );
        let connection = staging.connection().expect("inspect storage classes");
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM staged", [], |row| row
                    .get::<_, u64>(0))
                .expect("count authenticated rows"),
            0,
            "neither an unsigned proposal nor an unverified finalized download is executable"
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM downloaded", [], |row| {
                    row.get::<_, u64>(0)
                })
                .expect("count downloaded representations"),
            2
        );
        drop(connection);
        assert!(
            !staging
                .is_authenticated(signed.block_id())
                .expect("authentication state"),
            "raw same-ID representations cannot satisfy executable staging"
        );
        assert_eq!(
            staging.blocks(signed.block_id()).expect("read downloads"),
            expected
        );
        assert_eq!(
            staging
                .download(&unsigned)
                .expect("repeat unsigned representation"),
            StagingInsert::Identical
        );
        drop(staging);
        assert_eq!(
            Staging::open(&path)
                .expect("restart after finalized download")
                .blocks(signed.block_id())
                .expect("read downloads after restart"),
            expected
        );

        let reverse = Staging::open(&directory.path().join("reverse.sqlite")).expect("open");
        reverse
            .download(&signed)
            .expect("download finalized representation first");
        reverse
            .download(&unsigned)
            .expect("download unsigned representation second");
        drop(reverse);
        let reverse = Staging::open(&directory.path().join("reverse.sqlite")).expect("restart");
        assert_eq!(
            reverse
                .blocks(signed.block_id())
                .expect("read reverse insertion order"),
            expected
        );
        assert!(!reverse.is_authenticated(signed.block_id()).expect("state"));
    }

    #[test]
    fn additional_certificates_remain_distinct_and_restart_deterministically() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("staging.sqlite");
        let staging = Staging::open(&path).expect("open");
        let [first, second] = independent_certificates();
        let mut expected = vec![first.clone(), second.clone()];
        expected.sort_by_key(NakamotoBlock::encode);

        assert_eq!(
            staging
                .download(&first)
                .expect("download first certificate"),
            StagingInsert::Inserted
        );
        drop(staging);
        let staging = Staging::open(&path).expect("restart after first certificate");
        assert_eq!(
            staging
                .block(first.block_id())
                .expect("read first certificate after restart"),
            Some(first.clone())
        );
        assert_eq!(
            staging
                .download(&second)
                .expect("download second certificate"),
            StagingInsert::AdditionalCertificate
        );
        drop(staging);
        let staging = Staging::open(&path).expect("restart after second certificate");
        assert!(
            !staging
                .is_authenticated(first.block_id())
                .expect("authentication state"),
            "valid signatures are not local burn and signer-set authentication"
        );
        assert_eq!(
            staging
                .blocks(first.block_id())
                .expect("read downloaded certificates"),
            expected
        );
        assert_eq!(
            staging.download(&first).expect("repeat chosen certificate"),
            StagingInsert::Identical
        );
        drop(staging);
        assert_eq!(
            Staging::open(&path)
                .expect("reopen")
                .blocks(first.block_id())
                .expect("read reopened certificates"),
            expected
        );

        let reverse = Staging::open(&directory.path().join("reverse.sqlite")).expect("open");
        reverse
            .download(&second)
            .expect("stage second certificate first");
        reverse
            .download(&first)
            .expect("stage first certificate second");
        assert_eq!(
            reverse
                .blocks(first.block_id())
                .expect("read reverse insertion"),
            expected
        );
    }

    #[test]
    fn a_row_whose_bytes_disagree_with_its_identifier_is_refused() {
        let directory = tempfile::tempdir().expect("a directory");
        let staging = Staging::open(&directory.path().join("staging.sqlite")).expect("open");
        let blocks = fixtures();
        let expected = &blocks[0];
        let conflicting = &blocks[1];
        staging
            .connection()
            .expect("connection")
            .execute(
                "INSERT INTO staged (block_id, parent_block_id, height, bytes)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    expected.block_id().as_bytes().as_slice(),
                    conflicting.header.parent_block_id.as_bytes().as_slice(),
                    conflicting.header.chain_length,
                    conflicting.encode(),
                ],
            )
            .expect("write conflicting row");

        assert!(matches!(
            staging.download(expected),
            Err(StagingError::RepresentationConflict(block_id))
                if block_id == expected.block_id()
        ));
    }

    #[test]
    fn a_row_whose_index_disagrees_with_its_bytes_is_refused() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("staging.sqlite");
        let staging = Staging::open(&path).expect("open");
        let block = fixtures().remove(0);
        let wrong_parent = [0x44_u8; 32];
        staging
            .connection()
            .expect("connection")
            .execute(
                "INSERT INTO staged (block_id, parent_block_id, height, bytes)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    block.block_id().as_bytes().as_slice(),
                    wrong_parent.as_slice(),
                    block.header.chain_length.saturating_add(1),
                    block.encode(),
                ],
            )
            .expect("write incoherent row");

        assert!(matches!(
            staging.download(&block),
            Err(StagingError::RepresentationConflict(block_id))
                if block_id == block.block_id()
        ));
        drop(staging);
        assert!(matches!(
            Staging::open(&path),
            Err(StagingError::LegacyRow(error)) if error.contains("disagrees with its encoded block")
        ));
    }

    #[test]
    fn a_downloaded_representation_identifier_must_hash_its_exact_bytes() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("staging.sqlite");
        let staging = Staging::open(&path).expect("open");
        let block = fixtures().remove(0);
        staging.download(&block).expect("download block");
        staging
            .connection()
            .expect("connection")
            .execute(
                "UPDATE downloaded SET representation_id = ?1",
                params![[0x44_u8; 32].as_slice()],
            )
            .expect("corrupt representation identifier");
        drop(staging);

        assert!(matches!(
            Staging::open(&path),
            Err(StagingError::LegacyRow(error))
                if error.contains("does not hash its bytes")
        ));
    }

    #[test]
    fn a_legacy_store_is_quarantined_before_any_row_can_be_selected() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("staging.sqlite");
        let block = fixtures().remove(0);
        let connection = Connection::open(&path).expect("create legacy store");
        connection
            .execute_batch(
                "CREATE TABLE staged (
                     block_id BLOB PRIMARY KEY,
                     parent_block_id BLOB NOT NULL,
                     height INTEGER NOT NULL,
                     bytes BLOB NOT NULL
                 ) WITHOUT ROWID;
                 CREATE INDEX staged_parent ON staged (parent_block_id);
                 CREATE INDEX staged_height ON staged (height);",
            )
            .expect("create legacy schema");
        connection
            .execute(
                "INSERT INTO staged (block_id, parent_block_id, height, bytes)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    block.block_id().as_bytes().as_slice(),
                    block.header.parent_block_id.as_bytes().as_slice(),
                    block.header.chain_length,
                    block.encode(),
                ],
            )
            .expect("write legacy row");
        drop(connection);

        let staging = Staging::open(&path).expect("migrate legacy store");
        assert_eq!(staging.quarantined_rows(), 1);
        assert!(staging.is_empty().expect("new staging is empty"));
        assert_eq!(
            staging
                .connection()
                .expect("connection")
                .query_row("SELECT count(*) FROM quarantined_staged_v1", [], |row| {
                    row.get::<_, u64>(0)
                })
                .expect("count quarantined rows"),
            1
        );
        drop(staging);

        let reopened = Staging::open(&path).expect("reopen migrated store");
        assert_eq!(reopened.quarantined_rows(), 1);
        assert!(reopened.is_empty().expect("no legacy row is selected"));
    }

    #[test]
    fn a_newer_staging_schema_is_refused() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("staging.sqlite");
        Connection::open(&path)
            .expect("create store")
            .execute_batch("PRAGMA user_version = 3;")
            .expect("set future version");

        assert!(matches!(
            Staging::open(&path),
            Err(StagingError::UnsupportedSchema(3))
        ));
    }

    #[test]
    fn a_damaged_current_schema_is_refused_without_being_recreated() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("staging.sqlite");
        Connection::open(&path)
            .expect("create store")
            .pragma_update(None, "user_version", super::SCHEMA_VERSION)
            .expect("set current version without its tables");

        assert!(matches!(
            Staging::open(&path),
            Err(StagingError::Storage(_))
        ));
        let connection = Connection::open(&path).expect("inspect refused store");
        assert!(!super::table_exists(&connection, "staged").expect("inspect staged"));
        assert!(!super::table_exists(&connection, "downloaded").expect("inspect downloads"));
    }

    #[test]
    fn an_unsigned_row_in_authenticated_staging_is_refused_on_restart() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("staging.sqlite");
        drop(Staging::open(&path).expect("create current store"));
        let mut block = fixtures().remove(0);
        block.header.signer_signatures.clear();
        Connection::open(&path)
            .expect("open store directly")
            .execute(
                "INSERT INTO staged (block_id, parent_block_id, height, bytes)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    block.block_id().as_bytes().as_slice(),
                    block.header.parent_block_id.as_bytes().as_slice(),
                    block.header.chain_length,
                    block.encode(),
                ],
            )
            .expect("write corrupt authenticated row");

        assert!(matches!(
            Staging::open(&path),
            Err(StagingError::LegacyRow(error)) if error.contains("no signer certificate")
        ));
    }
}
