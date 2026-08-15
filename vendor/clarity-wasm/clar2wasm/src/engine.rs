use wasmtime::{
    Collector, Config, Engine, InstanceAllocationStrategy, OptLevel, ProfilingStrategy, Strategy,
    WasmBacktraceDetails,
};

pub const WASMTIME_VERSION: &str = "36.0.13";

/// Every setting that selects or admits consensus Wasm execution.
///
/// This is part of both the release identity and the native-module cache key.
/// Compile-time omissions are named too: their configuration methods do not
/// exist in this deliberately minimal Wasmtime build.
pub const ENGINE_CONFIG_ID: &str = concat!(
    "wasmtime=36.0.13;",
    "cargo=cranelift,gc,gc-null,runtime,std;",
    "compiler=cranelift;opt=speed;nan=canonical;collector=null;",
    "profiler=none;debug-info=false;native-unwind-info=false;",
    "wasm-backtrace=true;wasm-backtrace-details=disable;address-map=true;",
    "fuel=false;epoch-interruption=false;max-wasm-stack=524288;allocator=ondemand;",
    "store-memory=2147483648;store-table-elements=20;store-instances=1;",
    "store-tables=1;store-memories=1;grow-failure=trap;",
    "enabled=reference-types,simd,bulk-memory,multi-value;",
    "disabled=threads,shared-everything-threads,function-references,gc,wide-arithmetic,",
    "relaxed-simd,tail-call,custom-page-sizes,multi-memory,memory64,extended-const,",
    "stack-switching,component-model,exceptions,wasi,winch,async,parallel-compilation,",
    "coredump,profiling"
);

pub const STORE_MEMORY_BYTES: usize = 2 * 1024 * 1024 * 1024;
pub const STORE_TABLE_ELEMENTS: usize = 20;

pub(crate) fn store_limits() -> wasmtime::StoreLimits {
    wasmtime::StoreLimitsBuilder::new()
        .memory_size(STORE_MEMORY_BYTES)
        .table_elements(STORE_TABLE_ELEMENTS)
        .instances(1)
        .tables(1)
        .memories(1)
        .trap_on_grow_failure(true)
        .build()
}

/// Construct the sole engine configuration used for consensus modules.
pub fn consensus_engine() -> Result<Engine, wasmtime::Error> {
    let mut config = Config::new();

    config
        .strategy(Strategy::Cranelift)
        .cranelift_opt_level(OptLevel::Speed)
        .cranelift_nan_canonicalization(true)
        .collector(Collector::Null)
        .profiler(ProfilingStrategy::None)
        .debug_info(false)
        .native_unwind_info(false)
        .wasm_backtrace(true)
        .wasm_backtrace_details(WasmBacktraceDetails::Disable)
        .generate_address_map(true)
        .consume_fuel(false)
        .epoch_interruption(false)
        .max_wasm_stack(512 * 1024)
        .allocation_strategy(InstanceAllocationStrategy::OnDemand)
        .wasm_reference_types(true)
        .wasm_function_references(false)
        .wasm_gc(false)
        .wasm_simd(true)
        .wasm_relaxed_simd(false)
        .relaxed_simd_deterministic(true)
        .wasm_bulk_memory(true)
        .wasm_multi_value(true)
        .wasm_multi_memory(false)
        .wasm_memory64(false)
        .wasm_extended_const(false)
        .wasm_tail_call(false)
        .wasm_custom_page_sizes(false)
        .wasm_shared_everything_threads(false)
        .wasm_wide_arithmetic(false)
        .wasm_stack_switching(false)
        .wasm_exceptions(false);

    Engine::new(&config)
}

#[cfg(test)]
mod tests {
    use super::{
        consensus_engine, store_limits, ENGINE_CONFIG_ID, STORE_MEMORY_BYTES, STORE_TABLE_ELEMENTS,
        WASMTIME_VERSION,
    };
    use wasmtime::ResourceLimiter;

    fn refuses(wat: &str) {
        let engine = consensus_engine().expect("the checked-in engine configuration");
        let wasm = wat::parse_str(wat).expect("valid WebAssembly text");
        wasmtime::Module::new(&engine, wasm).expect_err("the disabled proposal was accepted");
    }

    #[test]
    fn the_consensus_engine_loads_the_standard_library() {
        let engine = consensus_engine().expect("the checked-in engine configuration");
        let standard = include_bytes!(concat!(env!("OUT_DIR"), "/standard.wasm"));
        wasmtime::Module::new(&engine, standard).expect("the standard library loads");
    }

    #[test]
    fn unused_wasm_proposals_are_rejected() {
        refuses("(module (memory 1) (memory 1))");
        refuses("(module (memory i64 1))");
        refuses("(module (func $f) (func return_call $f))");
        refuses("(module (memory 1 1 shared))");
    }

    #[test]
    fn the_configuration_identity_names_the_runtime_and_disabled_surface() {
        assert!(ENGINE_CONFIG_ID.contains(WASMTIME_VERSION));
        for required in [
            "compiler=cranelift",
            "collector=null",
            "fuel=false",
            "component-model",
            "memory64",
            "threads",
            "wasi",
        ] {
            assert!(
                ENGINE_CONFIG_ID.contains(required),
                "engine identity omits {required}"
            );
        }
    }

    #[test]
    fn stores_admit_the_generated_shape_and_refuse_every_excess() {
        let engine = consensus_engine().expect("the checked-in engine configuration");
        let module = |wat: &str| {
            let wasm = wat::parse_str(wat).expect("valid WebAssembly text");
            wasmtime::Module::new(&engine, wasm).expect("an enabled core module")
        };
        let store = || {
            let mut store = wasmtime::Store::new(&engine, store_limits());
            store.limiter(|limits| limits);
            store
        };

        let mut admitted = store();
        wasmtime::Instance::new(
            &mut admitted,
            &module("(module (memory 1) (table 20 funcref))"),
            &[],
        )
        .expect("one generated memory and table are admitted");

        let mut memory = store();
        wasmtime::Instance::new(&mut memory, &module("(module (memory 32769))"), &[])
            .expect_err("memory beyond the signed Wasm address boundary was admitted");

        let mut table = store();
        wasmtime::Instance::new(&mut table, &module("(module (table 21 funcref))"), &[])
            .expect_err("a table larger than the standard table was admitted");

        let empty = module("(module)");
        let mut instances = store();
        wasmtime::Instance::new(&mut instances, &empty, &[]).expect("the first instance");
        wasmtime::Instance::new(&mut instances, &empty, &[])
            .expect_err("a second instance was admitted to one execution store");

        let limits = store_limits();
        assert_eq!(limits.memories(), 1);
        assert_eq!(limits.tables(), 1);
        assert_eq!(limits.instances(), 1);
        assert_eq!(STORE_MEMORY_BYTES, 2_147_483_648);
        assert_eq!(STORE_TABLE_ELEMENTS, 20);
    }
}
