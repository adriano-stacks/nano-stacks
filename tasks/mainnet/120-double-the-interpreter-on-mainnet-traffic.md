---
id: "120"
group: mainnet
title: "Double the interpreter on mainnet traffic"
status: completed
priority: high
effort: large
dependencies: ["115"]
tags: ["mainnet", "performance", "vm", "clarity-wasm"]
created_at: 2026-08-11
completed_at: 2026-08-11
type: feature
---

# Double the interpreter on mainnet traffic

## Objective

Task 115 closed the wasm-vs-interpreter deficit to 1.52×. The follow-on
directive: keep going until the production engine is **2× the interpreter**
on the task-114 corpus. Reached: **2.06×** (wasm 15.6 s vs interpreter
32.0 s over 2,564 mainnet calls), per-call median 1.08×, p90 2.16×, zero
engine disagreements, every one of 4,149 blocks sealing the network's root.

## The rounds, each attributed before it was built

| round | change | corpus |
|---|---|---|
| (115 close) | parse caches + reference-walk skip | 1.52× |
| shape memo | arena-memoized `value_size`/`serialized_size`; invariant-list size from the representation; `runtime_shape_is_equal` through the parse cache | 1.58× |
| setup + admit | **linker template** (223 host functions registered once per `ModuleCache`, cloned per call — the `ClarityWasmContext` lifetime blocker resolved by one documented transmute; vendor permits unsafe, nano crates still forbid it); tuple admits short-cut when the arena value's signature equals the declaration; `admit_preserves` type-identity skip host-side and then at codegen | 1.72→1.94× |
| size-by-handle | new `runtime_shape_size(handle)` host call: generated code passes the handle instead of writing the whole representation into a region per measurement (a fold measures per iteration) | (in 1.94×) |
| **globals + `InstancePre`** | the five cost meters moved from linker-created store-owned imports to module-defined exported globals (initialized `i64::MAX` exactly as before, read/written through the same `Global` handles, now resolved from exports post-instantiation) — which unblocked caching a pre-resolved `InstancePre` per contract: instantiation stops walking 223 import names per call frame | **2.06×** |

Every round: clar2wasm's 1,473 lib tests, scoreboard 340/340 (roots,
receipts, **costs**) under `NANO_REPLAY_BOTH_ENGINES`, frozen digests
500/500, then the full 4,149-block corpus bench — which validates every
`state_index_root` while it measures, so a semantic slip fails the run.

## Correctness arguments worth keeping

- **Invariant-list sizing**: `Value::size()` of a list is
  `ListTypeData(len, least-supertype of element dynamic types).size()`; for
  `int`/`uint`/`bool`/`principal` elements the supertype fold is the identity
  (`type_of` maps every value of these types to exactly its declared type, and
  a declared-`principal` element always materializes as `Value::Principal` —
  the `CallableContract` arm needs a declared trait type). `type_size` is
  count-independent and equals the `NoType` empty-list derivation, so `(list)`
  agrees too.
- **`admit_preserves`**: admission = implicit cast + sanitize + `admits`. The
  cast only re-tags callables, sanitization only strips undeclared tuple
  fields, and exact types admit themselves — so a type with no tuple and no
  callable anywhere in it admits every value it can carry unchanged. Skipping
  the whole protocol is additionally safe because the generator itself copies
  the argument representation into the callee region *before* the host call:
  skipping leaves the generator's own copy in place.
- **Tuple admits**: a `TupleData`'s `type_signature` is its dynamic type,
  maintained by every constructor — signature equality with the declaration
  means nothing anywhere in the value can be stripped or re-tagged.
- **Cost globals**: all reads and writes were already through `wasmtime::Global`
  handles; only where the handles come from changed (exports instead of
  linker definitions). Same initialization, same per-instance freshness.
- **Lifetime erasure** (`host_linker`, `instance_pre`): the erased types differ
  only in lifetimes (identical layout); the linker and pre-instantiation hold
  no context value — host functions receive the real, correctly-lived context
  through `Caller` per invocation and capture nothing from it.

## What was tried and rejected, with reasons

- **Memoizing admits per handle** — a fold's accumulator is a new value (new
  handle) every iteration; nothing repeats.
- **Caching deserialized `ContractContext` per contract id** (~0.5 ms/call) —
  a reorged-away deploy would then fail with the wrong error identity (or
  execute against ghost state) instead of the reference's contract-not-found;
  consensus risk for 3%. The existence check must stay on the database read.
- **wasmtime pooling allocator** — sizing limits against arbitrary mainnet
  contract memories; instantiation is already CoW-backed.

## Still on the table, if more is ever needed

- `sha256`/`hash160` run as hand-written WAT inside the module; `keccak256`
  is a native host call. blocksurvey (hash-heavy, 722 calls) is the last
  major class below parity (0.96×).
- Host-function glue: per-op metadata clones in the var/map closures.
- The interpreter comparison itself is conservative: the oracle pays no
  wasm-side costs but also skips the reference's own transaction machinery.

## Verification

Same recipe as tasks 114/115 (`NANO_BENCH_ENGINES` over
`mainnet-capture-long`, 4,149 blocks, fresh reflink of `mainnet-pristine`).
Samples: `/home/aldur/bench-engines-round{2..7}.tsv`. `COMPILER_IDENTITY`
moved (vendored codegen changes); generated wasm changed shape (new import,
admit skips), so on-disk native-module caches recompile on first touch.
