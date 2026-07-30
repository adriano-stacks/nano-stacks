#![forbid(unsafe_code)]

use std::fmt;

pub use primitive_types::{U256 as Uint256, U512 as Uint512};
use ripemd::{Digest as RipemdDigest, Ripemd160};
use sha2::{Sha256, Sha512, Sha512_256};

macro_rules! hash_type {
    ($name:ident, $length:expr) => {
        #[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name([u8; $length]);

        impl Default for $name {
            fn default() -> Self {
                Self([0; $length])
            }
        }

        impl $name {
            #[must_use]
            pub const fn from_bytes(bytes: [u8; $length]) -> Self {
                Self(bytes)
            }

            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; $length] {
                &self.0
            }
        }

        impl From<[u8; $length]> for $name {
            fn from(bytes: [u8; $length]) -> Self {
                Self::from_bytes(bytes)
            }
        }

        impl AsRef<[u8]> for $name {
            fn as_ref(&self) -> &[u8] {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, formatter)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                for byte in self.0 {
                    write!(formatter, "{byte:02x}")?;
                }
                Ok(())
            }
        }
    };
}

hash_type!(TrieHash, 32);
hash_type!(BlockHeaderHash, 32);
hash_type!(StacksBlockId, 32);
hash_type!(SortitionId, 32);
hash_type!(BitcoinHeaderHash, 32);
hash_type!(ConsensusHash, 20);
hash_type!(Hash160, 20);
hash_type!(Sha256Sum, 32);
hash_type!(Sha512Sum, 64);

impl TrieHash {
    pub const EMPTY: Self = Self([
        0xc6, 0x72, 0xb8, 0xd1, 0xef, 0x56, 0xed, 0x28, 0xab, 0x87, 0xc3, 0x62, 0x2c, 0x51, 0x14,
        0x06, 0x9b, 0xdd, 0x3a, 0xd7, 0xb8, 0xf9, 0x73, 0x74, 0x98, 0xd0, 0xc0, 0x1e, 0xce, 0xf0,
        0x96, 0x7a,
    ]);

    #[must_use]
    pub fn from_data(data: &[u8]) -> Self {
        if data.is_empty() {
            Self::EMPTY
        } else {
            Self::from(sha512_256(data).0)
        }
    }
}

#[must_use]
pub fn sha512_256(data: &[u8]) -> Sha256Sum {
    let digest: [u8; 32] = Sha512_256::digest(data).into();
    Sha256Sum::from(digest)
}

#[must_use]
pub fn sha256(data: &[u8]) -> Sha256Sum {
    let digest: [u8; 32] = Sha256::digest(data).into();
    Sha256Sum::from(digest)
}

#[must_use]
pub fn sha512(data: &[u8]) -> Sha512Sum {
    let digest: [u8; 64] = Sha512::digest(data).into();
    Sha512Sum::from(digest)
}

#[must_use]
pub fn hash160(data: &[u8]) -> Hash160 {
    let digest: [u8; 20] = Ripemd160::digest(Sha256::digest(data)).into();
    Hash160::from(digest)
}

/// The chain a node executes against.
///
/// Both fields are consensus-visible: the flag picks the boot address and the
/// version byte inside every serialized principal, and the identifier is the
/// `chain_id` a transaction signs over and `(chain-id)` reads
/// (`stacks-common/src/libcommon.rs`).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Network {
    mainnet: bool,
    chain_id: u32,
}

impl Network {
    pub const MAINNET: Self = Self {
        mainnet: true,
        chain_id: 0x0000_0001,
    };

    pub const TESTNET: Self = Self {
        mainnet: false,
        chain_id: 0x8000_0000,
    };

    /// A non-mainnet chain that is not the public testnet, such as Hacknet.
    #[must_use]
    pub const fn testnet_with_chain_id(chain_id: u32) -> Self {
        Self {
            mainnet: false,
            chain_id,
        }
    }

    /// Recover the network a peer reports as its `network_id`.
    ///
    /// Only the mainnet identifier means mainnet; every other chain is a
    /// testnet of some description.
    #[must_use]
    pub const fn from_chain_id(chain_id: u32) -> Self {
        Self {
            mainnet: chain_id == Self::MAINNET.chain_id,
            chain_id,
        }
    }

    #[must_use]
    pub const fn is_mainnet(self) -> bool {
        self.mainnet
    }

    #[must_use]
    pub const fn chain_id(self) -> u32 {
        self.chain_id
    }

    /// The address every boot contract is published under.
    ///
    /// `SP000000000000000000002Q6VF78` on mainnet and
    /// `ST000000000000000000002AMW42H` elsewhere (`clarity/src/vm/types/mod.rs`,
    /// `boot_util::boot_code_addr`).
    #[must_use]
    pub const fn boot_address(self) -> &'static str {
        if self.mainnet {
            "SP000000000000000000002Q6VF78"
        } else {
            "ST000000000000000000002AMW42H"
        }
    }

    /// Fully qualify a boot contract for this network.
    #[must_use]
    pub fn boot_contract_id(self, name: &str) -> String {
        format!("{}.{name}", self.boot_address())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BitVec<const MAX_SIZE: u16> {
    data: Vec<u8>,
    len: u16,
}

impl<const MAX_SIZE: u16> BitVec<MAX_SIZE> {
    pub fn zeros(len: u16) -> Result<Self, BitVecError> {
        Self::new(len, false)
    }
    pub fn ones(len: u16) -> Result<Self, BitVecError> {
        Self::new(len, true)
    }

    fn new(len: u16, value: bool) -> Result<Self, BitVecError> {
        if len == 0 {
            return Err(BitVecError::Empty);
        }
        if len > MAX_SIZE {
            return Err(BitVecError::TooLong { len, max: MAX_SIZE });
        }
        let mut result = Self {
            data: vec![0; Self::data_len(len)],
            len,
        };
        if value {
            for index in 0..len {
                result.set(index, true)?;
            }
        }
        Ok(result)
    }

    #[must_use]
    pub const fn len(&self) -> u16 {
        self.len
    }
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }
    #[must_use]
    pub fn get(&self, index: u16) -> Option<bool> {
        (index < self.len).then(|| self.data[usize::from(index / 8)] & (1 << (index % 8)) != 0)
    }

    pub fn set(&mut self, index: u16, value: bool) -> Result<(), BitVecError> {
        if index >= self.len {
            return Err(BitVecError::OutOfBounds {
                index,
                len: self.len,
            });
        }
        let byte = &mut self.data[usize::from(index / 8)];
        let mask = 1 << (index % 8);
        if value {
            *byte |= mask;
        } else {
            *byte &= !mask;
        }
        Ok(())
    }

    #[must_use]
    pub fn as_wire_bytes(&self) -> &[u8] {
        &self.data
    }
    #[must_use]
    pub fn wire_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(6 + self.data.len());
        bytes.extend_from_slice(&self.len.to_be_bytes());
        bytes.extend_from_slice(
            &u32::try_from(self.data.len())
                .expect("bit vector byte length always fits u32")
                .to_be_bytes(),
        );
        bytes.extend_from_slice(&self.data);
        bytes
    }

    pub fn from_wire_bytes(bytes: &[u8]) -> Result<Self, BitVecError> {
        let header = bytes.get(..6).ok_or(BitVecError::Truncated)?;
        let len = u16::from_be_bytes(header[..2].try_into().expect("fixed slice"));
        let declared_length = u32::from_be_bytes(header[2..].try_into().expect("fixed slice"));
        let data = &bytes[6..];
        if usize::try_from(declared_length).expect("u32 fits usize") != data.len() {
            return Err(BitVecError::InvalidWireLength);
        }
        if data.len() != Self::data_len(len) {
            return Err(BitVecError::InvalidWireLength);
        }
        let mut result = Self::zeros(len)?;
        result.data.copy_from_slice(data);
        Ok(result)
    }

    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = bool> + '_ {
        (0..self.len).map(|index| self.get(index).expect("bounded index"))
    }

    fn data_len(len: u16) -> usize {
        usize::from(len).div_ceil(8)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BitVecError {
    Empty,
    TooLong { len: u16, max: u16 },
    OutOfBounds { index: u16, len: u16 },
    Truncated,
    InvalidWireLength,
}

impl fmt::Display for BitVecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("bit vector length must be positive"),
            Self::TooLong { len, max } => {
                write!(formatter, "bit vector length {len} exceeds {max}")
            }
            Self::OutOfBounds { index, len } => {
                write!(formatter, "bit {index} is outside length {len}")
            }
            Self::Truncated => formatter.write_str("bit vector is missing its length"),
            Self::InvalidWireLength => formatter.write_str("bit vector has an invalid wire length"),
        }
    }
}

impl std::error::Error for BitVecError {}

#[cfg(test)]
mod tests {
    use super::{BitVec, BitVecError, TrieHash, Uint256, hash160, sha512_256};
    use proptest::prelude::*;

    #[test]
    fn reference_hash_vectors() {
        assert_eq!(TrieHash::from_data(b""), TrieHash::EMPTY);
        assert_eq!(
            sha512_256(b"hello").to_string(),
            "e30d87cfa2a75db545eac4d61baf970366a8357c7f72fa95b52d0accb698f13a"
        );
        assert_eq!(
            hash160(b"hello").to_string(),
            "b6a9c8c230722b7c748331a8b450f05566dc7d0f"
        );
    }

    #[test]
    fn uint256_arithmetic_and_encoding() {
        let value = uint_from_words([u64::MAX, 0, 0, 0]);
        assert_eq!(
            value.checked_add(Uint256::one()),
            Some(uint_from_words([0, 1, 0, 0]))
        );
        assert_eq!(Uint256::zero().checked_sub(Uint256::one()), None);
        let product = Uint256::from(u64::MAX)
            .checked_mul(Uint256::from(2_u8))
            .expect("fits");
        assert_eq!(product, uint_from_words([u64::MAX - 1, 1, 0, 0]));
        assert_eq!(product / Uint256::from(2_u8), Uint256::from(u64::MAX));
        assert_eq!(uint_from_be_bytes(uint_to_be_bytes(product)), product);
    }

    #[test]
    fn bitvec_matches_reference_layout() {
        let mut bits = BitVec::<16>::zeros(10).expect("valid length");
        bits.set(0, true).expect("in bounds");
        bits.set(8, true).expect("in bounds");
        assert_eq!(bits.as_wire_bytes(), &[1, 1]);
        assert_eq!(bits.wire_bytes(), &[0, 10, 0, 0, 0, 2, 1, 1]);
        assert_eq!(BitVec::<16>::from_wire_bytes(&bits.wire_bytes()), Ok(bits));
        assert_eq!(BitVec::<16>::zeros(0), Err(BitVecError::Empty));
    }

    proptest! {
        #[test]
        fn low_word_arithmetic_matches_u128(left in any::<u64>(), right in any::<u64>()) {
            let left_256 = Uint256::from(left); let right_256 = Uint256::from(right);
            let sum = u128::from(left) + u128::from(right);
            let words = [u64::try_from(sum & u128::from(u64::MAX)).expect("masked"), u64::try_from(sum >> 64).expect("word"), 0, 0];
            prop_assert_eq!(left_256.checked_add(right_256), Some(uint_from_words(words)));
            let product = u128::from(left) * u128::from(right); let mut bytes = [0; 32]; bytes[16..].copy_from_slice(&product.to_be_bytes());
            prop_assert_eq!(left_256.checked_mul(right_256), Some(uint_from_be_bytes(bytes)));
            if let Some(quotient) = left.checked_div(right) { prop_assert_eq!(left_256 / right_256, Uint256::from(quotient)); prop_assert_eq!(left_256 % right_256, Uint256::from(left % right)); }
        }
    }

    fn uint_from_words(words: [u64; 4]) -> Uint256 {
        let mut bytes = [0; 32];
        for (index, word) in words.iter().enumerate() {
            bytes[index * 8..(index + 1) * 8].copy_from_slice(&word.to_le_bytes());
        }
        Uint256::from_little_endian(&bytes)
    }
    fn uint_from_be_bytes(bytes: [u8; 32]) -> Uint256 {
        Uint256::from_big_endian(&bytes)
    }
    fn uint_to_be_bytes(value: Uint256) -> [u8; 32] {
        value.to_big_endian()
    }
}
