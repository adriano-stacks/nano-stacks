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
//!
//! Given a fourth argument it also writes each block's **write journal**: the
//! ordered `(key, serialized value)` sequence its execution made, every
//! transaction's Clarity writes and every native effect, with the writes of a
//! rolled-back Clarity transaction removed exactly as the MARF removes them. That
//! is the artifact a root divergence with matching receipts needs, and this is how
//! it is taken from a real mainnet block standing on a pristine parent, through
//! the same recorder `write_journal` drives offline against the captured fixture.

use std::{fmt::Write as _, fs, io::Write as _, path::Path, process::ExitCode};

use nano_conformance::{
    FixtureManifest, FixtureMode, captured_blocks_sealed, durable_replay_chainstate, replay_into,
};

/// Execution decodes a trie node and boxes its children for every cache miss,
/// and under glibc malloc that churn cost 14% of a 4,149-block mainnet replay.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let ([fixtures, directory, blocks], journal) = match arguments.as_slice() {
        [fixtures, directory, blocks] => ([fixtures, directory, blocks], None),
        [fixtures, directory, blocks, journal] => {
            ([fixtures, directory, blocks], Some(journal.clone()))
        }
        _ => {
            eprintln!("usage: replay-blocks <fixtures> <state-dir> <blocks> [journal-file]");
            return ExitCode::FAILURE;
        }
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
    if journal.is_some() {
        chainstate.vm_mut().record_writes();
    }
    let sealed = match captured_blocks_sealed(fixtures, &chainstate) {
        Ok(sealed) => sealed,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let depth = replay_into(
        &mut chainstate,
        source,
        fixtures,
        FixtureManifest {
            mode: FixtureMode::Captured,
            replay_blocks: blocks,
            // A capture taken from an archived chainstate has no event observer,
            // so its receipts are absent and the PoX unlock heights come from
            // its provenance instead. Detected rather than passed in: a run that
            // got this wrong would execute against different unlock heights, and
            // a journal recorded from it would be a journal of another chain.
            receipts: has_receipts(fixtures),
        },
        sealed,
        // Printed per block so a parent can see the run is making progress, and
        // flushed because a killed process flushes nothing.
        &mut |block, _| {
            println!("sealed {}", block.header.chain_length);
        },
    );
    if let Some(path) = journal
        && let Err(error) = write_journal(&mut chainstate, Path::new(&path))
    {
        eprintln!("the journal cannot be written: {error}");
        return ExitCode::FAILURE;
    }
    if depth.completed < blocks {
        eprintln!(
            "replayed {} of {blocks} from offset {sealed}: {:?}",
            depth.completed, depth.first_divergence
        );
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Whether the capture carries the event-observer receipts.
fn has_receipts(fixtures: &Path) -> bool {
    fs::read_dir(fixtures.join("events/new_block"))
        .is_ok_and(|mut entries| entries.next().is_some())
}

/// Write every recorded journal, one stanza per block, in the order recorded.
///
/// Written even when the replay diverged, because the block that diverged is the
/// one whose journal is wanted.
///
/// Plain text rather than JSON, because what it is for is being read beside the
/// network's own writes for the same block — but one line per write, with
/// newlines in a value escaped, so it can also be read back and driven through
/// another MARF (`write_journal::a_recorded_mainnet_journal_seals_the_chains_root`).
/// `marf` names the MARF's own height keys, which any implementation writes for
/// itself and a replay must not feed back; `write` names what execution wrote.
fn write_journal(chainstate: &mut nano_chainstate::ChainState, path: &Path) -> std::io::Result<()> {
    let mut rendered = String::new();
    for journal in chainstate.vm_mut().take_journal() {
        let _ = writeln!(
            rendered,
            "block {} height {} parent {} root {}",
            journal.sealed_as.map(hex::encode).unwrap_or_default(),
            journal.height,
            journal.parent.map(hex::encode).unwrap_or_default(),
            journal.root.map(hex::encode).unwrap_or_default()
        );
        for (kind, write) in journal
            .height_keys
            .iter()
            .map(|write| ("marf", write))
            .chain(journal.writes.iter().map(|write| ("write", write)))
        {
            let _ = writeln!(
                rendered,
                "  {kind} {} = {} {}",
                write.key,
                hex::encode(write.marf_value),
                write
                    .value
                    .as_deref()
                    .unwrap_or("<encoded>")
                    .replace('\\', "\\\\")
                    .replace('\n', "\\n")
            );
        }
    }
    fs::File::create(path)?.write_all(rendered.as_bytes())
}
