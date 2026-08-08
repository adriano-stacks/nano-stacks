//! Cranelift's output for a Clarity contract, kept between runs.
//!
//! Turning a contract's wasm into native code costs milliseconds to seconds and
//! saturates every core while it happens; running the call that needed it costs
//! microseconds. A replaying node therefore spends most of its CPU compiling,
//! and spends it again from scratch on every restart. This keeps what wasmtime
//! produced in the node's state directory, so a contract is compiled once ever.
//!
//! One file per entry, beside `marf.sqlite`, rather than a table inside it:
//! `Module::deserialize_file` maps the file instead of reading it, which a
//! `SQLite` blob cannot offer; entries are multi-megabyte and would otherwise
//! churn the write-ahead log of a database whose contents are consensus state;
//! writing by rename makes concurrent writers and a killed process harmless
//! without a lock; and the cache can be deleted, copied or measured on its own.

use std::{
    ffi::OsString,
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use clar2wasm::NativeModuleStore;
use sha2::{Digest, Sha512_256};
use wasmtime::{Engine, Module};

/// What an entry holds and how it is named.
///
/// Bump it when either changes: entries written under an older number become
/// unreachable — a fresh miss, never a wrong hit — and can be deleted by
/// removing their directory.
const FORMAT: u32 = 1;

/// Where under the state directory the entries live.
const DIRECTORY: &str = "native-modules";

/// How a cache has been used, for tests and for reporting.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheStats {
    /// Modules answered from disk, which is native code not compiled again.
    pub hits: u64,
    /// Lookups with nothing usable on disk, which the caller compiled instead.
    pub misses: u64,
    /// Modules written for a later run.
    pub stored: u64,
}

/// Native modules a node keeps in its state directory.
#[derive(Debug)]
pub struct NativeModuleCache {
    directory: PathBuf,
    hits: AtomicU64,
    misses: AtomicU64,
    stored: AtomicU64,
}

impl NativeModuleCache {
    /// Open the cache held in `state_directory`, creating it if absent.
    pub fn open(state_directory: &Path) -> std::io::Result<Self> {
        Self::with_format(state_directory, FORMAT)
    }

    fn with_format(state_directory: &Path, format: u32) -> std::io::Result<Self> {
        let directory = state_directory.join(DIRECTORY).join(format.to_string());
        fs::create_dir_all(&directory)?;
        Ok(Self {
            directory,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            stored: AtomicU64::new(0),
        })
    }

    #[must_use]
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            stored: self.stored.load(Ordering::Relaxed),
        }
    }

    /// Where the native code for these wasm bytes, built by this engine, is kept.
    ///
    /// Two things are hashed. First the wasm — the module wasmtime compiles —
    /// which is what `clar2wasm` emitted from a contract's source, at its
    /// Clarity version, under the epoch it was compiled for, by this build of
    /// the compiler: change any of those and different bytes arrive here, under
    /// a different name. Keying on the source instead would leave the compiler
    /// out, to be covered by a tag someone has to remember to bump.
    ///
    /// Then the engine's own compatibility hash, which covers the wasmtime
    /// version and every compiler setting that changes what it emits, the
    /// Cranelift optimisation level among them. wasmtime would refuse such an
    /// entry anyway, but refusing it is only a miss, and a miss under a name
    /// that is already taken is a miss *forever* — `store` does not overwrite.
    /// So the level belongs in the name.
    fn entry(&self, engine: &Engine, wasm: &[u8]) -> PathBuf {
        let mut hasher = DefaultHasher::new();
        engine.precompile_compatibility_hash().hash(&mut hasher);
        let mut digest = Sha512_256::new();
        digest.update(hasher.finish().to_be_bytes());
        digest.update(wasm);
        self.directory
            .join(format!("{:x}.cwasm", digest.finalize()))
    }
}

impl NativeModuleStore for NativeModuleCache {
    fn load(&self, engine: &Engine, wasm: &[u8]) -> Option<Module> {
        let path = self.entry(engine, wasm);
        // SAFETY: `deserialize_file` trusts its bytes, so what matters is where
        // they come from and whether they can change.
        //
        // They come from `store` below, in a directory this node created inside
        // its own state directory, named by the hash of the wasm they were
        // compiled from. Anyone able to write there can already rewrite
        // `marf.sqlite`, so this grants no authority a node did not already
        // give its state directory.
        //
        // Everything that can go wrong arrives here as `Err` and is treated as
        // a miss: no file, a truncated or corrupt one, one written by another
        // version of wasmtime or by an engine configured differently -- the
        // serialized form carries a fingerprint of both and wasmtime checks it.
        //
        // The file is mapped rather than read, so it must not change while the
        // module is alive. It cannot: an entry's name is its content, `store`
        // never modifies a file in place, and replacing one by rename leaves
        // the mapped inode untouched.
        #[allow(unsafe_code)]
        let module = unsafe { Module::deserialize_file(engine, &path) }.ok();
        if module.is_some() {
            self.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        module
    }

    fn store(&self, wasm: &[u8], module: &Module) {
        let path = self.entry(module.engine(), wasm);
        if path.exists() {
            return;
        }
        let Ok(bytes) = module.serialize() else {
            return;
        };
        // A reader maps the file, so it has to appear whole or not at all. The
        // process id in the temporary name keeps two nodes over one state
        // directory from writing the same partial file.
        let mut name = OsString::from(path.file_name().unwrap_or_default());
        name.push(format!(".{}.writing", std::process::id()));
        let writing = self.directory.join(name);
        if fs::write(&writing, &bytes).is_ok() && fs::rename(&writing, &path).is_ok() {
            self.stored.fetch_add(1, Ordering::Relaxed);
        } else {
            // Failing to cache is not failing: leave nothing behind and let the
            // next run compile again.
            drop(fs::remove_file(&writing));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CacheStats, NativeModuleCache, NativeModuleStore};
    use wasmtime::{Engine, Module, Store};

    /// A module with something to compute, so a reload can be shown to compute it.
    fn adder(constant: i32) -> String {
        format!(
            "(module (func (export \"answer\") (param i32) (result i32) \
               local.get 0 i32.const {constant} i32.add))"
        )
    }

    fn answer(engine: &Engine, module: &Module, argument: i32) -> i32 {
        let mut store = Store::new(engine, ());
        let instance = wasmtime::Instance::new(&mut store, module, &[]).expect("instantiate");
        instance
            .get_typed_func::<i32, i32>(&mut store, "answer")
            .expect("the exported function")
            .call(&mut store, argument)
            .expect("call it")
    }

    #[test]
    fn a_reloaded_module_answers_what_a_freshly_compiled_one_does() {
        let directory = tempfile::tempdir().expect("a directory");
        let cache = NativeModuleCache::open(directory.path()).expect("open the cache");
        let engine = Engine::default();
        let wasm = wat::parse_str(adder(41)).expect("assemble the module");

        let fresh = Module::new(&engine, &wasm).expect("compile");
        cache.store(&wasm, &fresh);
        let reloaded = cache.load(&engine, &wasm).expect("a hit");

        assert_eq!(answer(&engine, &fresh, 1), answer(&engine, &reloaded, 1));
        assert_eq!(answer(&engine, &reloaded, 1), 42);
        assert_eq!(
            cache.stats(),
            CacheStats {
                hits: 1,
                misses: 0,
                stored: 1
            }
        );
    }

    /// A contract whose source changed compiles to different wasm, which is a
    /// different entry. Serving the old native code for it would be a
    /// consensus bug, so this is the test that says it cannot happen.
    #[test]
    fn changed_wasm_is_a_miss() {
        let directory = tempfile::tempdir().expect("a directory");
        let cache = NativeModuleCache::open(directory.path()).expect("open the cache");
        let engine = Engine::default();
        let before = wat::parse_str(adder(41)).expect("assemble the module");
        let after = wat::parse_str(adder(99)).expect("assemble the changed module");

        cache.store(&before, &Module::new(&engine, &before).expect("compile"));

        assert!(cache.load(&engine, &after).is_none());
        // And once both are stored, each answers for itself.
        cache.store(&after, &Module::new(&engine, &after).expect("compile"));
        let reloaded = cache.load(&engine, &after).expect("a hit");
        assert_eq!(answer(&engine, &reloaded, 1), 100);
    }

    #[test]
    fn a_bumped_format_is_a_miss() {
        let directory = tempfile::tempdir().expect("a directory");
        let engine = Engine::default();
        let wasm = wat::parse_str(adder(41)).expect("assemble the module");
        let before = NativeModuleCache::with_format(directory.path(), 1).expect("open the cache");
        before.store(&wasm, &Module::new(&engine, &wasm).expect("compile"));
        assert!(before.load(&engine, &wasm).is_some());

        let after = NativeModuleCache::with_format(directory.path(), 2).expect("open the cache");
        assert!(after.load(&engine, &wasm).is_none());
    }

    /// Native code built at another optimisation level has to be a miss under
    /// its own name, not under the name the other level already took: `store`
    /// never overwrites, so a shared name would be a miss that never heals.
    #[test]
    fn another_engine_configuration_is_a_miss_of_its_own() {
        let directory = tempfile::tempdir().expect("a directory");
        let cache = NativeModuleCache::open(directory.path()).expect("open the cache");
        let wasm = wat::parse_str(adder(41)).expect("assemble the module");
        let engine_of = |level| {
            let mut config = wasmtime::Config::new();
            config.cranelift_opt_level(level);
            Engine::new(&config).expect("a configured engine")
        };
        let optimised = engine_of(wasmtime::OptLevel::Speed);
        let plain = engine_of(wasmtime::OptLevel::None);

        cache.store(&wasm, &Module::new(&optimised, &wasm).expect("compile"));
        assert!(cache.load(&plain, &wasm).is_none());

        cache.store(&wasm, &Module::new(&plain, &wasm).expect("compile"));
        let reloaded = cache.load(&plain, &wasm).expect("a hit");
        assert_eq!(answer(&plain, &reloaded, 1), 42);
        assert!(cache.load(&optimised, &wasm).is_some());
    }

    #[test]
    fn a_corrupt_entry_is_a_miss() {
        let directory = tempfile::tempdir().expect("a directory");
        let cache = NativeModuleCache::open(directory.path()).expect("open the cache");
        let engine = Engine::default();
        let wasm = wat::parse_str(adder(41)).expect("assemble the module");
        cache.store(&wasm, &Module::new(&engine, &wasm).expect("compile"));
        let entry = cache.entry(&engine, &wasm);
        let whole = std::fs::read(&entry).expect("read the entry");

        for damaged in [
            whole[..whole.len() / 2].to_vec(),
            vec![],
            b"not a module at all".to_vec(),
        ] {
            std::fs::write(&entry, &damaged).expect("damage the entry");
            assert!(cache.load(&engine, &wasm).is_none());
        }
    }
}
