---
id: "075"
title: "Make the consensus scoreboard an authoritative gate"
status: completed
priority: critical
effort: large
type: bug
group: mainnet
dependencies: ["060"]
tags: ["consensus", "replay", "conformance", "release"]
created_at: 2026-08-07
completed_at: 2026-08-09
---

# Make the consensus scoreboard an authoritative gate

## Objective

Restore the bounded fixture replay and make every red scoreboard surface fail the
command. A table that reports a consensus divergence and exits zero is not a
gate.

## Tasks

- [x] Minimize and fix the block-76 transaction-status divergence that currently
      stops state-root, receipt and cost replay at 75/340.
- [x] Restore 340/340 equality for state roots, receipt status, costs, events and
      writes without weakening the oracle or editing expected output to match
      nano.
- [x] Return a non-zero exit status when any required scoreboard surface fails.
      Loading the manifest successfully is not a passing replay.
- [x] Make `release-report` consume the scoreboard result rather than treating
      its printed table as evidence independent of success or failure.
- [x] Add a command-level regression that corrupts one expected receipt or root
      and asserts that both `scoreboard` and the release gate fail.
- [x] Run the complete release conformance suite and close the event-observer,
      PoX-5 replay, kill-during-replay and write-journal failures exposed by the
      same regression.

## Where this stands, 2026-08-07

**Done.**

- The board reads **340/340** and exits zero. The block-76 divergence was transient
  — an in-flight edit in the vendored compiler while 073 was being worked — and is
  not reproducible on the current tree.
- `scoreboard` now exits non-zero when any required surface diverges.
  `scoreboard_result` answers the table *and* the verdict, and required means the
  captured replay: roots, receipts and costs, because a cost decides block admission
  even where the root matches.
- `release-report` reads that exit status and prints `FAIL` instead of describing
  the artifact as though the replay had passed.
- The command-level regression tampers rather than constructs: it copies the
  capture, flips one receipt's `status` from `success` to `abort_by_response` — the
  exact shape the block-76 regression took — and asserts the command fails, having
  first asserted the untampered tree passes.

The repeat audit narrowed the last claim: the regression calls
`scoreboard_result` directly. It proves the verdict function, but it does not spawn
`cargo xtask scoreboard` and does not assert `release-report` exits non-zero for the
same tampered tree. That is why the command-level bullet remains partial and the
release-report bullet remains open.

**Open, and the number moved the wrong way before it moved the right way.** The
suite was 241/6 when this task was written; it is **235 passed, 12 failed** now.
[[077-remove-peer-derived-consensus-execution-fallbacks]] is why: fifteen rigs
executed under a peer's sortition answer, and with that path gone they had no burn
view. Seeding them from the capture closed eight.

The twelve that remain are one finding, not twelve. They are three rig families --
`catch_up_rounds`, `follow_path`, `execution_stall` -- and the count moves with the
*seed height* of the fixture's consensus-hash history, not with anything else: seeding
at burn 459 leaves seven failing, seeding at 360 leaves twelve, because a lower seed
lets more rigs derive far enough forward to reach a reward cycle boundary and stop
there. No seed makes them all pass, and that is the point --
[[082-cross-a-reward-cycle-boundary-with-a-locally-derive]] is what makes them
passable at all.

The finding underneath is unchanged: the tenure VRF rule now **runs**
on these rigs, where for want of a local chain it was skipped. Three fail it with
`committed seed is not the hash of the parent tenure's VRF proof`, which says the
seeding is wrong rather than the rule is. The capture's history ends at burn 459 and
the anchor executes at 460+; whether the derived winner at those heights is right
needs the capture's Bitcoin blocks walked against stacks-core's own snapshot rows,
which is the next action here and is not guesswork to do blind.

**Answered, and it is not the seeding.** The blocks stand on burn **362**, not 460 --
the first seeding attempt was 98 burn blocks too high. Seeded below it at burn 360
the chain derives forward and executes blocks 462-470, then stops at burn **379**:
one short of the reward cycle boundary at 380, which `SortitionTracker::advance`
refuses to cross because the `PoxId` bit for the opening cycle is unknown and a
consensus hash mixes it. The capture spans five such boundaries.

So these rigs cannot replay locally end to end until a derived chain can cross one,
which is [[082-cross-a-reward-cycle-boundary-with-a-locally-derive]] -- and that is a
release blocker in its own right, because the live follower meets the same refusal at
cycle 141.

## Current closure evidence — 2026-08-09

The intervening reward-cycle, authentication and fixture repairs closed the stale
red-suite account above without weakening the captured oracle:

- `cargo xtask scoreboard` exits zero at **340/340** roots, receipts and costs,
  with the frozen mainnet slice at **500/500**. The retained output is
  `/tmp/scoreboard-current-20260809.log` (SHA-256
  `04018a86e7f657631cf68d666768a5f420e4b4d66bca0c5ecc820ee4fe9eb3d5`).
- `a_red_scoreboard_makes_both_commands_fail` invokes the real `scoreboard` and
  `release-report` commands against one tampered captured receipt and requires
  both exit statuses to be non-zero.
- `report_scoreboard` returns the subprocess verdict and that boolean is part of
  the final release decision; a printed red table cannot be accepted.
- The complete release-profile conformance command passed **277 tests, 0
  failed**, with five explicitly inventoried infrastructure tests ignored, in
  174.78 seconds. Its retained log is
  `/tmp/conformance-release-current-20260809.log` (SHA-256
  `247abc6893f102a3bfb35ed19ba11b92f6c6a5d9d3012e2b205d09fcfd4ec4b4`).

The original block-76 observation was caused by compiling during a concurrent
vendored-compiler edit. It is retained as the motivating failure, while both the
real command gate and the tampered-oracle regression now pin the durable rule.

## Acceptance Criteria

- `cargo xtask scoreboard` reports 340/340 for every required bounded-fixture
  surface and exits zero.
- An intentional root, receipt, cost, event or write mismatch produces the exact
  first divergence and a non-zero exit status.
- The expected fixture is changed only by a documented recapture from the pinned
  stacks-core oracle, never to bless nano's output.
- `cargo test --release -p nano-conformance --test conformance` is green with no
  required replay test ignored.

## Evidence that opened this task

On 2026-08-07 the scoreboard stopped at block 76: transaction
`3f5c51e7e823fff660c028c8a6c737d3534758b03eb114b1cf6035092b6813b8`
expected success and returned `abort_by_response`. The command still exited zero
because `print_scoreboard` returned success after loading the manifest. The full
conformance run reported 241 passed, 6 failed and 4 ignored.
