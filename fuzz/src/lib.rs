use arbitrary::Arbitrary;

const MAX_EDITS: usize = 128;
const MAX_MUTATED_BYTES: usize = 2 * 1024 * 1024;

#[derive(Arbitrary, Debug)]
pub struct MutationCase {
    seed: u8,
    edits: Vec<Edit>,
}

#[derive(Arbitrary, Debug)]
enum Edit {
    Set { offset: u16, value: u8 },
    Truncate { length: u16 },
    Insert { offset: u16, bytes: Vec<u8> },
    Duplicate { offset: u16, length: u8 },
}

pub fn mutate(case: MutationCase, seeds: &[&str]) -> Vec<u8> {
    let seed = seeds[usize::from(case.seed) % seeds.len()];
    let bytes = hex::decode(seed.split_whitespace().collect::<String>())
        .expect("checked-in hexadecimal seed");
    apply_edits(case, bytes)
}

pub fn mutate_bytes(case: MutationCase, seeds: &[&[u8]]) -> Vec<u8> {
    let seed = seeds[usize::from(case.seed) % seeds.len()];
    apply_edits(case, seed.to_vec())
}

fn apply_edits(case: MutationCase, mut bytes: Vec<u8>) -> Vec<u8> {
    for edit in case.edits.into_iter().take(MAX_EDITS) {
        match edit {
            Edit::Set { offset, value } if !bytes.is_empty() => {
                let index = usize::from(offset) % bytes.len();
                bytes[index] = value;
            }
            Edit::Truncate { length } => bytes.truncate(usize::from(length).min(bytes.len())),
            Edit::Insert {
                offset,
                bytes: mut inserted,
            } => {
                inserted.truncate(256);
                let index = usize::from(offset).min(bytes.len());
                if bytes.len().saturating_add(inserted.len()) <= MAX_MUTATED_BYTES {
                    bytes.splice(index..index, inserted);
                }
            }
            Edit::Duplicate { offset, length } if !bytes.is_empty() => {
                let start = usize::from(offset) % bytes.len();
                let end = start.saturating_add(usize::from(length)).min(bytes.len());
                let duplicate = bytes[start..end].to_vec();
                if bytes.len().saturating_add(duplicate.len()) <= MAX_MUTATED_BYTES {
                    bytes.extend_from_slice(&duplicate);
                }
            }
            Edit::Set { .. } | Edit::Duplicate { .. } => {}
        }
    }
    bytes
}

#[derive(Arbitrary, Debug)]
pub struct P2pCase {
    mainnet: bool,
    wrong_network: bool,
    epoch: u8,
    sequence: u32,
    stable_height: u64,
    advance: u16,
    payload: P2pPayload,
    signature: Vec<u8>,
}

#[derive(Arbitrary, Debug)]
enum P2pPayload {
    HandshakeReject,
    GetNeighbors,
    Nack(u32),
    Ping(u32),
    Pong(u32),
    Raw(Vec<u8>),
}

pub fn p2p_bytes(mut case: P2pCase) -> Vec<u8> {
    let major = if case.mainnet {
        0x1800_0000
    } else {
        0xfaca_de00
    };
    let network: u32 = if case.mainnet { 1 } else { 0x8000_0000 };
    let network = if case.wrong_network {
        network ^ 1
    } else {
        network
    };
    let mut frame = 0_u32.to_be_bytes().to_vec();
    match case.payload {
        P2pPayload::HandshakeReject => frame.push(2),
        P2pPayload::GetNeighbors => frame.push(3),
        P2pPayload::Nack(value) => {
            frame.push(14);
            frame.extend_from_slice(&value.to_be_bytes());
        }
        P2pPayload::Ping(value) => {
            frame.push(15);
            frame.extend_from_slice(&value.to_be_bytes());
        }
        P2pPayload::Pong(value) => {
            frame.push(16);
            frame.extend_from_slice(&value.to_be_bytes());
        }
        P2pPayload::Raw(mut bytes) => {
            bytes.truncate(64 * 1024);
            frame.extend_from_slice(&bytes);
        }
    }
    let stable_height = case
        .stable_height
        .min(u64::MAX - u64::from(case.advance) - 1);
    let height = stable_height + u64::from(case.advance) + 1;
    case.signature.resize(65, 0);
    case.signature.truncate(65);

    let mut bytes = Vec::with_capacity(165 + frame.len());
    bytes.extend_from_slice(&(major | u32::from(case.epoch)).to_be_bytes());
    bytes.extend_from_slice(&network.to_be_bytes());
    bytes.extend_from_slice(&case.sequence.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[0; 32]);
    bytes.extend_from_slice(&stable_height.to_be_bytes());
    bytes.extend_from_slice(&[0; 32]);
    bytes.extend_from_slice(&0_u32.to_be_bytes());
    bytes.extend_from_slice(&case.signature);
    bytes.extend_from_slice(
        &u32::try_from(frame.len())
            .expect("bounded frame")
            .to_be_bytes(),
    );
    bytes.extend_from_slice(&frame);
    bytes
}

#[derive(Arbitrary, Debug)]
pub struct SessionCase {
    operations: Vec<u8>,
}

pub fn session_bytes(mut case: SessionCase) -> Vec<u8> {
    case.operations.truncate(32);
    case.operations
}

#[derive(Arbitrary, Debug)]
pub struct MarfCase {
    operations: Vec<MarfOperation>,
}

#[derive(Arbitrary, Debug)]
enum MarfOperation {
    Insert { path: [u8; 32], value: [u8; 40] },
    Read { path: [u8; 32] },
}

pub fn marf_bytes(case: MarfCase) -> Vec<u8> {
    let mut bytes = Vec::new();
    for operation in case.operations.into_iter().take(256) {
        match operation {
            MarfOperation::Insert { path, value } => {
                bytes.push(0);
                bytes.extend_from_slice(&path);
                bytes.extend_from_slice(&value);
            }
            MarfOperation::Read { path } => {
                bytes.push(1);
                bytes.extend_from_slice(&path);
                bytes.extend_from_slice(&[0; 40]);
            }
        }
    }
    bytes
}

#[derive(Arbitrary, Debug)]
pub struct ClarityCase {
    template: u8,
    width: u8,
    value: u64,
    bytes: Vec<u8>,
}

pub fn clarity_source(mut case: ClarityCase) -> String {
    case.bytes.truncate(64);
    let width = usize::from(case.width % 16) + 1;
    match case.template % 5 {
        0 => format!(
            "(define-read-only (answer (value uint)) (ok (+ value u{})))",
            case.value
        ),
        1 => format!(
            "(define-read-only (answer (value (optional {{extra: uint, kept: uint}}))) \
             (default-to {{kept: u{}}} value))",
            case.value
        ),
        2 => format!(
            "(define-read-only (answer (values (list {width} uint))) \
             (index-of? values u{}))",
            case.value
        ),
        3 => {
            let fields = (0..width)
                .map(|index| format!("f{index}: uint"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("(define-read-only (answer (value {{{fields}}})) (ok (get f0 value)))")
        }
        _ => format!(
            "(define-read-only (answer) (ok 0x{}))",
            hex::encode(case.bytes)
        ),
    }
}
