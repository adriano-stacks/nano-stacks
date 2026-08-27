use clarity::types::StacksEpochId;
use clarity::vm::analysis::{run_analysis, AnalysisDatabase, ContractAnalysis};
use clarity::vm::ast::{build_ast_with_diagnostics, ContractAST};
use clarity::vm::costs::{ExecutionCost, LimitedCostTracker};
use clarity::vm::diagnostic::Diagnostic;
use clarity::vm::errors::VmExecutionError;
use clarity::vm::resource_limiter::ResourceLimiter;
use clarity::vm::types::QualifiedContractIdentifier;
use clarity::vm::ClarityVersion;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
pub use walrus::Module;
pub use wasm_generator::{ArityReport, MAX_WASM_TYPE_ARITY};
use wasm_generator::{GeneratorError, LocalsReport, WasmGenerator};

use crate::error::WasmError;

mod cost;

mod deserialize;
pub mod initialize;
pub mod linker;
mod serialize;
pub mod wasm_generator;
pub mod wasm_utils;
mod words;

pub mod datastore;
pub mod tools;

mod bitcoin;
mod copy;
mod debug_msg;
pub mod duck_type;
mod engine;
mod error;
mod error_mapping;
mod layout;
pub mod phases;
pub mod runtime_shape;

pub use engine::{consensus_engine, ENGINE_CONFIG_ID, WASMTIME_VERSION};

#[cfg(feature = "developer-mode")]
pub mod test_utils;

// FIXME: This is copied from stacks-blockchain
// Block limit in Stacks 2.1
pub const BLOCK_LIMIT_MAINNET_21: ExecutionCost = ExecutionCost {
    write_length: 15_000_000,
    write_count: 15_000,
    read_length: 100_000_000,
    read_count: 15_000,
    runtime: 5_000_000_000,
};

#[derive(Debug)]
pub struct CompileResult {
    pub ast: ContractAST,
    pub diagnostics: Vec<Diagnostic>,
    pub module: Module,
    pub contract_analysis: ContractAnalysis,
    /// Peak simultaneously-live wasm locals per generated function, measured
    /// during generation. Measurement only: nothing refuses compilation
    /// based on it.
    pub locals_report: LocalsReport,
    /// Maximum flattened function and control arities before packed lowering.
    pub arity_report: ArityReport,
}

pub struct CompiledContract {
    pub wasm: Vec<u8>,
    pub analysis: ContractAnalysis,
    /// The native code wasmtime makes of `wasm`, kept for the life of the
    /// process. Cranelift costs milliseconds to seconds per contract, and a
    /// running node calls a contract far more often than it deploys one, so
    /// compiling per call — which is what a fresh `Engine` per call forces —
    /// dominated everything else replay does.
    native: OnceLock<wasmtime::Module>,
    /// The native module with its imports already resolved against the host
    /// linker, built at most once. Import resolution walked 223 names per
    /// call frame; a pre-resolved instantiation only allocates.
    instance_pre: OnceLock<wasmtime::InstancePre<initialize::StaticClarityWasmContext>>,
}

impl Clone for CompiledContract {
    fn clone(&self) -> Self {
        Self {
            wasm: self.wasm.clone(),
            analysis: self.analysis.clone(),
            native: self.native.clone(),
            instance_pre: self.instance_pre.clone(),
        }
    }
}

impl std::fmt::Debug for CompiledContract {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CompiledContract")
            .field("wasm", &self.wasm.len())
            .field("native", &self.native.get().is_some())
            .finish_non_exhaustive()
    }
}

impl CompiledContract {
    pub fn new(wasm: Vec<u8>, analysis: ContractAnalysis) -> Self {
        Self {
            wasm,
            analysis,
            native: OnceLock::new(),
            instance_pre: OnceLock::new(),
        }
    }

    /// The native module for this contract, built at most once.
    pub fn native(&self, cache: &ModuleCache) -> Result<&wasmtime::Module, VmExecutionError> {
        if let Some(native) = self.native.get() {
            count(|counters| counters.native_hits += 1);
            return Ok(native);
        }
        let native = cache.native_module(&self.wasm)?;
        let _ = self.native.set(native);
        self.native
            .get()
            .ok_or_else(|| crate::error::wasm_error(WasmError::ModuleNotFound))
    }

    /// This contract's pre-resolved instantiation, built at most once.
    ///
    pub fn instance_pre(
        &self,
        cache: &ModuleCache,
    ) -> Result<wasmtime::InstancePre<initialize::StaticClarityWasmContext>, VmExecutionError> {
        if self.instance_pre.get().is_none() {
            count(|counters| counters.instance_pre_misses += 1);
            let native = self.native(cache)?.clone();
            let linker = cache.host_linker()?;
            let pre = linker
                .instantiate_pre(&native)
                .map_err(|error| crate::error::wasm_error(WasmError::UnableToLoadModule(error)))?;
            let _ = self.instance_pre.set(pre);
        } else {
            count(|counters| counters.instance_pre_hits += 1);
        }
        let pre = self
            .instance_pre
            .get()
            .ok_or_else(|| crate::error::wasm_error(WasmError::ModuleNotFound))?
            .clone();
        Ok(pre)
    }
}

impl CompileResult {
    pub fn into_compiled_contract(mut self) -> CompiledContract {
        CompiledContract::new(self.module.emit_wasm(), self.contract_analysis)
    }
}

/// Somewhere native code outlives the process that compiled it.
///
/// The node implements this over its state directory. A store never reports an
/// error: a miss, a truncated entry, an entry another wasmtime wrote and an
/// unreadable directory are all just "no module", and the caller compiles.
pub trait NativeModuleStore: std::fmt::Debug + Send + Sync {
    /// The native module previously stored for exactly these wasm bytes.
    fn load(&self, engine: &wasmtime::Engine, wasm: &[u8]) -> Option<wasmtime::Module>;

    /// Keep `module`, which was compiled from `wasm`, for a later process.
    fn store(&self, wasm: &[u8], module: &wasmtime::Module);
}

/// How many estimated bytes of compiled contracts stay resident.
///
/// Without a bound this held every contract the chain ever called, several
/// megabytes each, for the life of the process — a mainnet follower leaked its
/// way to an OOM kill through it. Eviction costs a recompilation on the next
/// call, never a wrong answer, so the budget only has to keep the hot set.
const MODULE_CACHE_BYTES: usize = 256 * 1024 * 1024;

/// Where obtaining native code goes, when asked.
///
/// `instance_pre` memoizes per contract, so its hit rate says whether the memo
/// survives the frames of a call, and splitting a miss into a deserialize and a
/// compile says which half of the cache answered. Off unless
/// `NANO_COUNT_NATIVE` is set, and reported every two thousand asks.
#[derive(Debug, Default)]
struct NativeCounters {
    instance_pre_hits: u64,
    instance_pre_misses: u64,
    native_hits: u64,
    native_from_disk: u64,
    native_compiled: u64,
    evictions: u64,
    inserts: u64,
}

static NATIVE_COUNTERS: std::sync::Mutex<Option<NativeCounters>> = std::sync::Mutex::new(None);

fn counting() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("NANO_COUNT_NATIVE").is_some())
}

fn count(field: impl Fn(&mut NativeCounters)) {
    if !counting() {
        return;
    }
    if let Ok(mut guard) = NATIVE_COUNTERS.lock() {
        let counters = guard.get_or_insert_with(NativeCounters::default);
        field(counters);
        let asked = counters.instance_pre_hits + counters.instance_pre_misses;
        if asked > 0 && asked % 2_000 == 0 {
            eprintln!("native-counters {counters:?}");
        }
    }
}

/// What keeping a compiled contract costs, estimated.
///
/// The wasm bytes are measurable. The analysis — the whole AST plus a boxed
/// type per node — and the native module are not, and both scale with the
/// wasm, so they are charged as a multiple of it. This is a relative weight,
/// not an allocator measurement.
fn entry_weight(module: &CompiledContract) -> usize {
    module.wasm.len() * 6 + 65_536
}

/// The compiled contracts one execution context has to hand.
///
/// Holds the `Engine` as well, because a `Module` may only be instantiated in a
/// store of the engine that made it: sharing the modules means sharing the
/// engine.
#[derive(Clone)]
pub struct ModuleCache {
    engine: wasmtime::Engine,
    persistent: Option<Arc<dyn NativeModuleStore>>,
    contracts: HashMap<QualifiedContractIdentifier, (Arc<CompiledContract>, Cell<u64>)>,
    /// A logical clock stamping each hit, so eviction drops what was touched
    /// least recently. A `Cell` because a lookup is a read everywhere else.
    clock: Cell<u64>,
    /// The estimated bytes of everything held, kept in step with `contracts`.
    bytes: usize,
    /// The 223 host functions, registered once per cache instead of once per
    /// call. See [`ModuleCache::host_linker`] for the lifetime story.
    linker_template: std::cell::OnceCell<wasmtime::Linker<initialize::StaticClarityWasmContext>>,
}

impl Default for ModuleCache {
    fn default() -> Self {
        let engine = consensus_engine().unwrap_or_else(|error| {
            panic!("the checked-in consensus Wasmtime configuration is invalid: {error}")
        });
        Self {
            engine,
            persistent: None,
            contracts: HashMap::new(),
            clock: Cell::new(0),
            bytes: 0,
            linker_template: std::cell::OnceCell::new(),
        }
    }
}

impl std::fmt::Debug for ModuleCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModuleCache")
            .field("contracts", &self.contracts.len())
            .field("persistent", &self.persistent)
            .finish()
    }
}

impl ModuleCache {
    /// Compiled contracts currently held in memory.
    #[must_use]
    pub fn resident_entries(&self) -> usize {
        self.contracts.len()
    }

    /// Estimated resident bytes charged to the cache's eviction budget.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        self.bytes
    }

    /// A cache that also reads and writes native code through `store`.
    pub fn persisting_in(store: Arc<dyn NativeModuleStore>) -> Self {
        Self {
            persistent: Some(store),
            ..Self::default()
        }
    }

    pub fn insert(&mut self, contract: QualifiedContractIdentifier, module: CompiledContract) {
        count(|counters| counters.inserts += 1);
        self.bytes += entry_weight(&module);
        let stamped = (Arc::new(module), Cell::new(self.tick()));
        if let Some((prior, _)) = self.contracts.insert(contract, stamped) {
            self.bytes -= entry_weight(&prior);
        }
        // The entry just inserted carries the newest stamp, so with more than
        // one entry it is never the minimum and survives its own insertion.
        while self.bytes > MODULE_CACHE_BYTES && self.contracts.len() > 1 {
            let Some(oldest) = self
                .contracts
                .iter()
                .min_by_key(|(_, (_, used))| used.get())
                .map(|(contract, _)| contract.clone())
            else {
                break;
            };
            if let Some((evicted, _)) = self.contracts.remove(&oldest) {
                self.bytes -= entry_weight(&evicted);
                count(|counters| counters.evictions += 1);
            }
        }
    }

    pub fn get(&self, contract: &QualifiedContractIdentifier) -> Option<&Arc<CompiledContract>> {
        let (module, used) = self.contracts.get(contract)?;
        used.set(self.tick());
        Some(module)
    }

    fn tick(&self) -> u64 {
        let now = self.clock.get() + 1;
        self.clock.set(now);
        now
    }

    /// The engine every module in this cache belongs to.
    pub fn engine(&self) -> &wasmtime::Engine {
        &self.engine
    }

    /// A linker carrying the host functions, built once and cloned per call.
    ///
    /// Re-registering 223 host functions on every contract call — and on
    /// every *nested* `contract-call?` — was measured at roughly seventy
    /// microseconds a call frame on a mainnet replay; a clone copies one
    /// name-indexed map of `Arc`s. The per-call cost globals are deliberately
    /// not in the template: they are store-owned, so the caller defines them
    /// on its clone exactly as before.
    ///
    pub fn host_linker(
        &self,
    ) -> Result<wasmtime::Linker<initialize::StaticClarityWasmContext>, VmExecutionError> {
        if self.linker_template.get().is_none() {
            let mut linker = wasmtime::Linker::new(&self.engine);
            crate::linker::link_host_functions(&mut linker)?;
            let _ = self.linker_template.set(linker);
        }
        let template = self
            .linker_template
            .get()
            .ok_or_else(|| {
                crate::error::wasm_error(WasmError::WasmGeneratorError(
                    "the host linker template cannot be built".to_owned(),
                ))
            })?
            .clone();
        Ok(template)
    }

    /// Native code for `wasm`, from the persistent store if it has it.
    pub fn native_module(&self, wasm: &[u8]) -> Result<wasmtime::Module, VmExecutionError> {
        if let Some(stored) = self
            .persistent
            .as_ref()
            .and_then(|store| store.load(&self.engine, wasm))
        {
            count(|counters| counters.native_from_disk += 1);
            return Ok(stored);
        }
        count(|counters| counters.native_compiled += 1);
        let module = wasmtime::Module::from_binary(&self.engine, wasm)
            .map_err(|error| crate::error::wasm_error(WasmError::UnableToLoadModule(error)))?;
        if let Some(store) = self.persistent.as_ref() {
            store.store(wasm, &module);
        }
        Ok(module)
    }
}

#[derive(Debug)]
pub enum CompileError {
    Generic {
        ast: Box<ContractAST>,
        diagnostics: Vec<Diagnostic>,
        cost_tracker: Box<LimitedCostTracker>,
    },
}

pub fn compile(
    source: &str,
    contract_id: &QualifiedContractIdentifier,
    cost_tracker: LimitedCostTracker,
    clarity_version: ClarityVersion,
    epoch: StacksEpochId,
    analysis_db: &mut AnalysisDatabase,
    emit_cost_code: bool,
) -> Result<CompileResult, CompileError> {
    compile_for_cost_epoch(
        source,
        contract_id,
        cost_tracker,
        clarity_version,
        epoch,
        epoch,
        analysis_db,
        emit_cost_code,
    )
}

/// Compile with the semantics of one epoch and the cost table of another.
///
/// A contract keeps the semantics of the epoch it was written for — that is
/// what its stored analysis means, and it is why a contract using a word a
/// later epoch removed still runs. But the chain charges it at the rate of the
/// epoch it is running in, so recompiling under the older epoch alone prices
/// every call into it wrongly.
#[allow(clippy::too_many_arguments)]
pub fn compile_for_cost_epoch(
    source: &str,
    contract_id: &QualifiedContractIdentifier,
    mut cost_tracker: LimitedCostTracker,
    clarity_version: ClarityVersion,
    epoch: StacksEpochId,
    cost_epoch: StacksEpochId,
    analysis_db: &mut AnalysisDatabase,
    emit_cost_code: bool,
) -> Result<CompileResult, CompileError> {
    // Parse the contract
    let (ast, mut diagnostics, success) = build_ast_with_diagnostics(
        contract_id,
        source,
        &mut cost_tracker,
        clarity_version,
        epoch,
    );

    if !success {
        return Err(CompileError::Generic {
            ast: Box::new(ast),
            diagnostics,
            cost_tracker: Box::new(cost_tracker),
        });
    }

    // Run the analysis passes
    let mut contract_analysis = match run_analysis(
        contract_id,
        &ast.expressions,
        analysis_db,
        false,
        cost_tracker,
        epoch,
        clarity_version,
        true,
        ResourceLimiter::unlimited(),
    ) {
        Ok(contract_analysis) => contract_analysis,
        Err(boxed) => {
            let (e, cost_track) = *boxed;
            diagnostics.push(Diagnostic::err(e.err.as_ref()));
            return Err(CompileError::Generic {
                ast: Box::new(ast),
                diagnostics,
                cost_tracker: Box::new(cost_track),
            });
        }
    };

    #[allow(clippy::expect_used)]
    let generator = match emit_cost_code {
        false => WasmGenerator::new(contract_analysis.clone()),
        true => WasmGenerator::with_cost_code_for_epoch(contract_analysis.clone(), cost_epoch),
    };

    let generator = match generator {
        Ok(generator) => generator,
        Err(e) => {
            diagnostics.push(Diagnostic::err(&e));
            return Err(CompileError::Generic {
                ast: Box::new(ast),
                diagnostics,
                #[allow(clippy::expect_used)]
                cost_tracker: Box::new(
                    contract_analysis
                        .cost_track
                        .take()
                        .expect("Failed to take cost tracker from contract analysis"),
                ),
            });
        }
    };

    // The generator is consumed by `generate`, so keep a handle on the
    // report it fills in as it works.
    let locals_report = generator.locals_report.clone();
    let arity_report = generator.arity_report.clone();

    match generator.generate() {
        Ok(module) => Ok(CompileResult {
            ast,
            diagnostics,
            module,
            contract_analysis,
            locals_report: locals_report.borrow().clone(),
            arity_report: arity_report.borrow().clone(),
        }),
        Err(e) => {
            diagnostics.push(Diagnostic::err(&e));
            Err(CompileError::Generic {
                ast: Box::new(ast),
                diagnostics,
                #[allow(clippy::expect_used)]
                cost_tracker: Box::new(
                    contract_analysis
                        .cost_track
                        .take()
                        .expect("Failed to take cost tracker from contract analysis"),
                ),
            })
        }
    }
}

pub fn compile_contract(contract_analysis: ContractAnalysis) -> Result<Module, GeneratorError> {
    let generator = WasmGenerator::new(contract_analysis)?;
    generator.generate()
}

/// Compile an analyzed contract while charging execution at a later cost epoch.
pub fn compile_contract_with_cost_epoch(
    contract_analysis: ContractAnalysis,
    cost_epoch: StacksEpochId,
) -> Result<Module, GeneratorError> {
    let generator = WasmGenerator::with_cost_code_for_epoch(contract_analysis, cost_epoch)?;
    generator.generate()
}
