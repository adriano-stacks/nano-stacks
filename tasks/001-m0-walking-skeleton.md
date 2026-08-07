---
id: "001"
group: build
title: "M0: establish the workspace, conformance harness, fixtures, and scoreboard"
status: completed
priority: critical
effort: large
dependencies: []
tags: ["m0", "foundation", "conformance"]
created_at: 2026-07-27
completed_at: 2026-07-27
---

# M0: establish the workspace, conformance harness, fixtures, and scoreboard

## Objective

Create the Rust workspace and an offline conformance harness. The first
scoreboard invocation must produce the explicit baseline: replay depth 0/1,
with its first divergence at block 1.

## Tasks

- [x] Create a linted Rust workspace and development shell.
- [x] Wire burnchain ingest, sortition, MARF, VM, and chainstate stubs end to end.
- [x] Add a fixture manifest and deterministic scoreboard command.
- [x] Capture the first real epoch-4 fixture set from hacknet or pox-5 testnet.

## Acceptance Criteria

- `cargo xtask scoreboard` runs without network access and reports replay depth
  `0 / 1`, first failure `block 1` until captured fixtures replace the baseline.
- The workspace passes fmt, clippy with warnings denied, and unit tests.
- Fixture capture remains an explicit incomplete subtask; no synthetic chain
  data is treated as conformance evidence.

## The fixture set exists, and there are two of them

The checked-in one is a captured hacknet 4.0 chain: 340 blocks with their Bitcoin
blocks, sortition snapshots, consensus-hash history, a checkpoint at the boundary
and — because nano's own harness was the event observer while that chain ran —
per-transaction receipts. That is what the scoreboard's four rows replay, 340/340,
on every commit.

The second is a mainnet capture at the epoch 4.0 boundary, 8,665,600, from an
archived 4.0.1 chainstate. It has no receipts and cannot be made to have any, for
the reason written up on
[[060-make-the-consensus-execution-engine-explicit-and-r]]: that stream only exists
if somebody was listening while the chain executed.

This item's acceptance criterion — a scoreboard reporting `0 / 1` and first failure
`block 1` until captured fixtures replace the baseline — is the *baseline* state it
was written in. It has been past that for a long time, and the checkbox is the last
thing here that still said otherwise.
