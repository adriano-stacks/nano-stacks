---
id: "034"
title: "Bring dependencies up to date"
status: completed
priority: low
effort: small
type: chore
dependencies: []
tags: ["notes", "build"]
created_at: 2026-07-30
completed_at: 2026-07-30
---

# Bring dependencies up to date

## Objective

From the code review in `notes.md`: dependencies are not on their latest
versions.

Two of them are load-bearing and deserve a decision rather than a bump:
`wasmtime`, which W6.6 wanted off 15.0.0, and the pinned stacks-core revision
`efc34a07`, which is what every conformance oracle compares against.

## Tasks

- [x] List which direct dependencies are behind and by how much.
- [x] Bump the ones that are a bump.
- [x] Decide what to do about `wasmtime` and record why.
- [x] Decide how the pinned stacks-core revision is chosen and moved.

## Acceptance Criteria

- Direct dependencies are current, or their pin has a stated reason.
- The full test suite and the scoreboard are unchanged by the bump.

## Where the dependencies stand

Every direct dependency is current except two, and `cargo update` moved the
only four transitive ones that had anywhere to go (`displaydoc`, `http`,
`tokio-macros`, `toml`).

**`rusqlite` 0.31.0, latest 0.40.1.** Deferred, not skipped. It is used in
exactly two places — the Clarity side store in `nano-vm` and the checkpoint
reader in `nano-marf` — and both are being rewritten by
[[021-hold-mainnet-scale-state-on-disk]], which may not keep the same usage or
the same crate. Bumping nine breaking minors underneath that is churn against a
moving target. The file format the checkpoint reads is stacks-core's, not
rusqlite's, so the version gap is an API question only. Bump it once storage
settles.

**`bitcoincore-rpc` 0.19.0** is the current release.

**`wasmtime` 15.0.0** in `vendor/clarity-wasm`. W6.6 wants nano off it. It is
not a bump to make in passing: the vendored compiler is a maintained fork and
its wasm codegen is the thing the cost work in
[[023-close-the-execution-cost-divergence]] is measuring, so moving the runtime
underneath that would confound the one signal telling us whether costs match.
It belongs after the cost row is green, as its own task.

**The pinned stacks-core revision** (`efc34a07`) is what every conformance
oracle compares against, so moving it changes what "matches stacks-core" means.
Two rules now hold it in place rather than convention:

- a fixture capture records the revision it came from in `provenance.toml`
- `cargo xtask capture-fixtures` refuses a node that is not that revision,
  reading the pin out of the lockfile so the check cannot drift

Moving the pin is therefore a deliberate act with a visible consequence: the
fixtures must be recaptured against the new revision, and a capture that was
not will be refused rather than silently believed. That is what went wrong once
already — see [[038-recapture-the-fixtures-from-the-pinned-revision]].
