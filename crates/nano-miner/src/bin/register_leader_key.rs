#![forbid(unsafe_code)]

use std::{error::Error, fs, io, path::PathBuf};

use bitcoincore_rpc::Auth;
use clap::Parser;
use nano_bitcoin::LeaderKeyRegistration;
use nano_crypto::{StacksPrivateKey, VrfPrivateKey};
use nano_miner::BitcoinWallet;
use nano_primitives::hash160;

#[derive(Parser)]
#[command(name = "stacks-register-leader-key")]
/// Register a miner's VRF and block-signing keys on Bitcoin.
struct Cli {
    /// Bitcoin Core RPC endpoint.
    #[arg(long)]
    bitcoin_rpc: String,
    /// Bitcoin Core RPC username.
    #[arg(long)]
    bitcoin_rpc_user: String,
    /// File containing the Bitcoin Core RPC password.
    #[arg(long)]
    bitcoin_rpc_password_file: PathBuf,
    /// Hex-encoded 20-byte consensus hash.
    #[arg(long)]
    consensus_hash: String,
    /// File containing the hex-encoded 32-byte VRF private key.
    #[arg(long)]
    vrf_private_key_file: PathBuf,
    /// File containing the hex-encoded 32-byte block-signing private key.
    #[arg(long)]
    block_signing_private_key_file: PathBuf,
    /// Optional hexadecimal memo, up to five bytes.
    #[arg(long)]
    memo: Option<String>,
    /// Optional Bitcoin fee rate in satoshis per vbyte.
    #[arg(long)]
    fee_rate_sats_per_vbyte: Option<u64>,
    /// Two hexadecimal magic bytes for the target network.
    #[arg(long, default_value = "5433")]
    magic: String,
}

fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let vrf_private_key = VrfPrivateKey::from_bytes(read_hex_array(&cli.vrf_private_key_file)?);
    let block_signing_key =
        StacksPrivateKey::from_bytes(read_hex_array(&cli.block_signing_private_key_file)?)?;
    let registration = LeaderKeyRegistration {
        consensus_hash: parse_hex_array(&cli.consensus_hash)?,
        vrf_public_key: vrf_private_key.public_key().to_bytes(),
        block_signing_key_hash: *hash160(&block_signing_key.public_key().to_bytes_compressed())
            .as_bytes(),
        memo: cli.memo.map_or(Ok(Vec::new()), |memo| decode_hex(&memo))?,
    };
    let password = fs::read_to_string(cli.bitcoin_rpc_password_file)?;
    let wallet = BitcoinWallet::connect(
        &cli.bitcoin_rpc,
        Auth::UserPass(cli.bitcoin_rpc_user, password.trim_end().to_owned()),
    )?;
    let submitted = wallet.submit_leader_key_registration(
        parse_hex_array(&cli.magic)?,
        &registration,
        cli.fee_rate_sats_per_vbyte,
    )?;
    println!(
        "submitted leader-key registration {} with change output {}",
        submitted.transaction_id, submitted.change_output
    );
    Ok(())
}

fn read_hex_array<const N: usize>(path: &PathBuf) -> Result<[u8; N], io::Error> {
    parse_hex_array(&fs::read_to_string(path)?)
}

fn parse_hex_array<const N: usize>(value: &str) -> Result<[u8; N], io::Error> {
    let bytes = decode_hex(value)?;
    let length = bytes.len();
    bytes.try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("expected {N} bytes, found {length}"),
        )
    })
}

fn decode_hex(value: &str) -> Result<Vec<u8>, io::Error> {
    hex::decode(value.trim().trim_start_matches("0x")).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid hexadecimal value: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{decode_hex, parse_hex_array};

    #[test]
    fn parses_fixed_size_hex_values() {
        assert_eq!(
            parse_hex_array::<2>("0x1234").expect("two bytes"),
            [0x12, 0x34]
        );
        assert!(parse_hex_array::<2>("123").is_err());
    }

    #[test]
    fn trims_hex_file_contents() {
        assert_eq!(decode_hex(" 0x1234\n").expect("hex"), [0x12, 0x34]);
    }
}
