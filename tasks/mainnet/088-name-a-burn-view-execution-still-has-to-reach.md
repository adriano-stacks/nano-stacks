---
id: "088"
group: mainnet
title: "Name a burn view execution still has to reach"
status: completed
priority: critical
effort: medium
dependencies: ["049"]
tags: ["mainnet", "sortition", "consensus", "liveness", "release"]
created_at: 2026-08-07
type: bug
completed_at: 2026-08-09
---

# Name a burn view execution still has to reach

## Objective

A follower that falls more than `CATCH_UP_LIMIT` burn blocks behind Bitcoin can
never catch up again. The locally derived sortition chain walks its *lookahead*
tip forward to Bitcoin's tip, but the two lookups execution needs —
`height_of_consensus_hash` and `snapshot_at` — are bounded by a window measured
backwards **from that lookahead tip**. Once Bitcoin is further ahead of the
executed burn view than the window is wide, the view execution is standing on
falls out of the window, the node refuses to execute the next block, refuses to
write its chain down, and re-walks the same ground every round forever.

It is not a stall that resolves. It gets worse: Bitcoin advances, the lookahead
follows, and the gap only widens.

## Evidence

A live mainnet follower (`/home/aldur/mainnet-restored`, binary
`sha256:fca37025…`), after one clean catch-up batch:

```
executed 500 blocks, 8708125 to 8708625, state root 44d76d9a…
derived 132 sortitions locally as Bitcoin advanced, from burn 961339 to 961471
the derived sortition chain could not be written down: sortition seed: this chain
  keeps no snapshot for burn 961206, which is the burn view execution has reached,
  so writing it down would save a chain seeded above what a restart needs
the local sortition chain cannot name burn view 501f9fe4…, standing on burn 961472
  … 961473 … 961474 …                                              (and on, forever)
```

The peer's answer for that view, which agrees with this node about Bitcoin:

```
GET /v3/sortitions/consensus/501f9fe46e27faccd09859672d0d4e1f692e942a
  burn_block_height 961206, was_sortition false
GET /v3/sortitions
  burn_block_height 961488                        (Bitcoin's tip, where nano stands)
```

So the wanted view is **282 burn blocks behind the tip** and
`CATCH_UP_LIMIT` is **144**. `height_of_consensus_hash`
(`crates/nano-node/src/sortition.rs:239`) takes `CATCH_UP_LIMIT` entries back from
`self.tip().bitcoin_height`, so it cannot see 961206, and `snapshot_at(961206)`
is `None` for the same reason — which is what `save_standing_on`
(`sortition.rs:930`) reports.

This is a burn view **this node has already walked through** and executed up to.
The window's comment reasons about a view "further off than a day of Bitcoin"
being a tracker seeded on another chain; that is sound for a view arriving from a
peer and wrong for the executed burn view, which necessarily lags whenever a
catch-up batch is larger than the tenures in one burn block. 500 Stacks blocks
moved the executed view eleven burn blocks while the lookahead moved 280.

A restart hides it and does not fix it: the tracker re-seeds at the executed burn
view, the needed view is in range again, one batch runs, and the gap re-opens.
That is exactly what happened between `named.log` and `run.log`, and it is why
one successful batch must not be read as a working follower.

## Not covered by

- **082** is the reward-cycle boundary and the PoX anchor; this fails inside a
  cycle with the anchor already decided.
- **077** is peer-derived consensus fallbacks; the refusal here is correct
  behaviour on a chain that has the answer and cannot look far enough back for it.
- **049** established local derivation and bounded the walk; this is the bound
  being measured from the wrong end.

## Tasks

- [x] Measure the lookup window from the burn view execution has reached, not
      from the lookahead tip, so a view the chain has already derived and passed
      is always nameable.
- [x] Keep the walk itself bounded per round: the cost 049 removed was one
      Bitcoin download per step toward an *undreached* view, which is a different
      thing from a backwards lookup over history already in memory.
- [x] Keep the retained snapshot window reaching the executed burn view, so
      `save_standing_on` can write the chain down after every batch rather than
      only when execution happens to be near the tip.
- [x] Bound the lookahead instead, or make retention follow execution: a tracker
      that may run arbitrarily far ahead of execution has to keep what execution
      still needs.
- [x] Keep refusing a view this node genuinely has not derived, and a view on
      another chain. The refusal is right; only its reach is wrong.
- [x] Add a conformance test that executes a batch large enough to leave the
      executed burn view more than `CATCH_UP_LIMIT` behind a lookahead standing at
      Bitcoin's tip, and requires the next block to execute without a restart.
- [x] Add a test that a restart is not what makes it work: the same chain, in one
      process, executes two consecutive batches.

## Acceptance Criteria

- A follower whose executed burn view is arbitrarily far behind Bitcoin's tip
  still names every burn view it has derived, and keeps executing.
- The derived sortition chain is written down after a catch-up batch rather than
  refused, so a restart resumes instead of re-deriving from the capture.
- No peer answer is consulted for a burn view, in any branch.
- The mainnet follower advances past 8,708,625 in one process, across more than
  one batch, without a restart.

## Evidence that opened this task

Found while reconciling task 086. The VM defect at 8,708,126 and this are
distinct and consecutive: `named.log` shows the node naming 8,708,126's burn view
successfully and then failing the block in clarity-wasm, and only afterwards
losing the ability to name any new view. Task 086 owns the first; folding the
second into it would let a compiler fix stand in for a follower that cannot stay
on the chain.

## What landed 2026-08-07

Both bounds were measured from the tracker's lookahead tip, which locating a
single burn view runs all the way to Bitcoin's.

`SnapshotChain` keeps a floor that follows execution: `keep_from` says which burn
view execution has reached and nothing above it is dropped, so the window is
`max(SNAPSHOTS_KEPT, tip - executed + 1)`. It costs two hundred bytes a snapshot
times the lag, the lag is what a follower catching up necessarily has, the window
closes again as execution catches up, and the floor only moves forward so it
cannot shrink under a reader standing in it. `Node::local_view` says it, because
that is the one place holding both numbers.

`height_of_consensus_hash` no longer takes `CATCH_UP_LIMIT` entries. That bound
exists because a walk costs one Bitcoin block download per step; this is a
comparison against bytes already in memory. `holds_consensus_hash` beside it was
already unbounded for the same reason.

Pinned now by `two_bounded_batches_keep_the_view_execution_still_needs`: after
two full lookahead batches, the executed view remains in the tracker while a
midpoint view is still nameable. The conformance control below proves this remains
true in the executor rather than only in the snapshot data structure.

## Closed, 2026-08-09

`two_bounded_batches_keep_the_view_execution_still_needs` advances the lookahead
twice by `CATCH_UP_LIMIT` in one tracker and retains the executed view.
`two_lookahead_batches_do_not_require_a_restart_before_execution` performs the two
batches in one executor and then executes the next captured block without reopening
the tracker. The full deterministic catch-up suite passes 8/8, including restart
and staging-conservation controls, and the anchor regression still passes.

The preserved follower on port 20492 supplied the independent live acceptance
evidence without operator intervention: PID 3569837 remained the same process
started 2026-08-08 13:02:42, advanced through repeated one/two-block batches to
executed height 8,722,785, and reported `blocks_behind=0`. It crossed 8,708,625
far behind it without a restart. Peer-sortition access is separately pinned at
zero by `peer_sortition_lies_never_reach_execution`.
