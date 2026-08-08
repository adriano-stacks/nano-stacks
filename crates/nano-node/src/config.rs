//! The one file a node starts from.
//!
//! Everything an operator has to decide lives here: which chain, which state,
//! which peers, which roles. A role is switched on by its table being present,
//! so a follower, a signer and a miner are the same binary reading different
//! files.

use std::{fmt, fs, io, net::SocketAddr, path::PathBuf};

use nano_address::StacksAddress;
use nano_crypto::{StacksPrivateKey, VrfPrivateKey};
use nano_primitives::{Network, TrieHash};
use nano_stackerdb::StackerDbContract;
use reqwest::Url;
use serde::Deserialize;

/// A node's complete configuration.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub node: NodeConfig,
    pub burnchain: BurnchainConfig,
    pub checkpoint: CheckpointConfig,
    /// Sign for the reward set this key holds a slot in.
    pub signer: Option<SignerConfig>,
    /// Commit on Bitcoin and mine the tenures those commitments win.
    pub miner: Option<MinerConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// Every file this node writes lives under here.
    pub working_dir: PathBuf,
    /// `mainnet`, `testnet`, or omitted to take the chain the peers report.
    pub network: Option<NetworkName>,
    /// Overrides the chain identifier of a non-mainnet network, which Hacknet
    /// and the private testnets set to something of their own.
    pub chain_id: Option<u32>,
    /// The HTTP peers this node follows, tried in order until one answers.
    ///
    /// May be empty when `p2p_seeds` gives a way into the binary network instead:
    /// what a node needs is *a* way in, and an operator who wants no hosted API in
    /// the picture should be able to say so by leaving this out.
    #[serde(default)]
    pub peers: Vec<String>,
    /// Bootstrap peers for the binary p2p network, as `<key>@<host>:<port>` or
    /// plain `<host>:<port>`.
    ///
    /// Omitted on mainnet, stacks-core's own published bootstrap nodes are used —
    /// they are public, and a mainnet node with no way into the network does
    /// nothing. An explicit empty list turns the transport off, which is how a node
    /// says "HTTP only" out loud. The key, if given, is a label: a session learns
    /// the peer's key from its handshake and authenticates against that.
    pub p2p_seeds: Option<Vec<String>>,
    /// Where to listen for inbound peers, or nothing to stay outbound-only.
    ///
    /// A node that does not listen can still sync; what it cannot do is get into
    /// other nodes' peer tables, which is what makes it useful to the network
    /// rather than only to itself.
    pub p2p_bind: Option<SocketAddr>,
    /// The address to tell peers to dial back on, when it is not the bind address.
    ///
    /// Behind NAT the bound address is not reachable, and a peer that records the
    /// wrong one wastes its slots on it. Left out, the bind address is advertised,
    /// and an unroutable one is advertised as the any-net address — which peers read
    /// as "I do not know my own address" rather than as a lie.
    pub p2p_address: Option<SocketAddr>,
    /// Where to serve the public RPC, or nothing to serve none of it.
    pub rpc_bind: Option<SocketAddr>,
    /// Where to POST the events an observer subscribes to.
    #[serde(default)]
    pub event_observers: Vec<String>,
    /// The `authorization` header `/v3/block_proposal` demands, or nothing to
    /// serve no proposals at all.
    ///
    /// Unauthenticated, that route lets anyone make a node execute a block of
    /// their choosing, so there is no default: a node that was not given a token
    /// answers `503` rather than inventing one.
    pub block_proposal_token: Option<String>,
    /// Where this chain's sBTC registry is deployed, as `<address>.<name>`.
    ///
    /// The single output a waterfall reward cycle pays is derived from that
    /// contract's current aggregate key, so a node that cannot name it cannot
    /// serve the 4.0 reward-set shape. Mainnet's registry is fixed and this is
    /// ignored there; everywhere else it is a deployment nothing can be guessed
    /// from — the captured hacknet chain's is not at the address stacks-core
    /// defaults a testnet to.
    pub pox_5_sbtc_registry_contract: Option<String>,
    #[serde(default = "one")]
    pub poll_interval_secs: u64,
    /// Blocks this node will download to reach the peer's tip before it
    /// demands a nearer checkpoint.
    #[serde(default = "max_sync_blocks")]
    pub max_sync_blocks: usize,
    /// How long startup waits for a peer that answers before giving up.
    ///
    /// Discovery keeps running while it waits, so the set of peers to try grows
    /// between attempts -- which is the whole reason to wait rather than exit: a
    /// live mainnet start was handed seven peers and ran, and the next was handed
    /// four that all refused HTTP within the same minute. Bounded so an unroutable
    /// configuration still fails rather than hanging.
    #[serde(default = "startup_peer_wait_secs")]
    pub startup_peer_wait_secs: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkName {
    Mainnet,
    Testnet,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BurnchainConfig {
    /// A Bitcoin Core JSON-RPC endpoint, when this node has one.
    #[serde(default)]
    pub rpc_url: String,
    #[serde(default)]
    pub rpc_user: String,
    #[serde(default)]
    pub rpc_password: String,
    /// An Esplora base URL to read burn blocks from instead.
    ///
    /// A follower reads the burnchain only for its blocks, and Esplora serves
    /// the same bytes `getblock` does, so a node can follow a public chain
    /// without carrying several hundred gigabytes of it. Mining still wants
    /// the RPC: it has to send transactions.
    pub rest_url: Option<String>,
    /// Two-byte magic prefixing every Stacks `OP_RETURN` on this burnchain.
    #[serde(default = "hacknet_magic")]
    pub magic: String,
    /// Bitcoin height at which PoX-5 activates, when the peers cannot say.
    pub pox_5_activation_height: Option<u32>,
}

/// The state this node starts executing from, the first time it starts.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointConfig {
    /// The exported Clarity MARF.
    pub marf: PathBuf,
    /// The block state the export was taken at, hex-encoded.
    pub source_state_id: String,
    /// The state root that state is published under, hex-encoded.
    pub state_root: String,
    /// The consensus-encoded block immediately after the checkpoint.
    pub anchor_block: PathBuf,
    pub anchor_bitcoin_height: u64,
    /// The rewards the checkpoint still owes, which nano cannot derive.
    pub tenure_accounting: Option<PathBuf>,
    /// The consensus-encoded block that sealed the checkpoint's state.
    ///
    /// Its `state_index_root` is what a reward set signed, so it is what makes
    /// the checkpoint trustworthy — the checkpoint saying its own root is not
    /// evidence of anything.
    pub attesting_block: Option<PathBuf>,
    /// The reward set that signed that block, obtained without the checkpoint.
    pub attesting_reward_set: Option<PathBuf>,
    /// The sortition history this node derives its own snapshots from.
    ///
    /// Holds the snapshot to start at and the consensus hashes behind it,
    /// because a consensus hash mixes the ones at power-of-two offsets back.
    /// Without it a node has to ask a peer what the sortition was, which lets
    /// that peer choose its consensus hashes and its fork.
    pub sortition: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignerConfig {
    /// The Stacks key stacked into the reward set, hex-encoded.
    pub private_key: String,
    /// Seconds a signed block is protected before its replacement may be signed.
    #[serde(default = "conflict_timeout")]
    pub conflict_timeout_secs: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MinerConfig {
    /// Bitcoin Core wallet funding the commitments, which must hold its keys.
    pub bitcoin_wallet: String,
    /// The Bitcoin transaction that registered this miner's leader key.
    pub key_txid: String,
    /// The Stacks key that signs blocks and leader-key registrations.
    pub block_signing_private_key: String,
    /// The ed25519 key seeding the coinbase VRF proof.
    pub vrf_private_key: String,
    #[serde(default = "commitment_sats")]
    pub commitment_sats: u64,
    pub fee_rate_sats_per_vbyte: Option<u64>,
    /// Seconds to wait for the threshold signer response set.
    #[serde(default = "signer_timeout")]
    pub signer_timeout_secs: u64,
    /// Seconds a tenure may run before nano extends it, matching the idle
    /// timeout a signer offers an extension after.
    #[serde(default = "tenure_extend_after")]
    pub tenure_extend_after_secs: u64,
}

const fn one() -> u64 {
    1
}

/// Ten minutes: long enough for a peer set to turn over on mainnet, short enough
/// that a wrong `burnchain` or an unroutable host is reported while somebody is
/// still watching.
const fn startup_peer_wait_secs() -> u64 {
    600
}

const fn max_sync_blocks() -> usize {
    20_000
}

const fn conflict_timeout() -> u64 {
    nano_signer::DEFAULT_CONFLICT_TIMEOUT_SECS
}

const fn commitment_sats() -> u64 {
    20_000
}

const fn signer_timeout() -> u64 {
    60
}

const fn tenure_extend_after() -> u64 {
    122
}

fn hacknet_magic() -> String {
    "T3".to_owned()
}

/// Why a configuration file cannot be used to start a node.
#[derive(Debug)]
pub enum ConfigError {
    Read(io::Error),
    Parse(toml::de::Error),
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => write!(formatter, "cannot read the configuration: {error}"),
            Self::Parse(error) => write!(formatter, "cannot parse the configuration: {error}"),
            Self::Invalid(reason) => write!(formatter, "invalid configuration: {reason}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::Parse(error) => Some(error),
            Self::Invalid(_) => None,
        }
    }
}

impl Config {
    /// Read and validate a configuration file.
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(ConfigError::Read)?;
        Self::parse(&text)
    }

    /// Parse and validate a configuration document.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(ConfigError::Parse)?;
        // A node needs *a* way into the network, and now there are two of them.
        // Requiring an HTTP peer specifically is what made a hosted API
        // load-bearing in the first place.
        if config.node.peers.is_empty() && config.node.bootstrap_seeds().is_empty() {
            return Err(ConfigError::Invalid(
                "node.peers or node.p2p_seeds must name at least one way into the network"
                    .to_owned(),
            ));
        }
        config.burnchain.magic()?;
        config.checkpoint.source_state_id()?;
        config.checkpoint.state_root()?;
        config.node.peers()?;
        config.node.event_observers()?;
        if let Some(signer) = &config.signer {
            signer.private_key()?;
        }
        if let Some(miner) = &config.miner {
            miner.block_signing_private_key()?;
            miner.vrf_private_key()?;
        }
        Ok(config)
    }

    /// The chain this node executes, when the configuration fixes it.
    ///
    /// Left out, the chain is whichever one the peers report, which is what a
    /// node joining a private network usually wants.
    #[must_use]
    pub const fn network(&self) -> Option<Network> {
        match (self.node.network, self.node.chain_id) {
            (Some(NetworkName::Mainnet), _) => Some(Network::MAINNET),
            (Some(NetworkName::Testnet), Some(chain_id)) => {
                Some(Network::testnet_with_chain_id(chain_id))
            }
            (Some(NetworkName::Testnet), None) => Some(Network::TESTNET),
            (None, Some(chain_id)) => Some(Network::from_chain_id(chain_id)),
            (None, None) => None,
        }
    }

    /// The directory a role keeps its executed state in.
    ///
    /// The signer validates proposals off the canonical tip while the node
    /// executes along it, so the two hold separate stores under the one
    /// working directory rather than one store with two writers.
    #[must_use]
    pub fn chainstate_dir(&self, role: &str) -> PathBuf {
        self.node.working_dir.join(role)
    }
}

impl NodeConfig {
    /// The peers to follow, in the order they are tried.
    pub fn peers(&self) -> Result<Vec<Url>, ConfigError> {
        urls("node.peers", &self.peers)
    }

    /// The p2p bootstrap peers, defaulting to stacks-core's own on mainnet.
    ///
    /// Only when `network = "mainnet"` is written down: a configuration that leaves
    /// the chain to be discovered from its peers cannot have its network id
    /// defaulted, because on this protocol the network id *is* the chain id and it
    /// goes in the first byte of the first message.
    #[must_use]
    pub fn bootstrap_seeds(&self) -> Vec<String> {
        match &self.p2p_seeds {
            Some(seeds) => seeds.clone(),
            None if matches!(self.network, Some(NetworkName::Mainnet)) => nano_p2p::MAINNET_SEEDS
                .iter()
                .map(|seed| (*seed).to_owned())
                .collect(),
            None => Vec::new(),
        }
    }

    /// The observers every event is posted to.
    pub fn event_observers(&self) -> Result<Vec<Url>, ConfigError> {
        urls("node.event_observers", &self.event_observers)
    }
}

impl BurnchainConfig {
    /// The two-byte burnchain magic, as the parser needs it.
    pub fn magic(&self) -> Result<[u8; 2], ConfigError> {
        self.magic.as_bytes().try_into().map_err(|_| {
            ConfigError::Invalid("burnchain.magic must be exactly two bytes".to_owned())
        })
    }
}

impl CheckpointConfig {
    pub fn source_state_id(&self) -> Result<[u8; 32], ConfigError> {
        parse_hex("checkpoint.source_state_id", &self.source_state_id)
    }

    pub fn state_root(&self) -> Result<TrieHash, ConfigError> {
        Ok(TrieHash::from_bytes(parse_hex(
            "checkpoint.state_root",
            &self.state_root,
        )?))
    }
}

impl SignerConfig {
    pub fn private_key(&self) -> Result<StacksPrivateKey, ConfigError> {
        private_key("signer.private_key", &self.private_key)
    }
}

impl MinerConfig {
    pub fn block_signing_private_key(&self) -> Result<StacksPrivateKey, ConfigError> {
        private_key(
            "miner.block_signing_private_key",
            &self.block_signing_private_key,
        )
    }

    pub fn vrf_private_key(&self) -> Result<VrfPrivateKey, ConfigError> {
        Ok(VrfPrivateKey::from_bytes(parse_hex(
            "miner.vrf_private_key",
            &self.vrf_private_key,
        )?))
    }
}

/// The `StackerDB` contract miners publish their proposals on.
#[must_use]
pub fn miner_contract(network: Network) -> StackerDbContract {
    StackerDbContract {
        address: boot_address(network),
        name: "miners".to_owned(),
    }
}

/// The `StackerDB` contract a reward cycle carries one kind of message on.
///
/// A message's contract is named by reward-cycle parity and message
/// identifier, which is not its payload type byte
/// (`libsigner/src/v0/messages.rs`).
#[must_use]
pub fn cycle_contract(network: Network, cycle: u64, message: u32) -> StackerDbContract {
    StackerDbContract {
        address: boot_address(network),
        name: format!("signers-{}-{message}", cycle % 2),
    }
}

fn boot_address(network: Network) -> StacksAddress {
    network
        .boot_address()
        .parse()
        .expect("the boot address is a valid Stacks address")
}

fn urls(field: &str, values: &[String]) -> Result<Vec<Url>, ConfigError> {
    values
        .iter()
        .map(|value| {
            Url::parse(value).map_err(|error| {
                ConfigError::Invalid(format!("{field}: {value} is not a URL: {error}"))
            })
        })
        .collect()
}

fn private_key(field: &str, value: &str) -> Result<StacksPrivateKey, ConfigError> {
    StacksPrivateKey::from_bytes(parse_hex(field, value)?)
        .map_err(|error| ConfigError::Invalid(format!("{field}: {error}")))
}

/// Decode a fixed-size hex value, tolerating the trailing compression byte a
/// Stacks private key is usually quoted with.
fn parse_hex<const N: usize>(field: &str, value: &str) -> Result<[u8; N], ConfigError> {
    let value = value.trim().trim_start_matches("0x");
    let value = match value.len() {
        length if length == 2 * N + 2 && value.ends_with("01") => &value[..2 * N],
        _ => value,
    };
    let bytes = hex::decode(value)
        .map_err(|error| ConfigError::Invalid(format!("{field} is not hexadecimal: {error}")))?;
    let length = bytes.len();
    bytes
        .try_into()
        .map_err(|_| ConfigError::Invalid(format!("{field} must be {N} bytes, found {length}")))
}

#[cfg(test)]
mod tests {
    use super::{Config, NetworkName};

    const MINIMAL: &str = r#"
        [node]
        working_dir = "/tmp/nano"
        network = "testnet"
        chain_id = 2147483648
        peers = ["http://127.0.0.1:20443/"]

        [burnchain]
        rpc_url = "http://127.0.0.1:18443"
        rpc_user = "hacknet"
        rpc_password = "hacknet"

        [checkpoint]
        marf = "/tmp/checkpoint/marf.sqlite"
        source_state_id = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
        state_root = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100"
        anchor_block = "/tmp/checkpoint/anchor-block.bin"
        anchor_bitcoin_height = 285
    "#;

    /// A mainnet node needs no configured HTTP peer at all.
    ///
    /// This is the whole point of task 054: requiring `node.peers` is what made a
    /// hosted API load-bearing. Mainnet's own published bootstrap nodes are the
    /// default way in, and they are p2p seeds rather than an RPC service.
    #[test]
    fn a_mainnet_node_needs_no_configured_http_peer() {
        let mainnet = MINIMAL
            .replace("network = \"testnet\"", "network = \"mainnet\"")
            .replace("chain_id = 2147483648\n", "")
            .replace("peers = [\"http://127.0.0.1:20443/\"]", "peers = []");
        let config = Config::parse(&mainnet).expect("valid configuration");
        assert!(config.node.peers.is_empty());
        assert_eq!(config.node.bootstrap_seeds().len(), 4);
        assert!(
            config.node.bootstrap_seeds()[0].contains("seed.mainnet.hiro.so"),
            "the default seeds are stacks-core's own"
        );
    }

    /// An explicit empty seed list is how a node says "HTTP only" out loud.
    #[test]
    fn an_empty_seed_list_turns_the_transport_off() {
        let http_only = MINIMAL
            .replace("network = \"testnet\"", "network = \"mainnet\"")
            .replace("chain_id = 2147483648", "p2p_seeds = []");
        let config = Config::parse(&http_only).expect("valid configuration");
        assert!(config.node.bootstrap_seeds().is_empty());
        assert_eq!(config.node.peers.len(), 1);
    }

    /// A configuration with no way into the network at all is refused.
    #[test]
    fn a_node_with_no_way_in_is_refused() {
        // Testnet, so the mainnet seed default does not apply and there is nothing
        // left to reach the network through.
        let nowhere = MINIMAL.replace("peers = [\"http://127.0.0.1:20443/\"]", "peers = []");
        let error = Config::parse(&nowhere).expect_err("a node with no peers is refused");
        assert!(
            error.to_string().contains("p2p_seeds"),
            "the error should name both ways in: {error}"
        );
    }

    /// A non-mainnet chain gets no default seeds, because nano does not know them.
    #[test]
    fn a_private_chain_gets_no_default_seeds() {
        let config = Config::parse(MINIMAL).expect("valid configuration");
        assert!(config.node.bootstrap_seeds().is_empty());
    }

    #[test]
    fn a_node_needs_only_a_chain_a_burnchain_and_a_checkpoint() {
        let config = Config::parse(MINIMAL).expect("valid configuration");

        assert_eq!(config.node.network, Some(NetworkName::Testnet));
        assert_eq!(
            config.network().expect("fixed network").chain_id(),
            0x8000_0000
        );
        assert_eq!(config.burnchain.magic().expect("magic"), *b"T3");
        assert_eq!(config.node.poll_interval_secs, 1);
        assert!(config.signer.is_none() && config.miner.is_none());
        assert_eq!(
            config.chainstate_dir("chainstate"),
            std::path::Path::new("/tmp/nano/chainstate")
        );
    }

    /// A role is on because its table is there, and a key quoted the way
    /// stacks-core quotes one still parses.
    #[test]
    fn roles_come_from_the_tables_that_are_present() {
        let text = format!(
            "{MINIMAL}
            [signer]
            private_key = \"1111111111111111111111111111111111111111111111111111111111111111\"

            [miner]
            bitcoin_wallet = \"nano-miner\"
            key_txid = \"0000000000000000000000000000000000000000000000000000000000000000\"
            block_signing_private_key = \"222222222222222222222222222222222222222222222222222222222222222201\"
            vrf_private_key = \"3333333333333333333333333333333333333333333333333333333333333333\"
            "
        );
        let config = Config::parse(&text).expect("valid configuration");

        let signer = config.signer.as_ref().expect("signer role");
        let miner = config.miner.as_ref().expect("miner role");
        signer.private_key().expect("signer key");
        miner.block_signing_private_key().expect("miner key");
        miner.vrf_private_key().expect("VRF key");
        assert_eq!(miner.commitment_sats, 20_000);
    }

    #[test]
    fn a_node_without_a_peer_or_with_a_bad_key_is_refused() {
        assert!(Config::parse(&MINIMAL.replace(r#"["http://127.0.0.1:20443/"]"#, "[]")).is_err());
        assert!(Config::parse(&format!("{MINIMAL}\n[signer]\nprivate_key = \"11\"\n")).is_err());
        assert!(
            Config::parse(&MINIMAL.replace(r#"rpc_user = "hacknet""#, r#"rpc_users = "x""#))
                .is_err()
        );
    }
}
