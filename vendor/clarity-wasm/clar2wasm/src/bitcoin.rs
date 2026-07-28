use clarity::vm::errors::{RuntimeCheckErrorKind, VmExecutionError};
use clarity::vm::types::{BuffData, ListData, SequenceData, TupleData, TypeSignature};
use clarity::vm::{ClarityName, Value};
use stacks_common::deps_common::bitcoin::blockdata::transaction::Transaction;
use stacks_common::deps_common::bitcoin::network::serialize::deserialize;
use stacks_common::deps_common::bitcoin::util::hash::Sha256dHash;

const MAX_PROOF_DEPTH: u32 = 24;
const MAX_SCRIPT_LENGTH: usize = 1024;

pub fn verify_merkle_proof(
    leaf: Value,
    root: Value,
    index: u128,
    count: u128,
    siblings: Value,
) -> Result<bool, VmExecutionError> {
    let leaf = buffer::<32>(leaf, TypeSignature::BUFFER_32)?;
    let root = buffer::<32>(root, TypeSignature::BUFFER_32)?;
    let list = match siblings {
        Value::Sequence(SequenceData::List(ListData { data, .. })) => data,
        value => {
            return Err(RuntimeCheckErrorKind::TypeValueError(
                Box::new(TypeSignature::list_of(
                    TypeSignature::BUFFER_32,
                    MAX_PROOF_DEPTH,
                )?),
                value.to_error_string(),
            )
            .into());
        }
    };
    let siblings = list
        .into_iter()
        .map(|value| buffer::<32>(value, TypeSignature::BUFFER_32))
        .collect::<Result<Vec<_>, _>>();
    let Ok(siblings) = siblings else {
        return Ok(false);
    };
    if count == 0 || index >= count || siblings.len() as u32 != proof_depth(count) {
        return Ok(false);
    }
    let mut hash = leaf;
    let mut position = index;
    let mut row_length = count;
    for sibling in siblings {
        let sibling_position = position | 1;
        if sibling_position >= row_length {
            if row_length & 1 == 0 || position != row_length - 1 || sibling != hash {
                return Ok(false);
            }
        } else if sibling == hash {
            return Ok(false);
        }
        let mut input = [0; 64];
        if position & 1 == 0 {
            input[..32].copy_from_slice(&hash);
            input[32..].copy_from_slice(&sibling);
        } else {
            input[..32].copy_from_slice(&sibling);
            input[32..].copy_from_slice(&hash);
        }
        hash = Sha256dHash::from_data(&input).0;
        position >>= 1;
        row_length = (row_length + 1) >> 1;
    }
    Ok(hash == root)
}

pub fn get_bitcoin_tx_output(tx: Value, vout: u128) -> Result<Value, VmExecutionError> {
    let bytes = match tx {
        Value::Sequence(SequenceData::Buffer(BuffData { data })) => data,
        value => {
            return Err(RuntimeCheckErrorKind::TypeValueError(
                Box::new(TypeSignature::BUFFER_MAX),
                value.to_error_string(),
            )
            .into());
        }
    };
    let Ok(vout) = usize::try_from(vout) else {
        return Ok(Value::err_uint(2));
    };
    let Ok(transaction) = deserialize::<Transaction>(&bytes) else {
        return Ok(Value::err_uint(1));
    };
    let Some(output) = transaction.output.get(vout) else {
        return Ok(Value::err_uint(2));
    };
    let script = output.script_pubkey.as_bytes();
    if script.len() > MAX_SCRIPT_LENGTH {
        return Ok(Value::err_uint(3));
    }
    let tuple = TupleData::from_data(vec![
        (
            ClarityName::from_literal("script"),
            Value::buff_from(script.to_vec())?,
        ),
        (
            ClarityName::from_literal("amount"),
            Value::UInt(u128::from(output.value)),
        ),
        (
            ClarityName::from_literal("txid"),
            Value::buff_from(transaction.txid().0.to_vec())?,
        ),
    ])?;
    Ok(Value::okay(Value::Tuple(tuple))?)
}

fn buffer<const N: usize>(
    value: Value,
    expected: TypeSignature,
) -> Result<[u8; N], VmExecutionError> {
    let error_value = value.to_error_string();
    let Value::Sequence(SequenceData::Buffer(BuffData { data })) = value else {
        return Err(RuntimeCheckErrorKind::TypeValueError(Box::new(expected), error_value).into());
    };
    data.try_into()
        .map_err(|_| RuntimeCheckErrorKind::TypeValueError(Box::new(expected), error_value).into())
}

const fn proof_depth(count: u128) -> u32 {
    if count <= 1 {
        0
    } else {
        (count - 1).ilog2() + 1
    }
}
