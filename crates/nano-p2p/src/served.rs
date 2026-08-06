//! What this node will tell a peer it has, and can still say after a restart.
//!
//! A `GetNakamotoInv` reply is a bit per tenure of a reward cycle: set means "I have
//! processed every block of that tenure and will serve it", unset means "do not ask
//! me". Nano derives the truthful answer from its executed ledger, and that answer
//! is bounded in a way the protocol is not: `ChainLedger`'s executed suffix reaches
//! `REORG_REACH = 256` blocks back, so a cycle of 2,100 tenures is answered at its
//! recent end and nowhere else. Restarting makes it no smaller and no larger — the
//! vector is derived afresh each round from the same 256-block window.
//!
//! This is the missing half: the bits accumulate. Each round records what the ledger
//! can see into one row per cycle, OR-ed into whatever was recorded before, so a node
//! that has walked a whole cycle can answer for the whole cycle — including across a
//! restart, which is the task's "persist enough authenticated canonical block data to
//! answer peer inventory and block requests after a restart".
//!
//! ## Why here and not in the chainstate
//!
//! The complete answer wants a consensus-hash-to-tenure index, and `block_header` in
//! the side store is keyed by block id, so there is no way to ask "did I run the
//! tenure at this consensus hash" without a scan. Adding such an index is a
//! consensus-store change, and this is not one: nothing here is read by execution,
//! nothing here can change what nano accepts, and a corrupt or deleted file costs
//! exactly the inventory answer it holds. A file whose worst failure is a peer asking
//! somebody else does not belong in the store whose worst failure is a fork.
//!
//! ## Only bits the ledger asserted
//!
//! Nothing is recorded that the executed ledger did not report in the round that
//! recorded it, so a bit here was true of nano's canonical chain when it was written.
//! A tenure could in principle be reorged away afterwards, which would leave a bit
//! nano cannot honour — but the ledger only reports tenures it has *sealed*, and a
//! reorg deeper than the window nano itself can reorganize is one this node would not
//! survive either. The cost of the residual case is a peer's failed fetch, which is
//! the same cost as any peer that has pruned.

use std::path::Path;

use nano_primitives::{BitVec, ConsensusHash};
use rusqlite::{Connection, OptionalExtension, params};

use crate::peers::PeerDbError;

/// The tenures this node has executed, by reward cycle, on disk.
pub struct ServedTenures {
    connection: Connection,
}

impl ServedTenures {
    /// Open or create the store at `path`.
    pub fn open(path: &Path) -> Result<Self, PeerDbError> {
        Self::from_connection(Connection::open(path)?)
    }

    /// A store that lives only as long as the process, for tests.
    pub fn in_memory() -> Result<Self, PeerDbError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(connection: Connection) -> Result<Self, PeerDbError> {
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS served_tenures (
                 cycle_start BLOB PRIMARY KEY,
                 tenures BLOB NOT NULL
             );",
        )?;
        Ok(Self { connection })
    }

    /// Fold what the ledger can see this round into what is already recorded, and
    /// say how many tenures are claimed afterwards.
    ///
    /// A union rather than a replacement, which is the whole point: the round's own
    /// answer shrinks as the executed window slides forward, and a replacement would
    /// make nano forget a tenure it really did run the moment its blocks left the
    /// window.
    pub fn record(
        &self,
        cycle_start: ConsensusHash,
        tenures: &BitVec<2100>,
    ) -> Result<u16, PeerDbError> {
        let merged = match self.inventory(cycle_start)? {
            Some(known) => union(&known, tenures),
            None => tenures.clone(),
        };
        self.connection.execute(
            "INSERT INTO served_tenures (cycle_start, tenures) VALUES (?1, ?2)
             ON CONFLICT (cycle_start) DO UPDATE SET tenures = ?2",
            params![&cycle_start.as_bytes()[..], merged.wire_bytes()],
        )?;
        Ok(claimed(&merged))
    }

    /// What this node has recorded for a cycle, or `None` for one it has never seen.
    ///
    /// `None` becomes a `Nack`, which is the honest answer and the one a stock node
    /// reads as "ask somebody else".
    pub fn inventory(
        &self,
        cycle_start: ConsensusHash,
    ) -> Result<Option<BitVec<2100>>, PeerDbError> {
        let stored: Option<Vec<u8>> = self
            .connection
            .query_row(
                "SELECT tenures FROM served_tenures WHERE cycle_start = ?1",
                params![&cycle_start.as_bytes()[..]],
                |row| row.get(0),
            )
            .optional()?;
        // A row that will not decode is one this node wrote and cannot read, which is
        // a corrupt file rather than a peer's problem. Treated as "not known", so the
        // reply is a nack and the next round writes a good row over it.
        Ok(stored
            .as_deref()
            .and_then(|bytes| BitVec::<2100>::from_wire_bytes(bytes).ok()))
    }

    /// How many cycles are recorded, for an operator who wants to know whether this
    /// node is useful to its peers yet.
    pub fn cycles(&self) -> Result<usize, PeerDbError> {
        let count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM served_tenures", [], |row| row.get(0))?;
        Ok(usize::try_from(count).unwrap_or(0))
    }
}

/// Both vectors' set bits, at the longer of the two lengths.
///
/// A bit is only ever added. Clearing one would need a reason this store cannot
/// have: it records what nano ran, and nano does not un-run a tenure.
fn union(left: &BitVec<2100>, right: &BitVec<2100>) -> BitVec<2100> {
    let len = left.len().max(right.len());
    let mut merged = BitVec::<2100>::zeros(len).unwrap_or_else(|_| left.clone());
    for index in 0..len {
        let set = left.get(index) == Some(true) || right.get(index) == Some(true);
        // Out of bounds cannot happen: `len` came from the vector being written.
        let _ = merged.set(index, set);
    }
    merged
}

fn claimed(tenures: &BitVec<2100>) -> u16 {
    (0..tenures.len())
        .filter(|index| tenures.get(*index) == Some(true))
        .count()
        .try_into()
        .unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CYCLE: ConsensusHash = ConsensusHash::from_bytes([0x2c; 20]);

    fn tenures(set: &[u16]) -> BitVec<2100> {
        let mut tenures = BitVec::<2100>::zeros(2100).expect("a cycle-length vector");
        for &index in set {
            tenures.set(index, true).expect("in bounds");
        }
        tenures
    }

    /// A cycle nobody has recorded is nacked, not answered with zeroes.
    #[test]
    fn an_unknown_cycle_is_not_answered() {
        let served = ServedTenures::in_memory().expect("a store");
        assert!(served.inventory(CYCLE).expect("a query").is_none());
        assert_eq!(served.cycles().expect("a count"), 0);
    }

    /// Rounds accumulate: the window slides and the answer grows.
    ///
    /// This is the whole reason the store exists. The executed ledger reaches 256
    /// blocks back, so round two cannot see what round one saw, and a store that
    /// replaced instead of merging would answer for the last few tenures forever.
    #[test]
    fn what_the_window_saw_is_not_forgotten_when_it_slides() {
        let served = ServedTenures::in_memory().expect("a store");
        assert_eq!(served.record(CYCLE, &tenures(&[0, 1, 2])).expect("record"), 3);
        // The next round's window has moved on and no longer mentions 0..=2.
        assert_eq!(served.record(CYCLE, &tenures(&[3, 4])).expect("record"), 5);
        let answer = served.inventory(CYCLE).expect("a query").expect("recorded");
        for index in 0..5 {
            assert_eq!(answer.get(index), Some(true), "tenure {index} was run");
        }
        assert_eq!(answer.get(5), Some(false), "and nothing more is claimed");
    }

    /// Reopening the same file answers the same question.
    #[test]
    fn a_restart_can_still_answer() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("served.sqlite");
        {
            let served = ServedTenures::open(&path).expect("a store");
            served.record(CYCLE, &tenures(&[7, 2099])).expect("record");
        }
        let reopened = ServedTenures::open(&path).expect("the same store");
        let answer = reopened.inventory(CYCLE).expect("a query").expect("recorded");
        assert_eq!(answer.get(7), Some(true));
        assert_eq!(answer.get(2099), Some(true));
        assert_eq!(answer.get(8), Some(false));
        assert_eq!(reopened.cycles().expect("a count"), 1);
    }

    /// Cycles are kept apart, because a bit index means nothing without one.
    #[test]
    fn one_cycles_tenures_are_not_anothers() {
        let served = ServedTenures::in_memory().expect("a store");
        let other = ConsensusHash::from_bytes([0x77; 20]);
        served.record(CYCLE, &tenures(&[1])).expect("record");
        served.record(other, &tenures(&[2])).expect("record");
        assert_eq!(
            served.inventory(CYCLE).expect("a query").expect("recorded").get(2),
            Some(false)
        );
        assert_eq!(
            served.inventory(other).expect("a query").expect("recorded").get(1),
            Some(false)
        );
        assert_eq!(served.cycles().expect("a count"), 2);
    }
}
