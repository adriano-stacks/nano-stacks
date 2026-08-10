use clarity::vm::errors::VmExecutionError;
use clarity::vm::Value;

use crate::error::WasmError;

/// Composite values whose run-time shape is wider than their analysed shape.
///
/// An arena belongs to one [`ClarityWasmContext`](crate::initialize::ClarityWasmContext),
/// so it is discarded after that initialization or function invocation. Wasm
/// values carry one-based handles and preservation reuses a nonzero handle;
/// consequently bindings and loops do not copy an already materialized value.
/// New entries are only created when an evaluated handle-zero composite first
/// crosses a preservation or host boundary. Clarity's value and execution-cost
/// limits bound those crossings. An independent entry cap would reject valid
/// executions before their cost limit, so exhaustion is limited by the checked
/// handle space and the process allocator instead.
#[derive(Debug, Default)]
pub struct RuntimeShapeArena {
    values: Vec<Value>,
}

impl RuntimeShapeArena {
    pub fn insert(&mut self, value: Value) -> Result<i32, VmExecutionError> {
        let handle = self.values.len().checked_add(1).ok_or_else(|| {
            crate::error::wasm_error(WasmError::WasmGeneratorError(
                "runtime-shape arena exhausted".to_owned(),
            ))
        })?;
        let handle = i32::try_from(handle).map_err(|_| {
            crate::error::wasm_error(WasmError::WasmGeneratorError(
                "runtime-shape arena exhausted".to_owned(),
            ))
        })?;
        self.values.push(value);
        Ok(handle)
    }

    pub fn get(&self, handle: i32) -> Result<&Value, VmExecutionError> {
        let index = usize::try_from(handle)
            .ok()
            .and_then(|handle| handle.checked_sub(1))
            .ok_or_else(|| invalid_runtime_shape_handle(handle, self.values.len()))?;
        self.values
            .get(index)
            .ok_or_else(|| invalid_runtime_shape_handle(handle, self.values.len()))
    }
}

fn invalid_runtime_shape_handle(handle: i32, entries: usize) -> VmExecutionError {
    crate::error::wasm_error(WasmError::WasmGeneratorError(format!(
        "invalid runtime-shape handle {handle}; arena contains {entries} entries"
    )))
}

pub trait RuntimeShapeStore {
    fn runtime_shapes(&self) -> Option<&RuntimeShapeArena>;
    fn runtime_shapes_mut(&mut self) -> Option<&mut RuntimeShapeArena>;

    fn save_runtime_shape(&mut self, value: Value) -> Result<i32, VmExecutionError> {
        match self.runtime_shapes_mut() {
            Some(arena) => arena.insert(value),
            None => Ok(0),
        }
    }

    fn load_runtime_shape(&self, handle: i32) -> Result<Value, VmExecutionError> {
        self.runtime_shapes()
            .ok_or_else(|| invalid_runtime_shape_handle(handle, 0))?
            .get(handle)
            .cloned()
    }
}

impl RuntimeShapeStore for () {
    fn runtime_shapes(&self) -> Option<&RuntimeShapeArena> {
        None
    }

    fn runtime_shapes_mut(&mut self) -> Option<&mut RuntimeShapeArena> {
        None
    }
}
