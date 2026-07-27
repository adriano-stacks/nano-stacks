#![forbid(unsafe_code)]

use std::{fmt, str::FromStr};

use bitcoin::{Address as BitcoinAddress, Network, ScriptBuf};
use nano_primitives::Hash160;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddressError {
    InvalidVersion,
    InvalidC32,
    InvalidBase58,
    InvalidBitcoinAddress,
}

impl fmt::Display for AddressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidVersion => "invalid address version",
            Self::InvalidC32 => "invalid c32 address",
            Self::InvalidBase58 => "invalid Bitcoin base58 address",
            Self::InvalidBitcoinAddress => "invalid Bitcoin address",
        })
    }
}

impl std::error::Error for AddressError {}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct StacksAddress {
    version: u8,
    hash160: Hash160,
}

impl StacksAddress {
    pub const fn new(version: u8, hash160: Hash160) -> Result<Self, AddressError> {
        if version > 31 {
            return Err(AddressError::InvalidVersion);
        }
        Ok(Self { version, hash160 })
    }

    #[must_use]
    pub const fn version(self) -> u8 {
        self.version
    }

    #[must_use]
    pub const fn hash160(self) -> Hash160 {
        self.hash160
    }

    #[must_use]
    pub const fn is_mainnet(self) -> bool {
        matches!(self.version, 20 | 22)
    }

    #[must_use]
    pub fn is_burn(self) -> bool {
        self.hash160.as_bytes().iter().all(|byte| *byte == 0)
    }

    pub fn from_bitcoin_base58(address: &str) -> Result<Self, AddressError> {
        let bytes = bs58::decode(address)
            .with_check(None)
            .into_vec()
            .map_err(|_| AddressError::InvalidBase58)?;
        let (version, hash160) = bytes.split_first().ok_or(AddressError::InvalidBase58)?;
        let hash160: [u8; 20] = hash160
            .try_into()
            .map_err(|_| AddressError::InvalidBase58)?;
        let version = match *version {
            0 => 22,
            5 => 20,
            111 => 26,
            196 => 21,
            _ => return Err(AddressError::InvalidBase58),
        };
        Self::new(version, Hash160::from_bytes(hash160))
    }

    #[must_use]
    pub fn to_bitcoin_base58(self) -> String {
        let version = match self.version {
            22 => 0,
            20 => 5,
            26 => 111,
            21 => 196,
            other => other,
        };
        let mut bytes = Vec::with_capacity(21);
        bytes.push(version);
        bytes.extend_from_slice(self.hash160.as_bytes());
        bs58::encode(bytes).with_check().into_string()
    }
}

impl fmt::Display for StacksAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let encoded = c32::encode_check_prefixed(self.hash160.as_bytes(), 'S', self.version)
            .map_err(|_| fmt::Error)?;
        formatter.write_str(&encoded)
    }
}

impl FromStr for StacksAddress {
    type Err = AddressError;

    fn from_str(address: &str) -> Result<Self, Self::Err> {
        let (bytes, version) =
            c32::decode_check_prefixed(address, 'S').map_err(|_| AddressError::InvalidC32)?;
        let hash160: [u8; 20] = bytes.try_into().map_err(|_| AddressError::InvalidC32)?;
        Self::new(version, Hash160::from_bytes(hash160))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum AddressHashMode {
    P2pkh = 0,
    P2sh = 1,
    P2wpkh = 2,
    P2wsh = 3,
}

impl AddressHashMode {
    #[must_use]
    pub const fn mainnet_version(self) -> u8 {
        match self {
            Self::P2pkh => 22,
            Self::P2sh | Self::P2wpkh | Self::P2wsh => 20,
        }
    }

    #[must_use]
    pub const fn testnet_version(self) -> u8 {
        match self {
            Self::P2pkh => 26,
            Self::P2sh | Self::P2wpkh | Self::P2wsh => 21,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum PoxAddressType20 {
    P2wpkh = 4,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum PoxAddressType32 {
    P2wsh = 5,
    P2tr = 6,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum PoxAddress {
    Standard {
        address: StacksAddress,
        hash_mode: Option<AddressHashMode>,
    },
    Addr20 {
        mainnet: bool,
        address_type: PoxAddressType20,
        bytes: [u8; 20],
    },
    Addr32 {
        mainnet: bool,
        address_type: PoxAddressType32,
        bytes: [u8; 32],
    },
}

impl PoxAddress {
    #[must_use]
    pub fn bytes(&self) -> Vec<u8> {
        match self {
            Self::Standard { address, .. } => address.hash160().as_bytes().to_vec(),
            Self::Addr20 { bytes, .. } => bytes.to_vec(),
            Self::Addr32 { bytes, .. } => bytes.to_vec(),
        }
    }

    #[must_use]
    pub fn script_pubkey(&self) -> ScriptBuf {
        let bytes = self.bytes();
        let script = match self {
            Self::Standard { address, .. } if matches!(address.version(), 22 | 26) => {
                [vec![0x76, 0xa9, 0x14], bytes, vec![0x88, 0xac]].concat()
            }
            Self::Standard { .. } => [vec![0xa9, 0x14], bytes, vec![0x87]].concat(),
            Self::Addr20 { .. } => [vec![0x00, 0x14], bytes].concat(),
            Self::Addr32 {
                address_type: PoxAddressType32::P2wsh,
                ..
            } => [vec![0x00, 0x20], bytes].concat(),
            Self::Addr32 {
                address_type: PoxAddressType32::P2tr,
                ..
            } => [vec![0x51, 0x20], bytes].concat(),
        };
        ScriptBuf::from_bytes(script)
    }

    pub fn bitcoin_address(&self) -> Result<BitcoinAddress, AddressError> {
        let network = match self {
            Self::Standard { address, .. } => {
                if address.is_mainnet() {
                    Network::Bitcoin
                } else {
                    Network::Testnet
                }
            }
            Self::Addr20 { mainnet, .. } | Self::Addr32 { mainnet, .. } => {
                if *mainnet {
                    Network::Bitcoin
                } else {
                    Network::Testnet
                }
            }
        };
        BitcoinAddress::from_script(&self.script_pubkey(), network)
            .map_err(|_| AddressError::InvalidBitcoinAddress)
    }
}

pub const POX_5_SBTC_DEPOSIT_MAX_FEE_SATS: u64 = 80_000;

const NUMS_X_COORDINATE: [u8; 32] = [
    0x50, 0x92, 0x9b, 0x74, 0xc1, 0xa0, 0x49, 0x54, 0xb7, 0x8b, 0x4b, 0x60, 0x35, 0xe9, 0x7a, 0x5e,
    0x07, 0x8a, 0x5a, 0x0f, 0x28, 0xec, 0x96, 0xd5, 0x47, 0xbf, 0xee, 0x9a, 0xce, 0x80, 0x3a, 0xc0,
];

pub fn sbtc_pox5_deposit_taproot_output_key(
    aggregate_public_key: &[u8; 33],
    recipient_principal: &[u8],
) -> Result<[u8; 32], AddressError> {
    let aggregate_public_key: [u8; 32] = aggregate_public_key[1..]
        .try_into()
        .expect("fixed-size slice");
    sbtc_deposit_taproot_output_key(
        &aggregate_public_key,
        recipient_principal,
        POX_5_SBTC_DEPOSIT_MAX_FEE_SATS,
        u16::MAX,
        &[0x6a],
    )
}

pub fn sbtc_deposit_taproot_output_key(
    aggregate_public_key: &[u8; 32],
    recipient_principal: &[u8],
    max_fee_sats: u64,
    reclaim_lock_time: u16,
    user_reclaim_script: &[u8],
) -> Result<[u8; 32], AddressError> {
    use bitcoin::secp256k1::{Scalar, Secp256k1, XOnlyPublicKey};

    XOnlyPublicKey::from_slice(aggregate_public_key)
        .map_err(|_| AddressError::InvalidBitcoinAddress)?;
    let deposit_leaf = tap_leaf_hash(&deposit_script(
        aggregate_public_key,
        recipient_principal,
        max_fee_sats,
    )?);
    let reclaim_leaf = tap_leaf_hash(&reclaim_script(reclaim_lock_time, user_reclaim_script));
    let merkle_root = tap_branch_hash(deposit_leaf, reclaim_leaf);
    let internal_key = XOnlyPublicKey::from_slice(&NUMS_X_COORDINATE)
        .map_err(|_| AddressError::InvalidBitcoinAddress)?;
    let mut tweak_data = [0; 64];
    tweak_data[..32].copy_from_slice(&NUMS_X_COORDINATE);
    tweak_data[32..].copy_from_slice(&merkle_root);
    let tweak = Scalar::from_be_bytes(tagged_hash(b"TapTweak", &tweak_data))
        .map_err(|_| AddressError::InvalidBitcoinAddress)?;
    let (output_key, _) = internal_key
        .add_tweak(&Secp256k1::verification_only(), &tweak)
        .map_err(|_| AddressError::InvalidBitcoinAddress)?;
    Ok(output_key.serialize())
}

fn deposit_script(
    aggregate_public_key: &[u8; 32],
    recipient_principal: &[u8],
    max_fee_sats: u64,
) -> Result<Vec<u8>, AddressError> {
    let mut data = max_fee_sats.to_be_bytes().to_vec();
    data.extend_from_slice(recipient_principal);
    let mut script = push_data(&data)?;
    script.push(0x75);
    script.extend_from_slice(&push_data(aggregate_public_key)?);
    script.push(0xac);
    Ok(script)
}

fn reclaim_script(lock_time: u16, user_script: &[u8]) -> Vec<u8> {
    let mut script = encode_script_number(lock_time);
    script.push(0xb2);
    script.extend_from_slice(user_script);
    script
}

fn push_data(data: &[u8]) -> Result<Vec<u8>, AddressError> {
    let mut result = Vec::with_capacity(data.len() + 3);
    match data.len() {
        0 => result.push(0),
        1..=75 => result.push(u8::try_from(data.len()).expect("bounded push length")),
        76..=255 => {
            result
                .extend_from_slice(&[0x4c, u8::try_from(data.len()).expect("bounded push length")]);
        }
        256..=65_535 => {
            result.push(0x4d);
            result.extend_from_slice(
                &u16::try_from(data.len())
                    .expect("bounded push length")
                    .to_le_bytes(),
            );
        }
        65_536..=4_294_967_295 => {
            result.push(0x4e);
            result.extend_from_slice(
                &u32::try_from(data.len())
                    .expect("bounded push length")
                    .to_le_bytes(),
            );
        }
        _ => return Err(AddressError::InvalidBitcoinAddress),
    }
    result.extend_from_slice(data);
    Ok(result)
}

fn encode_script_number(value: u16) -> Vec<u8> {
    if value == 0 {
        return vec![0];
    }
    if value <= 16 {
        return vec![0x50 + u8::try_from(value).expect("small script number")];
    }
    let mut bytes = value.to_le_bytes().to_vec();
    while bytes.last() == Some(&0) {
        let _ = bytes.pop();
    }
    if bytes.last().is_some_and(|byte| byte & 0x80 != 0) {
        bytes.push(0);
    }
    let mut result = vec![u8::try_from(bytes.len()).expect("u16 script number length")];
    result.extend_from_slice(&bytes);
    result
}

fn tap_leaf_hash(script: &[u8]) -> [u8; 32] {
    let mut data = Vec::with_capacity(script.len() + 10);
    data.push(0xc0);
    compact_size(&mut data, script.len());
    data.extend_from_slice(script);
    tagged_hash(b"TapLeaf", &data)
}

fn tap_branch_hash(left: [u8; 32], right: [u8; 32]) -> [u8; 32] {
    let (left, right) = if left <= right {
        (left, right)
    } else {
        (right, left)
    };
    tagged_hash(b"TapBranch", &[left.as_slice(), right.as_slice()].concat())
}

fn tagged_hash(tag: &[u8], data: &[u8]) -> [u8; 32] {
    let tag_hash = Sha256::digest(tag);
    let mut hasher = Sha256::new();
    hasher.update(tag_hash);
    hasher.update(tag_hash);
    hasher.update(data);
    hasher.finalize().into()
}

fn compact_size(output: &mut Vec<u8>, value: usize) {
    match value {
        0..=252 => output.push(u8::try_from(value).expect("bounded compact size")),
        253..=65_535 => {
            output.push(0xfd);
            output.extend_from_slice(
                &u16::try_from(value)
                    .expect("bounded compact size")
                    .to_le_bytes(),
            );
        }
        65_536..=4_294_967_295 => {
            output.push(0xfe);
            output.extend_from_slice(
                &u32::try_from(value)
                    .expect("bounded compact size")
                    .to_le_bytes(),
            );
        }
        _ => {
            output.push(0xff);
            output.extend_from_slice(&u64::try_from(value).expect("usize fits u64").to_le_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        POX_5_SBTC_DEPOSIT_MAX_FEE_SATS, StacksAddress, sbtc_deposit_taproot_output_key,
        sbtc_pox5_deposit_taproot_output_key,
    };
    use nano_primitives::Hash160;

    #[test]
    fn stacks_address_round_trip() {
        let address = StacksAddress::new(22, Hash160::from_bytes([1; 20])).expect("valid address");
        assert_eq!(
            address
                .to_string()
                .parse::<StacksAddress>()
                .expect("parses"),
            address
        );
    }

    #[test]
    fn sbtc_taproot_matches_reference_vector() {
        let aggregate_public_key = [
            0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce, 0x87,
            0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81, 0x5b,
            0x16, 0xf8, 0x17, 0x98,
        ];
        let recipient = [
            0x05, 0x16, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let output =
            sbtc_deposit_taproot_output_key(&aggregate_public_key, &recipient, 15_000, 6, &[])
                .expect("valid fixture");
        assert_eq!(
            output,
            [
                0x3a, 0x90, 0x00, 0x85, 0xe4, 0x60, 0x37, 0x15, 0xfd, 0x25, 0xab, 0xda, 0x92, 0x99,
                0xa4, 0x98, 0x9a, 0x9f, 0x94, 0xa4, 0x90, 0xf0, 0xd6, 0xf1, 0x89, 0x2b, 0xc8, 0xa1,
                0xc6, 0xba, 0xbf, 0x1a,
            ]
        );
    }

    #[test]
    fn pox5_taproot_is_the_fixed_parameter_specialization() {
        let aggregate_public_key = [
            0x02, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce,
            0x87, 0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81,
            0x5b, 0x16, 0xf8, 0x17, 0x98,
        ];
        let recipient = [
            0x05, 0x16, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
            0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11,
        ];
        assert_eq!(
            sbtc_pox5_deposit_taproot_output_key(&aggregate_public_key, &recipient)
                .expect("valid key"),
            sbtc_deposit_taproot_output_key(
                &aggregate_public_key[1..].try_into().expect("x-only key"),
                &recipient,
                POX_5_SBTC_DEPOSIT_MAX_FEE_SATS,
                u16::MAX,
                &[0x6a],
            )
            .expect("valid key")
        );
    }
}
