#![forbid(unsafe_code)]

use std::{fmt, str::FromStr};

use bitcoin::{Address as BitcoinAddress, Network, ScriptBuf};
use nano_primitives::Hash160;

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

    /// The standard single-signature address for a compressed secp256k1 key hash.
    #[must_use]
    pub const fn single_signature(hash160: Hash160, mainnet: bool) -> Self {
        Self {
            version: if mainnet { 22 } else { 26 },
            hash160,
        }
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
    pub const fn as_stacks_address(&self) -> Option<StacksAddress> {
        match self {
            Self::Standard { address, .. } => Some(*address),
            Self::Addr20 { .. } | Self::Addr32 { .. } => None,
        }
    }

    pub fn from_script_pubkey(script: &[u8], mainnet: bool) -> Result<Self, AddressError> {
        let standard = |version, hash_mode, bytes| {
            StacksAddress::new(version, Hash160::from_bytes(bytes)).map(|address| Self::Standard {
                address,
                hash_mode: Some(hash_mode),
            })
        };

        match script {
            [0x76, 0xa9, 0x14, bytes @ .., 0x88, 0xac] if bytes.len() == 20 => standard(
                if mainnet { 22 } else { 26 },
                AddressHashMode::P2pkh,
                bytes.try_into().expect("guarded script length"),
            ),
            [0xa9, 0x14, bytes @ .., 0x87] if bytes.len() == 20 => standard(
                if mainnet { 20 } else { 21 },
                AddressHashMode::P2sh,
                bytes.try_into().expect("guarded script length"),
            ),
            [0x00, 0x14, bytes @ ..] if bytes.len() == 20 => Ok(Self::Addr20 {
                mainnet,
                address_type: PoxAddressType20::P2wpkh,
                bytes: bytes.try_into().expect("guarded script length"),
            }),
            [0x00, 0x20, bytes @ ..] if bytes.len() == 32 => Ok(Self::Addr32 {
                mainnet,
                address_type: PoxAddressType32::P2wsh,
                bytes: bytes.try_into().expect("guarded script length"),
            }),
            [0x51, 0x20, bytes @ ..] if bytes.len() == 32 => Ok(Self::Addr32 {
                mainnet,
                address_type: PoxAddressType32::P2tr,
                bytes: bytes.try_into().expect("guarded script length"),
            }),
            _ => Err(AddressError::InvalidBitcoinAddress),
        }
    }

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

#[cfg(test)]
mod tests {
    use super::StacksAddress;
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

}
