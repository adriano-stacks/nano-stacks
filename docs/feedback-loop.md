# Developer feedback-loop latency

Measured 2026-08-05 on the dev box: 16 cores, 31 GB RAM, cargo/rustc 1.97.0 (nix),
Linux 6.18, `/home` on `/dev/vda` (2.0 T, 76 % full).

## How to read the numbers

Every timing below is a median of 2–3 runs. Two caveats matter:

- **Machine load.** Another agent was building in the same workspace during part of the
  session. Under load (`loadavg` 13–16) the same command measured 3–4× slower than on a
  quiet machine (`cargo build --release -p nano-vm` after a touch: 19.2 s loaded, 3.9 s
  quiet). Quiet-machine medians are reported; loaded outliers are noted where they were
  large.
- **`touch` is not always a one-line change.** Contents were never modified, only mtimes.
  `release` has `incremental = false`, so a touch forces a full recompile of that crate —
  identical to a real one-line change. `dev` and `cargo check` have `incremental = true`,
  where rustc's dep-graph is content-keyed, so a touch is the *best case* (comment-only
  edit). For those rows the cold-crate cost is given as the upper bound.

## Measured latencies

### Release, single crate — `touch X && cargo build --release -p X`

| crate | wall | note |
|---|---|---|
| `nano-vm` (4 468 LOC) | **3.9 s** | full recompile every time; incremental is off in release |
| `nano-chainstate` (4 644 LOC) | **5.3 s** | |
| `clar2wasm` (50 041 LOC) | **21.9 s** | largest single unit in the graph |
| `nano-conformance` `--all-targets` | **22.4 s** | lib + 28 test bins + 2 examples |

### Release, node binary — `cargo build --release --bin stacks-node`

| change in | wall |
|---|---|
| `nano-node/src/main.rs` (bin only) | **11.5 s** ← hard floor for any node rebuild |
| `nano-chainstate/src/lib.rs` | **22.7 s** |
| `nano-vm/src/lib.rs` | **23.0 s** |

### Release, whole test graph — `cargo build --release --workspace --all-targets`

| change in | wall |
|---|---|
| nothing (no-op) | **0.6 s** |
| `nano-conformance/src/lib.rs` | **22.4 s** |
| `nano-chainstate/src/lib.rs` | **39.9 s** |
| `nano-vm/src/lib.rs` | **43.8 s** |
| `vendor/clarity-wasm/clar2wasm/src/lib.rs` | **108.9 s** (142 s under load) |

### Test execution — `cargo test --release --workspace`, build already warm

**72–86 s** wall, 62 test binaries, all passing. Cargo runs the binaries one at a time;
the sequential sum measured directly is 83.2 s. Slowest ten:

| binary | crate | wall |
|---|---|---|
| `clar2wasm` (lib unit tests) | clar2wasm | **22.3 s** |
| `nano_sync` (lib unit tests) | nano-sync | **15.0 s** |
| `release_dependencies` | nano-conformance | **14.2 s** (90.8 s under load) |
| `wasm_generation` | clar2wasm | **11.9 s** |
| `event_observer` | nano-conformance | 4.9 s |
| `boot_contracts_tests` | clar2wasm | 3.0 s |
| `nano_conformance` (lib) | nano-conformance | 3.0 s |
| `lib` | clar2wasm | 2.7 s |
| `lib_tests` | clar2wasm | 2.5 s |
| `rejected_blocks` | nano-conformance | 0.6 s |

The remaining **48 binaries take 1.7 s combined.** Four binaries are 78 % of the run.

`release_dependencies` shells out to `cargo tree` once per production crate (16 of them);
that is the whole of its 14 s, and it is why it degraded to 90 s under load.

### End-to-end loop: edit → tests green

| edited | rebuild | + test run | total |
|---|---|---|---|
| `nano-conformance` | 22.4 s | 80 s | **~102 s** |
| `nano-vm` | 43.8 s | 80 s | **~124 s** |
| `nano-chainstate` | 39.9 s | 80 s | **~120 s** |
| `clar2wasm` | 108.9 s | 80 s | **~189 s** |

### Cheap loops that already exist

| command | no-op | after touching `nano-vm` | cold |
|---|---|---|---|
| `cargo check --workspace --all-targets` | 0.4 s | **2.5 s** | 8.3 s |
| `cargo clippy --workspace --all-targets` | — | **6.0–8.8 s** | 12.8 s |
| `cargo check -p nano-vm` | 0.3 s | 0.5–1.4 s | 15.7 s |

`cargo clippy --workspace --all-targets` catches every type, borrow and lint error in 6–9 s.
That is 5–7× faster than the release build it is presumably being used in place of.

### Debug (non-release) profile

It works, and it is **not** faster to compile.

| command | wall |
|---|---|
| `cargo build -p nano-vm` — cold crate cache | **43.7 s** (vs **3.9 s** in release: 11× slower) |
| `cargo build -p nano-vm` — touch, warm incremental cache | 0.5–4.2 s |
| `cargo test -p nano-chainstate` — cold `nano-vm` + `nano-chainstate` | **55.2 s** |
| `cargo test -p nano-chainstate` — touch, warm incremental | 5.4–18.8 s |

The dev profile optimizes all ~560 dependencies at `opt-level = 3` and keeps full
`debuginfo = 2`, so a first-time debug compile of a crate costs 11× its release compile and
produces ~660 MB test binaries. Once the incremental cache is warm, small edits are cheap —
but the cache has to be paid for first, and it is 126 GB on disk.

## Profile settings

`/home/aldur/nano-stacks/Cargo.toml` sets exactly two things:

```toml
[profile.dev]
opt-level = 1

[profile.dev.package."*"]
opt-level = 3
```

Everything else is at Cargo's default. **There is no `[profile.release]` section at all.**

| setting | `release` (effective) | `dev` (effective) | set explicitly? |
|---|---|---|---|
| `opt-level` | 3 | 1, deps 3 | dev only |
| `debug` | `false` (0) | `true` (2, full) | no |
| `split-debuginfo` | platform default (packed/off) | same | no |
| `incremental` | **`false`** | `true` | no |
| `codegen-units` | 16 | 256 | no |
| `lto` | `false` → **thin-local LTO across the 16 CGUs** | `false` | no |
| `panic` | `unwind` | `unwind` | no |
| `strip` | `none` | `none` | no |
| `debug-assertions` / `overflow-checks` | off | on | no |

No `[profile.test]`, `[profile.bench]`, or custom `inherits` profile.
No `[profile.release.package."*"]` split.

`.cargo/config.toml` contains only `xtask = "run -p xtask --"`. No `build.rustflags`,
no `build.jobs`, no `target.<triple>.linker`, no `RUSTFLAGS` in the environment,
no `CARGO_TARGET_DIR`, no `CARGO_INCREMENTAL`.

## Caching and disk

- **`target/` is 461 GB.** `target/debug` ~450 GB of it (`deps` 320 GB, `incremental`
  126 GB across 5 247 session directories, `examples` 24 GB); `target/release` is 8.1 GB.
- **195 GB of `target/debug/deps` has not been touched in 3 days.** Cargo never garbage
  collects: `target/debug/deps` holds 93 distinct fingerprint copies of `xtask`, 83 of
  `nano_conformance` (~660 MB each), 48 of `nano_chainstate`, 47 of `stacks_node`.
- **`vendor/clarity-wasm/target` is a second, separate 15 GB target dir** — the same
  dependency graph built twice, because that directory was built as its own workspace root
  at some point.
- **`sccache` is not installed and not configured.** No shared or global target directory;
  each of `target/` and `vendor/clarity-wasm/target/` is local.
- **No linker override.** Only `ld.gold` is on `PATH` alongside the default `cc`/GNU `ld`;
  `mold` and `lld` are absent.
- `/home` has 494 GB free at 76 % used. `target/` is within one full debug test build of
  becoming the actual blocker.

## What dominates

It is **LLVM optimization at `opt-level = 3` with local ThinLTO, re-run across ~60
downstream compilation units on every edit** — and 45 % of those units are test and example
binaries.

`cargo build --timings` for a one-line change in `nano-vm`, rebuilding
`--workspace --all-targets` (43 s wall, **352.8 s CPU**, 60 units):

| bucket | units | CPU | share |
|---|---|---|---|
| `nano-conformance` integration test binaries | 28 | **139.3 s** | 39 % |
| `lib (test)` unit-test targets | 10 | 91.5 s | 26 % |
| rlibs — the only thing a node binary needs | 10 | 49.6 s | 14 % |
| bins (`stacks-node` 15.4 s, `xtask` 14.8 s, 2 miner bins 13.4 s) | 4 | 43.6 s | 12 % |
| examples (`replay-divergence` 12.0 s, `captured-leaves` 5.7 s, `pox-read` 3.5 s) | 3 | 21.2 s | 6 % |

The edited crate itself is 11.8 s of the 352.8 s — **3 %**. The other 97 % is downstream
re-codegen. The single largest structural contributor is nano-conformance's 28 separate
`tests/*.rs` files: each is its own crate, each re-monomorphizes and re-optimizes the whole
`nano-*` + `clarity` + `wasmtime` generic surface into its own ~30 MB executable, at a mean
of 5.0 s and a max of 10.9 s apiece, for a combined 4.6 minutes of CPU per rebuild.

**That first row is history: those 28 targets are one target now.** See "The 28 conformance
targets are one" at the end of this document for the before/after measurement.

Inside a single unit (`stacks-node` bin, `rustc -Ztime-passes`, 12.1 s total):

| pass | time | share |
|---|---|---|
| `finish_ongoing_codegen` | 7.0 s | 58 % |
| — of which `LLVM_thinlto` | 4.5 s | 37 % |
| — of which `LLVM_passes` | 3.3 s | 27 % |
| `codegen_crate` (MIR → LLVM IR) | 1.8 s | 15 % |
| `run_linker` | 1.9 s | 16 % |
| front end (parse, type-check, borrow-check, mono collection) | < 1.0 s | 8 % |

What is **not** the problem:

- **Not the stacks-core git dependencies.** Zero dependency crates rebuilt in any measured
  edit loop. They are pinned to one rev, built once, and stay fresh. `blockstack_lib`
  (80 MB rlib) is a dev-dependency of `nano-conformance` only and never recompiles.
  Changing the pinned rev is a one-off cost, not a loop cost.
- **Not clar2wasm/wasmtime codegen in the loop** — unless you are editing clar2wasm, in
  which case it is 22 s for the crate and 109 s for everything downstream. `wasmtime`,
  `cranelift` and `walrus` themselves never rebuild.
- **Not linking.** 1.9 s of a 12.1 s unit (16 %), and less elsewhere. A faster linker
  cannot buy more than ~1.5 s.
- **Not test execution.** 72–86 s, of which four binaries are 78 % and 48 binaries are
  1.7 s combined.

## Ranked changes

Ordered by (saving ÷ risk). Items 6 and the noted parts of 2 and 7 **change generated code
or runtime behaviour** and are called out explicitly.

### 1. Collapse nano-conformance's 28 integration test targets into one — **done**

Replace `crates/nano-conformance/tests/*.rs` (28 separate targets) with a single
`tests/conformance/main.rs` that does `mod marf_lockstep; mod mainnet_codec; …`.

Landed; measured results at the end of this document. The estimate below was roughly right
about the build and wrong about the test run.

- **Saving: ~124 s of 353 s CPU per rebuild (35 %), ~10–15 s of the 43 s wall.** 28
  codegen+link units become 1. Also cuts test execution: 28 process launches and 28 libtest
  pools become one 16-thread pool over all tests.
- **Risk: low.** Tests lose per-process isolation — one `abort`, OOM or stack overflow takes
  the whole binary down, so audit `oom_checker` and anything that manipulates
  process-global state (env vars, working directory, signal handlers). Module-level name
  collisions need renaming. `#[should_panic]` and `CARGO_BIN_EXE_*` are unaffected.
- **No effect on node runtime.**

### 2. Turn on incremental compilation for the loop

Measured steady state with `-Cincremental`, content-identical rebuild (the floor):

| unit | default | incremental |
|---|---|---|
| `stacks-node` bin | 12.6 s | **2.8 s** (−78 %) |
| `nano-vm` lib | 3.9 s | **0.5 s** (−87 %) |

A real one-line change lands between those; expect 40–70 % off the changed-crate portion.

Two ways to get it, with different risk:

- **(a) Separate profile — recommended.** Add
  `[profile.loop] inherits = "release"` with `incremental = true`, and use
  `cargo test --profile loop` / `cargo build --profile loop` for the write/test loop while
  `--release` stays byte-identical for the node and for any mainnet-replay timing run.
  **No runtime risk to the shipped node.** Costs one full cold build into a new profile
  directory (~20–40 min once) and ~10–15 GB more disk, and the two profiles' caches do not
  share artifacts.
- **(b) `[profile.release] incremental = true`.** Cheaper to adopt, but **this can change
  the node's runtime performance.** Incremental fixes CGU partitioning per module, which can
  reduce cross-CGU inlining. Measured mitigations: local ThinLTO still runs under
  incremental (3.2 s pass present), and the binary is within 0.2 % of the same size
  (37 214 752 vs 37 303 296 bytes) — so the risk looks small, but it is not zero. **Do not
  adopt (b) without benchmarking mainnet replay throughput before and after.** Also: the
  first build after enabling costs ~5× (61 s for the node bin alone) and incremental caches
  run 43–70 MB *per unit*, i.e. tens of GB across the workspace.

### 3. Stop building what the loop does not need

- `--tests` instead of `--all-targets`: drops the 3 examples, **21.2 s CPU (6 %)**.
- `--exclude xtask`: drops `xtask`'s bin and lib-test, **15.8 s CPU (4 %)**.
- Make `cargo clippy --workspace --all-targets` (**6–9 s** after an edit) the default inner
  loop. It already works, costs a twentieth of the release rebuild, and finds every error
  that is not a test failure.
- **Saving: ~37 s CPU per rebuild, and 43 s → 6–9 s for the majority of iterations that end
  in a compile error rather than a test failure. Zero risk, no runtime effect.**

### 4. Fix the two slow tests

- `release_dependencies` spawns 16+ `cargo tree` subprocesses: **14 s clean, 90 s under
  load** — 17 % to 50 % of the test run on its own. Run `cargo tree` once (or
  `cargo metadata`) and parse the graph in-process, or gate it behind a CI-only feature.
- `nano_sync`'s lib tests take **15 s** with 6 ignored — almost certainly wall-clock sleeps
  or retry timeouts. Inject the clock or shorten the intervals.
- **Saving: ~29 s of the 72–86 s test run. Zero risk, no runtime effect.**

### 5. Adopt `cargo nextest`

Sequential sum is 83 s with a 22 s longest binary; nextest runs tests in parallel across
processes, so wall time approaches the longest single test.

- **Saving: ~50 s per test run (80 s → ~25–30 s).**
- **Risk: moderate.** Each test gets its own process, so anything relying on shared
  in-process state, or on being the only writer to a fixture or temp directory, breaks and
  must be found. Partly overlaps with #1 — do #1 first, then re-measure.
- **No runtime effect.**

### 6. Compile-speed profile knobs — measured, and all of them cost runtime speed

Measured directly on the `stacks-node` bin unit (12.6 s baseline):

| knob | wall | saving |
|---|---|---|
| `lto = "off"` | 8.7 s | **−31 %** |
| `opt-level = 2` | 10.7 s | −15 % |
| `codegen-units = 64` | 10.9 s | −14 % |
| `lto = "off"` + `codegen-units = 64` | 8.7 s | −31 % |

`lto = "off"` is the biggest single knob and extrapolates to roughly −25 to −30 % on the
whole 43 s rebuild, because it removes the 4.5 s ThinLTO pass that runs in *every* unit.

**All three of these directly reduce optimization quality and will slow the node at
runtime.** `lto = false` (today's default) is not "no LTO" — it is thin-local LTO across the
16 codegen units, and `lto = "off"` is what actually disables it. For a node whose entire
purpose is mainnet replay throughput this is the wrong trade in `[profile.release]`.
**Only put these in the separate `[profile.loop]` from #2(a), never in `release`.**

Two knobs here are *free*: `[profile.release] debug = false` and `strip = "symbols"` are
already the effective defaults for `debug`; `strip` would only shrink the 37 MB binary, not
speed anything up. There is nothing to gain from setting them.

### 7. Dev-profile hygiene

The dev profile is why `target/` is 461 GB and why a first-time debug compile of `nano-vm`
costs 43.7 s against 3.9 s in release.

- `[profile.dev] debug = "line-tables-only"` and `split-debuginfo = "unpacked"` — cuts debug
  artifact size several-fold (debug conformance test binaries are ~660 MB each) and speeds
  up debug links. Backtraces keep file/line; you lose variable inspection in a debugger.
- **Keep `[profile.dev.package."*"] opt-level = 3`.** Dropping it would make debug test
  *execution* glacial — `wasmtime`, `sha2` and `secp256k1` are all on the hot path. That is
  a **runtime** change for any debug replay, and a bad one.
- **Saving: disk, and debug link time. No effect on `--release` runtime.**
- **Caveat:** changing any dev debuginfo setting invalidates the entire 450 GB debug cache
  once.

### 8. Reclaim disk and stop building clar2wasm twice

- `cargo clean` on the debug profile frees ~450 GB; 195 GB of it is over 3 days old and 93
  stale fingerprint copies of `xtask` alone are sitting in `deps/`. Add `cargo-sweep`
  (`cargo sweep --time 7`) to a periodic job — Cargo never garbage collects.
- Point `vendor/clarity-wasm` at the root `target/` (or delete its own 15 GB one) so
  `clar2wasm` and its dependency closure are not built twice.
- **Saving: ~465 GB, and it removes the near-term risk of the disk filling. No effect on
  build latency itself, no runtime effect.**

### 9. Not worth doing

- **`sccache`.** Not installed, and it would not help: no dependency crate rebuilt in any
  measured loop, so there is nothing for it to cache. It is also mutually exclusive with
  incremental compilation, which is #2.
- **A faster linker (`mold`/`lld`).** `run_linker` is 1.9 s of a 12.1 s unit. Worth adding
  because it is nearly free (`mold` via nix, `-C link-arg=-fuse-ld=mold`), but it buys at
  most ~1.5 s and is not the bottleneck.
- **A shared/global target dir.** Only one checkout exists; nothing to share.

## One footgun observed

During the session `cargo build --release --workspace` and `cargo check --workspace`
(without `--all-targets`) both failed: `nano-oracle` calls `clarity::vm::ast::parse`, which
is gated behind `clarity/testing`, and that feature is only unified into the graph when
dev-dependencies are present. It compiles again as of commit `3e60ca9a`. Worth a regression
guard: while it is broken, the cheap `cargo check`/`cargo build` loops are unavailable and
everyone falls back to the 20–40× more expensive `--all-targets` path.


## What was acted on, 2026-08-05

Three of these landed the same day the measurements did, and one thing not on the
list turned out to matter more than any of them.

**The startup walk, which no build measurement would have found.** A node resuming
a mainnet state printed nothing for over six minutes before executing a block, so
every iteration of a divergence hunt paid it. `open_chainstate` walked `parent_of`
from the tip to the root of the MARF — a checkpoint import brings the whole
ancestry, so 8.6 million SQLite lookups against a 23 GB database, building a
277 MB list — to use the first entry unless a peer had lost our tip. Bounded to
256. Six minutes to 20 seconds. `Phase` in `runtime.rs` now prints any startup
phase over half a second, because three guesses were made about this before
anything was measured and all three were wrong.

**`target/debug` was 452 GB.** Nothing in this workspace builds debug on purpose;
it accumulated from `cargo check` and from `cargo test` without `--release`.
Deleted, along with `vendor/clarity-wasm/target`, a second 15 GB build of the same
dependency graph — `cargo test -p clar2wasm` works from the workspace root, so
that directory never needed to exist. `/home` went from 76 % to 53 % full, which
matters because a mainnet state snapshot is now a routine thing to take.

**`release_dependencies` no longer builds a second graph.** It was
`cargo check --all-targets`, which is a debug-profile check and was itself half of
why `target/debug` grew; it is `cargo check --release --tests` now. 16 s warm.

**`[profile.loop]` is defined and not adopted.** `inherits = "release"` with
`incremental = true`, per recommendation 2 — but iterating under `--profile loop`
rather than putting `incremental = true` into `[profile.release]`, because that
changes codegen-unit partitioning and this node's replay throughput is worth
measuring before trading. Costs one cold build, which has not been paid yet.

## The 28 conformance targets are one

Done, later the same day. All 28 files moved to `crates/nano-conformance/tests/conformance/`
and are declared as modules of one `tests/conformance/main.rs`, so the workspace links once
instead of 28 times. A test's name is now `<module>::<fn>`, and
`cargo test -p nano-conformance <module>` runs what `--test <module>` used to.

**Nothing had to stay a separate target.** `oom_checker`, the test the note above worried
about, is a clar2wasm test (`vendor/clarity-wasm/clar2wasm/tests/oom-checker/`) and was never
one of these 28. The audit of all 28 for process-global state found nothing that has to have
a process to itself: no `set_var`/`remove_var`, no `static`/`OnceLock` mutable state, no
working-directory change, no fixed port (the one test that serves HTTP binds `127.0.0.1:0`),
no global hook or panic handler. One fixed temp path, in `release_dependencies`, became a
`tempfile::tempdir()` first, in its own commit. The two tests that shell out to `cargo` still
do; nested cargo takes the target-directory lock, which it already did when they ran
concurrently inside their own binary.

### Measured, same box, same session

The machine was **not** quiet — other agents were building throughout, `loadavg` 15–52 — so
wall time is mostly noise and CPU time (user + sys, children included) is the number to
trust. It has a 3 % spread across runs where wall has 60 %.

`touch crates/nano-vm/src/lib.rs && cargo build --release --tests -p nano-conformance`,
median of 6 runs each side:

| | CPU | wall | link+write (`sys`) |
|---|---|---|---|
| before, 28 targets | **303 s** | 43 s | 19 s |
| after, 1 target | **202 s** | 27 s | 5 s |

**−101 s CPU, −33 %.** The `sys` drop is 27 fewer 36 MB executables written per rebuild.

`touch crates/nano-vm/src/lib.rs && cargo build --release --workspace --all-targets`,
median of 3 each side:

| | CPU | wall (at loadavg) |
|---|---|---|
| before | **596 s** | 92 s (41–52) |
| after | **480 s** | 51 s (26–34) |

**−116 s CPU, −19 %** of the whole test graph. These absolute numbers are much larger than
the 352.8 s in the table above because that measurement predates `nano-mempool`,
`nano-bitcoin` and `nano-oracle`; only the before/after pair is comparable.

### Test execution did not improve

Recommendation 1 predicted the merge would also cut the test run, on the theory that 28
process launches and 28 libtest pools cost something. They do not.
`cargo test --release -p nano-conformance`, build warm, three runs each side:

| | run 1 | run 2 | run 3 |
|---|---|---|---|
| before | 34 s | 73 s | 79 s |
| after | 32 s | 76 s | 80 s |

Identical within noise, and the same shape on both sides — the first run after a build is
fast and later ones are not, because `release_dependencies` shells out to
`cargo check --release --tests` over the 16 production crates and that dominates whatever
else is running. Process launch was never the cost. If the test run is the thing to fix, it
is that test (recommendation 4), not the target count.

### Evidence that nothing was dropped

`cargo test --release -p nano-conformance -- --list` before and after, compared as
`<file-stem>::<fn>` so the new module prefix does not read as a difference: **163 tests both
times, diff empty.** Per-test verdicts also diff-empty, both ungated (162 ok, 1 ignored) and
with `NANO_REQUIRE_MAINNET=1` and the mainnet capture present (156 ok, 6 failed, 1 ignored —
the same six gates failing on the same `skip_gate` panic for the same missing environment
variable). `skip_gate` still panics rather than reporting green, which is the whole reason it
exists.
