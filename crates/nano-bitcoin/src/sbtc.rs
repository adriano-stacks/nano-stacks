//! sBTC deposit taproot output derivation.
//!
//! The address a PoX-5 waterfall commitment pays is not an address anyone
//! chose: it is the taproot output key of the sBTC deposit script for the
//! current aggregate key, so deriving it is a Bitcoin script rule rather than
//! an address encoding (`stackslib/src/chainstate/stacks/sbtc.rs`).

use nano_address::AddressError;
use sha2::{Digest, Sha256};

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
        POX_5_SBTC_DEPOSIT_MAX_FEE_SATS, sbtc_deposit_taproot_output_key,
        sbtc_pox5_deposit_taproot_output_key,
    };

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
