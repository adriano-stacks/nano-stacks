//! The reference interpreter, as a differential oracle.
//!
//! clarity-wasm is the consensus engine and this crate is not part of it. It
//! exists so a disagreement between the two engines can be *found*: the same
//! call, the same state, two answers, and the one that differs from the chain
//! names a compiler bug to fix.
//!
//! This crate is deliberately outside `nano-vm`. The shipped `stacks-node`
//! links `nano-vm`, and `nano-vm` contains no interpreter call path at all — so
//! there is no environment variable, configuration field, feature or failure
//! mode that could make a production node execute a transaction this way. Only
//! conformance tests and `xtask` diagnostics depend on this crate.
//!
//! Everything here writes through whatever store it is given, so a caller that
//! means to leave no trace has to bracket it — `MarfStore::begin` and
//! `MarfStore::abort`, or a VM block that is aborted rather than sealed.

use std::collections::{HashMap, HashSet};

use clarity::{
    types::StacksEpochId,
    vm::{
        ClarityName, ClarityVersion, ContractContext, SymbolicExpression, Value,
        ast::build_ast,
        callables::{DefineType, DefinedFunction},
        contexts::{GlobalContext, OwnedEnvironment},
        costs::{ExecutionCost, LimitedCostTracker},
        database::{ClaritySerializable, MemoryBackingStore},
        errors::{ClarityEvalError, VmExecutionError, VmInternalError},
        eval_all,
        types::{PrincipalData, QualifiedContractIdentifier, TypeSignature, TypeSignatureExt},
    },
};
use nano_primitives::Network;
use nano_vm::{
    ChainContext, ContractCallOutcome, DeploymentAnalysis, MarfStore, TransactionResult,
    analyse_for_deployment, clarity_database, contract_source, epoch_for_version,
    is_acceptable_runtime_failure, null_context, referenced_contracts, save_contract_analysis,
};

/// The value and consensus-cost dimensions produced by one Clarity evaluation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Evaluation {
    pub value: Option<Value>,
    pub cost: ExecutionCost,
}

/// The functions a contract's source defines, with their real bodies.
///
/// Built straight from the parsed source, so nothing is deployed and no other
/// contract has to be present — which is the difference between this and
/// rebuilding by deploying into a throwaway store: a contract that names a
/// contract cannot be deployed beside nothing, and can still be *parsed*.
#[must_use]
pub fn defined_functions(
    contract: &QualifiedContractIdentifier,
    version: ClarityVersion,
    source: &str,
) -> HashMap<ClarityName, DefinedFunction> {
    use clarity::vm::representations::SymbolicExpressionType::{Atom, List};
    let epoch = epoch_for_version(version);
    let mut tracker = LimitedCostTracker::new_free();
    // `build_ast` rather than `ast::parse`: the latter is gated on
    // `clarity/testing`, and this crate must not need a feature that Cargo would
    // unify into every binary built beside it.
    let Ok(parsed) = build_ast(contract, source, &mut tracker.clone(), version, epoch) else {
        return HashMap::new();
    };
    let mut functions = HashMap::new();
    for expression in &parsed.expressions {
        let List(form) = &expression.expr else {
            continue;
        };
        let [head, signature, body] = form.as_slice() else {
            continue;
        };
        let Atom(keyword) = &head.expr else { continue };
        let define_type = match keyword.as_str() {
            "define-public" => DefineType::Public,
            "define-read-only" => DefineType::ReadOnly,
            "define-private" => DefineType::Private,
            _ => continue,
        };
        let List(signature) = &signature.expr else {
            continue;
        };
        let Some((name, arguments)) = signature.split_first() else {
            continue;
        };
        let Atom(name) = &name.expr else { continue };
        let mut typed = Vec::new();
        for argument in arguments {
            let List(pair) = &argument.expr else { continue };
            let [argument_name, argument_type] = pair.as_slice() else {
                continue;
            };
            let Atom(argument_name) = &argument_name.expr else {
                continue;
            };
            let Ok(signature) = TypeSignature::parse_type_repr(epoch, argument_type, &mut tracker)
            else {
                continue;
            };
            typed.push((argument_name.clone(), signature));
        }
        functions.insert(
            name.clone(),
            DefinedFunction::new(
                typed,
                body.clone(),
                define_type,
                name,
                &contract.to_string(),
            ),
        );
    }
    functions
}

/// Build a contract definition the interpreter can run, without touching state.
///
/// clar2wasm's deploy stores placeholder function bodies, so a contract the
/// compiler deployed cannot be interpreted. Rebuilding one means deploying it
/// again — which would re-run its top-level expressions and reset every data
/// variable it has changed since. So it is deployed into a **throwaway
/// in-memory store** instead: the definition that comes out is the real one, and
/// every side effect lands somewhere that is dropped a line later.
fn interpretable_contract(
    network: Network,
    contract: &QualifiedContractIdentifier,
    version: ClarityVersion,
    source: &str,
    dependencies: Vec<(
        QualifiedContractIdentifier,
        clarity::vm::contracts::Contract,
    )>,
) -> Result<clarity::vm::contracts::Contract, ClarityEvalError> {
    let mut backing_store = MemoryBackingStore::new();
    let mut database = backing_store.as_clarity_db();
    // A contract that names another cannot be deployed beside nothing, and the
    // throwaway store starts empty. Its dependencies are put in first, taken
    // from the state this node already holds.
    if !dependencies.is_empty() {
        database.begin();
        for (identifier, definition) in dependencies {
            database.insert_contract(&identifier, definition)?;
        }
        database.commit()?;
    }
    let mut environment = OwnedEnvironment::new_free(
        network.is_mainnet(),
        network.chain_id(),
        database,
        StacksEpochId::Epoch40,
    );
    environment.initialize_versioned_contract(contract.clone(), version, source, None)?;
    let (mut database, _) = environment.destruct().ok_or_else(|| {
        ClarityEvalError::from(VmExecutionError::Internal(VmInternalError::Expect(
            "rebuilding a contract definition left the throwaway store nested".to_owned(),
        )))
    })?;
    database.begin();
    let rebuilt = database.get_contract(contract)?;
    database.roll_back()?;
    Ok(rebuilt)
}

/// Evaluate a Clarity 6 program under the consensus Epoch 4.0 rules against an
/// ephemeral store, for programs that read and write nothing.
pub fn evaluate(network: Network, source: &str) -> Result<Option<Value>, ClarityEvalError> {
    let contract_id = QualifiedContractIdentifier::transient();
    let mut backing_store = MemoryBackingStore::new();
    let database = backing_store.as_clarity_db();
    let mut context = GlobalContext::new(
        network.is_mainnet(),
        network.chain_id(),
        database,
        LimitedCostTracker::new_free(),
        StacksEpochId::Epoch40,
    );
    let expressions = build_ast(
        &contract_id,
        source,
        &mut context.cost_track,
        ClarityVersion::Clarity6,
        StacksEpochId::Epoch40,
    )?
    .expressions;
    let mut contract = ContractContext::new(contract_id, ClarityVersion::Clarity6);

    context
        .execute(|global| eval_all(&expressions, &mut contract, global, None))
        .map_err(ClarityEvalError::from)
}

/// Evaluate a Clarity 6 program against an active MARF-backed state.
pub fn evaluate_in_store(
    store: &mut MarfStore,
    source: &str,
) -> Result<Option<Value>, ClarityEvalError> {
    Ok(evaluate_with_tracker(store, source, LimitedCostTracker::new_free())?.value)
}

/// Evaluate a Clarity 6 program with the supplied consensus cost tracker.
pub fn evaluate_with_tracker(
    store: &mut MarfStore,
    source: &str,
    cost_tracker: LimitedCostTracker,
) -> Result<Evaluation, ClarityEvalError> {
    evaluate_with_tracker_in_context(store, null_context(), source, cost_tracker)
}

fn evaluate_with_tracker_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    source: &str,
    cost_tracker: LimitedCostTracker,
) -> Result<Evaluation, ClarityEvalError> {
    let network = store.network();
    let contract_id = QualifiedContractIdentifier::transient();
    let database = clarity_database(store, bitcoin_context);
    let mut context = GlobalContext::new(
        network.is_mainnet(),
        network.chain_id(),
        database,
        cost_tracker,
        StacksEpochId::Epoch40,
    );
    let expressions = build_ast(
        &contract_id,
        source,
        &mut context.cost_track,
        ClarityVersion::Clarity6,
        StacksEpochId::Epoch40,
    )?
    .expressions;
    let mut contract = ContractContext::new(contract_id, ClarityVersion::Clarity6);

    let value = context
        .execute(|global| eval_all(&expressions, &mut contract, global, None))
        .map_err(ClarityEvalError::from)?;
    Ok(Evaluation {
        value,
        cost: context.cost_track.get_total(),
    })
}

pub fn deploy_contract(
    store: &mut MarfStore,
    contract: QualifiedContractIdentifier,
    version: ClarityVersion,
    source: &str,
    cost_tracker: LimitedCostTracker,
) -> Result<TransactionResult, ClarityEvalError> {
    deploy_contract_in_context(
        store,
        null_context(),
        contract,
        version,
        source,
        cost_tracker,
    )
}

fn deploy_contract_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    contract: QualifiedContractIdentifier,
    version: ClarityVersion,
    source: &str,
    cost_tracker: LimitedCostTracker,
) -> Result<TransactionResult, ClarityEvalError> {
    let network = store.network();
    let DeploymentAnalysis {
        ast,
        contract_analysis,
        cost_tracker,
    } = analyse_for_deployment(store, &contract, version, source, cost_tracker)
        .map_err(|failure| failure.error)?;
    let persisted_contract = contract.clone();
    let database = clarity_database(store, bitcoin_context);
    let mut environment = OwnedEnvironment::new_cost_limited(
        network.is_mainnet(),
        network.chain_id(),
        database,
        cost_tracker,
        StacksEpochId::Epoch40,
    );
    let ((), assets, events) = environment
        .initialize_contract_from_ast(contract, version, &ast, source, None)
        .map_err(ClarityEvalError::from)?;
    let cost = environment.get_cost_total();
    drop(environment);

    save_contract_analysis(store, &persisted_contract, &contract_analysis)?;

    Ok(TransactionResult {
        value: Some(Value::okay_true()),
        cost,
        assets,
        events,
    })
}

/// Call a Clarity contract using the encoded arguments found in a transaction payload.
pub fn execute_contract_call(
    store: &mut MarfStore,
    sender: PrincipalData,
    sponsor: Option<PrincipalData>,
    contract: QualifiedContractIdentifier,
    function: &str,
    arguments: &[Vec<u8>],
    cost_tracker: LimitedCostTracker,
) -> Result<TransactionResult, VmExecutionError> {
    match execute_contract_call_outcome(
        store,
        sender,
        sponsor,
        contract,
        function,
        arguments,
        cost_tracker,
    )? {
        ContractCallOutcome::Success(result) | ContractCallOutcome::AbortedByResponse(result) => {
            Ok(*result)
        }
        ContractCallOutcome::RuntimeFailure { error, .. } => Err(error),
    }
}

/// Call a contract while retaining acceptable runtime failures and their costs.
pub fn execute_contract_call_outcome(
    store: &mut MarfStore,
    sender: PrincipalData,
    sponsor: Option<PrincipalData>,
    contract: QualifiedContractIdentifier,
    function: &str,
    arguments: &[Vec<u8>],
    cost_tracker: LimitedCostTracker,
) -> Result<ContractCallOutcome, VmExecutionError> {
    execute_contract_call_outcome_in_context(
        store,
        null_context(),
        ContractCall {
            sender,
            sponsor,
            contract,
            function,
            arguments,
        },
        cost_tracker,
    )
}

#[derive(Debug)]
struct HealedContract {
    identifier: QualifiedContractIdentifier,
    definition: String,
}

/// Heal every statically reachable contract, including concrete contract
/// principals supplied as transaction arguments.
fn heal_reachable_contracts(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    contract: &QualifiedContractIdentifier,
) -> Vec<HealedContract> {
    let mut pending = vec![contract.clone()];
    let mut visited = HashSet::new();
    let mut healed = Vec::new();
    while let Some(reachable) = pending.pop() {
        if !visited.insert(reachable.clone()) {
            continue;
        }
        let Ok((source, version)) = contract_source(store, bitcoin_context, &reachable) else {
            continue;
        };
        pending.extend(referenced_contracts(&reachable, &source, version));
        if store.contract_is_interpretable(&reachable) {
            continue;
        }
        let Some(definition) = store.stored_contract(&reachable) else {
            continue;
        };
        if heal_contract_for_interpreter(store, bitcoin_context, &reachable).is_ok() {
            healed.push(HealedContract {
                identifier: reachable,
                definition,
            });
        }
    }
    healed
}

fn restore_contracts(
    store: &MarfStore,
    healed: Vec<HealedContract>,
) -> Result<(), VmExecutionError> {
    for contract in healed.into_iter().rev() {
        store
            .replace_contract_definition(&contract.identifier, &contract.definition)
            .map_err(|error| VmInternalError::Expect(error.to_string()))?;
    }
    Ok(())
}

fn collect_argument_contracts(value: &Value, contracts: &mut Vec<QualifiedContractIdentifier>) {
    use clarity::vm::types::SequenceData;

    match value {
        Value::Principal(PrincipalData::Contract(contract)) => contracts.push(contract.clone()),
        Value::CallableContract(callable) => {
            contracts.push(callable.contract_identifier.clone());
        }
        Value::Tuple(tuple) => {
            for value in tuple.data_map.values() {
                collect_argument_contracts(value, contracts);
            }
        }
        Value::Optional(optional) => {
            if let Some(value) = optional.data.as_deref() {
                collect_argument_contracts(value, contracts);
            }
        }
        Value::Response(response) => collect_argument_contracts(&response.data, contracts),
        Value::Sequence(SequenceData::List(list)) => {
            for value in &list.data {
                collect_argument_contracts(value, contracts);
            }
        }
        Value::Int(_)
        | Value::UInt(_)
        | Value::Bool(_)
        | Value::Principal(PrincipalData::Standard(_))
        | Value::Sequence(SequenceData::Buffer(_) | SequenceData::String(_)) => {}
    }
}

/// Make a contract the compiler deployed runnable by the interpreter.
///
/// Safe to store: contract definitions live in `metadata_table`, a side store
/// that never reaches the MARF, so this changes no state root. Nothing is
/// re-executed either — the real definition is built in a throwaway store — so
/// the contract's data variables keep whatever they have since become.
fn heal_contract_for_interpreter(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    contract: &QualifiedContractIdentifier,
) -> Result<(), VmExecutionError> {
    let network = store.network();
    let (source, version) = contract_source(store, bitcoin_context, contract)?;
    // Everything the source names, as this node already holds it.
    let referenced = referenced_contracts(contract, &source, version);
    let mut dependencies = Vec::new();
    {
        let mut database = clarity_database(store, bitcoin_context);
        database.begin();
        for identifier in referenced {
            if let Ok(definition) = database.get_contract(&identifier) {
                dependencies.push((identifier, definition));
            }
        }
        database.roll_back()?;
    }
    let rebuilt = match interpretable_contract(network, contract, version, &source, dependencies) {
        Ok(rebuilt) => rebuilt,
        // A contract whose dependencies this node does not hold cannot be
        // deployed beside them, however many are put in first. But it can still
        // be *parsed*: the stub the compiler stored is right about everything
        // except its function bodies, so take its own definition — which is
        // present, being the one asked for — and put the real bodies back.
        Err(_) => rebuilt_from_source(store, bitcoin_context, contract, version, &source)?,
    };
    store
        .replace_contract_definition(contract, &rebuilt.serialize())
        .map_err(|error| VmInternalError::Expect(error.to_string()))?;
    Ok(())
}

/// The stored definition with its stub bodies replaced by the source's own.
///
/// Nothing is deployed, so no other contract has to be present and no top-level
/// expression runs — re-running them would reset every data variable the
/// contract has changed since, which would corrupt state rather than heal it.
fn rebuilt_from_source(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    contract: &QualifiedContractIdentifier,
    version: ClarityVersion,
    source: &str,
) -> Result<clarity::vm::contracts::Contract, VmExecutionError> {
    let functions = defined_functions(contract, version, source);
    if functions.is_empty() {
        return Err(VmInternalError::Expect(format!(
            "no functions could be parsed from the source of {contract}"
        ))
        .into());
    }
    let mut database = clarity_database(store, bitcoin_context);
    database.begin();
    let stored = database.get_contract(contract);
    database.roll_back()?;
    let mut context = (*stored?).clone();
    context.functions = functions;
    Ok(context.into())
}

fn execute_contract_call_outcome_in_context(
    store: &mut MarfStore,
    bitcoin_context: &dyn ChainContext,
    call: ContractCall<'_>,
    cost_tracker: LimitedCostTracker,
) -> Result<ContractCallOutcome, VmExecutionError> {
    let network = store.network();
    let arguments = call
        .arguments
        .iter()
        .map(|argument| {
            let mut bytes = argument.as_slice();
            let value = Value::deserialize_read(&mut bytes, None, false).map_err(|error| {
                VmInternalError::Expect(format!("invalid transaction argument: {error}"))
            })?;
            if !bytes.is_empty() {
                return Err(VmInternalError::Expect(
                    "transaction argument has trailing bytes".to_owned(),
                )
                .into());
            }
            Ok(SymbolicExpression::atom_value(value))
        })
        .collect::<Result<Vec<_>, VmExecutionError>>()?;
    // No `begin` here: `execute_transaction` brackets the call itself, and an
    // extra level leaves the environment nested when it tries to unwind — which
    // `destruct` refuses, losing the call's own answer with it.
    let database = clarity_database(store, bitcoin_context);
    let mut environment = OwnedEnvironment::new_cost_limited(
        network.is_mainnet(),
        network.chain_id(),
        database,
        cost_tracker,
        StacksEpochId::Epoch40,
    );
    let called = format!("{}::{}", call.contract, call.function);
    let result = environment.execute_transaction(
        call.sender,
        call.sponsor,
        call.contract,
        call.function,
        &arguments,
    );
    let Some((_database, cost_tracker)) = environment.destruct() else {
        // A context outlived the call that opened it. Whatever the call was
        // actually complaining about is the useful half, and reporting only
        // that the unwind failed throws it away — which is how this looked
        // like a mystery rather than a bug report.
        // Name the call on the way out, and say when the cause is a stub.
        //
        // clar2wasm's deploy stores placeholder function bodies — the real ones
        // live in the module — so a contract the compiler deployed cannot be
        // run by the interpreter at all: it evaluates the placeholder and
        // reports whatever that is. Left unexplained it reads as a type error
        // in the contract, which is three days of looking in the wrong place.
        return Err(result.err().map_or_else(
            || {
                VmInternalError::Expect(format!("{called} left the database in an invalid state"))
                    .into()
            },
            |error| {
                let hint = if error.to_string().contains("must return response") {
                    " (this contract was deployed by the compiler, which stores \
                     placeholder bodies, so the interpreter cannot run it)"
                } else {
                    ""
                };
                VmInternalError::Expect(format!("{called}: {error}{hint}")).into()
            },
        ));
    };
    match result {
        Ok((value, assets, events)) => {
            let aborted = matches!(&value, Value::Response(response) if !response.committed);
            let receipt = Box::new(TransactionResult {
                value: Some(value),
                cost: cost_tracker.get_total(),
                assets,
                events,
            });
            Ok(if aborted {
                ContractCallOutcome::AbortedByResponse(receipt)
            } else {
                ContractCallOutcome::Success(receipt)
            })
        }
        Err(error) => {
            if is_acceptable_runtime_failure(&error) {
                Ok(ContractCallOutcome::RuntimeFailure {
                    cost: cost_tracker.get_total(),
                    error,
                })
            } else {
                Err(error)
            }
        }
    }
}

/// One contract call, as a transaction carries it.
pub struct ContractCall<'a> {
    pub sender: PrincipalData,
    pub sponsor: Option<PrincipalData>,
    pub contract: QualifiedContractIdentifier,
    pub function: &'a str,
    pub arguments: &'a [Vec<u8>],
}

/// Ask the interpreter a call against a VM's own state and chain context.
///
/// The same state and the same headers the engine reads, so a disagreement is
/// about the engines rather than about what either could see.
pub fn interpret_contract_call(
    vm: &mut nano_vm::Vm,
    call: ContractCall<'_>,
    cost_tracker: LimitedCostTracker,
) -> Result<ContractCallOutcome, VmExecutionError> {
    interpret_contract_call_measured(vm, call, cost_tracker).map(|(outcome, _)| outcome)
}

/// [`interpret_contract_call`], also answering how long the interpreter took.
///
/// Measured here rather than around the call because the healing and its
/// restore are this oracle's own scaffolding: a benchmark that charged them to
/// the interpreter would report our architecture, not the reference engine.
pub fn interpret_contract_call_measured(
    vm: &mut nano_vm::Vm,
    call: ContractCall<'_>,
    cost_tracker: LimitedCostTracker,
) -> Result<(ContractCallOutcome, std::time::Duration), VmExecutionError> {
    let (store, context) = vm.state_and_context();
    // A contract the compiler deployed carries placeholder bodies, which the
    // interpreter would evaluate and report as a type error. Rebuild its
    // definition first: the call's own contract and everything it reaches, since
    // a nested `contract-call?` lands in a contract the compiler may also have
    // deployed and healing only the named one leaves the failure one level down.
    let mut roots = vec![call.contract.clone()];
    for argument in call.arguments {
        let mut bytes = argument.as_slice();
        if let Ok(value) = Value::deserialize_read(&mut bytes, None, false)
            && bytes.is_empty()
        {
            collect_argument_contracts(&value, &mut roots);
        }
    }
    let mut healed = Vec::new();
    for root in roots {
        healed.extend(heal_reachable_contracts(store, context, &root));
    }
    let (store, context) = vm.state_and_context();
    let started = std::time::Instant::now();
    let result = execute_contract_call_outcome_in_context(store, context, call, cost_tracker);
    let took = started.elapsed();
    restore_contracts(store, healed)?;
    result.map(|outcome| (outcome, took))
}

/// Contracts in a state the interpreter cannot run, because the compiler
/// deployed them and stored placeholder bodies.
#[must_use]
pub fn uninterpretable_contracts(store: &MarfStore) -> Vec<QualifiedContractIdentifier> {
    store.stubbed_contracts()
}

/// Make one runnable by the interpreter. Contract definitions live in a side
/// store that never reaches the MARF, so this moves no state root.
pub fn heal_contract(
    vm: &mut nano_vm::Vm,
    contract: &QualifiedContractIdentifier,
) -> Result<(), VmExecutionError> {
    let (store, context) = vm.state_and_context();
    heal_contract_for_interpreter(store, context, contract)
}
