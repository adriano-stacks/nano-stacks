//! The executor process's serving loop, as a library.
//!
//! The `epoch4-executor` binary is a thin shell over [`serve`]; keeping the
//! loop here lets the conformance harness host the identical protocol from a
//! test binary of its own and drive it in and out of process. Nothing in this
//! module reads a clock, the environment, or any file outside the state
//! directory it is handed.

use std::io::{BufRead, Write};

use nano_chainstate::{ChainState, NakamotoBlock};
use nano_primitives::Network;

use crate::{DecisionRequest, judge};

/// A decision request carries at most one block (2 MiB consensus limit,
/// hexadecimal on the wire) beside its context and operations.
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

pub const STAND_SCHEMA: &str = "nano-stacks/epoch4-executor-stand/v1";
pub const READY_SCHEMA: &str = "nano-stacks/epoch4-executor-ready/v1";
pub const PROTOCOL_ERROR_SCHEMA: &str = "nano-stacks/epoch4-protocol-error/v1";

/// The network named on an executor's command line.
pub fn parse_network(text: &str) -> Result<Network, String> {
    match text {
        "mainnet" => Ok(Network::MAINNET),
        "testnet" => Ok(Network::TESTNET),
        other => other
            .strip_prefix("chain-id:")
            .and_then(|id| id.parse::<u32>().ok())
            .map(Network::from_chain_id)
            .ok_or_else(|| format!("unknown network {other}")),
    }
}

/// Serve decisions over a bounded line protocol until the input closes.
///
/// The first line must be a stand (schema [`STAND_SCHEMA`]) carrying the tip
/// block's consensus bytes; it is refused unless it is exactly the state
/// directory's durable tip with a committed ledger. Every following line is a
/// [`DecisionRequest`] answered by one canonical decision record; a malformed
/// or oversized line is a typed protocol error, never a crash.
pub fn serve(
    directory: &str,
    network: Network,
    registry: Option<&str>,
    mut lines: impl BufRead,
    mut out: impl Write,
) -> Result<(), String> {
    let stand = read_line(&mut lines)?.ok_or("the parent closed before standing")?;
    let stand: serde_json::Value =
        serde_json::from_str(&stand).map_err(|error| format!("the stand line: {error}"))?;
    if stand["schema"] != STAND_SCHEMA {
        return Err(format!("unknown stand schema {}", stand["schema"]));
    }
    let bytes = hex::decode(stand["block"].as_str().unwrap_or_default())
        .map_err(|error| format!("the stand block: {error}"))?;
    let mut tip = NakamotoBlock::decode(&bytes)
        .map_err(|error| format!("the stand block does not decode: {error:?}"))?;

    let mut chainstate = ChainState::open(network, directory).map_err(|error| error.to_string())?;
    let durable = chainstate
        .tip()
        .map_err(|error| error.to_string())?
        .ok_or("the state directory holds no sealed block")?;
    if durable != *tip.block_id().as_bytes() {
        return Err(format!(
            "the parent stands on {} but this state's durable tip is {}",
            tip.block_id(),
            hex::encode(durable)
        ));
    }
    if !chainstate
        .recover_ledger_at(durable)
        .map_err(|error| error.to_string())?
    {
        return Err(format!(
            "block {} has no committed ledger, so this executor cannot authenticate",
            tip.block_id()
        ));
    }

    writeln!(
        out,
        "{}",
        serde_json::json!({
            "schema": READY_SCHEMA,
            "tip": tip.block_id().to_string(),
            "height": tip.header.chain_length,
        })
    )
    .map_err(|error| error.to_string())?;
    out.flush().map_err(|error| error.to_string())?;

    while let Some(line) = read_line(&mut lines)? {
        let request: DecisionRequest = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                protocol_error(&mut out, &error.to_string())?;
                continue;
            }
        };
        let opened = match request.open() {
            Ok(opened) => opened,
            Err(error) => {
                protocol_error(&mut out, &error)?;
                continue;
            }
        };
        let decision = judge(&mut chainstate, &opened, &tip, registry);
        if decision.applied.is_some() {
            tip = opened.block;
        }
        writeln!(
            out,
            "{}",
            serde_json::to_string(&decision.record).map_err(|error| error.to_string())?
        )
        .map_err(|error| error.to_string())?;
        out.flush().map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn protocol_error(out: &mut impl Write, error: &str) -> Result<(), String> {
    writeln!(
        out,
        "{}",
        serde_json::json!({"schema": PROTOCOL_ERROR_SCHEMA, "error": error})
    )
    .map_err(|error| error.to_string())?;
    out.flush().map_err(|error| error.to_string())
}

/// One bounded line; `None` at a clean end of input.
fn read_line(input: &mut impl BufRead) -> Result<Option<String>, String> {
    let mut buffer = Vec::new();
    loop {
        let filled = input.fill_buf().map_err(|error| error.to_string())?;
        if filled.is_empty() {
            return if buffer.is_empty() {
                Ok(None)
            } else {
                Err("input ended inside a line".to_owned())
            };
        }
        if let Some(position) = filled.iter().position(|byte| *byte == b'\n') {
            buffer.extend_from_slice(&filled[..position]);
            input.consume(position + 1);
            break;
        }
        let take = filled.len();
        buffer.extend_from_slice(filled);
        input.consume(take);
        if buffer.len() > MAX_LINE_BYTES {
            return Err("a protocol line exceeds the bounded maximum".to_owned());
        }
    }
    String::from_utf8(buffer)
        .map(Some)
        .map_err(|error| error.to_string())
}
