use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsStr,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    time::{SystemTime, UNIX_EPOCH},
};

use nano_conformance::{FixtureManifest, FixtureStatus, validate_fixture_tree};
// The maturity window comes from the node's own crate rather than being restated
// here: the export refuses a window shorter than it, and a copy that drifted
// would write a checkpoint the node it is for cannot pay from.
use nano_chainstate::{
    CHECKPOINT_HISTORY_LIMIT, MINER_REWARD_MATURITY, NakamotoBlock, Signer, SignerSet,
};
use nano_primitives::Network;
use serde_json::json;

mod release_inventory;

use release_inventory::ReleaseInventory;

/// The chain a state directory on disk belongs to.
///
/// Read from the state rather than named here. Every one of these subcommands
/// opens somebody's chainstate, and a network is not a formatting preference:
/// it picks the boot address inside every principal and the identifier
/// `(chain-id)` reads, so naming the wrong one does not fail, it answers.
///
/// A state created before that was recorded says nothing, and mainnet is the
/// assumption those tools were written under; `NANO_NETWORK` overrides it with a
/// chain identifier for anything else.
fn state_network(chainstate: &Path) -> Network {
    if let Some(recorded) = nano_vm::recorded_network(chainstate) {
        return recorded;
    }
    env::var("NANO_NETWORK")
        .ok()
        .and_then(|value| {
            let value = value.trim();
            let parsed = value.strip_prefix("0x").map_or_else(
                || value.parse::<u32>().ok(),
                |hex| u32::from_str_radix(hex, 16).ok(),
            );
            if parsed.is_none() {
                eprintln!("NANO_NETWORK={value} is not a chain identifier; assuming mainnet");
            }
            parsed
        })
        .map_or(Network::MAINNET, Network::from_chain_id)
}

/// Open a state directory's VM to look at it, creating and writing nothing.
///
/// The route every inspection takes. `Vm::open` would create the directory, both
/// databases, a chain-identity row naming whatever network was assumed and an
/// `engine_identity` row — so a mistyped path there does not fail, it answers,
/// and it answers with an absence that looks exactly like a real one.
fn open_state_vm(chainstate: &Path) -> Result<nano_vm::Vm, nano_vm::MarfStoreError> {
    nano_vm::Vm::open_existing(chainstate)
}

/// Open a state directory's Clarity store to look at it, creating nothing.
fn open_state_store(chainstate: &Path) -> Result<nano_vm::MarfStore, nano_vm::MarfStoreError> {
    nano_vm::MarfStore::open_existing(chainstate)
}

/// Open a state directory's VM to *change* it, as the chain the state names.
///
/// Kept apart from [`open_state_vm`] and named for what it does: the commands
/// that repair, import or backfill are the only ones allowed through here.
fn open_state_vm_for_writing(chainstate: &Path) -> Result<nano_vm::Vm, nano_vm::MarfStoreError> {
    nano_vm::Vm::open(state_network(chainstate), chainstate)
}

fn main() -> ExitCode {
    let command = env::args().nth(1);
    match command.as_deref() {
        Some("scoreboard") => print_scoreboard(),
        Some("release-report") => release_report(&env::args().skip(2).collect::<Vec<_>>()),
        Some("infrastructure-tests") => infrastructure_tests(),
        Some("validate-fixtures") => validate_fixtures(),
        Some("capture-fixtures") => capture_fixtures(&env::args().skip(2).collect::<Vec<_>>()),
        Some("export-checkpoint-history") => {
            export_checkpoint_history(&env::args().skip(2).collect::<Vec<_>>())
        }
        Some("public-key") => print_public_key(env::args().nth(2).as_deref()),
        Some("verify-block") => verify_block(&env::args().skip(2).collect::<Vec<_>>()),
        Some("decode-blocks") => decode_blocks(env::args().nth(2).as_deref()),
        Some("check-module") => check_module(&env::args().skip(2).collect::<Vec<_>>()),
        Some("sweep-contracts") => sweep_contracts(&env::args().skip(2).collect::<Vec<_>>()),
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
        Some("export-sortition") => export_sortition(&env::args().skip(2).collect::<Vec<_>>()),
        Some("export-leader-keys") => export_leader_keys(&env::args().skip(2).collect::<Vec<_>>()),
        Some("leader-keys-from-blocks") => {
            leader_keys_from_blocks(&env::args().skip(2).collect::<Vec<_>>())
        }
        Some("block-info") => block_info(&env::args().skip(2).collect::<Vec<_>>()),
        Some("freeze-receipts") => freeze_receipts(&env::args().skip(2).collect::<Vec<_>>()),
        Some("compiler-identity") => compiler_identity(&env::args().skip(2).collect::<Vec<_>>()),
        Some("rebuild-accounting") => rebuild_accounting(&env::args().skip(2).collect::<Vec<_>>()),
        _ => {
            // Split by what they do to a state directory, because that is the
            // distinction an operator has to get right: the readers refuse a path
            // that is not already a state, and the writers create one.
            eprintln!(
                "usage: cargo xtask <command>\n\
                 \n\
                 reads a state directory, creating and changing nothing:\n\
                 \x20 block-info  call-both  call-both-tx  check-module  eval  probe-header\n\
                 \x20 probe-root  state-value\n\
                 \n\
                 writes to a state directory, creating one if it is not there:\n\
                 \x20 backfill-header  heal-contracts  import-headers  rebuild-accounting\n\
                 \x20 repair-ledger\n\
                 \n\
                 reads or writes elsewhere:\n\
                 \x20 capture-fixtures  compiler-identity  decode-blocks  export-headers\n\
                 \x20 export-checkpoint-history  export-leader-keys  export-sortition\n\
                 \x20 freeze-receipts  public-key\n\
                 \x20 infrastructure-tests  release-report  scoreboard  snapshot-state\n\
                 \x20 validate-fixtures\n\
                 \x20 verify-block"
            );
            ExitCode::from(2)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ContractArity {
    contract: String,
    report: nano_vm::ArityReport,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ContractLocalsPeak {
    contract: String,
    function: String,
    locals: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ContractRefusal {
    contract: String,
    arity: Option<nano_vm::ArityReport>,
}

#[derive(Default)]
struct ContractInventory {
    state_tip: [u8; 32],
    metadata_rows: usize,
    named: usize,
    current: usize,
    checked: usize,
    loaded: usize,
    deploy_epochs: BTreeMap<String, usize>,
    counterfactual_epoch40_checked: usize,
    counterfactual_epoch40_loaded: usize,
    counterfactual_epoch40_refused: BTreeMap<String, Vec<String>>,
    counterfactual_epoch40_unmeasured: BTreeMap<String, Vec<String>>,
    maximum: nano_vm::ArityReport,
    over_boundary: Vec<ContractArity>,
    maximum_emitted_locals: u32,
    maximum_emitted_local_sites: Vec<ContractLocalsPeak>,
    maximum_live_locals: u32,
    maximum_live_local_sites: Vec<ContractLocalsPeak>,
    refused: BTreeMap<String, Vec<ContractRefusal>>,
    stale_metadata: Vec<String>,
    unmeasured: BTreeMap<String, Vec<String>>,
}

impl ContractInventory {
    fn note_unmeasured(&mut self, contract: String, reason: String) {
        self.unmeasured.entry(reason).or_default().push(contract);
    }

    fn note_arity(&mut self, contract: &str, report: nano_vm::ArityReport) {
        self.maximum.max_function_params = self
            .maximum
            .max_function_params
            .max(report.max_function_params);
        self.maximum.max_function_results = self
            .maximum
            .max_function_results
            .max(report.max_function_results);
        self.maximum.max_control_params = self
            .maximum
            .max_control_params
            .max(report.max_control_params);
        self.maximum.max_control_results = self
            .maximum
            .max_control_results
            .max(report.max_control_results);
        self.maximum.top_level_results =
            self.maximum.top_level_results.max(report.top_level_results);
        if crosses_wasm_arity_boundary(&report) {
            self.over_boundary.push(ContractArity {
                contract: contract.to_owned(),
                report,
            });
        }
    }

    fn note_locals(&mut self, contract: &str, report: &nano_vm::LocalsReport) -> bool {
        for (function, measurement) in &report.emitted {
            note_locals_peak(
                &mut self.maximum_emitted_locals,
                &mut self.maximum_emitted_local_sites,
                contract,
                function,
                measurement.total,
            );
        }
        for (function, locals) in &report.max_live_locals {
            note_locals_peak(
                &mut self.maximum_live_locals,
                &mut self.maximum_live_local_sites,
                contract,
                function,
                *locals,
            );
        }
        !report.max_live_locals.is_empty() && !report.emitted.is_empty()
    }

    fn sort_measurements(&mut self) {
        self.over_boundary
            .sort_by(|left, right| left.contract.cmp(&right.contract));
        self.maximum_live_local_sites.sort_by(|left, right| {
            left.contract
                .cmp(&right.contract)
                .then_with(|| left.function.cmp(&right.function))
        });
        self.maximum_emitted_local_sites.sort_by(|left, right| {
            left.contract
                .cmp(&right.contract)
                .then_with(|| left.function.cmp(&right.function))
        });
        for contracts in self.counterfactual_epoch40_refused.values_mut() {
            contracts.sort();
        }
        for contracts in self.counterfactual_epoch40_unmeasured.values_mut() {
            contracts.sort();
        }
    }

    fn counterfactual_epoch40_refusals(&self) -> usize {
        self.counterfactual_epoch40_refused
            .values()
            .map(Vec::len)
            .sum()
    }

    fn passes(&self) -> bool {
        self.current + self.stale_metadata.len() == self.named
            && self.checked == self.current
            && self.loaded == self.current
            && self.counterfactual_epoch40_checked == self.current
            && self.counterfactual_epoch40_loaded + self.counterfactual_epoch40_refusals()
                == self.current
            && self.refused.is_empty()
            && self.unmeasured.is_empty()
            && self.counterfactual_epoch40_unmeasured.is_empty()
            && self.maximum_emitted_locals <= nano_vm::MAX_WASM_FUNCTION_LOCALS
    }
}

fn note_locals_peak(
    maximum: &mut u32,
    sites: &mut Vec<ContractLocalsPeak>,
    contract: &str,
    function: &str,
    locals: u32,
) {
    match locals.cmp(maximum) {
        std::cmp::Ordering::Greater => {
            *maximum = locals;
            sites.clear();
            sites.push(ContractLocalsPeak {
                contract: contract.to_owned(),
                function: function.to_owned(),
                locals,
            });
        }
        std::cmp::Ordering::Equal => sites.push(ContractLocalsPeak {
            contract: contract.to_owned(),
            function: function.to_owned(),
            locals,
        }),
        std::cmp::Ordering::Less => {}
    }
}

fn crosses_wasm_arity_boundary(report: &nano_vm::ArityReport) -> bool {
    [
        report.max_function_params,
        report.max_function_results,
        report.max_control_params,
        report.max_control_results,
        report.top_level_results,
    ]
    .into_iter()
    .any(|arity| arity > nano_vm::MAX_WASM_TYPE_ARITY)
}

fn arity_dimensions(report: &nano_vm::ArityReport) -> String {
    format!(
        "function params/results {}/{}, control params/results {}/{}, top-level results {}",
        report.max_function_params,
        report.max_function_results,
        report.max_control_params,
        report.max_control_results,
        report.top_level_results
    )
}

fn refusal_reason(error: &impl std::fmt::Debug) -> String {
    let reason = format!("{error:?}");
    reason
        .split("contract analysis failed: ")
        .nth(1)
        .unwrap_or(&reason)
        .to_owned()
}

fn inspect_counterfactual_epoch40(
    vm: &mut nano_vm::Vm,
    inventory: &mut ContractInventory,
    identifier: &clarity::vm::types::QualifiedContractIdentifier,
    contract: &str,
    version: clarity::vm::ClarityVersion,
    source: &str,
) {
    let inspected = vm.inspect_module_semantic_epoch(
        identifier,
        version,
        source,
        clarity::types::StacksEpochId::Epoch40,
    );
    match inspected {
        Ok(nano_vm::SemanticEpochInspection::Inspected(inspected)) => {
            let inspected = *inspected;
            inventory.counterfactual_epoch40_checked += 1;
            match inspected.refusal {
                Some(error) => inventory
                    .counterfactual_epoch40_refused
                    .entry(refusal_reason(&error))
                    .or_default()
                    .push(contract.to_owned()),
                None => inventory.counterfactual_epoch40_loaded += 1,
            }
        }
        Ok(nano_vm::SemanticEpochInspection::CompilationRefused(reason)) => {
            inventory.counterfactual_epoch40_checked += 1;
            inventory
                .counterfactual_epoch40_refused
                .entry(reason)
                .or_default()
                .push(contract.to_owned());
        }
        Err(error) => inventory
            .counterfactual_epoch40_unmeasured
            .entry(format!("counterfactual inspection failed: {error:?}"))
            .or_default()
            .push(contract.to_owned()),
    }
}

fn chainstate_directory(state: &Path) -> PathBuf {
    let has_database_pair = |directory: &Path| {
        directory.join("marf.sqlite").is_file() && directory.join("clarity.sqlite").is_file()
    };
    let nested = state.join("chainstate");
    if has_database_pair(&nested) {
        nested
    } else if has_database_pair(state)
        || state
            .file_name()
            .is_some_and(|name| name == OsStr::new("chainstate"))
    {
        state.to_path_buf()
    } else {
        nested
    }
}

const CONTRACT_METADATA_QUERY: &str = "SELECT key, COUNT(*) FROM metadata_table WHERE key LIKE 'clr-meta::%::analysis' \
     GROUP BY key ORDER BY key";

fn contract_metadata_candidates(database: &Path) -> Result<(usize, Vec<String>), String> {
    let connection = rusqlite::Connection::open_with_flags(
        nano_marf::immutable_uri(database),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|error| format!("cannot open contract metadata: {error}"))?;
    let mut statement = connection
        .prepare(CONTRACT_METADATA_QUERY)
        .map_err(|error| format!("cannot query contract metadata: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|error| format!("cannot read contract metadata: {error}"))?;
    let mut metadata_rows = 0_usize;
    let mut contracts = Vec::new();
    for row in rows {
        let (key, occurrences) =
            row.map_err(|error| format!("cannot read contract metadata: {error}"))?;
        let occurrences = usize::try_from(occurrences)
            .map_err(|_| format!("contract metadata count is invalid for {key}"))?;
        metadata_rows = metadata_rows
            .checked_add(occurrences)
            .ok_or_else(|| "contract metadata row count overflowed".to_owned())?;
        let contract = key
            .strip_prefix("clr-meta::")
            .and_then(|key| key.strip_suffix("::analysis"))
            .ok_or_else(|| format!("contract metadata key has an unexpected shape: {key}"))?;
        contracts.push(contract.to_owned());
    }
    Ok((metadata_rows, contracts))
}

fn inspect_contract_candidate(
    vm: &mut nano_vm::Vm,
    inventory: &mut ContractInventory,
    contract: String,
) {
    let identifier = match clarity::vm::types::QualifiedContractIdentifier::parse(&contract) {
        Ok(identifier) => identifier,
        Err(error) => {
            inventory.note_unmeasured(contract, format!("invalid contract identifier: {error}"));
            return;
        }
    };
    match vm.contract_presence(&identifier) {
        Ok(nano_vm::ContractPresence::Present) => inventory.current += 1,
        Ok(nano_vm::ContractPresence::Absent) => {
            inventory.stale_metadata.push(contract);
            return;
        }
        Err(error) => {
            inventory.note_unmeasured(
                contract,
                format!("current-tip presence is unavailable: {error:?}"),
            );
            return;
        }
    }
    let (source, version) = match vm.contract_source(&identifier) {
        Ok(source) => source,
        Err(error) => {
            inventory.note_unmeasured(contract, format!("source is unavailable: {error:?}"));
            return;
        }
    };
    let epoch = match vm.recorded_deploy_epoch(&identifier) {
        Ok(epoch) => epoch,
        Err(error) => {
            inventory.note_unmeasured(contract, format!("deploy epoch is unavailable: {error:?}"));
            return;
        }
    };
    *inventory
        .deploy_epochs
        .entry(format!("{epoch:?}"))
        .or_default() += 1;
    inventory.checked += 1;
    match vm.inspect_module_semantic_epoch(&identifier, version, &source, epoch) {
        Ok(nano_vm::SemanticEpochInspection::Inspected(inspected)) => {
            let inspected = *inspected;
            if !inventory.note_locals(&contract, &inspected.locals_report) {
                inventory.note_unmeasured(
                    contract.clone(),
                    "compiler returned incomplete emitted/live locals measurements".to_owned(),
                );
            }
            let report = inspected.arity_report;
            inventory.note_arity(&contract, report.clone());
            match inspected.refusal {
                Some(error) => inventory
                    .refused
                    .entry(refusal_reason(&error))
                    .or_default()
                    .push(ContractRefusal {
                        contract: contract.clone(),
                        arity: Some(report),
                    }),
                None => inventory.loaded += 1,
            }
        }
        Ok(nano_vm::SemanticEpochInspection::CompilationRefused(reason)) => inventory
            .refused
            .entry(reason)
            .or_default()
            .push(ContractRefusal {
                contract: contract.clone(),
                arity: None,
            }),
        Err(error) => {
            inventory.note_unmeasured(
                contract.clone(),
                format!("recorded-epoch module inspection failed: {error:?}"),
            );
        }
    }
    inspect_counterfactual_epoch40(vm, inventory, &identifier, &contract, version, &source);
}

fn contract_inventory(state: &Path) -> Result<ContractInventory, String> {
    let chainstate = chainstate_directory(state);
    let mut vm =
        open_state_vm(&chainstate).map_err(|error| format!("cannot open state: {error}"))?;
    let tip = vm
        .tip()
        .map_err(|error| format!("cannot read state tip: {error}"))?
        .ok_or_else(|| "the state is sealed at no block, so it holds no contracts".to_owned())?;
    vm.begin_block(Some(tip), [0xc5; 32])
        .map_err(|error| format!("cannot open a block on the tip: {error:?}"))?;

    let (metadata_rows, contracts) =
        contract_metadata_candidates(&chainstate.join("clarity.sqlite"))?;
    if contracts.is_empty() {
        return Err("the state names no contracts".to_owned());
    }

    let mut inventory = ContractInventory {
        state_tip: tip,
        metadata_rows,
        named: contracts.len(),
        ..ContractInventory::default()
    };
    for (index, contract) in contracts.into_iter().enumerate() {
        inspect_contract_candidate(&mut vm, &mut inventory, contract);
        let inspected = index + 1;
        if inspected % 10_000 == 0 {
            eprintln!(
                "inspected {inspected}/{} metadata candidates ({} current-tip)",
                inventory.named, inventory.current
            );
        }
    }
    inventory.sort_measurements();
    Ok(inventory)
}

fn print_epoch_inventory(inventory: &ContractInventory) {
    println!(
        "  state tip            {}",
        hex::encode(inventory.state_tip)
    );
    println!("  compiler             {}", nano_vm::COMPILER_IDENTITY);
    println!(
        "  stale metadata       {} noncanonical candidate(s) absent from the current tip",
        inventory.stale_metadata.len()
    );
    println!("  recorded deploy epochs");
    for (epoch, contracts) in &inventory.deploy_epochs {
        println!("    {epoch:<16} {contracts}");
    }
    println!(
        "  forced Epoch40       {}/{} load; {} semantic refusal(s); {} unmeasured",
        inventory.counterfactual_epoch40_loaded,
        inventory.current,
        inventory.counterfactual_epoch40_refusals(),
        inventory
            .counterfactual_epoch40_unmeasured
            .values()
            .map(Vec::len)
            .sum::<usize>()
    );
}

fn print_counterfactual_epoch40(inventory: &ContractInventory) {
    for (reason, contracts) in &inventory.counterfactual_epoch40_refused {
        println!(
            "\n  AFFECTED {} x forcing Epoch40 refuses: {reason}",
            contracts.len()
        );
        for contract in contracts {
            println!("      {contract}");
        }
    }
    for (reason, contracts) in &inventory.counterfactual_epoch40_unmeasured {
        println!("\n  EPOCH40 UNMEASURED {} x {reason}", contracts.len());
        for contract in contracts {
            println!("      {contract}");
        }
    }
}

fn print_contract_inventory(inventory: &ContractInventory) {
    println!(
        "{}/{} current-tip contracts compile and load ({} distinct candidates from {} metadata \
         rows)",
        inventory.loaded, inventory.current, inventory.named, inventory.metadata_rows
    );
    print_epoch_inventory(inventory);
    println!(
        "  Wasm type boundary   {} flattened parameters or results",
        nano_vm::MAX_WASM_TYPE_ARITY
    );
    println!(
        "  maximum function     {} params, {} results",
        inventory.maximum.max_function_params, inventory.maximum.max_function_results
    );
    println!(
        "  maximum control      {} params, {} results",
        inventory.maximum.max_control_params, inventory.maximum.max_control_results
    );
    println!(
        "  maximum top-level    {} results",
        inventory.maximum.top_level_results
    );
    let locals_limit = nano_vm::MAX_WASM_FUNCTION_LOCALS;
    if inventory.maximum_emitted_local_sites.is_empty() {
        println!("  exact emitted locals unavailable (no function was measured)");
    } else if inventory.maximum_emitted_locals <= locals_limit {
        println!(
            "  exact emitted locals {}/{} ({} headroom)",
            inventory.maximum_emitted_locals,
            locals_limit,
            locals_limit - inventory.maximum_emitted_locals
        );
    } else {
        println!(
            "  exact emitted locals {}/{} ({} OVER LIMIT)",
            inventory.maximum_emitted_locals,
            locals_limit,
            inventory.maximum_emitted_locals - locals_limit
        );
    }
    for peak in &inventory.maximum_emitted_local_sites {
        println!("    {} :: {}", peak.contract, peak.function);
    }
    println!(
        "  compiler live-pool peak {} (optimizer diagnostic)",
        inventory.maximum_live_locals
    );
    for peak in &inventory.maximum_live_local_sites {
        println!("    {} :: {}", peak.contract, peak.function);
    }

    let refused: BTreeSet<&str> = inventory
        .refused
        .values()
        .flatten()
        .map(|entry| entry.contract.as_str())
        .collect();
    println!(
        "  raw arity above boundary {} contract(s)",
        inventory.over_boundary.len()
    );
    for measurement in &inventory.over_boundary {
        println!(
            "    {}{}: {}",
            if refused.contains(measurement.contract.as_str()) {
                "REFUSED "
            } else {
                "lowered "
            },
            measurement.contract,
            arity_dimensions(&measurement.report)
        );
    }

    for (reason, contracts) in &inventory.refused {
        println!("\n  REFUSED {} x {reason}", contracts.len());
        for refusal in contracts {
            match &refusal.arity {
                Some(report) => {
                    println!("      {}: {}", refusal.contract, arity_dimensions(report));
                }
                None => println!(
                    "      {}: arity unavailable because compilation did not finish",
                    refusal.contract
                ),
            }
        }
    }
    print_counterfactual_epoch40(inventory);
    for (reason, contracts) in &inventory.unmeasured {
        println!("\n  UNMEASURED {} x {reason}", contracts.len());
        for contract in contracts {
            println!("      {contract}");
        }
    }
}

/// Compile **and load** every contract a state holds, and name what refuses.
///
/// Task 073 swept the imported mainnet state for peak locals and reported
/// 137,332 of 137,340 compiling. That sweep called `clar2wasm::compile` and never
/// handed the result to wasmtime, which matters: the arity limit task 084 is about
/// is a *validator* limit, so a module that exceeds it compiles cleanly and fails
/// to load. The sweep could not have seen one.
///
/// This runs `Vm::inspect_module`, the report-preserving form of the production
/// `check_module` path (`compile_under` followed by `loadable`), over every contract
/// in the state, in one process and read-only. So it answers both questions at
/// once: which contracts clarity-wasm cannot compile, and which produce a module
/// the engine will not accept.
fn sweep_contracts(arguments: &[String]) -> ExitCode {
    let [state] = arguments else {
        eprintln!(
            "usage: cargo xtask sweep-contracts <state-dir>\n\
             compiles and loads every contract the state holds, and names every refusal\n\
             reads only, and refuses a path that is not already a state\n\
             the node must not be running: its uncommitted pages are not readable"
        );
        return ExitCode::from(2);
    };
    match contract_inventory(Path::new(state)) {
        Ok(inventory) => {
            print_contract_inventory(&inventory);
            if inventory.passes() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            eprintln!("contract inventory failed: {error}");
            ExitCode::FAILURE
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
        format!(
            "{} vs {}",
            rebuilt.burn_block_height, recorded.burn_block_height
        ),
    );
    check(
        "burn_block_time",
        rebuilt.burn_block_time == recorded.burn_block_time,
        format!(
            "{} vs {}",
            rebuilt.burn_block_time, recorded.burn_block_time
        ),
    );
    check(
        "stacks_block_time",
        rebuilt.stacks_block_time == recorded.stacks_block_time,
        format!(
            "{} vs {}",
            rebuilt.stacks_block_time, recorded.stacks_block_time
        ),
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
    check(
        "vrf_seed",
        rebuilt.vrf_seed == recorded.vrf_seed,
        String::new(),
    );
    check(
        "burn_spend_total",
        rebuilt.burn_spend_total == recorded.burn_spend_total,
        format!(
            "{} vs {}",
            rebuilt.burn_spend_total, recorded.burn_spend_total
        ),
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
             reads only, and refuses a path that is not already a state\n\
             the node must not be running: its uncommitted pages are not readable"
        );
        return ExitCode::FAILURE;
    };
    let mut store = match open_state_store(&Path::new(state).join("chainstate")) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("cannot open the state: {error:?}");
            return ExitCode::FAILURE;
        }
    };
    let tip = match store.tip() {
        Ok(Some(tip)) => tip,
        Ok(None) => {
            eprintln!("the state is sealed at no block");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("cannot read the state tip: {error}");
            return ExitCode::FAILURE;
        }
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
             writes to the state, and creates one if the path is not already there\n\
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

    let mut vm = match open_state_vm_for_writing(&Path::new(state).join("chainstate")) {
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
    if let Err(error) =
        vm.record_partial_header(id, rebuilt, nano_vm::HeaderFields::PEER_BURN_CONTEXT)
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
        nano_vm::HeaderFields::PEER_BURN_CONTEXT
            .absent_names()
            .join(", ")
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
             reads only, and refuses a path that is not already a state\n\
             the node must not be running: its uncommitted pages are not readable"
        );
        return ExitCode::from(2);
    };
    let Ok(height) = height.parse::<u32>() else {
        eprintln!("{height} is not a height");
        return ExitCode::FAILURE;
    };
    let mut vm = match open_state_vm(&Path::new(state).join("chainstate")) {
        Ok(vm) => vm,
        Err(error) => {
            eprintln!("cannot open the state: {error}");
            return ExitCode::FAILURE;
        }
    };
    let tip = match vm.tip() {
        Ok(Some(tip)) => tip,
        Ok(None) => {
            eprintln!("the state is sealed at no block");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("cannot read the state tip: {error}");
            return ExitCode::FAILURE;
        }
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
        database
            .get_block_time(height)
            .map(|value| value.to_string()),
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
             writes to the state, and creates one if the path is not already there\n\
             the node must not be running: it holds the state open"
        );
        return ExitCode::from(2);
    };
    let mut vm = match open_state_vm_for_writing(&Path::new(state).join("chainstate")) {
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
/// Export the whole sortition history a checkpoint has to carry, not only its keys.
///
/// A node seeded without one derives no sortitions at all, and everything that
/// reads from a burn view then has nothing to read: no tenure's coinbase proof is
/// checkable, no miner signature is, and `/v3/sortitions` answers `503` — which is
/// what a stock signer's state machine fails to initialise on.
///
/// The three files go together and are useless apart. `snapshots.json` is the seed
/// and the burn blocks above it; `consensus-hashes.json` is the run of hashes
/// behind it, which is the one part a node cannot re-derive, because
/// `ConsensusHash::from_ops` mixes the hashes at power-of-two offsets back;
/// `leader-keys.json` is the registry a winning commitment names, registered once
/// and reused for years and so far below any window a checkpointed node holds.
///
/// `capture-fixtures` already writes all three for a fixture capture. A *checkpoint*
/// export had no way to, so every rig built from one derived nothing — which is how
/// [[069-resolve-the-pox-5-follower-state-root-divergence]] became reachable on the
/// hacknet rig, and why the stock signer hosted there could not initialise.
fn export_sortition(arguments: &[String]) -> ExitCode {
    let [sortition, out, height] = arguments else {
        eprintln!(
            "usage: cargo xtask export-sortition \
             <stacks-core burnchain/sortition/marf.sqlite> <out-dir> <to-burn-height>\n\
             writes snapshots.json, consensus-hashes.json and leader-keys.json, which a \
             node's `checkpoint.sortition` points at"
        );
        return ExitCode::from(2);
    };
    let Ok(to_height) = height.parse::<u64>() else {
        eprintln!("{height} is not a burn height");
        return ExitCode::FAILURE;
    };
    match write_sortition_export(Path::new(sortition), Path::new(out), to_height) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("exporting the sortition history failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn write_sortition_export(sortition: &Path, out: &Path, to_height: u64) -> Result<(), String> {
    // Every canonical snapshot up to the anchor, one row per burn block. A chain
    // with forks has more than one at a height, and only the `pox_valid` one is
    // this chain's.
    let snapshots = sqlite_json(
        sortition,
        &format!(
            "select block_height, burn_header_hash, sortition_id, parent_sortition_id, \
             burn_header_timestamp, parent_burn_header_hash, consensus_hash, ops_hash, \
             total_burn, sortition, sortition_hash, winning_block_txid, \
             winning_stacks_block_hash, num_sortitions, pox_valid, \
             accumulated_coinbase_ustx, pox_payouts, miner_pk_hash from snapshots \
             where pox_valid = 1 and block_height <= {to_height} \
             group by block_height order by block_height"
        ),
    )?;
    // Every burn block that elected somebody, ascending. nano's own field rather
    // than one of the archive's columns, and the seed cannot do without it: a chain
    // seeded at one snapshot holds no row below it, so it cannot say which burn
    // block before the seed last elected anybody. A tenure's accumulated coinbase is
    // measured from that height and is *minted*, and `/v3/sortitions` reports it as
    // `last_sortition_ch` -- which a stock signer requires, refusing to build a state
    // machine at all when the pair it asks for comes back as one entry.
    let electing = sqlite_json(
        sortition,
        &format!(
            "select block_height from snapshots where pox_valid = 1 and sortition = 1 \
             and block_height <= {to_height} order by block_height"
        ),
    )?;
    let electing: Vec<u64> = serde_json::from_str::<Vec<serde_json::Value>>(&electing)
        .map_err(|error| format!("unreadable sortition heights: {error}"))?
        .iter()
        .filter_map(|row| row.get("block_height")?.as_u64())
        .collect();
    let mut rows: Vec<serde_json::Value> = serde_json::from_str(&snapshots)
        .map_err(|error| format!("unreadable snapshots: {error}"))?;
    for row in &mut rows {
        let Some(height) = row.get("block_height").and_then(serde_json::Value::as_u64) else {
            continue;
        };
        let Some(object) = row.as_object_mut() else {
            continue;
        };
        let below: Vec<u64> = electing
            .iter()
            .copied()
            .filter(|elected| *elected <= height)
            .collect();
        if let Some(last) = below.last() {
            object.insert("last_sortition_height".to_owned(), (*last).into());
        }
        object.insert("sortitions_below_window".to_owned(), below.into());
    }
    write_file(
        &out.join("snapshots.json"),
        serde_json::to_vec(&rows)
            .map_err(|error| error.to_string())?
            .as_slice(),
    )?;
    // The same writer the fixture capture uses, so a checkpoint's history cannot be
    // read more loosely than a capture's, and both end at a burn block that elected
    // somebody.
    CaptureConfig::write_consensus_hashes(out, sortition, to_height)?;
    let (keys, signing) = read_leader_keys(sortition, to_height).and_then(|keys| {
        write_leader_keys(&out.join(nano_node::sortition::LEADER_KEY_FILE), &keys)
    })?;
    println!(
        "exported the sortition history up to burn {to_height}: {keys} leader-key \
         registrations, {signing} of them with a block-signing key hash"
    );
    Ok(())
}

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
/// Derive a leader-key registry from a capture's own Bitcoin blocks.
///
/// [`export_leader_keys`] copies stacks-core's `leader_keys` table, which is the
/// only source for mainnet: a registration a winning commitment names sits tens of
/// thousands of burn blocks below anything a checkpoint's window holds. A capture
/// that begins at burn 0 is the other case — every registration the chain ever made
/// is in the blocks it carries, so the registry is derivable from them and needs no
/// archive at all.
///
/// Which is why the offline suite had none: the capture is complete and nothing
/// read it. A node that executes blocks refuses to start without one rather than
/// accept every tenure unchecked, so the two rigs that run the shipped binary could
/// not start at all.
fn leader_keys_from_blocks(arguments: &[String]) -> ExitCode {
    let [blocks, out, magic] = arguments else {
        eprintln!(
            "usage: cargo xtask leader-keys-from-blocks <bitcoin/blocks dir> <out.json> <magic>\n\
             for a capture that begins at burn 0, where every registration the chain made \
             is in the blocks it carries; mainnet needs `export-leader-keys` and an archive"
        );
        return ExitCode::from(2);
    };
    let Ok(magic) = <[u8; 2]>::try_from(magic.as_bytes()) else {
        eprintln!("the magic is two bytes, such as T3 or X2");
        return ExitCode::FAILURE;
    };
    if !Path::new(blocks).is_dir() {
        eprintln!("{blocks} is not a directory of captured Bitcoin blocks");
        return ExitCode::FAILURE;
    }

    // Keyed by burn position, which is what a commitment names, so nothing can
    // produce two rows for one position.
    let mut keys: BTreeMap<(u64, u32), ExportedLeaderKey> = BTreeMap::new();
    let mut read = 0_usize;
    // Heights come from the capture's snapshots, which name each block by its
    // header hash — the same pairing the fixture's other readers use.
    let snapshots = Path::new(blocks)
        .parent()
        .and_then(Path::parent)
        .map(|root| root.join("sortition/snapshots.json"));
    let Some(snapshots) = snapshots.filter(|path| path.is_file()) else {
        eprintln!("the capture's sortition/snapshots.json is what names each block's height");
        return ExitCode::FAILURE;
    };
    let Ok(rows) = fs::read(&snapshots)
        .map_err(|error| error.to_string())
        .and_then(|bytes| {
            serde_json::from_slice::<Vec<serde_json::Value>>(&bytes).map_err(|e| e.to_string())
        })
    else {
        eprintln!("cannot read {}", snapshots.display());
        return ExitCode::FAILURE;
    };

    for row in &rows {
        let (Some(height), Some(hash)) = (
            row["block_height"].as_u64(),
            row["burn_header_hash"].as_str(),
        ) else {
            continue;
        };
        let path = Path::new(blocks).join(format!("{hash}.hex"));
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(bytes) = hex::decode(raw.trim()) else {
            continue;
        };
        let Ok(block) = nano_bitcoin::decode_block(height, &bytes, magic) else {
            continue;
        };
        read += 1;
        for operation in &block.operations {
            if let nano_bitcoin::BitcoinOperationKind::LeaderKeyRegistration {
                vrf_public_key,
                block_signing_key_hash,
                ..
            } = &operation.kind
            {
                keys.insert(
                    (height, operation.transaction_index),
                    ExportedLeaderKey {
                        block_height: height,
                        vtxindex: operation.transaction_index,
                        public_key: hex::encode(vrf_public_key),
                        memo: block_signing_key_hash.map(hex::encode).unwrap_or_default(),
                    },
                );
            }
        }
    }

    let keys: Vec<ExportedLeaderKey> = keys.into_values().collect();
    let signing = keys.iter().filter(|key| !key.memo.is_empty()).count();
    match write_leader_keys(Path::new(out), &keys) {
        Ok(_) => {
            println!(
                "read {read} Bitcoin blocks and derived {} leader-key registrations, {signing} \
                 of them carrying a block-signing key hash",
                keys.len()
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("writing the leader keys failed: {error}");
            ExitCode::FAILURE
        }
    }
}

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
            row.get(index)
                .map_err(|error: rusqlite::Error| error.to_string())
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
        let tenure_start_id =
            tenure.map_or_else(|| block_id.clone(), |start| start.block_id.clone());
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
        miner: payment
            .as_ref()
            .and_then(|(address, _, _)| miner_address(address)),
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
             reads only, and refuses a path that is not already a state\n\
             the node must not be running: its uncommitted pages are not readable"
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
    let mut vm = match open_state_vm(&chainstate) {
        Ok(vm) => vm,
        Err(error) => {
            eprintln!("cannot open the state: {error:?}");
            return ExitCode::FAILURE;
        }
    };

    let stacks_height = match vm.height_of(id) {
        Ok(height) => u64::from(height.unwrap_or(0)),
        Err(error) => {
            eprintln!("cannot read the block height: {error}");
            return ExitCode::FAILURE;
        }
    };
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
                    && let Some(tenure_height) = recorded
                        .field(nano_vm::HeaderFields::TENURE_HEIGHT, |header| {
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
    let block = match fs::read(block_path)
        .map_err(|error| error.to_string())
        .and_then(|bytes| NakamotoBlock::decode(&bytes).map_err(|error| error.to_string()))
    {
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
                        // Which contract a call targets, and how many arguments it
                        // carried. A block that fails on an internal VM error names
                        // the transaction now, and this is the next question after
                        // that one: what it was calling.
                        nano_codec::TransactionPayloadData::ContractCall {
                            address,
                            contract_name,
                            function_name,
                            arguments,
                        } => {
                            println!(
                                "    calls {address}.{contract_name}::{function_name} with {} \
                                 arguments",
                                arguments.len()
                            );
                            for (index, argument) in arguments.iter().enumerate() {
                                println!("      {index}: {argument:?}");
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(error) => {
                eprintln!("block {decoded} at byte {offset} does not decode: {error}");
                eprintln!(
                    "next 64 bytes: {}",
                    hex::encode(&bytes[offset..(offset + 64).min(bytes.len())])
                );
                return ExitCode::FAILURE;
            }
        }
    }
    println!("{decoded} blocks decoded");
    ExitCode::SUCCESS
}

/// The board, and an exit status that agrees with it.
///
/// It used to return success whenever the *manifest* loaded, so a table naming a
/// consensus divergence at block 76 exited zero and every caller that checks an
/// exit status -- CI, a release gate, a shell -- was told the replay had passed.
fn print_scoreboard() -> ExitCode {
    let manifest_path = fixture_root().join("manifest.toml");
    match FixtureManifest::load(&manifest_path) {
        Ok(manifest) => {
            let (board, passed) = nano_conformance::scoreboard_result(&fixture_root(), manifest);
            print!("{board}");
            if passed {
                ExitCode::SUCCESS
            } else {
                eprintln!(
                    "a required scoreboard surface diverged from its oracle, so this is not a \
                     passing replay"
                );
                ExitCode::FAILURE
            }
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
            match describe_fixture(&fixture_root(), replay_blocks) {
                Ok(description) => {
                    print!("{description}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("fixture validation failed: {error}");
                    ExitCode::FAILURE
                }
            }
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

/// Name the oracle and prove that its captured consensus history seeds a chain.
fn describe_fixture(root: &Path, replay_blocks: u64) -> Result<String, String> {
    let provenance = fs::read_to_string(root.join("provenance.toml"))
        .map_err(|error| format!("cannot read provenance.toml: {error}"))?;
    let field = |name: &str| {
        provenance
            .lines()
            .find_map(|line| line.trim().strip_prefix(&format!("{name} = ")))
            .map(|value| value.trim().trim_matches('"').to_owned())
            .ok_or_else(|| format!("provenance.toml does not name {name}"))
    };
    let source = field("source")?;
    let source_revision = field("hacknet_commit")?;
    let stacks_core = field("stacks_core_rev")?;
    let captured_at = field("captured_at_unix")?;
    let history = nano_node::sortition::SortitionTracker::history_from(&root.join("sortition"))
        .map_err(|error| format!("the consensus-hash history is unreadable: {error}"))?;
    let tracker = nano_node::sortition::SortitionTracker::from_capture(&root.join("sortition"))
        .map_err(|error| format!("the consensus-hash history cannot seed a chain: {error}"))?;

    Ok(format!(
        "captured fixture tree is valid\n\
         capture: {source} revision {source_revision}, taken at unix {captured_at}\n\
         stacks-core oracle revision: {stacks_core}\n\
         replay: {replay_blocks} blocks\n\
         consensus history: {} hashes; seeds a chain at burn {} ({})\n",
        history.len(),
        tracker.tip().bitcoin_height,
        tracker.tip().consensus_hash,
    ))
}

#[derive(Debug)]
struct CheckpointHistoryExport {
    blocks_db: PathBuf,
    source: [u8; 32],
    state_root: [u8; 32],
    out_dir: PathBuf,
}

impl CheckpointHistoryExport {
    fn parse(arguments: &[String]) -> Result<Self, String> {
        let mut values = arguments.iter();
        let mut blocks_db = None;
        let mut source = None;
        let mut state_root = None;
        let mut out_dir = None;
        while let Some(flag) = values.next() {
            let value = values
                .next()
                .ok_or_else(|| format!("missing value for {flag}"))?;
            match flag.as_str() {
                "--blocks-db" => blocks_db = Some(PathBuf::from(value)),
                "--source-id" => source = Some(parse_hash(value)?),
                "--state-root" => state_root = Some(parse_hash(value)?),
                "--out-dir" => out_dir = Some(PathBuf::from(value)),
                _ => {
                    return Err(format!(
                        "unknown export-checkpoint-history argument: {flag}"
                    ));
                }
            }
        }
        Ok(Self {
            blocks_db: blocks_db.ok_or_else(|| "--blocks-db is required".to_owned())?,
            source: source.ok_or_else(|| "--source-id is required".to_owned())?,
            state_root: state_root.ok_or_else(|| "--state-root is required".to_owned())?,
            out_dir: out_dir.ok_or_else(|| "--out-dir is required".to_owned())?,
        })
    }
}

fn export_checkpoint_history(arguments: &[String]) -> ExitCode {
    let result = CheckpointHistoryExport::parse(arguments).and_then(|config| {
        write_checkpoint_authentication_history(
            &config.blocks_db,
            config.source,
            config.state_root,
            &config.out_dir,
        )
    });
    match result {
        Ok(blocks) => {
            println!("exported {blocks} authenticated checkpoint blocks");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("checkpoint history export failed: {error}");
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
        let node_root = self
            .node_root
            .clone()
            .unwrap_or_else(|| self.state_dir.join("stacks-miner-1/nakamoto-neon"));
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
    /// The burn views the captured blocks actually execute under.
    ///
    /// A block's own `consensus_hash` names the sortition that elected its
    /// *tenure*, and for a tenure that outlives that burn block it is not the view
    /// the block runs in: a tenure **extend** moves the Clarity burn view forward
    /// while the tenure's name stays put, and `burn-block-height`,
    /// `get-burn-block-info?` and the seed all follow the view.
    ///
    /// Taking the span from the tenure names alone therefore truncated any capture
    /// containing an extend, and truncated it silently -- the replay stopped at the
    /// first block whose view was past the end with "block Bitcoin view is absent
    /// from captured Bitcoin snapshots", which reads like a divergence and is a
    /// missing fixture. Measured on a live pox-5 hacknet: block 931's tenure is burn
    /// 392 and its view is burn 399, seven blocks outside what the old span kept.
    ///
    /// `nakamoto_block_headers.burn_view` is stacks-core's own answer for this, so
    /// the union of the two columns is read rather than the blocks being decoded.
    fn burn_views(index_db: &Path, blocks: &[CapturedBlock]) -> Vec<String> {
        let ids = blocks
            .iter()
            .map(|block| format!("'{}'", block.index_block_hash))
            .collect::<Vec<_>>()
            .join(",");
        sqlite(
            index_db,
            &format!(
                "select distinct burn_view from nakamoto_block_headers \
                 where index_block_hash in ({ids}) and burn_view is not null"
            ),
        )
        .map(|output| {
            output
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        // A node whose index cannot be read leaves the span as the tenure names
        // give it, which is what it was before: the replay then says which view it
        // is missing, and that is better than refusing to capture at all.
        .unwrap_or_default()
    }

    fn burn_span(
        sortition_db: &Path,
        index_db: &Path,
        blocks: &[CapturedBlock],
    ) -> Result<(u64, u64), String> {
        let mut names: Vec<String> = blocks
            .iter()
            .map(|block| block.consensus_hash.clone())
            .collect();
        names.extend(Self::burn_views(index_db, blocks));
        names.sort();
        names.dedup();
        let hashes = names
            .iter()
            .map(|name| format!("'{name}'"))
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

    /// Every burn block the snapshots name, as bitcoind hands it over: the raw
    /// consensus bytes, which is the only form the ingest reads.
    fn write_bitcoin_blocks(&self, staging: &Path, snapshots: &str) -> Result<(), String> {
        for bitcoin_block in Self::bitcoin_blocks(snapshots)? {
            let burn_hash = bitcoin_block.hash;
            let encoded = if let Some(rest) = self.bitcoin_rest.as_ref() {
                let raw = http_get(&format!(
                    "{}/block/{burn_hash}/raw",
                    rest.trim_end_matches('/')
                ))?;
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
        Ok(())
    }

    /// The consensus-hash history, without which the snapshots beside it seed nothing.
    ///
    /// `ConsensusHash::from_ops` mixes the hashes at power-of-two offsets behind a burn
    /// block, so it reaches back thousands and a node cannot re-derive it from its own
    /// snapshots. `SortitionTracker::history_from` reads this file, and a capture
    /// written without it answered "neither the saved sortitions nor the capture can
    /// seed a chain" -- which is how a rig came to derive no sortitions at all, and how
    /// tasks/069 became reachable on it.
    ///
    /// Ended at the last burn block that *elected* somebody, not simply at the last one
    /// captured. A chain is seeded by the snapshot its history ends at, and the sampling
    /// of the block after a seed mixes the most recent winner's VRF seed -- so a seed
    /// whose own block elected nobody cannot supply it, and the tracker refuses rather
    /// than guessing. The snapshots and Bitcoin blocks still run to `last_burn`, because
    /// the replay needs the burn blocks above the seed; only the seed must have won.
    ///
    /// Whole from the chain's start, and one per burn block: a truncated run derives a
    /// different hash from there on. Mainnet's is 294,170 hashes, the cheapest thing in
    /// the capture.
    /// `directory` is the sortition directory itself, not the capture root: a
    /// checkpoint export writes the same three files somewhere else entirely.
    fn write_consensus_hashes(
        directory: &Path,
        sortition_db: &Path,
        last_burn: u64,
    ) -> Result<(), String> {
        let seed_burn = sqlite(
            sortition_db,
            &format!(
                "select max(block_height) from snapshots where pox_valid = 1 and \
                 sortition = 1 and block_height <= {last_burn}"
            ),
        )?;
        let seed_burn: u64 = seed_burn.trim().parse().map_err(|error| {
            format!(
                "no burn block at or below {last_burn} elected anybody, so no snapshot can \
                 seed a chain: {error}"
            )
        })?;
        let history = sqlite_json(
            sortition_db,
            &format!(
                "select consensus_hash from snapshots where pox_valid = 1 and \
                 block_height <= {seed_burn} group by block_height order by block_height"
            ),
        )?;
        let hashes: Vec<String> = serde_json::from_str::<Vec<serde_json::Value>>(&history)
            .map_err(|error| format!("unreadable consensus-hash history: {error}"))?
            .iter()
            .filter_map(|row| row.get("consensus_hash")?.as_str().map(ToOwned::to_owned))
            .collect();
        println!(
            "captured {} consensus hashes up to burn {seed_burn}, the last that elected \
             anybody, which is what lets the snapshots seed a chain",
            hashes.len()
        );
        write_file(
            &directory.join("consensus-hashes.json"),
            serde_json::to_vec(&serde_json::json!({ "hashes": hashes }))
                .map_err(|error| error.to_string())?
                .as_slice(),
        )
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
        let (first_burn, last_burn) = Self::burn_span(
            sortition_db,
            &node_root.join("chainstate/vm/index.sqlite"),
            blocks,
        )?;
        let snapshot_query = format!(
            "select block_height, burn_header_hash, sortition_id, parent_sortition_id, burn_header_timestamp, parent_burn_header_hash, consensus_hash, ops_hash, total_burn, sortition, sortition_hash, winning_block_txid, winning_stacks_block_hash, num_sortitions, stacks_block_accepted, stacks_block_height, arrival_index, canonical_stacks_tip_height, canonical_stacks_tip_hash, canonical_stacks_tip_consensus_hash, pox_valid, accumulated_coinbase_ustx, pox_payouts, miner_pk_hash from snapshots where pox_valid = 1 and block_height between {first_burn} and {last_burn} group by block_height order by block_height"
        );
        let snapshot_query = snapshot_query.as_str();
        let snapshots = sqlite_json(sortition_db, snapshot_query)?;
        self.write_blocks(staging, blocks, blocks_db)?;
        self.write_bitcoin_blocks(staging, &snapshots)?;

        write_file(
            &staging.join("sortition/snapshots.json"),
            snapshots.as_bytes(),
        )?;

        Self::write_consensus_hashes(&staging.join("sortition"), sortition_db, last_burn)?;

        // The leader-key registry, without which no tenure's coinbase proof can
        // be checked at all: the registration a winning commitment names is
        // registered once and reused for years, so it sits far below the burn
        // span this capture holds. It is the cheapest thing in the capture — a
        // quarter of a megabyte for mainnet's entire history — and the
        // alternative is asking the peer that supplied the block for the input
        // that decides whether to believe it.
        let (keys, signing) = read_leader_keys(sortition_db, last_burn).and_then(|keys| {
            write_leader_keys(
                &staging
                    .join("sortition")
                    .join(nano_node::sortition::LEADER_KEY_FILE),
                &keys,
            )
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
        let checkpoint_dir = staging.join("chainstate/checkpoint-H");
        let checkpoint_root = self.write_checkpoint_block(&checkpoint, &checkpoint_dir)?;
        write_checkpoint_authentication_history(
            blocks_db,
            parse_hash(&checkpoint.index_block_hash)?,
            parse_hash(&checkpoint_root)?,
            &checkpoint_dir.join("authentication-history"),
        )?;
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
        refuse_a_short_earnings_window(&tenures, last)?;
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

    /// Keep the block that sealed the checkpoint, and read its root out of it.
    ///
    /// The root is what the manifest publishes; the block is what makes that
    /// root trustworthy, because a reward set signed a preimage containing it. A
    /// capture that keeps only the root leaves the attestation with nothing to
    /// check offline, and stands a later block in for the checkpoint's own — the
    /// mechanism, but not the block a node actually adopts.
    ///
    /// The header is fixed-width up to the root, so the offsets are the layout:
    /// `version(1) ‖ chain_length(8) ‖ burn_spent(8) ‖ consensus_hash(20) ‖
    /// parent_block_id(32) ‖ tx_merkle_root(32)` puts `state_index_root` at 101.
    /// Both identifying fields are checked, because a peer answering with a
    /// different block would otherwise fix that block's root into the manifest.
    fn write_checkpoint_block(
        &self,
        checkpoint: &CapturedBlock,
        checkpoint_dir: &Path,
    ) -> Result<String, String> {
        let raw_block = http_get(&format!(
            "{}/v3/blocks/{}",
            self.stacks_rpc, checkpoint.index_block_hash
        ))?;
        let root = raw_block.get(101..133).ok_or_else(|| {
            "checkpoint block is too short to contain a state index root".to_owned()
        })?;
        let root = hex(root);
        let height = raw_block
            .get(1..9)
            .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
            .map(u64::from_be_bytes)
            .ok_or_else(|| "checkpoint block is too short to state its height".to_owned())?;
        if height != checkpoint.height {
            return Err(format!(
                "asked for the block at Stacks height {} and was served one at {height}",
                checkpoint.height
            ));
        }
        let consensus_hash = raw_block.get(17..37).map(hex).unwrap_or_default();
        if consensus_hash != checkpoint.consensus_hash {
            return Err(format!(
                "the block served for the checkpoint is in tenure {consensus_hash}, not {}",
                checkpoint.consensus_hash
            ));
        }
        write_file(
            &checkpoint_dir.join(nano_marf::CHECKPOINT_BLOCK_FILE),
            &raw_block,
        )?;
        Ok(root)
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

struct ArchivedNakamotoBlock {
    raw: Vec<u8>,
    block: NakamotoBlock,
}

const ARCHIVED_NAKAMOTO_BLOCK_QUERY: &str = "SELECT height, block_hash, consensus_hash, index_block_hash, parent_block_id, \
            is_tenure_start, data \
     FROM nakamoto_staging_blocks \
     WHERE index_block_hash = ?1 AND processed = 1 AND orphaned = 0";

fn read_archived_nakamoto_block(
    statement: &mut rusqlite::Statement<'_>,
    expected_id: [u8; 32],
) -> Result<ArchivedNakamotoBlock, String> {
    let expected_id_hex = encode_hex(&expected_id);
    let mut rows = statement
        .query(rusqlite::params![expected_id_hex])
        .map_err(|error| error.to_string())?;
    let Some(row) = rows.next().map_err(|error| error.to_string())? else {
        return Err(format!(
            "canonical Nakamoto block {} is absent from the archive",
            encode_hex(&expected_id)
        ));
    };
    let stored = (
        row.get::<_, u64>(0).map_err(|error| error.to_string())?,
        row.get::<_, String>(1).map_err(|error| error.to_string())?,
        row.get::<_, String>(2).map_err(|error| error.to_string())?,
        row.get::<_, String>(3).map_err(|error| error.to_string())?,
        row.get::<_, String>(4).map_err(|error| error.to_string())?,
        row.get::<_, i64>(5).map_err(|error| error.to_string())?,
        row.get::<_, Vec<u8>>(6)
            .map_err(|error| error.to_string())?,
    );
    if rows.next().map_err(|error| error.to_string())?.is_some() {
        return Err(format!(
            "canonical Nakamoto block {} occurs more than once in the archive",
            encode_hex(&expected_id)
        ));
    }
    let (height, block_hash, consensus_hash, index_block_hash, parent_block_id, start, raw) =
        stored;
    let block = NakamotoBlock::decode(&raw).map_err(|error| {
        format!(
            "canonical Nakamoto block {} cannot be decoded: {error}",
            encode_hex(&expected_id)
        )
    })?;
    let stored_start = match start {
        0 => false,
        1 => true,
        value => {
            return Err(format!(
                "canonical Nakamoto block {} has invalid is_tenure_start value {value}",
                encode_hex(&expected_id)
            ));
        }
    };
    let decoded_id = encode_hex(block.block_id().as_bytes());
    let decoded_hash = encode_hex(block.header.block_hash().as_bytes());
    let decoded_consensus = encode_hex(block.header.consensus_hash.as_bytes());
    let decoded_parent = encode_hex(block.header.parent_block_id.as_bytes());
    let decoded_start = nano_chainstate::starts_new_tenure(&block);
    if decoded_id != encode_hex(&expected_id)
        || index_block_hash != decoded_id
        || block_hash != decoded_hash
        || consensus_hash != decoded_consensus
        || parent_block_id != decoded_parent
        || height != block.header.chain_length
        || stored_start != decoded_start
    {
        return Err(format!(
            "canonical archive metadata does not match decoded Nakamoto block {}",
            encode_hex(&expected_id)
        ));
    }
    Ok(ArchivedNakamotoBlock { raw, block })
}

fn write_checkpoint_authentication_history(
    database: &Path,
    source: [u8; 32],
    state_root: [u8; 32],
    output: &Path,
) -> Result<usize, String> {
    if output.exists() {
        return Err(format!(
            "checkpoint authentication output {} already exists",
            output.display()
        ));
    }
    let connection = open_archive(database)?;
    let mut statement = connection
        .prepare(ARCHIVED_NAKAMOTO_BLOCK_QUERY)
        .map_err(|error| error.to_string())?;

    let source_block = read_archived_nakamoto_block(&mut statement, source)?;
    if source_block.block.header.state_index_root.as_bytes() != &state_root {
        return Err(format!(
            "checkpoint source {} publishes state root {}, not {}",
            encode_hex(&source),
            encode_hex(source_block.block.header.state_index_root.as_bytes()),
            encode_hex(&state_root)
        ));
    }

    let mut reversed = vec![source_block];
    while !nano_chainstate::starts_new_tenure(&reversed.last().expect("source block").block) {
        if reversed.len() == CHECKPOINT_HISTORY_LIMIT {
            return Err(format!(
                "checkpoint authentication suffix exceeds the bounded limit of \
                 {CHECKPOINT_HISTORY_LIMIT} blocks"
            ));
        }
        let child = &reversed.last().expect("source block").block;
        let parent_id = *child.header.parent_block_id.as_bytes();
        let parent = read_archived_nakamoto_block(&mut statement, parent_id)?;
        if parent.block.header.chain_length.checked_add(1) != Some(child.header.chain_length) {
            return Err(format!(
                "checkpoint authentication history jumps from Stacks height {} to {}",
                parent.block.header.chain_length, child.header.chain_length
            ));
        }
        reversed.push(parent);
    }

    let first = &reversed.last().expect("history starts at a tenure").block;
    let mut child_height = first.header.chain_length;
    let mut boundary_id = *first.header.parent_block_id.as_bytes();
    let boundary = loop {
        let parent = read_archived_nakamoto_block(&mut statement, boundary_id)?;
        if parent.block.header.chain_length.checked_add(1) != Some(child_height) {
            return Err(format!(
                "checkpoint boundary walk jumps from Stacks height {} to {child_height}",
                parent.block.header.chain_length
            ));
        }
        if nano_chainstate::starts_new_tenure(&parent.block) {
            break parent.block;
        }
        child_height = parent.block.header.chain_length;
        boundary_id = *parent.block.header.parent_block_id.as_bytes();
    };
    let proof = nano_chainstate::coinbase_vrf_proof(&boundary).ok_or_else(|| {
        format!(
            "checkpoint boundary tenure {} has no Nakamoto coinbase VRF proof",
            boundary.header.consensus_hash
        )
    })?;

    let boundary_json = serde_json::to_vec_pretty(&json!({
        "parent_tenure_consensus_hash": encode_hex(boundary.header.consensus_hash.as_bytes()),
        "coinbase_vrf_proof": encode_hex(&proof),
    }))
    .map_err(|error| format!("serialize checkpoint authentication boundary: {error}"))?;
    write_file(&output.join("boundary.json"), &boundary_json)?;
    reversed.reverse();
    for entry in &reversed {
        let block = &entry.block;
        write_file(
            &output.join("blocks").join(format!(
                "{:08}-{}.bin",
                block.header.chain_length,
                encode_hex(block.block_id().as_bytes())
            )),
            &entry.raw,
        )?;
    }
    Ok(reversed.len())
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

/// Refuse an earnings window a node could not pay from.
///
/// Contiguity, not a count. A count is what this used to check, and a count
/// cannot see the failure that actually happened: the live mainnet ledger
/// held 193 tenures spanning 201 heights with eight missing in the middle,
/// so by its outer bounds the window looked complete and long, and the
/// first payout it could not make was 27 tenures away — hours of execution,
/// all of it thrown away. The `continue`s above are how a hole gets in:
/// a tenure the archive cannot answer for is skipped rather than refused.
///
/// `last` is the deepest tenure the captured blocks belong to, and the window
/// has to reach it: a checkpoint whose earnings stop short of its own tip owes
/// nothing for the tenures between. One short of it is not, because a tenure's
/// entry needs its successor's row and the deepest tenure has none yet.
fn refuse_a_short_earnings_window(tenures: &[serde_json::Value], last: u64) -> Result<(), String> {
    let heights: Vec<u64> = tenures
        .iter()
        .filter_map(|tenure| tenure.get("coinbase_height")?.as_u64())
        .collect();
    let (Some(&lowest), Some(&highest)) = (heights.first(), heights.last()) else {
        return Err(format!(
            "the archive holds no tenure earnings at all for a checkpoint at coinbase \
             height {last}, so its first payout has nothing to derive from"
        ));
    };
    if let Some(missing) = (lowest..=highest).find(|height| !heights.contains(height)) {
        return Err(format!(
            "the archive has no scheduled payment for tenure {missing}, which is inside \
             the window {lowest}..{highest} this checkpoint would carry. A hole is not a \
             shorter window, it is a delayed failure: the tenures either side of it make \
             the window look complete, and the node stops at the one payout it cannot \
             derive, having sealed everything before it"
        ));
    }
    if highest + 1 < last {
        return Err(format!(
            "the earnings window ends at tenure {highest} and this checkpoint's blocks \
             reach tenure {last}, so the tenures between them would owe nothing"
        ));
    }
    let covered = highest - lowest + 1;
    if covered <= MINER_REWARD_MATURITY {
        return Err(format!(
            "the archive holds {covered} of the {} tenures a checkpoint at coinbase height \
             {last} needs: every tenure executed before nano's own mature pays out one of \
             them, so a short window fails at the first payout it cannot derive",
            MINER_REWARD_MATURITY + 1
        ));
    }
    Ok(())
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
                 reads only, and refuses a path that is not already a state\n\
             the node must not be running: its uncommitted pages are not readable"
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
    let mut vm = match open_state_vm(&Path::new(state).join("chainstate")) {
        Ok(vm) => vm,
        Err(error) => {
            eprintln!("cannot open the state: {error:?}");
            return ExitCode::FAILURE;
        }
    };
    let tip = match vm.tip() {
        Ok(Some(tip)) => tip,
        Ok(None) => {
            eprintln!("the state is sealed at no block, so there is nothing to compile against");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("cannot read the state tip: {error}");
            return ExitCode::FAILURE;
        }
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
    for stray in [
        "chainstate/marf.sqlite-wal",
        "chainstate/clarity.sqlite-wal",
    ] {
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
            println!(
                "{} is now a snapshot of {}",
                destination.display(),
                source.display()
            );
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
             reads only, and refuses a path that is not already a state\n\
             the node must not be running: its uncommitted pages are not readable"
        );
        return ExitCode::FAILURE;
    };
    let store = match open_state_store(&Path::new(state).join("chainstate")) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("cannot open the state: {error:?}");
            return ExitCode::FAILURE;
        }
    };
    let resolved = if block == "tip" {
        match store.tip() {
            Ok(tip) => tip,
            Err(error) => {
                eprintln!("cannot read the state tip: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        hex::decode(block)
            .ok()
            .and_then(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).ok())
    };
    let Some(block) = resolved else {
        eprintln!("the block must be 32 hexadecimal bytes, or `tip` on a sealed state");
        return ExitCode::FAILURE;
    };
    // Three answers, not two: the chain does not hold this key, or this state
    // could not be read. Collapsing the second into the first is what task 079
    // exists to stop, and a diagnostic is the last place it should happen.
    let value = match store.get(block, key) {
        Ok(Some(value)) => value,
        Ok(None) => {
            println!("no value for {key} at {}", hex::encode(block));
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("reading {key} at {} failed: {error}", hex::encode(block));
            return ExitCode::FAILURE;
        }
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
            "usage: cargo xtask rebuild-accounting <state-dir> <peer-urls> <tip-block-id> \
             <tip-tenure-height>\n\
             <peer-urls> may be a comma-separated list; a repair of a few hundred tenures \
             is thousands of requests, and one endpoint's rate limit is the whole of its \
             speed"
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
    let Some(tenures) = accounting
        .get("tenures")
        .and_then(serde_json::Value::as_array)
    else {
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
    if let Some(tenures) = accounting
        .get_mut("tenures")
        .and_then(serde_json::Value::as_array_mut)
    {
        for tenure in tenures.iter_mut() {
            let Some(height) = tenure
                .get("coinbase_height")
                .and_then(serde_json::Value::as_u64)
            else {
                continue;
            };
            // A tenure the walk did not reach in full is left alone: counting
            // part of one would replace a wrong number with another.
            let Some(fees) = counted.get(&height) else {
                continue;
            };
            let recorded = tenure
                .get("fees")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
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
    if let Err(error) = fs::copy(&path, &backup)
        .and_then(|_| fs::write(&path, serde_json::to_vec(&accounting).unwrap_or_default()))
    {
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
    peers: &str,
    tip: [u8; 32],
    tenure_height: u64,
    oldest: u64,
) -> Result<std::collections::BTreeMap<u64, u64>, String> {
    // Over the pool rather than one client. A walk of a few hundred tenures is
    // thousands of block requests, and sent down one connection to a hosted API the
    // rate limit *is* the repair's speed — one run was left going for 1h45m. The
    // spreading is `TenureSource`'s: consecutive requests go to different peers, a
    // throttled peer is set aside, and a peer that cannot serve one block costs a
    // request rather than the walk. It is safe over strangers because a block is
    // content-addressed and `SyncClient::block` refuses an answer that is not the
    // block asked for.
    let endpoints: Vec<String> = peers
        .split(',')
        .map(|peer| peer.trim().to_owned())
        .filter(|peer| !peer.is_empty())
        .collect();
    let pool = nano_sync::PeerPool::from_endpoints(&endpoints);
    if pool.is_empty() {
        return Err(format!("none of {peers} is a usable peer URL"));
    }
    println!("counting fees over {} peers", pool.len());
    let mut source = nano_sync::TenureSource::new(pool.into_clients());
    let runtime = tokio::runtime::Runtime::new().map_err(|error| format!("{error}"))?;

    let mut fees = std::collections::BTreeMap::new();
    let mut block_id = nano_primitives::StacksBlockId::from_bytes(tip);
    let mut height = tenure_height;
    let mut consensus = None;
    runtime.block_on(async {
        while height >= oldest {
            // Being turned away by every peer at once is not a reason to give up on a
            // repair that has to be complete to be worth anything: the throttles are
            // forgiven and the walk carries on, which is what a rate limit asks for.
            let mut block = Err(String::new());
            for attempt in 0..8u32 {
                match source.block(block_id).await {
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
                        source.forgive_throttles();
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
                    "tenure {height}: {} counted, {} to go, over {} peers",
                    fees.len(),
                    height.saturating_sub(oldest),
                    source.served_by()
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
        let mut marf = nano_marf::VersionedMarf::open_existing(&marf_path).ok()?;
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

    let mut chain = match open_state_as_the_node_left_it(&Path::new(state).join("chainstate")) {
        Ok(chain) => chain,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let tip = match chain.tip() {
        Ok(Some(tip)) => tip,
        Ok(None) => {
            eprintln!("the state is sealed at no block");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("cannot read the state tip: {error}");
            return ExitCode::FAILURE;
        }
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
    match ask_both_engines(
        chain.vm_mut(),
        tip,
        &caller,
        &identifier,
        function,
        &encoded,
    ) {
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

/// Open a state the way the node that wrote it stands on it.
///
/// `Vm::open` starts with an empty tenure-start map; the node seeds it from the
/// ledger committed with the tip's seal. Without that, every `get-block-info?`
/// and `get-tenure-info?` reaching a tenure below the tip answers Clarity `none`
/// where the chain answers a height — which made the interpreter look like the
/// engine that was wrong about mainnet block 8,706,194 when it was the only one
/// right.
fn open_state_as_the_node_left_it(directory: &Path) -> Result<nano_chainstate::ChainState, String> {
    // The chain the state names, not an assumption: a diagnostic opened as the
    // wrong network reads different boot principals and answers a different
    // `(chain-id)`, which is exactly the confusion these tools exist to remove.
    let mut chain = nano_chainstate::ChainState::open_existing(directory)
        .map_err(|error| format!("cannot open the state: {error:?}"))?;
    if let Some(tip) = chain
        .tip()
        .map_err(|error| format!("cannot read the state tip: {error}"))?
    {
        chain
            .recover_ledger_at(tip)
            .map_err(|error| format!("cannot read the ledger the tip sealed: {error:?}"))?;
    }
    Ok(chain)
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
            if interpreted {
                "interpreter"
            } else {
                "compiler"
            }
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
    let mut chain = match open_state_as_the_node_left_it(&chainstate) {
        Ok(chain) => chain,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let tip = match chain.tip() {
        Ok(Some(tip)) => tip,
        Ok(None) => {
            eprintln!("the state is sealed at no block");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("cannot read the state tip: {error}");
            return ExitCode::FAILURE;
        }
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
        match ask_both_engines_about(chain.vm_mut(), tip, transaction) {
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
        eprintln!(
            "usage: cargo xtask heal-contracts <state-dir>\n\
             writes to the state, and creates one if the path is not already there\n\
             the node must not be running: it holds the state open"
        );
        return ExitCode::FAILURE;
    };
    let mut vm = match open_state_vm_for_writing(&Path::new(state).join("chainstate")) {
        Ok(vm) => vm,
        Err(error) => {
            eprintln!("cannot open the state: {error:?}");
            return ExitCode::FAILURE;
        }
    };
    let tip = match vm.tip() {
        Ok(Some(tip)) => tip,
        Ok(None) => {
            eprintln!("the state is sealed at no block");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("cannot read the state tip: {error}");
            return ExitCode::FAILURE;
        }
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
        return Err(());
    };
    let Ok(mut ledger) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        eprintln!("the ledger committed with {block} does not read as JSON");
        return Err(());
    };
    let missing = match missing_tenures(&ledger) {
        Ok(missing) => missing,
        Err(error) => {
            eprintln!("{error}");
            return Err(());
        }
    };
    let restated = match restate_tenure_fees(&mut ledger, archive) {
        Ok(restated) => restated,
        Err(error) => {
            eprintln!("{error}");
            return Err(());
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
                return Err(());
            }
            Err(error) => {
                eprintln!("{error}");
                return Err(());
            }
        };
        let Some(tenures) = ledger
            .get_mut("accounting")
            .and_then(|accounting| accounting.get_mut("tenures"))
            .and_then(serde_json::Value::as_array_mut)
        else {
            eprintln!("the ledger names no tenures");
            return Err(());
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
        serde_json::to_string(&accounting)
            .unwrap_or_default()
            .as_bytes(),
    ) {
        Ok(accounting) => {
            if !missing_tenures(&ledger).is_ok_and(|missing| missing.is_empty()) {
                eprintln!("the repaired ledger for {block} still has a hole");
                return Err(());
            }
            let Some((first, last)) = accounting.known_earnings_span() else {
                eprintln!("the repaired ledger for {block} owes nothing");
                return Err(());
            };
            println!(
                "{block}: filled {} tenures, restated {restated} fee totals, window {first}..{last}",
                missing.len()
            );
        }
        Err(error) => {
            eprintln!("the repaired accounting for {block} does not read back: {error}");
            return Err(());
        }
    }
    write_ledger_row(side_store, block, &ledger)?;
    Ok((missing, restated))
}

/// Write one repaired ledger row back, and prove it went in as the node reads it.
fn write_ledger_row(side_store: &Path, block: &str, ledger: &serde_json::Value) -> Result<(), ()> {
    let encoded = serde_json::to_vec(ledger).unwrap_or_default();
    if let Err(error) = sqlite_script(
        side_store,
        &format!(
            "UPDATE chain_ledger SET data = x'{}' WHERE hex(block_id) = '{block}';\n",
            encode_hex(&encoded)
        ),
    ) {
        eprintln!("cannot write the repaired ledger for {block}: {error}");
        return Err(());
    }
    // Read back from the database, not from memory. The first attempt at
    // this validated what it was about to write and wrote it as the wrong
    // SQLite type, so every row passed and none could be read afterwards.
    match sqlite(
        side_store,
        &format!(
            "SELECT typeof(data), hex(data) FROM chain_ledger WHERE hex(block_id) = '{block}'"
        ),
    ) {
        Ok(written) => {
            let Some((kind, hex)) = written.trim().split_once('|') else {
                eprintln!("the repaired row for {block} did not read back");
                return Err(());
            };
            if kind != "blob" {
                eprintln!("the repaired row for {block} is a {kind} where the node reads bytes");
                return Err(());
            }
            if decode_hex(hex.trim()).as_deref() != Some(encoded.as_slice()) {
                eprintln!("the repaired row for {block} did not come back as it went in");
                return Err(());
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

// ─── the release report ────────────────────────────────────────────────────────
//
// Everything below serves `cargo xtask release-report`, which is the last item of
// tasks/053: "publish the exact commands, versions, checkpoint provenance and
// resulting conformance report".
//
// Written as a command rather than a document, because a document is a claim and
// a command is a measurement. The distinction the report exists to make is the
// one tasks/053 puts at its centre: **a suite where every mainnet test skipped
// looks identical to one where every mainnet test passed**. So the report does
// not parse skip lines and guess. It runs the conformance suite with
// `NANO_REQUIRE_MAINNET` set, which turns every `skip_gate` into a panic, and a
// green run under that variable is by construction a run in which every gate
// executed.

/// Where the workspace root is, from xtask's manifest.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask sits in the workspace root")
        .to_path_buf()
}

/// Run a command in the workspace and give back its trimmed stdout.
fn captured(program: &str, arguments: &[&str]) -> String {
    let Ok(output) = Command::new(program)
        .args(arguments)
        .current_dir(workspace_root())
        .output()
    else {
        return "unknown".to_owned();
    };
    if output.status.success() {
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    } else {
        "unknown".to_owned()
    }
}

/// One row of the report's gate table.
struct GateResult {
    command: String,
    passed: bool,
    detail: String,
    unrunnable: Vec<String>,
}

/// Run one gate and summarize what it said.
///
/// The summary is taken from libtest's own `test result:` line where there is
/// one and from the last line of output otherwise, so a gate that failed to
/// *build* cannot be reported as a gate that passed.
fn run_gate(command: &str, arguments: &[&str], environment: &[(&str, &str)]) -> GateResult {
    let mut printed = String::new();
    for (name, value) in environment {
        let _ = write!(printed, "{name}={value} ");
    }
    let _ = write!(printed, "{command} {}", arguments.join(" "));

    let mut process = Command::new(command);
    process.args(arguments).current_dir(workspace_root());
    for (name, value) in environment {
        process.env(name, value);
    }
    let Ok(output) = process.output() else {
        return GateResult {
            command: printed,
            passed: false,
            detail: format!("{command} could not be started"),
            unrunnable: Vec::new(),
        };
    };
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // Every `test result:` line, because one cargo invocation over several
    // packages prints one per test binary.
    let results: Vec<&str> = combined
        .lines()
        .filter_map(|line| line.trim().strip_prefix("test result: "))
        .collect();
    let mut detail = if results.is_empty() {
        combined
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("no output")
            .trim()
            .to_owned()
    } else {
        results.join("; ")
    };
    let (unrunnable, broken) = if output.status.success() {
        (BTreeMap::new(), Vec::new())
    } else {
        classify_failures(&combined)
    };
    if !output.status.success() {
        if !unrunnable.is_empty() {
            let total: usize = unrunnable.values().map(Vec::len).sum();
            let _ = write!(
                detail,
                "\n           {total} gate(s) could not run, so the run is not \
                 evidence for them:"
            );
            for (reason, tests) in &unrunnable {
                let _ = write!(detail, "\n             {} × {reason}", tests.len());
                for test in tests {
                    let _ = write!(detail, "\n                 {test}");
                }
            }
        }
        if !broken.is_empty() {
            let _ = write!(
                detail,
                "\n           {} gate(s) ran and FAILED: {}",
                broken.len(),
                broken.join(", ")
            );
        }
    }
    GateResult {
        command: printed,
        passed: output.status.success(),
        detail,
        unrunnable: unrunnable.into_values().flatten().collect(),
    }
}

/// Split a failing test run into gates that could not run and gates that broke.
///
/// The whole point of `NANO_REQUIRE_MAINNET` is that a gate whose fixture is
/// absent must not report itself green — so under it, an absent fixture arrives
/// as a *failure*, and a report that stopped there would say the same thing about
/// a missing environment variable as about a wrong state root. `skip_gate`'s
/// panic message is distinctive, so the two are separable: unrunnable gates are
/// grouped by the reason they gave, and everything else is a real failure.
///
/// Returns (reason → test names, other failing test names).
fn classify_failures(output: &str) -> (BTreeMap<String, Vec<String>>, Vec<String>) {
    const CANNOT_RUN: &str = "this gate cannot run and NANO_REQUIRE_MAINNET is set: ";
    let mut unrunnable: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut broken = Vec::new();
    // libtest prints one `---- <name> stdout ----` block per failure, and the
    // panic message is inside it.
    let mut blocks = output.split("---- ");
    blocks.next();
    for block in blocks {
        let Some((header, body)) = block.split_once('\n') else {
            continue;
        };
        let Some(name) = header.strip_suffix(" stdout ----") else {
            continue;
        };
        // The trailing summary repeats each name on its own line; only the
        // blocks carry a body worth reading.
        match body.split(CANNOT_RUN).nth(1) {
            Some(rest) => {
                let reason = rest.lines().next().unwrap_or("unstated").trim();
                unrunnable
                    .entry(reason.to_owned())
                    .or_default()
                    .push(name.to_owned());
            }
            None => broken.push(name.to_owned()),
        }
    }
    (unrunnable, broken)
}

/// The version the lock file pins for a crate, if it pins exactly one.
fn locked_version(crate_name: &str) -> String {
    let Ok(lock) = fs::read_to_string(workspace_root().join("Cargo.lock")) else {
        return "no lock file".to_owned();
    };
    for package in lock.split("\n[[package]]").skip(1) {
        let field = |key: &str| {
            package.lines().find_map(|line| {
                line.strip_prefix(&format!("{key} = \""))
                    .and_then(|value| value.strip_suffix('"'))
            })
        };
        if field("name") != Some(crate_name) {
            continue;
        }
        let version = field("version").unwrap_or("?");
        // A git source's revision is what identifies it; a registry version
        // identifies itself.
        return field("source")
            .and_then(|source| source.split("rev=").nth(1))
            // A locked git source spells the revision twice, as `?rev=X#X`.
            .map(|revision| revision.split('#').next().unwrap_or(revision))
            .map_or_else(
                || version.to_owned(),
                |revision| format!("{version} (stacks-core {revision})"),
            );
    }
    "not in the lock file".to_owned()
}

fn report_revision() {
    println!("\nrevision");
    println!(
        "  nano-stacks          {}",
        captured("git", &["rev-parse", "HEAD"])
    );
    let dirty = !captured("git", &["status", "--porcelain"]).is_empty();
    println!(
        "  branch               {} ({})",
        captured("git", &["rev-parse", "--abbrev-ref", "HEAD"]),
        if dirty {
            "UNCOMMITTED CHANGES"
        } else {
            "clean"
        }
    );
    println!(
        "  rustc                {}",
        captured("rustc", &["--version"])
    );
    println!(
        "  cargo                {}",
        captured("cargo", &["--version"])
    );
}

/// A supplied release capture is evidence only if production can seed from it.
fn report_capture_validation(capture: Option<&str>) -> bool {
    println!("\ncapture validation");
    let Some(capture) = capture else {
        println!("  no capture supplied; no capture-backed claim is made here");
        return true;
    };
    let root = Path::new(capture);
    match validate_fixture_tree(root) {
        Ok(FixtureStatus::Captured { replay_blocks }) => {
            match describe_fixture(root, replay_blocks) {
                Ok(description) => {
                    for line in description.lines() {
                        println!("  {line}");
                    }
                    true
                }
                Err(error) => {
                    println!("  FAIL: {error}");
                    false
                }
            }
        }
        Ok(FixtureStatus::Baseline { .. }) => {
            println!("  FAIL: the supplied tree is an empty baseline, not a capture");
            false
        }
        Err(error) => {
            println!("  FAIL: {error}");
            false
        }
    }
}

/// The engine, named by content.
///
/// tasks/060 asks for the clarity-wasm and compiler revisions by name. The
/// compiler is vendored in-tree rather than pinned as a git dependency, so its
/// revision is the *tree hash* of `vendor/clarity-wasm` — a content hash of
/// exactly the source that was compiled, which a commit id of the whole
/// repository is not.
fn report_engines() {
    println!("\nengines");
    println!(
        "  clarity-wasm         tree {}",
        captured("git", &["rev-parse", "HEAD:vendor/clarity-wasm"])
    );
    println!(
        "  clarity-wasm change  {}",
        captured(
            "git",
            &["log", "-1", "--format=%h %s", "--", "vendor/clarity-wasm"]
        )
    );
    // The identity the *binary* carries, which is the one a state and a fixture
    // can be bound to. The tree hash above is a claim about the repository; this
    // is a hash of the sources that were compiled, so it is also right in a tree
    // with uncommitted changes to the compiler -- which is exactly the tree where
    // the two disagree and where the difference matters.
    println!("  compiler identity    {}", nano_vm::COMPILER_IDENTITY);
    for crate_name in ["wasmtime", "clarity", "stackslib"] {
        println!("  {crate_name:<20} {}", locked_version(crate_name));
    }
    println!(
        "  execution            Clarity contracts enter clarity-wasm only; stacks-core's \
         frontend/ABI types and the native STX-transfer helper remain linked"
    );
}

/// Put the frozen receipt baseline and this compiler beside each other.
fn report_receipt_binding() -> bool {
    let path = workspace_root().join("crates/nano-conformance/fixtures/mainnet/receipts.json");
    let fixture = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value["compiler"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "UNNAMED".to_owned());
    println!("\nfrozen mainnet receipt slice");
    println!("  baseline compiler    {fixture}");
    println!("  artifact compiler    {}", nano_vm::COMPILER_IDENTITY);
    println!(
        "  binding              {}",
        if fixture == nano_vm::COMPILER_IDENTITY {
            "same compiler; a re-freeze must explain why the baseline moved"
        } else {
            "different compilers; the baseline is intentionally testing this artifact"
        }
    );
    fixture != "UNNAMED"
}

struct ObservedEpochReceipt {
    height: u64,
    txid: String,
    runtime: u64,
    events: usize,
}

fn reference_epoch_snapshot(root: &Path) -> Result<(), String> {
    let path = root.join("crates/nano-conformance/tests/conformance/at_block_refusal.rs");
    let source = fs::read_to_string(path).map_err(|error| error.to_string())?;
    for required in [
        "runtime_check_error_kind_at_block_unavailable_ccall",
        "STACKS_CORE_REFUSAL_COST",
        "runtime: 275",
    ] {
        if !source.contains(required) {
            return Err(format!("reference snapshot is missing {required}"));
        }
    }
    Ok(())
}

fn observed_epoch_receipt(root: &Path) -> Result<ObservedEpochReceipt, String> {
    let directory = root.join("crates/nano-conformance/fixtures/mainnet/divergence");
    let receipt_path = directory.join("tx-f338-receipt.json");
    let receipt: serde_json::Value = serde_json::from_slice(
        &fs::read(&receipt_path).map_err(|error| format!("{}: {error}", receipt_path.display()))?,
    )
    .map_err(|error| format!("{}: {error}", receipt_path.display()))?;
    let expected_txid = "f33840c54f18a314f00b1338bc3d43e3103cbe9ce424d0418e94bc463903fe62";
    if receipt["source"]
        .as_str()
        .is_none_or(|source| !source.starts_with("https://api.hiro.so/"))
        || receipt["txid"].as_str() != Some(expected_txid)
        || receipt["block_height"].as_u64() != Some(8_686_666)
        || receipt["canonical"].as_bool() != Some(true)
        || receipt["status"].as_str() != Some("success")
        || receipt["result"]["repr"].as_str() != Some("(ok u12395909)")
    {
        return Err("observed receipt identity or result is not canonical".to_owned());
    }
    for (dimension, expected) in [
        ("read_count", 53),
        ("read_length", 48_263),
        ("runtime", 87_814),
        ("write_count", 8),
        ("write_length", 83),
    ] {
        if receipt["cost"][dimension].as_u64() != Some(expected) {
            return Err(format!("observed receipt has the wrong {dimension}"));
        }
    }
    let events = receipt["events"]
        .as_array()
        .ok_or_else(|| "observed receipt has no ordered event list".to_owned())?
        .len();
    if events != 4 {
        return Err(format!("observed receipt has {events} events instead of 4"));
    }
    let block_path = directory.join("block-8686666.hex");
    let block = fs::read_to_string(&block_path)
        .map_err(|error| format!("{}: {error}", block_path.display()))?;
    if hex::decode(block.trim()).is_err() {
        return Err(format!(
            "{} is not a hexadecimal block",
            block_path.display()
        ));
    }
    Ok(ObservedEpochReceipt {
        height: 8_686_666,
        txid: expected_txid.to_owned(),
        runtime: 87_814,
        events,
    })
}

fn report_historical_epoch_evidence() -> bool {
    println!("\nhistorical epoch evidence");
    let root = workspace_root();
    let reference = reference_epoch_snapshot(&root);
    match &reference {
        Ok(()) => {
            println!("  reference snapshot   PASS stacks-core at-block refusal (runtime 275)");
        }
        Err(error) => {
            println!("  reference snapshot   FAIL {error}");
        }
    }
    let observed = observed_epoch_receipt(&root);
    match &observed {
        Ok(receipt) => {
            println!(
                "  observed mainnet     PASS block {} tx {} (runtime {}, {} events)",
                receipt.height, receipt.txid, receipt.runtime, receipt.events
            );
        }
        Err(error) => {
            println!("  observed mainnet     MISSING/INVALID {error}");
        }
    }
    println!(
        "  replay gate          mainnet_divergence::the_mainnet_8686666_old_epoch_receipt_and_root_match_the_canonical_oracle"
    );
    reference.is_ok() && observed.is_ok()
}

/// Every `#[ignore]` in the execution engine's own suite and in the conformance
/// suite, with the reason it gives.
///
/// tasks/060: "account for every ignored Clarity semantic differential in the
/// release report. A known engine disagreement may not be waived merely because it
/// has not appeared in the replayed mainnet window." A prose list would go stale
/// the first time somebody added one, so this is a scan: it reads the reasons out
/// of the sources and splits them by whether the reason is *infrastructure* — a
/// test that needs a running node, a network or a fixture nobody has — or a
/// **semantic** one, which is a disagreement about what Clarity means and is a
/// failed release gate rather than a skipped test.
///
/// The split is read from `ignored-tests.toml` and not guessed from the reason.
/// It used to be guessed, by looking for substrings — and one of the markers was
/// `needs to be implemented`, which filed "Clarity 4 costs needs to be
/// implemented" under *environment*. A reason string is prose; it is not a policy.
/// A reason the inventory does not list is `unclassified`, which counts against
/// the release exactly as `semantic` does, so the undecided case cannot be the
/// quiet one.
fn report_differentials(inventory: &ReleaseInventory) -> usize {
    println!("\nignored tests");
    let mut blocking = Vec::new();
    let mut infrastructure = 0usize;
    let mut tools = 0usize;
    for test in &inventory.ignored_tests {
        let policy = inventory.ignored_policy(&test.name);
        match policy.map(|policy| policy.class.as_str()) {
            Some("infrastructure") => infrastructure += 1,
            Some("covered" | "tool" | "out-of-scope") => tools += 1,
            Some(class) => blocking.push((
                test.path.clone(),
                format!("{}: {}", test.name, test.reason),
                class.to_owned(),
                policy.map_or("UNOWNED", |policy| policy.owner.as_str()),
            )),
            None => blocking.push((
                test.path.clone(),
                format!("{}: {}", test.name, test.reason),
                "unclassified".to_owned(),
                "UNOWNED",
            )),
        }
    }
    blocking.sort();
    println!(
        "  infrastructure       {infrastructure} (a service, network or fixture this machine \
         does not have; every one names the job that supplies it)"
    );
    println!(
        "  covered / tools / out-of-scope {tools} (covered by an unconditional replacement, \
         asserts no required behaviour, or a word epoch 4.0 removed)"
    );
    if blocking.is_empty() {
        println!("  blocking             0 -- nothing required is waived by being skipped");
        return 0;
    }
    println!(
        "  blocking             {} -- each one is a failed release gate, not a skipped test",
        blocking.len()
    );
    let count = blocking.len();
    for (path, reason, class, owner) in blocking {
        println!("    [{class}] {path}: {reason} (owner task {owner})");
    }
    count
}

/// Running tests that intentionally assert the engines are unequal.
fn report_declared_differentials() -> usize {
    println!("\ndeclared semantic differentials");
    let Ok(text) = fs::read_to_string(workspace_root().join("known-differentials.toml")) else {
        println!("  FAIL: known-differentials.toml is absent or unreadable");
        return 1;
    };
    let mut entries = Vec::new();
    let mut current = BTreeMap::new();
    for line in text.lines().map(str::trim) {
        if line == "[[differential]]" {
            if !current.is_empty() {
                entries.push(std::mem::take(&mut current));
            }
        } else if let Some((name, value)) = line.split_once(" = ")
            && let Some(value) = value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
        {
            current.insert(name.to_owned(), value.to_owned());
        }
    }
    if !current.is_empty() {
        entries.push(current);
    }
    if entries.is_empty() {
        println!("  0 -- no running test accepts unequal engine answers");
        return 0;
    }
    println!(
        "  {} blocking -- these tests measure unequal answers; green means unchanged, not equal",
        entries.len()
    );
    for entry in &entries {
        println!(
            "    {} ({}, owner task {}, {})",
            entry.get("test").map_or("UNNAMED", String::as_str),
            entry.get("file").map_or("no file", String::as_str),
            entry.get("owner").map_or("none", String::as_str),
            entry.get("surface").map_or("no surface", String::as_str),
        );
    }
    entries.len()
}

/// Run every ignored test classified as infrastructure, and no waived semantics.
///
/// Cargo supplies one filter string but any number of `--skip` filters, so one
/// workspace invocation can execute the complete infrastructure class without
/// starting a Cargo process per test. The release runner supplies their services.
fn infrastructure_tests() -> ExitCode {
    let inventory = ReleaseInventory::load(&workspace_root());
    if !inventory.errors.is_empty() {
        eprintln!(
            "the release inventory is invalid:\n  {}",
            inventory.errors.join("\n  ")
        );
        return ExitCode::FAILURE;
    }
    let mut arguments = vec![
        "test".to_owned(),
        "--release".to_owned(),
        "--workspace".to_owned(),
        "--".to_owned(),
        "--ignored".to_owned(),
        "--test-threads=1".to_owned(),
    ];
    let mut skipped: Vec<String> = inventory
        .ignored
        .iter()
        .filter(|policy| policy.class != "infrastructure")
        .map(|policy| policy.test.clone())
        .collect();
    skipped.sort();
    for test in skipped {
        arguments.push("--skip".to_owned());
        arguments.push(test);
    }
    Command::new("cargo")
        .args(&arguments)
        .current_dir(workspace_root())
        .status()
        .map_or_else(
            |error| {
                eprintln!("could not run the infrastructure tests: {error}");
                ExitCode::FAILURE
            },
            |status| {
                if status.success() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            },
        )
}

fn infrastructure_gate(environment: &[(&str, &str)]) -> GateResult {
    let binary = env::current_exe().ok();
    binary.map_or_else(
        || GateResult {
            command: "cargo xtask infrastructure-tests".to_owned(),
            passed: false,
            detail: "the xtask binary cannot be found".to_owned(),
            unrunnable: Vec::new(),
        },
        |binary| {
            run_gate(
                binary.to_string_lossy().as_ref(),
                &["infrastructure-tests"],
                environment,
            )
        },
    )
}

fn report_artifact(binary: &Path) -> bool {
    println!("\nartifact");
    match fs::read(binary) {
        Ok(bytes) => {
            println!("  path                 {}", binary.display());
            println!("  bytes                {}", bytes.len());
            println!(
                "  sha256               {}",
                hex::encode(nano_primitives::sha256(&bytes).as_bytes())
            );
            let embedded = bytes
                .windows(nano_vm::COMPILER_IDENTITY.len())
                .any(|window| window == nano_vm::COMPILER_IDENTITY.as_bytes());
            println!(
                "  embedded compiler    {}",
                if embedded {
                    nano_vm::COMPILER_IDENTITY
                } else {
                    "MISSING"
                }
            );
            embedded
        }
        Err(error) => {
            println!("  path                 {} ({error})", binary.display());
            false
        }
    }
}

/// Build the artifact this report hashes, rather than describing target/ leftovers.
fn build_release_artifact() -> bool {
    println!("\nbuild");
    let gate = run_gate(
        "cargo",
        &["build", "--release", "--bin", "stacks-node"],
        &[],
    );
    println!("  {}", gate.command);
    println!(
        "    {:<6} {}",
        if gate.passed { "pass" } else { "FAIL" },
        gate.detail
    );
    gate.passed
}

fn report_contract_arities(state: Option<&Path>, qualifying: bool) -> bool {
    println!("\ncontract arity inventory");
    let Some(state) = state else {
        println!("  UNEXECUTED: no --state directory, so no full-state locals/arity claim");
        return !qualifying;
    };
    match contract_inventory(state) {
        Ok(inventory) => {
            print_contract_inventory(&inventory);
            if inventory.passes() {
                println!("  PASS: every named contract was measured and its module loads");
                true
            } else {
                println!(
                    "  FAIL: a named contract was unmeasured or refused; this state does not \
                     support the no-arity-refusal release claim"
                );
                false
            }
        }
        Err(error) => {
            println!("  FAIL: {error}");
            false
        }
    }
}

fn report_checkpoint(state: Option<&Path>) {
    println!("\ncheckpoint provenance");
    let Some(directory) = state else {
        println!(
            "  no --state directory given, so no provenance is claimed. \
             docs/checkpoint-trust.md is the procedure."
        );
        return;
    };
    // A node records its provenance beside the MARF it imported, which is
    // `<state>/chainstate`, and an operator naturally names the state directory.
    // Both are accepted rather than making the caller know which.
    let found = [directory.to_path_buf(), directory.join("chainstate")]
        .into_iter()
        .find_map(
            |candidate| match nano_marf::CheckpointProvenance::load(&candidate) {
                Ok(Some(provenance)) => Some((candidate, Ok(Some(provenance)))),
                Ok(None) => None,
                Err(error) => Some((candidate, Err(error))),
            },
        );
    let (directory, loaded) = found.unwrap_or_else(|| (directory.to_path_buf(), Ok(None)));
    match loaded {
        Ok(Some(provenance)) => {
            let checkpoint = &provenance.checkpoint;
            println!("  state directory      {}", directory.display());
            println!("  format               {}", checkpoint.format);
            println!("  stacks_height        {}", checkpoint.stacks_height);
            println!(
                "  source_state_id      {}",
                hex::encode(checkpoint.source_state_id)
            );
            println!(
                "  state_index_root     {}",
                hex::encode(checkpoint.state_index_root.as_bytes())
            );
            println!("  first_bitcoin_height {}", checkpoint.first_bitcoin_height);
            match &provenance.attestation {
                Some(attestation) => {
                    println!(
                        "  attesting_block_id   {}",
                        hex::encode(attestation.attesting_block_id)
                    );
                    println!(
                        "  signer_weight        {} against a threshold of {}",
                        attestation.signer_weight, attestation.approval_threshold
                    );
                }
                None => println!(
                    "  attestation          NONE — the root was taken on trust rather \
                     than from a signed header"
                ),
            }
        }
        Ok(None) => println!(
            "  {} carries no checkpoint-provenance.toml, so what it descends from is \
             unrecorded",
            directory.display()
        ),
        Err(error) => println!("  {} is unreadable: {error}", directory.display()),
    }
    report_state_engines(&directory);
}

/// Which clarity-wasm builds wrote the state, and whether this artifact is one.
///
/// A checkpoint's manifest describes where the state *started*; every root beyond
/// it is a compiler's arithmetic, so the compiler belongs in the same paragraph.
/// A state naming a build other than the artifact's is not an error — a compiler
/// fix is an ordinary event — but a release that claims those roots as evidence
/// for *this* binary has to say which builds produced them.
fn report_state_engines(directory: &Path) {
    let recorded = nano_vm::recorded_engine_identities(directory);
    if recorded.is_empty() {
        println!(
            "  engine identity      UNRECORDED — this state was written before a build \
             stamped one, so which compiler produced its roots is not knowable from it"
        );
        return;
    }
    for (identity, first_seen) in &recorded {
        println!(
            "  engine identity      {identity} (first opened at unix {first_seen}){}",
            if identity == nano_vm::COMPILER_IDENTITY {
                " — this artifact"
            } else {
                ""
            }
        );
    }
    if !recorded
        .iter()
        .any(|(identity, _)| identity == nano_vm::COMPILER_IDENTITY)
    {
        println!(
            "  engine identity      NONE of the above is this artifact's compiler, so its \
             roots are another build's work"
        );
    }
}

/// The scoreboard, and a count of what the replay said while producing it.
///
/// Run as a subprocess rather than in-process, for one reason: the fixture replay
/// writes a diagnostic per tenure -- `carries a coinbase proof this node cannot
/// check`, `commits a seed this node cannot check` -- and a captured chain has
/// hundreds of tenures. Those lines used to land in the middle of this report,
/// between the artifact digest and the six-line table somebody is reading it for.
///
/// They are not noise on a *node*, where an unavailable leader-key registration is
/// a missing checkpoint input, and nothing here silences them there: production is
/// untouched, and the same message on a node still prints once a tenure. What is
/// wrong is printing them here, where they are expected -- a capture carries no
/// registry for keys registered years before it -- and where they bury the decision.
///
/// So they are counted by shape and reported as counts. A reader who wants them
/// runs `cargo xtask scoreboard`.
fn report_scoreboard() -> bool {
    println!("\nscoreboard");
    println!(
        "  NANO_REPLAY_BOTH_ENGINES=1: every captured contract call is compared with the \
         interpreter before sealing (required semantic gate, owner task 060)"
    );
    let manifest_path = fixture_root().join("manifest.toml");
    if let Err(error) = FixtureManifest::load(&manifest_path) {
        println!(
            "  no fixture manifest at {}: {error}",
            manifest_path.display()
        );
        return false;
    }
    let Ok(binary) = env::current_exe() else {
        println!("  cannot find this binary to run the scoreboard with");
        return false;
    };
    let Ok(run) = Command::new(binary)
        .arg("scoreboard")
        .env("NANO_REPLAY_BOTH_ENGINES", "1")
        .output()
    else {
        println!("  the scoreboard could not be run");
        return false;
    };
    for line in String::from_utf8_lossy(&run.stdout).lines() {
        println!("  {line}");
    }
    // The subprocess's exit status, not the table's appearance. A report that
    // printed a divergence and then went on to describe the artifact as though the
    // replay had passed is the failure mode 075 is named for.
    if !run.status.success() {
        println!(
            "  FAIL   a required surface diverged from its oracle: this replay does not \
             support a release"
        );
    }
    report_replay_diagnostics(&String::from_utf8_lossy(&run.stderr));
    run.status.success()
}

/// What the replay said, by shape rather than by line.
fn report_replay_diagnostics(stderr: &str) {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for line in stderr.lines().filter(|line| !line.trim().is_empty()) {
        // Keyed by the phrase that names the condition, so one entry covers every
        // tenure it happened at rather than one entry per tenure.
        let shape = if line.contains("carries a coinbase proof this node cannot check") {
            "tenures whose coinbase proof could not be checked: no leader-key registration"
        } else if line.contains("commits a seed this node cannot check") {
            "tenures whose committed seed could not be checked: no parent tenure proof"
        } else if line.contains("carries a miner signature this node cannot check") {
            "tenures whose miner signature could not be checked: no registered signing key"
        } else if line.contains("carries signer signatures this node cannot check") {
            "tenures whose signer signatures could not be checked: no recorded signer set"
        } else {
            "other lines the replay wrote"
        };
        *counts.entry(shape).or_default() += 1;
    }
    if counts.is_empty() {
        return;
    }
    println!("\n  what the fixture replay reported while producing that table");
    println!("  (expected of a capture, and *not* silenced on a node -- see tasks/076)");
    for (shape, count) in counts {
        println!("    {count:>5}  {shape}");
    }
}

/// Every `NANO_*` variable this run was given, without secret material.
///
/// Part of "the exact commands": most of the mainnet gates take their inputs from
/// the environment, so a report that printed only the command line would be
/// describing a different run from the one it made. `run_gate` inherits this
/// environment, which is what lets an operator hand the suite more fixtures
/// without the report knowing their names.
fn report_inputs() {
    println!("\ninputs");
    let mut names: Vec<(String, String)> = env::vars()
        .filter(|(name, _)| name.starts_with("NANO_"))
        .collect();
    names.sort();
    if names.is_empty() {
        println!("  none, so every gate that needs one will report that it could not run");
    }
    for (name, value) in names {
        let shown = if name == "NANO_FUNDED_KEY"
            || name == "NANO_MAINNET_KEY"
            || name.contains("PRIVATE_KEY")
            || name.contains("PASSWORD")
            || name.contains("SECRET")
            || name.contains("TOKEN")
        {
            "<redacted>"
        } else {
            &value
        };
        println!("  {name:<24} {shown}");
    }
}

fn report_release_inventory(inventory: &ReleaseInventory, run_gates: bool) -> bool {
    println!("\nrelease test inventory");
    let required = inventory.required_conditionals().count();
    let diagnostics = inventory.conditionals.len().saturating_sub(required);
    let mut modules = BTreeMap::new();
    for policy in &inventory.conditionals {
        let module = policy
            .site
            .split_once("::")
            .map_or("UNNAMED", |(name, _)| name);
        *modules.entry(module).or_insert(0usize) += 1;
    }
    println!(
        "  {required} required gates and {diagnostics} optional diagnostics across {} files are owned in \
         conditional-tests.toml.",
        modules.len()
    );
    println!(
        "  {} ignored infrastructure tests are owned in ignored-tests.toml.",
        inventory.infrastructure_ignored().count()
    );
    for (name, count) in modules {
        println!("    {name:<34} {count}");
    }
    for error in &inventory.errors {
        println!("  FAIL: {error}");
    }
    if !run_gates {
        println!("\n  UNEXECUTED: --no-gates is not release qualification.");
        for policy in inventory.infrastructure_ignored() {
            println!(
                "    ignored test {} (owner task {})",
                policy.test, policy.owner
            );
        }
        for policy in inventory.required_conditionals() {
            println!(
                "    conditional site {} (owner task {})",
                policy.site, policy.owner
            );
        }
    }
    required > 0 && inventory.errors.is_empty()
}

/// Run the three gates tasks/053 names and report what each said.
fn report_gates(capture: Option<&str>, inventory: &ReleaseInventory) -> bool {
    println!("\ngates");
    println!("  Each command below also inherits every variable under `inputs`.");
    let mut environment: Vec<(&str, &str)> = vec![("NANO_REQUIRE_MAINNET", "1")];
    match capture {
        Some(path) => environment.push(("NANO_MAINNET_CAPTURE", path)),
        None => println!(
            "  no --capture and no NANO_MAINNET_CAPTURE, so the mainnet gates cannot run \
             and the\n  conformance gate below is expected to FAIL under \
             NANO_REQUIRE_MAINNET. That failure is\n  the honest report."
        ),
    }
    let gates = [
        run_gate(
            "cargo",
            &[
                "test",
                "--release",
                "-p",
                "nano-vm",
                "-p",
                "nano-rpc",
                "-p",
                "nano-node",
            ],
            &[],
        ),
        run_gate(
            "cargo",
            &[
                "test",
                "--release",
                "-p",
                "nano-conformance",
                "--test",
                "conformance",
            ],
            &environment,
        ),
        infrastructure_gate(&environment),
        run_gate(
            "cargo",
            &[
                "clippy",
                "--release",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
            &[],
        ),
    ];
    let mut all_passed = true;
    for gate in &gates {
        all_passed &= gate.passed;
        println!("\n  {}", gate.command);
        println!(
            "    {:<6} {}",
            if gate.passed { "pass" } else { "FAIL" },
            gate.detail
        );
        for test in &gate.unrunnable {
            let owner = inventory
                .conditional_owner(test)
                .or_else(|| inventory.ignored_owner(test))
                .unwrap_or("UNOWNED");
            println!("      unexecuted {test} (owner task {owner})");
        }
    }
    all_passed
}

fn release_report(arguments: &[String]) -> ExitCode {
    let mut capture = env::var("NANO_MAINNET_CAPTURE").ok();
    let mut state: Option<PathBuf> = None;
    let mut artifact: Option<PathBuf> = None;
    let mut run_gates = true;
    let mut rest = arguments.iter();
    while let Some(flag) = rest.next() {
        match flag.as_str() {
            "--capture" => capture = rest.next().cloned(),
            "--state" => state = rest.next().map(PathBuf::from),
            "--artifact" => artifact = rest.next().map(PathBuf::from),
            "--no-gates" => run_gates = false,
            other => {
                eprintln!(
                    "usage: cargo xtask release-report [--capture <dir>] [--state <dir>] \
                     [--artifact <stacks-node>] [--no-gates]\nunexpected argument: {other}"
                );
                return ExitCode::from(2);
            }
        }
    }

    println!("nano-stacks release report");
    println!(
        "  generated            {} (unix {})",
        captured("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs())
    );
    println!("\nwhat this report cannot establish");
    println!("  It is not evidence for holding mainnet tip for 24 hours, a live Bitcoin");
    println!("  reorganization, or a stock stacks-signer run against this binary.");
    println!("  Those require the named task-053 qualification runs.");
    report_revision();
    if !report_capture_validation(capture.as_deref()) {
        println!(
            "\nrelease qualification stopped: an invalid capture cannot support replay or \
             artifact evidence"
        );
        return ExitCode::FAILURE;
    }
    let inventory = ReleaseInventory::load(&workspace_root());
    report_engines();
    let receipt_binding = report_receipt_binding();
    let historical_epoch_evidence = report_historical_epoch_evidence();
    let blocking = report_differentials(&inventory) + report_declared_differentials();
    let built = artifact.is_some() || build_release_artifact();
    let artifact_path =
        artifact.unwrap_or_else(|| workspace_root().join("target/release/stacks-node"));
    let artifact = report_artifact(&artifact_path);
    let contract_arities = report_contract_arities(state.as_deref(), run_gates);
    report_checkpoint(state.as_deref());
    let scoreboard = report_scoreboard();
    report_inputs();
    let release_inventory = report_release_inventory(&inventory, run_gates);

    let passed = if run_gates {
        report_gates(capture.as_deref(), &inventory)
    } else {
        println!("\ngates");
        println!(
            "  --no-gates: required test commands were not run; this report is explicitly \
             non-qualifying."
        );
        false
    };

    // A blocking ignore is a failed gate whether or not the gates that ran passed.
    // Printing the count and exiting zero is how a waived cost differential rode
    // along in a green report for as long as it did.
    if blocking > 0 {
        println!(
            "\n  {blocking} ignored or declared semantic differential(s) remain, so this \
             report fails whatever else passed. See the release inventories."
        );
    }
    if !run_gates {
        return if built
            && artifact
            && contract_arities
            && scoreboard
            && receipt_binding
            && historical_epoch_evidence
            && release_inventory
        {
            ExitCode::from(2)
        } else {
            ExitCode::FAILURE
        };
    }
    if passed
        && blocking == 0
        && artifact
        && contract_arities
        && scoreboard
        && receipt_binding
        && historical_epoch_evidence
        && release_inventory
    {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Freeze a bounded slice of an observer's `new_block` stream as a regression
/// fixture: one digest a block, over the receipts rather than over the payload.
///
/// The mainnet capture holds no `new_block` events and cannot be made to — that
/// stream only exists if somebody was listening while the chain executed, and no
/// public API serves it for a historical block. So this is deliberately **not** an
/// oracle: it is nano's own receipts, from blocks whose `state_index_root` the
/// chain verified before they were sealed. What it catches is the one failure a
/// root cannot: a compiler change that alters a receipt, a cost dimension or an
/// event without altering any state, which is exactly what a refused contract call
/// does — it writes nothing and seals the root an untouched block seals.
///
/// A digest and not the payloads, because 500 mainnet blocks of receipts are
/// 250 MB and this has to live in CI. Any change to a status, a cost dimension, an
/// event or the block's own identity moves it.
/// Name the compiler in a clarity-wasm source tree, this build's or another's.
///
/// With no argument it prints what this binary was built from, which is what a
/// state and a fixture get stamped with. With a directory it computes the same
/// identity for that tree — so the compiler behind a fixture frozen before this
/// existed can be recovered rather than asserted:
///
/// ```sh
/// git archive <commit> vendor/clarity-wasm | tar -x -C /tmp/old
/// cargo xtask compiler-identity /tmp/old/vendor/clarity-wasm
/// ```
fn compiler_identity(arguments: &[String]) -> ExitCode {
    match arguments {
        [] => {
            println!("{}", nano_vm::COMPILER_IDENTITY);
            ExitCode::SUCCESS
        }
        [directory] => nano_vm::compiler_identity_of(Path::new(directory)).map_or_else(
            || {
                eprintln!("{directory} is not a readable clarity-wasm source tree");
                ExitCode::FAILURE
            },
            |identity| {
                println!("{identity}");
                ExitCode::SUCCESS
            },
        ),
        _ => {
            eprintln!("usage: cargo xtask compiler-identity [<clarity-wasm-dir>]");
            ExitCode::from(2)
        }
    }
}

fn freeze_receipts(arguments: &[String]) -> ExitCode {
    let [observer, output, rest @ ..] = arguments else {
        eprintln!(
            "usage: cargo xtask freeze-receipts <observer-dir> <out.json> [first-height] [count]"
        );
        return ExitCode::from(2);
    };
    let first: u64 = rest
        .first()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let count: usize = rest
        .get(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(500);
    let directory = PathBuf::from(observer).join("new_block");
    let mut paths: Vec<PathBuf> = match fs::read_dir(&directory) {
        Ok(entries) => entries
            .filter_map(|entry| Some(entry.ok()?.path()))
            .collect(),
        Err(error) => {
            eprintln!("cannot read {}: {error}", directory.display());
            return ExitCode::FAILURE;
        }
    };
    paths.sort();
    let mut frozen = Vec::new();
    for path in paths {
        let Ok(body) = fs::read(&path) else { continue };
        let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&body) else {
            eprintln!("skipping an unreadable payload: {}", path.display());
            continue;
        };
        let height = payload["block_height"].as_u64().unwrap_or_default();
        if height < first {
            continue;
        }
        frozen.push(nano_conformance::receipt_digest(&payload));
        if frozen.len() >= count {
            break;
        }
    }
    if frozen.is_empty() {
        eprintln!(
            "no payloads at or above height {first} under {}",
            directory.display()
        );
        return ExitCode::FAILURE;
    }
    let document = json!({
        "source": "nano-stacks event observer, blocks whose state root the chain verified",
        // What produced these receipts. A frozen slice is nano checking itself, so
        // it is only evidence about a compiler if it says which one — and a slice
        // whose compiler is unknown cannot satisfy the release gate at all
        // (`mainnet_receipts::the_frozen_mainnet_slice_names_its_compiler`).
        "compiler": nano_vm::COMPILER_IDENTITY,
        "first_height": frozen.first().map(|entry| entry.height),
        "last_height": frozen.last().map(|entry| entry.height),
        "blocks": frozen,
    });
    match fs::write(
        output,
        serde_json::to_vec_pretty(&document).unwrap_or_default(),
    ) {
        Ok(()) => {
            println!(
                "froze {} blocks, {} to {}, into {output}",
                frozen.len(),
                frozen.first().map_or(0, |entry| entry.height),
                frozen.last().map_or(0, |entry| entry.height),
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("cannot write {output}: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, fs, path::Path};

    use super::{
        ARCHIVED_NAKAMOTO_BLOCK_QUERY, CheckpointHistoryExport, ContractArity, ContractInventory,
        ContractLocalsPeak, ContractRefusal, MINER_REWARD_MATURITY, arity_dimensions,
        chainstate_directory, contract_metadata_candidates, crosses_wasm_arity_boundary,
        encode_hex, refusal_reason, refuse_a_short_earnings_window, report_contract_arities,
        write_checkpoint_authentication_history,
    };
    use nano_chainstate::NakamotoBlock;
    use serde_json::json;

    struct FixtureBlock {
        raw: Vec<u8>,
        decoded: NakamotoBlock,
    }

    #[test]
    fn chainstate_directory_prefers_a_complete_nested_pair_over_distracting_root_databases() {
        let root = tempfile::tempdir().expect("temporary state");
        let nested = root.path().join("chainstate");
        fs::create_dir(&nested).expect("nested chainstate");
        for directory in [root.path(), nested.as_path()] {
            fs::write(directory.join("marf.sqlite"), []).expect("MARF database marker");
            fs::write(directory.join("clarity.sqlite"), []).expect("Clarity database marker");
        }

        assert_eq!(chainstate_directory(root.path()), nested);
        assert_eq!(chainstate_directory(&nested), nested);

        let direct = root.path().join("direct");
        fs::create_dir(&direct).expect("direct chainstate");
        fs::write(direct.join("marf.sqlite"), []).expect("direct MARF database marker");
        fs::write(direct.join("clarity.sqlite"), []).expect("direct Clarity database marker");
        assert_eq!(chainstate_directory(&direct), direct);
    }

    #[test]
    fn contract_metadata_candidates_are_distinct_and_count_every_source_row() {
        let root = tempfile::tempdir().expect("temporary state");
        let database = root.path().join("clarity.sqlite");
        let connection = rusqlite::Connection::open(&database).expect("create metadata store");
        connection
            .execute_batch(
                "CREATE TABLE metadata_table (
                    key TEXT NOT NULL,
                    blockhash TEXT,
                    value TEXT NOT NULL,
                    UNIQUE(key, blockhash)
                );",
            )
            .expect("metadata schema");
        let first = "clr-meta::SP000000000000000000002Q6VF78.first::analysis";
        let second = "clr-meta::SP000000000000000000002Q6VF78.second::analysis";
        for (key, block) in [(first, "01"), (first, "02"), (second, "03")] {
            connection
                .execute(
                    "INSERT INTO metadata_table (key, blockhash, value) VALUES (?1, ?2, '{}')",
                    rusqlite::params![key, block],
                )
                .expect("metadata row");
        }
        connection
            .execute(
                "INSERT INTO metadata_table (key, blockhash, value) VALUES (?1, '04', '{}')",
                rusqlite::params!["clr-meta::SP000000000000000000002Q6VF78.first::contract-src"],
            )
            .expect("unrelated metadata row");
        drop(connection);

        let (rows, contracts) =
            contract_metadata_candidates(&database).expect("read contract candidates");
        assert_eq!(rows, 3);
        assert_eq!(
            contracts,
            vec![
                "SP000000000000000000002Q6VF78.first".to_owned(),
                "SP000000000000000000002Q6VF78.second".to_owned(),
            ]
        );
    }

    fn fixture_blocks() -> Vec<FixtureBlock> {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../crates/nano-conformance/fixtures/nakamoto/blocks");
        let mut paths = fs::read_dir(directory)
            .expect("fixture blocks")
            .map(|entry| entry.expect("fixture entry").path())
            .collect::<Vec<_>>();
        paths.sort();
        paths
            .into_iter()
            .map(|path| {
                let raw = fs::read(path).expect("read fixture block");
                let decoded = NakamotoBlock::decode(&raw).expect("decode fixture block");
                FixtureBlock { raw, decoded }
            })
            .collect()
    }

    fn archive(path: &Path, blocks: &[FixtureBlock]) {
        let connection = rusqlite::Connection::open(path).expect("create archive");
        connection
            .execute_batch(
                "CREATE TABLE nakamoto_staging_blocks (
                    height INTEGER NOT NULL,
                    block_hash TEXT NOT NULL,
                    consensus_hash TEXT NOT NULL,
                    index_block_hash TEXT PRIMARY KEY NOT NULL,
                    parent_block_id TEXT NOT NULL,
                    is_tenure_start INTEGER NOT NULL,
                    data BLOB NOT NULL,
                    processed INTEGER NOT NULL,
                    orphaned INTEGER NOT NULL
                );",
            )
            .expect("archive schema");
        let mut insert = connection
            .prepare(
                "INSERT INTO nakamoto_staging_blocks
                 (height, block_hash, consensus_hash, index_block_hash, parent_block_id,
                  is_tenure_start, data, processed, orphaned)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, 0)",
            )
            .expect("archive insertion");
        for fixture in blocks {
            let block = &fixture.decoded;
            insert
                .execute(rusqlite::params![
                    block.header.chain_length,
                    encode_hex(block.header.block_hash().as_bytes()),
                    encode_hex(block.header.consensus_hash.as_bytes()),
                    encode_hex(block.block_id().as_bytes()),
                    encode_hex(block.header.parent_block_id.as_bytes()),
                    i64::from(nano_chainstate::starts_new_tenure(block)),
                    &fixture.raw,
                ])
                .expect("insert fixture block");
        }
    }

    #[test]
    fn checkpoint_history_lookup_uses_the_archive_block_id_index() {
        let blocks = fixture_blocks();
        let root = tempfile::tempdir().expect("temporary archive");
        let database = root.path().join("nakamoto.sqlite");
        archive(&database, &blocks[..1]);
        let connection = rusqlite::Connection::open(&database).expect("open archive");
        let plan = connection
            .query_row(
                &format!("EXPLAIN QUERY PLAN {ARCHIVED_NAKAMOTO_BLOCK_QUERY}"),
                rusqlite::params![encode_hex(blocks[0].decoded.block_id().as_bytes())],
                |row| row.get::<_, String>(3),
            )
            .expect("query plan");

        assert!(plan.contains("SEARCH"), "archive lookup scans: {plan}");
        assert!(
            plan.contains("INDEX"),
            "archive lookup ignores its index: {plan}"
        );
    }

    #[test]
    fn checkpoint_history_export_is_a_bounded_tenure_suffix() {
        let blocks = fixture_blocks();
        let starts = blocks
            .iter()
            .enumerate()
            .filter(|(_, fixture)| nano_chainstate::starts_new_tenure(&fixture.decoded))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert!(starts.len() >= 2, "fixture has two tenure starts");
        let boundary_index = starts[0];
        let first_index = starts[1];
        let source_index = starts.get(2).map_or(blocks.len() - 1, |index| *index - 1);
        let root = tempfile::tempdir().expect("temporary export");
        let database = root.path().join("nakamoto.sqlite");
        archive(&database, &blocks[boundary_index..=source_index]);
        let source = &blocks[source_index].decoded;
        let output = root.path().join("authentication-history");

        let written = write_checkpoint_authentication_history(
            &database,
            *source.block_id().as_bytes(),
            *source.header.state_index_root.as_bytes(),
            &output,
        )
        .expect("export authentication history");

        assert_eq!(written, source_index - first_index + 1);
        assert_eq!(
            fs::read_dir(output.join("blocks"))
                .expect("exported blocks")
                .count(),
            written
        );
        let boundary: serde_json::Value = serde_json::from_slice(
            &fs::read(output.join("boundary.json")).expect("boundary proof"),
        )
        .expect("boundary JSON");
        let expected_boundary = &blocks[boundary_index].decoded;
        assert_eq!(
            boundary["parent_tenure_consensus_hash"],
            encode_hex(expected_boundary.header.consensus_hash.as_bytes())
        );
        assert_eq!(
            boundary["coinbase_vrf_proof"],
            encode_hex(
                &nano_chainstate::coinbase_vrf_proof(expected_boundary)
                    .expect("boundary coinbase proof")
            )
        );
    }

    #[test]
    fn checkpoint_history_export_refuses_a_wrong_root() {
        let blocks = fixture_blocks();
        let starts = blocks
            .iter()
            .enumerate()
            .filter(|(_, fixture)| nano_chainstate::starts_new_tenure(&fixture.decoded))
            .map(|(index, _)| index)
            .take(2)
            .collect::<Vec<_>>();
        assert_eq!(starts.len(), 2, "fixture has two tenure starts");
        let root = tempfile::tempdir().expect("temporary export");
        let database = root.path().join("nakamoto.sqlite");
        archive(&database, &blocks[starts[0]..=starts[1]]);
        let source = &blocks[starts[1]].decoded;
        let output = root.path().join("wrong-root");

        let error = write_checkpoint_authentication_history(
            &database,
            *source.block_id().as_bytes(),
            [0xff; 32],
            &output,
        )
        .expect_err("wrong checkpoint root must be refused");

        assert!(error.contains("publishes state root"), "{error}");
        assert!(!output.exists(), "a refused export wrote output");
    }

    #[test]
    fn checkpoint_history_export_refuses_a_source_metadata_mismatch() {
        let blocks = fixture_blocks();
        let starts = blocks
            .iter()
            .enumerate()
            .filter(|(_, fixture)| nano_chainstate::starts_new_tenure(&fixture.decoded))
            .map(|(index, _)| index)
            .take(2)
            .collect::<Vec<_>>();
        assert_eq!(starts.len(), 2, "fixture has two tenure starts");
        let root = tempfile::tempdir().expect("temporary export");
        let database = root.path().join("nakamoto.sqlite");
        archive(&database, &blocks[starts[0]..=starts[1]]);
        let source = &blocks[starts[1]].decoded;
        let forged_source = [0xaa; 32];
        let connection = rusqlite::Connection::open(&database).expect("open archive");
        connection
            .execute(
                "UPDATE nakamoto_staging_blocks SET index_block_hash = ?1 \
                 WHERE index_block_hash = ?2",
                rusqlite::params![
                    encode_hex(&forged_source),
                    encode_hex(source.block_id().as_bytes()),
                ],
            )
            .expect("forge archive metadata");
        drop(connection);
        let output = root.path().join("wrong-source");

        let error = write_checkpoint_authentication_history(
            &database,
            forged_source,
            *source.header.state_index_root.as_bytes(),
            &output,
        )
        .expect_err("wrong checkpoint source must be refused");

        assert!(error.contains("metadata does not match"), "{error}");
        assert!(!output.exists(), "a refused export wrote output");
    }

    #[test]
    fn checkpoint_history_arguments_require_fixed_width_hashes() {
        let state_root = "00".repeat(32);
        let arguments = [
            "--blocks-db",
            "archive.sqlite",
            "--source-id",
            "abcd",
            "--state-root",
            &state_root,
            "--out-dir",
            "history",
        ]
        .map(str::to_owned);
        let error = CheckpointHistoryExport::parse(&arguments).expect_err("short source ID");
        assert!(error.contains("not 32 hex bytes"), "{error}");
    }

    fn arity(values: [usize; 5]) -> nano_vm::ArityReport {
        nano_vm::ArityReport {
            max_function_params: values[0],
            max_function_results: values[1],
            max_control_params: values[2],
            max_control_results: values[3],
            top_level_results: values[4],
        }
    }

    #[test]
    fn arity_inventory_records_each_numeric_maximum_and_exact_boundary_crossing() {
        let exact = arity([1_000, 9, 8, 7, 6]);
        let wide = arity([3, 1_001, 5, 1_002, 1_003]);
        assert!(!crosses_wasm_arity_boundary(&exact));
        assert!(crosses_wasm_arity_boundary(&wide));

        let mut inventory = ContractInventory {
            metadata_rows: 2,
            named: 2,
            current: 2,
            checked: 2,
            loaded: 2,
            counterfactual_epoch40_checked: 2,
            counterfactual_epoch40_loaded: 2,
            ..ContractInventory::default()
        };
        inventory.note_arity("SP000000000000000000002Q6VF78.exact", exact);
        inventory.note_arity("SP000000000000000000002Q6VF78.wide", wide.clone());

        assert_eq!(inventory.maximum, arity([1_000, 1_001, 8, 1_002, 1_003]));
        assert_eq!(
            inventory.over_boundary,
            vec![ContractArity {
                contract: "SP000000000000000000002Q6VF78.wide".to_owned(),
                report: wide,
            }]
        );
        assert!(inventory.passes());
    }

    #[test]
    fn stale_metadata_is_excluded_but_a_real_refusal_still_fails_with_its_full_reason() {
        struct RefusalMessage<'a>(&'a str);

        impl std::fmt::Debug for RefusalMessage<'_> {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.0)
            }
        }

        let report = arity([1_001, 2, 3, 4, 5]);
        let mut inventory = ContractInventory {
            metadata_rows: 3,
            named: 2,
            current: 1,
            checked: 1,
            loaded: 1,
            counterfactual_epoch40_checked: 1,
            counterfactual_epoch40_loaded: 1,
            stale_metadata: vec!["SP000000000000000000002Q6VF78.stale".to_owned()],
            ..ContractInventory::default()
        };
        assert!(inventory.passes());

        inventory.counterfactual_epoch40_loaded = 0;
        inventory.counterfactual_epoch40_refused.insert(
            "at-block is unavailable in Epoch40".to_owned(),
            vec!["SP000000000000000000002Q6VF78.current".to_owned()],
        );
        assert!(
            inventory.passes(),
            "a fully measured counterfactual refusal is evidence, not a current-state failure"
        );
        inventory.counterfactual_epoch40_refused.clear();
        assert!(
            !inventory.passes(),
            "missing counterfactual accounting fails"
        );
        inventory.counterfactual_epoch40_loaded = 1;

        inventory.loaded = 0;
        inventory.note_arity("SP000000000000000000002Q6VF78.refused", report.clone());
        let full_detail = format!(
            "contract analysis failed: validator refusal {} END",
            "x".repeat(120)
        );
        let reason = refusal_reason(&RefusalMessage(&full_detail));
        assert!(reason.ends_with(" END"), "refusal was truncated: {reason}");
        inventory.refused.insert(
            reason,
            vec![ContractRefusal {
                contract: "SP000000000000000000002Q6VF78.refused".to_owned(),
                arity: Some(report.clone()),
            }],
        );
        assert!(!inventory.passes());
        assert_eq!(
            arity_dimensions(&report),
            "function params/results 1001/2, control params/results 3/4, top-level results 5"
        );

        let mut unmeasured = ContractInventory {
            metadata_rows: 1,
            named: 1,
            ..ContractInventory::default()
        };
        unmeasured.unmeasured.insert(
            "source unavailable".to_owned(),
            vec!["SP.invalid".to_owned()],
        );
        assert!(!unmeasured.passes());
    }

    #[test]
    fn locals_inventory_records_exact_and_live_peaks_and_sorts_tied_sites() {
        assert_eq!(nano_vm::MAX_WASM_FUNCTION_LOCALS, 50_000);
        let mut inventory = ContractInventory::default();
        assert!(inventory.note_locals(
            "SP000000000000000000002Q6VF78.z-contract",
            &nano_vm::LocalsReport {
                max_live_locals: HashMap::from([
                    ("omega".to_owned(), 12),
                    ("alpha".to_owned(), 15),
                    ("gamma".to_owned(), 15),
                ]),
                emitted: HashMap::from([
                    (
                        "omega".to_owned(),
                        nano_vm::EmittedLocals {
                            parameters: 2,
                            declared: 16,
                            total: 18,
                        },
                    ),
                    (
                        "alpha".to_owned(),
                        nano_vm::EmittedLocals {
                            parameters: 1,
                            declared: 19,
                            total: 20,
                        },
                    ),
                ]),
            },
        ));
        assert!(inventory.note_locals(
            "SP000000000000000000002Q6VF78.a-contract",
            &nano_vm::LocalsReport {
                max_live_locals: HashMap::from([("beta".to_owned(), 15)]),
                emitted: HashMap::from([(
                    "beta".to_owned(),
                    nano_vm::EmittedLocals {
                        parameters: 4,
                        declared: 16,
                        total: 20,
                    },
                )]),
            },
        ));
        assert!(!inventory.note_locals("SP.empty", &nano_vm::LocalsReport::default()));
        inventory.sort_measurements();

        assert_eq!(inventory.maximum_live_locals, 15);
        assert_eq!(inventory.maximum_emitted_locals, 20);
        assert_eq!(
            inventory.maximum_emitted_local_sites,
            vec![
                ContractLocalsPeak {
                    contract: "SP000000000000000000002Q6VF78.a-contract".to_owned(),
                    function: "beta".to_owned(),
                    locals: 20,
                },
                ContractLocalsPeak {
                    contract: "SP000000000000000000002Q6VF78.z-contract".to_owned(),
                    function: "alpha".to_owned(),
                    locals: 20,
                },
            ]
        );
        assert_eq!(
            inventory.maximum_live_local_sites,
            vec![
                ContractLocalsPeak {
                    contract: "SP000000000000000000002Q6VF78.a-contract".to_owned(),
                    function: "beta".to_owned(),
                    locals: 15,
                },
                ContractLocalsPeak {
                    contract: "SP000000000000000000002Q6VF78.z-contract".to_owned(),
                    function: "alpha".to_owned(),
                    locals: 15,
                },
                ContractLocalsPeak {
                    contract: "SP000000000000000000002Q6VF78.z-contract".to_owned(),
                    function: "gamma".to_owned(),
                    locals: 15,
                },
            ]
        );

        inventory.current = 1;
        inventory.named = 1;
        inventory.checked = 1;
        inventory.loaded = 1;
        inventory.counterfactual_epoch40_checked = 1;
        inventory.counterfactual_epoch40_loaded = 1;
        inventory.maximum_emitted_locals = nano_vm::MAX_WASM_FUNCTION_LOCALS + 1;
        assert!(!inventory.passes());
    }

    #[test]
    fn qualifying_release_requires_a_full_state_inventory() {
        assert!(!report_contract_arities(None, true));
        assert!(report_contract_arities(None, false));
    }

    /// The shape `write_native_effects` builds: one entry per tenure it could
    /// price, in ascending order, with the heights it could not simply absent.
    fn window(heights: impl IntoIterator<Item = u64>) -> Vec<serde_json::Value> {
        heights
            .into_iter()
            .map(|coinbase_height| {
                json!({
                    "coinbase_height": coinbase_height,
                    "recipient": "SP000000000000000000002Q6VF78",
                    "coinbase": 0,
                    "fees": 0,
                })
            })
            .collect()
    }

    /// A window a node can pay a full maturity window of tenures from.
    ///
    /// The export stops one tenure short of the deepest one its blocks belong
    /// to, because a tenure's entry needs the row of its successor and the
    /// deepest one has none — so this is the accepted case, not a tolerated one.
    #[test]
    fn a_full_window_is_written() {
        let last = 8_665_600;
        let tenures = window(last - MINER_REWARD_MATURITY - 1..last);
        assert_eq!(refuse_a_short_earnings_window(&tenures, last), Ok(()));
    }

    /// An archive that answers for nothing is refused before a byte is written.
    #[test]
    fn an_empty_window_is_refused() {
        let error = refuse_a_short_earnings_window(&window([]), 8_665_600)
            .expect_err("an empty window was written");
        assert!(
            error.contains("no tenure earnings at all"),
            "the refusal says what it saw: {error}"
        );
    }

    /// The failure this guard was written for: 193 tenures spanning 201 heights.
    ///
    /// Outer bounds long enough and a hole in the middle, which a count cannot
    /// see. The message has to name the missing height, because the operator's
    /// next move is to ask the archive for that one tenure.
    #[test]
    fn a_holed_window_is_refused() {
        let last = 8_665_600;
        let missing = last - 27;
        let tenures = window((last - 200..last).filter(|height| *height != missing));
        let error =
            refuse_a_short_earnings_window(&tenures, last).expect_err("a holed window was written");
        assert!(
            error.contains(&missing.to_string()),
            "the refusal names the missing tenure: {error}"
        );
    }

    /// A window that stops short of the checkpoint's own tip.
    ///
    /// The tenures between owe nothing, and a node that reaches one of them
    /// stops there — a hundred tenures after it started, having sealed
    /// everything before.
    #[test]
    fn a_window_that_does_not_reach_the_checkpoint_is_refused() {
        let last = 8_665_600;
        let tenures = window(last - 200..last - 1);
        let error = refuse_a_short_earnings_window(&tenures, last)
            .expect_err("a window short of the tip was written");
        assert!(
            error.contains("would owe nothing"),
            "the refusal says the tenures between owe nothing: {error}"
        );
    }

    /// Two tenures where a hundred and one are needed, which is what the export
    /// used to write: contiguous, reaching the tip, and still unpayable.
    #[test]
    fn a_short_window_is_refused() {
        let last = 8_665_600;
        let tenures = window(last - 2..last);
        let error =
            refuse_a_short_earnings_window(&tenures, last).expect_err("a short window was written");
        assert!(
            error.contains(&format!("{} tenures", MINER_REWARD_MATURITY + 1)),
            "the refusal says how many a checkpoint needs: {error}"
        );
    }
}
