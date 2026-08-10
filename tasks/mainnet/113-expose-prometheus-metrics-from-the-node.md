---
title: "Expose Prometheus metrics from the node"
id: "113"
status: completed
priority: medium
type: feature
tags: ["mainnet", "node", "observability", "rpc"]
created_at: "2026-08-10"
effort: medium
completed_at: 2026-08-10
---

# Expose Prometheus metrics from the node

## Objective

The follower's health is currently read by tailing `run-*-docker.log` and
polling `/v2/info` by hand. The 8,733,929 stall (task 111) sat as a
once-a-minute log line for hours, and the memory-growth hunt (task 107) was
run with `docker stats` snapshots pasted into notes. Both would have been a
glance at a dashboard: a stuck executed-height gauge with a rising refusal
counter, and an RSS slope. Serve a Prometheus text-format `/metrics` endpoint
so the node can be scraped, graphed and alerted on.

Use a vetted metrics crate rather than hand-rolling the exposition format, and
keep it out of the consensus path: metrics are observations of work already
done, never inputs to it. stacks-core gates its equivalent behind
`monitoring_prom`; ours should be always-on but bound separately from the
public RPC so exposure is an operator decision.

## Tasks

- [x] Serve `/metrics` in Prometheus text format on a separately configurable
      bind address (default loopback), wired through `nano-rpc`/`nano-node`
      alongside the existing axum surface.
- [x] Chain progress: executed tip height, followed tip height (kept distinct,
      per task 046), burn/sortition height, and the timestamp of the last
      sealed block — the stall signature is "followed advances, executed does
      not".
- [x] Fail-closed visibility: counters for refused blocks by typed reason
      (compiler gap, root mismatch, signature, missing context), so a
      consensus refusal alerts instead of idling in a log.
- [x] Sync and peer health: per-role serving-peer pool size, failovers, rounds
      unanswered, download queue depth, and pushed-block accept/refuse counts
      (per-tenure attribution stays in the log; the gauge is for alerting).
- [x] Resource internals for task 107's follow-up: MARF node cache entries and
      bytes, wasm module cache entries, tenure-history window length, mempool
      size — the gauges the memory audit had to reconstruct from heap dumps.
- [x] Document the scrape target in the hacknet overlay and add it to the
      monitoring compose (or note where the operator points their own
      Prometheus), without registering the node with any service that would
      make monitoring load-bearing.

## Acceptance Criteria

- `curl :PORT/metrics` returns well-formed Prometheus text exposition while
  the node follows at tip, and the executed/followed height gauges visibly
  diverge when a block is refused (testable with a fault-injected refusal in
  a dev run, not a production fallback path).
- No metric read takes a lock the execution path contends on; a scrape during
  block execution does not measurably slow it.
- The release run's evidence can cite metrics without them being a liveness
  or consensus dependency: the node runs identically with the port unbound.

## Context

- Log lines being replaced as the primary signal: "executed nothing: sealed at
  H, then the round failed", "StackerDB replication has been served by N of M
  peers", "p2p: N connected".
- Memory audit that motivates the resource gauges: task 107 and the
  `node-memory-growth` findings (MARF node cache, wasm ModuleCache,
  tenure-history clones).
