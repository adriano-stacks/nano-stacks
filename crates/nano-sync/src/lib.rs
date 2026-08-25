#[cfg(feature = "mempool")]
use std::collections::HashMap;
use std::{
    collections::{BTreeSet, HashSet},
    fmt,
    hash::Hash,
    num::NonZeroUsize,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use lru::LruCache;

/// Requests this process has sent to a peer, and how long they took.
///
/// A follower's speed is either its own execution or what it waits for, and one
/// counter says which: an HTTP round trip to a hosted API costs hundreds of
/// milliseconds, so a per-block request is visible here before it is visible
/// anywhere else.
static REQUESTS: AtomicU64 = AtomicU64::new(0);
static REQUEST_NANOS: AtomicU64 = AtomicU64::new(0);

/// How many requests have been sent, and the seconds spent waiting for them.
#[must_use]
pub fn request_stats() -> (u64, f64) {
    (
        REQUESTS.load(Ordering::Relaxed),
        Duration::from_nanos(REQUEST_NANOS.load(Ordering::Relaxed)).as_secs_f64(),
    )
}

#[cfg(feature = "mempool")]
use nano_address::StacksAddress;
use nano_address::{PoxAddress, PoxAddressType32};
use nano_chainstate::{
    BitcoinBlockContext, CoinbaseSchedule, NakamotoBlock, NakamotoCodecError, Signer, SignerSet,
    SignerSetError, TenureError,
};
#[cfg(feature = "mempool")]
use nano_codec::Transaction;
use nano_crypto::{CryptoError, StacksPublicKey};
#[cfg(feature = "mempool")]
use nano_mempool::{Account, Admission, ChainTip, Mempool, Rejection};
#[cfg(feature = "mempool")]
use nano_primitives::Sha256Sum;
use nano_primitives::{
    BitcoinHeaderHash, BlockHeaderHash, ConsensusHash, Hash160, SortitionId, StacksBlockId,
};
pub use nano_sortition::{MiningCompetition, SortitionParticipant};
use reqwest::{Client, Url, header::CONTENT_TYPE};
use serde::Deserialize;
use serde_json::Value;

/// How many blocks one round will walk back from a tenure's tip.
const TENURE_WALK: usize = 32;

/// How far a round will walk to close a gap of whole tenures.
///
/// A partial walk toward the tip still leaves usable blocks behind; a partial
/// bridge leaves none, because it is walked from the far end and stops before
/// reaching anything this node holds. So it is allowed to be long, and a gap
/// beyond it means this node fell too far behind to follow.
const BRIDGE_WALK: usize = 1024;

/// How many `fork_info` pages one walk will ask a peer for.
///
/// stacks-core answers ten sortitions at a time, so this reaches three hundred
/// burn blocks back — days of burnchain, and far deeper than any fork a signer
/// set would let stand. A backstop, not a target: the walk ends as soon as it
/// reaches the bound it was given, and a peer whose chain never met this one
/// runs out of answers well before it.
const FORK_INFO_PAGES: usize = 32;

/// How long to wait out a peer's first rate limit, and how often to try again.
/// How a rate-limited peer is waited out.
///
/// Short and few on purpose: a round that gives up early still applies the
/// blocks it fetched and asks again next poll, so patience here buys nothing
/// and costs the round. Waiting minutes per block is how a follower stalls
/// with no error to show for it.
const RATE_LIMIT_WAIT: std::time::Duration = std::time::Duration::from_millis(400);
const RATE_LIMIT_RETRIES: usize = 3;
const RATE_LIMIT_CEILING: std::time::Duration = std::time::Duration::from_secs(2);
/// A bound on what a *peer* may ask this node to wait, so a hostile or broken
/// `Retry-After` cannot park a catch-up for an hour. Well above any real one.
const RETRY_AFTER_CEILING: std::time::Duration = std::time::Duration::from_mins(2);

const MIB: usize = 1024 * 1024;
/// JSON control responses, including reward and signer sets.
pub const MAX_JSON_RESPONSE_BYTES: usize = 4 * MIB;
/// One consensus-serialized block fetched by its content identifier.
pub const MAX_BLOCK_RESPONSE_BYTES: usize = 4 * MIB;
/// A raw tenure returned as concatenated consensus-serialized blocks.
pub const MAX_TENURE_RESPONSE_BYTES: usize = 64 * MIB;
/// A tenure returned inside the hex-expanded `fork_info` JSON shape.
pub const MAX_TENURE_JSON_RESPONSE_BYTES: usize = 2 * MAX_TENURE_RESPONSE_BYTES + MIB;
/// One bounded mempool page and its optional 32-byte cursor.
pub const MAX_MEMPOOL_RESPONSE_BYTES: usize = 8 * MIB + 32;
/// Full 128-transaction pages needed to fill the default 8,192-entry pool.
pub const MAX_MEMPOOL_PAGES: usize = 64;
/// A small acknowledgement to a block upload.
pub const MAX_UPLOAD_RESPONSE_BYTES: usize = 64 * 1024;
/// Content-addressed blocks retained across every peer and node role.
pub const PEER_BLOCK_CACHE_ITEMS: usize = 4096;
/// Canonical block bytes retained across every peer and node role.
pub const PEER_BLOCK_CACHE_BYTES: usize = 64 * MIB;
/// Sortitions retained across every peer and node role.
pub const PEER_SORTITION_CACHE_ITEMS: usize = 4096;
/// JSON bytes represented by retained peer sortitions.
pub const PEER_SORTITION_CACHE_BYTES: usize = 4 * MIB;

type BlockCacheKey = (Url, StacksBlockId);
type SortitionCacheKey = (Url, ConsensusHash);

static BLOCKS: OnceLock<Mutex<ByteCache<BlockCacheKey, NakamotoBlock>>> = OnceLock::new();
static SORTITIONS: OnceLock<Mutex<ByteCache<SortitionCacheKey, SortitionInfo>>> = OnceLock::new();

fn block_cache() -> &'static Mutex<ByteCache<BlockCacheKey, NakamotoBlock>> {
    BLOCKS.get_or_init(|| {
        Mutex::new(ByteCache::new(
            PEER_BLOCK_CACHE_ITEMS,
            PEER_BLOCK_CACHE_BYTES,
        ))
    })
}

fn sortition_cache() -> &'static Mutex<ByteCache<SortitionCacheKey, SortitionInfo>> {
    SORTITIONS.get_or_init(|| {
        Mutex::new(ByteCache::new(
            PEER_SORTITION_CACHE_ITEMS,
            PEER_SORTITION_CACHE_BYTES,
        ))
    })
}

#[derive(Debug)]
struct ByteCache<K: Hash + Eq, V> {
    entries: LruCache<K, (V, usize)>,
    bytes: usize,
    byte_limit: usize,
}

impl<K: Hash + Eq, V> ByteCache<K, V> {
    fn new(item_limit: usize, byte_limit: usize) -> Self {
        Self {
            entries: LruCache::new(NonZeroUsize::new(item_limit).expect("a non-zero cache limit")),
            bytes: 0,
            byte_limit,
        }
    }

    fn get(&mut self, key: &K) -> Option<&V> {
        self.entries.get(key).map(|(value, _)| value)
    }

    fn insert(&mut self, key: K, value: V, bytes: usize) -> bool {
        if bytes > self.byte_limit {
            return false;
        }
        if let Some((_, previous)) = self.entries.pop(&key) {
            self.bytes -= previous;
        }
        while self.entries.len() == self.entries.cap().get() || bytes > self.byte_limit - self.bytes
        {
            let Some((_, (_, evicted))) = self.entries.pop_lru() else {
                break;
            };
            self.bytes -= evicted;
        }
        self.entries.put(key, (value, bytes));
        self.bytes += bytes;
        true
    }

    fn stats(&self) -> CacheStats {
        CacheStats {
            items: self.entries.len(),
            bytes: self.bytes,
            item_limit: self.entries.cap().get(),
            byte_limit: self.byte_limit,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheStats {
    pub items: usize,
    pub bytes: usize,
    pub item_limit: usize,
    pub byte_limit: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerCacheStats {
    pub blocks: CacheStats,
    pub sortitions: CacheStats,
}

/// Retained peer data and the process-wide budgets that enforce it.
#[must_use]
pub fn peer_cache_stats() -> PeerCacheStats {
    PeerCacheStats {
        blocks: block_cache().lock().map_or_else(
            |poisoned| poisoned.into_inner().stats(),
            |cache| cache.stats(),
        ),
        sortitions: sortition_cache().lock().map_or_else(
            |poisoned| poisoned.into_inner().stats(),
            |cache| cache.stats(),
        ),
    }
}

#[derive(Clone, Debug)]
pub struct SyncClient {
    client: Client,
    base_url: Url,
}

/// The accounts a peer reports, as the tip a mempool judges against.
///
/// A node that follows a peer has no account index of its own until it
/// executes, so the peer's view of the tip is the one it admits against.
#[derive(Clone, Debug, Default)]
#[cfg(feature = "mempool")]
pub struct PeerAccounts(HashMap<StacksAddress, Account>);

#[cfg(feature = "mempool")]
impl ChainTip for PeerAccounts {
    fn account(&self, address: &StacksAddress) -> Account {
        self.0.account(address)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeInfo {
    pub bitcoin_height: u64,
    pub stacks_height: u64,
    pub stacks_tip: BlockHeaderHash,
    pub consensus_hash: ConsensusHash,
    pub network_id: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TenureInfo {
    pub consensus_hash: ConsensusHash,
    pub tenure_start_block_id: StacksBlockId,
    pub parent_consensus_hash: ConsensusHash,
    pub parent_tenure_start_block_id: StacksBlockId,
    pub tip_block_id: StacksBlockId,
    pub tip_height: u64,
    pub reward_cycle: u64,
}

/// Bitcoin sortition data used to authenticate a Nakamoto tenure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SortitionInfo {
    pub bitcoin_block_hash: BitcoinHeaderHash,
    pub bitcoin_height: u64,
    pub bitcoin_timestamp: u64,
    pub sortition_id: SortitionId,
    pub parent_sortition_id: SortitionId,
    pub consensus_hash: ConsensusHash,
    pub was_sortition: bool,
    pub miner_public_key_hash: Option<Hash160>,
    pub stacks_parent_consensus_hash: Option<ConsensusHash>,
    pub last_sortition_consensus_hash: Option<ConsensusHash>,
    pub committed_block_hash: Option<BlockHeaderHash>,
    /// The winning commitment's new seed, which seeds the next sortition hash.
    pub vrf_seed: Option<[u8; 32]>,
    /// Local diagnostic inputs for the election, absent from stock-node replies.
    pub mining_competition: Option<MiningCompetition>,
}

/// A locally validated tenure downloaded from a peer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FollowedTenure {
    pub info: TenureInfo,
    pub sortition: SortitionInfo,
    /// Immutable once validated, and shared by the follower and every published view.
    pub blocks: Arc<Vec<NakamotoBlock>>,
    wire_bytes: usize,
}

impl FollowedTenure {
    pub fn new(
        info: TenureInfo,
        sortition: SortitionInfo,
        blocks: Arc<Vec<NakamotoBlock>>,
    ) -> Result<Self, SyncError> {
        let wire_bytes = tenure_wire_bytes(&blocks)?;
        Ok(Self {
            info,
            sortition,
            blocks,
            wire_bytes,
        })
    }

    #[must_use]
    pub const fn wire_bytes(&self) -> usize {
        self.wire_bytes
    }
}

/// How many followed tenures stay in memory.
///
/// The follower itself only ever extends the last one; the rest exist for the
/// RPC's consumers. The deepest of those is the signer fork check, which walks
/// ten tenures (`FORK_INFO_DEPTH` in `nano-rpc`), and `.miners` slot
/// attribution, which wants eight recent sortition winners. A mainnet tenure
/// runs to hundreds of blocks, so an unbounded history was a follower-lifetime
/// memory leak — every block of every tenure since startup, deep-cloned into
/// each published view.
pub const FOLLOWED_TENURE_HISTORY_ITEMS: usize = 16;
pub const FOLLOWED_TENURE_HISTORY_BYTES: usize = 10 * MAX_TENURE_RESPONSE_BYTES;

fn tenure_wire_bytes(blocks: &[NakamotoBlock]) -> Result<usize, SyncError> {
    let mut total = 0usize;
    for block in blocks {
        total = add_tenure_wire_bytes(total, block.encode().len())?;
    }
    Ok(total)
}

const fn add_tenure_wire_bytes(total: usize, bytes: usize) -> Result<usize, SyncError> {
    if bytes > MAX_TENURE_RESPONSE_BYTES.saturating_sub(total) {
        return Err(SyncError::ResponseTooLarge {
            limit: MAX_TENURE_RESPONSE_BYTES,
        });
    }
    Ok(total + bytes)
}

/// Stateful HTTP follower for the peer's current tenure.
#[derive(Clone, Debug)]
pub struct TenureFollower {
    client: SyncClient,
    latest: Option<TenureInfo>,
    /// The most recent validated tenures, oldest first.
    history: Vec<FollowedTenure>,
    history_bytes: usize,
}

/// One burn block on the view a peer holds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForkInfo {
    pub bitcoin_height: u64,
    pub consensus_hash: ConsensusHash,
    pub was_sortition: bool,
    pub first_block_mined: Option<StacksBlockId>,
}

/// The first burn block two views agree on, which is where they parted.
///
/// Both are ordered newest first, as the endpoint returns them.
#[must_use]
pub fn fork_point(ours: &[ForkInfo], theirs: &[ForkInfo]) -> Option<ConsensusHash> {
    let ours: Vec<_> = ours.iter().map(|entry| entry.consensus_hash).collect();
    fork_point_of(&ours, theirs)
}

/// The same, for a side that holds only the tenures it executed.
///
/// A node comparing a peer's view against its own has consensus hashes and
/// nothing else — it did not learn its chain from a `fork_info` answer — and
/// making it fabricate the rest of a `ForkInfo` to ask the question would be
/// inventing burn heights to throw them away.
#[must_use]
pub fn fork_point_of(ours: &[ConsensusHash], theirs: &[ForkInfo]) -> Option<ConsensusHash> {
    let theirs: BTreeSet<_> = theirs.iter().map(|entry| entry.consensus_hash).collect();
    ours.iter().find(|hash| theirs.contains(hash)).copied()
}

/// A chain tip a peer is offering, and the header that proves it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateTip {
    /// Which of the followed peers offered it.
    pub peer: usize,
    pub info: TenureInfo,
    pub header: nano_chainstate::NakamotoBlockHeader,
}

/// Why a candidate tip is not one to follow.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TipRejection {
    /// Its signatures do not add up to the threshold the reward set requires.
    InsufficientWeight,
    /// It carries a signature from outside the reward set, or out of order.
    UnknownOrUnorderedSigner,
    /// Its signatures do not recover.
    UnrecoverableSignature,
    /// It names a burn view this node's own burnchain did not produce.
    ForeignBurnView,
}

impl fmt::Display for TipRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InsufficientWeight => "tip does not carry threshold signer weight",
            Self::UnknownOrUnorderedSigner => "tip carries a signature from outside the reward set",
            Self::UnrecoverableSignature => "tip carries a signature that does not recover",
            Self::ForeignBurnView => {
                "tip names a burn view this node's own burnchain did not produce"
            }
        })
    }
}

impl std::error::Error for TipRejection {}

/// What this node's own burnchain says about the tenure a peer is offering.
///
/// The point of this trait is that the answer comes from the node's own derived
/// sortition chain rather than from the peer being weighed: a peer's
/// `/v3/sortitions` answer describes the burnchain that peer read, which is the
/// thing under suspicion.
///
/// `Sync` because [`PeerPool::choose_source`] holds one across the request that
/// gathers the candidate tips, and that future is spawned as a task.
pub trait BurnView: Sync {
    /// The Bitcoin height a burn view sits at, where this node derived it.
    ///
    /// stacks-core breaks a fork-choice tie between two equally high tips on this
    /// number: "break ties by going with the latter-signed block", which it
    /// implements as `sn_current.block_height < sn_accepted.block_height` over the
    /// two tips' *sortitions* (`SortitionDB::set_stacks_block_accepted_at_tip`).
    /// `None` where this node has not derived that view, and then it has no opinion
    /// to bring to the tie.
    fn height_of(&self, consensus_hash: ConsensusHash) -> Option<u64>;

    /// Whether the burn view a candidate names is one this node derived.
    ///
    /// `None` means *this node cannot judge*, which is the ordinary case while
    /// catching up: the candidate stands on a burn block ahead of the ones this
    /// node has derived, so nothing local contradicts it and the burn total of
    /// every block it later executes is checked against this same chain
    /// (`CheckpointExecutionError::BitcoinSpent`) as it gets there.
    ///
    /// `bitcoin_spent` is the candidate header's cumulative burn, which is what
    /// places it against this node's own chain without asking anybody: it is the
    /// running total of the burn view the block was built on, and the reward set
    /// signed it.
    fn derived(&self, consensus_hash: ConsensusHash, bitcoin_spent: u64) -> Option<bool>;
}

/// Whether a tip is one the network would accept, and what weight signed it.
///
/// Length decides nothing on its own: a peer can serve a longer chain that no
/// signer put its name to. Weight is what makes a chain canonical, so it is
/// checked before length is even compared.
///
/// The set is a [`SignerWeights`] rather than a [`SignerSet`] because that is the
/// form this node's *executed state* holds — `.signers`, written by whichever node
/// reached the prepare phase, under a state root the network agreed with — and it
/// is the same value `ChainState::check_signer_signatures` enforces at execution.
/// Weighing selection against a set parsed from a peer's `/v3/stacker_set` would
/// be asking the candidates' own network who may approve them.
pub fn weigh_tip(
    header: &nano_chainstate::NakamotoBlockHeader,
    signers: &nano_chainstate::SignerWeights,
) -> Result<u32, TipRejection> {
    signers.verify(header).map_err(|error| match error {
        SignerSetError::InsufficientWeight => TipRejection::InsufficientWeight,
        SignerSetError::Signature(_) => TipRejection::UnrecoverableSignature,
        _ => TipRejection::UnknownOrUnorderedSigner,
    })
}

/// Why this node will not follow a candidate, or nothing if it will.
///
/// Both rules answer from what this node holds for itself. The burn view is
/// checked first because it is what decides whether the weight rule can be
/// applied at all: a candidate standing on a burn block this node has not derived
/// belongs to a reward cycle whose signer set this node's state may not record,
/// and refusing it on weight would refuse every honest peer of a node that is
/// catching up.
fn refuse_tip(
    candidate: &CandidateTip,
    signers: Option<&nano_chainstate::SignerWeights>,
    burn: Option<&dyn BurnView>,
) -> Option<TipRejection> {
    let judgeable = match burn {
        Some(burn) => match burn.derived(
            candidate.info.consensus_hash,
            candidate.header.bitcoin_spent,
        ) {
            // A burn view this node derived: the tip is on this node's burnchain
            // and in a cycle it has reached, so weight is enforced below.
            Some(true) => true,
            // A view at or below the ones this node derived that this node never
            // derived is a different chain of Bitcoin blocks. No signature check
            // would catch it — a chain built over another burnchain can be
            // perfectly signed by the signers of *that* chain.
            Some(false) => return Some(TipRejection::ForeignBurnView),
            None => false,
        },
        // A node with no burn view of its own has nothing to say about which
        // burnchain a tip stands on, so it applies the weight rule alone —
        // strictly, which is the safe direction for a node that cannot tell an
        // unreachable cycle from a wrong one.
        None => true,
    };
    if !judgeable {
        return None;
    }
    signers.and_then(|signers| weigh_tip(&candidate.header, signers).err())
}

/// Choose the tip to follow among the ones the peers are offering.
///
/// A candidate has to survive both of this node's own checks before its length
/// counts: the burn view it names has to be one this node's burnchain produced,
/// and — where this node can judge the cycle — the signatures have to carry
/// threshold weight against the set this node's executed state records. Among
/// those, the longest chain wins, and an exact tie goes to the lowest block
/// identifier so that every node looking at the same peers lands on the same
/// block rather than on whichever answered first.
#[must_use]
pub fn choose_canonical_tip<'a>(
    candidates: &'a [CandidateTip],
    signers: Option<&nano_chainstate::SignerWeights>,
    burn: Option<&dyn BurnView>,
) -> Option<&'a CandidateTip> {
    // The burn height of a tip's own sortition, which is what stacks-core breaks a
    // tie on. Unknown views compare equal to each other and below known ones: a
    // node with no opinion about where a view sits must not prefer it to one it
    // derived, and must not order two it knows nothing about.
    let sortition_height = |candidate: &CandidateTip| {
        burn.and_then(|burn| burn.height_of(candidate.header.consensus_hash))
    };
    candidates
        .iter()
        .filter(|candidate| refuse_tip(candidate, signers, burn).is_none())
        .max_by(|left, right| {
            left.header
                .chain_length
                .cmp(&right.header.chain_length)
                // stacks-core's tie-break, transcribed from
                // `SortitionDB::set_stacks_block_accepted_at_tip`: at equal height
                // and different tenures it "break[s] ties by going with the
                // latter-signed block", meaning the tip whose sortition is at the
                // higher burn height. This used to be nano's block-id comparison,
                // which is deterministic and *different*, so two nodes could stand
                // on different tips of the same length and both be behaving as
                // designed.
                .then_with(|| sortition_height(left).cmp(&sortition_height(right)))
                // Last, and only where the two are in the same tenure or in
                // sortitions this node cannot place. stacks-core keeps whichever it
                // saw first here, which is arrival order and not a function of the
                // two tips at all -- so there is nothing to agree with, and a
                // deterministic rule is strictly better than a coin toss.
                .then_with(|| right.header.block_id().cmp(&left.header.block_id()))
        })
}

/// Bitcoin calendar and stacking parameters advertised by a node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PoxInfo {
    pub first_bitcoin_height: u64,
    pub bitcoin_height: u64,
    pub prepare_phase_length: u32,
    pub reward_phase_length: u32,
    pub reward_slots: u32,
    pub rejection_fraction: Option<u64>,
    pub pox_5_activation_height: Option<u32>,
    /// Height at which PoX-1 locks expire, one block after epoch 2.1 begins.
    pub v1_unlock_height: Option<u32>,
    /// Height at which PoX-2 locks expire, one block after epoch 2.2 begins.
    pub v2_unlock_height: Option<u32>,
    /// Height at which PoX-3 locks expire, one block after epoch 2.5 begins.
    pub v3_unlock_height: Option<u32>,
}

/// The threshold signer set and optional waterfall payout address for one reward cycle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackerSet {
    pub pox_ustx_threshold: u128,
    pub sbtc_address: Option<PoxAddress>,
    pub signer_set: SignerSet,
}

/// A stock node's acknowledgement of a finalized block upload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockUpload {
    pub accepted: bool,
    pub block_id: StacksBlockId,
}

impl PoxInfo {
    /// The reward cycle a Bitcoin height belongs to.
    #[must_use]
    pub fn reward_cycle(&self, bitcoin_height: u64) -> u64 {
        let length = u64::from(self.prepare_phase_length) + u64::from(self.reward_phase_length);
        bitcoin_height.saturating_sub(self.first_bitcoin_height) / length.max(1)
    }

    /// The burn height whose consensus hash names this cycle on the inventory wire.
    ///
    /// `GetNakamotoInv` keeps stacks-core's original modulo-one boundary even after
    /// Nakamoto signer accounting moves to the waterfall's modulo-zero boundary.
    #[must_use]
    pub fn inventory_cycle_start(&self, bitcoin_height: u64) -> Option<u64> {
        let length = u64::from(self.prepare_phase_length)
            .checked_add(u64::from(self.reward_phase_length))?;
        if length == 0 {
            return None;
        }
        let offset = bitcoin_height
            .checked_sub(self.first_bitcoin_height)?
            .checked_sub(1)?;
        self.first_bitcoin_height
            .checked_add((offset / length).checked_mul(length)?)?
            .checked_add(1)
    }

    /// Convert the node response into the context required for VM execution.
    #[must_use]
    pub fn bitcoin_context(&self) -> BitcoinBlockContext {
        // Through `at_height` so the tenure's burn height comes with the view: the
        // two are the same block until a caller says a tenure was extended.
        let mut context = BitcoinBlockContext::at_height(self.bitcoin_height);
        context.first_height = self.first_bitcoin_height;
        context.prepare_phase_length = self.prepare_phase_length;
        context.reward_phase_length = self.reward_phase_length;
        context.rejection_fraction = self.rejection_fraction.unwrap_or(0);
        context.v1_unlock_height = self.v1_unlock_height.unwrap_or(u32::MAX);
        context.v2_unlock_height = self.v2_unlock_height.unwrap_or(u32::MAX);
        context.v3_unlock_height = self.v3_unlock_height.unwrap_or(u32::MAX);
        context.pox_5_activation_height = self.pox_5_activation_height.unwrap_or(u32::MAX);
        // Only a tenure-start block collects a coinbase, so its caller fills this
        // in from the sortitions around it.
        context.accumulated_coinbase = 0;
        context
    }
}

#[derive(Debug)]
pub enum SyncError {
    /// A tenure's blocks stopped short of the tip the peer reports.
    ///
    /// One response carries a bounded number of blocks; this says how many
    /// short it came, which is what tells a bounded page from a real fork.
    TenureGap {
        tenure: StacksBlockId,
        have: u64,
        want: u64,
    },
    InvalidBaseUrl,
    ResponseTooLarge {
        limit: usize,
    },
    Http(reqwest::Error),
    Json(serde_json::Error),
    Block(NakamotoCodecError),
    EmptyTenure,
    TenureStart,
    TenureLink(TenureError),
    InvalidHash,
    EmptySortition,
    InvalidSortition,
    InvalidRewardSet,
    Crypto(CryptoError),
    SignerSet(SignerSetError),
    BlockUploadRejected,
    UnstableTip,
    Fork,
    InvalidMempool,
    InvalidAccount,
    /// Every peer in the pool has been asked and none could answer.
    NoPeer,
    /// Every peer in the pool is rate limiting, so there is nobody left to ask.
    ///
    /// Distinct from [`SyncError::NoPeer`] because the two ask for opposite
    /// things: a pool with nobody able to serve is a failed round, while a pool
    /// that is throttling has not failed at all — the round keeps what it
    /// fetched, executes it, and asks again. Answering `NoPeer` here is what
    /// made one 429 end a mainnet round with an error before it executed
    /// anything it already held.
    Throttled,
    /// A peer answered a block request with a different block.
    ///
    /// Its own kind rather than a generic failure because it is the one thing a peer
    /// can do to a content-addressed fetch, and a caller spreading a walk over a pool
    /// needs to be able to say which peer did it.
    UnexpectedBlock {
        expected: StacksBlockId,
        found: StacksBlockId,
    },
    /// A peer answered a sortition request with another burn view's sortition.
    ///
    /// The one part of that answer a peer cannot be allowed to choose: everything
    /// else in it is checked by the state root of the block executed under it, but
    /// which burn block that is has to be the one asked for.
    UnexpectedSortition {
        asked: ConsensusHash,
        answered: ConsensusHash,
    },
    /// A peer answered a tenure request with a block from another tenure.
    ///
    /// The check that makes a *forward* download safe to spread over strangers. A
    /// backward walk is self-checking, because the identifier asked for is the hash
    /// of the block that comes back; a tenure asked for by the burn view that
    /// elected it is not, so the answer has to be compared against the view — which
    /// every Nakamoto block header carries.
    UnexpectedTenure {
        asked: ConsensusHash,
        answered: ConsensusHash,
    },
}

impl SyncError {
    /// Whether the peer never answered at all, as against answering unhelpfully.
    ///
    /// A connect failure or a timeout is a property of the *peer*, so it is worth
    /// remembering for the rest of a round; a status code is a property of the
    /// request, and the same peer answers the next one.
    #[must_use]
    pub fn is_unreachable(&self) -> bool {
        matches!(self, Self::Http(error) if error.is_connect() || error.is_timeout())
    }

    /// Whether the peer answered 429, which is a reason to wait rather than to
    /// treat the peer as broken.
    #[must_use]
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, Self::Http(error)
            if error.status() == Some(reqwest::StatusCode::TOO_MANY_REQUESTS))
            || matches!(self, Self::Throttled)
    }
}

impl fmt::Display for SyncError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBaseUrl => formatter.write_str("sync base URL cannot be a base"),
            Self::ResponseTooLarge { limit } => {
                write!(formatter, "peer response exceeds its {limit}-byte limit")
            }
            Self::NoPeer => formatter.write_str("no peer left to ask"),
            Self::Throttled => formatter.write_str("every peer is rate limiting this node"),
            Self::UnexpectedBlock { expected, found } => write!(
                formatter,
                "a peer answered the request for block {expected} with block {found}"
            ),
            Self::UnexpectedSortition { asked, answered } => write!(
                formatter,
                "a peer answered the request for the sortition of burn view {asked} with {answered}"
            ),
            Self::UnexpectedTenure { asked, answered } => write!(
                formatter,
                "a peer answered the request for the tenure of burn view {asked} with a block of {answered}"
            ),
            Self::Http(error) => write!(formatter, "HTTP sync error: {error}"),
            Self::Json(error) => write!(formatter, "invalid JSON sync response: {error}"),
            Self::Block(error) => write!(formatter, "invalid Nakamoto block response: {error}"),
            Self::EmptyTenure => formatter.write_str("tenure response contains no blocks"),
            Self::TenureStart => formatter.write_str("tenure response starts at the wrong block"),
            Self::TenureLink(error) => write!(formatter, "invalid tenure link: {error}"),
            Self::TenureGap { tenure, have, want } => write!(
                formatter,
                "tenure {tenure} answered up to height {have} but its tip is {want}"
            ),
            Self::InvalidHash => formatter.write_str("sync response contains an invalid hash"),
            Self::EmptySortition => formatter.write_str("sortition response contains no entries"),
            Self::InvalidSortition => formatter.write_str("sortition response is inconsistent"),
            Self::InvalidRewardSet => formatter.write_str("reward set response is invalid"),
            Self::Crypto(error) => write!(formatter, "invalid signer key: {error}"),
            Self::SignerSet(error) => write!(formatter, "invalid signer set: {error}"),
            Self::BlockUploadRejected => formatter.write_str("node rejected uploaded block"),
            Self::UnstableTip => formatter.write_str("peer tip changed during tenure download"),
            Self::Fork => formatter.write_str("peer tenure does not extend the followed chain"),
            Self::InvalidMempool => formatter.write_str("mempool page is not a transaction stream"),
            Self::InvalidAccount => formatter.write_str("account response has no readable balance"),
        }
    }
}

impl std::error::Error for SyncError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Http(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Block(error) => Some(error),
            Self::TenureLink(error) => Some(error),
            Self::Crypto(error) => Some(error),
            Self::SignerSet(error) => Some(error),
            Self::TenureGap { .. }
            | Self::InvalidBaseUrl
            | Self::ResponseTooLarge { .. }
            | Self::EmptyTenure
            | Self::TenureStart
            | Self::InvalidHash
            | Self::EmptySortition
            | Self::InvalidSortition
            | Self::InvalidRewardSet
            | Self::BlockUploadRejected
            | Self::UnstableTip
            | Self::Fork
            | Self::InvalidMempool
            | Self::InvalidAccount
            | Self::NoPeer
            | Self::Throttled
            | Self::UnexpectedBlock { .. }
            | Self::UnexpectedSortition { .. }
            | Self::UnexpectedTenure { .. } => None,
        }
    }
}

impl From<reqwest::Error> for SyncError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error)
    }
}

impl From<serde_json::Error> for SyncError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<CryptoError> for SyncError {
    fn from(error: CryptoError) -> Self {
        Self::Crypto(error)
    }
}

impl From<SignerSetError> for SyncError {
    fn from(error: SignerSetError) -> Self {
        Self::SignerSet(error)
    }
}

async fn bounded_response(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, SyncError> {
    let declared = response
        .content_length()
        .map(usize::try_from)
        .transpose()
        .map_err(|_| SyncError::ResponseTooLarge { limit })?;
    if declared.is_some_and(|length| length > limit) {
        return Err(SyncError::ResponseTooLarge { limit });
    }
    let mut body = Vec::with_capacity(declared.unwrap_or(0));
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(SyncError::ResponseTooLarge { limit });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn bounded_json<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
    limit: usize,
) -> Result<T, SyncError> {
    Ok(serde_json::from_slice(
        &bounded_response(response, limit).await?,
    )?)
}

impl SyncClient {
    pub fn new(base_url: Url) -> Result<Self, SyncError> {
        if base_url.cannot_be_a_base() {
            Err(SyncError::InvalidBaseUrl)
        } else {
            Ok(Self {
                client: Client::builder()
                    .timeout(Duration::from_secs(30))
                    // A peer that cannot complete a TCP handshake is not slow, it is
                    // unreachable, and the two want different patience. Discovery
                    // learns a peer's *p2p* port and its HTTP port is an assumption
                    // about the port beside it, so a pool of strangers holds several
                    // addresses whose 20443 never answers: a live mainnet catch-up
                    // spent fifty minutes at `SYN-SENT` against one of them, paying
                    // the whole 30 s request budget per attempt.
                    .connect_timeout(Duration::from_secs(4))
                    .build()?,
                base_url,
            })
        }
    }

    /// The peer this client talks to, for the other clients aimed at it.
    #[must_use]
    pub const fn base_url(&self) -> &Url {
        &self.base_url
    }

    pub async fn node_info(&self) -> Result<NodeInfo, SyncError> {
        let response: NodeInfoWire = self.get("v2/info").await?;
        Ok(NodeInfo {
            bitcoin_height: response.burn_block_height,
            stacks_height: response.stacks_tip_height,
            stacks_tip: parse_block_hash(&response.stacks_tip)?,
            consensus_hash: parse_consensus_hash(&response.stacks_tip_consensus_hash)?,
            network_id: response.network_id,
        })
    }

    pub async fn tenure_info(&self) -> Result<TenureInfo, SyncError> {
        let response: TenureInfoWire = self.get("v3/tenures/info").await?;
        Ok(TenureInfo {
            consensus_hash: parse_consensus_hash(&response.consensus_hash)?,
            tenure_start_block_id: parse_block_id(&response.tenure_start_block_id)?,
            parent_consensus_hash: parse_consensus_hash(&response.parent_consensus_hash)?,
            parent_tenure_start_block_id: parse_block_id(&response.parent_tenure_start_block_id)?,
            tip_block_id: parse_block_id(&response.tip_block_id)?,
            tip_height: response.tip_height,
            reward_cycle: response.reward_cycle,
        })
    }

    /// Fetch the Bitcoin calendar and stacking parameters used by the node.
    pub async fn pox_info(&self) -> Result<PoxInfo, SyncError> {
        let response: PoxInfoWire = self.get("v2/pox").await?;
        Ok(PoxInfo {
            first_bitcoin_height: response.first_burnchain_block_height,
            bitcoin_height: response.current_burnchain_block_height,
            prepare_phase_length: response.prepare_phase_block_length,
            reward_phase_length: response.reward_phase_block_length,
            reward_slots: response.reward_slots,
            rejection_fraction: response.rejection_fraction,
            pox_5_activation_height: response
                .contract_versions
                .iter()
                .find(|version| version.contract_id.ends_with(".pox-5"))
                .map(|version| version.activation_burnchain_block_height),
            // Each legacy PoX version unlocks one block into the epoch that
            // retires it, which is how a node derives its own constants.
            v1_unlock_height: response.unlock_height_after("Epoch21"),
            v2_unlock_height: response.unlock_height_after("Epoch22"),
            v3_unlock_height: response.unlock_height_after("Epoch25"),
        })
    }

    /// Fetch the next nonce an account's transactions must use.
    #[cfg(feature = "mempool")]
    pub async fn account_nonce(&self, address: StacksAddress) -> Result<u64, SyncError> {
        Ok(self.account(address).await?.nonce)
    }

    /// Fetch the nonce and spendable balance a peer holds for an account.
    #[cfg(feature = "mempool")]
    pub async fn account(&self, address: StacksAddress) -> Result<Account, SyncError> {
        let response: AccountWire = self.get(&format!("v2/accounts/{address}?proof=0")).await?;
        let balance = u128::from_str_radix(response.balance.trim_start_matches("0x"), 16)
            .map_err(|_| SyncError::InvalidAccount)?;
        Ok(Account {
            nonce: response.nonce,
            balance: Some(balance),
        })
    }

    /// Fetch the account state every transaction a mempool holds is judged
    /// against.
    #[cfg(feature = "mempool")]
    pub async fn accounts_for(&self, mempool: &Mempool) -> Result<PeerAccounts, SyncError> {
        let mut accounts = HashMap::new();
        for address in mempool.addresses() {
            accounts.insert(address, self.account(address).await?);
        }
        Ok(PeerAccounts(accounts))
    }

    /// Offer a peer's whole mempool to a local one, and report what it kept.
    ///
    /// A peer's mempool is a source of transactions, not the node's answer to
    /// what it will mine: every transaction it hands over is admitted on this
    /// node's own rules against this node's own view of the accounts.
    #[cfg(feature = "mempool")]
    pub async fn fill_mempool(&self, mempool: &mut Mempool, now: u64) -> Result<usize, SyncError> {
        let mut page = None;
        let mut cursors = HashSet::new();
        let mut accounts = PeerAccounts::default();
        let mut admitted = 0;
        for _ in 0..MAX_MEMPOOL_PAGES {
            let (transactions, next) = self.mempool_page(page).await?;
            for transaction in &transactions {
                for address in [transaction.origin_address(), transaction.sponsor_address()]
                    .into_iter()
                    .flatten()
                {
                    if let std::collections::hash_map::Entry::Vacant(slot) =
                        accounts.0.entry(address)
                    {
                        slot.insert(self.account(address).await?);
                    }
                }
            }
            for transaction in transactions {
                match mempool.submit(transaction, &accounts, now) {
                    Ok(Admission::Added | Admission::Replaced(_)) => admitted += 1,
                    Err(Rejection::MempoolFull { .. }) => return Ok(admitted),
                    _ => {}
                }
            }
            let Some(next) = next else {
                return Ok(admitted);
            };
            if !cursors.insert(next) {
                return Err(SyncError::InvalidMempool);
            }
            page = Some(next);
        }
        Ok(admitted)
    }

    /// Fetch the waterfall reward set active for one reward cycle.
    pub async fn stacker_set(&self, reward_cycle: u64) -> Result<StackerSet, SyncError> {
        let response: StackerSetResponseWire =
            self.get(&format!("v3/stacker_set/{reward_cycle}")).await?;
        parse_stacker_set(response.stacker_set)
    }

    /// Fetch the Bitcoin sortition identified by its consensus hash.
    pub async fn sortition(
        &self,
        consensus_hash: ConsensusHash,
    ) -> Result<SortitionInfo, SyncError> {
        let key = (self.base_url.clone(), consensus_hash);
        if let Some(sortition) = sortition_cache()
            .lock()
            .ok()
            .and_then(|mut cache| cache.get(&key).cloned())
        {
            return Ok(sortition);
        }
        let (sortition, bytes) = self
            .single_sortition_with_size(&format!("v3/sortitions/consensus/{consensus_hash}"))
            .await?;
        if sortition.consensus_hash != consensus_hash {
            return Err(SyncError::InvalidSortition);
        }
        if let Ok(mut cache) = sortition_cache().lock() {
            cache.insert(key, sortition.clone(), bytes);
        }
        Ok(sortition)
    }

    /// Fetch the sortition of the peer's current Bitcoin tip.
    pub async fn sortition_tip(&self) -> Result<SortitionInfo, SyncError> {
        self.single_sortition("v3/sortitions").await
    }

    /// Fetch the sortition recorded for one Bitcoin height.
    pub async fn sortition_at_height(&self, height: u64) -> Result<SortitionInfo, SyncError> {
        let sortition = self
            .single_sortition(&format!("v3/sortitions/burn_height/{height}"))
            .await?;
        if sortition.bitcoin_height != height {
            return Err(SyncError::InvalidSortition);
        }
        Ok(sortition)
    }

    /// Fetch the transactions a peer holds that this node has not seen.
    ///
    /// The request says which transactions are already known; asking with an
    /// empty set of tags asks for everything. The response is the transactions
    /// back to back with the page identifier for the next request as its last
    /// thirty-two bytes (`core/mempool.rs`, `decode_tx_stream`).
    #[cfg(feature = "mempool")]
    pub async fn mempool_page(
        &self,
        page: Option<Sha256Sum>,
    ) -> Result<(Vec<Transaction>, Option<Sha256Sum>), SyncError> {
        let path = page.map_or_else(
            || "v2/mempool/query".to_owned(),
            |page| format!("v2/mempool/query?page_id={page}"),
        );
        let url = self
            .base_url
            .join(&path)
            .map_err(|_| SyncError::InvalidBaseUrl)?;
        // An empty tag list under a zero seed claims no knowledge of the peer's
        // mempool, which is how a node that has just started asks for all of it.
        let mut query = vec![MEMPOOL_QUERY_TX_TAGS];
        query.extend_from_slice(&[0; 32]);
        query.extend_from_slice(&0_u32.to_be_bytes());
        let response = self
            .client
            .post(url)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(query)
            .send()
            .await?
            .error_for_status()?;
        let body = bounded_response(response, MAX_MEMPOOL_RESPONSE_BYTES).await?;
        decode_mempool_page(&body)
    }

    /// The last Bitcoin height before this one that chose a miner.
    ///
    /// A tenure collects the coinbase of every burn block since that height, so
    /// finding it is what makes a tenure-start block's reward derivable.
    /// One peer's answer, through the pool of one that it is.
    ///
    /// The walk is two dependent lookups and lives on [`TenureSource`], where a
    /// peer that stops answering costs the request rather than the round. A caller
    /// holding a single client asks a pool of one.
    pub async fn previous_sortition_height(
        &self,
        bitcoin_height: u64,
    ) -> Result<Option<u64>, SyncError> {
        TenureSource::only(self.clone())
            .previous_sortition_height(bitcoin_height)
            .await
    }

    /// The coinbase a block's tenure accumulated, or nothing when the block
    /// starts no tenure or no schedule says what a coinbase is worth.
    pub async fn accumulated_coinbase(
        &self,
        block: &NakamotoBlock,
        schedule: Option<CoinbaseSchedule>,
        bitcoin_height: u64,
    ) -> Result<Option<u128>, SyncError> {
        TenureSource::only(self.clone())
            .accumulated_coinbase(block, schedule, bitcoin_height)
            .await
    }

    /// Complete a block's execution context with the coinbase its tenure earns.
    pub async fn tenure_coinbase_context(
        &self,
        block: &NakamotoBlock,
        schedule: Option<CoinbaseSchedule>,
        context: BitcoinBlockContext,
    ) -> Result<BitcoinBlockContext, SyncError> {
        TenureSource::only(self.clone())
            .tenure_coinbase_context(block, schedule, context)
            .await
    }

    async fn single_sortition(&self, path: &str) -> Result<SortitionInfo, SyncError> {
        Ok(self.single_sortition_with_size(path).await?.0)
    }

    async fn single_sortition_with_size(
        &self,
        path: &str,
    ) -> Result<(SortitionInfo, usize), SyncError> {
        let body = self.bytes(path, MAX_JSON_RESPONSE_BYTES).await?;
        let mut sortitions: Vec<SortitionInfoWire> = serde_json::from_slice(&body)?;
        let sortition = sortitions.pop().ok_or(SyncError::EmptySortition)?;
        if !sortitions.is_empty() {
            return Err(SyncError::InvalidSortition);
        }
        Ok((parse_sortition_info(&sortition)?, body.len()))
    }

    /// Download and validate one Nakamoto block by its block ID.
    pub async fn block(&self, block_id: StacksBlockId) -> Result<NakamotoBlock, SyncError> {
        if let Some(block) = self.cached(block_id) {
            return Ok(block);
        }
        let bytes = self
            .bytes(&format!("v3/blocks/{block_id}"), MAX_BLOCK_RESPONSE_BYTES)
            .await?;
        let block = NakamotoBlock::decode(&bytes).map_err(SyncError::Block)?;
        // A block is content-addressed by the identifier that was asked for, so the
        // one check that makes "which peer answered" irrelevant is that the answer is
        // the block asked for. Without it, spreading a walk over a pool would let any
        // peer in the pool substitute a block of its choosing at any step, and the
        // caller — a repair counting fees, a descent following parent links — would
        // carry on from the substitute.
        if block.block_id() != block_id {
            return Err(SyncError::UnexpectedBlock {
                expected: block_id,
                found: block.block_id(),
            });
        }
        if let Ok(mut blocks) = block_cache().lock() {
            blocks.insert(
                (self.base_url.clone(), block_id),
                block.clone(),
                bytes.len(),
            );
        }
        Ok(block)
    }

    fn cached(&self, block_id: StacksBlockId) -> Option<NakamotoBlock> {
        block_cache()
            .lock()
            .ok()?
            .get(&(self.base_url.clone(), block_id))
            .cloned()
    }

    /// Upload a finalized block to a stock node and require its exact acknowledgement.
    pub async fn upload_block(&self, block: &NakamotoBlock) -> Result<BlockUpload, SyncError> {
        let url = self
            .base_url
            .join("v3/blocks/upload")
            .map_err(|_| SyncError::InvalidBaseUrl)?;
        let response = self
            .client
            .post(url)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(block.encode())
            .send()
            .await?
            .error_for_status()?;
        let response: BlockUploadWire = bounded_json(response, MAX_UPLOAD_RESPONSE_BYTES).await?;
        let upload = BlockUpload {
            accepted: response.accepted,
            block_id: parse_block_id(&response.stacks_block_id)?,
        };
        // A node that already holds the block reports it as not newly accepted,
        // which is success for the miner that produced it.
        if upload.block_id != block.block_id() {
            return Err(SyncError::BlockUploadRejected);
        }
        Ok(upload)
    }

    /// Walk the burn view between two consensus hashes.
    ///
    /// This is how a node finds where a candidate chain left the one it holds:
    /// the answer runs backwards from `start` to `stop`, one entry per burn
    /// block, and the first entry both chains agree on is the fork point.
    ///
    /// A page at a time, because one request does not reach. stacks-core walks
    /// back at most `DEPTH_LIMIT = 10` sortitions and then answers 200 with a
    /// **truncated** body, saying nothing about having stopped early
    /// (`stackslib/src/net/api/get_tenures_fork_info.rs:38`). A caller that asked
    /// once saw ten burn blocks and read everything below them as "these chains
    /// never met": on mainnet the walk reached burn 961671 while the two chains
    /// had parted at 961648, so a real fork was indistinguishable from no fork at
    /// all ([[096-cross-a-stacks-fork-inside-one-sortition-chain]]). Each page
    /// resumes from the oldest entry the last one carried, and the walk ends when
    /// it reaches `stop`, stops making progress, or runs out of pages.
    ///
    /// Pages overlap by their boundary entry, which no caller has to care about:
    /// the answer is read as a set of consensus hashes.
    pub async fn tenure_fork_info(
        &self,
        start: ConsensusHash,
        stop: ConsensusHash,
    ) -> Result<Vec<ForkInfo>, SyncError> {
        let mut walked: Vec<ForkInfo> = Vec::new();
        let mut cursor = start;
        for _ in 0..FORK_INFO_PAGES {
            let page = self.fork_info_page(cursor, stop).await?;
            let Some(oldest) = page.last().map(|entry| entry.consensus_hash) else {
                break;
            };
            walked.extend(page);
            if oldest == stop || oldest == cursor {
                break;
            }
            cursor = oldest;
        }
        Ok(walked)
    }

    /// One `fork_info` answer, however far back the peer chose to walk.
    ///
    /// stacks-core's route names the older bound first and the newer cursor
    /// second, despite returning the entries in the opposite direction.
    ///
    /// Bounded as a *tenure* response and not as metadata, which is the same
    /// bound [`Self::tenure_at`] gives this very route. Each entry carries its
    /// tenure's whole `nakamoto_blocks` body hex-encoded, so a page is up to
    /// `DEPTH_LIMIT` tenures of blocks rather than a list of hashes: a measured
    /// mainnet page is 10.2 MB over eleven entries, two and a half times the
    /// 4 MiB [`MAX_JSON_RESPONSE_BYTES`] the default `get` applies. The effect
    /// was not a slow fork check but no fork check at all — every page failed on
    /// the limit, [`crate::fork_point_of`] was never given anything to compare,
    /// and a follower that had executed onto a losing branch could not discover
    /// where it parted. Seen on mainnet: the port-20492 node held every
    /// canonical block from 8,831,604 up and stayed on a 92-block abandoned
    /// tenure for a day, printing only that a peer "could not say where its burn
    /// view parted from this one".
    async fn fork_info_page(
        &self,
        start: ConsensusHash,
        stop: ConsensusHash,
    ) -> Result<Vec<ForkInfo>, SyncError> {
        let wire: Vec<ForkInfoWire> = self
            .get_with_limit(
                &format!("v3/tenures/fork_info/{stop}/{start}"),
                MAX_TENURE_JSON_RESPONSE_BYTES,
            )
            .await?;
        wire.into_iter()
            .map(|entry| {
                Ok(ForkInfo {
                    bitcoin_height: entry.burn_block_height,
                    consensus_hash: parse_consensus_hash(&entry.consensus_hash)?,
                    was_sortition: entry.was_sortition,
                    first_block_mined: entry
                        .first_block_mined
                        .as_deref()
                        .map(parse_block_id)
                        .transpose()?,
                })
            })
            .collect()
    }

    /// Every block of the tenure a burn view elected, asked for by that view.
    ///
    /// The primitive a **forward** download needs, and the reason it is this endpoint
    /// rather than `/v3/tenures/:id`. A tenure fetched by block identifier can only be
    /// asked for once its blocks are already known, which is why every descent so far
    /// has had to walk parent links *backwards* from a tip: the identifier of the next
    /// thing to ask for is inside the answer to the last one. A consensus hash is
    /// derivable ahead of time — this node's own sortition chain names every burn block
    /// it has walked — so a tenure addressed this way can be asked for before anything
    /// above it is known, which is what makes an inventory's bit indices into a
    /// schedule.
    ///
    /// `start == stop` is one sortition: `get_tenures_fork_info` pushes the stop
    /// snapshot, then walks parents while the cursor is not the start, so a request
    /// naming the same view twice never enters the walk. The body it answers with is
    /// `get_nakamoto_blocks_in_tenure`, which is the whole tenure and not a page.
    ///
    /// Every block is checked against the view asked for. That check is what
    /// `SyncClient::block`'s content-address comparison is for a backward walk: the
    /// answer here is not addressed by its own hash, so without it any peer in a pool
    /// could answer a scheduled tenure with somebody else's blocks.
    pub async fn tenure_at(
        &self,
        consensus_hash: ConsensusHash,
    ) -> Result<Vec<NakamotoBlock>, SyncError> {
        let wire: Vec<ForkInfoWire> = self
            .get_with_limit(
                &format!("v3/tenures/fork_info/{consensus_hash}/{consensus_hash}"),
                MAX_TENURE_JSON_RESPONSE_BYTES,
            )
            .await?;
        let encoded = wire
            .into_iter()
            .find(|entry| {
                parse_consensus_hash(&entry.consensus_hash).is_ok_and(|hash| hash == consensus_hash)
            })
            .and_then(|entry| entry.nakamoto_blocks)
            .ok_or(SyncError::EmptyTenure)?;
        let blocks = decode_tenure_blocks(&encoded)?;
        if blocks.is_empty() {
            return Err(SyncError::EmptyTenure);
        }
        for block in &blocks {
            if block.header.consensus_hash != consensus_hash {
                return Err(SyncError::UnexpectedTenure {
                    asked: consensus_hash,
                    answered: block.header.consensus_hash,
                });
            }
        }
        Ok(blocks)
    }

    /// Download and validate all Nakamoto blocks in a tenure.
    pub async fn tenure(
        &self,
        start_block_id: StacksBlockId,
        stop_block_id: Option<StacksBlockId>,
    ) -> Result<Vec<NakamotoBlock>, SyncError> {
        let mut path = format!("v3/tenures/{start_block_id}");
        if let Some(stop_block_id) = stop_block_id {
            use std::fmt::Write;

            write!(path, "?stop={stop_block_id}").expect("writing to a string cannot fail");
        }
        let bytes = self.bytes(&path, MAX_TENURE_RESPONSE_BYTES).await?;
        let mut blocks = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let (block, consumed) =
                NakamotoBlock::decode_prefix(&bytes[offset..]).map_err(SyncError::Block)?;
            offset = offset.checked_add(consumed).ok_or(SyncError::InvalidHash)?;
            blocks.push(block);
        }
        validate_tenure(start_block_id, &blocks)?;
        Ok(blocks)
    }

    /// Every block of a tenure that this peer will answer for `block_id`.
    ///
    /// `/v3/tenures/:id` returns the tenure containing that block, which is one
    /// request where walking parent links is one per block. Asked from high in
    /// a tenure it answers with most of it; asked from a tenure's first block
    /// it answers with just that block. No order is assumed — the caller keys
    /// what it gets by height — so this only checks that the block asked for is
    /// among the answers.
    pub async fn blocks_of_tenure(
        &self,
        block_id: StacksBlockId,
    ) -> Result<Vec<NakamotoBlock>, SyncError> {
        let bytes = self
            .bytes(&format!("v3/tenures/{block_id}"), MAX_TENURE_RESPONSE_BYTES)
            .await?;
        let mut blocks = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let (block, consumed) =
                NakamotoBlock::decode_prefix(&bytes[offset..]).map_err(SyncError::Block)?;
            offset = offset.checked_add(consumed).ok_or(SyncError::InvalidHash)?;
            blocks.push(block);
        }
        if !blocks.iter().any(|block| block.block_id() == block_id) {
            return Err(SyncError::TenureStart);
        }
        Ok(blocks)
    }

    /// Ask for more of a tenure until its tip arrives.
    ///
    /// A tenure response may carry only the block it was asked for — a public
    /// endpoint answers `/v3/tenures/:id` with one block — so the rest is
    /// reached by walking back from the tip through `parent_block_id`, which
    /// needs nothing but `/v3/blocks/:id`.
    async fn extend_to_tenure_tip(
        &self,
        blocks: &mut Vec<NakamotoBlock>,
        tip: StacksBlockId,
    ) -> Result<(), SyncError> {
        let Some(known) = blocks.last().map(NakamotoBlock::block_id) else {
            return Err(SyncError::EmptyTenure);
        };
        if known == tip {
            return Ok(());
        }
        self.walk_back(blocks, tip, known, TENURE_WALK).await
    }

    /// Collect the blocks between `known` and `tip`, walking back by parent.
    async fn walk_back(
        &self,
        blocks: &mut Vec<NakamotoBlock>,
        tip: StacksBlockId,
        known: StacksBlockId,
        limit: usize,
    ) -> Result<(), SyncError> {
        let mut walked = Vec::new();
        let mut cursor = tip;
        while cursor != known {
            // A bounded walk per round, so one poll cannot spend itself on a
            // gap that keeps growing.
            if walked.len() >= limit {
                return Err(SyncError::TenureGap {
                    tenure: known,
                    have: blocks.last().map_or(0, |block| block.header.chain_length),
                    want: walked
                        .last()
                        .map_or(0, |block: &NakamotoBlock| block.header.chain_length),
                });
            }
            let block = self.block(cursor).await?;
            cursor = StacksBlockId::from_bytes(*block.header.parent_block_id.as_bytes());
            walked.push(block);
        }
        walked.reverse();
        blocks.extend(walked);
        Ok(())
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, SyncError> {
        self.get_with_limit(path, MAX_JSON_RESPONSE_BYTES).await
    }

    async fn get_with_limit<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        limit: usize,
    ) -> Result<T, SyncError> {
        let url = self
            .base_url
            .join(path)
            .map_err(|_| SyncError::InvalidBaseUrl)?;
        bounded_json(self.send(url).await?, limit).await
    }

    async fn bytes(&self, path: &str, limit: usize) -> Result<Vec<u8>, SyncError> {
        let url = self
            .base_url
            .join(path)
            .map_err(|_| SyncError::InvalidBaseUrl)?;
        bounded_response(self.send(url).await?, limit).await
    }

    /// Send a request, waiting out a peer that is rate limiting this node.
    ///
    /// A public endpoint answers 429 when asked too often, and a follower
    /// catching up asks constantly. Treating that as a failure drops the whole
    /// round and starts it again, which asks even more.
    async fn send(&self, url: reqwest::Url) -> Result<reqwest::Response, SyncError> {
        let started = std::time::Instant::now();
        let sent = self.send_inner(url).await;
        REQUESTS.fetch_add(1, Ordering::Relaxed);
        REQUEST_NANOS.fetch_add(
            u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        sent
    }

    async fn send_inner(&self, url: reqwest::Url) -> Result<reqwest::Response, SyncError> {
        let mut wait = RATE_LIMIT_WAIT;
        for _ in 0..RATE_LIMIT_RETRIES {
            let response = self.client.get(url.clone()).send().await?;
            if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Ok(response.error_for_status()?);
            }
            // Its own guess stays bounded, because a peer that says nothing
            // should not be able to stall a catch-up either.
            tokio::time::sleep(retry_after(response.headers()).unwrap_or(wait)).await;
            wait = wait.saturating_mul(2).min(RATE_LIMIT_CEILING);
        }
        Ok(self.client.get(url).send().await?.error_for_status()?)
    }
}

/// Where bulk history comes from: several peers, asked in turn.
///
/// Rebuilding tens of thousands of blocks from a checkpoint used to run every
/// request through the one client the follow loop had chosen. That makes one
/// service's rate limit the speed of catching up, which is precisely what joining
/// the peer network is meant to remove — `nano-p2p` hands back six mainnet
/// endpoints found by asking the network, and a tenure can come from any of them.
///
/// Three properties, and they are the same three that make a peer set worth having
/// at all:
///
/// * **Consecutive tenures go to different peers.** A descent of two thousand
///   tenures is two thousand requests, and sending them all to whichever peer
///   sorted first is a pool in name only.
/// * **A throttle moves the work, it does not stop it.** A peer that rate-limits is
///   doing its job; it is set aside for the rest of the round and the tenure is
///   asked of somebody else. Only when *every* peer has throttled does the round
///   report itself rate limited, which is the signal that means "wait", and by then
///   waiting is genuinely the only option.
/// * **A peer that cannot serve a tenure is not a failed round.** The next peer is
///   asked before an error is raised, so one peer missing one tenure — a fork it
///   never saw, history it has pruned — costs a request rather than the descent.
///
/// What it deliberately does *not* do is decide anything. Every block it returns
/// still goes through staging and the same authenticated execution path, so which
/// peer a block came from cannot change whether it is accepted.
#[derive(Debug)]
pub struct TenureSource {
    peers: Vec<SyncClient>,
    /// Which peer the next tenure goes to.
    next: usize,
    /// Peers that have rate-limited since the round began.
    throttled: BTreeSet<usize>,
    /// Peers that failed outright since the round began.
    ///
    /// Kept apart from `throttled` because the two are different facts and one of
    /// them is a measurement this pool reports: a rate limit is the peer working and
    /// asking to be asked less, while this is a peer that did not answer at all. Both
    /// are set aside for the round and both are forgiven together — what a failure
    /// must not do is cost every later request in the round the same wait again.
    failed: BTreeSet<usize>,
    /// Peers that have actually served a tenure, which is what makes "spread over the
    /// pool" a measurement rather than an intention.
    served: BTreeSet<usize>,
    /// Which peer answered the last request, so a caller can say who served the
    /// thing it just got. A set of indices says the pool was spread; this says
    /// over what, tenure by tenure, which is what a run has to retain to show
    /// that no one peer was load bearing.
    last_served: Option<usize>,
}

impl TenureSource {
    /// Fetch bulk history from these peers, in this order to begin with.
    #[must_use]
    pub const fn new(peers: Vec<SyncClient>) -> Self {
        Self {
            peers,
            next: 0,
            throttled: BTreeSet::new(),
            failed: BTreeSet::new(),
            served: BTreeSet::new(),
            last_served: None,
        }
    }

    /// One peer, which is what a node with a single configured peer still has.
    #[must_use]
    pub fn only(peer: SyncClient) -> Self {
        Self::new(vec![peer])
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.peers.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// How many distinct peers have actually served a tenure.
    ///
    /// The measurement the spreading exists for: a descent that reports one peer is a
    /// descent that has not been spread, whatever the pool holds.
    #[must_use]
    pub fn served_by(&self) -> usize {
        self.served.len()
    }

    /// The peer that answered the last request, by endpoint.
    #[must_use]
    pub fn last_served(&self) -> Option<&str> {
        self.last_served
            .and_then(|index| self.peers.get(index))
            .map(|peer| peer.base_url().as_str())
    }

    /// Whether every peer has rate-limited, so there is nobody left to ask.
    #[must_use]
    pub fn exhausted(&self) -> bool {
        self.peers.is_empty() || self.set_aside().count() >= self.peers.len()
    }

    /// Which peers this round has stopped asking, for either reason.
    fn set_aside(&self) -> impl Iterator<Item = &usize> {
        self.throttled.union(&self.failed)
    }

    /// Put a peer at the front of the queue, if it is in the pool.
    ///
    /// Used to honour what an inventory said: a peer that claims the cycle being
    /// walked is a better first guess than one that does not, and the inventory
    /// exists to avoid the round trip that finds out the hard way.
    /// A peer's `data_url` is compared *as a URL* and not as a string, which is the
    /// bug this had until it was measured: a handshake advertises
    /// `http://34.150.184.50:20443` while the client built from it holds
    /// `http://34.150.184.50:20443/`, so every comparison was false and preferring a
    /// claiming peer had no effect at all on the live node.
    pub fn prefer(&mut self, endpoints: &[String]) {
        // Stable, so peers the inventory said nothing about keep their order rather
        // than being shuffled by whichever claim arrived first.
        let preferred: BTreeSet<Url> = endpoints
            .iter()
            .filter_map(|url| Url::parse(url).ok())
            .collect();
        self.peers
            .sort_by_key(|peer| u8::from(!preferred.contains(peer.base_url())));
        self.next = 0;
        self.throttled.clear();
        self.failed.clear();
    }

    /// Ask the pool for one thing, starting where the last answer left off.
    ///
    /// Three properties, and every caller below wants all three: the work walks
    /// around the pool instead of settling on one member, a peer that rate-limits
    /// is set aside for the round rather than retried, and a peer that simply
    /// fails costs the request and not the walk. The last one is why this is a
    /// loop and not a call: a live mainnet catch-up stalled for fifty minutes on
    /// one discovered peer whose HTTP port stopped answering, with 28,458 blocks
    /// already downloaded and every round abandoning them.
    async fn spread<T, A, F>(&mut self, ask: A) -> Result<T, SyncError>
    where
        A: FnMut(SyncClient) -> F,
        F: Future<Output = Result<T, SyncError>>,
    {
        self.spread_from(None, ask).await
    }

    /// The same, starting at a named endpoint rather than at the round-robin cursor.
    ///
    /// What an inventory buys once it drives a schedule: the peer that claimed *this*
    /// tenure is asked for it, rather than whoever the cursor happens to be pointing
    /// at. The fallback is deliberate and is the whole reason this is the same loop —
    /// a claim is a claim, so a peer that named a tenure and cannot serve it costs one
    /// request and the rest of the pool is still asked.
    ///
    /// An endpoint that is not in the pool leaves the cursor alone, which is the right
    /// answer for a peer discovery found and this pool was not rebuilt from yet.
    async fn spread_from<T, A, F>(
        &mut self,
        first: Option<&str>,
        mut ask: A,
    ) -> Result<T, SyncError>
    where
        A: FnMut(SyncClient) -> F,
        F: Future<Output = Result<T, SyncError>>,
    {
        let start = first
            .and_then(|endpoint| Url::parse(endpoint).ok())
            .and_then(|url| self.peers.iter().position(|peer| *peer.base_url() == url))
            .unwrap_or(self.next);
        let mut last = None;
        for offset in 0..self.peers.len() {
            let index = (start + offset) % self.peers.len();
            if self.throttled.contains(&index) || self.failed.contains(&index) {
                continue;
            }
            let Some(peer) = self.peers.get(index).cloned() else {
                continue;
            };
            match ask(peer).await {
                Ok(answer) => {
                    self.next = index + 1;
                    self.served.insert(index);
                    self.last_served = Some(index);
                    return Ok(answer);
                }
                Err(error) if error.is_rate_limited() => {
                    self.throttled.insert(index);
                    last = Some(error);
                }
                Err(error) => {
                    // Only unreachability sets a peer aside. A 404 is an ordinary
                    // answer in a walk over strangers — a peer that does not hold the
                    // tenure being asked for is healthy and holds the next one — and
                    // setting those aside would empty the pool within one descent.
                    if error.is_unreachable() {
                        self.failed.insert(index);
                    }
                    last = Some(error);
                }
            }
        }
        Err(last.unwrap_or_else(|| self.nobody_left()))
    }

    /// Fetch one tenure, from whichever peer is next and willing.
    pub async fn blocks_of_tenure(
        &mut self,
        tip: StacksBlockId,
    ) -> Result<Vec<NakamotoBlock>, SyncError> {
        self.spread(|peer| async move { peer.blocks_of_tenure(tip).await })
            .await
    }

    /// Fetch the tenure a burn view elected, asking the peer that claimed it first.
    ///
    /// The forward half of bulk history. [`SyncClient::tenure_at`] is what makes it
    /// possible to ask at all — a tenure addressed by the consensus hash this node
    /// derived, rather than by a block identifier that only the answer above it
    /// carries — and `from` is the endpoint a peer's inventory named for this
    /// particular tenure.
    pub async fn tenure_at(
        &mut self,
        from: Option<&str>,
        consensus_hash: ConsensusHash,
    ) -> Result<Vec<NakamotoBlock>, SyncError> {
        self.spread_from(
            from,
            |peer| async move { peer.tenure_at(consensus_hash).await },
        )
        .await
    }

    /// Why nothing came back when no peer was even asked.
    ///
    /// Every peer was skipped, and the reason decides what the caller does: a pool
    /// that is throttling has not failed, so the round keeps what it has and asks
    /// again, while an empty pool is a round that cannot proceed. A pool whose peers
    /// were all *unreachable* is the second kind — nobody asked it to wait.
    fn nobody_left(&self) -> SyncError {
        if self.throttled.is_empty() {
            SyncError::NoPeer
        } else {
            SyncError::Throttled
        }
    }

    /// Fetch one block, from whichever peer is next and willing.
    ///
    /// For the repairs that walk the chain a block at a time rather than a tenure at
    /// a time — `rebuild-accounting` counts a few hundred tenures' fees block by
    /// block, which against one hosted endpoint is thousands of requests through one
    /// rate limit and was measured taking 1h45m. Spread over the peers p2p discovery
    /// found, the same walk is somebody's rate limit divided by the size of the pool.
    ///
    /// A block is content-addressed by the identifier asked for, so which peer answers
    /// cannot change the answer: `block()` on a `SyncClient` checks that the block it
    /// got back is the block it asked for. That is what makes spreading a repair over
    /// strangers safe in a way spreading a *choice* over them would not be.
    pub async fn block(&mut self, id: StacksBlockId) -> Result<NakamotoBlock, SyncError> {
        self.spread(|peer| async move { peer.block(id).await })
            .await
    }

    /// Look up one burn view's sortition, from whichever peer is next and willing.
    ///
    /// Not a choice being spread over strangers, for two reasons that both have to
    /// hold. The view is asked for by consensus hash and the answer that does not
    /// carry it back is refused here, so a peer cannot answer a *different* burn
    /// block's sortition. And every remaining field of it — the burn height, the
    /// burn header hash, its timestamp, the winning commitment's seed — is
    /// Clarity-visible, so it lands in the state root the block's own header commits
    /// to under threshold signer weight: a peer that lies about one makes the block
    /// fail to seal rather than making this node execute a different chain.
    ///
    /// The node derives all of this from its own burnchain as well, and rejects a
    /// header whose cumulative burn disagrees with what it derived
    /// ([[049-derive-canonical-sortitions-from-the-local-burncha]]). This is the
    /// download hint that says *which* burn block to stand on.
    pub async fn sortition(&mut self, view: ConsensusHash) -> Result<SortitionInfo, SyncError> {
        let sortition = self
            .spread(|peer| async move { peer.sortition(view).await })
            .await?;
        if sortition.consensus_hash != view {
            return Err(SyncError::UnexpectedSortition {
                asked: view,
                answered: sortition.consensus_hash,
            });
        }
        Ok(sortition)
    }

    /// Look up the sortition at a Bitcoin height, from whichever peer is willing.
    pub async fn sortition_at_height(&mut self, height: u64) -> Result<SortitionInfo, SyncError> {
        self.spread(|peer| async move { peer.sortition_at_height(height).await })
            .await
    }

    /// The last Bitcoin height before this one that chose a miner.
    ///
    /// A tenure collects the coinbase of every burn block since that height, so
    /// finding it is what makes a tenure-start block's reward derivable — and what
    /// makes this consensus-visible rather than a hint: the accumulated coinbase is
    /// minted, so a wrong answer here is a wrong balance and a wrong state root.
    pub async fn previous_sortition_height(
        &mut self,
        bitcoin_height: u64,
    ) -> Result<Option<u64>, SyncError> {
        let Some(parent_height) = bitcoin_height.checked_sub(1) else {
            return Ok(None);
        };
        let parent = self.sortition_at_height(parent_height).await?;
        if parent.was_sortition {
            return Ok(Some(parent.bitcoin_height));
        }
        match parent.last_sortition_consensus_hash {
            Some(consensus_hash) => Ok(Some(self.sortition(consensus_hash).await?.bitcoin_height)),
            None => Ok(None),
        }
    }

    /// The coinbase a block's tenure accumulated, or nothing when the block
    /// starts no tenure or no schedule says what a coinbase is worth.
    pub async fn accumulated_coinbase(
        &mut self,
        block: &NakamotoBlock,
        schedule: Option<CoinbaseSchedule>,
        bitcoin_height: u64,
    ) -> Result<Option<u128>, SyncError> {
        let Some(schedule) = schedule.filter(|_| nano_chainstate::starts_new_tenure(block)) else {
            return Ok(None);
        };
        let previous = self.previous_sortition_height(bitcoin_height).await?;
        Ok(Some(schedule.accumulated_at(bitcoin_height, previous)))
    }

    /// Complete a block's execution context with the coinbase its tenure earns.
    pub async fn tenure_coinbase_context(
        &mut self,
        block: &NakamotoBlock,
        schedule: Option<CoinbaseSchedule>,
        mut context: BitcoinBlockContext,
    ) -> Result<BitcoinBlockContext, SyncError> {
        if let Some(accumulated) = self
            .accumulated_coinbase(block, schedule, context.height)
            .await?
        {
            context.accumulated_coinbase = accumulated;
        }
        Ok(context)
    }

    /// Let every peer that had rate-limited be asked again.
    ///
    /// A throttle is set aside for a *round*, and a walk of a few hundred tenures is
    /// long enough that "the round" stops meaning anything: without this a repair that
    /// touched every peer's limit once would run out of peers and stop, when waiting a
    /// moment and starting again is exactly what a rate limit asks for.
    pub fn forgive_throttles(&mut self) {
        self.throttled.clear();
        self.failed.clear();
    }

    /// Whether any peer has rate-limited since the last forgiveness.
    #[must_use]
    pub fn throttled(&self) -> usize {
        self.throttled.len()
    }

    /// The tip a peer reports, from whichever peer is next and willing.
    ///
    /// A claim, not a decision — which is why it is safe to take from a stranger:
    /// it names a block to go and fetch, and the block is then authenticated
    /// against this node's own reward set and burn view before anything is
    /// executed. `PeerPool::choose_source` is where a *tip* gets chosen.
    pub async fn tenure_info(&mut self) -> Result<TenureInfo, SyncError> {
        self.spread(|peer| async move { peer.tenure_info().await })
            .await
    }

    /// One cycle's reward set, from whichever peer is next and willing.
    pub async fn stacker_set(&mut self, cycle: u64) -> Result<StackerSet, SyncError> {
        self.spread(|peer| async move { peer.stacker_set(cycle).await })
            .await
    }

    /// Ask the pool for something it has no method of its own for.
    ///
    /// The rotation, the round's set-asides and the record of who answered are the
    /// point, and they are the same three whatever is being asked — so a caller
    /// with its own protocol (`StackerDB` replication is the one) walks the pool
    /// through here rather than growing a second copy of the loop.
    ///
    /// It hands over a `SyncClient` because that is what carries the endpoint;
    /// a caller wanting a different client for the same peer builds one from
    /// `base_url()`. What it must not do is decide anything on the strength of
    /// which peer answered — every chunk taken this way is still verified against
    /// the writer the slot was assigned to.
    pub async fn ask<T, A, F>(&mut self, ask: A) -> Result<T, SyncError>
    where
        A: FnMut(SyncClient) -> F,
        F: Future<Output = Result<T, SyncError>>,
    {
        self.spread(ask).await
    }

    /// The endpoints this pool holds, so a caller can tell whether discovery has
    /// found anything new without rebuilding it.
    #[must_use]
    pub fn endpoints(&self) -> Vec<String> {
        self.peers
            .iter()
            .map(|peer| peer.base_url().to_string())
            .collect()
    }
}

/// Several peers, and which of them have proved unreliable.
///
/// A node with one peer follows whatever that peer says: the peer decides both
/// what nano sees and what it cannot see. A pool asks all of them, so a peer
/// that stalls, lies or withholds costs nano nothing as long as one other peer
/// is honest.
#[derive(Debug)]
pub struct PeerPool {
    peers: Vec<SyncClient>,
    distrusted: BTreeSet<usize>,
}

/// A node's validated view of a remote Nakamoto tenure stream.
#[derive(Clone, Debug)]
pub struct Node {
    client: SyncClient,
    follower: TenureFollower,
    peer_info: Option<NodeInfo>,
    pox_info: Option<PoxInfo>,
}

/// A consistent read-only snapshot that can be served by the public RPC.
#[derive(Clone, Debug)]
pub struct NodeView {
    pub node_info: NodeInfo,
    pub pox_info: PoxInfo,
    pub tenures: Vec<FollowedTenure>,
}

impl Node {
    /// Construct a node that follows the supplied HTTP peer.
    #[must_use]
    pub fn new(client: SyncClient) -> Self {
        Self {
            follower: TenureFollower::new(client.clone()),
            client,
            peer_info: None,
            pox_info: None,
        }
    }

    /// Return the latest validated peer tenure.
    #[must_use]
    pub const fn latest_tenure(&self) -> Option<&TenureInfo> {
        self.follower.latest()
    }

    /// Fetch and validate the peer's next tenure update.
    pub async fn poll(&mut self) -> Result<Option<FollowedTenure>, SyncError> {
        let followed = self.follower.poll().await?;
        self.peer_info = Some(self.client.node_info().await?);
        self.pox_info = Some(self.client.pox_info().await?);
        Ok(followed)
    }

    /// The validated tenures in hand, oldest first, without copying them.
    #[must_use]
    pub fn tenures(&self) -> &[FollowedTenure] {
        self.follower.history()
    }

    /// Return the latest complete local view after at least one successful poll.
    #[must_use]
    pub fn view(&self) -> Option<NodeView> {
        Some(NodeView {
            node_info: self.peer_info.clone()?,
            pox_info: self.pox_info.clone()?,
            tenures: self.follower.history().to_vec(),
        })
    }
}

impl PeerPool {
    /// Follow the given peers, trusting all of them until one misbehaves.
    #[must_use]
    pub const fn new(peers: Vec<SyncClient>) -> Self {
        Self {
            peers,
            distrusted: BTreeSet::new(),
        }
    }

    /// The peers still worth asking.
    pub fn trusted(&self) -> impl Iterator<Item = (usize, &SyncClient)> {
        self.peers
            .iter()
            .enumerate()
            .filter(|(index, _)| !self.distrusted.contains(index))
    }

    /// Stop asking a peer that served something invalid.
    ///
    /// Serving a bad block is that peer's fault, not a reason to stop
    /// following the chain, so it is set aside rather than raised.
    pub fn distrust(&mut self, peer: usize) {
        self.distrusted.insert(peer);
    }

    /// Whether a peer is still being asked.
    #[must_use]
    pub fn is_trusted(&self, peer: usize) -> bool {
        !self.distrusted.contains(&peer) && peer < self.peers.len()
    }

    /// A peer by index, whether or not it is still trusted.
    #[must_use]
    pub fn peer(&self, peer: usize) -> Option<&SyncClient> {
        self.peers.get(peer)
    }

    /// Ask every trusted peer what its tip is.
    ///
    /// A peer that fails to answer is skipped for this round rather than
    /// distrusted: not answering is usually a restart, not a lie.
    pub async fn candidate_tips(&self) -> Vec<CandidateTip> {
        let mut candidates = Vec::new();
        for (index, client) in self.trusted() {
            let Ok(info) = client.tenure_info().await else {
                continue;
            };
            let Ok(tip) = client.block(info.tip_block_id).await else {
                continue;
            };
            candidates.push(CandidateTip {
                peer: index,
                info,
                header: tip.header,
            });
        }
        candidates
    }

    /// Build a pool from endpoint URLs, keeping the ones that parse.
    ///
    /// This is how peer discovery reaches the chain view: `nano-p2p`'s swarm hands
    /// back the `data_url` of every peer that completed a handshake, and each one
    /// becomes a client here. A URL that does not parse is dropped rather than
    /// raised — it came from a peer's handshake, so a bad one is that peer's
    /// problem and not a reason to have no pool.
    ///
    /// De-duplicated on the *parsed* URL: the same peer arrives both configured
    /// (`http://host:20443/`) and from its own handshake (`http://host:20443`),
    /// spellings a raw-string comparison keeps apart, and a pool holding one
    /// peer twice doubles requests to it and counts it as two in per-peer
    /// serving attribution.
    #[must_use]
    pub fn from_endpoints(endpoints: &[String]) -> Self {
        let mut seen = HashSet::new();
        Self::new(
            endpoints
                .iter()
                .filter_map(|endpoint| Url::parse(endpoint).ok())
                .filter(|url| seen.insert(url.clone()))
                .filter_map(|url| SyncClient::new(url).ok())
                .collect(),
        )
    }

    /// The endpoints this pool holds, so a caller can tell whether discovery has
    /// found anything new without rebuilding it.
    #[must_use]
    pub fn endpoints(&self) -> Vec<String> {
        self.peers
            .iter()
            .map(|peer| peer.base_url().to_string())
            .collect()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.peers.len()
    }

    /// The clients themselves, for a caller that wants to spread work over them
    /// rather than pick one — [`TenureSource`] is what does that.
    #[must_use]
    pub fn into_clients(self) -> Vec<SyncClient> {
        self.peers
    }

    /// Which peer to catch up from this round.
    ///
    /// [`choose_canonical_tip`] decides: this node's own burn view, then the
    /// signer weight its own executed state records, then chain length, then the
    /// lower block identifier, so that every node looking at the same peers lands
    /// on the same block rather than on whichever answered first.
    ///
    /// Both inputs are optional and the reasons differ. No reward set is a
    /// *liveness* choice — said plainly because the difference matters: a set is
    /// not readable until the cycle has been reached, and a node that refused to
    /// sync without one would never acquire one. What makes it safe is that
    /// selection is not the only check: every block this node executes is still
    /// weighed against the set its own state records for that block's cycle, so a
    /// peer offering an unsigned chain wins the round and then fails to have a
    /// single block accepted. No burn view means this node derives no sortitions
    /// of its own and so has nothing local to compare a tip's burnchain against.
    ///
    /// Either way the answer is derived from headers this node fetched and weighed
    /// itself, never from a peer's claim about its own height.
    pub async fn choose_source(
        &self,
        signers: Option<&nano_chainstate::SignerWeights>,
        burn: Option<&dyn BurnView>,
    ) -> Option<(usize, SyncClient)> {
        let candidates = self.candidate_tips().await;
        let chosen = choose_canonical_tip(&candidates, signers, burn)?;
        let peer = chosen.peer;
        self.peer(peer).map(|client| (peer, client.clone()))
    }
}

impl TenureFollower {
    /// Start following a peer without assuming an initial tenure.
    #[must_use]
    pub const fn new(client: SyncClient) -> Self {
        Self {
            client,
            latest: None,
            history: Vec::new(),
            history_bytes: 0,
        }
    }

    /// Return the latest tenure accepted from the peer.
    #[must_use]
    pub const fn latest(&self) -> Option<&TenureInfo> {
        self.latest.as_ref()
    }

    /// Return the validated tenure history retained by this follower.
    #[must_use]
    pub fn history(&self) -> &[FollowedTenure] {
        &self.history
    }

    /// Download the current tenure when the peer's tip has advanced.
    /// The last block this node actually holds, which is what the tenure it
    /// fetches next has to extend. Comparing against the tip the peer reported
    /// would call a tenure this node only partly fetched a fork.
    fn held_tip(&self, latest: &TenureInfo) -> StacksBlockId {
        self.history
            .last()
            .and_then(|previous| previous.blocks.last())
            .map_or(latest.tip_block_id, NakamotoBlock::block_id)
    }

    /// Prepend whatever lies between `held` and the blocks in hand.
    ///
    /// Falling behind by more than one tenure was otherwise unrecoverable: the
    /// follower only ever asks for the peer's latest tenure, whose first block
    /// descends from one it never fetched. Parent links cross tenure
    /// boundaries like any other, so the gap closes the same way the tip is
    /// reached — and failing to close it leaves the round to the fork check.
    async fn bridge_gap(&self, blocks: &mut Vec<NakamotoBlock>, held: StacksBlockId) {
        let Some(first) = blocks.first() else { return };
        if first.header.parent_block_id == held {
            return;
        }
        let first = first.block_id();
        let mut bridge = Vec::new();
        if let Err(error) = self
            .client
            .walk_back(&mut bridge, first, held, BRIDGE_WALK)
            .await
        {
            eprintln!("bridging to the peer's tenure failed: {error}");
            return;
        }
        bridge.append(blocks);
        *blocks = bridge;
    }

    /// Carry the tenure this node already holds forward to the peer's tip.
    ///
    /// A tenure in hand only needs the blocks added since, and asking for it
    /// whole is what rate limits a follower out of tip: a mainnet tenure runs
    /// to hundreds of blocks. `None` means the incremental walk did not reach
    /// the tip, and the caller falls back to fetching the tenure.
    async fn extend_held_tenure(
        &mut self,
        info: &TenureInfo,
    ) -> Result<Option<FollowedTenure>, SyncError> {
        let previous = self.history.last().ok_or(SyncError::Fork)?;
        let held = previous.blocks.len();
        let mut blocks = previous.blocks.as_ref().clone();
        if self
            .client
            .extend_to_tenure_tip(&mut blocks, info.tip_block_id)
            .await
            .is_err()
            || !blocks
                .windows(2)
                .skip(held.saturating_sub(1))
                .all(|pair| pair[1].validate_successor(&pair[0].header).is_ok())
        {
            return Ok(None);
        }
        let block_consensus_hash = blocks
            .last()
            .ok_or(SyncError::EmptyTenure)?
            .header
            .consensus_hash;
        let sortition = self.client.sortition(block_consensus_hash).await?;
        if !sortition.was_sortition {
            return Err(SyncError::InvalidSortition);
        }
        let followed = FollowedTenure::new(info.clone(), sortition, Arc::new(blocks))?;
        self.record(followed.clone());
        Ok(Some(followed))
    }

    pub async fn poll(&mut self) -> Result<Option<FollowedTenure>, SyncError> {
        for _ in 0..3 {
            let requested_info = self.client.tenure_info().await?;
            if self
                .latest
                .as_ref()
                .is_some_and(|latest| latest.tip_block_id == requested_info.tip_block_id)
            {
                return Ok(None);
            }
            if self.latest.as_ref().is_some_and(|latest| {
                latest.tenure_start_block_id == requested_info.tenure_start_block_id
            }) && let Some(followed) = self.extend_held_tenure(&requested_info).await?
            {
                return Ok(Some(followed));
            }
            let mut blocks = self
                .client
                .tenure(requested_info.tenure_start_block_id, None)
                .await?;
            let info = self.client.tenure_info().await?;
            if info.tenure_start_block_id != requested_info.tenure_start_block_id {
                continue;
            }
            // Reaching the tip is worth trying and not worth failing over: the
            // blocks already in hand extend this node's chain, and a round
            // that throws them away because the last few could not be fetched
            // makes no progress at all — which is how a rate-limited follower
            // stops moving while reporting nothing wrong.
            if let Err(error) = self
                .client
                .extend_to_tenure_tip(&mut blocks, info.tip_block_id)
                .await
            {
                eprintln!("reaching the tenure's tip failed, applying what it answered: {error}");
            }
            if blocks.last().map(NakamotoBlock::block_id) != Some(info.tip_block_id) {
                let tip = self.client.block(info.tip_block_id).await?;
                let parent = blocks.last().ok_or(SyncError::EmptyTenure)?;
                if tip.validate_successor(&parent.header).is_ok() {
                    blocks.push(tip);
                }
            }
            if let Some(latest) = &self.latest {
                validate_tenure_transition(latest, &info)?;
                if latest.tenure_start_block_id == info.tenure_start_block_id {
                    let previous = self.history.last().ok_or(SyncError::Fork)?;
                    // A round may answer with fewer blocks of the same tenure
                    // than the one before it did — a peer that rate limits cuts
                    // the walk short — and that is the same chain, not a fork.
                    if !blocks.starts_with(previous.blocks.as_ref())
                        && !previous.blocks.starts_with(&blocks)
                    {
                        return Err(SyncError::Fork);
                    }
                    if blocks.len() < previous.blocks.len() {
                        blocks.clone_from(previous.blocks.as_ref());
                    }
                } else {
                    let held = self.held_tip(latest);
                    self.bridge_gap(&mut blocks, held).await;
                    if blocks
                        .first()
                        .is_none_or(|block| block.header.parent_block_id != held)
                    {
                        return Err(SyncError::Fork);
                    }
                }
            }
            let block_consensus_hash = blocks
                .last()
                .map(|block| block.header.consensus_hash)
                .ok_or(SyncError::EmptyTenure)?;
            let sortition = self.client.sortition(block_consensus_hash).await?;
            if !sortition.was_sortition {
                return Err(SyncError::InvalidSortition);
            }
            let followed = FollowedTenure::new(info.clone(), sortition, Arc::new(blocks))?;
            self.record(followed.clone());
            return Ok(Some(followed));
        }
        Err(SyncError::UnstableTip)
    }

    fn record(&mut self, followed: FollowedTenure) {
        if let Some(replaced) = self.history.pop_if(|latest| {
            latest.info.tenure_start_block_id == followed.info.tenure_start_block_id
        }) {
            self.history_bytes -= replaced.wire_bytes;
        }
        self.latest = Some(followed.info.clone());
        while !self.history.is_empty()
            && (self.history.len() >= FOLLOWED_TENURE_HISTORY_ITEMS
                || followed.wire_bytes
                    > FOLLOWED_TENURE_HISTORY_BYTES.saturating_sub(self.history_bytes))
        {
            self.history_bytes -= self.history.remove(0).wire_bytes;
        }
        self.history_bytes += followed.wire_bytes;
        self.history.push(followed);
    }
}

/// How long a peer's own `Retry-After` asks this node to wait.
///
/// Honoured *as given*, up to [`RETRY_AFTER_CEILING`]. Capping it at the backoff
/// this node invents for itself meant a peer asking for a minute was asked again
/// two seconds later, which earns another 429 and keeps earning them — a mainnet
/// accounting rebuild ran 1h45m against one rate-limited peer with six seconds of
/// CPU to show for it. The ceiling is therefore a bound on a hostile or broken
/// header rather than on a real one, and it is a separate function so that bound
/// can be checked without waiting two minutes for it.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(std::time::Duration::from_secs)
        .map(|told| told.min(RETRY_AFTER_CEILING))
}

fn validate_tenure(
    start_block_id: StacksBlockId,
    blocks: &[NakamotoBlock],
) -> Result<(), SyncError> {
    let first = blocks.first().ok_or(SyncError::EmptyTenure)?;
    if first.block_id() != start_block_id {
        return Err(SyncError::TenureStart);
    }
    for pair in blocks.windows(2) {
        pair[0]
            .validate_successor(&pair[1].header)
            .map_err(SyncError::TenureLink)?;
    }
    Ok(())
}

fn validate_tenure_transition(previous: &TenureInfo, next: &TenureInfo) -> Result<(), SyncError> {
    if previous.tenure_start_block_id == next.tenure_start_block_id {
        return Ok(());
    }
    if next.parent_consensus_hash == previous.consensus_hash
        && next.parent_tenure_start_block_id == previous.tenure_start_block_id
    {
        Ok(())
    } else {
        Err(SyncError::Fork)
    }
}

#[derive(Deserialize)]
struct NodeInfoWire {
    burn_block_height: u64,
    stacks_tip_height: u64,
    stacks_tip: String,
    stacks_tip_consensus_hash: String,
    network_id: u32,
}

#[derive(Deserialize)]
struct TenureInfoWire {
    consensus_hash: String,
    tenure_start_block_id: String,
    parent_consensus_hash: String,
    parent_tenure_start_block_id: String,
    tip_block_id: String,
    tip_height: u64,
    reward_cycle: u64,
}

#[derive(Deserialize)]
struct PoxInfoWire {
    first_burnchain_block_height: u64,
    current_burnchain_block_height: u64,
    prepare_phase_block_length: u32,
    reward_phase_block_length: u32,
    reward_slots: u32,
    rejection_fraction: Option<u64>,
    #[serde(default)]
    contract_versions: Vec<PoxContractVersionWire>,
    #[serde(default)]
    epochs: Vec<PoxEpochWire>,
}

impl PoxInfoWire {
    /// The first height inside a named epoch, which is when the locks the
    /// preceding `PoX` contract holds expire.
    fn unlock_height_after(&self, epoch_id: &str) -> Option<u32> {
        self.epochs
            .iter()
            .find(|epoch| epoch.epoch_id == epoch_id)
            .and_then(|epoch| u32::try_from(epoch.start_height).ok())
            .map(|height| height.saturating_add(1))
    }
}

#[derive(Deserialize)]
struct PoxContractVersionWire {
    activation_burnchain_block_height: u32,
    contract_id: String,
}

#[derive(Deserialize)]
struct PoxEpochWire {
    epoch_id: String,
    start_height: u64,
}

#[derive(Deserialize)]
struct StackerSetResponseWire {
    stacker_set: StackerSetWire,
}

#[derive(Deserialize)]
struct StackerSetWire {
    pox_ustx_threshold: u128,
    #[serde(default)]
    sbtc_address: Option<Value>,
    signers: Vec<StackerWire>,
}

#[derive(Deserialize)]
struct StackerWire {
    signing_key: String,
    weight: u32,
}

#[derive(Deserialize)]
struct BlockUploadWire {
    accepted: bool,
    stacks_block_id: String,
}

#[derive(Deserialize)]
#[cfg(feature = "mempool")]
struct AccountWire {
    nonce: u64,
    /// The balance the account can spend now, as thirty-two hexadecimal digits.
    balance: String,
}

#[derive(Deserialize)]
struct SortitionInfoWire {
    burn_block_hash: String,
    burn_block_height: u64,
    burn_header_timestamp: u64,
    sortition_id: String,
    parent_sortition_id: String,
    consensus_hash: String,
    was_sortition: bool,
    miner_pk_hash160: Option<String>,
    stacks_parent_ch: Option<String>,
    last_sortition_ch: Option<String>,
    committed_block_hash: Option<String>,
    vrf_seed: Option<String>,
}

#[derive(Deserialize)]
struct ForkInfoWire {
    burn_block_height: u64,
    consensus_hash: String,
    was_sortition: bool,
    first_block_mined: Option<String>,
    /// The whole tenure, consensus-serialized as one length-prefixed vector and
    /// hex-encoded — `prefix_opt_hex_codec` on stacks-core's side.
    ///
    /// Already on the wire of every `fork_info` answer a fork check makes, and
    /// discarded until now. That is what makes addressing a tenure by its burn view
    /// cost nothing beyond the request that a forward schedule needs anyway.
    nakamoto_blocks: Option<String>,
}

/// Read a length-prefixed, hex-encoded block vector out of a `fork_info` answer.
fn decode_tenure_blocks(encoded: &str) -> Result<Vec<NakamotoBlock>, SyncError> {
    let bytes = decode_hex(encoded.strip_prefix("0x").unwrap_or(encoded))?;
    let (count, mut offset) = bytes
        .get(..4)
        .and_then(|prefix| <[u8; 4]>::try_from(prefix).ok())
        .map(|prefix| (u32::from_be_bytes(prefix), 4))
        .ok_or(SyncError::InvalidHash)?;
    let mut blocks = Vec::new();
    for _ in 0..count {
        let (block, consumed) =
            NakamotoBlock::decode_prefix(bytes.get(offset..).ok_or(SyncError::InvalidHash)?)
                .map_err(SyncError::Block)?;
        offset = offset.checked_add(consumed).ok_or(SyncError::InvalidHash)?;
        blocks.push(block);
    }
    Ok(blocks)
}

/// Wire tag for a mempool query that lists the transactions already known
/// (`core/mempool.rs`, `MemPoolSyncDataID::TxTags`).
#[cfg(feature = "mempool")]
const MEMPOOL_QUERY_TX_TAGS: u8 = 0x02;

/// Split a mempool page into its transactions and the identifier of the page
/// after it.
///
/// Nothing frames either: the transactions run back to back, and a peer with
/// more to send appends the next page's identifier, which is why a stream that
/// ends on a transaction boundary is the last page (`core/mempool.rs`,
/// `decode_tx_stream`).
#[cfg(feature = "mempool")]
fn decode_mempool_page(body: &[u8]) -> Result<(Vec<Transaction>, Option<Sha256Sum>), SyncError> {
    let mut stream = body;
    let mut transactions = Vec::new();
    while !stream.is_empty() {
        let Ok((transaction, consumed)) = Transaction::decode(stream) else {
            let page = stream.try_into().map_err(|_| SyncError::InvalidMempool)?;
            return Ok((transactions, Some(Sha256Sum::from_bytes(page))));
        };
        stream = stream.get(consumed..).ok_or(SyncError::InvalidMempool)?;
        transactions.push(transaction);
    }
    Ok((transactions, None))
}

fn parse_block_id(value: &str) -> Result<StacksBlockId, SyncError> {
    parse_hex(value).map(StacksBlockId::from_bytes)
}

fn parse_block_hash(value: &str) -> Result<BlockHeaderHash, SyncError> {
    parse_hex(value).map(BlockHeaderHash::from_bytes)
}

fn parse_consensus_hash(value: &str) -> Result<ConsensusHash, SyncError> {
    parse_hex(value).map(ConsensusHash::from_bytes)
}

fn parse_prefixed_block_hash(value: &str) -> Result<BlockHeaderHash, SyncError> {
    parse_prefixed_hex(value).map(BlockHeaderHash::from_bytes)
}

fn parse_prefixed_bitcoin_block_hash(value: &str) -> Result<BitcoinHeaderHash, SyncError> {
    parse_prefixed_hex(value).map(BitcoinHeaderHash::from_bytes)
}

fn parse_prefixed_sortition_id(value: &str) -> Result<SortitionId, SyncError> {
    parse_prefixed_hex(value).map(SortitionId::from_bytes)
}

fn parse_prefixed_consensus_hash(value: &str) -> Result<ConsensusHash, SyncError> {
    parse_prefixed_hex(value).map(ConsensusHash::from_bytes)
}

fn parse_prefixed_hash160(value: &str) -> Result<Hash160, SyncError> {
    parse_prefixed_hex(value).map(Hash160::from_bytes)
}

fn parse_sortition_info(sortition: &SortitionInfoWire) -> Result<SortitionInfo, SyncError> {
    Ok(SortitionInfo {
        bitcoin_block_hash: parse_prefixed_bitcoin_block_hash(&sortition.burn_block_hash)?,
        bitcoin_height: sortition.burn_block_height,
        bitcoin_timestamp: sortition.burn_header_timestamp,
        sortition_id: parse_prefixed_sortition_id(&sortition.sortition_id)?,
        parent_sortition_id: parse_prefixed_sortition_id(&sortition.parent_sortition_id)?,
        consensus_hash: parse_prefixed_consensus_hash(&sortition.consensus_hash)?,
        was_sortition: sortition.was_sortition,
        miner_public_key_hash: sortition
            .miner_pk_hash160
            .as_deref()
            .map(parse_prefixed_hash160)
            .transpose()?,
        stacks_parent_consensus_hash: sortition
            .stacks_parent_ch
            .as_deref()
            .map(parse_prefixed_consensus_hash)
            .transpose()?,
        last_sortition_consensus_hash: sortition
            .last_sortition_ch
            .as_deref()
            .map(parse_prefixed_consensus_hash)
            .transpose()?,
        committed_block_hash: sortition
            .committed_block_hash
            .as_deref()
            .map(parse_prefixed_block_hash)
            .transpose()?,
        vrf_seed: sortition
            .vrf_seed
            .as_deref()
            .map(parse_prefixed_hex)
            .transpose()?,
        // This type also carries diagnostics this node derives for its own RPC.
        // A peer's copy is neither authentication evidence nor worth retaining in
        // the 4,096-entry sortition cache, so unknown additive fields are discarded.
        mining_competition: None,
    })
}

fn parse_stacker_set(value: StackerSetWire) -> Result<StackerSet, SyncError> {
    let sbtc_address = value
        .sbtc_address
        .as_ref()
        .map(parse_waterfall_address)
        .transpose()?;
    let mut signers = value
        .signers
        .into_iter()
        .map(|signer| {
            Ok(Signer {
                public_key: StacksPublicKey::from_bytes(&parse_hex::<33>(&signer.signing_key)?)?,
                weight: signer.weight,
            })
        })
        .collect::<Result<Vec<_>, SyncError>>()?;
    signers.sort_by_key(|signer| signer.public_key.to_bytes_compressed());
    Ok(StackerSet {
        pox_ustx_threshold: value.pox_ustx_threshold,
        sbtc_address,
        signer_set: SignerSet::new(signers)?,
    })
}

fn parse_waterfall_address(value: &Value) -> Result<PoxAddress, SyncError> {
    let address = value.get("Addr32").ok_or(SyncError::InvalidRewardSet)?;
    let (mainnet, address_type, bytes): (bool, String, [u8; 32]) =
        serde_json::from_value(address.clone()).map_err(|_| SyncError::InvalidRewardSet)?;
    if address_type != "P2TR" {
        return Err(SyncError::InvalidRewardSet);
    }
    Ok(PoxAddress::Addr32 {
        mainnet,
        address_type: PoxAddressType32::P2tr,
        bytes,
    })
}

fn parse_prefixed_hex<const LENGTH: usize>(value: &str) -> Result<[u8; LENGTH], SyncError> {
    parse_hex(value.strip_prefix("0x").ok_or(SyncError::InvalidHash)?)
}

/// Read a fixed-length hash, with or without the `0x` a JSON API may prefix.
///
/// Tolerant on purpose, and it had to become so: `/v3/tenures/fork_info` states every
/// hash prefixed — stacks-core through `prefix_hex`, and nano's own RPC through
/// `TenureForkInfoWire` — while `/v3/sortitions` on the same peer does not. A parser
/// that insisted on the bare form rejected every real `fork_info` answer, so the
/// only fork check that ever passed was the one against a test peer that happened to
/// serve the bare form. The prefix carries no information; refusing it only decided
/// which peers nano could read.
fn parse_hex<const LENGTH: usize>(value: &str) -> Result<[u8; LENGTH], SyncError> {
    let value = value.strip_prefix("0x").unwrap_or(value);
    if value.len() != LENGTH * 2 {
        return Err(SyncError::InvalidHash);
    }
    let mut bytes = [0; LENGTH];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| SyncError::InvalidHash)?;
    }
    Ok(bytes)
}

/// The same for a body whose length is whatever the peer sent.
fn decode_hex(value: &str) -> Result<Vec<u8>, SyncError> {
    if !value.len().is_multiple_of(2) {
        return Err(SyncError::InvalidHash);
    }
    (0..value.len() / 2)
        .map(|index| {
            u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
                .map_err(|_| SyncError::InvalidHash)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, env, fs, path::Path, sync::Arc, time::Duration};

    use reqwest::Url;
    use tokio::time::{sleep, timeout};

    use super::{
        BlockUploadWire, BurnView, ByteCache, CandidateTip, FOLLOWED_TENURE_HISTORY_BYTES,
        FOLLOWED_TENURE_HISTORY_ITEMS, FollowedTenure, MAX_TENURE_RESPONSE_BYTES, PoxInfo,
        RATE_LIMIT_RETRIES, RETRY_AFTER_CEILING, Signer, SignerSet, SortitionInfo,
        SortitionInfoWire, StackerSetResponseWire, StackerSetWire, SyncClient, SyncError,
        TenureSource, add_tenure_wire_bytes, block_cache, choose_canonical_tip, parse_block_hash,
        parse_block_id, parse_consensus_hash, parse_prefixed_hash160, parse_sortition_info,
        parse_stacker_set, retry_after, validate_tenure, validate_tenure_transition,
    };
    use super::{Node, TenureFollower, TenureInfo};
    use nano_chainstate::{NakamotoBlock, TenureError};
    use nano_mempool::Mempool;
    use nano_primitives::{BitcoinHeaderHash, ConsensusHash, Network, SortitionId, StacksBlockId};

    #[test]
    fn node_starts_without_a_followed_tenure() {
        let client = SyncClient::new(Url::parse("http://127.0.0.1:20443/").expect("valid URL"))
            .expect("create sync client");

        assert!(Node::new(client).latest_tenure().is_none());
    }

    fn followed_tenure(index: u8, wire_bytes: usize) -> FollowedTenure {
        FollowedTenure {
            info: TenureInfo {
                consensus_hash: ConsensusHash::from_bytes([index; 20]),
                tenure_start_block_id: StacksBlockId::from_bytes([index; 32]),
                parent_consensus_hash: ConsensusHash::from_bytes([index.saturating_sub(1); 20]),
                parent_tenure_start_block_id: StacksBlockId::from_bytes(
                    [index.saturating_sub(1); 32],
                ),
                tip_block_id: StacksBlockId::from_bytes([index; 32]),
                tip_height: u64::from(index),
                reward_cycle: 1,
            },
            sortition: SortitionInfo {
                bitcoin_block_hash: BitcoinHeaderHash::from_bytes([index; 32]),
                bitcoin_height: u64::from(index),
                bitcoin_timestamp: 0,
                sortition_id: SortitionId::from_bytes([index; 32]),
                parent_sortition_id: SortitionId::from_bytes([index.saturating_sub(1); 32]),
                consensus_hash: ConsensusHash::from_bytes([index; 20]),
                was_sortition: true,
                miner_public_key_hash: None,
                stacks_parent_consensus_hash: None,
                last_sortition_consensus_hash: None,
                committed_block_hash: None,
                vrf_seed: None,
                mining_competition: None,
            },
            blocks: Arc::new(Vec::new()),
            wire_bytes,
        }
    }

    #[test]
    fn followed_history_is_bounded_by_count_and_aggregate_bytes() {
        let client = SyncClient::new(Url::parse("http://127.0.0.1:20443/").expect("valid URL"))
            .expect("create sync client");
        let mut follower = TenureFollower::new(client);
        for index in 0..=u8::try_from(FOLLOWED_TENURE_HISTORY_ITEMS).expect("small item limit") {
            follower.record(followed_tenure(index, 1));
        }
        assert_eq!(follower.history.len(), FOLLOWED_TENURE_HISTORY_ITEMS);
        assert_eq!(follower.history_bytes, FOLLOWED_TENURE_HISTORY_ITEMS);
        assert_eq!(
            follower.history[0].info.tenure_start_block_id,
            StacksBlockId::from_bytes([1; 32])
        );

        let client = SyncClient::new(Url::parse("http://127.0.0.1:20443/").expect("valid URL"))
            .expect("create sync client");
        let mut follower = TenureFollower::new(client);
        for index in 0..=10 {
            follower.record(followed_tenure(index, MAX_TENURE_RESPONSE_BYTES));
        }
        assert_eq!(follower.history.len(), 10);
        assert_eq!(follower.history_bytes, FOLLOWED_TENURE_HISTORY_BYTES);
        assert_eq!(
            follower.history[0].info.tenure_start_block_id,
            StacksBlockId::from_bytes([1; 32])
        );

        let replacement = followed_tenure(10, 1);
        follower.record(replacement);
        assert_eq!(follower.history.len(), 10);
        assert_eq!(follower.history_bytes, 9 * MAX_TENURE_RESPONSE_BYTES + 1);
    }

    #[test]
    fn one_followed_tenure_cannot_grow_past_its_response_limit() {
        assert!(matches!(
            add_tenure_wire_bytes(MAX_TENURE_RESPONSE_BYTES - 1, 1),
            Ok(MAX_TENURE_RESPONSE_BYTES)
        ));
        assert!(matches!(
            add_tenure_wire_bytes(MAX_TENURE_RESPONSE_BYTES, 1),
            Err(SyncError::ResponseTooLarge {
                limit: MAX_TENURE_RESPONSE_BYTES
            })
        ));
    }

    #[test]
    fn inventory_cycles_keep_the_stock_modulo_one_boundary_after_the_waterfall() {
        let calendar = PoxInfo {
            first_bitcoin_height: 666_050,
            bitcoin_height: 962_434,
            prepare_phase_length: 100,
            reward_phase_length: 2_000,
            reward_slots: 4_000,
            rejection_fraction: None,
            pox_5_activation_height: Some(960_232),
            v1_unlock_height: None,
            v2_unlock_height: None,
            v3_unlock_height: None,
        };

        assert_eq!(calendar.inventory_cycle_start(962_150), Some(960_051));
        assert_eq!(calendar.inventory_cycle_start(962_151), Some(962_151));
        assert_eq!(calendar.inventory_cycle_start(962_434), Some(962_151));
        assert_eq!(calendar.inventory_cycle_start(666_050), None);
    }

    /// The same peer arrives configured with a trailing slash and discovered
    /// without one; a pool holding it twice doubles requests to it and counts
    /// it as two in serving attribution.
    #[test]
    fn a_peer_configured_and_discovered_is_pooled_once() {
        let pool = super::PeerPool::from_endpoints(&[
            "http://172.96.141.17:20443/".to_owned(),
            "http://172.96.141.17:20443".to_owned(),
            "http://108.130.44.244:20443/".to_owned(),
        ]);

        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn hashes_must_be_exact_lower_or_upper_hex() {
        assert!(parse_block_id("00").is_err());
        assert!(parse_consensus_hash("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").is_err());
        assert_eq!(
            parse_block_id("0000000000000000000000000000000000000000000000000000000000000000")
                .expect("valid block ID")
                .as_bytes(),
            &[0; 32]
        );
        assert_eq!(
            parse_block_hash("0000000000000000000000000000000000000000000000000000000000000000")
                .expect("valid block hash")
                .as_bytes(),
            &[0; 32]
        );
        assert_eq!(
            parse_consensus_hash("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
                .expect("valid uppercase consensus hash")
                .as_bytes(),
            &[0xaa; 20]
        );
        assert!(matches!(
            parse_consensus_hash("00"),
            Err(SyncError::InvalidHash)
        ));
        assert_eq!(
            parse_prefixed_hash160("0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
                .expect("valid prefixed hash")
                .as_bytes(),
            &[0xaa; 20]
        );
        assert!(parse_prefixed_hash160("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").is_err());
    }

    #[test]
    fn peer_mining_diagnostics_are_discarded_before_the_sortition_cache() {
        let wire: SortitionInfoWire = serde_json::from_value(serde_json::json!({
            "burn_block_hash": format!("0x{}", "11".repeat(32)),
            "burn_block_height": 123,
            "burn_header_timestamp": 456,
            "sortition_id": format!("0x{}", "22".repeat(32)),
            "parent_sortition_id": format!("0x{}", "33".repeat(32)),
            "consensus_hash": format!("0x{}", "44".repeat(20)),
            "was_sortition": true,
            "mining_competition": {
                "winner_txid": "not even hexadecimal",
                "participants": vec![serde_json::json!({
                    "txid": false,
                    "arbitrary_peer_bytes": "ff".repeat(1_024),
                }); 4_097],
            },
        }))
        .expect("unknown peer diagnostics are skipped");
        let sortition = parse_sortition_info(&wire).expect("stock sortition fields remain valid");

        assert_eq!(sortition.bitcoin_height, 123);
        assert_eq!(sortition.consensus_hash.as_bytes(), &[0x44; 20]);
        assert_eq!(sortition.mining_competition, None);
    }

    /// A hash reads the same whether or not the peer prefixed it.
    ///
    /// Not a nicety. `/v3/tenures/fork_info` states every hash `0x`-prefixed — both
    /// stacks-core, through `prefix_hex`, and nano's own RPC — while `/v3/sortitions` on
    /// the same peer states them bare. A parser that insisted on the bare form rejected
    /// every real `fork_info` answer, which meant the only fork check that ever parsed
    /// was the one against a test peer that happened to serve them bare.
    #[test]
    fn a_hash_reads_with_or_without_the_prefix_a_json_api_puts_on_it() {
        let bare = "da8c25d9d380c2f083193535594bb127186e67cd";
        assert_eq!(
            parse_consensus_hash(bare).expect("a bare consensus hash"),
            parse_consensus_hash(&format!("0x{bare}")).expect("a prefixed consensus hash")
        );
        // The length is still checked against the hash and not against the string, so a
        // prefix does not buy an extra byte either way.
        assert!(parse_consensus_hash(&format!("0x{bare}00")).is_err());
        assert!(parse_consensus_hash(&bare[2..]).is_err());
    }

    /// A tenure asked for by burn view is refused if it answers for another one.
    ///
    /// The check that makes a *forward* download safe to spread over strangers. A block
    /// fetched by identifier is self-verifying, because the identifier is the hash of
    /// what comes back; a tenure named by the burn view that elected it is not, so the
    /// only thing that can refuse a substitution is the view each block's own header
    /// states. Decoded here from the same length-prefixed hex vector the endpoint sends.
    #[test]
    fn a_scheduled_tenure_is_read_back_against_the_view_it_was_asked_for() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../nano-conformance/fixtures/nakamoto/blocks");
        let path = fs::read_dir(directory)
            .expect("read fixture blocks")
            .map(|entry| entry.expect("fixture block").path())
            .min()
            .expect("a fixture block");
        let block = NakamotoBlock::decode(&fs::read(path).expect("read fixture block"))
            .expect("decode fixture block");
        let view = block.header.consensus_hash;
        let mut bytes = 1u32.to_be_bytes().to_vec();
        bytes.extend(block.encode());
        let encoded = bytes.iter().fold("0x".to_owned(), |mut hex, byte| {
            use std::fmt::Write;

            write!(hex, "{byte:02x}").expect("writing to a string cannot fail");
            hex
        });
        let decoded = super::decode_tenure_blocks(&encoded).expect("the block vector decodes");
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].header.consensus_hash, view);
        assert_eq!(decoded[0].block_id(), block.block_id());
        // An empty vector is what a peer answers for a burn block that elected nobody,
        // and it decodes rather than failing: the caller turns it into `EmptyTenure` and
        // skips the offset, because a burn block with no sortition is not a gap.
        assert!(
            super::decode_tenure_blocks("0x00000000")
                .expect("an empty vector decodes")
                .is_empty()
        );
        assert!(super::decode_tenure_blocks("0x0000000101").is_err());
    }

    #[test]
    fn recorded_waterfall_stacker_set_parses_and_orders_signers() {
        #[derive(serde::Deserialize)]
        struct Fixture {
            stacker_set: StackerSetWire,
        }

        let fixture: Fixture =
            serde_json::from_slice(include_bytes!("../tests/waterfall-stacker-set.json"))
                .expect("parse recorded stacker set");
        let set = parse_stacker_set(fixture.stacker_set).expect("parse active stacker set");
        assert_eq!(set.pox_ustx_threshold, 14_666_666_667);
        assert_eq!(set.signer_set.weights(), vec![8, 8, 7, 7]);
        assert!(matches!(
            set.sbtc_address,
            Some(nano_address::PoxAddress::Addr32 { .. })
        ));
    }

    #[test]
    fn version_zero_stacker_set_parses_without_waterfall_address() {
        let response: StackerSetResponseWire = serde_json::from_str(
            r#"{"stacker_set":{"pox_ustx_threshold":50000000000,"reward_set_version":0,"rewarded_addresses":[],"signers":[{"signing_key":"02007311430123d4cad97f4f7e86e023b28143130a18099ecf094d36fef0f6135c","stacked_amt":2566784000000000,"weight":2},{"signing_key":"031a4d9f4903da97498945a4e01a5023a1d53bc96ad670bfe03adf8a06c52e6380","stacked_amt":2566784000000000,"weight":2},{"signing_key":"035249137286c077ccee65ecc43e724b9b9e5a588e3d7f51e3b62f9624c2a49e46","stacked_amt":2566784000000000,"weight":2}]}}"#,
        )
        .expect("parse version-zero response");
        let set = parse_stacker_set(response.stacker_set).expect("parse signer set");

        assert_eq!(set.sbtc_address, None);
        assert_eq!(set.signer_set.weights(), vec![2, 2, 2]);
    }

    #[test]
    fn block_upload_acknowledgement_requires_a_valid_block_id() {
        let acknowledgement: BlockUploadWire = serde_json::from_str(
            r#"{"accepted":true,"stacks_block_id":"0000000000000000000000000000000000000000000000000000000000000000"}"#,
        )
        .expect("parse upload acknowledgement");
        assert!(acknowledgement.accepted);
        assert_eq!(
            parse_block_id(&acknowledgement.stacks_block_id)
                .expect("valid block ID")
                .as_bytes(),
            &[0; 32]
        );
    }

    /// A rate limit is a reason to wait, not a reason for a node to give up
    /// starting — so it has to be told apart from every other HTTP failure.
    #[tokio::test]
    async fn a_rate_limit_is_distinguishable_from_a_failure() {
        let peer = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let address = peer.local_addr().expect("the bound address");
        tokio::spawn(async move {
            // The client retries a 429 before it gives up, so the peer has to
            // keep saying it; the last answer is what the caller then sees.
            for answered in 0.. {
                let (mut stream, _) = peer.accept().await.expect("a request");
                let status = if answered <= RATE_LIMIT_RETRIES {
                    "429 Too Many Requests"
                } else {
                    "404 Not Found"
                };
                let response = format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\n\r\n");
                let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await;
            }
        });
        let client =
            SyncClient::new(Url::parse(&format!("http://{address}/")).expect("a base url"))
                .expect("a client");

        let limited = client.node_info().await.expect_err("the peer said 429");
        assert!(limited.is_rate_limited(), "{limited}");
        let missing = client.node_info().await.expect_err("the peer said 404");
        assert!(!missing.is_rate_limited(), "{missing}");
    }

    /// A fork check has to survive a page bigger than a metadata response.
    ///
    /// Every `fork_info` entry carries its tenure's whole block body hex-encoded,
    /// so a page is tenures and not hashes: a measured mainnet page is 10.2 MB,
    /// well past the 4 MiB [`MAX_JSON_RESPONSE_BYTES`] the default `get` applies.
    /// Bounded as metadata, the walk did not degrade — it failed outright, which
    /// left a follower that had executed onto a losing branch with no way to find
    /// where it parted and no way off it.
    #[tokio::test]
    async fn a_fork_info_page_larger_than_a_json_response_is_still_read() {
        let peer = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let address = peer.local_addr().expect("the bound address");
        let stop = ConsensusHash::from_bytes([0x11; 20]);
        let start = ConsensusHash::from_bytes([0x22; 20]);
        // One entry per bound, and the padding is the tenure body a real answer
        // carries: enough of it that the page cannot fit the metadata limit.
        let padding = "ab".repeat(super::MAX_JSON_RESPONSE_BYTES);
        let body = format!(
            "[{{\"burn_block_height\":2,\"consensus_hash\":\"0x{start}\",\
             \"was_sortition\":true,\"first_block_mined\":null,\
             \"nakamoto_blocks\":\"0x{padding}\"}},\
             {{\"burn_block_height\":1,\"consensus_hash\":\"0x{stop}\",\
             \"was_sortition\":true,\"first_block_mined\":null}}]"
        );
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = peer.accept().await.expect("a request");
                // Drained before answering: a socket closed with the request still
                // unread is reset rather than finished, and the client then sees a
                // connection error instead of the body under test.
                let mut request = [0_u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut request).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await;
                let _ = tokio::io::AsyncWriteExt::shutdown(&mut stream).await;
            }
        });
        let client =
            SyncClient::new(Url::parse(&format!("http://{address}/")).expect("a base url"))
                .expect("a client");

        let walked = client
            .tenure_fork_info(start, stop)
            .await
            .expect("a fork_info page above the metadata limit is still a fork answer");
        assert_eq!(
            walked
                .iter()
                .map(|entry| entry.consensus_hash)
                .collect::<Vec<_>>(),
            vec![start, stop],
            "the walk has to reach the stop it was given"
        );
    }

    #[tokio::test]
    async fn peer_response_bytes_are_bounded_before_and_during_streaming() {
        let peer = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let address = peer.local_addr().expect("the bound address");
        let server = tokio::spawn(async move {
            for response in [
                "HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n4\r\nabcd\r\n0\r\n\r\n",
                "HTTP/1.1 200 OK\r\ncontent-length: 3\r\nconnection: close\r\n\r\nabc",
            ] {
                let (mut stream, _) = peer.accept().await.expect("a request");
                tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes())
                    .await
                    .expect("answer request");
            }
        });
        let client = reqwest::Client::new();
        let url = format!("http://{address}/");

        for edge in ["declared", "chunked"] {
            let response = client.get(&url).send().await.expect("peer response");
            assert!(
                matches!(
                    super::bounded_response(response, 3).await,
                    Err(SyncError::ResponseTooLarge { limit: 3 })
                ),
                "{edge} overflow was admitted"
            );
        }
        let response = client.get(url).send().await.expect("recovery response");
        assert_eq!(
            super::bounded_response(response, 3)
                .await
                .expect("a response at the limit"),
            b"abc"
        );
        server.await.expect("peer exits");
    }

    #[tokio::test]
    async fn a_cyclic_mempool_cursor_retains_no_unbounded_page_backlog() {
        let peer = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let address = peer.local_addr().expect("the bound address");
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = peer.accept().await.expect("a request");
                tokio::io::AsyncWriteExt::write_all(
                    &mut stream,
                    b"HTTP/1.1 200 OK\r\ncontent-length: 32\r\nconnection: close\r\n\r\n",
                )
                .await
                .expect("answer header");
                tokio::io::AsyncWriteExt::write_all(&mut stream, &[7; 32])
                    .await
                    .expect("answer cursor");
            }
        });
        let client =
            SyncClient::new(Url::parse(&format!("http://{address}/")).expect("a base url"))
                .expect("a client");
        let mut mempool = Mempool::new(Network::TESTNET);

        assert!(matches!(
            client.fill_mempool(&mut mempool, 0).await,
            Err(SyncError::InvalidMempool)
        ));
        assert_eq!(mempool.status().transactions, 0);
        server.await.expect("peer exits");
    }

    #[tokio::test]
    async fn a_peer_cannot_choose_how_many_mempool_pages_one_fill_retains() {
        let peer = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let address = peer.local_addr().expect("the bound address");
        let server = tokio::spawn(async move {
            for page in 0..super::MAX_MEMPOOL_PAGES {
                let (mut stream, _) = peer.accept().await.expect("a request");
                tokio::io::AsyncWriteExt::write_all(
                    &mut stream,
                    b"HTTP/1.1 200 OK\r\ncontent-length: 32\r\nconnection: close\r\n\r\n",
                )
                .await
                .expect("answer header");
                let cursor = [u8::try_from(page).expect("the page limit fits u8"); 32];
                tokio::io::AsyncWriteExt::write_all(&mut stream, &cursor)
                    .await
                    .expect("answer cursor");
            }
        });
        let client =
            SyncClient::new(Url::parse(&format!("http://{address}/")).expect("a base url"))
                .expect("a client");
        let mut mempool = Mempool::new(Network::TESTNET);

        assert_eq!(
            client
                .fill_mempool(&mut mempool, 0)
                .await
                .expect("the local page ceiling is normal shedding"),
            0
        );
        assert_eq!(mempool.status().transactions, 0);
        server.await.expect("peer exits");
    }

    /// The bound on what a peer may ask for, checked without waiting for it.
    ///
    /// A peer's `Retry-After` is honoured as given, which cannot be shown by
    /// waiting: a peer asking for an hour and a peer asking for two minutes are
    /// indistinguishable to an assertion that has to return. So the arithmetic is
    /// asserted directly, and it says both halves — a real header as given, an
    /// absurd one capped, so a hostile peer cannot park a catch-up for an hour.
    #[test]
    fn a_peers_retry_after_is_honoured_up_to_a_bound() {
        let asking = |value: &str| {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::RETRY_AFTER,
                reqwest::header::HeaderValue::from_str(value).expect("a header value"),
            );
            retry_after(&headers)
        };
        assert_eq!(
            asking("30"),
            Some(std::time::Duration::from_secs(30)),
            "a peer asking for half a minute is waited for as asked"
        );
        assert_eq!(
            asking(" 45 "),
            Some(std::time::Duration::from_secs(45)),
            "and the whitespace a header may carry is not a parse failure"
        );
        assert_eq!(
            asking("3600"),
            Some(RETRY_AFTER_CEILING),
            "a peer asking for an hour cannot park a catch-up for one"
        );
        // An HTTP-date `Retry-After` is legal and is not parsed, which is
        // deliberate: this node's own bounded backoff is a better answer than a
        // date it may have mis-parsed into hours.
        assert_eq!(asking("Wed, 21 Oct 2015 07:28:00 GMT"), None);
        assert_eq!(retry_after(&reqwest::header::HeaderMap::new()), None);
    }

    /// A pool with every peer throttled asks its caller to wait, not to stop.
    ///
    /// The distinction is the whole of a catch-up round's shape. A round that is
    /// told "no peer left to ask" fails and discards itself; a round that is told
    /// "everybody is throttling" keeps the blocks it staged, executes them and
    /// asks again. `NoPeer` was answered for both, and a mainnet round that met
    /// one 429 returned it before executing anything it already held.
    ///
    /// `Retry-After: 0` so the client's own retries cost this test nothing; what
    /// is under test is the answer after them, not the waiting.
    #[tokio::test]
    async fn a_pool_with_every_peer_throttled_asks_the_caller_to_wait() {
        let peer = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let address = peer.local_addr().expect("the bound address");
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = peer.accept().await.expect("a request");
                let response = "HTTP/1.1 429 Too Many Requests\r\n\
                                retry-after: 0\r\ncontent-length: 0\r\n\r\n";
                let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await;
            }
        });
        let client =
            SyncClient::new(Url::parse(&format!("http://{address}/")).expect("a base url"))
                .expect("a client");
        let mut source = TenureSource::only(client);
        let tenure = StacksBlockId::from_bytes([0x11; 32]);

        let refused = source
            .blocks_of_tenure(tenure)
            .await
            .expect_err("the peer said 429");
        assert!(refused.is_rate_limited(), "{refused}");
        assert_eq!(
            source.throttled(),
            1,
            "the peer was not set aside, so the ask below is not the one under test"
        );

        // Asked again with nobody left to ask. Nothing was even sent this time,
        // and the answer still has to be "wait".
        let exhausted = source
            .blocks_of_tenure(tenure)
            .await
            .expect_err("there is nobody left to ask");
        assert!(matches!(exhausted, SyncError::Throttled), "{exhausted}");
        assert!(exhausted.is_rate_limited(), "{exhausted}");

        // And an empty pool still says the other thing, which is what keeps the
        // two apart rather than collapsing them.
        let mut empty = TenureSource::new(Vec::new());
        assert!(matches!(
            empty.blocks_of_tenure(tenure).await,
            Err(SyncError::NoPeer)
        ));

        source.forgive_throttles();
        assert_eq!(source.throttled(), 0);
    }

    /// A peer asking for a longer wait than this node would choose gets it.
    ///
    /// Capping `Retry-After` at the self-chosen ceiling meant a peer asking for
    /// a minute was asked again two seconds later, which earns another 429 and
    /// keeps earning them. A mainnet accounting rebuild ran for 1h45m against a
    /// rate-limited peer with 6 seconds of CPU to show for it.
    #[tokio::test]
    async fn a_peer_asking_for_a_long_wait_is_waited_for() {
        let peer = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let address = peer.local_addr().expect("the bound address");
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = peer.accept().await.expect("a request");
                let response = "HTTP/1.1 429 Too Many Requests\r\n\
                                retry-after: 30\r\ncontent-length: 0\r\n\r\n";
                let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await;
            }
        });
        let client =
            SyncClient::new(Url::parse(&format!("http://{address}/")).expect("a base url"))
                .expect("a client");

        // Under the old cap the three retries took ~6s in total and returned.
        // Honouring the header they take 30s each, so the call is still running
        // when a generous bound on the old behaviour has passed.
        let outcome =
            tokio::time::timeout(std::time::Duration::from_secs(15), client.node_info()).await;
        assert!(
            outcome.is_err(),
            "the peer asked for 30s and was waited for, rather than asked again in 2"
        );
    }

    /// A cached block is served without a request, which is what makes a round
    /// that a peer rate limited cheap to retry.
    #[tokio::test]
    async fn a_fetched_block_is_shared_and_not_asked_for_twice() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../nano-conformance/fixtures/nakamoto/blocks");
        let path = fs::read_dir(directory)
            .expect("read fixture blocks")
            .map(|entry| entry.expect("fixture block").path())
            .min()
            .expect("a fixture block");
        let block = NakamotoBlock::decode(&fs::read(path).expect("read fixture block"))
            .expect("decode fixture block");

        // Nothing listens here, so a request would fail rather than answer.
        let client = SyncClient::new(Url::parse("http://127.0.0.1:1/").expect("a base url"))
            .expect("a client");
        assert!(client.block(block.block_id()).await.is_err());
        block_cache()
            .lock()
            .expect("the cache is not poisoned")
            .insert(
                (client.base_url.clone(), block.block_id()),
                block.clone(),
                block.encode().len(),
            );
        let another_role = SyncClient::new(client.base_url.clone()).expect("another role");
        assert_eq!(
            another_role
                .block(block.block_id())
                .await
                .expect("cached")
                .block_id(),
            block.block_id()
        );
    }

    #[test]
    fn peer_cache_evicts_by_bytes_and_count_and_recovers() {
        let mut bytes = ByteCache::new(3, 5);
        assert!(bytes.insert(1, "one", 3));
        assert!(bytes.insert(2, "two", 2));
        assert_eq!(bytes.stats().items, 2);
        assert_eq!(bytes.stats().bytes, 5);

        assert_eq!(bytes.get(&1), Some(&"one"));
        assert!(bytes.insert(3, "three", 3));
        assert_eq!(bytes.get(&1), None);
        assert_eq!(bytes.get(&2), None);
        assert_eq!(bytes.get(&3), Some(&"three"));
        assert_eq!(bytes.stats().bytes, 3);

        assert!(!bytes.insert(3, "oversize", 6));
        assert_eq!(bytes.get(&3), Some(&"three"));
        assert!(bytes.insert(3, "small", 1));
        assert_eq!(bytes.stats().bytes, 1);

        let mut count = ByteCache::new(2, 100);
        assert!(count.insert(1, "one", 1));
        assert!(count.insert(2, "two", 1));
        assert!(count.insert(3, "three", 1));
        assert_eq!(count.get(&1), None);
        assert_eq!(count.stats().items, 2);
        assert_eq!(count.stats().bytes, 2);
    }

    #[test]
    fn tenure_validation_requires_a_contiguous_stream() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../nano-conformance/fixtures/nakamoto/blocks");
        let mut paths = fs::read_dir(directory)
            .expect("read fixture blocks")
            .map(|entry| entry.expect("fixture block").path())
            .collect::<Vec<_>>();
        paths.sort();
        let mut blocks = paths
            .into_iter()
            .take(3)
            .map(|path| NakamotoBlock::decode(&fs::read(path).expect("read fixture block")))
            .collect::<Result<Vec<_>, _>>()
            .expect("decode fixture blocks");
        blocks.reverse();

        validate_tenure(blocks[0].block_id(), &blocks).expect("valid fixture tenure");
        assert!(matches!(
            validate_tenure(blocks[1].block_id(), &blocks),
            Err(SyncError::TenureStart)
        ));
        let mut invalid = blocks.clone();
        invalid[0].header.parent_block_id = StacksBlockId::from_bytes([0; 32]);
        assert!(matches!(
            validate_tenure(invalid[0].block_id(), &invalid),
            Err(SyncError::TenureLink(TenureError::ParentBlockId))
        ));
    }

    #[test]
    fn follower_requires_new_tenures_to_extend_the_previous_one() {
        let previous = tenure_info([1; 20], [1; 32], [2; 32]);
        let extension = tenure_info([1; 20], [1; 32], [3; 32]);
        let successor = TenureInfo {
            consensus_hash: ConsensusHash::from_bytes([4; 20]),
            tenure_start_block_id: StacksBlockId::from_bytes([4; 32]),
            parent_consensus_hash: previous.consensus_hash,
            parent_tenure_start_block_id: previous.tenure_start_block_id,
            tip_block_id: StacksBlockId::from_bytes([5; 32]),
            tip_height: 2,
            reward_cycle: 1,
        };
        let fork = tenure_info([6; 20], [6; 32], [7; 32]);

        assert!(validate_tenure_transition(&previous, &extension).is_ok());
        assert!(validate_tenure_transition(&previous, &successor).is_ok());
        assert!(matches!(
            validate_tenure_transition(&previous, &fork),
            Err(SyncError::Fork)
        ));
    }

    /// What a node's own burnchain says, stated directly.
    ///
    /// The three answers rather than the rule that produces them: which burn
    /// totals place a candidate below this node's tip is
    /// `SortitionTracker::derived`'s business and has its own oracle in
    /// `mainnet_sortition`, and a stub that recomputed it here would be testing
    /// itself.
    struct StubBurnView(std::collections::BTreeMap<[u8; 20], Option<bool>>);

    impl BurnView for StubBurnView {
        fn derived(&self, consensus_hash: ConsensusHash, _bitcoin_spent: u64) -> Option<bool> {
            self.0
                .get(consensus_hash.as_bytes())
                .copied()
                .unwrap_or(None)
        }

        /// Nothing, which is what these tests want: the tie-break is pinned in
        /// `nano-conformance` against the captured chain, and a stub answering here
        /// would order candidates by numbers this file made up.
        fn height_of(&self, _consensus_hash: ConsensusHash) -> Option<u64> {
            None
        }
    }

    /// A candidate tip a peer is offering, signed by `signer` if one is given.
    fn candidate(
        peer: usize,
        consensus_hash: [u8; 20],
        chain_length: u64,
        signer: Option<&nano_crypto::StacksPrivateKey>,
    ) -> CandidateTip {
        let mut header = nano_chainstate::NakamotoBlockHeader {
            version: 1,
            chain_length,
            bitcoin_spent: 1_000,
            consensus_hash: ConsensusHash::from_bytes(consensus_hash),
            parent_block_id: StacksBlockId::from_bytes([2; 32]),
            transaction_merkle_root: nano_primitives::Sha256Sum::default(),
            state_index_root: nano_primitives::TrieHash::from_bytes([4; 32]),
            timestamp: 5,
            miner_signature: nano_crypto::StacksPrivateKey::from_seed(b"miner").sign(&[5; 32]),
            signer_signatures: Vec::new(),
            pox_treatment: nano_primitives::BitVec::zeros(1).expect("a one-bit vector"),
            problematic_transactions: Vec::new(),
        };
        if let Some(signer) = signer {
            let digest = header.signer_signature_hash();
            header.signer_signatures = vec![signer.sign(digest.as_bytes())];
        }
        CandidateTip {
            peer,
            info: tenure_info(consensus_hash, [7; 32], *header.block_id().as_bytes()),
            header,
        }
    }

    /// The fork choice refuses a burn view this node's own burnchain contradicts.
    ///
    /// The lie no signature check can catch: a chain built over a different chain
    /// of Bitcoin blocks is perfectly signed by the signers of *that* chain, and
    /// executing it produces a perfectly consistent state for a chain nobody else
    /// is on. Only this node's own burnchain says otherwise.
    #[test]
    fn a_tip_on_a_burn_view_this_node_did_not_derive_is_refused() {
        let ours = [1; 20];
        let theirs = [2; 20];
        let burn = StubBurnView(
            [(ours, Some(true)), (theirs, Some(false))]
                .into_iter()
                .collect(),
        );
        // The foreign one is longer, so this cannot pass by the liar offering
        // something worse.
        let candidates = vec![
            candidate(0, ours, 100, None),
            candidate(1, theirs, 1_100, None),
        ];
        let chosen = choose_canonical_tip(&candidates, None, Some(&burn))
            .expect("the derived view is a candidate");
        assert_eq!(chosen.peer, 0, "the foreign burn view won the fork choice");

        // With only the foreign one left, the node follows nothing. Stalling is
        // visible and recoverable; following is a fork.
        assert!(
            choose_canonical_tip(&candidates[1..], None, Some(&burn)).is_none(),
            "a burn view this node did not derive was adopted for being the only one"
        );
    }

    /// A tip ahead of this node's burn view is followed, and weighed later.
    ///
    /// The liveness half, and the reason `derived` has three answers rather than
    /// two: a node catching up stands thousands of burn blocks below every honest
    /// peer, and one that refused what it could not yet judge would never acquire
    /// the state that lets it judge. What makes it safe is that the burn total of
    /// every block it executes is checked against this same chain as it reaches it.
    #[test]
    fn a_tip_ahead_of_this_nodes_burn_view_is_still_followed() {
        let unknown = [9; 20];
        let burn = StubBurnView(std::collections::BTreeMap::new());
        let candidates = vec![candidate(0, unknown, 8_000_000, None)];
        assert_eq!(
            choose_canonical_tip(&candidates, None, Some(&burn)).map(|tip| tip.peer),
            Some(0),
            "a node that cannot judge a tip refused to follow it and would never catch up"
        );
    }

    /// Signer weight is enforced where this node can judge the cycle, and not
    /// where it cannot.
    ///
    /// Both halves in one test because they are one rule: the set a node reads out
    /// of `.signers` is the set of *its own* burn view's cycle, so applying it to a
    /// candidate thousands of burn blocks ahead would refuse the honest peer for
    /// being in a cycle this node has not reached.
    #[test]
    fn weight_decides_a_judgeable_tip_and_not_one_beyond_this_nodes_view() {
        let key = nano_crypto::StacksPrivateKey::from_seed(b"a signer of this cycle");
        let signers = SignerSet::new(vec![Signer {
            public_key: key.public_key(),
            weight: 10,
        }])
        .expect("a set of one")
        .signing_weights()
        .expect("the set is well formed");

        let ours = [1; 20];
        let ahead = [2; 20];
        let burn = StubBurnView(std::iter::once((ours, Some(true))).collect());

        // On this node's own burn view and unsigned: refused, however long.
        let unsigned = vec![candidate(0, ours, 5_000, None)];
        assert!(
            choose_canonical_tip(&unsigned, Some(&signers), Some(&burn)).is_none(),
            "an unsigned tip on this node's own burn view was followed"
        );
        // Signed by the cycle's own signer: followed.
        let signed = vec![candidate(0, ours, 5_000, Some(&key))];
        assert_eq!(
            choose_canonical_tip(&signed, Some(&signers), Some(&burn)).map(|tip| tip.peer),
            Some(0),
        );
        // Ahead of this node's burn view and unsigned: followed anyway, because
        // this node's set is not the set that cycle uses and it has no way to say
        // which is. Execution weighs each block against its own cycle's set.
        let beyond = vec![candidate(0, ahead, 5_000, None)];
        assert_eq!(
            choose_canonical_tip(&beyond, Some(&signers), Some(&burn)).map(|tip| tip.peer),
            Some(0),
            "a node catching up refused a peer for being in a cycle it has not reached"
        );
    }

    fn tenure_info(
        consensus_hash: [u8; 20],
        tenure_start_block_id: [u8; 32],
        tip_block_id: [u8; 32],
    ) -> TenureInfo {
        TenureInfo {
            consensus_hash: ConsensusHash::from_bytes(consensus_hash),
            tenure_start_block_id: StacksBlockId::from_bytes(tenure_start_block_id),
            parent_consensus_hash: ConsensusHash::from_bytes([0; 20]),
            parent_tenure_start_block_id: StacksBlockId::from_bytes([0; 32]),
            tip_block_id: StacksBlockId::from_bytes(tip_block_id),
            tip_height: 1,
            reward_cycle: 1,
        }
    }

    /// A mempool page is the transactions back to back with the next page
    /// identifier as its last thirty-two bytes, and nothing frames either.
    #[test]
    fn mempool_pages_split_into_transactions_and_a_page_identifier() {
        let transaction = nano_codec::Transaction::sign_standard(
            nano_codec::TransactionVersion::Testnet,
            0x8000_0000,
            nano_codec::AnchorMode::Any,
            &nano_crypto::StacksPrivateKey::from_seed(b"mempool"),
            3,
            180,
            nano_codec::TransactionPayloadData::TokenTransfer {
                recipient: nano_codec::Principal::Standard(
                    nano_address::StacksAddress::single_signature(
                        nano_primitives::Hash160::from_bytes([4; 20]),
                        false,
                    ),
                ),
                amount: 1_000,
                memo: [0; 34],
            },
        )
        .expect("sign a transfer");
        let mut page = transaction.encode();
        page.extend_from_slice(transaction.encode().as_slice());
        page.extend_from_slice(&[9; 32]);

        let (transactions, next) = super::decode_mempool_page(&page).expect("decode the page");
        assert_eq!(transactions.len(), 2);
        assert_eq!(transactions[0].txid(), transaction.txid());
        assert_eq!(next, Some(nano_primitives::Sha256Sum::from_bytes([9; 32])));

        // A stream that ends on a transaction boundary is the last page, which
        // is what a peer sends when its mempool fits in one.
        let (transactions, next) =
            super::decode_mempool_page(&transaction.encode()).expect("decode the last page");
        assert_eq!(transactions.len(), 1);
        assert_eq!(next, None);
        assert!(super::decode_mempool_page(&[0; 8]).is_err());
    }

    #[tokio::test]
    #[ignore = "requires a running Hacknet node on localhost"]
    async fn hacknet_tip_block_downloads_and_validates() {
        let client =
            SyncClient::new(Url::parse("http://127.0.0.1:20443/").expect("valid Hacknet URL"))
                .expect("create sync client");
        let tenure = client.tenure_info().await.expect("fetch tenure info");
        let block = client
            .block(tenure.tip_block_id)
            .await
            .expect("fetch tip block");

        assert_eq!(block.block_id(), tenure.tip_block_id);
    }

    #[tokio::test]
    #[ignore = "requires a running Hacknet node on localhost"]
    async fn hacknet_pox_calendar_is_available() {
        let client =
            SyncClient::new(Url::parse("http://127.0.0.1:20443/").expect("valid Hacknet URL"))
                .expect("create sync client");
        let calendar = client.pox_info().await.expect("fetch stacking calendar");

        assert!(calendar.bitcoin_height >= calendar.first_bitcoin_height);
        assert!(calendar.prepare_phase_length > 0);
        assert!(calendar.reward_phase_length > 0);
        assert!(calendar.reward_slots > 0);
        assert_eq!(calendar.bitcoin_context().height, calendar.bitcoin_height);
    }

    #[tokio::test]
    #[ignore = "requires a running Hacknet node on localhost"]
    async fn hacknet_tenure_downloads_and_links() {
        let client =
            SyncClient::new(Url::parse("http://127.0.0.1:20443/").expect("valid Hacknet URL"))
                .expect("create sync client");
        let tenure = client.tenure_info().await.expect("fetch tenure info");
        let blocks = client
            .tenure(tenure.tenure_start_block_id, None)
            .await
            .expect("fetch tenure");

        assert!(!blocks.is_empty());
        assert_eq!(
            blocks.first().expect("non-empty tenure").block_id(),
            tenure.tenure_start_block_id
        );
        for pair in blocks.windows(2) {
            pair[1]
                .validate_successor(&pair[0].header)
                .expect("tenure blocks link");
        }
    }

    #[tokio::test]
    #[ignore = "requires a running Hacknet node on localhost"]
    async fn hacknet_follower_retains_an_authenticated_tenure() {
        let client =
            SyncClient::new(Url::parse("http://127.0.0.1:20443/").expect("valid Hacknet URL"))
                .expect("create sync client");
        let mut follower = TenureFollower::new(client);
        let tenure = follower
            .poll()
            .await
            .expect("follow current tenure")
            .expect("initial tenure");

        assert_eq!(follower.history(), std::slice::from_ref(&tenure));
        assert_eq!(
            tenure.sortition.consensus_hash,
            tenure
                .blocks
                .last()
                .expect("non-empty tenure")
                .header
                .consensus_hash
        );
        assert!(tenure.sortition.was_sortition);
    }

    #[tokio::test]
    #[ignore = "requires a running Hacknet node on localhost"]
    async fn hacknet_follower_spans_prepare_phase_and_reward_cycle_rollover() {
        let client =
            SyncClient::new(Url::parse("http://127.0.0.1:20443/").expect("valid Hacknet URL"))
                .expect("create sync client");
        let mut follower = TenureFollower::new(client.clone());
        let initial = follower
            .poll()
            .await
            .expect("follow current tenure")
            .expect("initial tenure");
        let mut reward_cycles = BTreeSet::from([initial.info.reward_cycle]);
        let mut saw_prepare_phase = false;
        let timeout_seconds = env::var("NANO_HACKNET_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(75);

        timeout(Duration::from_secs(timeout_seconds), async {
            loop {
                let calendar = client.pox_info().await.expect("fetch stacking calendar");
                saw_prepare_phase |= is_prepare_phase(&calendar);
                if let Some(tenure) = follower.poll().await.expect("follow tenure update") {
                    reward_cycles.insert(tenure.info.reward_cycle);
                }
                if saw_prepare_phase && reward_cycles.len() >= 2 {
                    break;
                }
                sleep(Duration::from_millis(500)).await;
            }
        })
        .await
        .expect("span a prepare phase and reward-cycle rollover");

        assert!(saw_prepare_phase);
        assert!(reward_cycles.len() >= 2);
        assert!(follower.history().len() >= 2);
    }

    #[tokio::test]
    #[ignore = "requires a running Hacknet node on localhost"]
    async fn hacknet_sortition_authenticates_the_current_tenure() {
        let client =
            SyncClient::new(Url::parse("http://127.0.0.1:20443/").expect("valid Hacknet URL"))
                .expect("create sync client");
        let tenure = client.tenure_info().await.expect("fetch tenure info");
        let block = client
            .block(tenure.tip_block_id)
            .await
            .expect("fetch tip block");
        let sortition = client
            .sortition(block.header.consensus_hash)
            .await
            .expect("fetch tenure sortition");

        assert_eq!(sortition.consensus_hash, block.header.consensus_hash);
        assert!(sortition.was_sortition);
        assert!(sortition.miner_public_key_hash.is_some());
    }

    fn is_prepare_phase(calendar: &super::PoxInfo) -> bool {
        let cycle_length = u64::from(calendar.reward_phase_length)
            .checked_add(u64::from(calendar.prepare_phase_length))
            .expect("PoX cycle length fits in u64");
        let position = calendar
            .bitcoin_height
            .checked_sub(calendar.first_bitcoin_height)
            .expect("Bitcoin height does not predate the first height")
            % cycle_length;
        position >= u64::from(calendar.reward_phase_length)
    }
}
