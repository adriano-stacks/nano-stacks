use std::string::FromUtf8Error;

use clarity::vm::errors::{VmExecutionError, VmInternalError};

#[derive(Debug)]
pub enum WasmError {
    WasmGeneratorError(String),
    ModuleNotFound,
    DefinesNotFound,
    MemoryNotFound,
    GlobalNotFound(String),
    NotInDatabase(String),
    WasmCompileFailed(wasmtime::Error),
    UnableToLoadModule(wasmtime::Error),
    UnableToLinkHostFunction(String, wasmtime::Error),
    UnableToReadIdentifier(FromUtf8Error),
    UnableToReadMemory(wasmtime::Error),
    UnableToWriteMemory(wasmtime::Error),
    ValueTypeMismatch,
    InvalidNoTypeInValue,
    InvalidListUnionTypeInValue,
    InvalidFunctionKind(String),
    DefineFunctionCalledInRunMode,
    InvalidIndicator(i32),
    Runtime(wasmtime::Error),
    InvalidTypeDescription,
    Expect(String),
}

impl std::fmt::Display for WasmError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for WasmError {}

pub fn wasm_error(error: WasmError) -> VmExecutionError {
    VmExecutionError::Internal(VmInternalError::InvariantViolation(error.to_string()))
}

impl From<WasmError> for VmExecutionError {
    fn from(error: WasmError) -> Self {
        wasm_error(error)
    }
}
