use std::cell::Cell;

use clarity::vm::errors::VmExecutionError;
use clarity::vm::types::{ListData, ListTypeData, SequenceData, TupleData};
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
    ///
    /// `extra` is a second capacity to add, which only `concat` has.
    ///
    /// `delta` and `cap` are how the words that inherit a capacity differ:
    /// `filter` keeps it (`0`, `None`), `append` adds one
    /// (`special_append`: `new_list(next_entry_type, size + 1)`), and
    /// `as-max-len?` reduces it (`special_as_max_len`:
    /// `type_signature.reduce_max_len(expected)`).
    ///
    /// An inherited `NoType` entry is *not* carried: the reference rebuilds from
    /// the element in that case (`special_append` returns `cons_list` when the
    /// entry type is `NoType`), so the result is measured as what it holds.
    fn save_runtime_shape_inheriting(
        &mut self,
        value: Value,
        inherited: Option<ListTypeData>,
        max_len: u32,
        delta: u32,
        cap: Option<u32>,
        extra: u32,
    ) -> Result<i32, VmExecutionError> {
        let value = match value {
            Value::Sequence(SequenceData::List(mut list)) => {
                let (entry, base) = match inherited {
                    Some(inherited) if !inherited.get_list_item_type().is_no_type() => {
                        let max_len = inherited.get_max_len();
                        (inherited.get_list_item_type().clone(), max_len)
                    }
                    _ => (list.type_signature.get_list_item_type().clone(), max_len),
                };
                let grown = base.saturating_add(delta).saturating_add(extra);
                let capped = cap.map_or(grown, |cap| grown.min(cap));
                list.type_signature = ListTypeData::new_list(entry, capped).map_err(|error| {
                    crate::error::wasm_error(WasmError::WasmGeneratorError(format!(
                        "cannot inherit a list capacity of {capped}: {error}"
                    )))
                })?;
                Value::Sequence(SequenceData::List(list))
            }
            other => other,
        };
        self.save_runtime_shape(value)
    }

    /// This entry's list capacity, for a caller that only needs the number.
    ///
    /// `concat` sums its arguments' capacities
    /// (`ListData::append`: `max_len = self.max_len + other.max_len`), and only
    /// the first of them is the value being rebuilt — the rest contribute a
    /// number and nothing else.
    fn runtime_shape_list_capacity(&self, handle: i32) -> Result<u32, VmExecutionError> {
        Ok(self
            .runtime_shape_list_type(handle)?
            .map_or(0, |list| list.get_max_len()))
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

    /// Sanitize an entry's capacities, keeping everything else it says.
    ///
    /// `Value::cons_list` rebuilds each element it stores against the entry type
    /// it derived, so an element keeps only the capacity it is using — every
    /// list inside it included, however deep. What sanitizing does *not* do is
    /// widen the element's own types to the list's: an element whose response
    /// arm is `NoType` keeps that, and the list's entry type is the least
    /// supertype of what the elements said.
    ///
    /// Which is why this narrows rather than forgetting. Dropping the entry
    /// entirely would answer the *analysed* type for the value, and the analysed
    /// type of a list of `(response principal NoType)` elements is a list of
    /// `(response principal principal)`: every element would come back claiming
    /// an arm it never had, which is a different value and not just a different
    /// width.
    fn narrow_runtime_shape(&mut self, handle: i32) -> Result<i32, VmExecutionError> {
        if handle == 0 {
            return Ok(0);
        }
        let value = self.load_runtime_shape(handle)?;
        let narrowed = narrow_capacities(value.clone())?;
        // By size, not by `==`: Clarity value equality is semantic and ignores
        // the capacities entirely, so an emptied `(list 12000 uint)` and an
        // emptied `(list 0 NoType)` compare equal — and comparing that way threw
        // every narrowing away silently.
        if measured_size(&narrowed)? == measured_size(&value)? {
            return Ok(handle);
        }
        self.save_runtime_shape(narrowed)
    }
    /// Sanitize an entry's *elements*, keeping what the entry says about itself.
    ///
    /// `Value::cons_list` says two things at once, and they disagree: the list's
    /// entry type is the least supertype of the elements *as they arrived*, so a
    /// list built from a widened element is measured at that width; but each
    /// element it stores is rebuilt against that entry type, so an element read
    /// back out is only as big as what it holds. On mainnet the first is what a
    /// `print` of the list charges and the second is what `map` charges per
    /// iteration, so getting either one wrong is visible.
    fn sanitize_runtime_shape_elements(&mut self, handle: i32) -> Result<i32, VmExecutionError> {
        if handle == 0 {
            return Ok(0);
        }
        let value = self.load_runtime_shape(handle)?;
        let Value::Sequence(SequenceData::List(list)) = value else {
            return Ok(handle);
        };
        let mut items = Vec::with_capacity(list.data.len());
        let mut changed = false;
        for item in list.data.iter() {
            let narrowed = narrow_capacities(item.clone())?;
            changed = changed || measured_size(&narrowed)? != measured_size(item)?;
            items.push(narrowed);
        }
        if !changed {
            return Ok(handle);
        }
        self.save_runtime_shape(Value::Sequence(SequenceData::List(ListData {
            data: items,
            type_signature: list.type_signature,
        })))
    }
}

/// Rebuild every sequence capacity in a value from what it holds.
///
/// The constructors do the deriving: `cons_list_unsanitized` takes the entry
/// type from the elements and the capacity from their count, and
/// `TupleData::from_data` takes each field's from the field. Buffers and strings
/// carry no capacity apart from their own length, so they are already narrow.
fn narrow_capacities(value: Value) -> Result<Value, VmExecutionError> {
    let narrowed = match value {
        Value::Sequence(SequenceData::List(list)) => {
            let mut items = Vec::with_capacity(list.data.len());
            for item in list.data {
                items.push(narrow_capacities(item)?);
            }
            Value::cons_list_unsanitized(items).map_err(narrowing_failed)?
        }
        Value::Tuple(tuple) => {
            let mut fields = Vec::with_capacity(tuple.data_map.len());
            for (name, field) in tuple.data_map {
                fields.push((name, narrow_capacities(field)?));
            }
            Value::Tuple(TupleData::from_data(fields).map_err(narrowing_failed)?)
        }
        Value::Optional(optional) => match optional.data {
            Some(inner) => Value::some(narrow_capacities(*inner)?).map_err(narrowing_failed)?,
            None => Value::none(),
        },
        Value::Response(response) => {
            let inner = narrow_capacities(*response.data)?;
            if response.committed {
                Value::okay(inner).map_err(narrowing_failed)?
            } else {
                Value::error(inner).map_err(narrowing_failed)?
            }
        }
        other => other,
    };
    Ok(narrowed)
}

/// `Value::size()`, which is the only thing a capacity changes.
fn measured_size(value: &Value) -> Result<u32, VmExecutionError> {
    value
        .size()
        .map_err(|_| crate::error::wasm_error(WasmError::ValueTypeMismatch))
}

fn narrowing_failed(error: impl std::fmt::Display) -> VmExecutionError {
    crate::error::wasm_error(WasmError::WasmGeneratorError(format!(
        "cannot narrow a runtime shape's capacities: {error}"
    )))
}

impl RuntimeShapeStore for () {
    fn runtime_shapes(&self) -> Option<&RuntimeShapeArena> {
        None
    }

    fn runtime_shapes_mut(&mut self) -> Option<&mut RuntimeShapeArena> {
        None
    }
}
