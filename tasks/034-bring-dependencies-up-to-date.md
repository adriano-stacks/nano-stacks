---
id: "034"
title: "Bring dependencies up to date"
status: pending
priority: low
effort: small
type: chore
dependencies: []
tags: ["notes", "build"]
created_at: 2026-07-30
---

# Bring dependencies up to date

## Objective

From the code review in `notes.md`: dependencies are not on their latest
versions.

Two of them are load-bearing and deserve a decision rather than a bump:
`wasmtime`, which W6.6 wanted off 15.0.0, and the pinned stacks-core revision
`efc34a07`, which is what every conformance oracle compares against.

## Tasks

- [ ] List which direct dependencies are behind and by how much.
- [ ] Bump the ones that are a bump.
- [ ] Decide what to do about `wasmtime` and record why.
- [ ] Decide how the pinned stacks-core revision is chosen and moved.

## Acceptance Criteria

- Direct dependencies are current, or their pin has a stated reason.
- The full test suite and the scoreboard are unchanged by the bump.
