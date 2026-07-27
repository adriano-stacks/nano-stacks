#![forbid(unsafe_code)]

use std::{collections::HashMap, fmt};

use bitcoin::{
    Block,
    consensus::deserialize,
    hashes::Hash,
    script::{Instruction, Script},
};
use nano_address::PoxAddress;

/// A Bitcoin block accepted by the HTTP/RPC ingest boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BitcoinBlock {
    pub height: u64,
    pub hash: [u8; 32],
    pub operations: Vec<BitcoinOperation>,
}

const PRE_STX_WINDOW_BLOCKS: u64 = 6;

/// `PreStx` outputs available to later Bitcoin blocks.
#[derive(Clone, Debug, Default)]
pub struct PreStxCache {
    senders: HashMap<[u8; 32], (nano_address::StacksAddress, u64)>,
}

impl PreStxCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn retain_window(&mut self, height: u64) {
        self.senders.retain(|_, (_, seen_height)| {
            height.saturating_sub(*seen_height) <= PRE_STX_WINDOW_BLOCKS
        });
    }
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
    pub inputs: Vec<BitcoinInput>,
    pub outputs: Vec<BitcoinOutput>,
    pub kind: BitcoinOperationKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BitcoinInput {
    pub txid: [u8; 32],
    pub output_index: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BitcoinOutput {
    pub amount_sats: u64,
    pub recipient: PoxAddress,
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
    PreStx {
        sender: nano_address::StacksAddress,
    },
    StackStx {
        sender: nano_address::StacksAddress,
        reward_address: PoxAddress,
        amount: u128,
        cycles: u8,
        signer_key: Option<[u8; 33]>,
        max_amount: Option<u128>,
        authorization_id: Option<u32>,
    },
    TransferStx {
        sender: nano_address::StacksAddress,
        recipient: nano_address::StacksAddress,
        amount: u128,
        memo: Vec<u8>,
    },
    DelegateStx {
        sender: nano_address::StacksAddress,
        delegate: nano_address::StacksAddress,
        amount: u128,
        reward_address: Option<PoxAddress>,
        reward_address_output: Option<u32>,
        until_bitcoin_height: Option<u64>,
    },
    VoteForAggregateKey {
        sender: nano_address::StacksAddress,
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
    decode_block_with_pre_stx(height, bytes, magic, &mut PreStxCache::new())
}

/// Decode a Bitcoin block while retaining `PreStx` outputs needed by later blocks.
pub fn decode_block_with_pre_stx(
    height: u64,
    bytes: &[u8],
    magic: [u8; 2],
    pre_stx_cache: &mut PreStxCache,
) -> Result<BitcoinBlock, BitcoinParseError> {
    pre_stx_cache.retain_window(height);
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
        let Some(outputs) = transaction
            .output
            .iter()
            .skip(1)
            .map(|output| {
                PoxAddress::from_script_pubkey(output.script_pubkey.as_bytes(), false)
                    .ok()
                    .map(|recipient| BitcoinOutput {
                        amount_sats: output.value.to_sat(),
                        recipient,
                    })
            })
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        if transaction.input.is_empty() || outputs.is_empty() {
            continue;
        }
        let inputs: Vec<_> = transaction
            .input
            .iter()
            .map(|input| BitcoinInput {
                txid: bitcoin_hash_bytes(input.previous_output.txid.to_byte_array()),
                output_index: input.previous_output.vout,
            })
            .collect();
        let sender = inputs
            .first()
            .filter(|input| input.output_index == 1)
            .and_then(|input| pre_stx_cache.senders.get(&input.txid))
            .map(|(sender, _)| *sender);
        let Some(kind) = parse_operation(opcode, payload, &outputs, sender) else {
            continue;
        };
        let txid = bitcoin_hash_bytes(transaction.compute_txid().to_byte_array());
        if let BitcoinOperationKind::PreStx { sender } = &kind {
            pre_stx_cache.senders.insert(txid, (*sender, height));
        }
        operations.push(BitcoinOperation {
            txid,
            transaction_index: index,
            inputs,
            outputs,
            kind,
        });
    }
    Ok(BitcoinBlock {
        height,
        hash: bitcoin_hash_bytes(block.block_hash().to_byte_array()),
        operations,
    })
}

fn bitcoin_hash_bytes(mut bytes: [u8; 32]) -> [u8; 32] {
    bytes.reverse();
    bytes
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

fn parse_operation(
    opcode: u8,
    data: &[u8],
    outputs: &[BitcoinOutput],
    sender: Option<nano_address::StacksAddress>,
) -> Option<BitcoinOperationKind> {
    match opcode {
        b'[' => parse_leader_block_commit(data),
        b'^' => parse_leader_key_registration(data),
        b'p' => Some(BitcoinOperationKind::PreStx {
            sender: outputs.first()?.recipient.as_stacks_address()?,
        }),
        b'x' => parse_stack_stx(data, outputs, sender?),
        b'$' => parse_transfer_stx(data, outputs, sender?),
        b'#' => parse_delegate_stx(data, outputs, sender?),
        b'v' => parse_vote_for_aggregate_key(data, sender?),
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
    nano_crypto::VrfPublicKey::from_bytes(vrf_public_key).ok()?;
    let memo = data.get(52..)?.to_vec();
    let block_signing_key_hash = memo.get(..20).and_then(array);
    Some(BitcoinOperationKind::LeaderKeyRegistration {
        consensus_hash,
        vrf_public_key,
        block_signing_key_hash,
        memo,
    })
}

fn parse_stack_stx(
    data: &[u8],
    outputs: &[BitcoinOutput],
    sender: nano_address::StacksAddress,
) -> Option<BitcoinOperationKind> {
    let amount = u128::from_be_bytes(array(data.get(..16)?)?);
    let cycles = *data.get(16)?;
    let signer_key = data.get(17..50).and_then(array);
    let max_amount = data.get(50..66).and_then(array).map(u128::from_be_bytes);
    let authorization_id = data.get(66..70).and_then(array).map(u32::from_be_bytes);
    Some(BitcoinOperationKind::StackStx {
        sender,
        reward_address: outputs.first()?.recipient.clone(),
        amount,
        cycles,
        signer_key,
        max_amount,
        authorization_id,
    })
}

fn parse_transfer_stx(
    data: &[u8],
    outputs: &[BitcoinOutput],
    sender: nano_address::StacksAddress,
) -> Option<BitcoinOperationKind> {
    if !(16..=77).contains(&data.len()) {
        return None;
    }
    Some(BitcoinOperationKind::TransferStx {
        sender,
        recipient: outputs.first()?.recipient.as_stacks_address()?,
        amount: u128::from_be_bytes(array(data.get(..16)?)?),
        memo: data.get(16..)?.to_vec(),
    })
}

fn parse_delegate_stx(
    data: &[u8],
    outputs: &[BitcoinOutput],
    sender: nano_address::StacksAddress,
) -> Option<BitcoinOperationKind> {
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
        sender,
        delegate: outputs.first()?.recipient.as_stacks_address()?,
        amount,
        reward_address: reward_address_output
            .and_then(|index| outputs.get(usize::try_from(index).ok()?).cloned())
            .map(|output| output.recipient),
        reward_address_output,
        until_bitcoin_height,
    })
}

fn parse_vote_for_aggregate_key(
    data: &[u8],
    sender: nano_address::StacksAddress,
) -> Option<BitcoinOperationKind> {
    if data.len() != 47 {
        return None;
    }
    Some(BitcoinOperationKind::VoteForAggregateKey {
        sender,
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

    use bitcoin::{
        Amount, Block, BlockHash, CompactTarget, OutPoint, ScriptBuf, Transaction, TxIn,
        TxMerkleNode, TxOut,
        absolute::LockTime,
        block::{Header, Version as BlockVersion},
        consensus::serialize,
        hashes::Hash,
        transaction::Version as TransactionVersion,
    };

    use super::{
        BitcoinOperationKind, PreStxCache, decode_block, decode_block_with_pre_stx,
        parse_leader_key_registration,
    };

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
            assert_eq!(block.operations.len(), 3, "{}", path.display());
        }
    }

    #[test]
    fn captured_bitcoin_blocks_keep_prestx_sender_state() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../nano-conformance/fixtures/bitcoin/blocks");
        let mut paths = fs::read_dir(directory)
            .expect("read fixture directory")
            .map(|entry| entry.expect("fixture entry").path())
            .collect::<Vec<_>>();
        paths.sort();
        let mut pre_stx_cache = PreStxCache::new();
        for path in paths {
            let hex = fs::read_to_string(&path).expect("read fixture block");
            let bytes = hex::decode(hex.trim()).expect("decode fixture hex");
            let block = decode_block_with_pre_stx(0, &bytes, *b"T3", &mut pre_stx_cache)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            assert_eq!(block.operations.len(), 3, "{}", path.display());
        }
    }

    #[test]
    fn prestx_cache_expires_after_six_bitcoin_blocks() {
        let mut cache = PreStxCache::new();
        let sender =
            nano_address::StacksAddress::new(26, nano_primitives::Hash160::from_bytes([0x24; 20]))
                .expect("valid Stacks address");
        cache.senders.insert([0x42; 32], (sender, 100));

        cache.retain_window(106);
        assert_eq!(cache.senders.len(), 1);
        cache.retain_window(107);
        assert!(cache.senders.is_empty());
    }

    #[test]
    fn prestx_sender_is_resolved_from_the_second_output() {
        let pre_stx = transaction(
            vec![TxIn::default()],
            vec![protocol_output(b'p', &[]), p2pkh_output(0x24)],
        );
        let transfer = transaction(
            vec![TxIn {
                previous_output: OutPoint::new(pre_stx.compute_txid(), 1),
                ..TxIn::default()
            }],
            vec![protocol_output(b'$', &[0; 16]), p2pkh_output(0x42)],
        );

        let block = decode_block(100, &block_bytes(vec![pre_stx, transfer]), *b"T3")
            .expect("valid Bitcoin block");
        assert_eq!(block.operations.len(), 2);
        match (&block.operations[0].kind, &block.operations[1].kind) {
            (
                BitcoinOperationKind::PreStx { sender },
                BitcoinOperationKind::TransferStx {
                    sender: transfer_sender,
                    ..
                },
            ) => assert_eq!(sender, transfer_sender),
            operations => panic!("unexpected operations: {operations:?}"),
        }
    }

    #[test]
    fn operations_require_an_input_and_a_decodable_output() {
        let transaction = transaction(vec![], vec![protocol_output(b'p', &[]), p2pkh_output(0x24)]);
        let block = decode_block(100, &block_bytes(vec![transaction]), *b"T3")
            .expect("valid Bitcoin block");
        assert!(block.operations.is_empty());
    }

    #[test]
    fn leader_key_registration_requires_a_valid_vrf_key() {
        assert!(parse_leader_key_registration(&[0; 52]).is_none());
    }

    fn transaction(input: Vec<TxIn>, output: Vec<TxOut>) -> Transaction {
        Transaction {
            version: TransactionVersion::TWO,
            lock_time: LockTime::ZERO,
            input,
            output,
        }
    }

    fn protocol_output(opcode: u8, payload: &[u8]) -> TxOut {
        let mut data = Vec::with_capacity(payload.len() + 3);
        data.extend_from_slice(b"T3");
        data.push(opcode);
        data.extend_from_slice(payload);
        let length = u8::try_from(data.len()).expect("test packet fits direct push");
        TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::from_bytes([vec![0x6a, length], data].concat()),
        }
    }

    fn p2pkh_output(byte: u8) -> TxOut {
        TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::from_bytes(
                [vec![0x76, 0xa9, 0x14], vec![byte; 20], vec![0x88, 0xac]].concat(),
            ),
        }
    }

    fn block_bytes(transactions: Vec<Transaction>) -> Vec<u8> {
        serialize(&Block {
            header: Header {
                version: BlockVersion::ONE,
                prev_blockhash: BlockHash::all_zeros(),
                merkle_root: TxMerkleNode::all_zeros(),
                time: 0,
                bits: CompactTarget::from_consensus(0),
                nonce: 0,
            },
            txdata: transactions,
        })
    }
}
