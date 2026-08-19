//! The wire form of one decision request.
//!
//! Everything the deterministic decision needs, serialized: the candidate
//! block's consensus bytes, the Bitcoin execution context, the burn block's
//! decoded operations and the exact parent the caller stands on. The envelope
//! is versioned by its schema string, and the context round-trips through the
//! same invariant-preserving setters the in-process path uses — a wire form
//! that could desynchronize the tenure height from the view would be a second
//! implementation of the one rule that reads it.

use nano_bitcoin::BitcoinOperation;
use nano_chainstate::{BitcoinBlockContext, NakamotoBlock};
use serde::{Deserialize, Serialize};

pub const REQUEST_SCHEMA: &str = "nano-stacks/epoch4-decision-request/v1";

/// One candidate block and the authenticated context to judge it under.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DecisionRequest {
    pub schema: String,
    /// The candidate's consensus encoding, hexadecimal.
    pub block: String,
    pub context: ContextWire,
    pub operations: Vec<BitcoinOperation>,
    /// The exact parent the caller stands on; refused when it is not the tip.
    pub parent: Option<String>,
}

/// [`BitcoinBlockContext`], flattened for the wire.
///
/// The tenure's own burn height travels beside the view explicitly, and
/// decoding reconstructs the pair through `move_to_burn_block` +
/// `extend_view_to`, which is the only way the in-process path may move them.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextWire {
    pub height: u64,
    pub tenure_burn_height: u64,
    pub first_height: u64,
    pub prepare_phase_length: u32,
    pub reward_phase_length: u32,
    pub rejection_fraction: u64,
    pub v1_unlock_height: u32,
    pub v2_unlock_height: u32,
    pub v3_unlock_height: u32,
    pub pox_5_activation_height: u32,
    pub accumulated_coinbase: u128,
    pub burn_header_hash: [u8; 32],
    pub burn_block_time: u64,
    pub vrf_seed: [u8; 32],
    pub burn_spend_total: u128,
    pub burn_spend_winner: u128,
    pub sortition_hash: [u8; 32],
    pub winner_vrf_public_key: Option<[u8; 32]>,
    pub winner_signing_key_hash: Option<[u8; 20]>,
}

impl From<BitcoinBlockContext> for ContextWire {
    fn from(context: BitcoinBlockContext) -> Self {
        Self {
            height: context.height,
            tenure_burn_height: context.tenure_burn_height(),
            first_height: context.first_height,
            prepare_phase_length: context.prepare_phase_length,
            reward_phase_length: context.reward_phase_length,
            rejection_fraction: context.rejection_fraction,
            v1_unlock_height: context.v1_unlock_height,
            v2_unlock_height: context.v2_unlock_height,
            v3_unlock_height: context.v3_unlock_height,
            pox_5_activation_height: context.pox_5_activation_height,
            accumulated_coinbase: context.accumulated_coinbase,
            burn_header_hash: context.burn_header_hash,
            burn_block_time: context.burn_block_time,
            vrf_seed: context.vrf_seed,
            burn_spend_total: context.burn_spend_total,
            burn_spend_winner: context.burn_spend_winner,
            sortition_hash: context.sortition_hash,
            winner_vrf_public_key: context.winner_vrf_public_key,
            winner_signing_key_hash: context.winner_signing_key_hash,
        }
    }
}

impl TryFrom<ContextWire> for BitcoinBlockContext {
    type Error = String;

    fn try_from(wire: ContextWire) -> Result<Self, String> {
        if wire.tenure_burn_height > wire.height {
            return Err(format!(
                "the tenure's burn height {} is above its own view {}",
                wire.tenure_burn_height, wire.height
            ));
        }
        let mut context = Self::at_height(wire.tenure_burn_height);
        context.extend_view_to(wire.height);
        context.first_height = wire.first_height;
        context.prepare_phase_length = wire.prepare_phase_length;
        context.reward_phase_length = wire.reward_phase_length;
        context.rejection_fraction = wire.rejection_fraction;
        context.v1_unlock_height = wire.v1_unlock_height;
        context.v2_unlock_height = wire.v2_unlock_height;
        context.v3_unlock_height = wire.v3_unlock_height;
        context.pox_5_activation_height = wire.pox_5_activation_height;
        context.accumulated_coinbase = wire.accumulated_coinbase;
        context.burn_header_hash = wire.burn_header_hash;
        context.burn_block_time = wire.burn_block_time;
        context.vrf_seed = wire.vrf_seed;
        context.burn_spend_total = wire.burn_spend_total;
        context.burn_spend_winner = wire.burn_spend_winner;
        context.sortition_hash = wire.sortition_hash;
        context.winner_vrf_public_key = wire.winner_vrf_public_key;
        context.winner_signing_key_hash = wire.winner_signing_key_hash;
        Ok(context)
    }
}

/// A decoded request: the candidate and the exact context it carries.
pub struct OpenedRequest {
    pub block: NakamotoBlock,
    pub context: BitcoinBlockContext,
    pub operations: Vec<BitcoinOperation>,
    pub parent: Option<[u8; 32]>,
}

impl DecisionRequest {
    /// Wrap one candidate with its context, ready for the wire.
    #[must_use]
    pub fn new(
        block: &NakamotoBlock,
        context: BitcoinBlockContext,
        operations: Vec<BitcoinOperation>,
        parent: Option<[u8; 32]>,
    ) -> Self {
        Self {
            schema: REQUEST_SCHEMA.to_owned(),
            block: hex::encode(block.encode()),
            context: ContextWire::from(context),
            operations,
            parent: parent.map(hex::encode),
        }
    }

    /// Decode the candidate and the exact context the request carries.
    pub fn open(&self) -> Result<OpenedRequest, String> {
        if self.schema != REQUEST_SCHEMA {
            return Err(format!("unknown request schema {}", self.schema));
        }
        let bytes = hex::decode(&self.block).map_err(|error| error.to_string())?;
        let block = NakamotoBlock::decode(&bytes)
            .map_err(|error| format!("the candidate does not decode: {error:?}"))?;
        let context = BitcoinBlockContext::try_from(self.context)?;
        let parent = self
            .parent
            .as_ref()
            .map(|parent| {
                let bytes = hex::decode(parent).map_err(|error| error.to_string())?;
                <[u8; 32]>::try_from(bytes.as_slice())
                    .map_err(|_| "the parent is not 32 bytes".to_owned())
            })
            .transpose()?;
        Ok(OpenedRequest {
            block,
            context,
            operations: self.operations.clone(),
            parent,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{BitcoinBlockContext, ContextWire};

    fn context() -> BitcoinBlockContext {
        let mut context = BitcoinBlockContext::at_height(962_100);
        context.extend_view_to(962_149);
        context.first_height = 666_050;
        context.prepare_phase_length = 100;
        context.reward_phase_length = 2_000;
        context.pox_5_activation_height = 960_230;
        context.accumulated_coinbase = 1_000_000_000;
        context.burn_header_hash = [7; 32];
        context.burn_block_time = 1_787_000_000;
        context.vrf_seed = [9; 32];
        context.burn_spend_total = 437_990_659_148;
        context.burn_spend_winner = 88_000_000;
        context.sortition_hash = [3; 32];
        context.winner_vrf_public_key = Some([5; 32]);
        context.winner_signing_key_hash = Some([6; 20]);
        context
    }

    #[test]
    fn the_context_round_trips_with_its_tenure_height_intact() {
        let original = context();
        let wire = ContextWire::from(original);
        let decoded = BitcoinBlockContext::try_from(wire).expect("decode");
        assert_eq!(decoded, original);
        assert_eq!(decoded.tenure_burn_height(), 962_100);
        assert_eq!(decoded.height, 962_149);
    }

    #[test]
    fn a_tenure_above_its_own_view_is_refused() {
        let mut wire = ContextWire::from(context());
        wire.tenure_burn_height = wire.height + 1;
        let refused = BitcoinBlockContext::try_from(wire).expect_err("refuse");
        assert!(refused.contains("above its own view"), "{refused}");
    }

    #[test]
    fn the_wire_json_round_trips() {
        let wire = ContextWire::from(context());
        let text = serde_json::to_string(&wire).expect("serialize");
        let back: ContextWire = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, wire);
    }
}
