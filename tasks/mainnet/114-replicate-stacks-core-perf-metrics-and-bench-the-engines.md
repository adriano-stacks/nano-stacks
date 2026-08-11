---
id: "114"
group: mainnet
title: "Replicate stacks-core perf metrics and bench wasm against the interpreter"
status: completed
priority: high
effort: medium
dependencies: ["109", "113"]
tags: ["mainnet", "performance", "observability", "conformance"]
created_at: 2026-08-11
completed_at: 2026-08-11
type: feature
---

# Replicate stacks-core perf metrics and bench wasm against the interpreter

## Objective

Two questions the node cannot answer today: *how much of the block budget did
the last block spend* (stacks-core exports this; we only put costs in event
payloads), and *how fast is clarity-wasm relative to the interpreter on the
transactions mainnet actually sends* (task 109 compared whole nodes, where
trie walks and fsyncs drown the engine; nothing has ever timed the two engines
on the same call against the same state).

## What stacks-core has (surveyed 2026-08-11)

Behind `monitoring_prom` (prometheus 0.9, no-op wrappers without the feature):

- `stacks_node_last_block_{read_count,write_count,read_length,write_length,runtime}`
  — gauges, each dimension of the last processed block's `ExecutionCost` as a
  *fraction of the block limit*, set in `append_block`
  (`stackslib/src/monitoring/mod.rs:105-122`). Two long-standing bugs there:
  `write_count` is divided by `read_count`'s limit (`mod.rs:114`, `:143`), and
  `stacks_node_rpc_bandwidth_outbound` is never incremented
  (`net/rpc.rs:616` calls the inbound updater). We replicate intent, not bugs.
- `stacks_node_last_block_transaction_count`, `stacks_contract_calls_processed`,
  RPC latency histograms labelled by path, mempool confirm-time histogram,
  miner `assembly_time_ms` + `last_mined_*` variants, per-tx `time_estimate_ms`
  persisted in the mempool DB (miner deadline predictor).
- **No block-processing wall-time metric** — log-only (`"block_cost"` debug
  line). The MARF profiler (`index/profile.rs::TrieBenchmark`) is `#[cfg(test)]`;
  the new `stacks-profiler` crate is wired to nothing yet.
- The richest per-tx tool is an RPC: `/v3/blocks/replay/:id?profiler=1`
  (per-tx `execution_cost`, wall `execution_stats`, perf hardware counters),
  password-gated, unlimited budgets.

## Tasks

- [x] Metrics parity in `nano-rpc/src/metrics.rs`, published from the executor
      right after `apply`: `nano_last_block_{read_count,write_count,read_length,
      write_length,runtime}` as fractions of `EPOCH_4_BLOCK_LIMIT`,
      `nano_last_block_transaction_count`, `nano_contract_calls_processed_total`.
- [x] What stacks-core is missing and we need for the perf question:
      `nano_block_execution_seconds` histogram around `apply` — the executed
      blocks/second signal, scrapeable instead of `NANO_TIMING` printlns.
- [x] The engine bench, seamed exactly where the crosscheck already sits:
      `NANO_BENCH_ENGINES=<samples.tsv>` makes any capture replay time every
      contract call through **both** engines against the parent state
      (open-and-abort, shared `contract_calls`/`ask_engine_before_sealing`
      helpers with `engines_disagree_before_sealing`), one untimed warm-up,
      `NANO_BENCH_REPEATS` (default 3) alternating repeats, consensus cost
      trackers, medians in `scripts/bench-engines-report.py`. The interpreter's
      clock comes from `interpret_contract_call_measured`, which excludes the
      oracle's own heal/restore scaffolding — the first partial run charged
      ~800 ms of healing per call to freshly deployed contracts, which was our
      architecture being measured, not the reference engine.
- [x] Pick the workload from the capture itself: rank contracts by call count
      over `mainnet-capture-long` (6,182 blocks, 8,665,601–8,671,783). Measured
      2026-08-11: blocksurvey-proof-of-submission 912, xyk-core-v-1-2 340,
      pox-5 216, **dlmm-swap-router-v-1-1 194**, gas-oracle 137. Offline replay
      stops at 8,669,751 (task 110's header-ancestry bound), so the bench range
      is 8,665,602–8,669,750: 2,565 calls, 117 on the DLMM router, 206 xyk.
- [x] Run on a reflink copy of `mainnet-pristine`, record results here, and name
      the improvement levers the numbers point at.

## Result, 2026-08-11 — the folklore inverts

4,149 mainnet blocks (8,665,602–8,669,750), 2,564 contract calls, both engines
per call against the identical parent state, warm caches, consensus cost
trackers, medians of 3 alternating repeats, **zero disagreements**, every block
sealing the network's root. Replay wall 28:57 including the bench itself.

**clarity-wasm is ~1.9× slower than the interpreter on aggregate mainnet
traffic**: 63.4 s wasm vs 34.1 s interpreter of pure engine time. Per call:
p10 0.27×, median 0.54×, p90 0.85× — wasm loses on more than 90% of real
calls, and wins exactly where compute density is high:

| workload | calls | wasm | interpreter | wasm speedup |
|---|---|---|---|---|
| dlmm-liquidity-router::add-relative-liquidity-same-multi | 3 | 1354 ms | 3846 ms | **2.84×** |
| dlmm-swap-router::swap-simple-multi | 117 | 2613 ms | 3015 ms | **1.15×** |
| xyk-core-v-1-2::swap-y-for-x | 198 | 2339 ms | 1304 ms | 0.56× |
| blocksurvey proof-of-submission | 722 | 1623 ms | 937 ms | 0.58× |
| meme-token template (loto, psis, …) `r*` reads | ~200 | ~24 s | ~11 s | ≈0.45× |
| pox-5 stake/stake-update/unstake | 128 | 2049 ms | 320 ms | **0.16×** |

By charged runtime cost, the deficit is structural, not a fixed floor: every
bucket from <10k to >10M runtime units shows wasm at 0.30–0.61× — plus a ~1 ms
fixed penalty visible on the cheapest calls (wasm median 1.57 ms vs 0.57 ms).
The cost model itself is another finding: the interpreter delivers ~0.03
charged runtime units per wall ns and wasm ~0.01, so a runtime-full 5×10⁹
block is minutes of either engine — the runtime dimension overstates neither
engine and constrains neither; blocks limit on the read dimensions first.

**Reading.** Wasm wins when a call loops over arithmetic (DLMM multi-bin
liquidity math, sbtc-withdrawal signature checks ~1.9×). It loses when a call
is a chain of host-boundary crossings — map reads/writes, value
(de)serialization across linear memory, event construction — which is what
mainnet mostly sends. pox-5's 6× deficit is the extreme: stake is nearly all
storage traffic and event building.

**Levers, in evidence order:**

1. **Host-boundary cost** (the multiplier): every `get_variable`/`map_get`/
   `map_set` serializes values across wasm memory and re-parses type
   signatures; task 110 already measured the linker rebuilding 223 host
   functions per call. This is where pox-5's 13 ms/call and the meme tokens'
   2.5× live.
2. **Per-call fixed ~1 ms** (dominates cheap calls, i.e. most of the mempool):
   instantiation, data-segment init, linker setup. Amortizing the linker
   (blocked on `ClarityWasmContext` lifetimes) and pooling instances are the
   named fixes.
3. **Not a lever: codegen.** The in-memory `ModuleCache` is keyed by contract
   id, and repeats hit it — the measured times are execution, not compilation.

Follow-up optimization work should target 1–2 and re-run this bench; the
samples format and `scripts/bench-engines-report.py` make the comparison
one command each way.

## Not in scope, recorded

- Miner-side `last_mined_*` gauges and `time_estimate_ms` — nano mines in
  hacknet only; add when the miner path matters.
- A `/v3/blocks/replay?profiler=1` equivalent — the bench binary answers the
  same question offline without adding a password-gated RPC.

## Acceptance Criteria

- `/metrics` exposes the cost-ratio gauges, tx count, contract-call counter and
  the execution-time histogram while following, with the golden exposition test
  extended; no new lock on the execution path.
- `bench-engines` runs offline against a capture + state dir, reports per-call
  wasm and interpreter timings on identical state with zero engine
  disagreements, and its numbers for the DLMM/xyk/pox-5 contracts are recorded
  in this file with a first analysis of whether wasm can get faster.
