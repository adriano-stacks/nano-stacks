//! Env-gated phase accounting for the runtime, so a benchmark can say where a
//! contract call's wall time goes instead of guessing.
//!
//! `NANO_WASM_PHASES` turns it on; off, every instrumented site pays one branch
//! on a lazily read flag and no clock. Counters are thread-local because the
//! callers that matter execute one call at a time; a harness snapshots before
//! and after a call and diffs.
//!
//! The phases nest: `WasmInvoke` contains the `Host*` buckets, which contain
//! `ValueRead`/`ValueWrite`/`IdentRead`. A report must subtract, not add.

use std::cell::Cell;
use std::sync::LazyLock;
use std::time::Instant;

/// One measured region. The order is the snapshot's array order.
#[derive(Clone, Copy, Debug)]
#[repr(usize)]
pub enum Phase {
    /// Deserializing the callee's stored contract context (recorded by the
    /// embedder, which is the only place that sees it).
    ContractLoad,
    /// `Linker::new` + host functions + cost globals.
    LinkerSetup,
    /// `linker.instantiate`: import resolution, memories, data segments.
    Instantiate,
    /// Export lookups, argument-size writes and argument marshalling.
    CallSetup,
    /// The exported function itself, host calls included.
    WasmInvoke,
    /// Reading the return value back out of linear memory.
    ReturnRead,
    /// Host: data-var get/set.
    HostVar,
    /// Host: map get/set/insert/delete.
    HostMap,
    /// Host: `print` event registration.
    HostEvent,
    /// Host: STX transfer/account.
    HostStx,
    /// Host: runtime shape and size measurement (`save_runtime_shape`,
    /// `runtime_value_size`, `admit_function_argument`, …).
    HostShape,
    /// Clarity values written into linear memory.
    ValueWrite,
    /// Clarity values read out of linear memory.
    ValueRead,
    /// Identifiers (names, serialized types) read out of linear memory.
    IdentRead,
    /// Embedder: database + `GlobalContext` construction and `begin`.
    ContextSetup,
    /// Embedder: module-cache probes for the call's contract arguments.
    ModuleProbe,
    /// `commit`/`handle_tx_result`/`roll_back`: asset maps, event batches and
    /// the rollback wrapper, once per call frame.
    Commit,
}

/// How many phases a snapshot carries.
pub const PHASES: usize = 17;

/// The phase names, in snapshot order.
#[must_use]
pub const fn labels() -> [&'static str; PHASES] {
    [
        "contract_load",
        "linker_setup",
        "instantiate",
        "call_setup",
        "wasm_invoke",
        "return_read",
        "host_var",
        "host_map",
        "host_event",
        "host_stx",
        "host_shape",
        "value_write",
        "value_read",
        "ident_read",
        "context_setup",
        "module_probe",
        "commit",
    ]
}

thread_local! {
    static NANOS: [Cell<u64>; PHASES] = const { [const { Cell::new(0) }; PHASES] };
    static COUNTS: [Cell<u64>; PHASES] = const { [const { Cell::new(0) }; PHASES] };
    /// Same-phase nesting depth, so a recursive region records only its
    /// outermost span: a tuple written field by field is one value write, and
    /// a nested `contract-call?`'s invoke is already inside its caller's.
    /// Distinct phases still nest freely — a nested call's `LinkerSetup`
    /// inside the outer `WasmInvoke` is real cost and is counted.
    static DEPTHS: [Cell<u32>; PHASES] = const { [const { Cell::new(0) }; PHASES] };
}

static ENABLED: LazyLock<bool> = LazyLock::new(|| std::env::var_os("NANO_WASM_PHASES").is_some());

/// Whether phase accounting is on for this process.
#[must_use]
pub fn enabled() -> bool {
    *ENABLED
}

/// Run `action`, charging its wall time to `phase` when accounting is on.
pub fn time<R>(phase: Phase, action: impl FnOnce() -> R) -> R {
    if !enabled() {
        return action();
    }
    let outermost = DEPTHS.with(|depths| {
        let cell = &depths[phase as usize];
        cell.set(cell.get() + 1);
        cell.get() == 1
    });
    let started = outermost.then(Instant::now);
    let result = action();
    DEPTHS.with(|depths| {
        let cell = &depths[phase as usize];
        cell.set(cell.get() - 1);
    });
    finish(phase, started);
    result
}

/// Open a measured region for a span a closure cannot wrap. `None` when off.
///
/// No same-phase nesting guard: only [`time`] regions recurse.
#[must_use]
pub fn start() -> Option<Instant> {
    enabled().then(Instant::now)
}

/// Close a region opened by [`start`], charging it to `phase`.
pub fn finish(phase: Phase, started: Option<Instant>) {
    let Some(started) = started else { return };
    let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    NANOS.with(|nanos| {
        let cell = &nanos[phase as usize];
        cell.set(cell.get().saturating_add(elapsed));
    });
    COUNTS.with(|counts| {
        let cell = &counts[phase as usize];
        cell.set(cell.get().saturating_add(1));
    });
}

/// Accumulated (nanoseconds, invocations) per phase on this thread.
///
/// Monotonic; a harness diffs two snapshots to attribute one region.
#[must_use]
pub fn snapshot() -> [(u64, u64); PHASES] {
    let mut taken = [(0, 0); PHASES];
    NANOS.with(|nanos| {
        COUNTS.with(|counts| {
            for (slot, (time, count)) in taken.iter_mut().zip(nanos.iter().zip(counts.iter())) {
                *slot = (time.get(), count.get());
            }
        });
    });
    taken
}

#[cfg(test)]
mod tests {
    use super::{Phase, snapshot, time};

    #[test]
    fn timing_is_transparent_to_the_result() {
        // The switch is off in tests, so this exercises the pass-through arm.
        let before = snapshot();
        assert_eq!(time(Phase::WasmInvoke, || 7), 7);
        assert_eq!(snapshot(), before);
    }
}
