#![forbid(unsafe_code)]

use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    time::{SystemTime, UNIX_EPOCH},
};

use nano_conformance::{FixtureManifest, FixtureStatus, scoreboard_at, validate_fixture_tree};
use serde_json::json;

fn main() -> ExitCode {
    let command = env::args().nth(1);
    match command.as_deref() {
        Some("scoreboard") => print_scoreboard(),
        Some("validate-fixtures") => validate_fixtures(),
        Some("capture-fixtures") => capture_fixtures(&env::args().skip(2).collect::<Vec<_>>()),
        _ => {
            eprintln!("usage: cargo xtask <scoreboard|validate-fixtures|capture-fixtures>");
            ExitCode::from(2)
        }
    }
}

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../crates/nano-conformance/fixtures")
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
            eprintln!("fixture tree is still the M0 baseline; capture real epoch-4 data first");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("fixture validation failed: {error}");
            ExitCode::FAILURE
        }
    }
}

struct CaptureConfig {
    out_dir: Option<PathBuf>,
    state_dir: PathBuf,
    events_dir: PathBuf,
    bitcoin_rpc: String,
    stacks_rpc: String,
    hacknet_commit: String,
    first_height: u64,
    replay_blocks: u64,
    checkpoint_height: u64,
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
        let mut out_dir = None;
        let mut state_dir = None;
        let mut events_dir = None;
        let mut bitcoin_rpc = None;
        let mut stacks_rpc = None;
        let mut hacknet_commit = None;
        let mut first_height = None;
        let mut replay_blocks = None;
        let mut checkpoint_height = None;

        while let Some(flag) = values.next() {
            let value = values
                .next()
                .ok_or_else(|| format!("missing value for {flag}"))?;
            match flag.as_str() {
                "--out-dir" => out_dir = Some(PathBuf::from(value)),
                "--state-dir" => state_dir = Some(PathBuf::from(value)),
                "--events-dir" => events_dir = Some(PathBuf::from(value)),
                "--bitcoin-rpc" => bitcoin_rpc = Some(value.to_owned()),
                "--stacks-rpc" => stacks_rpc = Some(value.to_owned()),
                "--hacknet-commit" => hacknet_commit = Some(value.to_owned()),
                "--first-height" => first_height = Some(parse_u64(flag, value)?),
                "--replay-blocks" => replay_blocks = Some(parse_u64(flag, value)?),
                "--checkpoint-height" => checkpoint_height = Some(parse_u64(flag, value)?),
                _ => return Err(format!("unknown capture-fixtures argument: {flag}")),
            }
        }

        Ok(Self {
            out_dir,
            state_dir: state_dir.ok_or_else(|| "--state-dir is required".to_owned())?,
            events_dir: events_dir.ok_or_else(|| "--events-dir is required".to_owned())?,
            bitcoin_rpc: bitcoin_rpc.ok_or_else(|| "--bitcoin-rpc is required".to_owned())?,
            stacks_rpc: stacks_rpc.ok_or_else(|| "--stacks-rpc is required".to_owned())?,
            hacknet_commit: hacknet_commit
                .ok_or_else(|| "--hacknet-commit is required".to_owned())?,
            first_height: first_height.ok_or_else(|| "--first-height is required".to_owned())?,
            replay_blocks: replay_blocks.ok_or_else(|| "--replay-blocks is required".to_owned())?,
            checkpoint_height: checkpoint_height
                .ok_or_else(|| "--checkpoint-height is required".to_owned())?,
        })
    }

    fn capture(&self, root: &Path) -> Result<(), String> {
        if self.replay_blocks == 0 {
            return Err("--replay-blocks must be greater than zero".to_owned());
        }
        if self.checkpoint_height >= self.first_height {
            return Err("--checkpoint-height must precede --first-height".to_owned());
        }
        let node_root = self.state_dir.join("stacks-miner-1/nakamoto-neon");
        let blocks_db = node_root.join("chainstate/blocks/nakamoto.sqlite");
        let sortition_db = node_root.join("burnchain/sortition/marf.sqlite");
        let blocks = self.blocks(&blocks_db)?;
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

    fn write_capture(
        &self,
        staging: &Path,
        blocks: &[CapturedBlock],
        blocks_db: &Path,
        sortition_db: &Path,
        node_root: &Path,
    ) -> Result<(), String> {
        let snapshot_query = "select block_height, burn_header_hash, sortition_id, parent_sortition_id, burn_header_timestamp, parent_burn_header_hash, consensus_hash, ops_hash, total_burn, sortition, sortition_hash, winning_block_txid, winning_stacks_block_hash, num_sortitions, stacks_block_accepted, stacks_block_height, arrival_index, canonical_stacks_tip_height, canonical_stacks_tip_hash, canonical_stacks_tip_consensus_hash, pox_valid, accumulated_coinbase_ustx, pox_payouts, miner_pk_hash from snapshots order by block_height";
        let snapshots = sqlite_json(sortition_db, snapshot_query)?;
        let bitcoin_blocks = Self::bitcoin_blocks(&snapshots)?;

        for block in blocks {
            let name = format!("{:08}-{}.bin", block.height, block.block_hash);
            let destination = staging.join("nakamoto/blocks").join(name);
            let raw_block = http_get(&format!(
                "{}/v3/blocks/{}",
                self.stacks_rpc, block.index_block_hash
            ))?;
            write_file(&destination, &raw_block)?;

            let event = self.event_for(&block.block_hash)?;
            let event_name = format!("{:08}-{}.json", block.height, block.block_hash);
            write_file(
                &staging.join("events/new_block").join(event_name),
                event.as_bytes(),
            )?;
        }

        for bitcoin_block in bitcoin_blocks {
            let burn_hash = bitcoin_block.hash;
            let payload = format!(
                "{{\"jsonrpc\":\"1.0\",\"id\":\"nano-stacks\",\"method\":\"getblock\",\"params\":[\"{burn_hash}\",0]}}"
            );
            let response = http_post(&self.bitcoin_rpc, &payload)?;
            let encoded = json_result_string(&response)?;
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

        let cycle = self.current_reward_cycle()?;
        let stacker_set = http_get(&format!("{}/v3/stacker_set/{cycle}", self.stacks_rpc))?;
        write_file(
            &staging
                .join("stacker_set")
                .join(format!("cycle-{cycle}.json")),
            &stacker_set,
        )?;

        let checkpoint = Self::block_at_height(blocks_db, self.checkpoint_height)?;
        let checkpoint_root = self.checkpoint_root(&checkpoint)?;
        let checkpoint_dir = staging.join("chainstate/checkpoint-H");
        copy_clarity_source(&node_root.join("chainstate/vm/clarity"), &checkpoint_dir)?;
        Self::write_native_effects(
            &node_root.join("chainstate/vm/index.sqlite"),
            blocks,
            &checkpoint_dir,
        )?;
        let checkpoint_manifest = format!(
            "format = \"stacks-core-marf-sqlite-v2\"\ncheckpoint_stacks_height = {}\nsource_state_id = \"{}\"\npublished_state_index_root = \"{}\"\n",
            checkpoint.height, checkpoint.index_block_hash, checkpoint_root
        );
        write_file(
            &checkpoint_dir.join("checkpoint.toml"),
            checkpoint_manifest.as_bytes(),
        )?;
        self.write_provenance(staging, blocks, &checkpoint, &checkpoint_root)?;
        write_file(
            &staging.join("manifest.toml"),
            format!(
                "mode = \"captured\"\nreplay_blocks = {}\n",
                self.replay_blocks
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

    fn write_native_effects(
        chainstate_db: &Path,
        blocks: &[CapturedBlock],
        checkpoint_dir: &Path,
    ) -> Result<(), String> {
        let block_ids = blocks
            .iter()
            .map(|block| format!("'{}'", block.index_block_hash))
            .collect::<Vec<_>>()
            .join(",");
        let heights = sqlite(
            chainstate_db,
            &format!(
                "SELECT DISTINCT coinbase_height FROM nakamoto_tenure_events \
                 WHERE cause = 0 AND block_id IN ({block_ids}) ORDER BY coinbase_height"
            ),
        )?;
        let mut effects = Vec::new();
        for height in heights.lines().filter(|height| !height.is_empty()) {
            let coinbase_height = parse_u64("coinbase height", height)?;
            let matured_height = coinbase_height.saturating_sub(100);
            let matured_block = sqlite(
                chainstate_db,
                &format!(
                    "SELECT block_id FROM nakamoto_tenure_events \
                     WHERE cause = 0 AND coinbase_height = {matured_height} LIMIT 1"
                ),
            )?;
            let Some(matured_block) = matured_block.lines().next() else {
                continue;
            };
            let rewards = sqlite(
                chainstate_db,
                &format!(
                    "SELECT COALESCE(recipient, address), coinbase, tx_fees_anchored, \
                     tx_fees_streamed_confirmed, tx_fees_streamed_produced \
                     FROM matured_rewards WHERE child_index_block_hash = '{matured_block}' \
                     ORDER BY vtxindex, CAST(coinbase AS INTEGER) DESC"
                ),
            )?;
            let mut credits = Vec::new();
            let mut liquid_supply_increase = 0_u128;
            for reward in rewards.lines().filter(|reward| !reward.is_empty()) {
                let mut fields = reward.split('|');
                let recipient = fields
                    .next()
                    .ok_or_else(|| "matured reward has no recipient".to_owned())?;
                let coinbase = parse_u128("matured reward coinbase", fields.next())?;
                let anchored = parse_u128("matured reward anchored fees", fields.next())?;
                let confirmed = parse_u128("matured reward confirmed fees", fields.next())?;
                let produced = parse_u128("matured reward produced fees", fields.next())?;
                if fields.next().is_some() {
                    return Err("matured reward has unexpected fields".to_owned());
                }
                let amount = coinbase
                    .checked_add(anchored)
                    .and_then(|amount| amount.checked_add(confirmed))
                    .and_then(|amount| amount.checked_add(produced))
                    .ok_or_else(|| "matured reward amount overflow".to_owned())?;
                // stacks-core credits both matured shares unconditionally, and a
                // zero credit still writes the recipient's balance into the block.
                credits.push(json!({ "recipient": recipient, "amount": amount }));
                liquid_supply_increase = liquid_supply_increase
                    .checked_add(coinbase)
                    .ok_or_else(|| "matured liquid supply overflow".to_owned())?;
            }
            effects.push(json!({
                "coinbase_height": coinbase_height,
                "credits": credits,
                "liquid_supply_increase": liquid_supply_increase,
            }));
        }
        let contents = serde_json::to_vec_pretty(&json!({ "matured_effects": effects }))
            .map_err(|error| format!("serialize native accounting: {error}"))?;
        write_file(&checkpoint_dir.join("native-effects.json"), &contents)
    }

    fn event_for(&self, block_hash: &str) -> Result<String, String> {
        let needle = format!("\"block_hash\":\"0x{block_hash}\"");
        let mut candidates = fs::read_dir(self.events_dir.join("new_block"))
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
        let contents = format!(
            "source = \"hacknet\"\nhacknet_commit = \"{}\"\ncaptured_at_unix = {captured_at}\ncheckpoint_stacks_height = {}\ncheckpoint_state_id = \"{}\"\ncheckpoint_state_index_root = \"{}\"\nfirst_stacks_height = {}\nreplay_blocks = {}\nbitcoin_rpc = \"{}\"\nstacks_rpc = \"{}\"\nfirst_block_hash = \"{}\"\nfirst_consensus_hash = \"{}\"\n",
            self.hacknet_commit,
            checkpoint.height,
            checkpoint.index_block_hash,
            checkpoint_root,
            self.first_height,
            self.replay_blocks,
            self.bitcoin_rpc,
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
            let target = root.join(relative);
            if target.exists() {
                fs::remove_dir_all(&target).map_err(io_error("replace captured fixture data"))?;
            }
            fs::rename(staging.join(relative), target)
                .map_err(io_error("install captured fixture data"))?;
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

fn sqlite_json(database: &Path, query: &str) -> Result<String, String> {
    command_output(
        Command::new("sqlite3")
            .arg("-json")
            .arg(database)
            .arg(query),
    )
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
