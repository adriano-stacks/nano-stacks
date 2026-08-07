---
id: "075"
title: "Make the consensus scoreboard an authoritative gate"
status: pending
priority: critical
effort: large
type: bug
group: mainnet
dependencies: ["060"]
tags: ["consensus", "replay", "conformance", "release"]
created_at: 2026-08-07
---

# Make the consensus scoreboard an authoritative gate

## Objective

Restore the bounded fixture replay and make every red scoreboard surface fail the
command. A table that reports a consensus divergence and exits zero is not a
gate.

## Tasks

- [ ] Minimize and fix the block-76 transaction-status divergence that currently
      stops state-root, receipt and cost replay at 75/340.
- [ ] Restore 340/340 equality for state roots, receipt status, costs, events and
      writes without weakening the oracle or editing expected output to match
      nano.
- [ ] Return a non-zero exit status when any required scoreboard surface fails.
      Loading the manifest successfully is not a passing replay.
- [ ] Make `release-report` consume the scoreboard result rather than treating
      its printed table as evidence independent of success or failure.
- [ ] Add a command-level regression that corrupts one expected receipt or root
      and asserts that both `scoreboard` and the release gate fail.
- [ ] Run the complete release conformance suite and close the event-observer,
      PoX-5 replay, kill-during-replay and write-journal failures exposed by the
      same regression.

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
