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
        Some("rebuild-accounting") => {
            rebuild_accounting(&env::args().skip(2).collect::<Vec<_>>())
        }
        _ => {
            eprintln!(
                "usage: cargo xtask <scoreboard|validate-fixtures|capture-fixtures|public-key|verify-block|decode-blocks|check-module|rebuild-accounting|probe-root|call-both>"
            );
            ExitCode::from(2)
        }
    }
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
                        } => println!("    deploys {contract_name}, {} chars", source.len()),
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
        let earliest = last.saturating_sub(MINER_REWARD_MATURITY + 1);
        for coinbase_height in earliest..=last {
            let Some(earned) = Self::scheduled_payment(chainstate_db, coinbase_height)? else {
                continue;
            };
            tenures.push(json!({
                "coinbase_height": coinbase_height,
                "recipient": earned.recipient,
                "coinbase": earned.coinbase,
                "fees": earned.anchored,
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
    let [state, contract, version, source] = arguments else {
        eprintln!(
            "usage: cargo xtask check-module <state-dir> <contract-id> <clarity-version> <source-file>\n\
             the node must not be running: it holds the state open"
        );
        return ExitCode::FAILURE;
    };
    let Ok(identifier) = clarity::vm::types::QualifiedContractIdentifier::parse(contract) else {
        eprintln!("{contract} is not a contract identifier");
        return ExitCode::FAILURE;
    };
    let version = match version.as_str() {
        "1" => clarity::vm::ClarityVersion::Clarity1,
        "2" => clarity::vm::ClarityVersion::Clarity2,
        "3" => clarity::vm::ClarityVersion::Clarity3,
        "4" => clarity::vm::ClarityVersion::Clarity4,
        "5" => clarity::vm::ClarityVersion::Clarity5,
        "6" => clarity::vm::ClarityVersion::Clarity6,
        other => {
            eprintln!("{other} is not a Clarity version");
            return ExitCode::FAILURE;
        }
    };
    let source = match fs::read_to_string(source) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("cannot read the source: {error}");
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
    let Some(tip) = vm.tip() else {
        eprintln!("the state is sealed at no block, so there is nothing to compile against");
        return ExitCode::FAILURE;
    };
    if let Err(error) = vm.begin_block(Some(tip), [0xcc; 32]) {
        eprintln!("cannot begin a block on the tip: {error:?}");
        return ExitCode::FAILURE;
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
            // The bytes are only readable disassembled, and a refused module is
            // exactly when someone wants to read them.
            if let Some(path) = env::var_os("NANO_DUMP_REFUSED_WASM") {
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
            ExitCode::FAILURE
        }
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

/// Run one contract call through both engines and print what each answered.
///
/// The interpreter is what mainnet runs, so a call the two answer differently
/// names clarity-wasm without any argument about state — and against a real
/// chainstate it can be a read-only function of a contract that is only
/// reachable through half a dozen others.
fn call_both(arguments: &[String]) -> ExitCode {
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
        match hex::decode(argument) {
            Ok(bytes) => encoded.push(bytes),
            Err(error) => {
                eprintln!("{argument} is not hexadecimal: {error}");
                return ExitCode::FAILURE;
            }
        }
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

    for interpreted in [false, true] {
        if let Err(error) = vm.begin_block(Some(tip), [0xca; 32]) {
            eprintln!("cannot begin a block: {error:?}");
            return ExitCode::FAILURE;
        }
        vm.interpret_contract_calls(interpreted);
        let outcome = vm.execute_contract_call_outcome(
            identifier.issuer.clone().into(),
            None,
            identifier.clone(),
            function,
            &encoded,
            &clarity::vm::costs::LimitedCostTracker::new_free(),
        );
        println!(
            "{:<12} {}",
            if interpreted { "interpreter" } else { "compiler" },
            match &outcome {
                Ok(
                    nano_vm::ContractCallOutcome::Success(result)
                    | nano_vm::ContractCallOutcome::AbortedByResponse(result),
                ) => format!("{:?}", result.value),
                Ok(nano_vm::ContractCallOutcome::RuntimeFailure { error, .. }) => {
                    format!("{error:?}")
                }
                Err(error) => format!("{error:?}"),
            }
        );
        // Nothing is sealed, so the state is untouched either way.
        drop(vm.abort_block());
    }
    ExitCode::SUCCESS
}

