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
    /// The peers this node follows, tried in order until one answers.
    pub peers: Vec<String>,
    /// Where to serve the public RPC, or nothing to serve none of it.
    pub rpc_bind: Option<SocketAddr>,
    /// Where to POST the events an observer subscribes to.
    #[serde(default)]
    pub event_observers: Vec<String>,
    #[serde(default = "one")]
    pub poll_interval_secs: u64,
    /// Blocks this node will download to reach the peer's tip before it
    /// demands a nearer checkpoint.
    #[serde(default = "max_sync_blocks")]
    pub max_sync_blocks: usize,
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
    pub rpc_url: String,
    pub rpc_user: String,
    pub rpc_password: String,
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
        if config.node.peers.is_empty() {
            return Err(ConfigError::Invalid(
                "node.peers must name at least one peer to follow".to_owned(),
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
