---
title: "Refuse an unrecoverable checkpoint winner seed before sortition"
id: "083"
status: completed
priority: critical
effort: medium
type: bug
group: mainnet
dependencies: ["049", "051"]
tags: ["mainnet", "sortition", "checkpoint", "consensus", "release"]
created_at: "2026-08-07"
completed_at: 2026-08-09
---

# Refuse an unrecoverable checkpoint winner seed before sortition

## Objective

A checkpoint seed that says its burn block elected somebody must carry the winning
VRF seed or let the node recover one unambiguous seed from that block's eligible
commitments. Today `SortitionTracker::prime` logs that it will sample subsequent
sortitions against zero when those commitments disagree, then returns success.
That turns a contradictory checkpoint into a locally derived but wrong winner
chain instead of the typed startup refusal `plan.md` requires.

## Tasks

- [x] Make priming return a typed `TrackerError` when an elected seed snapshot
      carries no winner seed and its eligible commitments do not agree on one.
- [x] Distinguish the valid cases explicitly: a saved chain carrying its effective
      winner seed, a captured block with one unanimous recoverable seed, and a burn
      block that elected nobody and therefore must carry the older effective seed.
- [x] Propagate the error through `resume_or_capture_below` and runtime startup;
      do not persist a derived snapshot or execute a Stacks block after it.
- [x] Make `validate-fixtures` and `release-report` reject the same contradictory
      seed before presenting any replay or artifact result as evidence.
- [x] Add adversarial fixtures for disagreeing commitments, no eligible
      commitment, a sortition-less seed missing its effective predecessor seed,
      and a valid unanimous recovery control.
- [x] Remove the "sample against zero" continuation and prove that no default seed
      remains reachable from checkpoint input.

## Acceptance Criteria

- Every checkpoint seed either supplies or deterministically proves the exact seed
  mixed into the next sortition.
- Missing or contradictory winner-seed evidence causes typed startup refusal before
  synchronization, persistence or execution.
- No input error can select a leader key by sampling against an all-zero seed.
- The bounded replay and a saved-chain restart still derive the same winners,
  consensus hashes and sortition identifiers as their oracle.

## Evidence that opened this task

At `95a17add`, `SortitionTracker::prime` handles
`unanimous_winner_seed(&block) == None` by printing that every later sortition will
"sample against zero", then calls `engine.prime` and returns `Ok(())`. The failure
is therefore visible in logs but not in control flow, despite the checkpoint
completeness rule requiring contradictory inputs to refuse startup.

## Resolved 2026-08-07

Two halves, and the first is why the second was reachable.

**The seed row already names its winner.** Recovery required every eligible
commitment in the seed's burn block to agree on a `new_seed`, and gave up when
they did not — but `snapshots.json` states `winning_block_txid`, and *that*
commitment's own `new_seed` is the seed the next sortition mixes. Exact, and
needing no agreement between candidates. `winner_seed` reads it, falling back to
unanimity only for a winning transaction this node did not decode as an on-time
commitment. The checked-in capture is exactly the case that was being given up
on: its seed block's commitments disagree, and its winner is named.

**And it refuses rather than reports.** Priming used to print that it would
"sample against zero" and carry on, which names miners that did not win and only
surfaces hundreds of blocks later as their tenures' proofs being refused. It now
returns `TrackerError::Seed` naming the commitment and saying a checkpoint has to
carry `winner_vrf_seed` for a seed row that elected somebody.

Before this the capture's derived consensus hashes diverged from the chain's at
burn 364; after it they match every block from 361 to 479. Pinned by
`pox_boundary`, which cannot pass without it.

## Closure evidence — 2026-08-09

- `SortitionTracker::recover_seed` and `advance` return `TrackerError::Seed`
  before advancement when no effective seed is recoverable. Runtime checkpoint
  construction propagates `recover_seed(...)?` before returning a tracker to the
  host, so the failing tracker cannot be saved or used for execution.
- `cargo test -p nano-conformance --lib
  a_capture_with_an_absent_winner_and_disagreeing_candidates_is_rejected`
  passed with a decoded captured Bitcoin block whose eligible commitments
  disagree and whose named winner is absent. Validation returned the typed
  `InvalidSortitionSeed` refusal; the unmodified captured seed remained the valid
  control.
- `cargo test -p xtask --test release_report
  an_invalid_checkpoint_seed_stops_before_replay_or_artifact_evidence` passed.
  The CLI named the invalid seed and printed no artifact, scoreboard, or gate
  evidence after fixture validation failed.
- The focused sortition regressions cover a named eligible winner, unanimous
  fallback, disagreeing candidates, no eligible commitment, a sortition-less
  capture without its predecessor seed, and refusal to advance without an
  effective seed. The bounded `pox_boundary` oracle remains green across burns
  361 through 479.
- `cargo clippy -p nano-conformance -p nano-vm -p xtask --all-targets -- -D
  warnings` passed with the release validation path included.
