---
id: "048"
title: "Carry complete mainnet tenure accounting"
status: in-progress
priority: critical
effort: medium
type: bug
group: mainnet
dependencies: ["043", "056"]
tags: ["mainnet", "checkpoint", "chainstate"]
created_at: 2026-08-02
---

# Carry complete mainnet tenure accounting

## Objective

The mainnet capture's `native-effects.json` contains one matured effect at
coinbase height 251,321 and no `tenures` or `coinbase_schedule`. It was captured
before [[043-carry-every-unmatured-tenure-with-the-checkpoint]] fixed the Hacknet
export. The next mainnet tenure cannot derive its earnings, and the first payout
not explicitly seeded must fail with `UnknownTenure`.

## Tasks

- [x] Recapture mainnet accounting with the complete maturity window, emission
      schedule, current started tenure and accumulated fees.
- [ ] Make capture fail when any required tenure in the maturity window is
      absent instead of writing a partial checkpoint.
- [ ] Validate network and checkpoint tenure height against the exported
      schedule and entries.
- [ ] Replay across at least 101 tenure starts, including a restart, and compare
      every state root.
- [x] Replace the incomplete mainnet artifact used by the node and scoreboard.

## Acceptance Criteria

- The artifact contains every pre-checkpoint earning needed until nano's own
  executed tenures mature.
- The first post-checkpoint tenure and the first nano-derived maturity both match
  stacks-core.
- Missing, duplicate or short accounting windows are rejected during capture
  and startup.

## Why this task is open again

The replacement artifact now contains 102 tenures and three schedule entries,
and its structural fixture test passes. That is necessary but it does not meet
the behavioral acceptance criterion: replay has crossed only a few tenure
starts, not the 101 required to observe nano-derived earnings mature.

The live accounting file is also polluted by failed retries at 8,665,780 as
described in [[056-make-rejected-block-execution-leave-no-state]]. It is not
evidence for this task and must be regenerated before the 101-tenure replay.
