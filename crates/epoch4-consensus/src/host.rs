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
    out: impl Write,
) -> Result<(), String> {
    let tip = read_stand(&mut lines)?;

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
    serve_over(chainstate, tip, registry, lines, out)
}

/// Serve decisions over an already opened and adopted chainstate.
///
/// The state-directory door above is one way in; a conformance harness opens a
/// captured checkpoint its own way and serves the identical protocol through
/// this one. The ready line and every decision line are the same either way.
pub fn serve_over(
    mut chainstate: ChainState,
    mut tip: NakamotoBlock,
    registry: Option<&str>,
    mut lines: impl BufRead,
    mut out: impl Write,
) -> Result<(), String> {
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

    loop {
        let line = match read_line(&mut lines)? {
            Line::Ready(line) => line,
            // The edge feeding this executor takes blocks from strangers, so an
            // oversized line is something a peer can cause. Answering it and
            // reading on keeps that from ending the one process allowed to write
            // chainstate; `read_line` has already left the stream on a boundary.
            Line::TooLong => {
                protocol_error(&mut out, "a protocol line exceeds the bounded maximum")?;
                continue;
            }
            Line::End => break,
        };
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
        // The parent's locally derived burn headers land before the decision:
        // Clarity may ask about any recent burn block, and the executor never
        // fetches — a reorganization's replacement hash supersedes by height.
        let mut seeded = true;
        for seed in &request.burn_headers {
            if let Err(error) = chainstate.record_burn_header(seed.height, seed.hash) {
                protocol_error(&mut out, &format!("burn header {}: {error}", seed.height))?;
                seeded = false;
                break;
            }
        }
        if !seeded {
            continue;
        }
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

/// The parent's opening line: the block the executor is asked to stand on.
pub fn read_stand(mut lines: impl BufRead) -> Result<NakamotoBlock, String> {
    let stand = match read_line(&mut lines)? {
        Line::Ready(line) => line,
        // Fatal here, unlike a decision line: standing is the handshake, and an
        // executor that never learned which block it stands on has nothing to
        // answer from.
        Line::TooLong => return Err("the stand line exceeds the bounded maximum".to_owned()),
        Line::End => return Err("the parent closed before standing".to_owned()),
    };
    let stand: serde_json::Value =
        serde_json::from_str(&stand).map_err(|error| format!("the stand line: {error}"))?;
    if stand["schema"] != STAND_SCHEMA {
        return Err(format!("unknown stand schema {}", stand["schema"]));
    }
    let bytes = hex::decode(stand["block"].as_str().unwrap_or_default())
        .map_err(|error| format!("the stand block: {error}"))?;
    NakamotoBlock::decode(&bytes)
        .map_err(|error| format!("the stand block does not decode: {error:?}"))
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

/// What one read of the protocol found.
enum Line {
    /// A complete line within the bound.
    Ready(String),
    /// A line past the bound. The rest of it has been read and discarded, so the
    /// next read starts on a line boundary and the caller can answer and go on.
    TooLong,
    /// A clean end of input, between lines.
    End,
}

/// One bounded line, refusing an oversized one without losing the stream.
fn read_line(input: &mut impl BufRead) -> Result<Line, String> {
    let mut buffer = Vec::new();
    let mut over = false;
    loop {
        let filled = input.fill_buf().map_err(|error| error.to_string())?;
        if filled.is_empty() {
            return if over {
                Ok(Line::TooLong)
            } else if buffer.is_empty() {
                Ok(Line::End)
            } else {
                Err("input ended inside a line".to_owned())
            };
        }
        // The bound is measured against the line, not against however much the
        // reader happened to hand over: checking only the no-newline branch made
        // it depend on the caller's buffer size, so a reader whose buffer holds
        // the whole line — a slice or a `Cursor`, which is how the conformance
        // harness drives this — enforced no bound at all.
        if let Some(position) = filled.iter().position(|byte| *byte == b'\n') {
            over = over || buffer.len() + position > MAX_LINE_BYTES;
            if !over {
                buffer.extend_from_slice(&filled[..position]);
            }
            input.consume(position + 1);
            break;
        }
        let take = filled.len();
        // Stop accumulating as soon as the line is known to be too long: it must
        // not become an oversized allocation either. Reading continues to the
        // newline so the stream resynchronizes.
        over = over || buffer.len() + take > MAX_LINE_BYTES;
        if over {
            buffer = Vec::new();
        } else {
            buffer.extend_from_slice(filled);
        }
        input.consume(take);
    }
    if over {
        return Ok(Line::TooLong);
    }
    String::from_utf8(buffer)
        .map(Line::Ready)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{Line, MAX_LINE_BYTES, read_line};

    fn read(input: &mut &[u8]) -> Line {
        read_line(input).expect("a readable line")
    }

    #[test]
    fn a_line_at_the_bound_is_answered_and_one_past_it_is_refused() {
        let at_bound = "a".repeat(MAX_LINE_BYTES);
        let bytes = format!("{at_bound}\n");
        let mut input = bytes.as_bytes();
        assert!(matches!(read(&mut input), Line::Ready(line) if line == at_bound));

        let bytes = format!("{}\n", "a".repeat(MAX_LINE_BYTES + 1));
        let mut input = bytes.as_bytes();
        assert!(matches!(read(&mut input), Line::TooLong));
    }

    /// The point of refusing rather than failing: the executor is the only
    /// process allowed to write chainstate, so a line a peer can inflate must
    /// not end it. That only holds if the next line still parses.
    #[test]
    fn the_line_after_an_oversized_one_is_still_read() {
        let bytes = format!("{}\n{{\"schema\":\"x\"}}\n", "a".repeat(MAX_LINE_BYTES + 1));
        let mut input = bytes.as_bytes();
        assert!(matches!(read(&mut input), Line::TooLong));
        assert!(matches!(read(&mut input), Line::Ready(line) if line == "{\"schema\":\"x\"}"));
        assert!(matches!(read(&mut input), Line::End));
    }

    #[test]
    fn an_oversized_line_without_its_newline_ends_as_a_refusal() {
        let bytes = "a".repeat(MAX_LINE_BYTES + 1);
        let mut input = bytes.as_bytes();
        assert!(matches!(read(&mut input), Line::TooLong));
    }

    #[test]
    fn a_truncated_line_is_an_error_and_a_clean_end_is_not() {
        let mut input = &b"partial"[..];
        assert!(read_line(&mut input).is_err());
        let mut input = &b""[..];
        assert!(matches!(read(&mut input), Line::End));
    }
}
