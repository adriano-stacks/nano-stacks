#![forbid(unsafe_code)]

use std::fmt;

use bitcoin::{
    Block,
    consensus::deserialize,
    hashes::Hash,
    script::{Instruction, Script},
};

/// A Bitcoin block accepted by the HTTP/RPC ingest boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BitcoinBlock {
    pub height: u64,
    pub hash: [u8; 32],
    pub operations: Vec<BitcoinOperation>,
}

/// The source boundary for Bitcoin input.
pub trait BitcoinSource {
    type Error;

    fn block_at(&self, height: u64) -> Result<BitcoinBlock, Self::Error>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BitcoinOperation {
    pub txid: [u8; 32],
    pub transaction_index: u32,
    pub kind: BitcoinOperationKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BitcoinOperationKind {
    LeaderBlockCommit {
        block_header_hash: [u8; 32],
        new_seed: [u8; 32],
        parent_block_height: u32,
        parent_transaction_index: u16,
        key_block_height: u32,
        key_transaction_index: u16,
        memo: u8,
        parent_modulus: u8,
    },
    LeaderKeyRegistration {
        consensus_hash: [u8; 20],
        vrf_public_key: [u8; 32],
        block_signing_key_hash: Option<[u8; 20]>,
        memo: Vec<u8>,
    },
    PreStx,
    StackStx {
        amount: u128,
        cycles: u8,
        signer_key: Option<[u8; 33]>,
        max_amount: Option<u128>,
        authorization_id: Option<u32>,
    },
    TransferStx {
        amount: u128,
        memo: Vec<u8>,
    },
    DelegateStx {
        amount: u128,
        reward_address_output: Option<u32>,
        until_bitcoin_height: Option<u64>,
    },
    VoteForAggregateKey {
        signer_index: u16,
        aggregate_key: [u8; 33],
        round: u32,
        reward_cycle: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BitcoinParseError {
    InvalidBlock,
    TooManyTransactions,
}

impl fmt::Display for BitcoinParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidBlock => "invalid Bitcoin block",
            Self::TooManyTransactions => "Bitcoin block has too many transactions",
        })
    }
}

impl std::error::Error for BitcoinParseError {}

/// Decode a Bitcoin block and classify protocol operations in output zero.
pub fn decode_block(
    height: u64,
    bytes: &[u8],
    magic: [u8; 2],
) -> Result<BitcoinBlock, BitcoinParseError> {
    let block: Block = deserialize(bytes).map_err(|_| BitcoinParseError::InvalidBlock)?;
    let mut operations = Vec::new();
    for (index, transaction) in block.txdata.iter().enumerate() {
        let index = u32::try_from(index).map_err(|_| BitcoinParseError::TooManyTransactions)?;
        let Some((opcode, payload)) = transaction
            .output
            .first()
            .and_then(|output| protocol_payload(output.script_pubkey.as_script(), magic))
        else {
            continue;
        };
        let Some(kind) = parse_operation(opcode, payload) else {
            continue;
        };
        operations.push(BitcoinOperation {
            txid: transaction.compute_txid().to_byte_array(),
            transaction_index: index,
            kind,
        });
    }
    Ok(BitcoinBlock {
        height,
        hash: block.block_hash().to_byte_array(),
        operations,
    })
}

fn protocol_payload(script: &Script, magic: [u8; 2]) -> Option<(u8, &[u8])> {
    let mut instructions = script.instructions_minimal();
    let Instruction::Op(op_return) = instructions.next()?.ok()? else {
        return None;
    };
    if op_return.to_u8() != 0x6a {
        return None;
    }
    let Instruction::PushBytes(data) = instructions.next()?.ok()? else {
        return None;
    };
    if instructions.next().is_some() {
        return None;
    }
    let data = data.as_bytes();
    (data.starts_with(&magic) && data.len() > magic.len()).then(|| (data[2], &data[3..]))
}

fn parse_operation(opcode: u8, data: &[u8]) -> Option<BitcoinOperationKind> {
    match opcode {
        b'[' => parse_leader_block_commit(data),
        b'^' => parse_leader_key_registration(data),
        b'p' => Some(BitcoinOperationKind::PreStx),
        b'x' => parse_stack_stx(data),
        b'$' => parse_transfer_stx(data),
        b'#' => parse_delegate_stx(data),
        b'v' => parse_vote_for_aggregate_key(data),
        _ => None,
    }
}

fn parse_leader_block_commit(data: &[u8]) -> Option<BitcoinOperationKind> {
    let block_header_hash = array(data.get(..32)?)?;
    let new_seed = array(data.get(32..64)?)?;
    let parent_block_height = u32::from_be_bytes(array(data.get(64..68)?)?);
    let parent_transaction_index = u16::from_be_bytes(array(data.get(68..70)?)?);
    let key_block_height = u32::from_be_bytes(array(data.get(70..74)?)?);
    let key_transaction_index = u16::from_be_bytes(array(data.get(74..76)?)?);
    let flags = *data.get(76)?;
    Some(BitcoinOperationKind::LeaderBlockCommit {
        block_header_hash,
        new_seed,
        parent_block_height,
        parent_transaction_index,
        key_block_height,
        key_transaction_index,
        memo: flags >> 3,
        parent_modulus: (flags & 0b111) % 5,
    })
}

fn parse_leader_key_registration(data: &[u8]) -> Option<BitcoinOperationKind> {
    let consensus_hash = array(data.get(..20)?)?;
    let vrf_public_key = array(data.get(20..52)?)?;
    let memo = data.get(52..)?.to_vec();
    let block_signing_key_hash = memo.get(..20).and_then(array);
    Some(BitcoinOperationKind::LeaderKeyRegistration {
        consensus_hash,
        vrf_public_key,
        block_signing_key_hash,
        memo,
    })
}

fn parse_stack_stx(data: &[u8]) -> Option<BitcoinOperationKind> {
    let amount = u128::from_be_bytes(array(data.get(..16)?)?);
    let cycles = *data.get(16)?;
    let signer_key = data.get(17..50).and_then(array);
    let max_amount = data.get(50..66).and_then(array).map(u128::from_be_bytes);
    let authorization_id = data.get(66..70).and_then(array).map(u32::from_be_bytes);
    Some(BitcoinOperationKind::StackStx {
        amount,
        cycles,
        signer_key,
        max_amount,
        authorization_id,
    })
}

fn parse_transfer_stx(data: &[u8]) -> Option<BitcoinOperationKind> {
    if !(16..=77).contains(&data.len()) {
        return None;
    }
    Some(BitcoinOperationKind::TransferStx {
        amount: u128::from_be_bytes(array(data.get(..16)?)?),
        memo: data.get(16..)?.to_vec(),
    })
}

fn parse_delegate_stx(data: &[u8]) -> Option<BitcoinOperationKind> {
    let amount = u128::from_be_bytes(array(data.get(..16)?)?);
    let reward_address_output = match *data.get(16)? {
        0 => None,
        1 => Some(u32::from_be_bytes(array(data.get(17..21)?)?)),
        _ => return None,
    };
    let until_bitcoin_height = match *data.get(21)? {
        0 => None,
        1 => Some(u64::from_be_bytes(array(data.get(22..30)?)?)),
        _ => return None,
    };
    Some(BitcoinOperationKind::DelegateStx {
        amount,
        reward_address_output,
        until_bitcoin_height,
    })
}

fn parse_vote_for_aggregate_key(data: &[u8]) -> Option<BitcoinOperationKind> {
    if data.len() != 47 {
        return None;
    }
    Some(BitcoinOperationKind::VoteForAggregateKey {
        signer_index: u16::from_be_bytes(array(data.get(..2)?)?),
        aggregate_key: array(data.get(2..35)?)?,
        round: u32::from_be_bytes(array(data.get(35..39)?)?),
        reward_cycle: u64::from_be_bytes(array(data.get(39..47)?)?),
    })
}

fn array<const N: usize>(bytes: &[u8]) -> Option<[u8; N]> {
    bytes.try_into().ok()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::decode_block;

    #[test]
    fn captured_bitcoin_blocks_decode_with_hacknet_magic() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../nano-conformance/fixtures/bitcoin/blocks");
        for entry in fs::read_dir(directory).expect("read fixture directory") {
            let path = entry.expect("fixture entry").path();
            let hex = fs::read_to_string(&path).expect("read fixture block");
            let bytes = hex::decode(hex.trim()).expect("decode fixture hex");
            let block = decode_block(0, &bytes, *b"T3")
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            assert_ne!(block.hash, [0; 32]);
        }
    }
}
