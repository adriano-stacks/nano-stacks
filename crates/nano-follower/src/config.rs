//! Configuration accepted by the standalone follower artifact.

use std::{
    fmt, fs, io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
};

use nano_primitives::{Network, TrieHash};
use reqwest::Url;
use schemars::JsonSchema;
use serde::Deserialize;

/// Everything the follower is allowed to configure.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub follower: FollowerConfig,
    pub burnchain: BurnchainConfig,
    pub checkpoint: CheckpointConfig,
}

/// State, outbound acquisition and loopback observation.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FollowerConfig {
    /// Every mutable file the follower owns lives below this directory.
    pub working_dir: PathBuf,
    /// `mainnet`, `testnet`, or omitted to take the peer's network identifier.
    pub network: Option<NetworkName>,
    /// A private/test network's chain identifier.
    pub chain_id: Option<u32>,
    /// Read-only HTTP endpoints used alongside peers found over P2P.
    #[serde(default)]
    pub peers: Vec<String>,
    /// Outbound P2P bootstrap peers. Mainnet's published peers are the default.
    pub p2p_seeds: Option<Vec<String>>,
    /// Loopback health endpoint.
    #[serde(default = "health_bind")]
    pub health_bind: SocketAddr,
    /// Loopback Prometheus endpoint.
    #[serde(default = "metrics_bind")]
    pub metrics_bind: SocketAddr,
    #[serde(default = "one")]
    pub poll_interval_secs: u64,
    #[serde(default = "max_sync_blocks")]
    pub max_sync_blocks: usize,
    #[serde(default = "startup_peer_wait_secs")]
    pub startup_peer_wait_secs: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkName {
    Mainnet,
    Testnet,
}

/// One read-only source of canonical Bitcoin blocks.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BurnchainConfig {
    #[serde(default)]
    pub rpc_url: String,
    #[serde(default)]
    pub rpc_user: String,
    #[serde(default)]
    pub rpc_password: String,
    pub rest_url: Option<String>,
    #[serde(default = "stacks_magic")]
    pub magic: String,
    pub pox_5_activation_height: Option<u32>,
    #[serde(default = "stable_confirmations")]
    pub stable_confirmations: u64,
}

/// Immutable state and authentication material used on the first start.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckpointConfig {
    pub bundle: Option<PathBuf>,
    pub builder_policy: Option<PathBuf>,
    pub builder_signatures: Option<PathBuf>,
    pub marf: PathBuf,
    pub source_state_id: String,
    pub state_root: String,
    pub anchor_block: PathBuf,
    pub anchor_bitcoin_height: u64,
    pub tenure_accounting: Option<PathBuf>,
    pub attesting_block: Option<PathBuf>,
    pub attesting_reward_set: Option<PathBuf>,
    pub sortition: Option<PathBuf>,
    pub authentication_history: Option<PathBuf>,
}

const fn one() -> u64 {
    1
}

const fn max_sync_blocks() -> usize {
    20_000
}

/// How long startup waits for a peer that answers both `/v2/info` and `/v2/pox`.
///
/// Ten minutes was enough while the answer came from a configured endpoint. It is
/// not enough for a node discovering its peers over P2P: mainnet handed one start
/// two endpoints, both of which never answered, and the process gave up while
/// ninety-five other addresses sat in its peer database waiting to be tried. A
/// discovering node has nothing to lose by waiting — it cannot execute a block
/// without a peer either way — so it waits an hour before calling the network
/// absent.
const fn startup_peer_wait_secs() -> u64 {
    3_600
}

const fn stable_confirmations() -> u64 {
    nano_p2p::STABLE_CONFIRMATIONS
}

const fn health_bind() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9152)
}

const fn metrics_bind() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9153)
}

fn stacks_magic() -> String {
    "X2".to_owned()
}

/// Why a follower configuration is unusable.
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
    /// Read and validate a follower configuration.
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(ConfigError::Read)?;
        Self::parse(&text)
    }

    /// Parse and validate a follower configuration.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(ConfigError::Parse)?;
        if config.follower.peers.is_empty() && config.follower.bootstrap_seeds().is_empty() {
            return Err(ConfigError::Invalid(
                "follower.peers or follower.p2p_seeds must name an outbound way into the network"
                    .to_owned(),
            ));
        }
        config.follower.peers()?;
        config.follower.validate_loopback()?;
        config.burnchain.validate()?;
        config.checkpoint.source_state_id()?;
        config.checkpoint.state_root()?;
        let signed_bundle = config.checkpoint.signed_bundle()?;
        if config.network().is_some_and(Network::is_mainnet) && signed_bundle.is_none() {
            return Err(ConfigError::Invalid(
                "a mainnet follower needs bundle, builder_policy and builder_signatures".to_owned(),
            ));
        }
        Ok(config)
    }

    /// The configured network, or none when peers decide a private chain.
    #[must_use]
    pub const fn network(&self) -> Option<Network> {
        match (self.follower.network, self.follower.chain_id) {
            (Some(NetworkName::Mainnet), _) => Some(Network::MAINNET),
            (Some(NetworkName::Testnet), Some(chain_id)) => {
                Some(Network::testnet_with_chain_id(chain_id))
            }
            (Some(NetworkName::Testnet), None) => Some(Network::TESTNET),
            (None, Some(chain_id)) => Some(Network::from_chain_id(chain_id)),
            (None, None) => None,
        }
    }

    #[must_use]
    pub fn chainstate_dir(&self) -> PathBuf {
        self.follower.working_dir.join("chainstate")
    }
}

impl FollowerConfig {
    /// Parsed read-only endpoints, in operator order.
    pub fn peers(&self) -> Result<Vec<Url>, ConfigError> {
        self.peers
            .iter()
            .map(|value| {
                Url::parse(value).map_err(|error| {
                    ConfigError::Invalid(format!("follower.peers: {value} is not a URL: {error}"))
                })
            })
            .collect()
    }

    /// Outbound bootstrap peers, defaulting only for explicit mainnet.
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

    fn validate_loopback(&self) -> Result<(), ConfigError> {
        for (name, address) in [
            ("follower.health_bind", self.health_bind),
            ("follower.metrics_bind", self.metrics_bind),
        ] {
            if !address.ip().is_loopback() {
                return Err(ConfigError::Invalid(format!(
                    "{name} must be a loopback address"
                )));
            }
        }
        Ok(())
    }
}

impl BurnchainConfig {
    pub fn magic(&self) -> Result<[u8; 2], ConfigError> {
        self.magic.as_bytes().try_into().map_err(|_| {
            ConfigError::Invalid("burnchain.magic must be exactly two bytes".to_owned())
        })
    }

    fn validate(&self) -> Result<(), ConfigError> {
        self.magic()?;
        if self.stable_confirmations == 0 {
            return Err(ConfigError::Invalid(
                "burnchain.stable_confirmations must be greater than zero".to_owned(),
            ));
        }
        let endpoint = self.rest_url.as_deref().unwrap_or(&self.rpc_url);
        if endpoint.is_empty() {
            return Err(ConfigError::Invalid(
                "burnchain.rest_url or burnchain.rpc_url is required".to_owned(),
            ));
        }
        Url::parse(endpoint).map_err(|error| {
            ConfigError::Invalid(format!("the burnchain endpoint is not a URL: {error}"))
        })?;
        Ok(())
    }
}

impl CheckpointConfig {
    pub(crate) fn signed_bundle(
        &self,
    ) -> Result<Option<(&PathBuf, &PathBuf, &PathBuf)>, ConfigError> {
        match (&self.bundle, &self.builder_policy, &self.builder_signatures) {
            (Some(bundle), Some(policy), Some(signatures)) => {
                Ok(Some((bundle, policy, signatures)))
            }
            (None, None, None) => Ok(None),
            _ => Err(ConfigError::Invalid(
                "checkpoint.bundle, builder_policy and builder_signatures must be set together"
                    .to_owned(),
            )),
        }
    }

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

fn parse_hex<const N: usize>(field: &str, value: &str) -> Result<[u8; N], ConfigError> {
    let value = value.trim().trim_start_matches("0x");
    let bytes = hex::decode(value)
        .map_err(|error| ConfigError::Invalid(format!("{field} is not hexadecimal: {error}")))?;
    let length = bytes.len();
    bytes
        .try_into()
        .map_err(|_| ConfigError::Invalid(format!("{field} must be {N} bytes, found {length}")))
}

#[cfg(test)]
mod tests {
    use super::{Config, ConfigError, NetworkName};

    const MINIMAL: &str = r#"
        [follower]
        working_dir = "/tmp/nano-follower"
        network = "testnet"
        chain_id = 2147483648
        peers = ["http://127.0.0.1:20443/"]

        [burnchain]
        rpc_url = "http://127.0.0.1:18443"
        rpc_user = "hacknet"
        rpc_password = "hacknet"

        [checkpoint]
        bundle = "/tmp/checkpoint"
        builder_policy = "/tmp/checkpoint-builders.toml"
        builder_signatures = "/tmp/checkpoint-signatures"
        marf = "/tmp/checkpoint/marf.sqlite"
        source_state_id = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
        state_root = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100"
        anchor_block = "/tmp/checkpoint/anchor-block.bin"
        anchor_bitcoin_height = 285
    "#;

    #[test]
    fn a_follower_configuration_contains_only_the_follower_surface() {
        let config = Config::parse(MINIMAL).expect("minimal follower configuration");
        assert_eq!(config.follower.network, Some(NetworkName::Testnet));
        assert_eq!(config.follower.health_bind.to_string(), "127.0.0.1:9152");
        assert_eq!(config.follower.metrics_bind.to_string(), "127.0.0.1:9153");
        assert_eq!(
            config.chainstate_dir(),
            std::path::Path::new("/tmp/nano-follower/chainstate")
        );
    }

    #[test]
    fn mainnet_defaults_to_outbound_p2p_and_signed_checkpoint_evidence() {
        let mainnet = MINIMAL
            .replace("network = \"testnet\"", "network = \"mainnet\"")
            .replace("chain_id = 2147483648\n", "")
            .replace("peers = [\"http://127.0.0.1:20443/\"]", "peers = []");
        let config = Config::parse(&mainnet).expect("mainnet follower");
        assert!(config.follower.peers.is_empty());
        assert_eq!(config.follower.bootstrap_seeds().len(), 4);

        let unsigned = mainnet
            .replace("bundle = \"/tmp/checkpoint\"\n", "")
            .replace("builder_policy = \"/tmp/checkpoint-builders.toml\"\n", "")
            .replace("builder_signatures = \"/tmp/checkpoint-signatures\"\n", "");
        assert!(matches!(
            Config::parse(&unsigned),
            Err(ConfigError::Invalid(reason)) if reason.contains("mainnet follower needs bundle")
        ));
    }

    #[test]
    fn public_observation_and_role_configuration_are_unrepresentable() {
        for forbidden in [
            "\n[signer]\nprivate_key = \"11\"\n",
            "\n[miner]\nbitcoin_wallet = \"wallet\"\n",
            "\n[node]\nrpc_bind = \"127.0.0.1:20492\"\n",
            "p2p_bind = \"127.0.0.1:20444\"\n",
            "rpc_bind = \"127.0.0.1:20492\"\n",
            "event_observer = \"http://127.0.0.1:3700\"\n",
            "block_proposal_token = \"secret\"\n",
        ] {
            let text = if forbidden.starts_with('\n') {
                format!("{MINIMAL}{forbidden}")
            } else {
                MINIMAL.replace("peers =", &format!("{forbidden}        peers ="))
            };
            assert!(
                matches!(Config::parse(&text), Err(ConfigError::Parse(_))),
                "forbidden configuration parsed: {forbidden}"
            );
        }
    }

    #[test]
    fn health_and_metrics_cannot_be_published() {
        for field in ["health_bind", "metrics_bind"] {
            let text = MINIMAL.replace(
                "peers =",
                &format!("{field} = \"0.0.0.0:9153\"\n        peers ="),
            );
            assert!(matches!(
                Config::parse(&text),
                Err(ConfigError::Invalid(reason)) if reason.contains("loopback")
            ));
        }
    }

    #[test]
    fn a_follower_needs_one_outbound_source() {
        let unreachable = MINIMAL.replace("peers = [\"http://127.0.0.1:20443/\"]", "peers = []");
        assert!(matches!(
            Config::parse(&unreachable),
            Err(ConfigError::Invalid(reason)) if reason.contains("outbound way")
        ));
    }
}
