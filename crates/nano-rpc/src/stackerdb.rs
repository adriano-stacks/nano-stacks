//! The `StackerDB` replicas a node serves over `/v2/stackerdb/...`.
//!
//! A signer reads its miner's proposals and writes its responses through these
//! three routes and nothing else, so a node that does not replicate chunks
//! cannot host a signer at all.
//!
//! Replication is only as trustworthy as the slot-to-writer map a node builds
//! from the reward set: a chunk is accepted when it is signed by the key that
//! owns its slot and it is newer than what the slot holds.

use std::collections::BTreeMap;

use nano_primitives::Hash160;
use nano_stackerdb::Chunk;

/// Signed chunks retained across all locally configured replicas.
pub const MAX_RETAINED_CHUNKS: usize = 16_384;

/// Chunk payload bytes retained across all locally configured replicas.
pub const MAX_RETAINED_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StackerDbLimits {
    pub chunks: usize,
    pub bytes: usize,
}

impl StackerDbLimits {
    #[must_use]
    pub const fn new(chunks: usize, bytes: usize) -> Self {
        assert!(chunks > 0, "a StackerDB store must retain a chunk");
        assert!(bytes > 0, "a StackerDB store must retain chunk bytes");
        Self { chunks, bytes }
    }
}

impl Default for StackerDbLimits {
    fn default() -> Self {
        Self::new(MAX_RETAINED_CHUNKS, MAX_RETAINED_BYTES)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StackerDbStatus {
    pub chunks: usize,
    pub bytes: usize,
    pub chunk_limit: usize,
    pub byte_limit: usize,
    pub saturations: u64,
}

/// Why a node refused a chunk (`net/api/poststackerdbchunk.rs`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChunkRefusal {
    DataAlreadyExists,
    NoSuchSlot,
    BadSigner,
    Capacity,
}

impl ChunkRefusal {
    #[must_use]
    pub const fn code(self) -> u32 {
        match self {
            Self::DataAlreadyExists => 0,
            Self::NoSuchSlot => 1,
            Self::BadSigner => 2,
            Self::Capacity => 3,
        }
    }

    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::DataAlreadyExists => "Data for this slot and version already exist",
            Self::NoSuchSlot => "No such StackerDB slot",
            Self::BadSigner => "Signature does not match slot signer",
            Self::Capacity => "The local StackerDB replica is at capacity",
        }
    }
}

/// One replicated contract: who may write each slot, and what each slot holds.
#[derive(Clone, Debug, Default)]
struct Replica {
    writers: Vec<Hash160>,
    slots: Vec<Option<Chunk>>,
}

/// The `StackerDB` contracts a node replicates.
#[derive(Clone, Debug)]
pub struct StackerDbStore {
    contracts: BTreeMap<String, Replica>,
    limits: StackerDbLimits,
    chunks: usize,
    bytes: usize,
    saturations: u64,
}

impl Default for StackerDbStore {
    fn default() -> Self {
        Self::with_limits(StackerDbLimits::default())
    }
}

impl StackerDbStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub const fn with_limits(limits: StackerDbLimits) -> Self {
        Self {
            contracts: BTreeMap::new(),
            limits,
            chunks: 0,
            bytes: 0,
            saturations: 0,
        }
    }

    #[must_use]
    pub const fn status(&self) -> StackerDbStatus {
        StackerDbStatus {
            chunks: self.chunks,
            bytes: self.bytes,
            chunk_limit: self.limits.chunks,
            byte_limit: self.limits.bytes,
            saturations: self.saturations,
        }
    }

    /// Replicate a contract whose slots belong, in order, to these writers.
    ///
    /// Reconfiguring a contract clears it, because a reward cycle rollover
    /// reassigns every slot and the chunks the old set wrote are unreadable
    /// under the new one.
    pub fn configure(&mut self, contract_id: &str, writers: Vec<Hash160>) {
        let slots = vec![None; writers.len()];
        if let Some(prior) = self
            .contracts
            .insert(contract_id.to_owned(), Replica { writers, slots })
        {
            self.chunks -= prior.slots.iter().flatten().count();
            self.bytes -= prior
                .slots
                .iter()
                .flatten()
                .map(|chunk| chunk.data.len())
                .sum::<usize>();
        }
    }

    /// The signed metadata of every slot, which is what a replica compares
    /// against to decide what it is missing.
    #[must_use]
    pub fn metadata(&self, contract_id: &str) -> Option<Vec<nano_stackerdb::SlotMetadata>> {
        let replica = self.contracts.get(contract_id)?;
        Some(
            replica
                .slots
                .iter()
                .enumerate()
                .map(|(slot, chunk)| {
                    chunk.as_ref().map_or_else(
                        || {
                            nano_stackerdb::SlotMetadata::unsigned(
                                u32::try_from(slot).unwrap_or(u32::MAX),
                                0,
                                nano_primitives::Sha256Sum::default(),
                            )
                        },
                        Chunk::metadata,
                    )
                })
                .collect(),
        )
    }

    /// The data a slot holds, at a given version or at whatever is latest.
    #[must_use]
    pub fn chunk(&self, contract_id: &str, slot: u32, version: Option<u32>) -> Option<&[u8]> {
        let held = self
            .contracts
            .get(contract_id)?
            .slots
            .get(usize::try_from(slot).ok()?)?
            .as_ref()?;
        version
            .is_none_or(|version| version == held.slot_version)
            .then_some(held.data.as_slice())
    }

    /// Accept a chunk into its slot, or say why this node will not.
    pub fn put(&mut self, contract_id: &str, chunk: Chunk) -> Result<(), ChunkRefusal> {
        let Self {
            contracts,
            limits,
            chunks,
            bytes,
            saturations,
        } = self;
        let replica = contracts
            .get_mut(contract_id)
            .ok_or(ChunkRefusal::NoSuchSlot)?;
        let slot = usize::try_from(chunk.slot_id).map_err(|_| ChunkRefusal::NoSuchSlot)?;
        let writer = *replica.writers.get(slot).ok_or(ChunkRefusal::NoSuchSlot)?;
        if !chunk.verify(writer).unwrap_or(false) {
            return Err(ChunkRefusal::BadSigner);
        }
        let held = replica
            .slots
            .get_mut(slot)
            .ok_or(ChunkRefusal::NoSuchSlot)?;
        // The version is a Lamport clock, so a repeat or a rewind is refused.
        if held
            .as_ref()
            .is_some_and(|held| chunk.slot_version <= held.slot_version)
        {
            return Err(ChunkRefusal::DataAlreadyExists);
        }
        let prior_bytes = held.as_ref().map_or(0, |held| held.data.len());
        let retained_chunks = *chunks + usize::from(held.is_none());
        let retained_bytes = *bytes - prior_bytes;
        if retained_chunks > limits.chunks
            || chunk.data.len() > limits.bytes.saturating_sub(retained_bytes)
        {
            *saturations = (*saturations).saturating_add(1);
            return Err(ChunkRefusal::Capacity);
        }
        *chunks = retained_chunks;
        *bytes = retained_bytes + chunk.data.len();
        *held = Some(chunk);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use nano_crypto::StacksPrivateKey;
    use nano_primitives::hash160;
    use nano_stackerdb::Chunk;

    use super::{ChunkRefusal, StackerDbLimits, StackerDbStore};

    fn writer(seed: &[u8]) -> (StacksPrivateKey, nano_primitives::Hash160) {
        let key = StacksPrivateKey::from_seed(seed);
        let hash = hash160(&key.public_key().to_bytes_compressed());
        (key, hash)
    }

    fn signed(key: &StacksPrivateKey, slot: u32, version: u32, data: &[u8]) -> Chunk {
        let mut chunk = Chunk::new(slot, version, data.to_vec());
        chunk.sign(key).expect("sign chunk");
        chunk
    }

    #[test]
    fn a_slot_takes_newer_chunks_from_its_own_writer_and_nothing_else() {
        let (owner, owner_hash) = writer(b"owner");
        let (stranger, _) = writer(b"stranger");
        let mut store = StackerDbStore::new();
        store.configure("SP0.signers-0-1", vec![owner_hash]);

        assert_eq!(
            store.put("SP0.signers-0-1", signed(&owner, 0, 1, b"first")),
            Ok(())
        );
        assert_eq!(store.chunk("SP0.signers-0-1", 0, None), Some(&b"first"[..]));
        assert_eq!(
            store.chunk("SP0.signers-0-1", 0, Some(1)),
            Some(&b"first"[..])
        );
        assert_eq!(store.chunk("SP0.signers-0-1", 0, Some(2)), None);

        assert_eq!(
            store.put("SP0.signers-0-1", signed(&owner, 0, 1, b"again")),
            Err(ChunkRefusal::DataAlreadyExists)
        );
        assert_eq!(
            store.put("SP0.signers-0-1", signed(&stranger, 0, 2, b"forged")),
            Err(ChunkRefusal::BadSigner)
        );
        assert_eq!(
            store.put("SP0.signers-0-1", signed(&owner, 1, 1, b"elsewhere")),
            Err(ChunkRefusal::NoSuchSlot)
        );
        assert_eq!(
            store.put("SP0.unknown", signed(&owner, 0, 2, b"elsewhere")),
            Err(ChunkRefusal::NoSuchSlot)
        );

        assert_eq!(
            store.put("SP0.signers-0-1", signed(&owner, 0, 2, b"second")),
            Ok(())
        );
        assert_eq!(
            store.chunk("SP0.signers-0-1", 0, None),
            Some(&b"second"[..])
        );

        let metadata = store.metadata("SP0.signers-0-1").expect("metadata");
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[0].slot_version, 2);
        assert!(metadata[0].verify(owner_hash).expect("verify"));
    }

    #[test]
    fn retained_chunks_are_bounded_by_bytes_and_reconfiguration_recovers() {
        let (owner, owner_hash) = writer(b"bounded owner");
        let mut store = StackerDbStore::with_limits(StackerDbLimits::new(2, 5));
        store.configure("SP0.signers-0-1", vec![owner_hash, owner_hash]);
        assert_eq!(
            store.put("SP0.signers-0-1", signed(&owner, 0, 1, b"abc")),
            Ok(())
        );
        assert_eq!(
            store.put("SP0.signers-0-1", signed(&owner, 1, 1, b"def")),
            Err(ChunkRefusal::Capacity)
        );
        assert_eq!(store.chunk("SP0.signers-0-1", 0, None), Some(&b"abc"[..]));
        assert_eq!(store.chunk("SP0.signers-0-1", 1, None), None);
        assert_eq!(store.status().bytes, 3);
        assert_eq!(store.status().saturations, 1);

        assert_eq!(
            store.put("SP0.signers-0-1", signed(&owner, 0, 2, b"12345")),
            Ok(())
        );
        assert_eq!(
            store.status().bytes,
            5,
            "a replacement releases prior bytes"
        );

        store.configure("SP0.signers-0-1", vec![owner_hash, owner_hash]);
        assert_eq!(store.status().chunks, 0);
        assert_eq!(store.status().bytes, 0);
        assert_eq!(
            store.put("SP0.signers-0-1", signed(&owner, 1, 1, b"def")),
            Ok(())
        );
    }

    #[test]
    fn retained_tiny_chunks_are_bounded_by_count() {
        let (owner, owner_hash) = writer(b"bounded count owner");
        let mut store = StackerDbStore::with_limits(StackerDbLimits::new(1, usize::MAX));
        store.configure("SP0.signers-0-1", vec![owner_hash, owner_hash]);
        assert_eq!(
            store.put("SP0.signers-0-1", signed(&owner, 0, 1, b"a")),
            Ok(())
        );
        assert_eq!(
            store.put("SP0.signers-0-1", signed(&owner, 1, 1, b"b")),
            Err(ChunkRefusal::Capacity)
        );
        assert_eq!(store.status().chunks, 1);
        assert_eq!(store.status().saturations, 1);
    }

    #[test]
    fn a_reconfigured_contract_keeps_nothing_the_old_set_wrote() {
        let (owner, owner_hash) = writer(b"owner");
        let mut store = StackerDbStore::new();
        store.configure("SP0.signers-0-1", vec![owner_hash]);
        store
            .put("SP0.signers-0-1", signed(&owner, 0, 1, b"cycle"))
            .expect("accept");

        store.configure("SP0.signers-0-1", vec![owner_hash, owner_hash]);
        assert_eq!(store.chunk("SP0.signers-0-1", 0, None), None);
        assert_eq!(
            store.metadata("SP0.signers-0-1").expect("metadata").len(),
            2
        );
    }
}
