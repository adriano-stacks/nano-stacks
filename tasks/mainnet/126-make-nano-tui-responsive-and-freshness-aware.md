---
id: "126"
title: "Make nano-tui responsive and freshness-aware"
status: completed
priority: high
effort: medium
type: feature
group: mainnet
dependencies: []
tags: ["tui", "ux", "reliability"]
touches: ["crates/nano-tui", "README.md", "Cargo.lock"]
created_at: 2026-08-14
verify:
  - type: bash
    run: "nix develop -c cargo test -p nano-tui"
  - type: bash
    run: "nix develop -c cargo clippy -p nano-tui --all-targets -- -D warnings"
completed_at: 2026-08-14
---

# Make nano-tui responsive and freshness-aware

## Objective

Make the TUI remain interactive through slow or partial RPC failure, state the
freshness of every answer it retains, and render legibly from 80x24 upward.

This is delivery slice 1 from task 125's usability study.

## Tasks

- [x] Parse and validate `--rpc-url`, optional `--metrics-url` and `--once` with
      a conventional CLI parser; document the binary and exit behavior.
- [x] Move HTTP polling and block backfill out of the render/input loop.
- [x] Fetch independent status routes concurrently and deliver partial snapshots.
- [x] Track loading, fresh, stale and unavailable state per RPC source, including
      last-success time and the latest short error.
- [x] Backfill blocks incrementally without delaying keys, redraw or shutdown.
- [x] Add wide, standard 80x24 and explicit too-small layouts.
- [x] Cover slow, timed-out, partial and long-value snapshots at 80x24, 110x32
      and a wide terminal.
- [x] Run rustfmt, tests and strict clippy without warnings.

## Acceptance Criteria

- The first frame appears before any HTTP request finishes.
- A four-second endpoint timeout never blocks redraw, resize, help or quit.
- One failed route cannot mark fresh answers from other routes as current or
  mark the entire node unreachable.
- The last good value remains visible with its stale age and failure reason.
- Long mainnet values do not join labels to values or clip the selected panel at
  80x24, 110x32 or wider sizes.
- `nano-tui --once` uses the default URL, validates flags, and exits differently
  for healthy/degraded and unreachable snapshots.
- The README contains a minimal launch and keyboard example.
