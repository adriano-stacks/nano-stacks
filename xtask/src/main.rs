use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    time::{SystemTime, UNIX_EPOCH},
};

use nano_conformance::{FixtureManifest, FixtureStatus, scoreboard_at, validate_fixture_tree};
use nano_chainstate::{NakamotoBlock, Signer, SignerSet};
use nano_primitives::Network;
use serde_json::json;

fn main() -> ExitCode {
    let command = env::args().nth(1);
    match command.as_deref() {
        Some("scoreboard") => print_scoreboard(),
        Some("validate-fixtures") => validate_fixtures(),
        Some("capture-fixtures") => capture_fixtures(&env::args().skip(2).collect::<Vec<_>>()),
        Some("public-key") => print_public_key(env::args().nth(2).as_deref()),
        Some("verify-block") => {
            verify_block(&env::args().skip(2).collect::<Vec<_>>())
        }
        Some("decode-blocks") => decode_blocks(env::args().nth(2).as_deref()),
        Some("check-module") => check_module(&env::args().skip(2).collect::<Vec<_>>()),
        Some("probe-root") => probe_root(&env::args().skip(2).collect::<Vec<_>>()),
        Some("call-both") => call_both(&env::args().skip(2).collect::<Vec<_>>()),
        Some("call-both-tx") => call_both_tx(&env::args().skip(2).collect::<Vec<_>>()),
        Some("heal-contracts") => heal_contracts(env::args().nth(2).as_deref()),
        Some("probe-header") => probe_header(&env::args().skip(2).collect::<Vec<_>>()),
        Some("eval") => eval_in_state(&env::args().skip(2).collect::<Vec<_>>()),
        Some("state-value") => state_value(&env::args().skip(2).collect::<Vec<_>>()),
        Some("snapshot-state") => snapshot_state(&env::args().skip(2).collect::<Vec<_>>()),
        Some("backfill-header") => backfill_header(&env::args().skip(2).collect::<Vec<_>>()),
        Some("repair-ledger") => repair_ledger(&env::args().skip(2).collect::<Vec<_>>()),
        Some("export-headers") => export_headers(&env::args().skip(2).collect::<Vec<_>>()),
        Some("import-headers") => import_headers(&env::args().skip(2).collect::<Vec<_>>()),
        Some("export-leader-keys") => {
            export_leader_keys(&env::args().skip(2).collect::<Vec<_>>())
        }
        Some("block-info") => block_info(&env::args().skip(2).collect::<Vec<_>>()),
        Some("rebuild-accounting") => {
            rebuild_accounting(&env::args().skip(2).collect::<Vec<_>>())
        }
        _ => {
            eprintln!(
                "usage: cargo xtask <scoreboard|validate-fixtures|capture-fixtures|public-key|verify-block|decode-blocks|check-module|rebuild-accounting|repair-ledger|export-headers|import-headers|export-leader-keys|block-info|probe-root|call-both|call-both-tx|state-value|snapshot-state|heal-contracts>"
            );
            ExitCode::from(2)
        }
    }
}

/// Diff a rebuilt header against the one this node recorded, field by field.
///
/// The oracle this whole approach needs, and it earns its keep: run against a
/// block whose header nano *does* hold, it caught three fields being plausible
/// rather than right. Only the fields the rebuild claims are compared — a field
/// it does not claim has no value to compare.
fn compare_headers(rebuilt: &nano_vm::BlockHeader, recorded: &nano_vm::BlockHeader) -> ExitCode {
        let mut wrong = 0;
        let mut check = |field: &str, same: bool, detail: String| {
            if !same {
                wrong += 1;
            }
            println!("  {field:20} {} {detail}", if same { "==" } else { "!=" });
        };
        check(
            "burn_header_hash",
            rebuilt.burn_header_hash == recorded.burn_header_hash,
            String::new(),
        );
        check(
            "burn_block_height",
            rebuilt.burn_block_height == recorded.burn_block_height,
            format!("{} vs {}", rebuilt.burn_block_height, recorded.burn_block_height),
        );
        check(
            "burn_block_time",
            rebuilt.burn_block_time == recorded.burn_block_time,
            format!("{} vs {}", rebuilt.burn_block_time, recorded.burn_block_time),
        );
        check(
            "stacks_block_time",
            rebuilt.stacks_block_time == recorded.stacks_block_time,
            format!("{} vs {}", rebuilt.stacks_block_time, recorded.stacks_block_time),
        );
        check(
            "block_header_hash",
            rebuilt.block_header_hash == recorded.block_header_hash,
            String::new(),
        );
        check(
            "consensus_hash",
            rebuilt.consensus_hash == recorded.consensus_hash,
            String::new(),
        );
        check("vrf_seed", rebuilt.vrf_seed == recorded.vrf_seed, String::new());
        check(
            "burn_spend_total",
            rebuilt.burn_spend_total == recorded.burn_spend_total,
            format!("{} vs {}", rebuilt.burn_spend_total, recorded.burn_spend_total),
        );
        println!(
            "{wrong} of the 8 compared fields disagree with the recorded header \
             (nothing was written: this block already has one)"
        );
    if wrong == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Evaluate a Clarity expression against a state directory's tip.
///
/// The fastest question to ask a divergence: what does this node actually
/// answer, right where it stopped. Read-only and rolled back.
///
/// Answers that come from the *state* are real — `tenure-height`,
/// `stacks-block-time`, every `get-stacks-block-info?`, any contract call. The
/// burn context is not: this opens a block with no Bitcoin context, so
/// `burn-block-height` reads 0 here and means nothing.
fn eval_in_state(arguments: &[String]) -> ExitCode {
    let [state, source] = arguments else {
        eprintln!(
            "usage: cargo xtask eval <state-dir> <clarity-expression>\n\
             the node must not be running: it holds the state open"
        );
        return ExitCode::FAILURE;
    };
    let mut store = match nano_vm::MarfStore::open(Network::MAINNET, Path::new(state).join("chainstate")) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("cannot open the state: {error:?}");
            return ExitCode::FAILURE;
        }
    };
    let Some(tip) = store.tip() else {
        eprintln!("the state is sealed at no block");
        return ExitCode::FAILURE;
    };
    if let Err(error) = store.begin(Some(tip), [0xef; 32]) {
        eprintln!("cannot open a block on the tip: {error:?}");
        return ExitCode::FAILURE;
    }
    match nano_oracle::evaluate_in_store(&mut store, source) {
        Ok(value) => println!("{value:?}"),
        Err(error) => println!("error: {error}"),
    }
    let _ = store.abort();
    ExitCode::SUCCESS
}

/// Rebuild one pre-checkpoint header from a peer, so a replay can pass it.
///
/// The burn context is exactly recoverable — `/v3/blocks/:id` gives the header
/// and `/v3/sortitions/consensus/:ch` gives the sortition it was elected by —
/// and that is what the epoch check in front of every `get-stacks-block-info?`
/// wants. The tenure and reward fields are *not* recoverable this way and are
/// left zero, so this says loudly which fields it did not fill: a contract
/// reading one of those gets a number that is not the chain's.
fn backfill_header(arguments: &[String]) -> ExitCode {
    let [state, peer, block] = arguments else {
        eprintln!(
            "usage: cargo xtask backfill-header <state-dir> <peer-url> <block-id>\n\
             the node must not be running: it holds the state open"
        );
        return ExitCode::FAILURE;
    };
    let Some(id) = hex::decode(block)
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).ok())
    else {
        eprintln!("the block id must be 32 hexadecimal bytes");
        return ExitCode::FAILURE;
    };
    let Ok(url) = peer.parse() else {
        eprintln!("{peer} is not a URL");
        return ExitCode::FAILURE;
    };
    let Ok(client) = nano_sync::SyncClient::new(url) else {
        eprintln!("cannot reach {peer}");
        return ExitCode::FAILURE;
    };
    let Ok(runtime) = tokio::runtime::Runtime::new() else {
        eprintln!("cannot start a runtime");
        return ExitCode::FAILURE;
    };
    let fetched = runtime.block_on(async {
        let block = client
            .block(nano_primitives::StacksBlockId::from_bytes(id))
            .await?;
        let sortition = client.sortition(block.header.consensus_hash).await?;
        Ok::<_, nano_sync::SyncError>((block, sortition))
    });
    let (block, sortition) = match fetched {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!("cannot rebuild the header from {peer}: {error:?}");
            return ExitCode::FAILURE;
        }
    };

    let mut vm = match nano_vm::Vm::open(Network::MAINNET, Path::new(state).join("chainstate")) {
        Ok(vm) => vm,
        Err(error) => {
            eprintln!("cannot open the state: {error:?}");
            return ExitCode::FAILURE;
        }
    };
    // The oracle this whole approach needs: rebuild a header the node already
    // holds and diff it. A reconstruction is only trustworthy for a block that
    // cannot be checked if it is exact for one that can.
    let recorded = vm.recorded_block_header(&id);
    let Ok(burn_block_height) = u32::try_from(sortition.bitcoin_height) else {
        eprintln!("the burn height does not fit");
        return ExitCode::FAILURE;
    };
    let rebuilt = nano_vm::BlockHeader {
            burn_header_hash: *sortition.bitcoin_block_hash.as_bytes(),
            burn_block_height,
            stacks_block_time: block.header.timestamp,
            block_header_hash: *block.header.block_hash().as_bytes(),
            consensus_hash: *block.header.consensus_hash.as_bytes(),
            // The oracle below caught all three of these being wrong. The
            // sortition's seed is not the one nano records; the header's
            // `bitcoin_spent` is a *running* total, not the per-tenure burn
            // `burn_spend_total` means; and nano leaves burn_block_time zero
            // for blocks it executes itself. Matching nano's own convention is
            // what keeps a backfilled header indistinguishable from a recorded
            // one — filling them from a peer would make this block answer
            // differently from every block beside it.
            burn_block_time: 0,
            vrf_seed: [0; 32],
            burn_spend_total: 0,
            // Not recoverable from these two calls either. Named below, because
            // a header has no way to say a field is absent and a zero read as a
            // real answer is worse than the stall this replaces.
            miner_address: (0, [0; 20]),
            burn_spend_winner: 0,
            block_reward: 0,
            tenure_height: 0,
            tenure_start_height: 0,
    };
    if let Some(recorded) = recorded {
        return compare_headers(&rebuilt, &recorded.header);
    }
    if let Err(error) = vm.record_partial_header(id, rebuilt, nano_vm::HeaderFields::PEER_BURN_CONTEXT)
    {
        eprintln!("recording the header failed: {error}");
        return ExitCode::FAILURE;
    }
    println!(
        "recorded a partial header for {} at burn height {burn_block_height}\n\
         exact: burn_header_hash, burn_block_height, consensus_hash, \
         stacks_block_time, block_header_hash\n\
         NOT KNOWN: {} -- a Clarity read of one of these now stops the node and \
         names the field, rather than answering with the zero in its place",
        hex::encode(id),
        nano_vm::HeaderFields::PEER_BURN_CONTEXT.absent_names().join(", ")
    );
    ExitCode::SUCCESS
}

/// Export the header fields Clarity can read, for a checkpoint's whole ancestry.
///
/// A checkpoint carries the trie for all of history and, until this existed, no
/// headers at all, so every `get-stacks-block-info?`, `get-tenure-info?` and
/// epoch lookup below the anchor had to be answered by asking a peer for one
/// block at a time — five fields out of thirteen, with zeros for the rest. A
/// stacks-core chainstate holds all thirteen for every block it ever processed,
/// so this is an export and an import rather than a reconstruction, and it needs
/// no peer at all.
///
/// Read straight out of the tables stacks-core's own `HeadersDB` reads
/// (`nakamoto_block_headers`, `block_headers`, `nakamoto_tenure_events`,
/// `payments`, `matured_rewards`), through the same joins: a field resolved at
/// the wrong block is a wrong answer, and five of these are resolved at the
/// block's *tenure start* rather than at the block.
fn export_headers(arguments: &[String]) -> ExitCode {
    let [index, out, height] = arguments else {
        eprintln!(
            "usage: cargo xtask export-headers <stacks-core index.sqlite> <out.sqlite> <to-height>\n\
             the height is the checkpoint's: a checkpoint has no business shipping \
             headers for blocks its state does not cover"
        );
        return ExitCode::from(2);
    };
    let Ok(to_height) = height.parse::<u64>() else {
        eprintln!("{height} is not a height");
        return ExitCode::FAILURE;
    };
    match run_header_export(Path::new(index), Path::new(out), to_height) {
        Ok((nakamoto, epoch2, partial)) => {
            println!(
                "exported {} headers ({nakamoto} Nakamoto, {epoch2} from epoch 2.x) up to height {to_height}\n\
                 {partial} of them are incomplete, and say so",
                nakamoto + epoch2
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("header export failed: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Print everything Clarity can read about a Stacks height, as Clarity reads it.
///
/// `xtask eval` cannot answer this: it evaluates against a context that knows no
/// chain, so every header read there is `none` whatever the state holds. This
/// goes through the state's own context and through `ClarityDatabase`, which is
/// where the guards live that decide when `none` is the chain's answer rather
/// than this node's ignorance.
fn block_info(arguments: &[String]) -> ExitCode {
    let [state, height] = arguments else {
        eprintln!(
            "usage: cargo xtask block-info <state-dir> <stacks-height>\n\
             the node must not be running: it holds the state open"
        );
        return ExitCode::from(2);
    };
    let Ok(height) = height.parse::<u32>() else {
        eprintln!("{height} is not a height");
        return ExitCode::FAILURE;
    };
    let mut vm = match nano_vm::Vm::open(Network::MAINNET, Path::new(state).join("chainstate")) {
        Ok(vm) => vm,
        Err(error) => {
            eprintln!("cannot open the state: {error}");
            return ExitCode::FAILURE;
        }
    };
    let Some(tip) = vm.tip() else {
        eprintln!("the state is sealed at no block");
        return ExitCode::FAILURE;
    };
    // The tip's own burn height, so the epoch of the block being stood on is the
    // one it really ran under. Every read below resolves its own block's epoch
    // from that block's header, so this only has to be sane.
    let mut context = nano_vm::BitcoinBlockContext::at_height(0);
    if let Some(header) = vm.recorded_header(tip) {
        context.height = u64::from(header.burn_block_height);
    }
    if let Err(error) = vm.begin_block_with_bitcoin_context(Some(tip), [0xef; 32], context) {
        eprintln!("cannot open a block on the tip: {error}");
        return ExitCode::FAILURE;
    }
    print_block_info(&mut vm.clarity_db(), height);
    let _ = vm.abort_block();
    ExitCode::SUCCESS
}

/// Print every header answer for a height, saying which stop the node.
fn print_block_info(database: &mut clarity::vm::database::ClarityDatabase<'_>, height: u32) {
    database.begin();
    let say = |name: &str, answer: Result<String, clarity::vm::errors::VmExecutionError>| {
        println!(
            "  {name:<28} {}",
            match answer {
                Ok(value) => value,
                // The loud failure this task chose over answering `none`: it names
                // the block and the field, and the node fetches what it lacks.
                Err(error) => format!("STOPS THE NODE: {error}"),
            }
        );
    };
    println!("get-stacks-block-info? at height {height}");
    say(
        "id-header-hash",
        database
            .get_index_block_header_hash(height)
            .map(|id| id.to_string()),
    );
    say(
        "header-hash",
        database
            .get_block_header_hash(height)
            .map(|hash| hash.to_string()),
    );
    say(
        "time",
        database.get_block_time(height).map(|value| value.to_string()),
    );
    let tenure_height = database.get_tenure_height().unwrap_or(0);
    println!(
        "get-tenure-info? for the tenure at that height (this state is at tenure {tenure_height})"
    );
    say(
        "burnchain-header-hash",
        database
            .get_burnchain_block_header_hash(height)
            .map(|hash| hash.to_string()),
    );
    say(
        "miner-address",
        database
            .get_miner_address(height)
            .map(|address| format!("{address}")),
    );
    say(
        "block-reward",
        database.get_block_reward(height).map(|reward| {
            reward.map_or_else(
                || "none -- and that is the chain's own answer".to_owned(),
                |value| value.to_string(),
            )
        }),
    );
    say(
        "miner-spend-total",
        database
            .get_miner_spend_total(height)
            .map(|value| value.to_string()),
    );
    say(
        "miner-spend-winner",
        database
            .get_miner_spend_winner(height)
            .map(|value| value.to_string()),
    );
    say(
        "vrf-seed",
        database
            .get_block_vrf_seed(height)
            .map(|seed| seed.to_string()),
    );
    say(
        "time",
        database
            .get_burn_block_time(height, None)
            .map(|value| value.to_string()),
    );
}

/// Take a header export into a state that already exists.
///
/// A checkpoint import does this itself. This is for the state that was imported
/// before the export existed, which is every mainnet state on this machine and
/// would otherwise have to be imported again from a 380 GB checkpoint.
fn import_headers(arguments: &[String]) -> ExitCode {
    let [state, export] = arguments else {
        eprintln!(
            "usage: cargo xtask import-headers <state-dir> <block-headers.sqlite>\n\
             the node must not be running: it holds the state open"
        );
        return ExitCode::from(2);
    };
    let mut vm = match nano_vm::Vm::open(Network::MAINNET, Path::new(state).join("chainstate")) {
        Ok(vm) => vm,
        Err(error) => {
            eprintln!("cannot open the state: {error}");
            return ExitCode::FAILURE;
        }
    };
    match vm.import_block_headers(Path::new(export)) {
        Ok(imported) => {
            println!("imported {imported} headers");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("importing the headers failed: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Export the leader-key registry a checkpoint has to carry.
///
/// A winning block commitment names the registration that authorises its VRF
/// proof by burn position, and a leader key is registered *once* and named by
/// commitments for years afterwards: the five keys mainnet's miners used across
/// the epoch 4.0 boundary were registered between burn 867,772 and 939,759, up
/// to ninety thousand blocks below it. No burnchain window a follower holds
/// reaches them, so a node that does not carry them cannot check a single
/// tenure's coinbase proof — it can only report that it could not.
///
/// Fetching them from the peer that supplied the block is exactly the dependency
/// this group of tasks exists to remove, and carrying them is small: mainnet's
/// whole history is 2,477 registrations, a quarter of a megabyte of JSON.
fn export_leader_keys(arguments: &[String]) -> ExitCode {
    let [sortition, out, height] = arguments else {
        eprintln!(
            "usage: cargo xtask export-leader-keys <stacks-core burnchain/sortition/marf.sqlite> \
             <out.json> <to-burn-height>\n\
             the height is the checkpoint's burn anchor: a registration above it belongs to \
             burn blocks the node walks for itself"
        );
        return ExitCode::from(2);
    };
    let Ok(to_height) = height.parse::<u64>() else {
        eprintln!("{height} is not a burn height");
        return ExitCode::FAILURE;
    };
    match read_leader_keys(Path::new(sortition), to_height)
        .and_then(|keys| write_leader_keys(Path::new(out), &keys))
    {
        Ok(exported) => {
            println!(
                "exported {} leader-key registrations at or below burn {to_height}, {} of them \
                 carrying a block-signing key hash",
                exported.0, exported.1
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("exporting the leader keys failed: {error}");
            ExitCode::FAILURE
        }
    }
}

/// One row of stacks-core's `leader_keys`, in its own column names.
///
/// Kept as the archive spells it — `public_key` and `memo` rather than anything
/// friendlier — so the export is a copy of the rows and not a translation of
/// them, which is where a wrong field would hide. `memo` is the 20-byte
/// block-signing key hash for a Nakamoto-era registration and empty for the
/// registrations from before it, which is most of mainnet's.
struct ExportedLeaderKey {
    block_height: u64,
    vtxindex: u32,
    public_key: String,
    memo: String,
}

/// Read the registry out of a stacks-core sortition database.
///
/// Only the canonical rows are wanted, and `leader_keys` is keyed by
/// `(txid, sortition_id)`, so a registration seen on two sortitions of a
/// Bitcoin fork appears twice. Grouping by burn position collapses them, which
/// is sound because the position is what a commitment names: mainnet's 2,477
/// rows occupy 2,477 distinct positions and no position carries two different
/// keys, so this drops nothing.
fn read_leader_keys(sortition: &Path, to_height: u64) -> Result<Vec<ExportedLeaderKey>, String> {
    let archive = open_archive(sortition)?;
    let mut statement = archive
        .prepare(
            "SELECT block_height, vtxindex, public_key, COALESCE(memo, '') FROM leader_keys \
             WHERE block_height <= ?1 GROUP BY block_height, vtxindex \
             ORDER BY block_height, vtxindex",
        )
        .map_err(|error| format!("reading leader_keys: {error}"))?;
    let keys = statement
        .query_map([to_height], |row| {
            Ok(ExportedLeaderKey {
                block_height: row.get(0)?,
                vtxindex: row.get(1)?,
                public_key: row.get(2)?,
                memo: row.get(3)?,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    if keys.is_empty() {
        return Err(format!(
            "{} holds no leader keys at or below burn {to_height}",
            sortition.display()
        ));
    }
    Ok(keys)
}

/// Write a registry where a checkpoint's sortition directory expects it.
fn write_leader_keys(out: &Path, keys: &[ExportedLeaderKey]) -> Result<(usize, usize), String> {
    let signing = keys.iter().filter(|key| !key.memo.is_empty()).count();
    let rows: Vec<serde_json::Value> = keys
        .iter()
        .map(|key| {
            json!({
                "block_height": key.block_height,
                "vtxindex": key.vtxindex,
                "public_key": key.public_key,
                "memo": key.memo,
            })
        })
        .collect();
    write_file(
        out,
        &serde_json::to_vec(&rows).map_err(|error| error.to_string())?,
    )?;
    Ok((keys.len(), signing))
}

/// A tenure's start block, which is where five of a header's fields live.
struct TenureStart {
    block_id: String,
    tenure_height: u32,
}

/// What a tenure's start block answers for every block of that tenure.
struct TenureFacts {
    start_height: u32,
    vrf_seed: Option<[u8; 32]>,
    miner: Option<(u8, [u8; 20])>,
    spends: Option<(u128, u128)>,
    reward: Option<u128>,
}

fn run_header_export(
    index: &Path,
    out: &Path,
    to_height: u64,
) -> Result<(usize, usize, usize), String> {
    let source = open_archive(index)?;
    let destination = create_export(out)?;
    let tenures = read_tenure_starts(&source)?;
    let mut facts: std::collections::HashMap<String, TenureFacts> =
        std::collections::HashMap::new();
    let mut partial = 0;
    let transaction = destination
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    let nakamoto = export_rows(
        &source,
        &destination,
        to_height,
        true,
        &tenures,
        &mut facts,
        &mut partial,
    )?;
    let epoch2 = export_rows(
        &source,
        &destination,
        to_height,
        false,
        &tenures,
        &mut facts,
        &mut partial,
    )?;
    transaction.commit().map_err(|error| error.to_string())?;
    Ok((nakamoto, epoch2, partial))
}

/// Open a stacks-core chainstate index without being able to write to it.
fn open_archive(index: &Path) -> Result<rusqlite::Connection, String> {
    rusqlite::Connection::open_with_flags(
        format!("file:{}?mode=ro&immutable=1", index.display()),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|error| format!("cannot read {}: {error}", index.display()))
}

fn create_export(out: &Path) -> Result<rusqlite::Connection, String> {
    if out.exists() {
        return Err(format!("{} already exists", out.display()));
    }
    let connection = rusqlite::Connection::open(out).map_err(|error| error.to_string())?;
    connection
        .query_row("PRAGMA journal_mode = OFF", [], |_| Ok(()))
        .map_err(|error| error.to_string())?;
    connection
        .execute_batch(
            "PRAGMA synchronous = OFF;
             PRAGMA cache_size = -200000;
             PRAGMA temp_store = MEMORY;",
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute_batch(nano_vm::HEADER_EXPORT_SCHEMA)
        .map_err(|error| error.to_string())?;
    Ok(connection)
}

/// Every tenure's start block and tenure height, by the tenure's consensus hash.
///
/// This is what stacks-core reaches through the MARF key
/// `nakamoto::headers::tenure_start_block_id::<ch>`; the same fact is in this
/// table, keyed the same way, and a table can be read in one pass.
fn read_tenure_starts(
    source: &rusqlite::Connection,
) -> Result<std::collections::HashMap<String, TenureStart>, String> {
    let mut statement = source
        .prepare(
            "SELECT tenure_id_consensus_hash, block_id, coinbase_height \
             FROM nakamoto_tenure_events WHERE cause = 0",
        )
        .map_err(|error| error.to_string())?;
    let mut rows = statement.query([]).map_err(|error| error.to_string())?;
    let mut starts = std::collections::HashMap::new();
    while let Some(row) = rows.next().map_err(|error| error.to_string())? {
        let consensus_hash: String = row.get(0).map_err(|error| error.to_string())?;
        let block_id: String = row.get(1).map_err(|error| error.to_string())?;
        let tenure_height: u32 = row.get(2).map_err(|error| error.to_string())?;
        starts.insert(
            consensus_hash,
            TenureStart {
                block_id,
                tenure_height,
            },
        );
    }
    Ok(starts)
}

/// Export one header table, Nakamoto's or epoch 2.x's.
fn export_rows(
    source: &rusqlite::Connection,
    destination: &rusqlite::Connection,
    to_height: u64,
    nakamoto: bool,
    tenures: &std::collections::HashMap<String, TenureStart>,
    facts: &mut std::collections::HashMap<String, TenureFacts>,
    partial: &mut usize,
) -> Result<usize, String> {
    let time_column = if nakamoto { "timestamp" } else { "0" };
    let table = header_table(nakamoto);
    let mut statement = source
        .prepare(&format!(
            "SELECT index_block_hash, block_height, burn_header_hash, burn_header_height, \
                    burn_header_timestamp, {time_column}, block_hash, consensus_hash \
             FROM {table} WHERE block_height <= ?1"
        ))
        .map_err(|error| error.to_string())?;
    let mut rows = statement
        .query(rusqlite::params![to_height])
        .map_err(|error| error.to_string())?;
    let mut exported = 0;
    while let Some(row) = rows.next().map_err(|error| error.to_string())? {
        let text = |index: usize| -> Result<String, String> {
            row.get(index).map_err(|error: rusqlite::Error| error.to_string())
        };
        let block_id = text(0)?;
        let stacks_height: u64 = row.get(1).map_err(|error| error.to_string())?;
        // A Nakamoto block's tenure fields come from its tenure's start block; an
        // epoch 2.x block *is* its own tenure, which is what
        // `get_first_block_in_tenure` returns for one.
        let consensus_hash = text(7)?;
        let tenure = if nakamoto {
            tenures.get(&consensus_hash)
        } else {
            None
        };
        let tenure_start_id = tenure.map_or_else(|| block_id.clone(), |start| start.block_id.clone());
        if !facts.contains_key(&tenure_start_id) {
            let computed = read_tenure_facts(source, &tenure_start_id, nakamoto, tenures)?;
            facts.insert(tenure_start_id.clone(), computed);
        }
        let known = facts.get(&tenure_start_id).ok_or("tenure facts vanished")?;
        let fields = exported_fields(nakamoto, known, tenure.is_some());
        if !fields.is_complete() {
            *partial += 1;
        }
        let (spend_total, spend_winner) = known.spends.unwrap_or((0, 0));
        let (miner_version, miner_hash) = known.miner.unwrap_or((0, [0; 20]));
        // Parsed before the insert closure, whose errors have to be SQLite's.
        let block = parse_hash::<32>(&block_id)?;
        let burn_header_hash = parse_hash::<32>(&text(2)?)?;
        let block_header_hash = parse_hash::<32>(&text(6)?)?;
        let consensus = parse_hash::<20>(&consensus_hash)?;
        destination
            .prepare_cached(
                "INSERT OR REPLACE INTO exported_header (\
                    block_id, stacks_height, burn_header_hash, burn_block_height, \
                    burn_block_time, stacks_block_time, block_header_hash, consensus_hash, \
                    vrf_seed, miner_version, miner_hash, burn_spend_total, burn_spend_winner, \
                    block_reward, tenure_height, tenure_start_height, known) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            )
            .and_then(|mut insert| {
                insert.execute(rusqlite::params![
                    block.as_slice(),
                    stacks_height,
                    burn_header_hash.as_slice(),
                    row.get::<_, u32>(3)?,
                    row.get::<_, u64>(4)?,
                    row.get::<_, u64>(5)?,
                    block_header_hash.as_slice(),
                    consensus.as_slice(),
                    known.vrf_seed.unwrap_or([0; 32]).as_slice(),
                    miner_version,
                    miner_hash.as_slice(),
                    spend_total.to_string(),
                    spend_winner.to_string(),
                    known.reward.unwrap_or(0).to_string(),
                    tenure.map_or(0, |start| start.tenure_height),
                    if tenure.is_some() { known.start_height } else { 0 },
                    fields.bits(),
                ])
            })
            .map_err(|error| error.to_string())?;
        exported += 1;
    }
    Ok(exported)
}

/// Which of a block's fields the archive actually answered.
///
/// A zero in an unanswered column is not an answer, and this is where that is
/// decided: an epoch 2.x block has no Nakamoto timestamp and no tenure height,
/// and a tenure with no matured reward row has no reward, because stacks-core has
/// not matured it either.
const fn exported_fields(
    nakamoto: bool,
    facts: &TenureFacts,
    has_tenure: bool,
) -> nano_vm::HeaderFields {
    let mut fields = nano_vm::HeaderFields::BURN_HEADER_HASH
        .union(nano_vm::HeaderFields::BURN_BLOCK_HEIGHT)
        .union(nano_vm::HeaderFields::BURN_BLOCK_TIME)
        .union(nano_vm::HeaderFields::BLOCK_HEADER_HASH)
        .union(nano_vm::HeaderFields::CONSENSUS_HASH);
    if nakamoto {
        fields = fields.union(nano_vm::HeaderFields::STACKS_BLOCK_TIME);
    }
    if facts.vrf_seed.is_some() {
        fields = fields.union(nano_vm::HeaderFields::VRF_SEED);
    }
    if facts.miner.is_some() {
        fields = fields.union(nano_vm::HeaderFields::MINER_ADDRESS);
    }
    if facts.spends.is_some() {
        fields = fields
            .union(nano_vm::HeaderFields::BURN_SPEND_TOTAL)
            .union(nano_vm::HeaderFields::BURN_SPEND_WINNER);
    }
    if facts.reward.is_some() {
        fields = fields.union(nano_vm::HeaderFields::BLOCK_REWARD);
    }
    if has_tenure {
        fields = fields
            .union(nano_vm::HeaderFields::TENURE_HEIGHT)
            .union(nano_vm::HeaderFields::TENURE_START_HEIGHT);
    }
    fields
}

const fn header_table(nakamoto: bool) -> &'static str {
    if nakamoto {
        "nakamoto_block_headers"
    } else {
        "block_headers"
    }
}

/// The five fields resolved at a tenure's start block rather than at the block.
fn read_tenure_facts(
    source: &rusqlite::Connection,
    tenure_start_id: &str,
    nakamoto: bool,
    tenures: &std::collections::HashMap<String, TenureStart>,
) -> Result<TenureFacts, String> {
    let table = header_table(nakamoto);
    let proof_column = if nakamoto { "vrf_proof" } else { "proof" };
    let start: Option<(u64, Option<String>, String)> = source
        .query_row(
            &format!(
                "SELECT block_height, {proof_column}, parent_block_id FROM {table} \
                 WHERE index_block_hash = ?1"
            ),
            rusqlite::params![tenure_start_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional_row()?;
    let Some((start_height, proof, parent_block_id)) = start else {
        return Ok(TenureFacts {
            start_height: 0,
            vrf_seed: None,
            miner: None,
            spends: None,
            reward: None,
        });
    };
    let payment: Option<(String, u64, u64)> = source
        .query_row(
            "SELECT address, burnchain_sortition_burn, burnchain_commit_burn FROM payments \
             WHERE index_block_hash = ?1 AND miner = 1",
            rusqlite::params![tenure_start_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional_row()?;
    Ok(TenureFacts {
        start_height: u32::try_from(start_height).map_err(|_| "height overflows u32".to_owned())?,
        vrf_seed: proof.as_deref().and_then(vrf_seed_of_proof),
        miner: payment.as_ref().and_then(|(address, _, _)| miner_address(address)),
        spends: payment.map(|(_, sortition, commit)| (u128::from(sortition), u128::from(commit))),
        reward: read_matured_reward(source, tenure_start_id, &parent_block_id, tenures)?,
    })
}

/// What the tenure earned once its reward matured, as stacks-core totals it.
///
/// Keyed by the *parent tenure* and this tenure, and split across two rows: the
/// child's carries the coinbase and the parent's carries the streamed fees it
/// produced. Any other shape is not a reward this node may claim to know, so it
/// is left absent rather than summed anyway — the last hundred tenures before a
/// checkpoint have no rows at all, because stacks-core has not matured them yet
/// either.
fn read_matured_reward(
    source: &rusqlite::Connection,
    tenure_start_id: &str,
    parent_block_id: &str,
    tenures: &std::collections::HashMap<String, TenureStart>,
) -> Result<Option<u128>, String> {
    let parent_tenure_id = parent_tenure(source, parent_block_id, tenures)?;
    let Some(parent_tenure_id) = parent_tenure_id else {
        return Ok(None);
    };
    let mut statement = source
        .prepare_cached(
            "SELECT coinbase, tx_fees_anchored, tx_fees_streamed_confirmed, \
                    tx_fees_streamed_produced FROM matured_rewards \
             WHERE parent_index_block_hash = ?1 AND child_index_block_hash = ?2 AND vtxindex = 0",
        )
        .map_err(|error| error.to_string())?;
    let mut rows = statement
        .query(rusqlite::params![parent_tenure_id, tenure_start_id])
        .map_err(|error| error.to_string())?;
    let mut child = None;
    let mut streamed_by_parent = None;
    while let Some(row) = rows.next().map_err(|error| error.to_string())? {
        let amount = |index: usize| -> Result<u128, String> {
            row.get::<_, String>(index)
                .map_err(|error| error.to_string())?
                .parse()
                .map_err(|_| "malformed reward amount".to_owned())
        };
        let (coinbase, anchored, confirmed, produced) =
            (amount(0)?, amount(1)?, amount(2)?, amount(3)?);
        if coinbase > 0 && produced == 0 {
            child = Some(coinbase + anchored + confirmed);
        } else if coinbase == 0 {
            streamed_by_parent = Some(produced);
        }
    }
    Ok(match (child, streamed_by_parent) {
        (Some(child), Some(streamed)) => Some(child + streamed),
        _ => None,
    })
}

/// The tenure a block belongs to, which for an epoch 2.x block is itself.
fn parent_tenure(
    source: &rusqlite::Connection,
    block_id: &str,
    tenures: &std::collections::HashMap<String, TenureStart>,
) -> Result<Option<String>, String> {
    let epoch2: Option<u32> = source
        .query_row(
            "SELECT 1 FROM block_headers WHERE index_block_hash = ?1",
            rusqlite::params![block_id],
            |row| row.get(0),
        )
        .optional_row()?;
    if epoch2.is_some() {
        return Ok(Some(block_id.to_owned()));
    }
    let consensus_hash: Option<String> = source
        .query_row(
            "SELECT consensus_hash FROM nakamoto_block_headers WHERE index_block_hash = ?1",
            rusqlite::params![block_id],
            |row| row.get(0),
        )
        .optional_row()?;
    Ok(consensus_hash
        .and_then(|hash| tenures.get(&hash))
        .map(|start| start.block_id.clone()))
}

/// A VRF seed is the hash of the tenure's proof, as `VRFSeed::from_proof` has it.
fn vrf_seed_of_proof(proof: &str) -> Option<[u8; 32]> {
    let bytes = hex::decode(proof).ok()?;
    Some(*nano_primitives::sha512_256(&bytes).as_bytes())
}

/// A miner's c32 address as the version byte and hash Clarity answers with.
fn miner_address(address: &str) -> Option<(u8, [u8; 20])> {
    use clarity::types::Address as _;
    let parsed = clarity::types::chainstate::StacksAddress::from_string(address)?;
    Some((parsed.version(), parsed.bytes().0))
}

fn parse_hash<const LENGTH: usize>(value: &str) -> Result<[u8; LENGTH], String> {
    hex::decode(value)
        .ok()
        .and_then(|bytes| <[u8; LENGTH]>::try_from(bytes.as_slice()).ok())
        .ok_or_else(|| format!("{value} is not {LENGTH} hex bytes"))
}

/// `Option`-shaped row reads, since a missing row is ordinary here.
trait OptionalRow<T> {
    fn optional_row(self) -> Result<Option<T>, String>;
}

impl<T> OptionalRow<T> for Result<T, rusqlite::Error> {
    fn optional_row(self) -> Result<Option<T>, String> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// Read what a header could be rebuilt from, at a block older than the checkpoint.
///
/// Task 055 turns on one question: which of a `BlockHeader`'s fields can be
/// recovered for a block this node never executed. Guessing the answer is how a
/// contract ends up reading an invented miner address, so this measures it —
/// against a block whose header *is* recorded, where every answer can be
/// checked, before anything relies on it for one that cannot.
fn probe_header(arguments: &[String]) -> ExitCode {
    let [state, block] = arguments else {
        eprintln!(
            "usage: cargo xtask probe-header <state-dir> <block-id>\n\
             the node must not be running: it holds the state open"
        );
        return ExitCode::FAILURE;
    };
    let Some(id) = hex::decode(block)
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).ok())
    else {
        eprintln!("the block id must be 32 hexadecimal bytes");
        return ExitCode::FAILURE;
    };
    let chainstate = Path::new(state).join("chainstate");
    let mut vm = match nano_vm::Vm::open(Network::MAINNET, &chainstate) {
        Ok(vm) => vm,
        Err(error) => {
            eprintln!("cannot open the state: {error:?}");
            return ExitCode::FAILURE;
        }
    };

    let stacks_height = u64::from(vm.height_of(id).unwrap_or(0));
    println!("Stacks height: {stacks_height}");

    // Three different answers, and the point of printing which: a block with no
    // header at all is one to fetch, a block that is not on this fork is a bug
    // somewhere else, and a header that is present but incomplete answers some
    // Clarity reads and stops the node on others.
    let recorded = match vm.header_knowledge(id) {
        nano_vm::HeaderKnowledge::Held(known) => {
            println!(
                "header: held, {} of 13 fields known{}",
                known.count(),
                if known.is_complete() {
                    String::new()
                } else {
                    format!("; NOT KNOWN: {}", known.absent_names().join(", "))
                }
            );
            vm.recorded_block_header(&id)
        }
        nano_vm::HeaderKnowledge::NeverCarried => {
            println!(
                "header: none, though the block is in this node's index -- \
                 this is the case the node stalls on, and the case a header export fixes"
            );
            None
        }
        nano_vm::HeaderKnowledge::Absent => {
            println!("header: none, and the block is not in this node's index either");
            None
        }
    };

    // Tenure height is a Clarity value, so it lives *in* the MARF, and the
    // checkpoint imports the trie graph — which makes a read here look like it
    // must be exact. It is not: below the anchor's parent the key is absent and
    // the read answers with the block height instead, silently. Printing the
    // value beside the Stacks height is what makes that visible.
    match vm.begin_block(Some(id), [0xce; 32]) {
        Ok(()) => match vm.tenure_height() {
            Ok(height) => {
                // A tenure height equal to the Stacks height is the tell: this
                // block's trie has no tenure-height key and the read fell
                // through, so the answer is not this block's at all.
                println!(
                    "tenure height at this block: {height}{}",
                    if u64::from(height) >= stacks_height {
                        " -- NOT A TENURE HEIGHT: no state at this block, the read fell through"
                    } else {
                        " (read from this block's trie)"
                    }
                );
                if let Some(recorded) = recorded
                    && let Some(tenure_height) =
                        recorded.field(nano_vm::HeaderFields::TENURE_HEIGHT, |header| {
                            header.tenure_height
                        })
                {
                    println!(
                        "recorded tenure height:      {tenure_height} -> {}",
                        if tenure_height == height {
                            "MATCHES"
                        } else {
                            "DIFFERS"
                        }
                    );
                }
            }
            Err(error) => println!("tenure height unreadable here: {error:?}"),
        },
        Err(error) => println!("cannot open the trie at this block: {error:?}"),
    }
    let _ = vm.abort_block();
    if let Some(recorded) = recorded {
        print_recorded_fields(&recorded);
    }
    ExitCode::SUCCESS
}

/// Print the fields a recorded header answers, and name the ones it does not.
fn print_recorded_fields(recorded: &nano_vm::RecordedHeader) {
    // Each field asked for by name, so an absent one prints as absent rather than
    // as the zero sitting in its place.
    let say = |field: nano_vm::HeaderFields, value: String| {
        println!(
            "  {:<20} {}",
            field.name(),
            if recorded.known.contains(field) {
                value
            } else {
                "NOT KNOWN HERE".to_owned()
            }
        );
    };
    let header = recorded.header;
    say(
        nano_vm::HeaderFields::BURN_BLOCK_HEIGHT,
        header.burn_block_height.to_string(),
    );
    say(
        nano_vm::HeaderFields::BURN_BLOCK_TIME,
        header.burn_block_time.to_string(),
    );
    say(
        nano_vm::HeaderFields::MINER_ADDRESS,
        format!("{:?}", header.miner_address),
    );
    say(
        nano_vm::HeaderFields::BLOCK_REWARD,
        header.block_reward.to_string(),
    );
    say(
        nano_vm::HeaderFields::TENURE_START_HEIGHT,
        header.tenure_start_height.to_string(),
    );
}

/// Print the compressed public key a private key signs with.
/// Check a block against the reward set that was published for its cycle.
///
/// Everything this needs is served by any node — the block from
/// `/v3/blocks/:id` and the set from `/v3/stacker_set/:cycle` — so it works
/// against mainnet without a chainstate to replay from. It proves the envelope
/// only: that nano derives the same signer signature hash the network signed,
/// recovers the same keys from it, and counts the same weight against the same
/// threshold. It says nothing about execution.
fn verify_block(arguments: &[String]) -> ExitCode {
    let [block_path, set_path] = arguments else {
        eprintln!("usage: cargo xtask verify-block <block.bin> <stacker_set.json>");
        return ExitCode::from(2);
    };
    let block = match fs::read(block_path).map_err(|error| error.to_string()).and_then(
        |bytes| NakamotoBlock::decode(&bytes).map_err(|error| error.to_string()),
    ) {
        Ok(block) => block,
        Err(error) => {
            eprintln!("{block_path}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let set = match fs::read(set_path)
        .map_err(|error| error.to_string())
        .and_then(|bytes| signer_set_from_json(&bytes))
    {
        Ok(set) => set,
        Err(error) => {
            eprintln!("{set_path}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let header = &block.header;
    println!(
        "block {} at height {}",
        hex::encode(header.block_hash().as_bytes()),
        header.chain_length
    );
    println!(
        "signer signature hash {}",
        hex::encode(header.signer_signature_hash().as_bytes())
    );
    println!(
        "{} signatures against {} signers",
        header.signer_signatures.len(),
        set.signers().len()
    );
    match set.verify(header) {
        Ok(weight) => {
            let total: u32 = set.signers().iter().map(|signer| signer.weight).sum();
            println!("accepted with weight {weight} of {total}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("rejected: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Build a signer set from what `/v3/stacker_set/:cycle` serves.
fn signer_set_from_json(bytes: &[u8]) -> Result<SignerSet, String> {
    let document: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    let set = document.get("stacker_set").unwrap_or(&document);
    let entries = set
        .get("signers")
        .and_then(|signers| signers.as_array())
        .ok_or_else(|| "no signers in the reward set".to_owned())?;
    let mut signers = Vec::with_capacity(entries.len());
    for entry in entries {
        let key = entry
            .get("signing_key")
            .and_then(|key| key.as_str())
            .ok_or_else(|| "a signer has no signing key".to_owned())?;
        let bytes = hex::decode(key.trim_start_matches("0x")).map_err(|error| error.to_string())?;
        let public_key =
            nano_crypto::StacksPublicKey::from_bytes(&bytes).map_err(|error| error.to_string())?;
        let weight = entry
            .get("weight")
            .and_then(serde_json::Value::as_u64)
            .and_then(|weight| u32::try_from(weight).ok())
            .ok_or_else(|| "a signer has no weight".to_owned())?;
        signers.push(Signer { public_key, weight });
    }
    SignerSet::new(signers).map_err(|error| error.to_string())
}

fn print_public_key(private_key: Option<&str>) -> ExitCode {
    let Some(bytes) = private_key
        .map(|key| key.trim().trim_start_matches("0x"))
        .map(hex::decode)
        .transpose()
        .ok()
        .flatten()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
    else {
        eprintln!("usage: cargo xtask public-key <32-byte hexadecimal private key>");
        return ExitCode::from(2);
    };
    match nano_crypto::StacksPrivateKey::from_bytes(bytes) {
        Ok(key) => {
            println!("{}", hex::encode(key.public_key().to_bytes_compressed()));
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// Tenures between a reward being earned and paid, mirroring stacks-core.
const MINER_REWARD_MATURITY: u64 = 100;

/// The reward one tenure earned, as stacks-core scheduled it.
struct ScheduledPayment {
    recipient: String,
    coinbase: u128,
    anchored: u128,
    nakamoto: bool,
}

fn fixture_root() -> PathBuf {
    // `NANO_FIXTURES` points the scoreboard at a capture outside the tree,
    // which is how a mainnet one is read without installing it first.
    env::var_os("NANO_FIXTURES").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../crates/nano-conformance/fixtures"),
        PathBuf::from,
    )
}

/// Decode a concatenated stream of consensus-serialized Nakamoto blocks.
///
/// A block nano cannot decode stops a mainnet descent dead, and finding out
/// which byte by restarting a node against a live peer costs minutes a time.
/// This reads the bytes off disk and says exactly which block and which
/// transaction failed, in the time a build takes.
fn decode_blocks(path: Option<&str>) -> ExitCode {
    let Some(path) = path else {
        eprintln!("usage: cargo xtask decode-blocks <blocks.bin>");
        return ExitCode::from(2);
    };
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("cannot read {path}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut offset = 0;
    let mut decoded = 0;
    while offset < bytes.len() {
        match NakamotoBlock::decode_prefix(&bytes[offset..]) {
            Ok((block, consumed)) => {
                decoded += 1;
                offset += consumed;
                println!(
                    "block {} height {} with {} transactions",
                    block.block_id(),
                    block.header.chain_length,
                    block.transactions.len()
                );
                for transaction in &block.transactions {
                    let payer = transaction.auth().payer();
                    println!(
                        "  tx {} origin {} nonce {} fee {} payload {}",
                        transaction.txid(),
                        transaction.auth().origin().account_address(true),
                        payer.nonce(),
                        payer.fee(),
                        transaction.payload_type() as u8
                    );
                    // A block of dependent deploys is routine on mainnet, and
                    // which contract each transaction publishes is what says
                    // where to look when one of them fails.
                    match transaction.payload().data() {
                        nano_codec::TransactionPayloadData::SmartContract {
                            contract_name,
                            source,
                        }
                        | nano_codec::TransactionPayloadData::VersionedSmartContract {
                            contract_name,
                            source,
                            ..
                        } => {
                            println!("    deploys {contract_name}, {} chars", source.len());
                            // The source of a deployment that *failed* is not in
                            // the state -- the transaction that would have put it
                            // there is the one that did not run -- so the block is
                            // the only place it exists. Written out so
                            // `check-module` can be pointed at it.
                            if let Some(directory) = env::var_os("NANO_DUMP_DEPLOYS") {
                                let path =
                                    Path::new(&directory).join(format!("{contract_name}.clar"));
                                match fs::write(&path, source) {
                                    Ok(()) => println!("    wrote {}", path.display()),
                                    Err(error) => {
                                        eprintln!("    cannot write the source: {error}");
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(error) => {
                eprintln!("block {decoded} at byte {offset} does not decode: {error}");
                eprintln!("next 64 bytes: {}", hex::encode(&bytes[offset..(offset + 64).min(bytes.len())]));
                return ExitCode::FAILURE;
            }
        }
    }
    println!("{decoded} blocks decoded");
    ExitCode::SUCCESS
}

fn print_scoreboard() -> ExitCode {
    let manifest_path = fixture_root().join("manifest.toml");
    match FixtureManifest::load(&manifest_path) {
        Ok(manifest) => {
            print!("{}", scoreboard_at(&fixture_root(), manifest));
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn validate_fixtures() -> ExitCode {
    match validate_fixture_tree(&fixture_root()) {
        Ok(FixtureStatus::Captured { replay_blocks }) => {
            println!("captured fixture tree is valid for {replay_blocks} replay blocks");
            ExitCode::SUCCESS
        }
        Ok(FixtureStatus::Baseline { .. }) => {
            eprintln!("fixture tree is still the empty baseline; capture real epoch-4 data first");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("fixture validation failed: {error}");
            ExitCode::FAILURE
        }
    }
}

struct CaptureConfig {
    /// Rewrite only `native-effects.json`, into this existing checkpoint.
    accounting_only: Option<PathBuf>,
    out_dir: Option<PathBuf>,
    state_dir: PathBuf,
    /// The directory holding `chainstate/` and `burnchain/`.
    ///
    /// Hacknet nests it under a participant, a node run anywhere else does
    /// not, so a capture from an archived mainnet chainstate names it
    /// directly.
    node_root: Option<PathBuf>,
    /// Absent when no event observer was attached, as for a capture taken
    /// from an archived chainstate: the receipts simply are not there.
    events_dir: Option<PathBuf>,
    /// The unlock heights the events otherwise carry, needed only without them.
    unlock_heights: Option<[u64; 4]>,
    bitcoin_rpc: Option<String>,
    /// A build to capture from other than the pinned one, named explicitly.
    ///
    /// The guard exists so a Hacknet capture cannot silently disagree with the
    /// in-process oracles. Mainnet is a different case: the chain is the
    /// oracle, and it runs whatever it runs. Naming the build rather than
    /// waving the check through keeps it in the provenance, so a divergence
    /// can be read against the right source.
    accept_node_revision: Option<String>,
    /// An Esplora base URL, for a capture with no Bitcoin node to ask.
    ///
    /// `<base>/block/<hash>/raw` serves the same bytes `getblock <hash> 0`
    /// returns, which is all the capture wants them for.
    bitcoin_rest: Option<String>,
    stacks_rpc: String,
    hacknet_commit: String,
    first_height: u64,
    replay_blocks: u64,
    checkpoint_height: u64,
    bitcoin_magic: String,
}

fn capture_fixtures(arguments: &[String]) -> ExitCode {
    match CaptureConfig::parse(arguments)
        .and_then(|config| config.capture(&config.out_dir.clone().unwrap_or_else(fixture_root)))
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fixture capture failed: {error}");
            ExitCode::FAILURE
        }
    }
}

impl CaptureConfig {
    fn parse(arguments: &[String]) -> Result<Self, String> {
        let mut values = arguments.iter();
        let mut accounting_only = None;
        let mut out_dir = None;
        let mut state_dir = None;
        let mut node_root = None;
        let mut events_dir = None;
        let mut unlock_heights: [Option<u64>; 4] = [None; 4];
        let mut bitcoin_rpc = None;
        let mut bitcoin_rest = None;
        let mut accept_node_revision = None;
        let mut stacks_rpc = None;
        let mut hacknet_commit = None;
        let mut first_height = None;
        let mut replay_blocks = None;
        let mut checkpoint_height = None;
        let mut bitcoin_magic = None;

        while let Some(flag) = values.next() {
            let value = values
                .next()
                .ok_or_else(|| format!("missing value for {flag}"))?;
            match flag.as_str() {
                "--out-dir" => out_dir = Some(PathBuf::from(value)),
                "--state-dir" => state_dir = Some(PathBuf::from(value)),
                "--node-root" => node_root = Some(PathBuf::from(value)),
                "--accounting-only" => accounting_only = Some(PathBuf::from(value)),
                "--events-dir" => events_dir = Some(PathBuf::from(value)),
                "--pox-v1-unlock-height" => unlock_heights[0] = Some(parse_u64(flag, value)?),
                "--pox-v2-unlock-height" => unlock_heights[1] = Some(parse_u64(flag, value)?),
                "--pox-v3-unlock-height" => unlock_heights[2] = Some(parse_u64(flag, value)?),
                "--pox-v4-unlock-height" => unlock_heights[3] = Some(parse_u64(flag, value)?),
                "--bitcoin-rpc" => bitcoin_rpc = Some(value.to_owned()),
                "--bitcoin-rest" => bitcoin_rest = Some(value.to_owned()),
                "--accept-node-revision" => accept_node_revision = Some(value.to_owned()),
                "--stacks-rpc" => stacks_rpc = Some(value.to_owned()),
                "--hacknet-commit" => hacknet_commit = Some(value.to_owned()),
                "--first-height" => first_height = Some(parse_u64(flag, value)?),
                "--replay-blocks" => replay_blocks = Some(parse_u64(flag, value)?),
                "--checkpoint-height" => checkpoint_height = Some(parse_u64(flag, value)?),
                "--bitcoin-magic" => bitcoin_magic = Some(value.to_owned()),
                _ => return Err(format!("unknown capture-fixtures argument: {flag}")),
            }
        }

        Ok(Self {
            accounting_only,
            out_dir,
            state_dir: state_dir.ok_or_else(|| "--state-dir is required".to_owned())?,
            node_root,
            events_dir,
            unlock_heights: match unlock_heights {
                [Some(v1), Some(v2), Some(v3), Some(v4)] => Some([v1, v2, v3, v4]),
                _ => None,
            },
            bitcoin_rpc,
            bitcoin_rest,
            accept_node_revision,
            stacks_rpc: stacks_rpc.ok_or_else(|| "--stacks-rpc is required".to_owned())?,
            hacknet_commit: hacknet_commit
                .ok_or_else(|| "--hacknet-commit is required".to_owned())?,
            first_height: first_height.ok_or_else(|| "--first-height is required".to_owned())?,
            replay_blocks: replay_blocks.ok_or_else(|| "--replay-blocks is required".to_owned())?,
            checkpoint_height: checkpoint_height
                .ok_or_else(|| "--checkpoint-height is required".to_owned())?,
            bitcoin_magic: bitcoin_magic.unwrap_or_else(|| "T3".to_owned()),
        })
    }

    fn capture(&self, root: &Path) -> Result<(), String> {
        self.check_node_revision()?;
        if self.replay_blocks == 0 {
            return Err("--replay-blocks must be greater than zero".to_owned());
        }
        if self.checkpoint_height >= self.first_height {
            return Err("--checkpoint-height must precede --first-height".to_owned());
        }
        let node_root = self.node_root.clone().unwrap_or_else(|| {
            self.state_dir.join("stacks-miner-1/nakamoto-neon")
        });
        let blocks_db = node_root.join("chainstate/blocks/nakamoto.sqlite");
        let sortition_db = node_root.join("burnchain/sortition/marf.sqlite");
        let blocks = self.blocks(&blocks_db)?;
        // Rewriting the accounting alone is worth a mode of its own: it is a
        // couple of hundred queries against the archive where a full capture is
        // hours and hundreds of gigabytes. It returns before anything else is
        // read, written or cleared — a capture empties the fixture tree it
        // writes to, and re-exporting one file must never do that.
        if let Some(into) = &self.accounting_only {
            return Self::write_native_effects(
                &node_root.join("chainstate/vm/index.sqlite"),
                &blocks,
                into,
                self.accounting_network().boot_address(),
                self.pox_calendar()?.0,
            );
        }
        if blocks.len() != usize::try_from(self.replay_blocks).map_err(|error| error.to_string())? {
            return Err(format!(
                "requested {} blocks from height {}, but only {} canonical blocks are available",
                self.replay_blocks,
                self.first_height,
                blocks.len()
            ));
        }

        fs::create_dir_all(root).map_err(io_error("create capture output directory"))?;
        let staging = root.join(".capture-staging");
        if staging.exists() {
            fs::remove_dir_all(&staging).map_err(io_error("remove prior capture staging"))?;
        }
        fs::create_dir_all(&staging).map_err(io_error("create capture staging"))?;

        let result = self.write_capture(&staging, &blocks, &blocks_db, &sortition_db, &node_root);
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }

        Self::install_capture(root, &staging)?;
        println!(
            "captured {} real Nakamoto blocks with a portable MARF checkpoint",
            blocks.len()
        );
        Ok(())
    }

    fn blocks(&self, database: &Path) -> Result<Vec<CapturedBlock>, String> {
        let last_height = self
            .first_height
            .checked_add(self.replay_blocks)
            .ok_or_else(|| "height range overflow".to_owned())?;
        let query = format!(
            "select height, block_hash, consensus_hash, index_block_hash from nakamoto_staging_blocks where processed = 1 and orphaned = 0 and height >= {} and height < {} order by height",
            self.first_height, last_height
        );
        let output = sqlite(database, &query)?;
        output
            .lines()
            .filter(|line| !line.is_empty())
            .map(CapturedBlock::parse)
            .collect()
    }

    /// The burn heights the captured blocks span, widened by the window a
    /// commitment can reach back over.
    fn burn_span(sortition_db: &Path, blocks: &[CapturedBlock]) -> Result<(u64, u64), String> {
        let hashes = blocks
            .iter()
            .map(|block| format!("'{}'", block.consensus_hash))
            .collect::<Vec<_>>()
            .join(",");
        let output = sqlite(
            sortition_db,
            &format!(
                "select min(block_height), max(block_height) from snapshots where pox_valid = 1 and consensus_hash in ({hashes})"
            ),
        )?;
        let (first, last) = output
            .trim()
            .split_once('|')
            .ok_or_else(|| "the captured blocks belong to no burn block".to_owned())?;
        let first: u64 = first
            .trim()
            .parse()
            .map_err(|error| format!("unreadable first burn height: {error}"))?;
        let last: u64 = last
            .trim()
            .parse()
            .map_err(|error| format!("unreadable last burn height: {error}"))?;
        // A block commitment may reach back over the mining window, and the
        // replay reads the burn blocks either side of its own span.
        Ok((first.saturating_sub(12), last.saturating_add(1)))
    }

    /// Write each captured block, and its receipts when there are any.
    fn write_blocks(
        &self,
        staging: &Path,
        blocks: &[CapturedBlock],
        blocks_db: &Path,
    ) -> Result<(), String> {
        for block in blocks {
            let name = format!("{:08}-{}.bin", block.height, block.block_hash);
            let destination = staging.join("nakamoto/blocks").join(name);
            // The blocks are in the node's own database, so a capture reads
            // them there rather than asking the network for what it already
            // has — which also keeps a hundred-block window from being rate
            // limited by a public API.
            let encoded = sqlite(
                blocks_db,
                &format!(
                    "select lower(hex(data)) from nakamoto_staging_blocks where index_block_hash = '{}' and processed = 1 and orphaned = 0",
                    block.index_block_hash
                ),
            )?;
            let raw_block = decode_hex(encoded.trim())
                .ok_or_else(|| format!("block {} is not stored as bytes", block.block_hash))?;
            write_file(&destination, &raw_block)?;

            // Without an observer there are no receipts to carry; the manifest
            // records that, and the replay checks state roots only.
            if self.events_dir.is_some() {
                let event = self.event_for(&block.block_hash)?;
                let event_name = format!("{:08}-{}.json", block.height, block.block_hash);
                write_file(
                    &staging.join("events/new_block").join(event_name),
                    event.as_bytes(),
                )?;
            }
        }
        Ok(())
    }

    fn write_capture(
        &self,
        staging: &Path,
        blocks: &[CapturedBlock],
        blocks_db: &Path,
        sortition_db: &Path,
        node_root: &Path,
    ) -> Result<(), String> {
        // Only the burn window the captured blocks sit in, and only the
        // canonical snapshot at each height. A chain with forks has more than
        // one row per height, and a chain with a million burn blocks does not
        // want all of them in a fixture.
        let (first_burn, last_burn) = Self::burn_span(sortition_db, blocks)?;
        let snapshot_query = format!(
            "select block_height, burn_header_hash, sortition_id, parent_sortition_id, burn_header_timestamp, parent_burn_header_hash, consensus_hash, ops_hash, total_burn, sortition, sortition_hash, winning_block_txid, winning_stacks_block_hash, num_sortitions, stacks_block_accepted, stacks_block_height, arrival_index, canonical_stacks_tip_height, canonical_stacks_tip_hash, canonical_stacks_tip_consensus_hash, pox_valid, accumulated_coinbase_ustx, pox_payouts, miner_pk_hash from snapshots where pox_valid = 1 and block_height between {first_burn} and {last_burn} group by block_height order by block_height"
        );
        let snapshot_query = snapshot_query.as_str();
        let snapshots = sqlite_json(sortition_db, snapshot_query)?;
        let bitcoin_blocks = Self::bitcoin_blocks(&snapshots)?;

        self.write_blocks(staging, blocks, blocks_db)?;

        for bitcoin_block in bitcoin_blocks {
            let burn_hash = bitcoin_block.hash;
            let encoded = if let Some(rest) = self.bitcoin_rest.as_ref() {
                let raw = http_get(&format!("{}/block/{burn_hash}/raw", rest.trim_end_matches('/')))?;
                encode_hex(&raw)
            } else {
                let rpc = self
                    .bitcoin_rpc
                    .as_ref()
                    .ok_or_else(|| "--bitcoin-rpc or --bitcoin-rest is required".to_owned())?;
                let payload = format!(
                    "{{\"jsonrpc\":\"1.0\",\"id\":\"nano-stacks\",\"method\":\"getblock\",\"params\":[\"{burn_hash}\",0]}}"
                );
                json_result_string(&http_post(rpc, &payload)?)?
            };
            write_file(
                &staging
                    .join("bitcoin/blocks")
                    .join(format!("{burn_hash}.hex")),
                encoded.as_bytes(),
            )?;
        }

        write_file(
            &staging.join("sortition/snapshots.json"),
            snapshots.as_bytes(),
        )?;

        // The leader-key registry, without which no tenure's coinbase proof can
        // be checked at all: the registration a winning commitment names is
        // registered once and reused for years, so it sits far below the burn
        // span this capture holds. It is the cheapest thing in the capture — a
        // quarter of a megabyte for mainnet's entire history — and the
        // alternative is asking the peer that supplied the block for the input
        // that decides whether to believe it.
        let (keys, signing) = read_leader_keys(sortition_db, last_burn).and_then(|keys| {
            write_leader_keys(&staging.join("sortition").join(nano_node::sortition::LEADER_KEY_FILE), &keys)
        })?;
        println!(
            "captured {keys} leader-key registrations up to burn {last_burn}, {signing} with a \
             block-signing key hash"
        );

        self.write_stacker_sets(staging, &snapshots, blocks)?;

        let first_bitcoin_height =
            snapshots_by_consensus_hash(&snapshots, &blocks[0].consensus_hash)
                .ok_or_else(|| "captured first block has no sortition snapshot".to_owned())?;
        let checkpoint = Self::block_at_height(blocks_db, self.checkpoint_height)?;
        let checkpoint_root = self.checkpoint_root(&checkpoint)?;
        let checkpoint_dir = staging.join("chainstate/checkpoint-H");
        copy_clarity_source(&node_root.join("chainstate/vm/clarity"), &checkpoint_dir)?;
        Self::write_native_effects(
            &node_root.join("chainstate/vm/index.sqlite"),
            blocks,
            &checkpoint_dir,
            Network::from_chain_id(u32::try_from(self.chain_id()?).map_err(|error| {
                format!("captured node reports an out-of-range chain identifier: {error}")
            })?)
            .boot_address(),
            self.pox_calendar()?.0,
        )?;
        // The headers Clarity can read for everything below the anchor. Without
        // them a node that starts here answers `none` for the whole ancestry, or
        // asks a peer for five fields of thirteen and fills the rest with zeros
        // that read as the chain's answers.
        if let Err(error) = run_header_export(
            &node_root.join("chainstate/vm/index.sqlite"),
            &checkpoint_dir.join(nano_vm::HEADER_EXPORT_FILE),
            checkpoint.height,
        ) {
            return Err(format!("exporting the ancestry's headers failed: {error}"));
        }
        let checkpoint_manifest = format!(
            "format = \"stacks-core-marf-sqlite-v2\"\ncheckpoint_stacks_height = {}\nsource_state_id = \"{}\"\npublished_state_index_root = \"{}\"\nfirst_bitcoin_height = {}\n",
            checkpoint.height, checkpoint.index_block_hash, checkpoint_root, first_bitcoin_height
        );
        write_file(
            &checkpoint_dir.join("checkpoint.toml"),
            checkpoint_manifest.as_bytes(),
        )?;
        self.write_provenance(staging, blocks, &checkpoint, &checkpoint_root)?;
        write_file(
            &staging.join("manifest.toml"),
            format!(
                "mode = \"captured\"\nreplay_blocks = {}\nreceipts = {}\n",
                self.replay_blocks,
                self.events_dir.is_some()
            )
            .as_bytes(),
        )?;
        let block_hex = sqlite(
            blocks_db,
            &format!(
                "select hex(data) from nakamoto_staging_blocks where block_hash = '{}' and processed = 1 and orphaned = 0",
                blocks[0].block_hash
            ),
        )?;
        if block_hex.trim().is_empty() {
            return Err("captured block disappeared from the staging database".to_owned());
        }
        Ok(())
    }

    /// The reward a tenure earned: its recipient, its coinbase, and its anchored fees.
    ///
    /// Before Nakamoto a tenure was a single block, so its schedule is keyed by
    /// height rather than by a tenure event.
    fn scheduled_payment(
        chainstate_db: &Path,
        coinbase_height: u64,
    ) -> Result<Option<ScheduledPayment>, String> {
        if coinbase_height == 0 {
            return Ok(None);
        }
        let tenure = sqlite(
            chainstate_db,
            &format!(
                "SELECT block_id FROM nakamoto_tenure_events \
                 WHERE cause = 0 AND coinbase_height = {coinbase_height} LIMIT 1"
            ),
        )?;
        let selector = tenure.lines().next().map_or_else(
            || format!("stacks_block_height = {coinbase_height}"),
            |block_id| format!("index_block_hash = '{block_id}'"),
        );
        let payment = sqlite(
            chainstate_db,
            &format!(
                "SELECT COALESCE(recipient, address), coinbase, tx_fees_anchored, schedule_type \
                 FROM payments WHERE {selector} AND miner = 1 ORDER BY rowid LIMIT 1"
            ),
        )?;
        let Some(payment) = payment.lines().next() else {
            return Ok(None);
        };
        let mut fields = payment.split('|');
        let recipient = fields
            .next()
            .ok_or_else(|| "scheduled payment has no recipient".to_owned())?
            .to_owned();
        let coinbase = parse_u128("scheduled payment coinbase", fields.next())?;
        let anchored = parse_u128("scheduled payment anchored fees", fields.next())?;
        let nakamoto = fields.next() == Some("nakamoto");
        Ok(Some(ScheduledPayment {
            recipient,
            coinbase,
            anchored,
            nakamoto,
        }))
    }

    /// The network a re-export names, without asking a peer.
    ///
    /// A capture reads the chain identifier from the node it captures, but a
    /// re-export runs against an archive with no node behind it, and a rate
    /// limited peer is no reason to fail. The magic bytes already say which
    /// chain this is.
    fn accounting_network(&self) -> Network {
        if self.bitcoin_magic == "X2" {
            Network::MAINNET
        } else {
            Network::TESTNET
        }
    }

    fn write_native_effects(
        chainstate_db: &Path,
        blocks: &[CapturedBlock],
        checkpoint_dir: &Path,
        boot_address: &str,
        first_bitcoin_height: u64,
    ) -> Result<(), String> {
        let block_ids = blocks
            .iter()
            .map(|block| format!("'{}'", block.index_block_hash))
            .collect::<Vec<_>>()
            .join(",");
        // Every tenure the captured blocks *belong to*, not only the ones they
        // start. A window that opens part way through a tenure still executes
        // blocks of it, and each of those pays out the tenure a hundred back,
        // so a tenure whose own start block fell outside the window still owes.
        let span = sqlite(
            chainstate_db,
            &format!(
                "SELECT MIN(coinbase_height), MAX(coinbase_height) FROM nakamoto_tenure_events \
                 WHERE block_id IN ({block_ids})"
            ),
        )?;
        let (first, last) = span
            .lines()
            .next()
            .and_then(|line| line.split_once('|'))
            .ok_or_else(|| "the captured blocks belong to no tenure".to_owned())?;
        let first = parse_u64("first coinbase height", first)?;
        let last = parse_u64("last coinbase height", last)?;
        let mut effects = Vec::new();
        for coinbase_height in first..=last {
            let Some(earned) = Self::scheduled_payment(
                chainstate_db,
                coinbase_height.saturating_sub(MINER_REWARD_MATURITY),
            )?
            else {
                continue;
            };
            // A tenure's coinbase pays its own recipient and its anchored fees pay
            // the previous tenure's. Both are credited even when zero, because the
            // write itself is consensus state.
            // A Nakamoto tenure hands its anchored fees to the previous tenure;
            // before Nakamoto the miner kept them. Without a preceding tenure the
            // parent share still lands, on the boot address, because stacks-core
            // credits it unconditionally.
            let (own, parent) = if earned.nakamoto {
                (earned.coinbase, earned.anchored)
            } else {
                (earned.coinbase + earned.anchored, 0)
            };
            let previous = Self::scheduled_payment(
                chainstate_db,
                coinbase_height.saturating_sub(MINER_REWARD_MATURITY + 1),
            )?
            .map_or_else(|| boot_address.to_owned(), |payment| payment.recipient);
            effects.push(json!({
                "coinbase_height": coinbase_height,
                "credits": [
                    json!({ "recipient": earned.recipient, "amount": own }),
                    json!({ "recipient": previous, "amount": parent }),
                ],
                "liquid_supply_increase": earned.coinbase,
            }));
        }
        // The payouts above cover only the tenures the captured window happens
        // to touch. A node carries on past that window, and every tenure it
        // executes for the next hundred pays out one earned before the
        // checkpoint — which it cannot derive and the archive still holds. So
        // the earnings of that whole window travel with the checkpoint, and
        // `effects_for_tenure` derives the payouts from them.
        let mut tenures = Vec::new();
        // One tenure deeper than the payouts need, because the entry for a tenure
        // is only complete once the archive also holds its successor.
        let earliest = last.saturating_sub(MINER_REWARD_MATURITY + 2);
        for coinbase_height in earliest..=last {
            let Some(earned) = Self::scheduled_payment(chainstate_db, coinbase_height)? else {
                continue;
            };
            // A tenure's fee total is not in its own schedule. stacks-core can
            // only total it once the next tenure change proves the tenure over,
            // so it lands in the *following* tenure's schedule as `parent_fees`
            // — and a checkpoint that copies `anchored` from the same row it took
            // the recipient and coinbase from carries every tenure's fees under
            // its successor's name. That reads correctly for as long as the
            // checkpoint's own window lasts and then diverges by one tenure's
            // fees, which is how 8,673,846 was found.
            let Some(following) = Self::scheduled_payment(chainstate_db, coinbase_height + 1)?
            else {
                continue;
            };
            tenures.push(json!({
                "coinbase_height": coinbase_height,
                "recipient": earned.recipient,
                "coinbase": earned.coinbase,
                "fees": following.anchored,
            }));
        }
        let covered = u64::try_from(tenures.len()).unwrap_or(0);
        if covered <= MINER_REWARD_MATURITY {
            return Err(format!(
                "the archive holds {covered} of the {} tenures a checkpoint at coinbase height                  {last} needs: every tenure executed before nano's own mature pays out one of                  them, so a short window fails at the first payout it cannot derive",
                MINER_REWARD_MATURITY + 1
            ));
        }
        // Without the schedule a node cannot price the coinbase of a tenure it
        // executes itself, so the first tenure start past the checkpoint pays
        // nothing and its state root diverges.
        let schedule = json!({
            "mainnet": boot_address == Network::MAINNET.boot_address(),
            "first_bitcoin_height": first_bitcoin_height,
            "initial_mining_bonus_ustx": 0,
        });
        let contents = serde_json::to_vec_pretty(&json!({
            "matured_effects": effects,
            "tenures": tenures,
            "coinbase_schedule": schedule,
        }))
            .map_err(|error| format!("serialize native accounting: {error}"))?;
        write_file(&checkpoint_dir.join("native-effects.json"), &contents)
    }

    fn event_for(&self, block_hash: &str) -> Result<String, String> {
        let Some(events_dir) = self.events_dir.as_ref() else {
            return Err("no events directory was given".to_owned());
        };
        let needle = format!("\"block_hash\":\"0x{block_hash}\"");
        let mut candidates = fs::read_dir(events_dir.join("new_block"))
            .map_err(io_error("read new_block events"))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension() == Some(OsStr::new("json")));
        let path = candidates
            .find(|path| fs::read_to_string(path).is_ok_and(|event| event.contains(&needle)))
            .ok_or_else(|| format!("no new_block event captured for {block_hash}"))?;
        fs::read_to_string(path).map_err(io_error("read new_block event"))
    }

    fn block_at_height(database: &Path, height: u64) -> Result<CapturedBlock, String> {
        let query = format!(
            "select height, block_hash, consensus_hash, index_block_hash from nakamoto_staging_blocks where processed = 1 and orphaned = 0 and height = {height}"
        );
        let output = sqlite(database, &query)?;
        CapturedBlock::parse(output.trim())
    }

    fn bitcoin_blocks(snapshots: &str) -> Result<Vec<CapturedBitcoinBlock>, String> {
        let snapshots: Vec<serde_json::Value> = serde_json::from_str(snapshots)
            .map_err(|error| format!("could not parse captured Bitcoin snapshots: {error}"))?;
        let bitcoin_blocks = snapshots
            .iter()
            .map(CapturedBitcoinBlock::from_snapshot)
            .collect::<Result<Vec<_>, _>>()?;
        if bitcoin_blocks
            .windows(2)
            .any(|blocks| blocks[1].height != blocks[0].height.saturating_add(1))
        {
            return Err("captured Bitcoin blocks are not contiguous".to_owned());
        }
        Ok(bitcoin_blocks)
    }

    fn checkpoint_root(&self, checkpoint: &CapturedBlock) -> Result<String, String> {
        let raw_block = http_get(&format!(
            "{}/v3/blocks/{}",
            self.stacks_rpc, checkpoint.index_block_hash
        ))?;
        let root = raw_block.get(101..133).ok_or_else(|| {
            "checkpoint block is too short to contain a state index root".to_owned()
        })?;
        Ok(hex(root))
    }

    fn current_reward_cycle(&self) -> Result<u64, String> {
        let response =
            String::from_utf8(http_get(&format!("{}/v3/tenures/info", self.stacks_rpc))?)
                .map_err(|error| format!("tenures response was not UTF-8: {error}"))?;
        json_unsigned_field(&response, "reward_cycle")
    }

    /// The stacking calendar a replay needs to place a block in its reward cycle.
    /// Write the reward set of every cycle the captured window spans.
    ///
    /// A block is verified against the reward set of its own cycle, so a window
    /// long enough to cross a rollover needs more than the cycle it ends in.
    fn write_stacker_sets(
        &self,
        staging: &Path,
        snapshots: &str,
        blocks: &[CapturedBlock],
    ) -> Result<(), String> {
        let (first_pox, prepare, reward) = self.pox_calendar()?;
        let length = u64::from(prepare) + u64::from(reward);
        let cycle_at = |height: u64| height.saturating_sub(first_pox) / length.max(1);
        let first = snapshots_by_consensus_hash(snapshots, &blocks[0].consensus_hash)
            .ok_or_else(|| "captured first block has no sortition snapshot".to_owned())?;
        let last = blocks
            .last()
            .and_then(|block| snapshots_by_consensus_hash(snapshots, &block.consensus_hash))
            .unwrap_or(first);
        for cycle in cycle_at(first)..=cycle_at(last).max(self.current_reward_cycle()?) {
            let Ok(stacker_set) = http_get(&format!("{}/v3/stacker_set/{cycle}", self.stacks_rpc))
            else {
                continue;
            };
            write_file(
                &staging
                    .join("stacker_set")
                    .join(format!("cycle-{cycle}.json")),
                &stacker_set,
            )?;
        }
        Ok(())
    }

    fn pox_calendar(&self) -> Result<(u64, u32, u32), String> {
        let response = String::from_utf8(http_get(&format!("{}/v2/pox", self.stacks_rpc))?)
            .map_err(|error| format!("PoX response was not UTF-8: {error}"))?;
        Ok((
            json_unsigned_field(&response, "first_burnchain_block_height")?,
            u32::try_from(json_unsigned_field(
                &response,
                "prepare_phase_block_length",
            )?)
            .map_err(|error| error.to_string())?,
            u32::try_from(json_unsigned_field(&response, "reward_phase_block_length")?)
                .map_err(|error| error.to_string())?,
        ))
    }

    /// The stacks-core revision every conformance oracle compares against.
    ///
    /// Read from the lockfile rather than restated here, so it cannot drift
    /// from what the crates actually build against.
    fn pinned_stacks_core() -> Result<String, String> {
        let lockfile = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../Cargo.lock");
        let contents =
            fs::read_to_string(&lockfile).map_err(io_error("read the workspace lockfile"))?;
        contents
            .lines()
            .find_map(|line| {
                line.split_once("stacks-core.git?rev=")
                    .map(|(_, rest)| rest.split(['#', '"']).next().unwrap_or_default().to_owned())
            })
            .filter(|rev| !rev.is_empty())
            .ok_or_else(|| "the lockfile pins no stacks-core revision".to_owned())
    }

    /// Refuse a capture from a node that is not the revision the oracles use.
    ///
    /// A capture from a different build records what *that* build decided, and
    /// nothing says so afterwards: the fixtures then contradict every
    /// in-process comparison, and the contradiction reads as a nano bug.
    fn check_node_revision(&self) -> Result<(), String> {
        let pinned = Self::pinned_stacks_core()?;
        let response = String::from_utf8(http_get(&format!("{}/v2/info", self.stacks_rpc))?)
            .map_err(|error| format!("node information response was not UTF-8: {error}"))?;
        let version = response
            .split_once("\"server_version\":\"")
            .and_then(|(_, rest)| rest.split('"').next())
            .ok_or_else(|| "node reports no server version".to_owned())?
            .to_owned();
        if version.contains(&pinned[..pinned.len().min(7)]) {
            return Ok(());
        }
        if let Some(accepted) = self.accept_node_revision.as_ref() {
            if version.contains(accepted.as_str()) {
                return Ok(());
            }
            return Err(format!(
                "node reports {version:?}, which does not carry the accepted revision {accepted}"
            ));
        }
        Err(format!(
            "node reports {version:?} but the oracles compare against stacks-core {pinned}; \
             capturing from a different build produces fixtures that contradict them"
        ))
    }

    /// The chain identifier the captured node reports, which decides the network
    /// replay must execute the capture as.
    fn chain_id(&self) -> Result<u64, String> {
        let response = String::from_utf8(http_get(&format!("{}/v2/info", self.stacks_rpc))?)
            .map_err(|error| format!("node information response was not UTF-8: {error}"))?;
        json_unsigned_field(&response, "network_id")
    }

    /// The unlock heights a receipt-less capture has to record itself.
    fn unlock_height_lines(&self) -> String {
        self.unlock_heights.map_or_else(String::new, |heights| {
            format!(
                "\npox_v1_unlock_height = {}\npox_v2_unlock_height = {}\npox_v3_unlock_height = {}\npox_v4_unlock_height = {}",
                heights[0], heights[1], heights[2], heights[3]
            )
        })
    }

    fn write_provenance(
        &self,
        staging: &Path,
        blocks: &[CapturedBlock],
        checkpoint: &CapturedBlock,
        checkpoint_root: &str,
    ) -> Result<(), String> {
        let captured_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("system time precedes Unix epoch: {error}"))?
            .as_secs();
        let (pox_first_height, prepare_phase_length, reward_phase_length) = self.pox_calendar()?;
        let chain_id = self.chain_id()?;
        let magic = &self.bitcoin_magic;
        // The revision decides what "matches stacks-core" means, so a capture
        // that does not name it cannot be told apart from one that disagrees.
        let stacks_core_rev = self
            .accept_node_revision
            .clone()
            .map_or_else(Self::pinned_stacks_core, Ok)?;
        let unlock_heights = self.unlock_height_lines();
        let contents = format!(
            "source = \"hacknet\"\nhacknet_commit = \"{}\"\ncaptured_at_unix = {captured_at}\nchain_id = {chain_id}\nbitcoin_magic = \"{magic}\"\nstacks_core_rev = \"{stacks_core_rev}\"\ncheckpoint_stacks_height = {}\ncheckpoint_state_id = \"{}\"\ncheckpoint_state_index_root = \"{}\"\nfirst_stacks_height = {}\nreplay_blocks = {}\nbitcoin_rpc = \"{}\"\nstacks_rpc = \"{}\"\nfirst_block_hash = \"{}\"\nfirst_consensus_hash = \"{}\"\npox_first_bitcoin_height = {pox_first_height}\npox_prepare_phase_length = {prepare_phase_length}\npox_reward_phase_length = {reward_phase_length}{unlock_heights}\n",
            self.hacknet_commit,
            checkpoint.height,
            checkpoint.index_block_hash,
            checkpoint_root,
            self.first_height,
            self.replay_blocks,
            self.bitcoin_rpc.as_deref().unwrap_or_default(),
            self.stacks_rpc,
            blocks[0].block_hash,
            blocks[0].consensus_hash,
        );
        write_file(&staging.join("provenance.toml"), contents.as_bytes())
    }

    fn install_capture(root: &Path, staging: &Path) -> Result<(), String> {
        for relative in [
            "bitcoin",
            "nakamoto",
            "events",
            "sortition",
            "stacker_set",
            "chainstate",
        ] {
            let source = staging.join(relative);
            // A capture without an observer writes no events at all, so the
            // directory is simply absent rather than empty.
            if !source.exists() {
                continue;
            }
            let target = root.join(relative);
            if target.exists() {
                fs::remove_dir_all(&target).map_err(io_error("replace captured fixture data"))?;
            }
            fs::rename(source, target).map_err(io_error("install captured fixture data"))?;
        }
        let provenance = root.join("provenance.toml");
        if provenance.exists() {
            fs::remove_file(&provenance).map_err(io_error("replace fixture provenance"))?;
        }
        fs::rename(staging.join("provenance.toml"), provenance)
            .map_err(io_error("install fixture provenance"))?;
        fs::rename(staging.join("manifest.toml"), root.join("manifest.toml"))
            .map_err(io_error("install fixture manifest"))?;
        fs::remove_dir(staging).map_err(io_error("remove capture staging"))
    }
}

struct CapturedBlock {
    height: u64,
    block_hash: String,
    consensus_hash: String,
    index_block_hash: String,
}

struct CapturedBitcoinBlock {
    height: u64,
    hash: String,
}

impl CapturedBitcoinBlock {
    fn from_snapshot(snapshot: &serde_json::Value) -> Result<Self, String> {
        let height = snapshot
            .get("block_height")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| "missing Bitcoin block height from captured snapshot".to_owned())?;
        let hash = snapshot
            .get("burn_header_hash")
            .and_then(serde_json::Value::as_str)
            .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| "missing Bitcoin block hash from captured snapshot".to_owned())?
            .to_owned();
        Ok(Self { height, hash })
    }
}

impl CapturedBlock {
    fn parse(line: &str) -> Result<Self, String> {
        let mut fields = line.split('|');
        let height = fields
            .next()
            .ok_or_else(|| "missing block height from sqlite output".to_owned())
            .and_then(|value| parse_u64("height", value))?;
        let block_hash = fields
            .next()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "missing block hash from sqlite output".to_owned())?
            .to_owned();
        let consensus_hash = fields
            .next()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "missing consensus hash from sqlite output".to_owned())?
            .to_owned();
        let index_block_hash = fields
            .next()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "missing index block hash from sqlite output".to_owned())?
            .to_owned();
        if fields.next().is_some() {
            return Err("unexpected sqlite block fields".to_owned());
        }
        Ok(Self {
            height,
            block_hash,
            consensus_hash,
            index_block_hash,
        })
    }
}

fn parse_u64(flag: &str, value: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|error| format!("invalid {flag} value {value:?}: {error}"))
}

fn parse_u128(field: &str, value: Option<&str>) -> Result<u128, String> {
    value
        .ok_or_else(|| format!("{field} is missing"))?
        .parse()
        .map_err(|error| format!("invalid {field}: {error}"))
}

fn sqlite(database: &Path, query: &str) -> Result<String, String> {
    command_output(
        Command::new("sqlite3")
            .arg("-separator")
            .arg("|")
            .arg(database)
            .arg(query),
    )
}

/// Run a statement too long for an argument list.
///
/// A repaired ledger row is 67 KB of hexadecimal, and an `UPDATE` carrying it is
/// past `E2BIG` — the first version of `repair-ledger` failed on its first row
/// for exactly that. Fed on standard input there is no limit to hit.
fn sqlite_script(database: &Path, script: &str) -> Result<String, String> {
    use std::io::Write as _;

    let mut child = Command::new("sqlite3")
        .arg(database)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run sqlite3: {error}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "sqlite3 has no standard input".to_owned())?
        .write_all(script.as_bytes())
        .map_err(|error| format!("cannot write the statement: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("sqlite3 did not finish: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "sqlite3 refused the statement: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn sqlite_json(database: &Path, query: &str) -> Result<String, String> {
    command_output(
        Command::new("sqlite3")
            .arg("-json")
            .arg(database)
            .arg(query),
    )
}

/// Encode bytes as lowercase hexadecimal, as `getblock <hash> 0` returns them.
fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// Decode a lowercase hexadecimal string, as `SQLite`'s `hex()` produces.
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(text.get(index..index + 2)?, 16).ok())
        .collect()
}

fn http_get(url: &str) -> Result<Vec<u8>, String> {
    command_output_bytes(
        Command::new("curl")
            .arg("--fail")
            .arg("--silent")
            .arg("--show-error")
            .arg(url),
    )
}

fn http_post(url: &str, payload: &str) -> Result<String, String> {
    command_output(
        Command::new("curl")
            .arg("--fail")
            .arg("--silent")
            .arg("--show-error")
            .arg("--user")
            .arg("hacknet:hacknet")
            .arg("--data-binary")
            .arg(payload)
            .arg(url),
    )
}

fn command_output(command: &mut Command) -> Result<String, String> {
    String::from_utf8(command_output_bytes(command)?)
        .map_err(|error| format!("command output was not UTF-8: {error}"))
}

fn command_output_bytes(command: &mut Command) -> Result<Vec<u8>, String> {
    let output = command
        .output()
        .map_err(|error| format!("could not run {command:?}: {error}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "{command:?} exited {}; {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn write_file(path: &Path, contents: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("fixture output has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).map_err(io_error("create fixture output directory"))?;
    fs::write(path, contents).map_err(io_error("write fixture output"))
}

fn copy_clarity_source(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(io_error("create raw chainstate directory"))?;
    let database = source.join("marf.sqlite");
    let backup = destination.join("marf.sqlite");
    command_output(
        Command::new("sqlite3")
            .arg(&database)
            .arg(format!(".backup {}", backup.display())),
    )?;
    fs::copy(
        source.join("marf.sqlite.blobs"),
        destination.join("marf.sqlite.blobs"),
    )
    .map_err(io_error("copy raw chainstate blobs"))?;
    Ok(())
}

/// The Bitcoin height of the sortition that a consensus hash identifies.
fn snapshots_by_consensus_hash(snapshots: &str, consensus_hash: &str) -> Option<u64> {
    let needle = format!("\"consensus_hash\":\"{consensus_hash}\"");
    let entry = snapshots
        .split("},")
        .find(|entry| entry.contains(&needle))?;
    let marker = "\"block_height\":";
    let start = entry.find(marker)? + marker.len();
    let rest = &entry[start..];
    let end = rest.find(|character: char| !character.is_ascii_digit())?;
    rest[..end].parse().ok()
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push(char::from(DIGITS[usize::from(byte >> 4)]));
        value.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    value
}

fn json_result_string(response: &str) -> Result<String, String> {
    let marker = "\"result\":\"";
    let start = response
        .find(marker)
        .map(|index| index + marker.len())
        .ok_or_else(|| "Bitcoin RPC response did not contain a string result".to_owned())?;
    let end = response[start..]
        .find('"')
        .map(|index| start + index)
        .ok_or_else(|| "Bitcoin RPC result string was unterminated".to_owned())?;
    Ok(response[start..end].to_owned())
}

fn json_unsigned_field(response: &str, field: &str) -> Result<u64, String> {
    let marker = format!("\"{field}\":");
    let start = response
        .find(&marker)
        .map(|index| index + marker.len())
        .ok_or_else(|| format!("JSON response is missing {field}"))?;
    let value = response[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    parse_u64(field, &value)
}

fn io_error(context: &'static str) -> impl FnOnce(std::io::Error) -> String {
    move |error| format!("could not {context}: {error}")
}

/// Compile one contract against a node's own state and say whether it loads.
///
/// A contract that compiles to wasm the runtime refuses cannot be reproduced
/// from its source alone — analysing it needs every contract it references, and
/// for a mainnet aggregator that is hundreds. A node already has them, so this
/// borrows its state rather than rebuilding a fixture of it, which also makes
/// bisecting an edited source a matter of seconds instead of a replay.
fn check_module(arguments: &[String]) -> ExitCode {
    let ([state, contract], rest) = match arguments {
        [state, contract, rest @ ..] if rest.len() <= 2 => ([state, contract], rest),
        _ => {
            eprintln!(
                "usage: cargo xtask check-module <state-dir> <contract-id> [clarity-version] [source-file]\n\
                 without either the state's own version and source are used\n\
                 the node must not be running: it holds the state open"
            );
            return ExitCode::FAILURE;
        }
    };
    let asked_version = match rest.first().map(|text| clarity_version(text)) {
        Some(Some(version)) => Some(version),
        Some(None) => {
            eprintln!("{} is not a Clarity version", rest[0]);
            return ExitCode::FAILURE;
        }
        None => None,
    };
    let Ok(identifier) = clarity::vm::types::QualifiedContractIdentifier::parse(contract) else {
        eprintln!("{contract} is not a contract identifier");
        return ExitCode::FAILURE;
    };
    let mut vm = match nano_vm::Vm::open(Network::MAINNET, Path::new(state).join("chainstate")) {
        Ok(vm) => vm,
        Err(error) => {
            eprintln!("cannot open the state: {error:?}");
            return ExitCode::FAILURE;
        }
    };
    let Some(tip) = vm.tip() else {
        eprintln!("the state is sealed at no block, so there is nothing to compile against");
        return ExitCode::FAILURE;
    };
    if let Err(error) = vm.begin_block(Some(tip), [0xcc; 32]) {
        eprintln!("cannot begin a block on the tip: {error:?}");
        return ExitCode::FAILURE;
    }
    // A source file is asked for only when the state does not have one -- a
    // deployment that *failed* leaves no contract behind, so the block is the
    // only place its source exists. So the state is consulted for its version,
    // not required to answer.
    let stored = vm.contract_source(&identifier).ok();
    if let (Some((_, stored_version)), Some(asked)) = (stored.as_ref(), asked_version)
        && *stored_version != asked
    {
        println!("the state holds {contract} as {stored_version:?}, not {asked:?}");
    }
    let source = if let Some(path) = rest.get(1) {
        match fs::read_to_string(path) {
            Ok(source) => source,
            Err(error) => {
                eprintln!("cannot read the source: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        let Some((source, _)) = stored.as_ref() else {
            eprintln!(
                "the state has no source for {contract}, so one has to be given: \
                 `xtask decode-blocks` with NANO_DUMP_DEPLOYS writes out the source \
                 of every deployment in a block"
            );
            return ExitCode::FAILURE;
        };
        source.clone()
    };
    let Some(version) = asked_version.or_else(|| stored.as_ref().map(|(_, version)| *version))
    else {
        eprintln!("neither the state nor the arguments say which Clarity version to use");
        return ExitCode::FAILURE;
    };
    if let Some(path) = env::var_os("NANO_DUMP_SOURCE")
        && let Err(error) = fs::write(&path, &source)
    {
        eprintln!("cannot write the source: {error}");
    }

    match vm.check_module(
        &identifier,
        version,
        &source,
        clarity::types::StacksEpochId::Epoch40,
    ) {
        Ok(()) => {
            println!("{contract} compiles to a module that loads");
            ExitCode::SUCCESS
        }
        Err(error) => {
            println!("{contract}: {error:?}");
            dump_refused_wasm();
            ExitCode::FAILURE
        }
    }
}

const fn clarity_version(text: &str) -> Option<clarity::vm::ClarityVersion> {
    match text.as_bytes() {
        b"1" => Some(clarity::vm::ClarityVersion::Clarity1),
        b"2" => Some(clarity::vm::ClarityVersion::Clarity2),
        b"3" => Some(clarity::vm::ClarityVersion::Clarity3),
        b"4" => Some(clarity::vm::ClarityVersion::Clarity4),
        b"5" => Some(clarity::vm::ClarityVersion::Clarity5),
        b"6" => Some(clarity::vm::ClarityVersion::Clarity6),
        _ => None,
    }
}

/// Disassemble a module the runtime refused, which is the only readable form.
fn dump_refused_wasm() {
    let Some(path) = env::var_os("NANO_DUMP_REFUSED_WASM") else {
        return;
    };
    match fs::read(&path).map(wasmprinter::print_bytes) {
        Ok(Ok(text)) => {
            let text_path = Path::new(&path).with_extension("wat");
            if let Err(error) = fs::write(&text_path, text) {
                eprintln!("cannot write the disassembly: {error}");
            } else {
                println!("disassembled to {}", text_path.display());
            }
        }
        Ok(Err(error)) => eprintln!("cannot disassemble the module: {error}"),
        Err(error) => eprintln!("cannot read the module: {error}"),
    }
}

/// Copy a node's state directory in seconds, so an experiment is reversible.
///
/// This is the feedback loop, not a convenience. Importing a mainnet checkpoint
/// costs about four and a half hours, and until now that was the price of any
/// change that turned out to corrupt a state — so the cautious move was to not
/// try things, which is the worst possible effect on a divergence hunt.
///
/// On a copy-on-write filesystem the copy is a reflink: 33 GB in three seconds,
/// sharing every block until one side writes. Refused rather than silently
/// downgraded when the filesystem cannot do it, because a fallback that quietly
/// costs 33 GB and several minutes is a decision the operator should make.
fn snapshot_state(arguments: &[String]) -> ExitCode {
    let [source, destination] = arguments else {
        eprintln!(
            "usage: cargo xtask snapshot-state <state-dir> <destination>\n\
             the node must not be running: a half-written sqlite page copies as one"
        );
        return ExitCode::FAILURE;
    };
    let (source, destination) = (Path::new(source), Path::new(destination));
    if !source.join("chainstate").is_dir() {
        eprintln!("{} does not hold a chainstate", source.display());
        return ExitCode::FAILURE;
    }
    if destination.exists() {
        eprintln!(
            "{} already exists; a snapshot never overwrites one",
            destination.display()
        );
        return ExitCode::FAILURE;
    }
    // A `-wal` beside the database means the node is running or was killed, and
    // either way the copy would be of a state nothing has committed.
    for stray in ["chainstate/marf.sqlite-wal", "chainstate/clarity.sqlite-wal"] {
        let path = source.join(stray);
        if path.metadata().is_ok_and(|data| data.len() > 0) {
            eprintln!(
                "{} has uncommitted pages: stop the node before snapshotting it",
                path.display()
            );
            return ExitCode::FAILURE;
        }
    }
    let status = Command::new("cp")
        .arg("-a")
        .arg("--reflink=always")
        .arg(source)
        .arg(destination)
        .status();
    match status {
        Ok(status) if status.success() => {
            println!("{} is now a snapshot of {}", destination.display(), source.display());
            println!(
                "it shares every block with the original until one of them is written to, \
                 so it costs no disk until it diverges"
            );
            ExitCode::SUCCESS
        }
        Ok(_) => {
            eprintln!(
                "this filesystem cannot reflink, so the copy would be a real 33 GB one. \
                 Run `cp -a` yourself if that is what you want."
            );
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("cannot run cp: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Read what a state holds for one Clarity key at one block.
///
/// A root divergence whose receipts all match is a wrong *value* somewhere, and
/// the write trace only shows the 40-byte MARF value. This resolves the value
/// itself, and at the parent as well as at the tip, which is what turns "these
/// two roots differ" into "this balance is wrong by this much".
fn state_value(arguments: &[String]) -> ExitCode {
    let [state, block, key] = arguments else {
        eprintln!(
            "usage: cargo xtask state-value <state-dir> <block-id|tip> <clarity-key>\n\
             the node must not be running: it holds the state open"
        );
        return ExitCode::FAILURE;
    };
    let store = match nano_vm::MarfStore::open(Network::MAINNET, Path::new(state).join("chainstate"))
    {
        Ok(store) => store,
        Err(error) => {
            eprintln!("cannot open the state: {error:?}");
            return ExitCode::FAILURE;
        }
    };
    let resolved = if block == "tip" {
        store.tip()
    } else {
        hex::decode(block)
            .ok()
            .and_then(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).ok())
    };
    let Some(block) = resolved else {
        eprintln!("the block must be 32 hexadecimal bytes, or `tip` on a sealed state");
        return ExitCode::FAILURE;
    };
    let Some(value) = store.get(block, key) else {
        println!("no value for {key} at {}", hex::encode(block));
        return ExitCode::FAILURE;
    };
    println!("{value}");
    describe_stored_value(&value);
    ExitCode::SUCCESS
}

/// Say what a stored value means, for the two shapes a divergence lands in.
///
/// `STXBalance` is 16 bytes unlocked, 16 locked and 8 unlock height; a Clarity
/// `uint` is a `0x01` tag and 16 big-endian bytes. Everything else is left as
/// the hex it is.
fn describe_stored_value(value: &str) {
    let Ok(bytes) = hex::decode(value) else {
        return;
    };
    let be = |slice: &[u8]| -> u128 {
        slice
            .iter()
            .fold(0_u128, |total, byte| (total << 8) | u128::from(*byte))
    };
    match bytes.len() {
        40 => println!(
            "  balance: unlocked {} locked {} unlock height {}",
            be(&bytes[0..16]),
            be(&bytes[16..32]),
            be(&bytes[32..40])
        ),
        17 if bytes[0] == 1 => println!("  uint: {}", be(&bytes[1..17])),
        _ => {}
    }
}

/// Recompute every tenure's fees from the blocks themselves.
///
/// Tenure accounting lives outside the MARF, so a state root proves nothing
/// about it. A node that retried a block it could not execute added that
/// block's fees again on every attempt ([[056-roll-back-what-a-rejected-block-touched]]),
/// and the only way to know which tenures that reached is to count them again
/// from the chain: walk back from the tip block by block, start a new tenure
/// wherever the consensus hash changes, and sum what each block's transactions
/// paid.
fn rebuild_accounting(arguments: &[String]) -> ExitCode {
    let [state, peer, tip, tenure_height] = arguments else {
        eprintln!(
            "usage: cargo xtask rebuild-accounting <state-dir> <peer-url> <tip-block-id> \
             <tip-tenure-height>"
        );
        return ExitCode::FAILURE;
    };
    let (Ok(tip), Ok(tenure_height)) = (hex::decode(tip), tenure_height.parse::<u64>()) else {
        eprintln!("the tip must be hexadecimal and the tenure height a number");
        return ExitCode::FAILURE;
    };
    let Ok(tip) = <[u8; 32]>::try_from(tip.as_slice()) else {
        eprintln!("the tip must be 32 bytes");
        return ExitCode::FAILURE;
    };

    let path = Path::new(state).join("chainstate").join("accounting.json");
    let Ok(bytes) = fs::read(&path) else {
        eprintln!("cannot read {}", path.display());
        return ExitCode::FAILURE;
    };
    let Ok(mut accounting) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        eprintln!("cannot parse {}", path.display());
        return ExitCode::FAILURE;
    };
    let Some(tenures) = accounting.get("tenures").and_then(serde_json::Value::as_array) else {
        eprintln!("the accounting names no tenures");
        return ExitCode::FAILURE;
    };
    let oldest = tenures
        .iter()
        .filter_map(|tenure| tenure.get("coinbase_height")?.as_u64())
        .min()
        .unwrap_or(tenure_height);

    let counted = match count_fees(peer, tip, tenure_height, oldest) {
        Ok(counted) => counted,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };

    let mut corrected = 0;
    if let Some(tenures) = accounting.get_mut("tenures").and_then(serde_json::Value::as_array_mut) {
        for tenure in tenures.iter_mut() {
            let Some(height) = tenure.get("coinbase_height").and_then(serde_json::Value::as_u64) else {
                continue;
            };
            // A tenure the walk did not reach in full is left alone: counting
            // part of one would replace a wrong number with another.
            let Some(fees) = counted.get(&height) else {
                continue;
            };
            let recorded = tenure.get("fees").and_then(serde_json::Value::as_u64).unwrap_or(0);
            if recorded != *fees {
                println!("tenure {height}: {recorded} recorded, {fees} counted");
                tenure["fees"] = serde_json::json!(fees);
                corrected += 1;
            }
        }
    }

    if corrected == 0 {
        println!("every tenure the walk covered already agrees with the chain");
        return ExitCode::SUCCESS;
    }
    let backup = path.with_extension("json.before-rebuild");
    if let Err(error) = fs::copy(&path, &backup).and_then(|_| {
        fs::write(
            &path,
            serde_json::to_vec(&accounting).unwrap_or_default(),
        )
    }) {
        eprintln!("cannot write the corrected accounting: {error}");
        return ExitCode::FAILURE;
    }
    println!(
        "corrected {corrected} tenures; the previous file is at {}",
        backup.display()
    );
    ExitCode::SUCCESS
}

/// Walk back from `tip`, summing each tenure's transaction fees.
fn count_fees(
    peer: &str,
    tip: [u8; 32],
    tenure_height: u64,
    oldest: u64,
) -> Result<std::collections::BTreeMap<u64, u64>, String> {
    let url = peer.parse().map_err(|_| format!("{peer} is not a URL"))?;
    let client = nano_sync::SyncClient::new(url).map_err(|error| format!("{error}"))?;
    let runtime = tokio::runtime::Runtime::new().map_err(|error| format!("{error}"))?;

    let mut fees = std::collections::BTreeMap::new();
    let mut block_id = nano_primitives::StacksBlockId::from_bytes(tip);
    let mut height = tenure_height;
    let mut consensus = None;
    runtime.block_on(async {
        while height >= oldest {
            // A public peer rate-limits a walk this long, and being turned away
            // is not a reason to give up on a repair that has to be complete to
            // be worth anything.
            let mut block = Err(String::new());
            for attempt in 0..8u32 {
                match client.block(block_id).await {
                    Ok(fetched) => {
                        block = Ok(fetched);
                        break;
                    }
                    Err(error) => {
                        block = Err(format!("cannot fetch {block_id}: {error}"));
                        tokio::time::sleep(std::time::Duration::from_secs(u64::from(
                            2 + attempt * 3,
                        )))
                        .await;
                    }
                }
            }
            let block = block?;
            // The consensus hash changing is the tenure changing: walking back,
            // that means the block before belongs to the tenure before.
            let this = block.header.consensus_hash;
            if consensus.is_some_and(|seen| seen != this) {
                height = height.saturating_sub(1);
                if height < oldest {
                    break;
                }
            }
            if consensus != Some(this) {
                // A walk of ~200 tenures against a rate-limited peer takes
                // hours, and without this it is indistinguishable from a hang —
                // which is exactly how one run was left going for 1h45m.
                println!(
                    "tenure {height}: {} counted, {} to go",
                    fees.len(),
                    height.saturating_sub(oldest)
                );
            }
            consensus = Some(this);
            *fees.entry(height).or_insert(0u64) += block_fees(&block);
            let parent = block.header.parent_block_id;
            if parent == block_id {
                break;
            }
            block_id = parent;
        }
        Ok::<_, String>(())
    })?;
    // The tenure the walk stopped inside was only partly counted.
    fees.remove(&height);
    Ok(fees)
}

fn block_fees(block: &nano_chainstate::NakamotoBlock) -> u64 {
    block
        .transactions
        .iter()
        .map(|transaction| transaction.auth().payer().fee())
        .sum()
}

/// The placeholder a block is executed under before it is sealed.
fn temporary_state_id() -> [u8; 32] {
    *nano_primitives::sha512_256(&[1; 52]).as_bytes()
}

/// Ask which single write, if any, stands between nano's root and the chain's.
///
/// A block whose receipts all match the network and whose state root does not
/// has either written something the network did not, or written the same things
/// in another order. The first is cheap to settle: seal the block again with
/// each write left out in turn, and see whether any of them lands on the root
/// the header commits to.
///
/// The writes come from a `NANO_TRACE_WRITES` log, which records them in the
/// order they were made — which is the order that matters, since a trie packs a
/// node's pointers as its children first arrive.
fn probe_root(arguments: &[String]) -> ExitCode {
    let [state, parent, expected, writes] = arguments else {
        eprintln!(
            "usage: cargo xtask probe-root <state-dir> <parent-block-id> <expected-root> \
             <trace-file>"
        );
        return ExitCode::FAILURE;
    };
    let decode32 = |value: &str| -> Option<[u8; 32]> {
        <[u8; 32]>::try_from(hex::decode(value).ok()?.as_slice()).ok()
    };
    let (Some(parent), Some(expected)) = (decode32(parent), decode32(expected)) else {
        eprintln!("the parent and the expected root must each be 32 hexadecimal bytes");
        return ExitCode::FAILURE;
    };
    let Ok(trace) = fs::read_to_string(writes) else {
        eprintln!("cannot read the trace");
        return ExitCode::FAILURE;
    };

    // One block's writes: from the first `block_time` to the next, which is
    // where the node gave up and started the block again.
    let mut pairs: Vec<(String, [u8; 40])> = Vec::new();
    let mut started = false;
    for line in trace.lines() {
        let Some(rest) = line.strip_prefix("write ") else {
            continue;
        };
        let Some((key, value)) = rest.split_once(" = ") else {
            continue;
        };
        if key.ends_with("clarity_storage::block_time") {
            if started {
                break;
            }
            started = true;
        }
        let Ok(bytes) = hex::decode(value) else {
            continue;
        };
        let Ok(value) = <[u8; 40]>::try_from(bytes.as_slice()) else {
            continue;
        };
        pairs.push((key.to_owned(), value));
    }
    if pairs.is_empty() {
        eprintln!("the trace holds no writes");
        return ExitCode::FAILURE;
    }
    println!("{} writes traced for the block", pairs.len());

    let marf_path = Path::new(state).join("chainstate").join("marf.sqlite");
    let mut distinct: Vec<String> = Vec::new();
    for (key, _) in &pairs {
        if !distinct.contains(key) {
            distinct.push(key.clone());
        }
    }
    println!("{} distinct keys", distinct.len());

    // A block writes its own identifier into the trie, so this has to seal to
    // the real one — sealing to a stand-in gives a root that cannot be compared
    // with anything. Each attempt therefore rolls its block back.
    let seal = |omit: Option<&str>| -> Option<[u8; 32]> {
        let mut marf = nano_marf::VersionedMarf::open(&marf_path).ok()?;
        // The node executes under a placeholder identifier and renames the
        // block when it seals, so the identifier the trie carries during
        // execution is that placeholder, not the block's own.
        marf.begin(Some(parent), temporary_state_id()).ok()?;
        for (key, value) in &pairs {
            if omit == Some(key.as_str()) {
                continue;
            }
            marf.insert(key.as_bytes(), nano_marf::MarfValue::from_bytes(*value))
                .ok()?;
        }
        // Read the root without sealing: a probe must not leave blocks behind
        // in the state it is asking about.
        let root = marf.pending_root().ok()?;
        marf.abort().ok()?;
        Some(*root.as_bytes())
    };

    match seal(None) {
        Some(root) if root == expected => {
            println!("the traced writes already seal the expected root");
            return ExitCode::SUCCESS;
        }
        Some(root) => println!("all writes:      {}", hex::encode(root)),
        None => {
            eprintln!("cannot seal the block from the trace");
            return ExitCode::FAILURE;
        }
    }

    for key in &distinct {
        if seal(Some(key)) == Some(expected) {
            println!("without {key}: the expected root");
            return ExitCode::SUCCESS;
        }
    }
    println!("no single omitted write reaches it");

    if probe_orders(&marf_path, parent, &pairs, &distinct, expected) {
        return ExitCode::SUCCESS;
    }
    println!("no order of the traced writes reaches it either");
    ExitCode::SUCCESS
}

/// Try the same keys and values in several orders.
///
/// A trie packs a node's pointers as its children first arrive, so order is
/// consensus — but only for keys that share a node, which thirty scattered
/// paths in a chain this size rarely do.
fn probe_orders(
    marf_path: &Path,
    parent: [u8; 32],
    pairs: &[(String, [u8; 40])],
    distinct: &[String],
    expected: [u8; 32],
) -> bool {
    let final_value = |key: &str| -> [u8; 40] {
        pairs
            .iter()
            .rev()
            .find(|(candidate, _)| candidate == key)
            .map_or([0; 40], |(_, value)| *value)
    };
    let seal_order = |order: &[String]| -> Option<[u8; 32]> {
        let mut marf = nano_marf::VersionedMarf::open(marf_path).ok()?;
        marf.begin(Some(parent), temporary_state_id()).ok()?;
        for key in order {
            marf.insert(
                key.as_bytes(),
                nano_marf::MarfValue::from_bytes(final_value(key)),
            )
            .ok()?;
        }
        let root = marf.pending_root().ok()?;
        marf.abort().ok()?;
        Some(*root.as_bytes())
    };

    let mut by_key = distinct.to_vec();
    by_key.sort();
    let mut by_path = distinct.to_vec();
    by_path.sort_by_key(|key| *nano_marf::key_path(key.as_bytes()).as_bytes());
    let mut reversed = distinct.to_vec();
    reversed.reverse();
    for (name, order) in [
        ("as traced", distinct.to_vec()),
        ("sorted by key", by_key),
        ("sorted by trie path", by_path),
        ("traced, reversed", reversed),
    ] {
        match seal_order(&order) {
            Some(root) if root == expected => {
                println!("{name}: the expected root");
                return true;
            }
            Some(root) => println!("{name}: {}", hex::encode(root)),
            None => println!("{name}: cannot be sealed"),
        }
    }
    false
}

/// Read a call argument written as `u123`, `SP....name`, or raw hexadecimal.
///
/// Hand-encoding a contract principal is a c32 checksum away from being wrong in
/// a way that looks like a missing contract, so the same parser Clarity uses
/// does it here.
fn clarity_argument(text: &str) -> Option<Vec<u8>> {
    use clarity::vm::Value;
    if let Some(number) = text.strip_prefix('u') {
        return Value::UInt(number.parse().ok()?).serialize_to_vec().ok();
    }
    if text.contains('.') {
        let contract = clarity::vm::types::QualifiedContractIdentifier::parse(text).ok()?;
        return Value::Principal(contract.into()).serialize_to_vec().ok();
    }
    hex::decode(text).ok()
}

/// Run one contract call through both engines and print what each answered.
///
/// The interpreter is what mainnet runs, so a call the two answer differently
/// names clarity-wasm without any argument about state — and against a real
/// chainstate it can be a read-only function of a contract that is only
/// reachable through half a dozen others.
fn call_both(arguments: &[String]) -> ExitCode {
    // An optional leading `--sender <principal>`: a swap called by the wrong
    // sender fails on a balance long before it reaches anything worth
    // comparing, and the sender that matters is usually a contract.
    let (sender, arguments) = match arguments {
        [flag, principal, rest @ ..] if flag == "--sender" => (Some(principal.clone()), rest),
        _ => (None, arguments),
    };
    let [state, contract, function, rest @ ..] = arguments else {
        eprintln!(
            "usage: cargo xtask call-both <state-dir> <contract-id> <function> [hex-argument...]\n\
             arguments are consensus-serialized Clarity values in hexadecimal"
        );
        return ExitCode::FAILURE;
    };
    let Ok(identifier) = clarity::vm::types::QualifiedContractIdentifier::parse(contract) else {
        eprintln!("{contract} is not a contract identifier");
        return ExitCode::FAILURE;
    };
    let mut encoded = Vec::new();
    for argument in rest {
        let Some(bytes) = clarity_argument(argument) else {
            eprintln!("{argument} is not a uint, a contract principal, or hexadecimal");
            return ExitCode::FAILURE;
        };
        encoded.push(bytes);
    }

    let mut vm = match nano_vm::Vm::open(Network::MAINNET, Path::new(state).join("chainstate")) {
        Ok(vm) => vm,
        Err(error) => {
            eprintln!("cannot open the state: {error:?}");
            return ExitCode::FAILURE;
        }
    };
    let Some(tip) = vm.tip() else {
        eprintln!("the state is sealed at no block");
        return ExitCode::FAILURE;
    };

    let caller = match sender.as_deref() {
        Some(text) if text.contains('.') => {
            clarity::vm::types::QualifiedContractIdentifier::parse(text)
                .map_or_else(|_| identifier.issuer.clone().into(), Into::into)
        }
        Some(text) => clarity::vm::types::PrincipalData::parse(text)
            .unwrap_or_else(|_| identifier.issuer.clone().into()),
        None => identifier.issuer.clone().into(),
    };
    match ask_both_engines(&mut vm, tip, &caller, &identifier, function, &encoded) {
        Ok([compiler, interpreter]) => {
            println!("compiler     {compiler}");
            println!("interpreter  {interpreter}");
            if compiler == interpreter {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// Ask both engines the same contract call and print what each answered.
///
/// Every run opens its own block on `tip` and aborts it, so neither engine sees
/// the other's writes and the state is untouched.
fn ask_both_engines(
    vm: &mut nano_vm::Vm,
    tip: [u8; 32],
    sender: &clarity::vm::types::PrincipalData,
    contract: &clarity::vm::types::QualifiedContractIdentifier,
    function: &str,
    encoded: &[Vec<u8>],
) -> Result<[String; 2], String> {
    let mut answers = [String::new(), String::new()];
    for (slot, interpreted) in answers.iter_mut().zip([false, true]) {
        // A write trace is only readable if it says which engine wrote it, and
        // the first write the two make differently names the contract they
        // diverge in — which the returned value alone never does.
        println!(
            "--- {} {contract}::{function}",
            if interpreted { "interpreter" } else { "compiler" }
        );
        vm.begin_block(Some(tip), [0xca; 32])
            .map_err(|error| format!("cannot begin a block: {error:?}"))?;
        let free = clarity::vm::costs::LimitedCostTracker::new_free();
        let outcome = if interpreted {
            nano_oracle::interpret_contract_call(
                vm,
                nano_oracle::ContractCall {
                    sender: sender.clone(),
                    sponsor: None,
                    contract: contract.clone(),
                    function,
                    arguments: encoded,
                },
                free,
            )
        } else {
            vm.execute_contract_call_outcome(
                sender.clone(),
                None,
                contract.clone(),
                function,
                encoded,
                &free,
            )
        };
        *slot = match &outcome {
            Ok(
                nano_vm::ContractCallOutcome::Success(result)
                | nano_vm::ContractCallOutcome::AbortedByResponse(result),
            ) => format!("{:?}", result.value),
            Ok(nano_vm::ContractCallOutcome::RuntimeFailure { error, .. }) => {
                format!("failed: {error:?}")
            }
            Err(error) => format!("error: {error:?}"),
        };
        drop(vm.abort_block());
    }
    Ok(answers)
}

/// Replay the staged child of a state's tip through both engines, call by call.
///
/// This is the tool the two compiler bugs were found without: a divergence stops
/// the node on a *transaction*, whose arguments are already in the block, and
/// hand-serializing them back into `call-both` is where the hours went. Here the
/// block is read from staging, so the call is exactly the one that diverged.
///
/// Rolled back, and read-only by construction — nothing is ever sealed.
fn call_both_tx(arguments: &[String]) -> ExitCode {
    let (only, arguments) = match arguments {
        [flag, txid, rest @ ..] if flag == "--txid" => (Some(txid.to_lowercase()), rest),
        _ => (None, arguments),
    };
    let [state] = arguments else {
        eprintln!(
            "usage: cargo xtask call-both-tx [--txid <txid>] <state-dir>\n\
             replays the staged block above the tip; the node must not be running"
        );
        return ExitCode::FAILURE;
    };
    let chainstate = Path::new(state).join("chainstate");
    let mut vm = match nano_vm::Vm::open(Network::MAINNET, &chainstate) {
        Ok(vm) => vm,
        Err(error) => {
            eprintln!("cannot open the state: {error:?}");
            return ExitCode::FAILURE;
        }
    };
    let Some(tip) = vm.tip() else {
        eprintln!("the state is sealed at no block");
        return ExitCode::FAILURE;
    };
    let staging = match nano_node::staging::Staging::open(&chainstate.join("staging.sqlite")) {
        Ok(staging) => staging,
        Err(error) => {
            eprintln!("cannot open the staged blocks: {error:?}");
            return ExitCode::FAILURE;
        }
    };
    let block = match staging.child_of(nano_primitives::StacksBlockId::from_bytes(tip)) {
        Ok(Some(block)) => block,
        Ok(None) => {
            eprintln!("nothing is staged above the tip");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("cannot read the staged block: {error:?}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "block {} at height {}, {} transactions",
        hex::encode(block.header.block_hash().as_bytes()),
        block.header.chain_length,
        block.transactions.len()
    );

    let mut disagreements = 0;
    for transaction in &block.transactions {
        let txid = hex::encode(transaction.txid().as_bytes());
        if only.as_ref().is_some_and(|wanted| wanted != &txid) {
            continue;
        }
        match ask_both_engines_about(&mut vm, tip, transaction) {
            Ok(Some([compiler, interpreter])) => {
                println!("  compiler     {compiler}");
                println!("  interpreter  {interpreter}");
                if compiler != interpreter {
                    disagreements += 1;
                    println!("  DISAGREE");
                }
            }
            Ok(None) => {}
            Err(error) => eprintln!("{txid}: {error}"),
        }
    }
    println!("{disagreements} calls the engines answer differently");
    if disagreements == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Ask both engines one transaction's contract call, if that is what it is.
///
/// Only a contract call has two engines to disagree about; a transfer, a
/// deployment or a tenure change is answered the same way either way.
fn ask_both_engines_about(
    vm: &mut nano_vm::Vm,
    tip: [u8; 32],
    transaction: &nano_codec::Transaction,
) -> Result<Option<[String; 2]>, String> {
    let nano_codec::TransactionPayloadData::ContractCall {
        address,
        contract_name,
        function_name,
        arguments,
    } = transaction.payload().data()
    else {
        return Ok(None);
    };
    let origin = transaction
        .origin_address()
        .ok_or_else(|| "no recognized network".to_owned())?;
    let sender = clarity::vm::types::PrincipalData::parse(&origin.to_string())
        .map_err(|error| error.to_string())?;
    let contract = format!("{address}.{contract_name}");
    let identifier = clarity::vm::types::QualifiedContractIdentifier::parse(&contract)
        .map_err(|_| format!("{contract} is not a contract identifier"))?;
    let encoded = arguments
        .iter()
        .map(|argument| argument.as_bytes().to_vec())
        .collect::<Vec<_>>();
    println!(
        "{} {contract}::{function_name} ({} arguments)",
        hex::encode(transaction.txid().as_bytes()),
        encoded.len()
    );
    ask_both_engines(vm, tip, &sender, &identifier, function_name, &encoded).map(Some)
}

/// Make every contract in a state runnable by the interpreter.
///
/// The compiler's deploy stores placeholder function bodies, because the real
/// ones live in the module — so a contract it deployed cannot be interpreted.
/// A checkpoint carries real definitions, so the ones needing repair are only
/// those this node deployed itself, and there are few of them.
///
/// Safe: contract definitions live in a side store that is not the MARF, so
/// nothing here moves a state root.
fn heal_contracts(state: Option<&str>) -> ExitCode {
    let Some(state) = state else {
        eprintln!("usage: cargo xtask heal-contracts <state-dir>\n\
                   the node must not be running: it holds the state open");
        return ExitCode::FAILURE;
    };
    let mut vm = match nano_vm::Vm::open(Network::MAINNET, Path::new(state).join("chainstate")) {
        Ok(vm) => vm,
        Err(error) => {
            eprintln!("cannot open the state: {error:?}");
            return ExitCode::FAILURE;
        }
    };
    let Some(tip) = vm.tip() else {
        eprintln!("the state is sealed at no block");
        return ExitCode::FAILURE;
    };
    if let Err(error) = vm.begin_block(Some(tip), [0xea; 32]) {
        eprintln!("cannot begin a block on the tip: {error:?}");
        return ExitCode::FAILURE;
    }

    let stubbed = nano_oracle::uninterpretable_contracts(vm.state_and_context().0);
    println!("{} contracts the interpreter cannot run", stubbed.len());
    let mut healed = 0;
    for contract in &stubbed {
        match nano_oracle::heal_contract(&mut vm, contract) {
            Ok(()) => healed += 1,
            Err(error) => eprintln!("{contract}: {error}"),
        }
    }
    drop(vm.abort_block());
    println!("healed {healed} of {}", stubbed.len());
    ExitCode::SUCCESS
}


/// Fill one ledger row's holes and restate its fee totals, or say why it could
/// not be.
///
/// Answers the tenures it filled and how many fee totals it restated, both empty
/// when the row already owed a contiguous window under the right rule. Split out
/// of `repair_ledger` because a per-row body and a per-state loop are two jobs.
fn repair_ledger_row(
    side_store: &Path,
    archive: &mut ArchivedPayments,
    block: &str,
    data: &str,
) -> Result<(Vec<u64>, usize), ()> {
    let Some(bytes) = decode_hex(data.trim()) else {
        eprintln!("the ledger committed with {block} is not hexadecimal");
        return Err(())
    };
    let Ok(mut ledger) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        eprintln!("the ledger committed with {block} does not read as JSON");
        return Err(())
    };
    let missing = match missing_tenures(&ledger) {
        Ok(missing) => missing,
        Err(error) => {
            eprintln!("{error}");
            return Err(())
        }
    };
    let restated = match restate_tenure_fees(&mut ledger, archive) {
        Ok(restated) => restated,
        Err(error) => {
            eprintln!("{error}");
            return Err(())
        }
    };
    if missing.is_empty() && restated == 0 {
        return Ok((Vec::new(), 0));
    }
    for height in &missing {
        let entry = match archive.tenure(*height) {
            Ok(Some(entry)) => entry,
            Ok(None) => {
                eprintln!("the archive has no scheduled payment for tenure {height}");
                return Err(())
            }
            Err(error) => {
                eprintln!("{error}");
                return Err(())
            }
        };
        let Some(tenures) = ledger
            .get_mut("accounting")
            .and_then(|accounting| accounting.get_mut("tenures"))
            .and_then(serde_json::Value::as_array_mut)
        else {
            eprintln!("the ledger names no tenures");
            return Err(())
        };
        tenures.push(entry);
        tenures.sort_by_key(|tenure| {
            tenure
                .get("coinbase_height")
                .and_then(serde_json::Value::as_u64)
        });
    }
    // Validated through the node's own reader before it is written back: a
    // repair that produces something the node cannot parse, or that leaves a
    // hole, has to fail here rather than at the next start.
    let accounting = ledger.get("accounting").cloned().unwrap_or_default();
    match nano_chainstate::TenureAccounting::from_json(
        serde_json::to_string(&accounting).unwrap_or_default().as_bytes(),
    ) {
        Ok(accounting) => {
            if !missing_tenures(&ledger).is_ok_and(|missing| missing.is_empty()) {
                eprintln!("the repaired ledger for {block} still has a hole");
                return Err(())
            }
            let Some((first, last)) = accounting.known_earnings_span() else {
                eprintln!("the repaired ledger for {block} owes nothing");
                return Err(())
            };
            println!(
                "{block}: filled {} tenures, restated {restated} fee totals, window {first}..{last}",
                missing.len()
            );
        }
        Err(error) => {
            eprintln!("the repaired accounting for {block} does not read back: {error}");
            return Err(())
        }
    }
    write_ledger_row(side_store, block, &ledger)?;
    Ok((missing, restated))
}

/// Write one repaired ledger row back, and prove it went in as the node reads it.
fn write_ledger_row(
    side_store: &Path,
    block: &str,
    ledger: &serde_json::Value,
) -> Result<(), ()> {
    let encoded = serde_json::to_vec(ledger).unwrap_or_default();
    if let Err(error) = sqlite_script(
        side_store,
        &format!(
            "UPDATE chain_ledger SET data = x'{}' WHERE hex(block_id) = '{block}';\n",
            encode_hex(&encoded)
        ),
    ) {
        eprintln!("cannot write the repaired ledger for {block}: {error}");
        return Err(())
    }
    // Read back from the database, not from memory. The first attempt at
    // this validated what it was about to write and wrote it as the wrong
    // SQLite type, so every row passed and none could be read afterwards.
    match sqlite(
        side_store,
        &format!("SELECT typeof(data), hex(data) FROM chain_ledger WHERE hex(block_id) = '{block}'"),
    ) {
        Ok(written) => {
            let Some((kind, hex)) = written.trim().split_once('|') else {
                eprintln!("the repaired row for {block} did not read back");
                return Err(())
            };
            if kind != "blob" {
                eprintln!("the repaired row for {block} is a {kind} where the node reads bytes");
                return Err(())
            }
            if decode_hex(hex.trim()).as_deref() != Some(encoded.as_slice()) {
                eprintln!("the repaired row for {block} did not come back as it went in");
                return Err(())
            }
            Ok(())
        }
        Err(error) => {
            eprintln!("cannot read back the repaired ledger for {block}: {error}");
            Err(())
        }
    }
}

/// Restate every tenure's fee total from the archive, and answer how many moved.
///
/// A tenure's `fees` is the total *its own* transactions paid, which stacks-core
/// keeps in the following tenure's schedule. A checkpoint written before that was
/// understood carried each total under its successor's name, so a state imported
/// from one owes the wrong tenure's fees for as long as its window lasts. Where
/// the archive and the state already agree this changes nothing, which is what
/// makes running it over a whole state safe: it is a check on the tenures the
/// node totalled itself and a correction only on the ones it was handed.
fn restate_tenure_fees(
    ledger: &mut serde_json::Value,
    archive: &mut ArchivedPayments,
) -> Result<usize, String> {
    let tenures = ledger
        .get_mut("accounting")
        .and_then(|accounting| accounting.get_mut("tenures"))
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| "the ledger names no tenures".to_owned())?;
    let mut restated = 0;
    for tenure in tenures.iter_mut() {
        let Some(height) = tenure
            .get("coinbase_height")
            .and_then(serde_json::Value::as_u64)
        else {
            continue;
        };
        // Only the successor's row is needed: the recipient and coinbase already
        // in the ledger are the tenure's own and are not in question here.
        let Some((_, _, own_fees)) = archive.payment(height + 1)? else {
            continue;
        };
        let recorded = tenure
            .get("fees")
            .and_then(serde_json::Value::as_u64)
            .map(u128::from);
        if recorded == Some(own_fees) {
            continue;
        }
        tenure["fees"] = json!(own_fees);
        restated += 1;
    }
    Ok(restated)
}

/// Fill a hole in a state's tenure accounting from stacks-core's own archive.
///
/// A tenure's earnings are recorded when its start block executes, and a state
/// that missed one runs perfectly until the payout a hundred tenures later that
/// it cannot derive — hours in, having written state that has to be thrown away.
/// The live mainnet state carried exactly that: eight tenures, 251,322 through
/// 251,329, that nano executed and did not record, only reachable at tenure
/// 251,422.
///
/// The numbers come from the archive's own `payments` rows rather than from a
/// walk that re-derives them, for the same reason `capture-fixtures` reads them
/// there: they are stacks-core's arithmetic, not a reimplementation of it. That
/// also settles the one field a reconstruction would have had to guess — tenure
/// 251,329's coinbase is 2,000,000,000, twice the emission, because the burn
/// block before it produced no sortition and the coinbase accumulated.
///
/// Verified against the checkpoint before being trusted for anything it does not
/// hold: for every tenure the capture and the archive share, recipient, coinbase
/// and fees agree exactly, down to 251,321's oddly small 15,114.
fn repair_ledger(arguments: &[String]) -> ExitCode {
    let [state, archive] = arguments else {
        eprintln!(
            "usage: cargo xtask repair-ledger <state-dir> <stacks-core-index.sqlite>\n\
             fills tenures missing from the chain ledger; the node must not be running"
        );
        return ExitCode::FAILURE;
    };
    let side_store = Path::new(state).join("chainstate").join("clarity.sqlite");
    let archive = Path::new(archive);
    if !side_store.exists() || !archive.exists() {
        eprintln!("both the state's clarity.sqlite and the archive have to exist");
        return ExitCode::FAILURE;
    }

    // Both columns as hexadecimal. `data` is a *blob*, and rusqlite's reader
    // refuses a text value where it wants bytes — so a repair that writes the
    // JSON back as text leaves rows the node can no longer read at all, which
    // is exactly what the first attempt at this did. Hex in, hex out.
    let rows = match sqlite(
        &side_store,
        "SELECT hex(block_id), hex(data) FROM chain_ledger ORDER BY sequence",
    ) {
        Ok(rows) => rows,
        Err(error) => {
            eprintln!("cannot read the chain ledger: {error}");
            return ExitCode::FAILURE;
        }
    };
    let rows: Vec<(String, String)> = rows
        .lines()
        .filter_map(|line| line.split_once('|'))
        .map(|(block, data)| (block.to_owned(), data.to_owned()))
        .collect();
    if rows.is_empty() {
        eprintln!("the state has no chain ledger to repair");
        return ExitCode::FAILURE;
    }

    let mut payments = ArchivedPayments::new(archive);
    let mut repaired = 0_usize;
    let mut filled = std::collections::BTreeSet::new();
    let mut restated = 0_usize;
    for (block, data) in &rows {
        match repair_ledger_row(&side_store, &mut payments, block, data) {
            Ok((missing, moved)) if missing.is_empty() && moved == 0 => {}
            Ok((missing, moved)) => {
                filled.extend(missing);
                restated += moved;
                repaired += 1;
            }
            Err(()) => return ExitCode::FAILURE,
        }
    }

    if repaired == 0 {
        println!("every ledger row already owes a contiguous window");
    } else {
        println!(
            "repaired {repaired} of {} ledger rows, filling tenures {filled:?} and restating \
             {restated} fee totals",
            rows.len()
        );
    }
    ExitCode::SUCCESS
}

/// The coinbase heights absent from a ledger's tenure accounting.
///
/// Between the lowest and the highest it holds: a window that simply starts
/// later is shorter, and this is looking for the holes that make a *longer*
/// window unpayable.
fn missing_tenures(ledger: &serde_json::Value) -> Result<Vec<u64>, String> {
    let tenures = ledger
        .get("accounting")
        .and_then(|accounting| accounting.get("tenures"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "the ledger names no tenures".to_owned())?;
    let heights: std::collections::BTreeSet<u64> = tenures
        .iter()
        .filter_map(|tenure| tenure.get("coinbase_height")?.as_u64())
        .collect();
    let (Some(first), Some(last)) = (heights.iter().next(), heights.iter().next_back()) else {
        return Ok(Vec::new());
    };
    Ok((*first..=*last)
        .filter(|height| !heights.contains(height))
        .collect())
}

/// stacks-core's scheduled payments, read once each.
///
/// A repair walks every ledger row and every row owes the same window of
/// tenures, so without this the same two queries run tens of thousands of times.
struct ArchivedPayments {
    archive: PathBuf,
    read: std::collections::HashMap<u64, Option<(String, u128, u128)>>,
}

impl ArchivedPayments {
    fn new(archive: &Path) -> Self {
        Self {
            archive: archive.to_path_buf(),
            read: std::collections::HashMap::new(),
        }
    }

    fn payment(&mut self, coinbase_height: u64) -> Result<Option<(String, u128, u128)>, String> {
        if let Some(payment) = self.read.get(&coinbase_height) {
            return Ok(payment.clone());
        }
        let payment = archived_payment(&self.archive, coinbase_height)?;
        self.read.insert(coinbase_height, payment.clone());
        Ok(payment)
    }

    /// One tenure's earnings, as stacks-core's archive recorded them.
    ///
    /// The fee total comes from the *next* tenure's schedule, which is the only
    /// place stacks-core keeps it: a tenure is not over, and so cannot be
    /// totalled, until the next tenure change. A tenure whose successor the
    /// archive has not reached therefore has no complete entry here.
    fn tenure(&mut self, coinbase_height: u64) -> Result<Option<serde_json::Value>, String> {
        let Some(earned) = self.payment(coinbase_height)? else {
            return Ok(None);
        };
        let Some(following) = self.payment(coinbase_height + 1)? else {
            return Ok(None);
        };
        Ok(Some(json!({
            "coinbase_height": coinbase_height,
            "recipient": earned.0,
            "coinbase": earned.1,
            "fees": following.2,
        })))
    }
}

/// A tenure's archived scheduled payment: recipient, coinbase, `parent_fees`.
fn archived_payment(
    archive: &Path,
    coinbase_height: u64,
) -> Result<Option<(String, u128, u128)>, String> {
    let tenure = sqlite(
        archive,
        &format!(
            "SELECT block_id FROM nakamoto_tenure_events \
             WHERE cause = 0 AND coinbase_height = {coinbase_height} LIMIT 1"
        ),
    )?;
    let Some(block_id) = tenure.lines().next() else {
        return Ok(None);
    };
    let payment = sqlite(
        archive,
        &format!(
            "SELECT COALESCE(recipient, address), coinbase, tx_fees_anchored \
             FROM payments WHERE index_block_hash = '{block_id}' AND miner = 1 \
             ORDER BY rowid LIMIT 1"
        ),
    )?;
    let Some(payment) = payment.lines().next() else {
        return Ok(None);
    };
    let mut fields = payment.split('|');
    let recipient = fields
        .next()
        .ok_or_else(|| format!("tenure {coinbase_height} has no recipient"))?
        .to_owned();
    let coinbase = parse_u128("archived coinbase", fields.next())?;
    let anchored = parse_u128("archived anchored fees", fields.next())?;
    Ok(Some((recipient, coinbase, anchored)))
}
