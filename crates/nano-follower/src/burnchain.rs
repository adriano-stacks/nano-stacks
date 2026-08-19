//! The read-only Bitcoin source used by the follower.

use std::error::Error;

use nano_bitcoin::{BitcoinRestSource, BitcoinRpcSource};
use nano_chainstate::BitcoinBlockContext;
use nano_sync::PoxInfo;

use crate::config::Config;

/// Verify a peer's `PoX` answer against the constants this network fixes.
///
/// On mainnet the `PoX` origin, phase geometry, slot count and activation
/// height are consensus facts the executable profile pins; they feed every
/// `BitcoinBlockContext` and the local cycle math, and an untrusted peer must
/// not choose them. A peer that contradicts the profile is refused — its
/// answer contributes only the burn tip it reports. Any other network keeps
/// the peer's answer, which is how a private chain describes itself.
pub fn verified_pox(config: &Config, answer: PoxInfo) -> Result<PoxInfo, String> {
    if !config
        .network()
        .is_some_and(nano_primitives::Network::is_mainnet)
    {
        return Ok(answer);
    }
    let profile = nano_consensus_profile::profile()?;
    let pox = profile.pox;
    let expected_slots = u32::from(pox.outputs_per_commit) * pox.reward_phase_length;
    let expected = [
        (
            "first burn height",
            answer.first_bitcoin_height,
            pox.first_burn_height,
        ),
        (
            "prepare phase length",
            u64::from(answer.prepare_phase_length),
            u64::from(pox.prepare_phase_length),
        ),
        (
            "reward phase length",
            u64::from(answer.reward_phase_length),
            u64::from(pox.reward_phase_length),
        ),
        (
            "reward slots",
            u64::from(answer.reward_slots),
            u64::from(expected_slots),
        ),
    ];
    for (name, answered, pinned) in expected {
        if answered != pinned {
            return Err(format!(
                "the peer's PoX {name} is {answered} where the profile pins {pinned}"
            ));
        }
    }
    if let Some(height) = answer.pox_5_activation_height
        && u64::from(height) != pox.activation_burn_height
    {
        return Err(format!(
            "the peer's PoX activation height is {height} where the profile pins {}",
            pox.activation_burn_height
        ));
    }
    let activation = u32::try_from(pox.activation_burn_height)
        .map_err(|_| "the profile's PoX activation height does not fit in u32".to_owned())?;
    Ok(PoxInfo {
        first_bitcoin_height: pox.first_burn_height,
        bitcoin_height: answer.bitcoin_height,
        prepare_phase_length: pox.prepare_phase_length,
        reward_phase_length: pox.reward_phase_length,
        reward_slots: expected_slots,
        rejection_fraction: None,
        pox_5_activation_height: Some(activation),
        v1_unlock_height: None,
        v2_unlock_height: None,
        v3_unlock_height: None,
    })
}

/// Build the execution context fixed by the local network configuration.
#[must_use]
pub fn bitcoin_context(config: &Config, pox: &PoxInfo) -> BitcoinBlockContext {
    let mut context = pox.bitcoin_context();
    if let Some(height) = config.burnchain.pox_5_activation_height {
        context.pox_5_activation_height = height;
    }
    context
}

/// Connect to the configured read-only Bitcoin source.
pub fn bitcoin_source(config: &Config) -> Result<BurnchainSource, Box<dyn Error>> {
    if let Some(rest) = config.burnchain.rest_url.as_ref() {
        return Ok(BurnchainSource::Rest(Box::new(BitcoinRestSource::new(
            rest,
            config.burnchain.magic()?,
        )?)));
    }
    Ok(BurnchainSource::Rpc(Box::new(BitcoinRpcSource::new(
        &config.burnchain.rpc_url,
        config.burnchain.rpc_user.clone(),
        config.burnchain.rpc_password.clone(),
        config.burnchain.magic()?,
    )?)))
}

/// A canonical Bitcoin reader, independent of its transport.
#[derive(Debug)]
pub enum BurnchainSource {
    Rpc(Box<BitcoinRpcSource>),
    Rest(Box<BitcoinRestSource>),
}

impl nano_bitcoin::BitcoinSource for BurnchainSource {
    type Error = nano_bitcoin::BitcoinRpcSourceError;

    fn block_at(&mut self, height: u64) -> Result<nano_bitcoin::BitcoinBlock, Self::Error> {
        match self {
            Self::Rpc(source) => source.block_at(height),
            Self::Rest(source) => source.block_at(height),
        }
    }

    fn block_hash_at(&self, height: u64) -> Result<[u8; 32], Self::Error> {
        match self {
            Self::Rpc(source) => source.block_hash_at(height),
            Self::Rest(source) => source.block_hash_at(height),
        }
    }

    fn tip_height(&self) -> Result<u64, Self::Error> {
        match self {
            Self::Rpc(source) => source.tip_height(),
            Self::Rest(source) => source.tip_height(),
        }
    }

    fn invalidate_from(&mut self, height: u64) {
        match self {
            Self::Rpc(source) => source.invalidate_from(height),
            Self::Rest(source) => source.invalidate_from(height),
        }
    }
}

#[cfg(test)]
mod tests {
    use nano_sync::PoxInfo;

    use super::{BurnchainSource, bitcoin_context, bitcoin_source};
    use crate::config::Config;

    const CONFIG: &str = r#"
        [follower]
        working_dir = "/tmp/nano-follower"
        network = "testnet"
        peers = ["http://127.0.0.1:20443/"]

        [burnchain]
        rest_url = "http://127.0.0.1:3002/"
        pox_5_activation_height = 500

        [checkpoint]
        marf = "/tmp/checkpoint/marf.sqlite"
        source_state_id = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
        state_root = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100"
        anchor_block = "/tmp/checkpoint/anchor-block.bin"
        anchor_bitcoin_height = 285
    "#;

    fn pox() -> PoxInfo {
        PoxInfo {
            first_bitcoin_height: 100,
            bitcoin_height: 600,
            prepare_phase_length: 5,
            reward_phase_length: 15,
            reward_slots: 10,
            rejection_fraction: None,
            pox_5_activation_height: Some(400),
            v1_unlock_height: None,
            v2_unlock_height: None,
            v3_unlock_height: None,
        }
    }

    const MAINNET_CONFIG: &str = r#"
        [follower]
        working_dir = "/tmp/nano-follower"
        network = "mainnet"
        peers = ["http://127.0.0.1:20443/"]

        [burnchain]
        rest_url = "http://127.0.0.1:3002/"

        [checkpoint]
        bundle = "/tmp/checkpoint/bundle"
        builder_policy = "/tmp/checkpoint/builders.toml"
        builder_signatures = "/tmp/checkpoint/signatures"
        marf = "/tmp/checkpoint/bundle/marf.sqlite"
        source_state_id = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
        state_root = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100"
        anchor_block = "/tmp/checkpoint/bundle/anchor-block.bin"
        anchor_bitcoin_height = 960231
    "#;

    /// What mainnet's stock nodes answer today, burn tip aside.
    fn mainnet_answer() -> PoxInfo {
        PoxInfo {
            first_bitcoin_height: 666_050,
            bitcoin_height: 963_160,
            prepare_phase_length: 100,
            reward_phase_length: 2_000,
            reward_slots: 4_000,
            rejection_fraction: None,
            pox_5_activation_height: None,
            v1_unlock_height: None,
            v2_unlock_height: None,
            v3_unlock_height: None,
        }
    }

    #[test]
    fn a_mainnet_peer_contributes_only_its_burn_tip() {
        let config = Config::parse(MAINNET_CONFIG).expect("mainnet config");
        let pinned =
            super::verified_pox(&config, mainnet_answer()).expect("a truthful peer is adopted");
        assert_eq!(pinned.bitcoin_height, 963_160);
        // The activation height comes from the profile even where the peer's
        // answer is silent about it.
        assert_eq!(pinned.pox_5_activation_height, Some(960_230));
        assert_eq!(pinned.first_bitcoin_height, 666_050);
    }

    #[test]
    fn a_mainnet_peer_cannot_choose_the_pox_geometry() {
        let config = Config::parse(MAINNET_CONFIG).expect("mainnet config");
        let mut lying = mainnet_answer();
        lying.prepare_phase_length = 99;
        let refused = super::verified_pox(&config, lying).expect_err("a lying peer is refused");
        assert!(refused.contains("prepare phase length"), "{refused}");

        let mut lying = mainnet_answer();
        lying.pox_5_activation_height = Some(960_231);
        let refused = super::verified_pox(&config, lying).expect_err("a lying peer is refused");
        assert!(refused.contains("activation height"), "{refused}");
    }

    #[test]
    fn a_private_network_keeps_its_peers_answer() {
        let config = Config::parse(CONFIG).expect("follower config");
        let answer = pox();
        let kept = super::verified_pox(&config, answer.clone()).expect("kept");
        assert_eq!(kept, answer);
    }

    #[test]
    fn the_configured_epoch_boundary_is_in_every_execution_context() {
        let config = Config::parse(CONFIG).expect("follower config");
        let context = bitcoin_context(&config, &pox());
        assert_eq!(context.pox_5_activation_height, 500);
    }

    #[test]
    fn the_follower_opens_only_the_configured_read_transport() {
        let config = Config::parse(CONFIG).expect("follower config");
        assert!(matches!(
            bitcoin_source(&config).expect("REST source"),
            BurnchainSource::Rest(_)
        ));
    }
}
