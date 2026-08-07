use clarity::types::StacksEpochId;
use clarity::vm::analysis::{run_analysis, AnalysisDatabase, ContractAnalysis};
use clarity::vm::ast::{build_ast_with_diagnostics, ContractAST};
use clarity::vm::costs::{ExecutionCost, LimitedCostTracker};
use clarity::vm::diagnostic::Diagnostic;
use clarity::vm::errors::VmExecutionError;
use clarity::vm::resource_limiter::ResourceLimiter;
use clarity::vm::types::QualifiedContractIdentifier;
use clarity::vm::ClarityVersion;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
pub use walrus::Module;
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
mod error;
mod error_mapping;
mod layout;

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
}

#[derive(Debug)]
pub struct CompiledContract {
    pub wasm: Vec<u8>,
    pub analysis: ContractAnalysis,
    /// The native code wasmtime makes of `wasm`, kept for the life of the
    /// process. Cranelift costs milliseconds to seconds per contract, and a
    /// running node calls a contract far more often than it deploys one, so
    /// compiling per call — which is what a fresh `Engine` per call forces —
    /// dominated everything else replay does.
    native: OnceLock<wasmtime::Module>,
}

impl Clone for CompiledContract {
    fn clone(&self) -> Self {
        Self {
            wasm: self.wasm.clone(),
            analysis: self.analysis.clone(),
            native: self.native.clone(),
        }
    }
}

impl CompiledContract {
    pub fn new(wasm: Vec<u8>, analysis: ContractAnalysis) -> Self {
        Self {
            wasm,
            analysis,
            native: OnceLock::new(),
        }
    }

    /// The native module for this contract, built at most once.
    pub fn native(&self, cache: &ModuleCache) -> Result<&wasmtime::Module, VmExecutionError> {
        if let Some(native) = self.native.get() {
            return Ok(native);
        }
        let native = cache.native_module(&self.wasm)?;
        let _ = self.native.set(native);
        self.native
            .get()
            .ok_or_else(|| crate::error::wasm_error(WasmError::ModuleNotFound))
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

/// The compiled contracts one execution context has to hand.
///
/// Holds the `Engine` as well, because a `Module` may only be instantiated in a
/// store of the engine that made it: sharing the modules means sharing the
/// engine.
#[derive(Clone, Default)]
pub struct ModuleCache {
    engine: wasmtime::Engine,
    persistent: Option<Arc<dyn NativeModuleStore>>,
    contracts: HashMap<QualifiedContractIdentifier, Arc<CompiledContract>>,
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
    /// A cache that also reads and writes native code through `store`.
    pub fn persisting_in(store: Arc<dyn NativeModuleStore>) -> Self {
        Self {
            persistent: Some(store),
            ..Self::default()
        }
    }

    pub fn insert(&mut self, contract: QualifiedContractIdentifier, module: CompiledContract) {
        self.contracts.insert(contract, Arc::new(module));
    }

    pub fn get(&self, contract: &QualifiedContractIdentifier) -> Option<&Arc<CompiledContract>> {
        self.contracts.get(contract)
    }

    /// The engine every module in this cache belongs to.
    pub fn engine(&self) -> &wasmtime::Engine {
        &self.engine
    }

    /// Native code for `wasm`, from the persistent store if it has it.
    pub fn native_module(&self, wasm: &[u8]) -> Result<wasmtime::Module, VmExecutionError> {
        if let Some(stored) = self
            .persistent
            .as_ref()
            .and_then(|store| store.load(&self.engine, wasm))
        {
            return Ok(stored);
        }
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

    match generator.generate() {
        Ok(module) => Ok(CompileResult {
            ast,
            diagnostics,
            module,
            contract_analysis,
            locals_report: locals_report.borrow().clone(),
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
