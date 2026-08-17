//! The read-only Bitcoin source used by the follower.

use std::error::Error;

use nano_bitcoin::{BitcoinRestSource, BitcoinRpcSource};
use nano_chainstate::BitcoinBlockContext;
use nano_sync::PoxInfo;

use crate::config::Config;

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
