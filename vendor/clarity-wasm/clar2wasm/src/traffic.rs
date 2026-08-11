//! Env-gated accounting for owned Clarity-value traffic.
//!
//! `NANO_VALUE_TRAFFIC` turns it on. Each bucket records logical operations
//! and the bytes those operations own, clone, decode or copy. The counters are
//! thread-local so a benchmark can snapshot around one contract call.

use std::cell::Cell;
use std::sync::LazyLock;

/// One value-traffic boundary. The order is the snapshot's array order.
#[derive(Clone, Copy, Debug)]
#[repr(usize)]
pub enum Traffic {
    /// An owned hex string extracted from SQLite.
    SqliteString,
    /// A full hex string cloned into or out of the side-value cache.
    CacheClone,
    /// An owned hex string returned through `ClarityBackingStore`.
    BackingStoreValue,
    /// A stored value decoded into an owned `Value` tree.
    ValueDecode,
    /// An explicit full `Value` clone made while marshalling.
    ValueClone,
    /// Bytes copied into wasm linear memory.
    WasmWrite,
}

/// How many traffic buckets a snapshot carries.
pub const TRAFFIC: usize = 6;

/// Traffic names in snapshot order.
#[must_use]
pub const fn labels() -> [&'static str; TRAFFIC] {
    [
        "sqlite_string",
        "cache_clone",
        "backing_store_value",
        "value_decode",
        "value_clone",
        "wasm_write",
    ]
}

thread_local! {
    static COUNTS: [Cell<u64>; TRAFFIC] = const { [const { Cell::new(0) }; TRAFFIC] };
    static BYTES: [Cell<u64>; TRAFFIC] = const { [const { Cell::new(0) }; TRAFFIC] };
    static WASM_WRITE_DEPTH: Cell<u32> = const { Cell::new(0) };
    static FORCED_DEPTH: Cell<u32> = const { Cell::new(0) };
}

static ENABLED: LazyLock<bool> = LazyLock::new(|| std::env::var_os("NANO_VALUE_TRAFFIC").is_some());

/// Whether traffic accounting is on for this process.
#[must_use]
pub fn enabled() -> bool {
    *ENABLED || FORCED_DEPTH.with(|depth| depth.get() != 0)
}

/// Record one logical operation and the bytes it handled.
pub fn record(traffic: Traffic, bytes: u64) {
    if !enabled() {
        return;
    }
    COUNTS.with(|counts| {
        let count = &counts[traffic as usize];
        count.set(count.get().saturating_add(1));
    });
    BYTES.with(|sizes| {
        let size = &sizes[traffic as usize];
        size.set(size.get().saturating_add(bytes));
    });
}

/// Measure one outermost recursive write into wasm linear memory.
pub fn wasm_write<E>(action: impl FnOnce() -> Result<(i32, i32), E>) -> Result<(i32, i32), E> {
    if !enabled() {
        return action();
    }
    let outermost = WASM_WRITE_DEPTH.with(|depth| {
        depth.set(depth.get() + 1);
        depth.get() == 1
    });
    let result = action();
    WASM_WRITE_DEPTH.with(|depth| depth.set(depth.get() - 1));
    if outermost {
        if let Ok((representation, payload)) = result.as_ref() {
            record(
                Traffic::WasmWrite,
                u64::try_from(representation.saturating_add(*payload)).unwrap_or_default(),
            );
        }
    }
    result
}

/// Accumulated `(operations, bytes)` per traffic boundary on this thread.
#[must_use]
pub fn snapshot() -> [(u64, u64); TRAFFIC] {
    let mut taken = [(0, 0); TRAFFIC];
    COUNTS.with(|counts| {
        BYTES.with(|sizes| {
            for (slot, (count, size)) in taken.iter_mut().zip(counts.iter().zip(sizes.iter())) {
                *slot = (count.get(), size.get());
            }
        });
    });
    taken
}

/// Measure one synchronous operation even when the environment switch is off.
///
/// This is intended for focused diagnostics and regression tests. Production
/// benchmark runs use `NANO_VALUE_TRAFFIC` and explicit snapshots instead.
pub fn measure<R>(action: impl FnOnce() -> R) -> (R, [(u64, u64); TRAFFIC]) {
    let before = snapshot();
    FORCED_DEPTH.with(|depth| depth.set(depth.get() + 1));
    let result = action();
    FORCED_DEPTH.with(|depth| depth.set(depth.get() - 1));
    let after = snapshot();
    let mut difference = [(0, 0); TRAFFIC];
    for (slot, (after, before)) in difference.iter_mut().zip(after.into_iter().zip(before)) {
        *slot = (after.0 - before.0, after.1 - before.1);
    }
    (result, difference)
}

#[cfg(test)]
mod tests {
    use super::{labels, TRAFFIC};

    #[test]
    fn every_traffic_bucket_has_one_label() {
        assert_eq!(labels().len(), TRAFFIC);
    }
}
