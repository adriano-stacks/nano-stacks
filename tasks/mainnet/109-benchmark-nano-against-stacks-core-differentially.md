---
id: "109"
group: mainnet
title: "Benchmark nano against stacks-core differentially"
status: in-progress
priority: medium
effort: medium
dependencies: []
tags: ["mainnet", "performance", "conformance", "tooling"]
created_at: 2026-08-10
type: chore
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
- [ ] Tier 2 and 3 are follow-ups; this task records their design so the
      numbers land somewhere agreed.

## Acceptance Criteria

- `cargo bench -p nano-conformance` runs both sides of each surface and
  reports per-implementation timings on identical inputs.
- Benches build under the workspace clippy gates and touch no production
  crate's dependency graph.
