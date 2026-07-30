//! The `costs-5` schedule, which epoch 4.0 charges.
//!
//! `costs-5` re-tunes most of the words `costs-4` inherited from `costs-3` on
//! top of pricing the words Clarity 6 adds, so this starts from the `costs-4`
//! table and replaces every function stacks-core's `Costs5` overrides. A
//! function stated here in full also states the dimensions it charges nothing
//! for: `ExecutionCost::runtime` zeroes the other four.

use std::collections::HashMap;

use clarity::vm::ClarityName;
use lazy_static::lazy_static;
use Caf::*;

use super::{Caf, WordCost};
use crate::cost::clar4;
use crate::words::arithmetic::{Add, Div, Log2, Modulo, Mul, Power, Sqrti, Sub};
use crate::words::bindings::Let;
use crate::words::bitcoin::{GetBitcoinTxOutput, VerifyMerkleProof};
use crate::words::bitwise::{
    BitwiseAnd, BitwiseLShift, BitwiseNot, BitwiseOr, BitwiseRShift, BitwiseXor,
};
use crate::words::buff_to_integer::{BuffToIntBe, BuffToIntLe, BuffToUintBe, BuffToUintLe};
use crate::words::comparison::{CmpGeq, CmpGreater, CmpLeq, CmpLess};
use crate::words::conditionals::{And, Filter, Match, Or, Try, Unwrap, UnwrapErr};
use crate::words::consensus_buff::{FromConsensusBuff, ToConsensusBuff};
use crate::words::control_flow::{Begin, UnwrapErrPanic, UnwrapPanic};
use crate::words::conversion::{IntToAscii, IntToUtf8, StringToInt, StringToUint};
use crate::words::data_vars::{GetDataVar, SetDataVar};
use crate::words::default_to::DefaultTo;
use crate::words::enums::{ClarityErr, ClarityOk, ClaritySome};
use crate::words::equal::IsEq;
use crate::words::hashing::{Hash160, Keccak256, Sha256, Sha512, Sha512_256};
use crate::words::index_of::IndexOf;
use crate::words::logical::Not;
use crate::words::maps::{MapDelete, MapGet, MapInsert, MapSet};
use crate::words::noop::{ToInt, ToUint};
use crate::words::options::{IsNone, IsSome};
use crate::words::principal::{Construct, Destruct, IsStandard};
use crate::words::print::Print;
use crate::words::responses::{IsErr, IsOk};
use crate::words::secp256k1::{Decompress, Ed25519Verify, Recover, Verify};
use crate::words::secp256r1::Verify as Secp256r1Verify;
use crate::words::sequences::{
    Append, AsMaxLen, Concat, ElementAt, Fold, Len, ListCons, Map, ReplaceAt, Slice,
};
use crate::words::to_ascii::ToAscii;
use crate::words::tuples::{TupleCons, TupleGet, TupleMerge};
use crate::words::Word;

/// A word whose only charge is runtime.
const fn runtime(runtime: Caf) -> WordCost {
    WordCost {
        runtime,
        read_count: None,
        read_length: None,
        write_count: None,
        write_length: None,
    }
}

lazy_static! {
    pub(super) static ref WORD_COSTS: HashMap<ClarityName, WordCost> = {
        let mut map = clar4::WORD_COSTS.clone();

        // `linear(n >> shift, a, b)` in stacks-core.
        let shifted = |a, b, shift| runtime(LinearShift { a, b, shift });

        for (word, cost) in [
            // Arithmetic, comparison and logic.
            (Add.name(), shifted(1, 31, 4)),
            (Sub.name(), shifted(1, 32, 5)),
            (Mul.name(), shifted(1, 31, 4)),
            (Div.name(), shifted(1, 32, 5)),
            (Modulo.name(), runtime(Constant(31))),
            (Power.name(), runtime(Constant(31))),
            (Sqrti.name(), runtime(Constant(31))),
            (Log2.name(), runtime(Constant(31))),
            (ToInt.name(), runtime(Constant(31))),
            (ToUint.name(), runtime(Constant(31))),
            (CmpGeq.name(), shifted(1, 38, 8)),
            (CmpLeq.name(), shifted(1, 38, 8)),
            (CmpLess.name(), shifted(1, 38, 8)),
            (CmpGreater.name(), shifted(1, 38, 8)),
            (IsEq.name(), shifted(1, 38, 9)),
            (Not.name(), runtime(Constant(31))),
            (Or.name(), shifted(1, 31, 4)),
            (And.name(), shifted(1, 31, 4)),
            (BitwiseXor.name(), shifted(1, 31, 4)),
            (BitwiseAnd.name(), shifted(1, 31, 4)),
            (BitwiseOr.name(), shifted(1, 31, 4)),
            (BitwiseNot.name(), runtime(Constant(31))),
            (BitwiseLShift.name(), runtime(Constant(31))),
            (BitwiseRShift.name(), runtime(Constant(31))),

            // Conversions.
            (BuffToIntLe.name(), runtime(Constant(31))),
            (BuffToUintLe.name(), runtime(Constant(31))),
            (BuffToIntBe.name(), runtime(Constant(31))),
            (BuffToUintBe.name(), runtime(Constant(31))),
            (IntToAscii.name(), runtime(Constant(31))),
            (IntToUtf8.name(), runtime(Constant(31))),
            (StringToInt.name(), shifted(1, 31, 4)),
            (StringToUint.name(), shifted(1, 31, 4)),
            (ToAscii.name(), shifted(1, 32, 6)),
            (ToConsensusBuff.name(), shifted(1, 38, 9)),
            (FromConsensusBuff.name(), shifted(1, 38, 9)),

            // Hashing and signatures.
            (Hash160.name(), shifted(1, 38, 9)),
            (Sha256.name(), shifted(1, 38, 9)),
            (Sha512.name(), shifted(1, 38, 9)),
            (Sha512_256.name(), shifted(1, 38, 9)),
            (Keccak256.name(), shifted(1, 38, 9)),
            (Recover.name(), runtime(Constant(38))),
            (Verify.name(), runtime(Constant(38))),
            (Secp256r1Verify.name(), runtime(Constant(38))),
            (Ed25519Verify.name(), shifted(1, 39, 9)),
            (Decompress.name(), runtime(Constant(39))),

            // Bitcoin words.
            (VerifyMerkleProof.name(), shifted(1, 38, 2)),
            (GetBitcoinTxOutput.name(), shifted(1, 38, 9)),

            // Options, responses and control flow.
            (ClaritySome.name(), runtime(Constant(31))),
            (ClarityOk.name(), runtime(Constant(31))),
            (ClarityErr.name(), runtime(Constant(31))),
            (DefaultTo.name(), runtime(Constant(31))),
            (Unwrap.name(), runtime(Constant(31))),
            (UnwrapErr.name(), runtime(Constant(31))),
            (UnwrapPanic.name(), runtime(Constant(31))),
            (UnwrapErrPanic.name(), runtime(Constant(31))),
            (IsOk.name(), runtime(Constant(31))),
            (IsErr.name(), runtime(Constant(31))),
            (IsNone.name(), runtime(Constant(31))),
            (IsSome.name(), runtime(Constant(31))),
            (Try.name(), runtime(Constant(31))),
            (Match.name(), runtime(Constant(31))),
            (Begin.name(), shifted(1, 31, 4)),
            (Let.name(), shifted(1, 32, 3)),

            // Sequences and tuples.
            (Len.name(), runtime(Constant(31))),
            (ElementAt::Original.name(), runtime(Constant(31))),
            (ElementAt::Alias.name(), runtime(Constant(31))),
            (IndexOf::Original.name(), shifted(1, 38, 9)),
            (IndexOf::Alias.name(), shifted(1, 38, 9)),
            (Append.name(), shifted(1, 31, 4)),
            (Concat.name(), shifted(1, 31, 10)),
            (AsMaxLen.name(), runtime(Constant(31))),
            (ListCons.name(), shifted(1, 31, 4)),
            (Slice.name(), shifted(1, 38, 9)),
            (ReplaceAt.name(), shifted(1, 31, 9)),
            (Fold.name(), shifted(1, 32, 3)),
            (Map.name(), shifted(1, 32, 2)),
            (Filter.name(), shifted(1, 32, 2)),
            (TupleCons.name(), shifted(1, 31, 2)),
            (TupleGet.name(), shifted(1, 32, 2)),
            (TupleMerge.name(), shifted(1, 32, 2)),

            // Principals.
            (IsStandard.name(), runtime(Constant(31))),
            (Destruct.name(), runtime(Constant(32))),
            (Construct.name(), runtime(Constant(32))),

            // Printing.
            (Print.name(), shifted(1, 31, 9)),
        ] {
            map.insert(word, cost);
        }

        // Data words also charge the dimensions their storage touches.
        let fetch_entry = WordCost {
            runtime: Constant(44),
            read_count: Constant(1),
            read_length: Linear { a: 1, b: 1 },
            write_count: None,
            write_length: None,
        };
        let set_entry = WordCost {
            runtime: LinearShift {
                a: 3,
                b: 45,
                shift: 9,
            },
            read_count: Constant(1),
            read_length: None,
            write_count: Constant(1),
            write_length: Linear { a: 1, b: 1 },
        };
        map.insert(MapGet.name(), fetch_entry);
        map.insert(MapSet.name(), set_entry);
        map.insert(MapInsert.name(), set_entry);
        map.insert(MapDelete.name(), set_entry);
        map.insert(
            GetDataVar.name(),
            WordCost {
                runtime: LinearShift {
                    a: 2,
                    b: 44,
                    shift: 9,
                },
                read_count: Constant(1),
                read_length: Linear { a: 1, b: 1 },
                write_count: None,
                write_length: None,
            },
        );
        map.insert(
            SetDataVar.name(),
            WordCost {
                runtime: LinearShift {
                    a: 4,
                    b: 45,
                    shift: 9,
                },
                read_count: Constant(1),
                read_length: None,
                write_count: Constant(1),
                write_length: Linear { a: 1, b: 1 },
            },
        );

        map
    };
}
