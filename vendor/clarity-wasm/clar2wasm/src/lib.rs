use clarity::types::StacksEpochId;
use clarity::vm::analysis::{run_analysis, AnalysisDatabase, ContractAnalysis};
use clarity::vm::ast::{build_ast_with_diagnostics, ContractAST};
use clarity::vm::costs::{ExecutionCost, LimitedCostTracker};
use clarity::vm::diagnostic::Diagnostic;
use clarity::vm::resource_limiter::ResourceLimiter;
use clarity::vm::types::QualifiedContractIdentifier;
use clarity::vm::ClarityVersion;
use std::collections::HashMap;
pub use walrus::Module;
use wasm_generator::{GeneratorError, WasmGenerator};

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
}

#[derive(Clone, Debug)]
pub struct CompiledContract {
    pub wasm: Vec<u8>,
    pub analysis: ContractAnalysis,
}

impl CompileResult {
    pub fn into_compiled_contract(mut self) -> CompiledContract {
        CompiledContract {
            wasm: self.module.emit_wasm(),
            analysis: self.contract_analysis,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ModuleCache {
    contracts: HashMap<QualifiedContractIdentifier, CompiledContract>,
}

impl ModuleCache {
    pub fn insert(&mut self, contract: QualifiedContractIdentifier, module: CompiledContract) {
        self.contracts.insert(contract, module);
    }

    pub fn get(&self, contract: &QualifiedContractIdentifier) -> Option<&CompiledContract> {
        self.contracts.get(contract)
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

    match generator.and_then(WasmGenerator::generate) {
        Ok(module) => Ok(CompileResult {
            ast,
            diagnostics,
            module,
            contract_analysis,
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
