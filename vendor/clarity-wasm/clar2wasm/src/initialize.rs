use clarity::vm::analysis::ContractAnalysis;
use clarity::vm::contexts::GlobalContext;
use clarity::vm::costs::cost_functions::ClarityCostFunction;
use clarity::vm::costs::{runtime_cost, CostTracker};
use clarity::vm::errors::{RuntimeError, VmExecutionError};
use clarity::vm::events::*;
use clarity::vm::types::signatures::CallableSubtype;
use clarity::vm::types::{
    AssetIdentifier, BuffData, CallableData, FunctionType, PrincipalData,
    QualifiedContractIdentifier, TypeSignature,
};
use clarity::vm::{CallStack, ContractContext, Value};
use stacks_common::types::chainstate::StacksBlockId;
use wasmtime::{AsContextMut, Linker, Module, Store, Val};

use crate::cost::{CostGlobals, CostMeter};
use crate::error::WasmError;
use crate::error_mapping;
use crate::linker::{link_cost_globals, link_host_functions};
use crate::wasm_utils::*;
use crate::{CompiledContract, ModuleCache};

// The context used when making calls into the Wasm module.
pub struct ClarityWasmContext<'a, 'b, 'hooks> {
    pub global_context: &'a mut GlobalContext<'b, 'hooks>,
    contract_context: Option<&'a ContractContext>,
    contract_context_mut: Option<&'a mut ContractContext>,
    pub call_stack: &'a mut CallStack,
    pub sender: Option<PrincipalData>,
    pub caller: Option<PrincipalData>,
    pub sponsor: Option<PrincipalData>,
    // Stack of senders, used for `as-contract` expressions.
    sender_stack: Vec<PrincipalData>,
    /// Stack of callers, used for `contract-call?` and `as-contract` expressions.
    caller_stack: Vec<PrincipalData>,
    /// Stack of block hashes, used for `at-block` expressions.
    bhh_stack: Vec<StacksBlockId>,
    /// Contract analysis data, used for typing information, and only available
    /// when initializing a contract. Should always be `Some` when initializing
    /// a contract, and `None` otherwise.
    pub contract_analysis: Option<&'a ContractAnalysis>,
    pub cost_globals: Option<CostGlobals>,
    pub module_cache: &'a ModuleCache,
}

impl<'a, 'b, 'hooks> ClarityWasmContext<'a, 'b, 'hooks> {
    #[allow(clippy::too_many_arguments)]
    pub fn new_init(
        global_context: &'a mut GlobalContext<'b, 'hooks>,
        contract_context: &'a mut ContractContext,
        call_stack: &'a mut CallStack,
        sender: Option<PrincipalData>,
        caller: Option<PrincipalData>,
        sponsor: Option<PrincipalData>,
        contract_analysis: Option<&'a ContractAnalysis>,
        cost_globals: Option<CostGlobals>,
        module_cache: &'a ModuleCache,
    ) -> Self {
        ClarityWasmContext {
            global_context,
            contract_context: None,
            contract_context_mut: Some(contract_context),
            call_stack,
            sender,
            caller,
            sponsor,
            sender_stack: vec![],
            caller_stack: vec![],
            bhh_stack: vec![],
            contract_analysis,
            cost_globals,
            module_cache,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_run(
        global_context: &'a mut GlobalContext<'b, 'hooks>,
        contract_context: &'a ContractContext,
        call_stack: &'a mut CallStack,
        sender: Option<PrincipalData>,
        caller: Option<PrincipalData>,
        sponsor: Option<PrincipalData>,
        contract_analysis: Option<&'a ContractAnalysis>,
        module_cache: &'a ModuleCache,
    ) -> Self {
        ClarityWasmContext {
            global_context,
            contract_context: Some(contract_context),
            contract_context_mut: None,
            call_stack,
            sender,
            caller,
            sponsor,
            sender_stack: vec![],
            caller_stack: vec![],
            bhh_stack: vec![],
            contract_analysis,
            cost_globals: None,
            module_cache,
        }
    }

    pub fn push_sender(&mut self, sender: PrincipalData) {
        if let Some(current) = self.sender.take() {
            self.sender_stack.push(current);
        }
        self.sender = Some(sender);
    }

    pub fn pop_sender(&mut self) -> Result<PrincipalData, VmExecutionError> {
        self.sender
            .take()
            .ok_or(RuntimeError::NoSenderInContext.into())
            .inspect(|_| {
                self.sender = self.sender_stack.pop();
            })
    }

    pub fn push_caller(&mut self, caller: PrincipalData) {
        if let Some(current) = self.caller.take() {
            self.caller_stack.push(current);
        }
        self.caller = Some(caller);
    }

    pub fn pop_caller(&mut self) -> Result<PrincipalData, VmExecutionError> {
        self.caller
            .take()
            .ok_or(RuntimeError::NoCallerInContext.into())
            .inspect(|_| {
                self.caller = self.caller_stack.pop();
            })
    }

    pub fn push_at_block(&mut self, bhh: StacksBlockId) {
        self.bhh_stack.push(bhh);
    }

    pub fn pop_at_block(&mut self) -> Result<StacksBlockId, VmExecutionError> {
        self.bhh_stack
            .pop()
            .ok_or(crate::error::wasm_error(WasmError::WasmGeneratorError(
                "Could not pop at_block".to_string(),
            )))
    }

    /// Return an immutable reference to the contract_context
    pub fn contract_context(&self) -> &ContractContext {
        if let Some(contract_context) = &self.contract_context {
            contract_context
        } else if let Some(contract_context) = &self.contract_context_mut {
            contract_context
        } else {
            unreachable!("contract_context and contract_context_mut are both None")
        }
    }

    /// Return a mutable reference to the contract_context if we are currently
    /// initializing a contract, else, return an error.
    pub fn contract_context_mut(&mut self) -> Result<&mut ContractContext, VmExecutionError> {
        match &mut self.contract_context_mut {
            Some(contract_context) => Ok(contract_context),
            None => Err(crate::error::wasm_error(
                WasmError::DefineFunctionCalledInRunMode,
            )),
        }
    }

    pub fn push_to_event_batch(&mut self, event: StacksTransactionEvent) {
        if let Some(batch) = self.global_context.event_batches.last_mut() {
            batch.0.events.push(event);
        }
    }

    pub fn construct_print_transaction_event(
        contract_id: &QualifiedContractIdentifier,
        value: &Value,
    ) -> StacksTransactionEvent {
        let print_event = SmartContractEventData {
            key: (contract_id.clone(), "print".to_string()),
            value: value.clone(),
        };

        StacksTransactionEvent::SmartContractEvent(print_event)
    }

    pub fn register_print_event(&mut self, value: Value) -> Result<(), VmExecutionError> {
        let event = Self::construct_print_transaction_event(
            &self.contract_context().contract_identifier,
            &value,
        );

        self.push_to_event_batch(event);
        Ok(())
    }

    pub fn register_stx_transfer_event(
        &mut self,
        sender: PrincipalData,
        recipient: PrincipalData,
        amount: u128,
        memo: BuffData,
    ) -> Result<(), VmExecutionError> {
        let event_data = STXTransferEventData {
            sender,
            recipient,
            amount,
            memo,
        };
        let event = StacksTransactionEvent::STXEvent(STXEventType::STXTransferEvent(event_data));

        self.push_to_event_batch(event);
        Ok(())
    }

    pub fn register_stx_burn_event(
        &mut self,
        sender: PrincipalData,
        amount: u128,
    ) -> Result<(), VmExecutionError> {
        let event_data = STXBurnEventData { sender, amount };
        let event = StacksTransactionEvent::STXEvent(STXEventType::STXBurnEvent(event_data));

        self.push_to_event_batch(event);
        Ok(())
    }

    pub fn register_nft_transfer_event(
        &mut self,
        sender: PrincipalData,
        recipient: PrincipalData,
        value: Value,
        asset_identifier: AssetIdentifier,
    ) -> Result<(), VmExecutionError> {
        let event_data = NFTTransferEventData {
            sender,
            recipient,
            asset_identifier,
            value,
        };
        let event = StacksTransactionEvent::NFTEvent(NFTEventType::NFTTransferEvent(event_data));

        self.push_to_event_batch(event);
        Ok(())
    }

    pub fn register_nft_mint_event(
        &mut self,
        recipient: PrincipalData,
        value: Value,
        asset_identifier: AssetIdentifier,
    ) -> Result<(), VmExecutionError> {
        let event_data = NFTMintEventData {
            recipient,
            asset_identifier,
            value,
        };
        let event = StacksTransactionEvent::NFTEvent(NFTEventType::NFTMintEvent(event_data));

        self.push_to_event_batch(event);
        Ok(())
    }

    pub fn register_nft_burn_event(
        &mut self,
        sender: PrincipalData,
        value: Value,
        asset_identifier: AssetIdentifier,
    ) -> Result<(), VmExecutionError> {
        let event_data = NFTBurnEventData {
            sender,
            asset_identifier,
            value,
        };
        let event = StacksTransactionEvent::NFTEvent(NFTEventType::NFTBurnEvent(event_data));

        self.push_to_event_batch(event);
        Ok(())
    }

    pub fn register_ft_transfer_event(
        &mut self,
        sender: PrincipalData,
        recipient: PrincipalData,
        amount: u128,
        asset_identifier: AssetIdentifier,
    ) -> Result<(), VmExecutionError> {
        let event_data = FTTransferEventData {
            sender,
            recipient,
            asset_identifier,
            amount,
        };
        let event = StacksTransactionEvent::FTEvent(FTEventType::FTTransferEvent(event_data));

        self.push_to_event_batch(event);
        Ok(())
    }

    pub fn register_ft_mint_event(
        &mut self,
        recipient: PrincipalData,
        amount: u128,
        asset_identifier: AssetIdentifier,
    ) -> Result<(), VmExecutionError> {
        let event_data = FTMintEventData {
            recipient,
            asset_identifier,
            amount,
        };
        let event = StacksTransactionEvent::FTEvent(FTEventType::FTMintEvent(event_data));

        self.push_to_event_batch(event);
        Ok(())
    }

    pub fn register_ft_burn_event(
        &mut self,
        sender: PrincipalData,
        amount: u128,
        asset_identifier: AssetIdentifier,
    ) -> Result<(), VmExecutionError> {
        let event_data = FTBurnEventData {
            sender,
            asset_identifier,
            amount,
        };
        let event = StacksTransactionEvent::FTEvent(FTEventType::FTBurnEvent(event_data));

        self.push_to_event_batch(event);
        Ok(())
    }
}

/// Successful return of a contract initialization
///
/// Contains the result of the execution of the top-level expressions, and the cost of executing
/// them.
#[derive(Debug, PartialEq)]
pub struct ContractInitReturn {
    pub ret: Option<Value>,
    pub cost: CostMeter,
}

/// Initialize a contract, executing all of the top-level expressions and
/// registering all of the definitions in the context. Returns the value
/// returned from the last top-level expression.
pub fn initialize_contract(
    global_context: &mut GlobalContext,
    contract_context: &mut ContractContext,
    sponsor: Option<PrincipalData>,
    contract_analysis: &ContractAnalysis,
    wasm: &[u8],
    module_cache: &ModuleCache,
) -> Result<ContractInitReturn, VmExecutionError> {
    let publisher: PrincipalData = contract_context.contract_identifier.issuer.clone().into();

    let mut call_stack = CallStack::new();
    let epoch = global_context.epoch_id;
    let clarity_version = *contract_context.get_clarity_version();
    let engine = wasmtime::Engine::default();
    let init_context = ClarityWasmContext::new_init(
        global_context,
        contract_context,
        &mut call_stack,
        Some(publisher.clone()),
        Some(publisher),
        sponsor.clone(),
        Some(contract_analysis),
        None,
        module_cache,
    );
    let module = Module::from_binary(&engine, wasm)
        .map_err(|e| crate::error::wasm_error(WasmError::UnableToLoadModule(e)))?;
    let mut store = Store::new(&engine, init_context);
    let mut linker = Linker::new(&engine);
    // Link in the host interface functions and globals.
    link_host_functions(&mut linker)?;
    store.data_mut().cost_globals = Some(
        link_cost_globals(&mut linker, &mut store.as_context_mut())
            .map_err(|e| crate::error::wasm_error(WasmError::UnableToLoadModule(e.into())))?,
    );

    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|e| crate::error::wasm_error(WasmError::UnableToLoadModule(e)))?;

    // Call the `.top-level` function, which contains all top-level expressions
    // from the contract.
    let top_level = instance
        .get_func(&mut store, ".top-level")
        .ok_or(crate::error::wasm_error(WasmError::DefinesNotFound))?;

    // Get the return type of the top-level expressions function
    let ty = top_level.ty(&mut store);
    let results_iter = ty.results();
    let mut results = vec![];
    for result_ty in results_iter {
        results.push(placeholder_for_type(result_ty));
    }

    top_level
        .call(&mut store, &[], results.as_mut_slice())
        .map_err(|e| {
            error_mapping::resolve_error(e, instance, &mut store, &epoch, &clarity_version)
        })?;

    // Get the type of the last top-level expression with a return value
    // or default to `None`.
    let return_type = contract_analysis.expressions.iter().rev().find_map(|expr| {
        contract_analysis
            .type_map
            .as_ref()
            .and_then(|type_map| type_map.get_type_expected(expr))
    });

    let ret = if let Some(return_type) = return_type {
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;
        wasm_to_clarity_value(return_type, 0, &results, memory, &mut &mut store, epoch)
            .map(|(val, _offset)| val)?
    } else {
        None
    };

    let remaining = store
        .data()
        .cost_globals
        .ok_or(crate::error::wasm_error(WasmError::GlobalNotFound(
            "cost-*".to_string(),
        )))?
        .remaining_costs(&mut store)
        .map_err(|e| crate::error::wasm_error(WasmError::UnableToLoadModule(e)))?;
    let cost = CostMeter::used_from_remaining(remaining);

    Ok(ContractInitReturn { ret, cost })
}

#[allow(clippy::too_many_arguments)]
pub fn call_function(
    function_name: &str,
    arguments: &[Value],
    module: &CompiledContract,
    global_context: &mut GlobalContext,
    contract_context: &ContractContext,
    call_stack: &mut CallStack,
    sender: Option<PrincipalData>,
    caller: Option<PrincipalData>,
    sponsor: Option<PrincipalData>,
    module_cache: &ModuleCache,
) -> Result<Value, VmExecutionError> {
    let contract_size = global_context
        .database
        .get_contract_size(&contract_context.contract_identifier)?;
    runtime_cost(
        ClarityCostFunction::LoadContract,
        global_context,
        contract_size,
    )?;
    let function = contract_context
        .lookup_function(function_name)
        .ok_or_else(|| {
            clarity::vm::errors::RuntimeCheckErrorKind::UndefinedFunction(function_name.into())
        })?;
    let function_type = module
        .analysis
        .get_public_function_type(function_name)
        .or_else(|| module.analysis.get_read_only_function_type(function_name))
        .or_else(|| module.analysis.get_private_function(function_name))
        .ok_or(crate::error::wasm_error(WasmError::NotInDatabase(
            function_name.into(),
        )))?;
    let return_type = match function_type {
        FunctionType::Fixed(function) => function.returns.clone(),
        _ => {
            return Err(crate::error::wasm_error(WasmError::InvalidFunctionKind(
                function_name.into(),
            )));
        }
    };
    let expected_arguments = function.get_arg_types();
    if arguments.len() != expected_arguments.len() {
        return Err(
            clarity::vm::errors::RuntimeCheckErrorKind::IncorrectArgumentCount(
                expected_arguments.len(),
                arguments.len(),
            )
            .into(),
        );
    }

    let epoch = global_context.epoch_id;
    let clarity_version = *contract_context.get_clarity_version();
    let engine = wasmtime::Engine::default();
    let wasm_module = Module::from_binary(&engine, &module.wasm)
        .map_err(|error| crate::error::wasm_error(WasmError::UnableToLoadModule(error)))?;
    let context = ClarityWasmContext::new_run(
        global_context,
        contract_context,
        call_stack,
        sender.clone(),
        caller,
        sponsor.clone(),
        Some(&module.analysis),
        module_cache,
    );
    let mut store = Store::new(&engine, context);
    let mut linker = Linker::new(&engine);
    link_host_functions(&mut linker)?;
    store.data_mut().cost_globals = Some(link_cost_globals(&mut linker, &mut store)?);
    let instance = linker
        .instantiate(&mut store, &wasm_module)
        .map_err(|error| crate::error::wasm_error(WasmError::UnableToLoadModule(error)))?;
    let memory = instance
        .get_memory(&mut store, "memory")
        .ok_or(crate::error::wasm_error(WasmError::MemoryNotFound))?;
    let stack_pointer =
        instance
            .get_global(&mut store, "stack-pointer")
            .ok_or(crate::error::wasm_error(WasmError::GlobalNotFound(
                "stack-pointer".into(),
            )))?;
    let mut offset = stack_pointer
        .get(&mut store)
        .i32()
        .ok_or(crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
    let mut wasm_arguments = Vec::new();
    for (argument, expected_type) in arguments.iter().zip(expected_arguments) {
        let argument = implicit_contract_cast(expected_type, argument);
        if !expected_type.admits(&epoch, &argument)? {
            return Err(clarity::vm::errors::RuntimeCheckErrorKind::TypeError(
                Box::new(expected_type.clone()),
                Box::new(TypeSignature::type_of(&argument)?),
            )
            .into());
        }
        let (values, next_offset) =
            pass_argument_to_wasm(memory, &mut store, expected_type, &argument, offset)?;
        wasm_arguments.extend(values);
        offset = next_offset;
    }
    stack_pointer
        .set(&mut store, Val::I32(offset))
        .map_err(|error| crate::error::wasm_error(WasmError::Runtime(error)))?;
    let wasm_function = instance
        .get_func(&mut store, function_name)
        .ok_or_else(|| {
            clarity::vm::errors::RuntimeCheckErrorKind::UndefinedFunction(function_name.into())
        })?;
    let mut results = wasm_value_types(&return_type)
        .into_iter()
        .map(placeholder_for_type)
        .collect::<Vec<_>>();
    let call_result = wasm_function.call(&mut store, &wasm_arguments, &mut results);
    let remaining = store
        .data()
        .cost_globals
        .ok_or(crate::error::wasm_error(WasmError::GlobalNotFound(
            "cost-*".to_string(),
        )))?
        .remaining_costs(&mut store)
        .map_err(|error| crate::error::wasm_error(WasmError::UnableToLoadModule(error)))?;
    let cost = CostMeter::used_from_remaining(remaining);
    store
        .data_mut()
        .global_context
        .cost_track
        .add_cost(cost.into())?;
    call_result.map_err(|error| {
        error_mapping::resolve_error(error, instance, &mut store, &epoch, &clarity_version)
    })?;
    let (value, _) =
        wasm_to_clarity_value(&return_type, 0, &results, memory, &mut &mut store, epoch)?;
    drop(store);
    let value = value.ok_or(crate::error::wasm_error(WasmError::Expect(
        "function returned no value".into(),
    )))?;
    if let Some(handler) = global_context.database.get_cc_special_cases_handler() {
        handler(
            global_context,
            sender.as_ref(),
            sponsor.as_ref(),
            &contract_context.contract_identifier,
            function_name,
            arguments,
            &value,
        )?;
    }
    Ok(value)
}

fn implicit_contract_cast(expected_type: &TypeSignature, argument: &Value) -> Value {
    match (expected_type, argument) {
        (
            TypeSignature::CallableType(CallableSubtype::Trait(trait_identifier)),
            Value::Principal(PrincipalData::Contract(contract_identifier)),
        ) => Value::CallableContract(CallableData {
            contract_identifier: contract_identifier.clone(),
            trait_identifier: Some(trait_identifier.clone()),
        }),
        _ => argument.clone(),
    }
}
