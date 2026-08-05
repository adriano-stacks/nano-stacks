//! Replay captured blocks into a state directory, until told to or killed.
//!
//! A separate process because that is the only way to test what a crash leaves
//! behind: an in-process test can drop a chainstate, which closes its stores
//! cleanly, and a clean close is exactly the case that was already covered.
//! `kill_during_replay` spawns this and sends it SIGKILL.
//!
//! It resumes from whatever the directory already holds — asking the fixtures how
//! many blocks have state rather than being told, because a process that is killed
//! cannot report where it got to.

use std::{path::Path, process::ExitCode};

use nano_conformance::{
    FixtureManifest, FixtureMode, captured_blocks_sealed, durable_replay_chainstate, replay_into,
};

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let [fixtures, directory, blocks] = arguments.as_slice() else {
        eprintln!("usage: replay-blocks <fixtures> <state-dir> <blocks>");
        return ExitCode::FAILURE;
    };
    let Ok(blocks) = blocks.parse::<u64>() else {
        eprintln!("the block count must be a number");
        return ExitCode::FAILURE;
    };
    let fixtures = Path::new(fixtures);
    let (mut chainstate, source) = match durable_replay_chainstate(fixtures, Path::new(directory)) {
        Ok(opened) => opened,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let sealed = captured_blocks_sealed(fixtures, &chainstate);
    let depth = replay_into(
        &mut chainstate,
        source,
        fixtures,
        FixtureManifest {
            mode: FixtureMode::Captured,
            replay_blocks: blocks,
            receipts: true,
        },
        sealed,
        // Printed per block so a parent can see the run is making progress, and
        // flushed because a killed process flushes nothing.
        &mut |block, _| {
            println!("sealed {}", block.header.chain_length);
        },
    );
    if depth.completed < blocks {
        eprintln!(
            "replayed {} of {blocks} from offset {sealed}: {:?}",
            depth.completed, depth.first_divergence
        );
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
