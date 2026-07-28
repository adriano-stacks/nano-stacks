#![forbid(unsafe_code)]

//! Bitcoin wallet integration for nano-stacks mining.

use std::fmt;

use bitcoin::{Amount, Transaction, Txid, consensus::encode::serialize_hex};
use bitcoincore_rpc::{Auth, Client, RpcApi, json};
use nano_address::PoxAddress;
use nano_bitcoin::{
    LeaderBlockCommitment, LeaderCommitmentTransactionError, build_leader_commitment_transaction,
};
use serde::Deserialize;
use serde_json::{Value, json as json_value};

#[derive(Debug)]
pub enum MinerError {
    BitcoinRpc(bitcoincore_rpc::Error),
    Commitment(LeaderCommitmentTransactionError),
    TransactionDecode(bitcoin::consensus::encode::Error),
    MissingInputs,
    AlteredProtocolOutputs,
    UnexpectedChangePosition(i32),
    IncompleteSignature,
    MempoolRejected(String),
}

impl fmt::Display for MinerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BitcoinRpc(error) => error.fmt(formatter),
            Self::Commitment(error) => error.fmt(formatter),
            Self::TransactionDecode(error) => error.fmt(formatter),
            Self::MissingInputs => {
                formatter.write_str("Bitcoin wallet did not fund the transaction")
            }
            Self::AlteredProtocolOutputs => {
                formatter.write_str("Bitcoin wallet altered leader commitment outputs")
            }
            Self::UnexpectedChangePosition(position) => {
                write!(
                    formatter,
                    "Bitcoin wallet placed change at output {position}"
                )
            }
            Self::IncompleteSignature => {
                formatter.write_str("Bitcoin wallet could not fully sign the transaction")
            }
            Self::MempoolRejected(reason) => {
                write!(formatter, "Bitcoin mempool rejected transaction: {reason}")
            }
        }
    }
}

impl std::error::Error for MinerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BitcoinRpc(error) => Some(error),
            Self::Commitment(error) => Some(error),
            Self::TransactionDecode(error) => Some(error),
            Self::MissingInputs
            | Self::AlteredProtocolOutputs
            | Self::UnexpectedChangePosition(_)
            | Self::IncompleteSignature
            | Self::MempoolRejected(_) => None,
        }
    }
}

impl From<bitcoincore_rpc::Error> for MinerError {
    fn from(error: bitcoincore_rpc::Error) -> Self {
        Self::BitcoinRpc(error)
    }
}

impl From<LeaderCommitmentTransactionError> for MinerError {
    fn from(error: LeaderCommitmentTransactionError) -> Self {
        Self::Commitment(error)
    }
}

impl From<bitcoin::consensus::encode::Error> for MinerError {
    fn from(error: bitcoin::consensus::encode::Error) -> Self {
        Self::TransactionDecode(error)
    }
}

/// A leader commitment accepted by the local Bitcoin wallet and mempool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmittedCommitment {
    pub transaction_id: Txid,
    pub transaction: Transaction,
    pub fee: Amount,
    pub change_output: Option<usize>,
}

/// The replacement transaction created by Bitcoin Core's `bumpfee` RPC.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplacedCommitment {
    pub transaction_id: Txid,
}

/// A wallet-scoped Bitcoin Core RPC client.
pub struct BitcoinWallet {
    rpc: Client,
}

impl BitcoinWallet {
    pub fn connect(url: &str, auth: Auth) -> Result<Self, MinerError> {
        Ok(Self {
            rpc: Client::new(url, auth)?,
        })
    }

    #[must_use]
    pub const fn from_rpc(rpc: Client) -> Self {
        Self { rpc }
    }

    /// Fund, sign, verify, and broadcast a replaceable waterfall commitment.
    pub fn submit_leader_commitment(
        &self,
        magic: [u8; 2],
        commitment: LeaderBlockCommitment,
        sbtc_address: &PoxAddress,
        commitment_amount: Amount,
        fee_rate_sats_per_vbyte: Option<u64>,
    ) -> Result<SubmittedCommitment, MinerError> {
        let template = build_leader_commitment_transaction(
            magic,
            commitment,
            Vec::new(),
            sbtc_address,
            commitment_amount,
            None,
        )?;
        let funded: json::FundRawTransactionResult = self.rpc.call(
            "fundrawtransaction",
            &[
                Value::from(serialize_hex(&template)),
                funding_options(fee_rate_sats_per_vbyte),
            ],
        )?;
        let funded_transaction = funded.transaction()?;
        let change_output =
            validate_funded_transaction(&template, &funded_transaction, funded.change_position)?;
        let signed = self
            .rpc
            .sign_raw_transaction_with_wallet(&funded_transaction, None, None)?;
        if !signed.complete {
            return Err(MinerError::IncompleteSignature);
        }
        let signed_transaction = signed.transaction()?;
        let acceptance = self.rpc.test_mempool_accept(&[&signed_transaction])?;
        let Some(result) = acceptance.first() else {
            return Err(MinerError::MempoolRejected(
                "Bitcoin Core returned no acceptance result".to_owned(),
            ));
        };
        if !result.allowed {
            return Err(MinerError::MempoolRejected(
                result
                    .reject_reason
                    .clone()
                    .unwrap_or_else(|| "unknown rejection".to_owned()),
            ));
        }
        let transaction_id = self.rpc.send_raw_transaction(&signed_transaction)?;

        Ok(SubmittedCommitment {
            transaction_id,
            transaction: signed_transaction,
            fee: funded.fee,
            change_output,
        })
    }

    /// Replace an unconfirmed commitment at a higher fee rate.
    pub fn bump_commitment_fee(
        &self,
        transaction_id: Txid,
        fee_rate_sats_per_vbyte: u64,
    ) -> Result<ReplacedCommitment, MinerError> {
        let response: BumpFeeResponse = self.rpc.call(
            "bumpfee",
            &[
                Value::from(transaction_id.to_string()),
                json_value!({
                    "fee_rate": fee_rate_sats_per_vbyte,
                    "replaceable": true,
                }),
            ],
        )?;
        Ok(ReplacedCommitment {
            transaction_id: response.transaction_id,
        })
    }
}

fn funding_options(fee_rate_sats_per_vbyte: Option<u64>) -> Value {
    let mut options = json_value!({
        "changePosition": 2,
        "replaceable": true,
    });
    if let Some(fee_rate) = fee_rate_sats_per_vbyte {
        options["fee_rate"] = Value::from(fee_rate);
    }
    options
}

fn validate_funded_transaction(
    template: &Transaction,
    funded: &Transaction,
    change_position: i32,
) -> Result<Option<usize>, MinerError> {
    if funded.input.is_empty() {
        return Err(MinerError::MissingInputs);
    }
    if funded.output.get(..2) != template.output.get(..2) {
        return Err(MinerError::AlteredProtocolOutputs);
    }
    match change_position {
        -1 => Ok(None),
        2 if funded.output.len() == 3 => Ok(Some(2)),
        position => Err(MinerError::UnexpectedChangePosition(position)),
    }
}

#[derive(Deserialize)]
struct BumpFeeResponse {
    #[serde(rename = "txid")]
    transaction_id: Txid,
}

#[cfg(test)]
mod tests {
    use bitcoin::{Amount, OutPoint, TxIn};

    use super::{MinerError, funding_options, validate_funded_transaction};

    #[test]
    fn funding_requests_rbf_with_change_after_protocol_outputs() {
        let options = funding_options(Some(7));
        assert_eq!(options["changePosition"], 2);
        assert_eq!(options["replaceable"], true);
        assert_eq!(options["fee_rate"], 7);
    }

    #[test]
    fn funding_validation_requires_wallet_to_preserve_output_order() {
        let template = bitcoin::Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: Vec::new(),
            output: vec![
                bitcoin::TxOut {
                    value: Amount::ZERO,
                    script_pubkey: bitcoin::ScriptBuf::new(),
                },
                bitcoin::TxOut {
                    value: Amount::from_sat(1),
                    script_pubkey: bitcoin::ScriptBuf::new(),
                },
            ],
        };
        let mut funded = template.clone();
        funded.input.push(TxIn {
            previous_output: OutPoint::default(),
            ..TxIn::default()
        });
        funded.output.push(bitcoin::TxOut {
            value: Amount::from_sat(2),
            script_pubkey: bitcoin::ScriptBuf::new(),
        });
        assert!(matches!(
            validate_funded_transaction(&template, &funded, 2),
            Ok(Some(2))
        ));
        funded.output.swap(1, 2);
        assert!(matches!(
            validate_funded_transaction(&template, &funded, 2),
            Err(MinerError::AlteredProtocolOutputs)
        ));
    }
}
