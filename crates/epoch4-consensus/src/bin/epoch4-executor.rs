//! The supervised Epoch 4.0 executor: the sole chainstate writer.
//!
//! One process, one state directory, no listener and no client capability.
//! The protocol is line-delimited JSON on stdin/stdout, versioned by schema
//! strings and bounded by a maximum line length: the parent first says which
//! block the executor stands on (the tip's bytes, which the state directory
//! itself authenticates by holding its sealed trie and ledger), the executor
//! answers ready, and every following line is one decision request answered
//! by one canonical decision record. A malformed or oversized line is a typed
//! refusal, never a crash; an unusable state directory is a refusal to start.
//!
//! Everything nondeterministic stays with the parent. This process reads no
//! clock for any decision, no environment beyond its arguments, no file
//! outside the state directory, and no socket at all.

use std::io::{BufRead, Write};
use std::process::ExitCode;

use epoch4_consensus::{DecisionRequest, judge};
use nano_chainstate::{ChainState, NakamotoBlock};
use nano_primitives::Network;

/// A decision request carries at most one block (2 MiB consensus limit,
/// hexadecimal on the wire) beside its context and operations.
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

const STAND_SCHEMA: &str = "nano-stacks/epoch4-executor-stand/v1";
const READY_SCHEMA: &str = "nano-stacks/epoch4-executor-ready/v1";

/// Execution decodes trie nodes and boxes children on every cache miss; the
/// same measured choice every other execution binary in this tree makes.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let (directory, network, registry) = match arguments.as_slice() {
        [directory, network] => (directory, network, None),
        [directory, network, registry] => (directory, network, Some(registry.clone())),
        _ => {
            eprintln!(
                "usage: epoch4-executor <state-dir> <mainnet|testnet|chain-id:N> [waterfall-registry]"
            );
            return ExitCode::FAILURE;
        }
    };
    let network = match parse_network(network) {
        Ok(network) => network,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    match run(
        directory,
        network,
        registry.as_deref(),
        std::io::stdin().lock(),
        std::io::stdout().lock(),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("epoch4-executor: {error}");
            ExitCode::FAILURE
        }
    }
}

fn parse_network(text: &str) -> Result<Network, String> {
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

fn run(
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
                writeln!(
                    out,
                    "{}",
                    serde_json::json!({"schema": "nano-stacks/epoch4-protocol-error/v1",
                                       "error": error.to_string()})
                )
                .map_err(|error| error.to_string())?;
                out.flush().map_err(|error| error.to_string())?;
                continue;
            }
        };
        let opened = match request.open() {
            Ok(opened) => opened,
            Err(error) => {
                writeln!(
                    out,
                    "{}",
                    serde_json::json!({"schema": "nano-stacks/epoch4-protocol-error/v1",
                                       "error": error})
                )
                .map_err(|error| error.to_string())?;
                out.flush().map_err(|error| error.to_string())?;
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
