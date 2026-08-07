---
title: "Eliminate unaccounted ignored and conditional release tests"
id: "085"
status: in-progress
priority: critical
effort: large
type: bug
group: mainnet
dependencies: []
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

- [x] Build a machine-readable inventory of every `#[ignore]`, `skip_gate`,
      conditional fixture lookup and environment-gated assertion in the workspace
      and vendored compiler tests the workspace owns.
- [x] Give every entry an explicit class, owner task, required environment and
      release policy. Do not infer semantic versus infrastructure from substrings
      such as `needs to be implemented`.
- [ ] Remove every ignored semantic, cost, receipt, state, consensus and
      interoperability differential; a known unequal answer must remain a failing
      test until fixed, not two accepted expectations.
- [ ] Run infrastructure-only tests in named CI/release jobs with their fixtures or
      services. A missing input must produce `could not run` and a failed release,
      never a green test count.
- [x] Add a source-level gate that rejects a new ignore or conditional skip unless
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

## The guess is gone, 2026-08-07

`ignored-tests.toml` is the inventory, keyed by the exact reason text, with a
class and an owner task per entry. `report_differentials` reads it and
`is_infrastructure` — the substring rule this task was opened over — is deleted.
A reason the inventory does not list is `unclassified`, which the report counts
against the release exactly as `semantic` does, so the undecided case cannot be
the quiet one.

What that turned up, by name and line rather than as a count:

```
ignored tests
  infrastructure       18 (a service, network or fixture this machine does not have)
  tools                1  (assert no required behaviour)
  blocking             15 -- each one is a failed release gate
    [semantic]     vendor/.../src/cost.rs:1895: Clarity 4 costs needs to be implemented
    [unclassified] vendor/.../blockinfo.rs (×12): test system needs to be improved
                     relative to versioning and epochs
    [unclassified] vendor/.../blockinfo.rs (×2): block-reward is not simulated in the
                     test framework
```

The first is the one the evidence named: `needs to be implemented` matched the
environment marker list, so a cost differential was filed as a missing machine. A
cost decides block admission even where the state root matches.

The fourteen `unclassified` are honest rather than resolved. Both reasons describe
the *harness* — which may mean they hide nothing, or may mean they hide a
per-epoch differential — and nobody has looked. Classing them by what their prose
sounds like is the mistake being removed, so they are recorded as undecided and
count against the release until somebody decides them. Owner 060.

**The source-level gate is in the mandatory suite.**
`release_inventory::every_ignored_test_is_named_in_the_inventory` fails on any
`#[ignore]` whose reason is not inventoried — verified by adding one and watching
it fail — and `the_inventory_names_no_test_that_is_gone` fails on an entry left
behind after its test was fixed, which is the direction that rots and would let a
later test inherit a waiver silently.

## What is still open

- `skip_gate` is not inventoried yet. It already has the right *mechanism*:
  `NANO_REQUIRE_MAINNET` turns every skip into a failure, so a release run cannot
  report green on gates that did not run. What it lacks is the same per-site
  ownership the `#[ignore]` sites now have.
- The fifteen blocking entries have to reach zero, which is 023's and 060's work
  and not this task's.
- The release report does not yet *exit non-zero* on a blocking count; it prints
  it. That is the last enforcement step.
