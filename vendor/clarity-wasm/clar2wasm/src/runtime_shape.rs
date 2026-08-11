use std::cell::Cell;

use clarity::vm::errors::VmExecutionError;
use clarity::vm::Value;

use crate::error::WasmError;

type Measurements = Cell<(Option<u32>, Option<u32>)>;

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
    /// Memoized `Value::size()` and `serialized_size()` per entry, filled on
    /// first ask. Entries are immutable once inserted, so the answers are
    /// too — and a fold asks about its accumulator on every iteration, which
    /// cloned the whole value out and re-derived its type each time.
    measurements: Vec<Measurements>,
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
        self.measurements.push(Cell::new((None, None)));
        Ok(handle)
    }

    pub fn get(&self, handle: i32) -> Result<&Value, VmExecutionError> {
        self.values
            .get(self.index(handle)?)
            .ok_or_else(|| invalid_runtime_shape_handle(handle, self.values.len()))
    }

    /// This entry's `Value::size()`, computed once.
    pub fn value_size(&self, handle: i32) -> Result<u32, VmExecutionError> {
        let cell = self.measurement(handle)?;
        let (size, serialized) = cell.get();
        if let Some(size) = size {
            return Ok(size);
        }
        let size = self
            .get(handle)?
            .size()
            .map_err(|_| crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
        cell.set((Some(size), serialized));
        Ok(size)
    }

    /// This entry's consensus `serialized_size()`, computed once.
    pub fn serialized_size(&self, handle: i32) -> Result<u32, VmExecutionError> {
        let cell = self.measurement(handle)?;
        let (size, serialized) = cell.get();
        if let Some(serialized) = serialized {
            return Ok(serialized);
        }
        let serialized = self
            .get(handle)?
            .serialized_size()
            .map_err(|_| crate::error::wasm_error(WasmError::ValueTypeMismatch))?;
        cell.set((size, Some(serialized)));
        Ok(serialized)
    }

    fn measurement(&self, handle: i32) -> Result<&Measurements, VmExecutionError> {
        self.measurements
            .get(self.index(handle)?)
            .ok_or_else(|| invalid_runtime_shape_handle(handle, self.values.len()))
    }

    fn index(&self, handle: i32) -> Result<usize, VmExecutionError> {
        usize::try_from(handle)
            .ok()
            .and_then(|handle| handle.checked_sub(1))
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

    /// The entry's `Value::size()`, without cloning the value out.
    fn runtime_shape_value_size(&self, handle: i32) -> Result<u32, VmExecutionError> {
        self.runtime_shapes()
            .ok_or_else(|| invalid_runtime_shape_handle(handle, 0))?
            .value_size(handle)
    }

    /// The entry's `serialized_size()`, without cloning the value out.
    fn runtime_shape_serialized_size(&self, handle: i32) -> Result<u32, VmExecutionError> {
        self.runtime_shapes()
            .ok_or_else(|| invalid_runtime_shape_handle(handle, 0))?
            .serialized_size(handle)
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
