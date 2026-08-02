---
id: "047"
title: "Make mainnet synchronization monotonic and restart-safe"
status: in-progress
priority: critical
effort: large
type: bug
group: mainnet
dependencies: ["046", "048"]
tags: ["mainnet", "sync", "persistence"]
created_at: 2026-08-02
---

# Make mainnet synchronization monotonic and restart-safe

## Objective

Both sync layers can discard useful progress. `TenureFollower` drops a partial
same-tenure extension when its 32-block walk does not reach a moving tip. The
live node's partial response repeatedly stopped at height 8,689,223 while the
peer advanced, followed by `peer tenure does not extend the followed chain` on
every poll.

`CheckpointExecutor::follow_to_tip` is worse: it walks the entire gap backward
into a `Vec` before executing anything. One 429 discards the walk and the next
round begins again. The mainnet store still ends at its 8,665,601 anchor after
hundreds of failed attempts.

## Tasks

- [x] Retain every validated partial tenure extension and resume from its last
      block on the next poll.
- [x] Replace whole-gap buffering with bounded forward execution chunks.
- [x] Make rate limits and bounded peer pages end a round successfully after all
      available progress is committed.
- [x] Persist executed tip, parent links and tenure accounting together at each
      chunk boundary.
- [x] Resume after a process stop without refetching or re-executing sealed
      blocks.
- [ ] Bound caches and in-memory ancestry independently of distance from tip.
- [ ] Test gaps spanning long tenures and multiple tenures with deterministic
      429s, short pages, tip movement and a restart after every chunk boundary.

## Acceptance Criteria

- Executed height advances monotonically from a checkpoint 20,000 blocks behind
  under forced rate limits and bounded responses.
- Restarting at any committed block produces the same final root and accounting
  as uninterrupted execution.
- A live mainnet soak crosses tenure boundaries without a permanent `Fork` loop
  and reports executed, rather than followed, lag.

## Restarting reaches the same state

`crates/nano-conformance/tests/restart.rs` replays forty captured blocks twice:
once uninterrupted, and once in two halves with the chainstate closed and
reopened between them, resuming from the accounting the first half wrote out.
Both reach the same sealed tip and owe the same.

That is the property a catch-up depends on, and it is checked offline against
the captured fixture rather than by stopping a live node and hoping. The
remaining unchecked item is a deterministic harness for rate limits and short
pages, which the live mainnet run exercises but no test yet pins.
