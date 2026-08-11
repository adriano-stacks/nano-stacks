---
title: "Reduce metrics-handling LoC with metrics-rs"
id: "118"
status: completed
priority: medium
type: improvement
tags: ["mainnet", "observability", "refactor"]
created_at: "2026-08-11"
dependencies: ["113", "114"]
effort: small
completed_at: 2026-08-11
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

- [x] Record the current production/test LoC, dependency footprint and scrape
      output as the comparison baseline.
- [x] Prototype the current counters, gauges, histograms and labels through
      `metrics`, with `metrics-exporter-prometheus` serving the separately bound
      scrape endpoint.
- [x] Remove superseded registry, handle and encoding wrappers if the prototype
      is retained; the measured prototype was rejected, so no second metrics
      implementation or compatibility wrapper remains.
- [x] Compare the prototype with the current implementation and retain it only
      if total metrics-handling LoC and bespoke machinery fall materially.
- [x] Preserve the golden exposition, live scrape/update and bind-failure tests;
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

## Measured decision: keep `prometheus-client`

The isolated prototype replaced `prometheus-client` with `metrics` 0.24.6 and
`metrics-exporter-prometheus` 0.18.3, with exporter default features disabled.
It kept one recorder and one set of metric handles per `NodeMetrics`; it did not
install the process-global recorder. All three metrics tests passed against the
prototype, including the golden scrape and live TCP update.

The result is not a material simplification:

| measurement | current | prototype | change |
|---|---:|---:|---:|
| production lines in `metrics.rs` | 747 | 671 | -76 (-10.2%) |
| test lines | 206 | 206 | 0 |
| normal `nano-rpc` dependency closure | 446 packages | 464 packages | +18 |

Cargo added 24 lockfile packages for the prototype. More importantly, the
facade's gauge stores `f64`, while the current gauges retain exact integer
heights and byte counts; the prototype needed a precision-losing conversion.
The exporter also required explicit histogram upkeep and manual OpenMetrics EOF
framing to preserve the current scrape contract. Its global recorder can be
installed only once, while its local recorder is thread-local and explicitly
does not follow multithreaded async work. Keeping per-node handles avoids that
global-state bug, but gives up most of the facade's intended reduction.

The prototype was therefore removed. The retained implementation gained
`node_metrics_instances_are_isolated`, which proves two node/test registries do
not share samples. `cargo test -p nano-rpc` passes 38 tests with the one declared
Hacknet infrastructure test ignored; `cargo clippy -p nano-rpc --all-targets --
-D warnings`, rustfmt and diff-check are green. No dependency or scrape-format
change is retained.
