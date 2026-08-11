---
title: "Reduce metrics-handling LoC with metrics-rs"
id: "118"
status: pending
priority: medium
type: improvement
tags: ["mainnet", "observability", "refactor"]
created_at: "2026-08-11"
dependencies: ["113", "114"]
effort: small
---

# Reduce metrics-handling LoC with metrics-rs

## Objective

Tasks 113 and 114 left `nano-rpc/src/metrics.rs` at roughly 950 lines while
using `prometheus-client` directly and wrapping every metric in `NodeMetrics`.
Evaluate replacing that plumbing with the
[`metrics`](https://github.com/metrics-rs/metrics) facade and
[`metrics-exporter-prometheus`](https://github.com/metrics-rs/metrics/tree/main/metrics-exporter-prometheus),
and keep the migration only if it produces a material net reduction in
metrics-handling code without weakening the existing observability contract.

## Tasks

- [ ] Record the current production/test LoC, dependency footprint and scrape
      output as the comparison baseline.
- [ ] Prototype the current counters, gauges, histograms and labels through
      `metrics`, with `metrics-exporter-prometheus` serving the separately bound
      scrape endpoint.
- [ ] Remove superseded registry, handle and encoding wrappers; keep a thin
      nano-specific API only where it preserves typed refusal labels or groups
      coherent updates.
- [ ] Compare the prototype with the current implementation and retain it only
      if total metrics-handling LoC and bespoke machinery fall materially.
- [ ] Preserve the golden exposition, live scrape/update and bind-failure tests;
      run fmt, clippy and the affected test suites.

## Acceptance Criteria

- The task records before/after production and test LoC; the retained design has
  a clear net reduction rather than moving equivalent wrappers between files.
- Existing metric names, types, labels and `/metrics` behavior remain compatible
  with the task 113/114 scrape contract, or any intentional change is documented
  with its dashboard/query migration.
- Recording remains lock-free or demonstrably uncontended on block execution,
  and metrics remain optional observations rather than consensus or liveness
  inputs.
- Recorder installation and test isolation are deterministic: multiple node/test
  instances do not race over global state or leak observations between tests.
- If the ecosystem crates do not simplify the implementation after the spike,
  document the measured reason and close the task without migrating.
