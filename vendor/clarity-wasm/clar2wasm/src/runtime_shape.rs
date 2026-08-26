use std::cell::Cell;

use clarity::vm::errors::VmExecutionError;
use clarity::vm::types::{ListTypeData, SequenceData};
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

    /// This entry's list type, which is what the reference sizes a list by.
    ///
    /// `ListTypeData::inner_size` is `type_signature_size + max_len *
    /// entry_type.size()`, and a value carries its own type — so a list whose
    /// run-time length is shorter than the capacity it was constructed with is
    /// still sized by that capacity, over that entry type. Both halves matter:
    /// an emptied list's own entry type is `NoType`, whose size is 1 rather
    /// than a `uint`'s 16. `None` for anything that is not a list.
    pub fn list_type(&self, handle: i32) -> Result<Option<ListTypeData>, VmExecutionError> {
        Ok(match self.get(handle)? {
            Value::Sequence(SequenceData::List(list)) => Some(list.type_signature.clone()),
            _ => None,
        })
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

    /// Materialize a value that inherits another list's `max_len`.
    ///
    /// `filter` is the one sequence word the reference implements by mutating
    /// its argument in place: `special_filter` evaluates the sequence, calls
    /// `SequenceData::filter` on it and returns the same value, so the result
    /// keeps the `type_signature` — and therefore the `max_len` — the input
    /// carried, however many elements were dropped. Rebuilding the list from
    /// the kept elements alone would size it by its new length, which is what
    /// the compiler used to do and what parted it from the reference.
    /// `inherited` is the input's own list type, when the input was itself a
    /// widened value and therefore carries one. Otherwise the input's capacity
    /// was its element count, `max_len`, and its entry type is the result's.
    fn save_runtime_shape_inheriting(
        &mut self,
        value: Value,
        inherited: Option<ListTypeData>,
        max_len: u32,
    ) -> Result<i32, VmExecutionError> {
        let value = match value {
            Value::Sequence(SequenceData::List(mut list)) => {
                list.type_signature = match inherited {
                    Some(inherited) => inherited,
                    None => {
                        let entry = list.type_signature.get_list_item_type().clone();
                        ListTypeData::new_list(entry, max_len).map_err(|error| {
                            crate::error::wasm_error(WasmError::WasmGeneratorError(format!(
                                "cannot inherit a list capacity of {max_len}: {error}"
                            )))
                        })?
                    }
                };
                Value::Sequence(SequenceData::List(list))
            }
            other => other,
        };
        self.save_runtime_shape(value)
    }

    /// This entry's list type. See [`RuntimeShapeArena::list_type`].
    fn runtime_shape_list_type(
        &self,
        handle: i32,
    ) -> Result<Option<ListTypeData>, VmExecutionError> {
        self.runtime_shapes()
            .ok_or_else(|| invalid_runtime_shape_handle(handle, 0))?
            .list_type(handle)
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
