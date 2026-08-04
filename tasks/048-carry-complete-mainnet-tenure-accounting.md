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
- [x] Pay the maturing tenure's parent its own tenure fees, not the child
      tenure's fees.
- [ ] Rebuild the live accounting from accepted chain history and reject the
      result unless all 201 required tenures are present and contiguous.
- [ ] Re-execute mainnet block 8,673,864 from the reconstructed state and match
      its expected root before counting any later replay depth.
- [x] Replace the incomplete mainnet artifact used by the node and scoreboard.

## Acceptance Criteria

- The artifact contains every pre-checkpoint earning needed until nano's own
  executed tenures mature.
- The first post-checkpoint tenure and the first nano-derived maturity both match
  stacks-core.
- Missing, duplicate or short accounting windows are rejected during capture
  and startup.
- Repeatedly rejecting the first maturity block cannot change `started`, tenure
  earnings or matured effects on disk.

## Why this task is open again

The replacement artifact now contains 102 tenures and three schedule entries,
and its structural fixture test passes. That is necessary but it does not meet
the behavioral acceptance criterion: replay has crossed only a few tenure
starts, not the 101 required to observe nano-derived earnings mature.

The live accounting file is also polluted by failed retries at 8,665,780 as
described in [[056-make-rejected-block-execution-leave-no-state]]. It is not
evidence for this task and must be regenerated before the 101-tenure replay.

## `rebuild-accounting` needs to say where it is

Re-deriving mainnet accounting from a public peer walks every block of every
tenure in the maturity window — roughly 200 tenures — and a rate-limited peer
turns most requests away. In practice that is **over an hour with no output at
all**, and nothing distinguishes it from a hang: no tenure counter, no block
counter, no note when a request is retried.

The retry itself is right (`count_fees` backs off up to 8 times, because a
repair that is not complete is worth nothing). What is missing is saying so.
It should log the tenure it has reached, so a run can be judged rather than
waited on.

## The silence was hiding a starved backoff, not a slow walk

`/proc/<pid>/io` showed the 1h45m run had read **nothing in 90 seconds**, on the
same socket, with 6 seconds of CPU. It was not slow; it was starved.

The cause was in `nano-sync`'s 429 handling: it took the peer's `Retry-After`
and applied the same 2-second ceiling it uses for its own guess, so a peer
asking for a minute was asked again two seconds later — earning another 429,
indefinitely. Fixed: a peer's answer is honoured as given, bounded at two
minutes so a broken header cannot park a catch-up.

With that and the progress line in, the same walk runs visibly:

```
tenure 251419: 2 counted, 199 to go
...
tenure 251394: 27 counted, 174 to go
```

About 0.6 tenures a minute against a public peer, so the full window is a
multi-hour run — but a bounded and observable one, and it reaches the tenure
that matters (251321) around the halfway point.

## The replay reached the first nano-derived maturity

The durable mainnet chain now matches **8,263 consecutive roots**, through
8,673,863. Block 8,673,864 starts tenure 251,422 and is the first block that
matures a tenure nano accounted for itself. Its receipts succeed, but its root
does not match.

The discrepancy named a consensus rule: the new tenure pays its parent the
parent's own accumulated fees. Nano paid the new tenure's fees instead. The rule
is fixed and covered by a focused test, but it is not yet live-root evidence:
the existing `accounting.json` has only 158 tenure records and is missing 44,
including 251,335–251,336 and 251,378–251,419.

`rebuild-accounting` is reconstructing the 201-tenure window from chain history.
Do not resume against the old file or close this task on the formula unit test.
The acceptance event is a clean reconstruction followed by a matching root at
8,673,864 and a restart that preserves the same accounting.
