use std::collections::HashMap;

use clarity::vm::ClarityName;
use lazy_static::lazy_static;

use super::{Caf, WordCost};
use crate::cost::clar4;
use crate::words::bitcoin::{GetBitcoinTxOutput, VerifyMerkleProof};
use crate::words::secp256k1::{Decompress, Ed25519Verify};
use crate::words::Word;

lazy_static! {
    pub(super) static ref WORD_COSTS: HashMap<ClarityName, WordCost> = {
        use Caf::*;

        let mut map = clar4::WORD_COSTS.clone();

        map.insert(
            VerifyMerkleProof.name(),
            WordCost {
                runtime: Linear { a: 125, b: 502 },
                read_count: None,
                read_length: None,
                write_count: None,
                write_length: None,
            },
        );
        map.insert(
            GetBitcoinTxOutput.name(),
            WordCost {
                runtime: LinearShift {
                    a: 125,
                    b: 291,
                    shift: 10,
                },
                read_count: None,
                read_length: None,
                write_count: None,
                write_length: None,
            },
        );
        map.insert(
            Ed25519Verify.name(),
            WordCost {
                runtime: LinearShift {
                    a: 125,
                    b: 7_880,
                    shift: 10,
                },
                read_count: None,
                read_length: None,
                write_count: None,
                write_length: None,
            },
        );
        map.insert(
            Decompress.name(),
            WordCost {
                runtime: Constant(1_035),
                read_count: None,
                read_length: None,
                write_count: None,
                write_length: None,
            },
        );

        map
    };
}
