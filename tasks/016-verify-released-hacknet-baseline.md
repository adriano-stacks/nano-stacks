---
id: "016"
title: "Verify nano-stacks against released Hacknet baseline"
status: completed
priority: high
effort: medium
dependencies: []
tags: ["hacknet", "pox5", "interop", "verification"]
created_at: 2026-07-29
completed_at: 2026-07-29
---

# Verify nano-stacks against released Hacknet baseline

## Objective

Run nano-stacks' full verification suite after moving Hacknet to released
PoX-5 dependencies, and resolve any incompatibilities it reveals.

## Tasks

- [x] Run all workspace tests.
- [x] Run Clippy with warnings denied for every target.
- [x] Diagnose and fix any compatibility regressions.

## Acceptance Criteria

- All workspace tests pass.
- Clippy reports no warnings for any workspace target.

## Run on 2026-08-06, against everything that has landed since

The three items were never checked off although the suite has been the gate on every
commit since. Recorded here with the numbers of the run that closed them, on the
released PoX-5 dependency set:

```
cargo clippy --release --all-targets            0 warnings, 0 errors
cargo test --release (workspace)                72 + 208 + 19 passed, 0 failed, 2 ignored
```

The two ignored are the pair that need infrastructure this suite does not stand up,
and they are `skip_gate`d rather than silently absent — a distinction
[[053-pass-the-mainnet-node-release-gate]] had to make when it turned out that a
suite where every mainnet test skipped looked identical to one where they all passed.

No compatibility regression was found by this run. The regressions that *were* found
since the baseline moved are recorded on the tasks that fixed them rather than here,
because a list of them in this file would go stale the moment the next one lands.
