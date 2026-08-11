---
id: "115"
group: mainnet
title: "Make clarity-wasm outrun the interpreter on mainnet traffic"
status: completed
priority: high
effort: large
dependencies: ["114"]
tags: ["mainnet", "performance", "vm", "clarity-wasm"]
created_at: 2026-08-11
completed_at: 2026-08-11
type: feature
---

# Make clarity-wasm outrun the interpreter on mainnet traffic

## Objective

Task 114 measured the production engine against the reference interpreter on
2,564 real mainnet contract calls, identical parent state, warm caches, zero
disagreements — and the folklore inverted: **clarity-wasm is ~1.9× slower in
aggregate** (63.4 s vs 34.1 s of pure engine time; per-call median 0.54×, p90
0.85×). The engine we compile to native code loses on more than 90% of the
calls mainnet actually sends, and wins only where a call loops over arithmetic
between few host calls (DLMM multi-bin liquidity 2.84×, sbtc-withdrawal
~1.9×). Execution speed bounds catch-up time and tip-hold latency, and wasm is
the only engine the node has: this deficit is the node's execution ceiling.

Close it. The north star is aggregate parity or better (≥1.0×) on the task-114
corpus, with the worst class (pox-5 `stake`, 0.16×) brought above 0.5× —
and every consensus gate untouched.

## What task 114 measured, so this task doesn't re-derive it

The deficit is two separable components, not one:

1. **A ~2–3× multiplier on host-boundary work**, present in *every* charged
   runtime bucket (0.30–0.61× from <10k to >10M runtime units). Extreme case:
   pox-5 `stake` at 15.9 ms/call vs the interpreter's 2.5 ms — a call that is
   almost entirely map reads/writes and event construction, i.e. every
   operation crosses wasm linear memory with value (de)serialization and
   host-side work per crossing.
2. **A ~1 ms fixed per-call cost**: over the 100 cheapest calls, wasm median
   1.57 ms vs interpreter 0.57 ms. Most mainnet calls are cheap, so this
   floor moves the aggregate more than any single hot contract.

**Ruled out: codegen.** The in-memory `ModuleCache` is keyed by contract id
(`clar2wasm/src/lib.rs:158`) and the bench's repeats hit it; `loto::ri` holds
a stable ~157 ms across repeats. The measured time is execution.

Raw samples: `/home/aldur/bench-engines-samples.tsv` (format in task 114);
aggregate with `scripts/bench-engines-report.py`.

## The levers, in the order the evidence ranks them

### 1. Amortize per-call setup (the ~1 ms floor)

Today every contract call rebuilds the wasmtime `Linker` — 223 host-function
registrations — and instantiates the module from scratch (task 110 named this
and parked it as "blocked on `ClarityWasmContext`'s lifetimes"). Unblock it:

- The `Linker` closures capture nothing per-call except through
  `Caller<'_, ClarityWasmContext>`; what actually varies per call is the
  `Store` data. A `Linker` built **once per engine** (or per epoch/version
  pair) and reused across calls is the design wasmtime intends —
  `Linker::instantiate_pre` then gives an `InstancePre` that amortizes
  import resolution too. The blocker is that `ClarityWasmContext` today
  borrows the `GlobalContext`/`ContractContext` with per-call lifetimes;
  the fix is owning or handle-based context (e.g. `*mut`-free interior
  handles swapped per call), which is a refactor of the context type, not of
  any semantic.
- Measure what remains: instantiation, memory/data-segment initialization,
  argument copy-in. wasmtime's pooling instance allocator is the next step
  if instantiation itself is the residue.

### 2. Cheapen the host boundary (the multiplier)

Attribution first, optimization second — the multiplier is spread across
`get_variable`/`set_variable`/`map_get`/`map_set`/event emission, and the box
cannot run `perf` (`perf_event_paranoid = 3`, no ptrace), so the tool is an
instrumented build, exactly as task 110's arena hunt did it:

- Wrap the linker's host functions (or the few hot ones) in counters/timers
  behind an env switch; run one pox-5 `stake` and one `loto::ri` call; split
  each call's wall time into: host-function count × dispatch cost, value
  serialization bytes/time, type-signature parsing, MARF/side-store reads,
  event construction.
- Known candidates the attribution should confirm or kill:
  - Clarity `Value` serialization across linear memory on every read/write —
    the interpreter passes `Value`s by reference in-process and pays nothing.
  - Repeated `signature_from_string` parses — `linker.rs` recently gained a
    `parsed_types` cache keyed by data-segment coordinates; verify it covers
    the hot paths (`runtime_value_type`, shape checks) and extend where it
    does not.
  - Per-crossing epoch/version lookups and allocations in `read_identifier_
    from_wasm`/`write_to_wasm`.
- Only then change representation: candidates include passing side-store
  values by handle instead of by bytes for values that round-trip unread,
  and batching event data. Anything that changes *which* bytes execution
  writes is off the table (see guardrails).

### 3. Re-verify, same corpus, same command

```
cp --reflink=always -r /home/aldur/mainnet-pristine/chainstate <fresh-state>
NANO_BENCH_ENGINES=samples.tsv target/release/replay-blocks \
    /home/aldur/mainnet-capture-long <fresh-state> 4149
python3 scripts/bench-engines-report.py samples.tsv
```

~29 min; the replay validates every `state_index_root` while it benches, so a
semantic slip fails the run itself. Record before/after per lever, not only
the end state — the next person needs to know which change bought what.

## Guardrails — what this task must not move

- **No consensus surface changes.** Receipts, costs, events, write sets and
  roots are byte-identical before and after: the 340/340 scoreboard, the
  frozen mainnet receipt digests, and a `NANO_REPLAY_BOTH_ENGINES` crosscheck
  replay all stay green. A "fast path" that skips a write or reorders one is
  a consensus bug wearing a performance hat.
- **Any vendored clarity-wasm change re-stamps `COMPILER_IDENTITY`.** That is
  by design (the checkpoint binds to compiler identity), but it invalidates
  the on-disk native-module cache and must be called out in the change that
  does it; batching identity-moving changes beats trickling them.
- **The wasm-only rule is untouched.** The interpreter remains a dev-only
  oracle; nothing here may route production execution through it, however
  much faster it currently is. `wasm_is_the_engine` and
  `one_engine_in_the_artifact` gate this already.
- **Bench hygiene.** No compiles or heavy jobs on the box during a measured
  run (task 114 threw away two runs over this); alternating repeats and
  medians stay; a result from a loaded box is not a result.

## Attribution, measured 2026-08-11

`clar2wasm::phases` (env `NANO_WASM_PHASES`, thread-local, per-phase nesting
guard) + the bench writing a `.phases` TSV per call; report:
`scripts/bench-phases-report.py`. Blocks 8,665,602–8,666,401, 321 calls,
mean 28.2 ms/call before any fix:

| phase | ms/call | share | note |
|---|---|---|---|
| host_shape | 17.99 | 70% of invoke | 1,462 size/shape measurements per call |
| unattributed wasm+glue | 6.79 | 27% | includes per-measurement list writes |
| module_probe | 3.09 | — | `needed_module` per call (found in round 2) |
| linker+instantiate+call_setup | 2.06 | — | ×15.5 setups/call (nested `contract-call?`s) |
| commit / handle_tx_result | 0.93 | — | per frame |
| contract_load | 0.49 | — | callee context deserialize |
| host_var+map+event+stx (DB) | 0.80 | — | the storage the deficit was blamed on |

The two headline surprises: the **database is innocent** (0.8 ms of 28), and
the top two costs were both *re-parsing text per call*:

1. `runtime_value_size` — the hottest host call in the engine ("two thirds of
   every host call") — re-ran `signature_from_string` on its type text on
   every measurement; the `parsed_types` cache built for exactly this was
   never applied to it (nor to `print`, `save_runtime_shape`,
   `runtime_shape_is_equal`, `with_nft`'s allowance type). A fold measuring
   its accumulator per iteration paid ~90 of `loto::ri`'s ~120 ms here.
2. `ensure_wasm_module_and_references` read the target's **whole source and
   `build_ast`-parsed it** (plus every contract argument's) on every call to
   warm literal-named callees — 6.25 ms of an 11.65 ms xyk swap — although
   the on-demand missing-module retry already covers anything a dispatch
   reaches. Now skipped when the target is already in the module cache.

After both (same window, same method): mean 14.4 ms/call, module_probe
0.002 ms, host_shape 3.25 ms; window aggregate **wasm 1.73× faster than the
interpreter**, per-call median 0.92×, and wasm's charged-runtime-per-wall-ns
(0.04) now above the interpreter's (0.03). Scoreboard, frozen digests and the
`NANO_REPLAY_BOTH_ENGINES` crosscheck replay all green after the change.

## Tasks

- [x] Attribution build: env-gated counters/timers on the host-function
      boundary; publish the split for one pox-5 `stake` and one meme-token
      read call. No lever is implemented before its cost is on this table.
      (Table above; `clar2wasm::phases` + `scripts/bench-phases-report.py`.)
- [x] Lever 2 (promoted to first by the attribution): parse-cache coverage for
      `runtime_value_size` / `print` / `save_runtime_shape` /
      `runtime_shape_is_equal` / `with_nft`, and the per-call reference-walk
      skip in `ensure_wasm_module_and_references`.
- [x] Lever 1a/1b (linker reuse, `InstancePre`): **not taken, by the numbers.**
      The attribution priced one setup at ~70 µs linker + ~50 µs instantiate +
      ~15 µs call setup; with the parse fixes landed, the cheapest-100 floor is
      0.753 ms vs the interpreter's 0.559 ms — inside the acceptance target
      without touching `ClarityWasmContext`'s lifetimes. The refactor (or an
      unsafe lifetime-erased linker template — vendor does not forbid unsafe,
      nano crates do) stays the named next lever if the sub-200k-runtime
      bucket (0.74–0.80×) ever needs closing; the per-frame `Commit`
      (~0.9 ms/call across frames) and `contract_load` (~0.5 ms) are on the
      same list.
- [x] Lever 2: parse-cache coverage + reference-walk skip (see above); the
      marshalling representation itself was left alone — the attribution
      priced value reads/writes at ~1 ms/call combined, not worth new
      representation risk.
- [x] Full re-run of the task-114 bench: table below.
- [x] Gates after the final state: workspace clippy clean, nano-vm (32) /
      nano-chainstate (41) / nano-rpc suites green, clar2wasm's own 1,473 lib
      tests green, scoreboard 340/340 (plain **and** under
      `NANO_REPLAY_BOTH_ENGINES`), frozen digests 500/500,
      `wasm_is_the_engine` and `one_engine_in_the_artifact` green.

## Result — task-114 corpus, 4,149 blocks, 2,564 calls, 2026-08-11

Zero disagreements; every block sealed the network's root; replay wall 24:52.

| | task 114 (before) | after | 
|---|---|---|
| total engine time, wasm | 63.4 s | **23.0 s** |
| total engine time, interpreter | 34.1 s | 35.0 s |
| aggregate wasm speedup | 0.54× | **1.52×** |
| per-call p10 / median / p90 | 0.27 / 0.54 / 0.85× | 0.75 / 0.89 / 1.49× |
| cheapest-100 wasm floor | 1.57 ms | 0.753 ms |
| pox-5 | 0.16× | 0.92× |
| dlmm-swap-router | 1.15× | 1.98× |
| dlmm-liquidity-router | 2.84× | 3.38× |
| xyk-core-v-1-2 | 0.56× | 1.29× |
| blocksurvey | 0.58× | 0.80× |
| meme-token reads (loto) | 0.39× | 1.43× |

By charged-runtime bucket the crossover now sits at ~200k units: below it
0.74–0.80× (the floor: setups, contract load, per-frame commit), above it
1.31–1.46×. wasm now delivers 0.03 charged runtime units per wall ns — even
with the interpreter, where it was a third of it.

`COMPILER_IDENTITY` moved with the vendored changes (phases instrumentation +
linker parse caches), so the on-disk native-module caches recompile on first
touch after deployment — expected, called out here per the guardrail.

## Acceptance Criteria

- The attribution table exists and names where every previously unexplained
  millisecond of the pox-5 `stake` call goes; each lever's before/after is
  recorded here with the exact bench command used.
- On the task-114 corpus: aggregate wasm/interpreter ≥ 1.0×, the cheapest-100
  floor within 2× of the interpreter's (≤ ~1.1 ms at current numbers), and
  pox-5 `stake` ≥ 0.5× — or a written finding for whichever target is
  structurally unreachable, with the measurement that proves it.
- Zero engine disagreements on the re-run, every block sealing the network's
  root, and all guardrail gates green.
- If `COMPILER_IDENTITY` moved, the change that moved it says so and the
  native-module cache invalidation is accounted for in the deploy notes.

## Context

- Task 114 — the bench, its fairness rules, and the measured tables.
- Task 110 — prior perf hunt: the arena, 16 KiB pages, the mmap reversal, and
  the first naming of the linker-rebuild and cache-key-recompile candidates.
- `vendor/clarity-wasm/clar2wasm/src/linker.rs` — the 223 host functions and
  the `parsed_types` cache.
- Memory note `engine-bench-wasm-vs-interpreter` — re-run recipe.
