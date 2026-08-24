---
id: "148"
title: "Recover from an unnameable burn view without a restart"
status: pending
priority: critical
effort: medium
dependencies: []
tags: ["mainnet", "sortition", "liveness", "release"]
created_at: 2026-08-24
type: bug
---

# Recover from an unnameable burn view without a restart

## Objective

A catching-up follower stops permanently on a burn view it has already derived,
reports itself healthy while doing so, and only a process restart clears it. It
cost sixteen hours of the release subject's catch-up on 2026-08-23/24 and it
would void a 24-hour hold, so it blocks
[[106-hold-the-release-candidate-at-mainnet-tip-for-24-h]] rather than merely
slowing it.

## What was observed

The release subject
(`nano-stacks-follower-0.1.0-88920833e521`, `/home/aldur/release-subject-88920833`)
finished its import, executed to Stacks 8,743,989, and then repeated one round
forever:

```text
followed 8743989 -> 8743989
burn view 0f4add0d…: 46 blocks from http://104.238.220.206:20443/
no peer served the scheduled tenure at burn view b6ab13cc…: HTTP sync error …
the local sortition chain cannot name burn view 653ee60a7e292c85b46b24783ef1ae667378eb9a,
standing on burn 963817: this node will not execute block 8743990 under a burn
block a peer picked, and the next round walks again
```

`/health` answered `ready: true`, `last_error: null`, `p2p_connected: 8`
throughout, so nothing outside the log could tell the node was dead.

**The refusal is right and the state is not corrupt.** The view it could not name,
`653ee60a…`, is burn **962,151** — confirmed against both stock oracles — and
execution was standing on burn 962,149, two blocks below it, with the tracker's
tip at 963,817. So the view sits *inside* the range the chain had derived, and
`SnapshotChain::snapshots_to_keep` is written to retain exactly that:
`needed_from` up, not merely `SNAPSHOTS_KEPT`. A restart resumed execution
immediately, at ~6,400 blocks an hour, from the same durable state — so this is
transient in-memory tracker state, not the window design failing and not
anything on disk.

## Where to look

Three things are already in place and one of them is not doing its job:

- `Tracker::keep_for_execution` (`nano-follower/src/sortition.rs`) lowers the
  floor to `execution_rollback_floor(executed)`, and `local_view` calls it on
  every lookup with `self.bitcoin_height()`.
- `SnapshotChain::keep_from` (`nano-sortition/src/lib.rs`) is **monotonic
  upward** by design: "a floor that went backwards would let the window shrink
  under a reader still standing in it." So any path that once raised
  `needed_from` above the view execution later needs strands it permanently, and
  `stand_on_known_block` resets the executor's `bitcoin_height` to 0 without
  lowering that floor.
- `Executor::local_view` takes the fast path only `if tracker.is_primed()`. When
  it is not, the answer comes from a bounded walk that reports
  `LocalView::Unreached` — which is what the log shows — rather than from the
  unbounded consensus-hash history, which is never front-pruned and *does*
  contain the view.

The reproduction to write first is the cheap one: drive a tracker so
`needed_from` is raised above a view, then ask for that view, and assert the
chain still names it or says precisely which of the three causes applies.

## Tasks

- [x] Reproduce the stall in a test from the tracker's own API, without a node.
      `nano_sortition::tests::a_rolled_back_executor_can_still_name_its_burn_view`,
      ignored and inventoried as `semantic` with owner 148 so it blocks release.
- [x] Establish which of the three paths above produces it, by measurement rather
      than by reading — the log line alone cannot distinguish "not primed" from
      "floor too high". **It is the floor.** The test fails at its second
      assertion: after the floor rises, the window closes, and execution then
      retracts below it, `snapshot_at` answers `None` while `history()` still
      contains the view — so the chain has not forgotten deriving it and is not
      unprimed, it simply refuses to answer. The remaining work is the fix.
- [ ] Make a view the chain has derived nameable for as long as any unexecuted
      staged block can ask for it, or refuse in a way that names the cause.
      **The reproduction is at the wrong layer to gate this fix, which is worth
      knowing before anyone tries to make it pass.** `SnapshotChain` has no
      burnchain source, so once it has dropped a snapshot it cannot get it back;
      lowering the floor afterwards can never satisfy the test at that layer. The
      fix therefore has to either stop the drop happening or live in `Tracker`,
      which does hold a block source and can re-derive. Two candidate shapes:
      raise the floor only as far as the executor can promise never to go back
      below — `execution_rollback_floor` subtracts `MINING_COMMITMENT_WINDOW`,
      which bounds a *Bitcoin* reorganisation and says nothing about how far a
      *Stacks* fork retraction moves the burn view — or make `keep_from` report
      that it refused a lower floor instead of silently keeping a higher one, so
      the caller can rebuild rather than loop. The silent disagreement is the part
      that turned a recoverable condition into sixteen hours.
- [ ] Report the condition through `/health` and `/nano/sync_status`. A node that
      cannot advance must not answer `ready: true` with a null error; the hold
      harness and any supervisor read those and both were blind here.
- [ ] Add a regression that fails if executed height is static while staged
      blocks exist and health still reports ready.

## Acceptance Criteria

- A follower whose tracker has run ahead of execution continues to execute
  without a restart, on the same durable state.
- The stall condition is visible in `/health` and `/nano/sync_status` before a
  human reads a log.
- The test reproducing it fails on the current code and passes after the fix.
- No regression in the per-Stacks-block cost of `local_view`: the fast path stays
  a history lookup and does not reinstate a burnchain round trip per block, which
  [[049]] measured out of existence.
