//! What this node knows about other nodes, across restarts.
//!
//! stacks-core's `PeerDB` is ten tables and three thousand lines because it also
//! stores the local peer's identity, ASN allocations, `StackerDB` memberships
//! and a frontier organised for the neighbour walk. Nano needs one question —
//! *who should this node try next, and who has been wasting its time* — so this
//! is one table.
//!
//! Two things are kept apart on purpose:
//!
//! * A key **hash** learned from gossip is a third party's claim, and is stored
//!   as a hint. A key learned from a handshake is proof, and overwrites it.
//! * Failures are counted, never fatal. A peer that stops answering earns a
//!   growing backoff and stays in the table, because the reason is far more often
//!   a restart than dishonesty, and forgetting it would leave a small network
//!   with nowhere to go.

use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nano_primitives::Hash160;
use rusqlite::{Connection, OptionalExtension, params};

use crate::wire::{Handshake, NeighborAddress, PeerAddress};

/// How many peers to remember.
///
/// A neighbour walk learns 128 addresses per reply, so an unbounded table is a
/// peer-supplied disk write; beyond a few thousand the extra rows are ones this
/// node will never get round to dialling anyway.
pub const MAX_KNOWN_PEERS: usize = 4096;

/// The shortest wait after a peer fails, doubling per consecutive failure.
const BASE_BACKOFF: Duration = Duration::from_secs(30);

/// The longest that doubling is allowed to reach. A peer that has been down for
/// an hour is still worth one attempt an hour: mainnet peers restart.
const MAX_BACKOFF: Duration = Duration::from_hours(1);

/// The failure count [`PeerDb::isolate`] records, chosen to saturate the backoff.
///
/// Written as a count rather than as a flag so that a peer which then answers is
/// forgiven by the same code path as any other — `record_handshake` clears it, and
/// there is no second kind of forgiveness to get wrong.
const ISOLATION_FAILURES: u32 = 16;

/// A peer this node knows of, and how well that has gone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KnownPeer {
    pub address: PeerAddress,
    pub port: u16,
    /// The peer's node key, once a handshake has proved it.
    pub public_key: Option<[u8; 33]>,
    /// The key hash a *third* peer claimed for this one. A hint for recognising
    /// the peer, never a reason to trust a message.
    pub public_key_hash: Option<Hash160>,
    pub peer_version: u32,
    pub network_id: u32,
    pub services: u16,
    pub data_url: String,
    /// When a handshake with this peer last succeeded, in seconds since the
    /// epoch, or `None` if one never has.
    pub last_seen: Option<u64>,
    pub consecutive_failures: u32,
}

impl KnownPeer {
    #[must_use]
    pub fn socket_addr(&self) -> SocketAddr {
        self.address.to_socket_addr(self.port)
    }

    /// Whether this peer is worth dialling at `now`.
    #[must_use]
    pub fn is_due(&self, last_failed: Option<u64>, now: u64) -> bool {
        let Some(last_failed) = last_failed else {
            return true;
        };
        // The first failure waits one base period, the second two, and so on, so
        // the exponent is one less than the count.
        let doublings = self.consecutive_failures.saturating_sub(1);
        let backoff = BASE_BACKOFF
            .saturating_mul(1_u32.checked_shl(doublings).unwrap_or(u32::MAX))
            .min(MAX_BACKOFF);
        last_failed.saturating_add(backoff.as_secs()) <= now
    }
}

#[derive(Debug)]
pub enum PeerDbError {
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for PeerDbError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "peer database failed: {error}"),
        }
    }
}

impl std::error::Error for PeerDbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
        }
    }
}

impl From<rusqlite::Error> for PeerDbError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// The peers this node knows, on disk.
pub struct PeerDb {
    connection: Connection,
}

impl PeerDb {
    /// Open or create the database at `path`.
    pub fn open(path: &Path) -> Result<Self, PeerDbError> {
        Self::from_connection(Connection::open(path)?)
    }

    /// A database that lives only as long as the process, for tests.
    pub fn in_memory() -> Result<Self, PeerDbError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(connection: Connection) -> Result<Self, PeerDbError> {
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS peers (
                 address BLOB NOT NULL,
                 port INTEGER NOT NULL,
                 public_key BLOB,
                 public_key_hash BLOB,
                 peer_version INTEGER NOT NULL DEFAULT 0,
                 network_id INTEGER NOT NULL DEFAULT 0,
                 services INTEGER NOT NULL DEFAULT 0,
                 data_url TEXT NOT NULL DEFAULT '',
                 last_seen INTEGER,
                 last_failed INTEGER,
                 consecutive_failures INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (address, port)
             );",
        )?;
        Ok(Self { connection })
    }

    /// Add an address an operator configured, if it is not already known.
    ///
    /// A seed is not evidence of anything beyond the operator's intent, so it
    /// never overwrites what a handshake has established: restarting with a seed
    /// list must not throw away the keys the last run proved.
    pub fn seed(&self, address: PeerAddress, port: u16) -> Result<(), PeerDbError> {
        self.connection.execute(
            "INSERT OR IGNORE INTO peers (address, port) VALUES (?1, ?2)",
            params![&address.as_bytes()[..], port],
        )?;
        Ok(())
    }

    /// Record the addresses a peer gossiped.
    ///
    /// Only unknown peers are inserted, and only the key *hash* is taken, because
    /// everything in a `Neighbors` reply is one peer's claim about another.
    pub fn learn(&self, neighbors: &[NeighborAddress]) -> Result<usize, PeerDbError> {
        let mut learned = 0;
        for neighbor in neighbors {
            learned += self.connection.execute(
                "INSERT OR IGNORE INTO peers (address, port, public_key_hash)
                 VALUES (?1, ?2, ?3)",
                params![
                    &neighbor.address.as_bytes()[..],
                    neighbor.port,
                    &neighbor.public_key_hash.as_bytes()[..],
                ],
            )?;
        }
        self.prune()?;
        Ok(learned)
    }

    /// Record a completed handshake, which is the only thing that proves a key.
    pub fn record_handshake(
        &self,
        address: PeerAddress,
        port: u16,
        handshake: &Handshake,
        peer_version: u32,
        network_id: u32,
    ) -> Result<(), PeerDbError> {
        self.connection.execute(
            "INSERT INTO peers (
                 address, port, public_key, public_key_hash, peer_version, network_id,
                 services, data_url, last_seen, last_failed, consecutive_failures
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, 0)
             ON CONFLICT (address, port) DO UPDATE SET
                 public_key = excluded.public_key,
                 public_key_hash = excluded.public_key_hash,
                 peer_version = excluded.peer_version,
                 network_id = excluded.network_id,
                 services = excluded.services,
                 data_url = excluded.data_url,
                 last_seen = excluded.last_seen,
                 last_failed = NULL,
                 consecutive_failures = 0",
            params![
                &address.as_bytes()[..],
                port,
                &handshake.public_key[..],
                &nano_primitives::hash160(&handshake.public_key).as_bytes()[..],
                peer_version,
                network_id,
                handshake.services,
                &handshake.data_url,
                now_stored(),
            ],
        )?;
        Ok(())
    }

    /// Set a peer aside for breaking the protocol.
    ///
    /// This is the longest penalty the table can express — the backoff ceiling,
    /// one attempt an hour — and deliberately not a permanent ban. A malformed
    /// message is far more often a version skew or somebody's bug than malice, and
    /// a node that bans permanently on protocol errors bans the network one
    /// deployment at a time. What it does buy is that a peer serving garbage stops
    /// occupying one of a handful of session slots.
    pub fn isolate(&self, address: PeerAddress, port: u16) -> Result<(), PeerDbError> {
        self.connection.execute(
            "UPDATE peers SET last_failed = ?3, consecutive_failures = ?4
              WHERE address = ?1 AND port = ?2",
            params![
                &address.as_bytes()[..],
                port,
                now_stored(),
                ISOLATION_FAILURES
            ],
        )?;
        Ok(())
    }

    /// Record that a peer did not work, which earns it a longer wait rather than
    /// removal.
    pub fn record_failure(&self, address: PeerAddress, port: u16) -> Result<(), PeerDbError> {
        self.connection.execute(
            "UPDATE peers
                SET last_failed = ?3, consecutive_failures = consecutive_failures + 1
              WHERE address = ?1 AND port = ?2",
            params![&address.as_bytes()[..], port, now_stored()],
        )?;
        Ok(())
    }

    /// The peers worth dialling now, best first.
    ///
    /// The order is: fewest consecutive failures, then most recently seen, then
    /// never-tried peers, then address. It is fully determined by what this node
    /// has observed, so no peer can promote itself by answering in a particular
    /// order — the same reasoning as `PeerPool`'s deterministic tip ranking in
    /// [027].
    pub fn candidates(&self, limit: usize) -> Result<Vec<KnownPeer>, PeerDbError> {
        let now = now();
        let mut statement = self.connection.prepare(
            "SELECT address, port, public_key, public_key_hash, peer_version, network_id,
                    services, data_url, last_seen, last_failed, consecutive_failures
               FROM peers
              ORDER BY consecutive_failures ASC, last_seen DESC, address ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((read_peer(row)?, row.get::<_, Option<i64>>(9)?))
        })?;
        let mut candidates = Vec::new();
        for row in rows {
            let (peer, last_failed) = row?;
            let last_failed = last_failed.and_then(|seconds| u64::try_from(seconds).ok());
            if peer.is_due(last_failed, now) {
                candidates.push(peer);
                if candidates.len() >= limit {
                    break;
                }
            }
        }
        Ok(candidates)
    }

    /// What is known about one peer.
    pub fn get(&self, address: PeerAddress, port: u16) -> Result<Option<KnownPeer>, PeerDbError> {
        Ok(self
            .connection
            .query_row(
                "SELECT address, port, public_key, public_key_hash, peer_version, network_id,
                        services, data_url, last_seen, last_failed, consecutive_failures
                   FROM peers WHERE address = ?1 AND port = ?2",
                params![&address.as_bytes()[..], port],
                read_peer,
            )
            .optional()?)
    }

    pub fn count(&self) -> Result<usize, PeerDbError> {
        let count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM peers", [], |row| row.get(0))?;
        Ok(usize::try_from(count).unwrap_or(usize::MAX))
    }

    /// Keep the table bounded, dropping the peers least worth keeping.
    ///
    /// A peer that has never answered goes before one that has, however long ago:
    /// the whole value of the table across a restart is the peers that were real.
    fn prune(&self) -> Result<(), PeerDbError> {
        self.connection.execute(
            "DELETE FROM peers WHERE rowid IN (
                 SELECT rowid FROM peers
                  ORDER BY (last_seen IS NULL) ASC, last_seen DESC,
                           consecutive_failures ASC, address ASC
                  LIMIT -1 OFFSET ?1
             )",
            params![i64::try_from(MAX_KNOWN_PEERS).unwrap_or(i64::MAX)],
        )?;
        Ok(())
    }
}

fn read_peer(row: &rusqlite::Row<'_>) -> rusqlite::Result<KnownPeer> {
    let address: Vec<u8> = row.get(0)?;
    let public_key: Option<Vec<u8>> = row.get(2)?;
    let public_key_hash: Option<Vec<u8>> = row.get(3)?;
    Ok(KnownPeer {
        address: PeerAddress::from_bytes(
            address.try_into().unwrap_or([0; 16]),
        ),
        port: row.get(1)?,
        public_key: public_key.and_then(|bytes| bytes.try_into().ok()),
        public_key_hash: public_key_hash
            .and_then(|bytes| <[u8; 20]>::try_from(bytes).ok())
            .map(Hash160::from_bytes),
        peer_version: row.get(4)?,
        network_id: row.get(5)?,
        services: row.get(6)?,
        data_url: row.get(7)?,
        last_seen: row
            .get::<_, Option<i64>>(8)?
            .and_then(|seconds| u64::try_from(seconds).ok()),
        consecutive_failures: row.get(10)?,
    })
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Seconds since the epoch as sqlite stores them.
fn now_stored() -> i64 {
    i64::try_from(now()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(last: u8) -> PeerAddress {
        PeerAddress::from_ip(std::net::IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, last)))
    }

    fn handshake(key: u8) -> Handshake {
        Handshake {
            address: address(key),
            port: 20444,
            services: 0x03,
            public_key: [key; 33],
            expire_bitcoin_height: 1_000_000,
            data_url: format!("http://203.0.113.{key}:20443"),
        }
    }

    /// A seed never overwrites what a handshake proved.
    ///
    /// Restarting with a seed list must not throw away the keys the last run
    /// established, because those are the only ones this node has evidence for.
    #[test]
    fn a_seed_does_not_overwrite_a_proved_key() {
        let peers = PeerDb::in_memory().expect("a table");
        peers
            .record_handshake(address(1), 20444, &handshake(7), 0x1800_0010, 1)
            .expect("record");
        peers.seed(address(1), 20444).expect("seed");
        let known = peers.get(address(1), 20444).expect("read").expect("present");
        assert_eq!(known.public_key, Some([7; 33]));
        assert_eq!(known.services, 0x03);
        assert_eq!(known.peer_version, 0x1800_0010);
        assert!(known.last_seen.is_some());
    }

    /// Gossip contributes addresses and hints, never keys.
    ///
    /// A `Neighbors` reply is one peer's claim about a third party; taking the key
    /// hash as a hint is useful, and taking it as proof would let any peer decide
    /// who nano thinks another peer is.
    #[test]
    fn gossip_contributes_hints_and_not_keys() {
        let peers = PeerDb::in_memory().expect("a table");
        let learned = peers
            .learn(&[
                NeighborAddress {
                    address: address(2),
                    port: 20444,
                    public_key_hash: Hash160::from_bytes([2; 20]),
                },
                NeighborAddress {
                    address: address(3),
                    port: 20444,
                    public_key_hash: Hash160::from_bytes([3; 20]),
                },
            ])
            .expect("learn");
        assert_eq!(learned, 2);
        let known = peers.get(address(2), 20444).expect("read").expect("present");
        assert_eq!(known.public_key, None);
        assert_eq!(known.public_key_hash, Some(Hash160::from_bytes([2; 20])));
        assert!(known.last_seen.is_none());

        // Hearing about a peer we have already handshaked with cannot demote it.
        peers
            .record_handshake(address(2), 20444, &handshake(9), 0x1800_0010, 1)
            .expect("record");
        assert_eq!(
            peers.learn(&[NeighborAddress {
                address: address(2),
                port: 20444,
                public_key_hash: Hash160::from_bytes([0xaa; 20]),
            }])
            .expect("learn"),
            0
        );
        let known = peers.get(address(2), 20444).expect("read").expect("present");
        assert_eq!(known.public_key, Some([9; 33]));
    }

    /// A failing peer waits longer each time, and a working one is forgiven.
    #[test]
    fn failures_earn_a_growing_backoff_and_a_handshake_clears_it() {
        let peers = PeerDb::in_memory().expect("a table");
        peers.seed(address(4), 20444).expect("seed");
        assert_eq!(peers.candidates(10).expect("candidates").len(), 1);

        peers.record_failure(address(4), 20444).expect("fail");
        // Still inside the first backoff window, so not yet worth dialling.
        assert!(peers.candidates(10).expect("candidates").is_empty());
        let known = peers.get(address(4), 20444).expect("read").expect("present");
        assert_eq!(known.consecutive_failures, 1);
        // A failure is never fatal: the peer stays known, and comes back when its
        // wait is up.
        assert!(known.is_due(Some(now() - BASE_BACKOFF.as_secs()), now()));
        assert!(!known.is_due(Some(now()), now()));

        peers.record_failure(address(4), 20444).expect("fail");
        let known = peers.get(address(4), 20444).expect("read").expect("present");
        assert_eq!(known.consecutive_failures, 2);
        // Doubling: one backoff period is no longer enough.
        assert!(!known.is_due(Some(now() - BASE_BACKOFF.as_secs()), now()));
        // And it never grows past the ceiling, so a long-dead peer is still tried
        // once an hour rather than never again.
        let mut forever = known;
        forever.consecutive_failures = u32::MAX;
        assert!(forever.is_due(Some(now() - MAX_BACKOFF.as_secs()), now()));

        peers
            .record_handshake(address(4), 20444, &handshake(4), 0x1800_0010, 1)
            .expect("record");
        let known = peers.get(address(4), 20444).expect("read").expect("present");
        assert_eq!(known.consecutive_failures, 0);
        assert_eq!(peers.candidates(10).expect("candidates").len(), 1);
    }

    /// Candidate order comes from what this node observed, never from the order a
    /// peer answered in.
    #[test]
    fn candidates_are_ordered_by_our_own_evidence() {
        let peers = PeerDb::in_memory().expect("a table");
        for last in 10..=12 {
            peers.seed(address(last), 20444).expect("seed");
        }
        peers
            .record_handshake(address(11), 20444, &handshake(11), 0x1800_0010, 1)
            .expect("record");
        peers.record_failure(address(10), 20444).expect("fail");
        let candidates = peers.candidates(10).expect("candidates");
        // 11 handshaked, 12 is untried, and 10 is inside its backoff.
        assert_eq!(
            candidates.iter().map(|peer| peer.address).collect::<Vec<_>>(),
            vec![address(11), address(12)]
        );
    }

    /// The table stays bounded, and drops the peers there is least reason to keep.
    #[test]
    fn the_table_is_bounded_and_keeps_the_peers_that_answered() {
        let peers = PeerDb::in_memory().expect("a table");
        peers
            .record_handshake(address(200), 20444, &handshake(200), 0x1800_0010, 1)
            .expect("record");
        // More gossip than the table holds, in one reply-sized batch at a time.
        let mut gossip = Vec::new();
        for index in 0..=u32::try_from(MAX_KNOWN_PEERS).expect("bound fits u32") + 100 {
            let octets = index.to_be_bytes();
            gossip.push(NeighborAddress {
                address: PeerAddress::from_ip(std::net::IpAddr::V4(
                    std::net::Ipv4Addr::new(10, octets[1], octets[2], octets[3]),
                )),
                port: 20444,
                public_key_hash: Hash160::from_bytes([0; 20]),
            });
        }
        peers.learn(&gossip).expect("learn");
        assert_eq!(peers.count().expect("count"), MAX_KNOWN_PEERS);
        // The one peer that actually answered survived the flood.
        assert!(
            peers
                .get(address(200), 20444)
                .expect("read")
                .is_some_and(|peer| peer.public_key == Some([200; 33]))
        );
    }
}
