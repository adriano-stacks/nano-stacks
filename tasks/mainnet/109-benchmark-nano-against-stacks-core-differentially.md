---
id: "109"
group: mainnet
title: "Benchmark nano against stacks-core differentially"
status: completed
priority: medium
effort: medium
dependencies: []
tags: ["mainnet", "performance", "conformance", "tooling"]
created_at: 2026-08-10
type: chore
completed_at: 2026-08-10
---

# Benchmark nano against stacks-core differentially

## Objective

Correctness against stacks-core is proven; performance is folklore. The same
structure that makes correctness testable — stacks-core as a dev-dependency of
`nano-conformance`, fixtures replayed offline — makes performance
*differentially* measurable: both implementations, identical inputs, one
process, one machine, with stacks-core still absent from the release graph.

## Tiers, cheapest first

1. **Differential microbenchmarks** (this task's deliverable): criterion
   benches in `nano-conformance` running nano and the stackslib equivalent on
   the same inputs. First surfaces: SIP-005/Nakamoto codec decode and
   re-encode over the 340 fixture blocks, and MARF block sealing over the
   lockstep scripts (insert + seal + commit, fresh stores per sample).
2. **Replay throughput**: blocks/second executing the same range from the same
   checkpoint — nano's replay harness vs `stacks-inspect replay-naka-block` on
   equivalent chainstate. Cache configuration dominates (measured: 0.78 s vs
   1.8 s per block on cache sizing alone, task 107), so both sides' cache
   settings are part of the result, not a footnote.
3. **Assembled-node comparison**: hacknet side-by-side block
   arrival→executed latency via event observers; mainnet
   checkpoint-to-tip time, tip hold lag, RSS/CPU footprint.

Not benchmarked: P2P sync against a live network — peer weather makes the
comparison meaningless; checkpoint-to-tip time is the honest aggregate.

## Tasks

- [x] Criterion harness in `nano-conformance` (`benches/differential.rs`),
      dev-only, `harness = false`.
- [x] Codec: decode and re-encode the fixture Nakamoto blocks, nano vs
      stacks-core, same bytes. First quick-mode reading over the 340 captured
      blocks: decode 1.71 ms vs 3.09 ms, encode 369 µs vs 488 µs.
- [x] MARF: seal the lockstep workload (batch insert + seal + commit), nano vs
      stacks-core, same keys and block chain. First quick-mode reading:
      6.90 ms vs 10.76 ms for the ten-block chain, fresh stores per sample.
- [x] Tier 2 and 3 are follow-ups; this task records their design so the
      numbers land somewhere agreed.

## Tier 2 result, 2026-08-10

The same 99 mainnet blocks (heights 8,665,602–8,665,700, the first tenures
after the attested checkpoint), replayed on the same machine from reflink
copies. nano: `replay-blocks` over `mainnet-capture` against a copy of
`mainnet-pristine` (33 GB state at the anchor). stacks-core `6d58b498d3`:
`stacks-inspect validate-block <db> range 8665602 8665701` against a copy of
the 722 GB archive chainstate — verified to run the full `append_block`
(execution, cost check, root check) and then roll the trie back.

| run | nano | stacks-core |
|---|---|---|
| wall, cold | 25.9 s (0.262 s/block) | 22.8 s (0.230 s/block) |
| wall, warm | 24.1 s (0.244 s/block) | 20.8 s (0.210 s/block) |
| user CPU | 6.2–7.0 s | 2.1–2.3 s |
| peak RSS | 819 MB | 50 MB |

Reading: a wall-clock tie within ~15%, both runs I/O-dominated (23–43 %
CPU). Two structural asymmetries, one each way: nano *commits* every block
durably (~65 MB written, WAL fsyncs) while stacks-core validates and
discards (~0.5 MB written); stacks-core walks a 722 GB chainstate while
nano walks 33 GB. nano burns ~3× the CPU per block despite the wasm
engine — trie-node decoding is the known cost, and these blocks are small
(2–3 transactions), so per-block fixed costs dominate engine differences.
A longer range with fatter blocks is the follow-up that would separate
engine speed from storage-walk speed.

## Acceptance Criteria

- `cargo bench -p nano-conformance` runs both sides of each surface and
  reports per-implementation timings on identical inputs.
- Benches build under the workspace clippy gates and touch no production
  crate's dependency graph.
