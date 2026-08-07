---
title: "Eliminate unaccounted ignored and conditional release tests"
id: "085"
status: in-progress
priority: critical
effort: large
type: bug
group: mainnet
dependencies: ["074", "075"]
tags: ["mainnet", "conformance", "ci", "release", "gates"]
created_at: "2026-08-07"
---

# Eliminate unaccounted ignored and conditional release tests

## Objective

No required behavior may disappear behind `#[ignore]`, `skip_gate`, a missing
`NANO_*` input or a reason-string heuristic. Infrastructure tests may need a
different job, but the release decision must either run them successfully or fail
while naming the missing input. Semantic, consensus and required interoperability
tests may not be waived.

## Tasks

- [ ] Build a machine-readable inventory of every `#[ignore]`, `skip_gate`,
      conditional fixture lookup and environment-gated assertion in the workspace
      and vendored compiler tests the workspace owns.
- [ ] Give every entry an explicit class, owner task, required environment and
      release policy. Do not infer semantic versus infrastructure from substrings
      such as `needs to be implemented`.
- [ ] Remove every ignored semantic, cost, receipt, state, consensus and
      interoperability differential; a known unequal answer must remain a failing
      test until fixed, not two accepted expectations.
- [ ] Run infrastructure-only tests in named CI/release jobs with their fixtures or
      services. A missing input must produce `could not run` and a failed release,
      never a green test count.
- [ ] Add a source-level gate that rejects a new ignore or conditional skip unless
      it is added to the inventory with an open owner task and explicit policy.
- [ ] Reconcile the current ignored Hacknet, hosted-signer, miner, sync, Clarity
      cost and block-info tests; record which ordinary CI job and which release job
      exercises each one.
- [ ] Make the release report consume the inventory and fail when any required
      entry did not run or any semantic entry still exists.

## Acceptance Criteria

- The release report accounts for every test that did not execute, by exact test
  name and owner task.
- No semantic or required conformance test remains ignored or conditionally green.
- Every infrastructure-only test has a reproducible command and a mandatory job
  that supplies its prerequisites before task 053 can complete.
- Adding an unowned `#[ignore]`, `skip_gate` or equivalent conditional fails CI.
- A release run with any missing required capture, network, signer or tool exits
  non-zero.

## Evidence that opened this task

The audit found ignored tests throughout the workspace, including a Clarity cost
test whose reason says the implementation is missing. `release-report` classifies
that semantic gap as infrastructure because the reason contains `needs to be
implemented`. Normal conformance also lets conditional mainnet assertions report
green when their capture is absent, while the committed release-report CI command
has no capture and is expected to fail. Reporting these individually is necessary,
but ensuring that required ones actually run needs an explicit gate of its own.
