---
id: "001"
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
- [ ] Capture the first real epoch-4 fixture set from hacknet or pox-5 testnet.

## Acceptance Criteria

- `cargo xtask scoreboard` runs without network access and reports replay depth
  `0 / 1`, first failure `block 1` until captured fixtures replace the baseline.
- The workspace passes fmt, clippy with warnings denied, and unit tests.
- Fixture capture remains an explicit incomplete subtask; no synthetic chain
  data is treated as conformance evidence.
