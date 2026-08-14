---
id: "127"
title: "Add operator health and diagnostics to nano-tui"
status: in-progress
priority: high
effort: medium
type: feature
group: mainnet
dependencies: ["126"]
tags: ["tui", "operations", "metrics", "ux"]
touches: ["crates/nano-rpc", "crates/nano-node", "crates/nano-tui"]
created_at: 2026-08-14
verify:
  - type: bash
    run: "nix develop -c cargo test -p nano-rpc -p nano-node -p nano-tui"
  - type: bash
    run: "nix develop -c cargo clippy -p nano-rpc -p nano-node -p nano-tui --all-targets -- -D warnings"
---

# Add operator health and diagnostics to nano-tui

## Objective

Let an operator decide whether the node is healthy and identify the constrained
subsystem without reading logs, using diagnostics the node already publishes.

This is delivery slice 2 from task 125's usability study.

## Tasks

- [x] Publish this process's enabled follower, miner and signer roles through a
      small nano-specific RPC field.
- [ ] Derive conservative starting, syncing, healthy, degraded, stalled and
      unreachable states from named evidence and freshness.
- [ ] Put health, reason, last-sealed age, lag, network and local roles in the
      persistent header.
- [x] Add an Operations view for staged work, relay and role queues, peers,
      StackerDB/proposal coverage and event-observer delivery.
- [x] Optionally poll the metrics URL for refusals, unanswered rounds, failovers,
      mempool, last-block execution/cost and cache memory.
- [x] Render counters as changes or rates since the TUI opened and gauges as
      current state; never alert on an unexplained lifetime total.
- [ ] Show the facts used by every derived status and make missing optional
      metrics explicit without degrading RPC health.
- [ ] Cover healthy, catch-up, stalled, refusal, queue pressure, observer failure,
      missing metrics and mixed-role nodes.
- [ ] Run rustfmt, tests and strict clippy without warnings.

## Acceptance Criteria

- The header states health, evidence and freshness without using lag as the only
  definition of health.
- A non-moving executed height, growing queue, new refusal or unreachable role
  peer is attributed to the relevant subsystem.
- The network's current miner cannot be mistaken for this process's role.
- Every queue and observer already present in `/nano/sync_status` is visible in
  Operations, including zero and unavailable as distinct states.
- Metrics being absent or private leaves their panels unavailable and does not
  make a responsive RPC endpoint unhealthy.
- Counter labels state their session window; detail values remain inspectable.
