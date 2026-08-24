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
      Two, both unconditional and both green:
      `sortition::tests::a_dropped_view_is_told_apart_from_one_not_yet_walked_to`
      pins the distinction the executor now reports, and
      `follow_path::a_fork_retraction_leaves_a_chain_that_can_name_its_burn_view`
      drives a real fork retraction end to end. The earlier ignored
      `SnapshotChain`-layer placeholder was removed: it asserted something that
      layer cannot deliver, so it could never have passed, and keeping a permanently
      red `semantic` ignore beside a working regression would have been noise.
- [x] Establish which of the three paths above produces it, by measurement rather
      than by reading — the log line alone cannot distinguish "not primed" from
      "floor too high". **It is the floor.** The test fails at its second
      assertion: after the floor rises, the window closes, and execution then
      retracts below it, `snapshot_at` answers `None` while `history()` still
      contains the view — so the chain has not forgotten deriving it and is not
      unprimed, it simply refuses to answer. The remaining work is the fix.
- [x] Make a view the chain has derived nameable for as long as any unexecuted
      staged block can ask for it, or refuse in a way that names the cause.
      **Implemented and now covered end to end.** The refusal names
      the cause, and `reseed_sortitions_after_retraction` now calls
      `resume_or_capture_below` at the retraction — adopting the saved chain when
      it sits below what the retracted execution needs and re-deriving from the
      capture when it does not, which is what a restart was doing. The capture path
      is optional and set only by the two production wirings, so rigs that do not
      set it keep today's behaviour.

      The regression discriminates, checked by disabling the fix and re-running:
      without it the test fails with "the derived sortition chain was left at burn
      453, above the burn 423 the retracted execution stands on"; with it the chain
      is re-seeded at burn 422 and the test passes. That is the acceptance
      criterion's "fails on the current code and passes after the fix", verified
      rather than asserted.
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
- [x] Report the condition through `/health` and `/nano/sync_status`. A node that
      cannot advance must not answer `ready: true` with a null error; the hold
      harness and any supervisor read those and both were blind here.
      **Done for `/health` (ad250086, acb959d6).**
      `Tracker::window_closed_below` separates a view this chain derived and
      dropped from one it has not walked to, leaning on the consensus-hash history
      never being front-pruned. The executor holds why it cannot progress, cleared
      by any block executing, and the follower's `snapshot()` turns that into
      `ready: false` with the reason as `last_error` — in `snapshot()` rather than
      at each call site, because every publisher needs it and none of them can know
      it. Readiness now means "can still make progress" rather than "is running".
      Consensus behaviour is untouched.

      **Done for `/nano/sync_status` too (8a16559d).** `SyncStatusWire` gained
      `cannot_progress`, published beside the sealed tip because that is the pair a
      reader needs — a height, and whether it can still move. Both surfaces now
      answer the question that every other field on them obscures.
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

## Traced further, 2026-08-24: two candidates ruled out

**The retraction path already knows about this.** `resume_from_common_ancestor`
calls `persist_sortitions_for_restart()` *before* the chainstate write, and that
writes a seed at `bitcoin_height() - 1` with the reason stated exactly: "A branch
can replace the first block of the current sortition without a Bitcoin
reorganization; its common parent then stands one burn block lower. Saving exactly
at execution made a restart unable to reach that parent because a sortition chain
only walks forward." Its failure message is "the sortition chain could not retain
the burn view needed to restart this Stacks fork".

So the durable half of the fix is already there, and that is precisely why a
restart cures the stall: the next start seeds from that saved point. What is
missing is the in-process half — after retracting, the executor keeps the same
in-memory chain, with the floor still raised and the snapshots below it already
dropped.

**Ruled out, so nobody repeats them:**

- `Tracker::remember_elected_heights` looks like the repair and is not.
  `track_sortitions` calls it, and its comment does describe this shape of bug
  ("a staged block standing lower stops execution — mainnet at 8,712,512, asked
  about burn 961,320 from a chain seeded at 961,342"). But it feeds
  `seed_sortitions_below_window`, which serves the accumulated-coinbase walk over
  burn blocks that elected somebody. It does not make
  `height_of_consensus_hash` answer for a view below the window, which is the
  symptom here.
- Lowering the floor after the fact cannot work at the `SnapshotChain` layer, as
  recorded above: nothing there can re-derive a dropped snapshot.

**So the fix is one of two, both crossing a module boundary:**

1. Re-seed the in-memory tracker from `sortition_state` after a retraction — the
   same thing startup does. **Correction: this is not blocked on a cross-crate
   constructor.** `SortitionTracker::resume_or_capture` and
   `resume_or_capture_below` live in `nano-follower/src/sortition.rs`, the
   executor's own crate, and the second exists for *exactly* this failure class:
   "the saved chain is seeded at burn {seeded_at}, above the burn view
   {executed_burn_view} execution has reached, and a chain only walks forward",
   recorded against a live mainnet state that no restart could escape (executed tip
   needing burn 961,447, saved chain ending at 961,450). Startup checks this; a
   running node has no equivalent check, which is the whole gap.

   What the executor lacks is only the *capture* path — it stores
   `sortition_state` but not the capture that `resume_or_capture_below` falls back
   to. For the common case that may not matter: the retraction path saves at
   `bitcoin_height() - 1`, deliberately below execution, so reloading the saved
   form alone would already yield a usable chain. The capture fallback is needed
   only for a retraction deeper than one burn block, which is the same unanswered
   question as candidate 2.
2. Never raise the floor beyond what a retraction can undo, which requires the
   bound the objective already questions: `execution_rollback_floor` subtracts
   `MINING_COMMITMENT_WINDOW`, and that bounds a Bitcoin reorganisation, not how
   far a Stacks fork retraction moves the burn view.

Option 1 is the smaller change and reuses a path that is already exercised on
every start. It was not attempted here rather than attempted badly: it moves a
constructor across a crate boundary in consensus-adjacent code, and the release
subject was mid-catch-up at the time.

## The open question is answered, and it picks the fix

How far can a retraction move the burn view? **It is not bounded by a constant.**
`switch_to_staged_branch` resumes at whatever ancestor `staging.descent_resumes_at()`
names, and the comment beside it records a mainnet case with 1,509 blocks staged and
none executable. So a retraction can give back thousands of Stacks blocks, spanning
many burn blocks.

That rules out candidate 2. Bounding the floor by the deepest possible retraction
means retaining the whole staging span, which is the leak the window exists to
prevent — `SNAPSHOTS_KEPT` is 144 and staging can be an order of magnitude wider.
`execution_rollback_floor`'s subtraction of `MINING_COMMITMENT_WINDOW` is not
merely too small; no fixed number is right.

**So candidate 1 is the fix, and the capture fallback is required rather than
optional.** After a retraction the executor should rebuild its tracker exactly as
startup does, via `SortitionTracker::resume_or_capture_below(state, capture,
executed_burn_view)`: that adopts the saved chain when it sits below the retracted
execution — which the `bitcoin_height() - 1` seed covers for a one-block fork — and
falls back to re-deriving from the capture when the retraction went deeper, which
is the case a fixed floor can never cover.

What that needs, and all it needs:

- the executor to hold the capture path beside `sortition_state`. Prefer a small
  setter over widening `track_sortitions`, whose signature has several callers
  including tests.
- the rebuild called from `switch_to_staged_branch` after `stand_on_known_block`,
  where `persist_sortitions_for_restart` has already run.
- a test that retracts across a burn boundary and asserts execution continues
  without a restart. The existing reproduction is at the `SnapshotChain` layer and
  cannot cover this; the retraction path is where the regression belongs, and
  writing that fixture is the remaining work.

## Where the regression belongs, and why it is not a quick add

`execution_stall.rs` is the right home for the reporting half. It already runs the
*shipped binary* against a peer serving a coherent chain and reads all three heights
back over `/nano/sync_status` — built for exactly the class of bug where "an RPC is
only trustworthy about a disagreement". Asserting that a node which cannot advance
stops answering `ready: true` belongs there, beside it.

The obstacle is fixture width, and it is worth stating so the next attempt budgets
for it. Reaching this stall needs the retained window to have closed *above* the
burn view execution needs, and the window is `SNAPSHOTS_KEPT = 144` snapshots. The
captured fixtures these rigs replay span twelve blocks (`fork_retraction.rs`'s
`BLOCKS = 12`, `execution_stall.rs`'s `SERVED_BLOCKS = 12`), so the window never
closes in them at all. Producing the condition needs one of:

- a capture spanning more than 144 burn blocks, driven far enough that the tracker
  runs that far ahead of execution, then retracted; or
- a test-only way to narrow `SNAPSHOTS_KEPT`, so a twelve-block fixture can close
  the window. Cheaper, and it changes a constant that is already documented as "what
  the deepest reader needs plus margin" rather than a consensus rule.

The second is the pragmatic route and is what the fix should be gated on. Until one
of them exists, the reproduction committed at the `SnapshotChain` layer is the only
mechanical evidence, and it cannot cover the executor's behaviour.

## The regression is cheaper than the earlier note said

The note above costed this as needing a capture spanning 144+ burn blocks or a
test-only narrowing of `SNAPSHOTS_KEPT`, on the reasoning that the retained window
has to *close* for the stall to appear. That is true of the stall and not of the
fix, which is what matters for a regression.

`reseed_sortitions_after_retraction` runs on **every** retraction, and its effect
is directly observable without any window closing: before the fix the tracker keeps
whatever seed it had walked to; after it, the tracker is replaced by one seeded
from the saved state at or below the retracted execution's burn view. So the
assertion is "the tracker's tip is not above what execution now stands on", which
holds for any retraction at all.

`follow_path::a_branch_that_parts_at_a_block_is_followed_onto_the_fork` already
drives a real one: it executes a local orphan, serves a heavier byte-exact branch,
and `catch_up` takes the fork through `switch_to_staged_branch`. Extending it, or
adding a sibling beside it, needs two things:

- call `keep_sortition_capture` on the test's executor with the fixture's sortition
  directory. The setter is deliberately optional, so these rigs currently take the
  early return and exercise nothing — that is why the fix is safe to have landed,
  and also why no existing test covers it.
- assert the tracker's tip after the retraction. `derived_sortitions()` is already
  on the executor; whether it exposes enough, or wants a narrow accessor beside it,
  is the only open design question left.

No wider capture, no window parameterisation, no new fixture. And
`derived_bitcoin_height()` already returns the tracker's tip, so no new accessor
either — that open design question is answered.

**One thing does have to be added, though.** That fork test has no sortition
tracker at all: neither it nor `execute_fixture_orphan` calls
`track_sortitions`, so both of the fix's guards return early and there is nothing
to observe. Sortitions have to be set up in it first, and the pattern to copy is
the burnchain-reorganisation test in the same file — it builds a tracker against
`directory.join("capture")`, then asserts

```rust
tracker.consensus_hash_at(retracted_at) == tenures.first().copied()
```

before retracting, with the reason stated: "without this the retraction below could
discard nothing and the test would still pass, because a wrong consensus hash
matches no tenure." A fork regression that skipped that precondition would pass
for the wrong reason, which is worse than not having it.

So the remaining work is: give that fork test a tracker under the same
precondition, call `keep_sortition_capture(directory.join("capture"))`, and assert
`derived_bitcoin_height() <= bitcoin_height()` after the fork is taken. Pre-fix the
tracker stays where it walked, which for this fixture is the burnchain tip and
therefore above execution, so the assertion discriminates.
