---
id: "074"
title: "Make the release report readable and its fixtures self-describing"
status: completed
priority: critical
effort: medium
dependencies: []
tags: ["mainnet", "conformance", "release", "tooling"]
created_at: 2026-08-07
type: bug
group: mainnet
completed_at: 2026-08-08
---

# Make the release report readable and its fixtures self-describing

## Objective

`cargo xtask release-report` is the artifact task 053 decides on. It is currently
neither readable nor sufficient to make that decision: it can classify a missing
semantic implementation as infrastructure, report a stale binary beside current
source metadata and print a red scoreboard without failing.

A run today prints **hundreds of lines** of `tenure at burn N carries a coinbase
proof this node cannot check` and `tenure at burn N commits a seed this node
cannot check` — one pair per tenure of the captured Hacknet fixture replay —
straight into the middle of the report, between `checkpoint provenance` and the
scoreboard table. The scoreboard itself is six lines. Somebody reading the report
to make a release decision has to filter the noise out to find the decision.

Worse than ugly: those lines *are* real diagnostics on a live node, and a reader
who learns to skip them on the report learns to skip them everywhere.

## Tasks

- [x] Stop the fixture replay's per-tenure diagnostics from reaching the report's
      standard output. The replay does not have leader-key registrations for a
      captured chain and never will, so this is expected for that oracle and is
      not for a node run — count them and print the count, or route them where a
      reader can ask for them.
- [x] Do not silence the same messages on a node. What is expected in a fixture
      replay is a missing checkpoint input in production, and
      [[070-carry-leader-key-history-into-proposal-validation]] is exactly that
      distinction.
- [x] Say what the report cannot say more prominently than what it can. The
      "what this report does not say" section is last and correct; the reader who
      stops at the first green line never reaches it.
- [x] Give `validate-fixtures` a stronger claim than "valid for 340 replay
      blocks": which capture, taken from which stacks-core revision, seeding which
      consensus-hash history. A capture that cannot seed a chain was shipped
      before ([[038-recapture-the-fixtures-from-the-pinned-revision]]) and was
      only found when a test needed it.
- [x] Re-check the frozen mainnet receipt slice's binding after every re-freeze.
      It names compiler `sha256:1813d530…` today against an artifact built by
      `sha256:a544b056…`, which is what a baseline is for — but the report should
      state the pair rather than leave it to a test's stderr.
- [x] Keep the 21 unrunnable gates named individually. That part is right and is
      the reason the report is trustworthy; nothing here should compress it into
      a number.
- [x] Remove `needs to be implemented` from infrastructure classification. An
      ignored VM or cost test with that reason is a semantic release failure.
- [x] Report every declared known differential, including non-ignored tests that
      deliberately pin different engine answers under [[060]] and [[068]]. A
      zero ignored-test count is not a zero differential count.
- [x] Consume the explicit ignored/conditional-test inventory from
      [[085-eliminate-unaccounted-ignored-and-conditional-rele]] instead of
      discovering policy from test-name or reason-string heuristics.
- [x] Fail the report when a semantic differential, required ignored test or red
      scoreboard surface exists.
- [x] Build the release binary before reporting it, or require an explicit
      immutable artifact path. Verify the binary's embedded compiler identity
      before pairing it with revision and source identity.
- [x] Describe the artifact accurately: the Clarity interpreter machinery is
      linked as unreachable frontend/ABI code, while no interpreter entry point
      or call edge is reachable from production.

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
- Every known semantic difference is named and makes the report fail, whether it
  is ignored or pinned as two unequal expected answers.
- The artifact digest, embedded compiler identity, source identity and revision
  describe one freshly built binary.
- A red scoreboard or required gate makes `release-report` exit non-zero.

## Evidence that opened this task

A `release-report` run on 07c853d1: 226 conformance assertions pass, 21 gates
report they could not run, and roughly 200 lines of fixture tenure warnings sit
between the artifact digest and the scoreboard. `5b532421` and `6383fc82` are the
most recent instance of a fixture whose incompleteness was invisible until
something needed it.

The repeat audit at `95a17add` confirmed that diagnostic counting landed without
silencing production, but the decision remains unsound: `report_scoreboard` returns
no verdict to `release_report`, `needs to be implemented` is still classified as
infrastructure, the artifact path is read before any build, and the final exit code
depends only on the three later gates. These are unchecked above and keep the task
critical and in progress.

## Reconciliation, 2026-08-08

The report now starts with its limitations, builds `stacks-node` unless an
explicit artifact is supplied, hashes that exact file and refuses it if the
compiler identity is absent. It prints the frozen receipt compiler beside the
artifact compiler, consumes both release inventories, names declared running
differentials and includes the scoreboard, capture validation, artifact and gate
results in its exit status. `NANO_*` inputs remain reproducible without printing
private keys into CI logs.

`validate-fixtures` now validates the production sortition seed path as well as
the fixture hashes. At checkpoint `4a40dee6` it named Hacknet revision
`bf821e9d556eab8c7a30c6e86a7dc1f9b200f1a1`, stacks-core oracle revision
`efc34a07a225c4b950ab9404a1652aa5e14affaf`, 340 replay blocks and 361 consensus
hashes seeding burn height 360 at consensus hash
`567f4551f17e8fbe1c9aa3d68058bf9a7afcb74e`.

The release-inventory conformance tests, `cargo xtask validate-fixtures` and the
command-level tampered-receipt regression all pass at that checkpoint. The last
one spawns both `scoreboard` and `release-report`, and proves the same red receipt
surface makes both commands exit non-zero. Missing required inputs still retain
their individual test names through `classify_failures`; the two parameterized
investigations are explicitly optional diagnostics instead of being counted as
release gates.
