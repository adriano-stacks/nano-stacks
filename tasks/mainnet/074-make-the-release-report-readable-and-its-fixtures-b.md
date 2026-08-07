---
id: "074"
title: "Make the release report readable and its fixtures self-describing"
status: pending
priority: high
effort: medium
dependencies: []
tags: ["mainnet", "conformance", "release", "tooling"]
created_at: 2026-08-07
type: bug
group: mainnet
---

# Make the release report readable and its fixtures self-describing

## Objective

`cargo xtask release-report` is the artifact task 053 decides on. It is currently
correct and unreadable, and an unreadable gate is one nobody checks.

A run today prints **hundreds of lines** of `tenure at burn N carries a coinbase
proof this node cannot check` and `tenure at burn N commits a seed this node
cannot check` — one pair per tenure of the captured Hacknet fixture replay —
straight into the middle of the report, between `checkpoint provenance` and the
scoreboard table. The scoreboard itself is six lines. Somebody reading the report
to make a release decision has to filter the noise out to find the decision.

Worse than ugly: those lines *are* real diagnostics on a live node, and a reader
who learns to skip them on the report learns to skip them everywhere.

## Tasks

- [ ] Stop the fixture replay's per-tenure diagnostics from reaching the report's
      standard output. The replay does not have leader-key registrations for a
      captured chain and never will, so this is expected for that oracle and is
      not for a node run — count them and print the count, or route them where a
      reader can ask for them.
- [ ] Do not silence the same messages on a node. What is expected in a fixture
      replay is a missing checkpoint input in production, and
      [[070-carry-leader-key-history-into-proposal-validation]] is exactly that
      distinction.
- [ ] Say what the report cannot say more prominently than what it can. The
      "what this report does not say" section is last and correct; the reader who
      stops at the first green line never reaches it.
- [ ] Give `validate-fixtures` a stronger claim than "valid for 340 replay
      blocks": which capture, taken from which stacks-core revision, seeding which
      consensus-hash history. A capture that cannot seed a chain was shipped
      before ([[038-recapture-the-fixtures-from-the-pinned-revision]]) and was
      only found when a test needed it.
- [ ] Re-check the frozen mainnet receipt slice's binding after every re-freeze.
      It names compiler `sha256:1813d530…` today against an artifact built by
      `sha256:a544b056…`, which is what a baseline is for — but the report should
      state the pair rather than leave it to a test's stderr.
- [ ] Keep the 21 unrunnable gates named individually. That part is right and is
      the reason the report is trustworthy; nothing here should compress it into
      a number.

## Acceptance Criteria

- A release report fits a reader's screen down to the scoreboard, with no
  per-tenure fixture diagnostics in the body.
- The same diagnostics still appear, unchanged, when a node lacks the leader-key
  registrations in production.
- `validate-fixtures` names the capture, its stacks-core revision and whether its
  consensus-hash history can seed a chain.
- The report states the frozen slice's compiler and the artifact's compiler
  side by side.
- No gate that could not run is reported as anything other than "could not run".

## Evidence that opened this task

A `release-report` run on 07c853d1: 226 conformance assertions pass, 21 gates
report they could not run, and roughly 200 lines of fixture tenure warnings sit
between the artifact digest and the scoreboard. `5b532421` and `6383fc82` are the
most recent instance of a fixture whose incompleteness was invisible until
something needed it.
