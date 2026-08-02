---
id: "048"
title: "Carry complete mainnet tenure accounting"
status: pending
priority: critical
effort: medium
type: bug
group: mainnet
dependencies: ["043"]
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

- [ ] Recapture mainnet accounting with the complete maturity window, emission
      schedule, current started tenure and accumulated fees.
- [ ] Make capture fail when any required tenure in the maturity window is
      absent instead of writing a partial checkpoint.
- [ ] Validate network and checkpoint tenure height against the exported
      schedule and entries.
- [ ] Replay across at least 101 tenure starts, including a restart, and compare
      every state root.
- [ ] Replace the incomplete mainnet artifact used by the node and scoreboard.

## Acceptance Criteria

- The artifact contains every pre-checkpoint earning needed until nano's own
  executed tenures mature.
- The first post-checkpoint tenure and the first nano-derived maturity both match
  stacks-core.
- Missing, duplicate or short accounting windows are rejected during capture
  and startup.
