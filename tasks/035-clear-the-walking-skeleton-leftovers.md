---
id: "035"
title: "Clear the walking-skeleton leftovers"
status: pending
priority: low
effort: small
type: chore
dependencies: []
tags: ["notes"]
created_at: 2026-07-30
---

# Clear the walking-skeleton leftovers

## Objective

From the code review in `notes.md`: comments referring to `M0` do not mean
anything to someone reading the codebase.

The stubs M0 wired up are still exported, and the milestone numbers are still in
the doc comments:

- `nano_vm::execute_stub` (`crates/nano-vm/src/lib.rs:632`)
- `nano_chainstate::append_stub` (`crates/nano-chainstate/src/lib.rs:1876`)
- `crates/nano-vm/src/lib.rs:51`, `crates/nano-chainstate/src/lib.rs:47`,
  `crates/nano-conformance/src/lib.rs:23`
- an empty `crates/nano-chainstate/examples` directory

## Tasks

- [ ] Remove the stub functions and anything that still calls them.
- [ ] Rewrite the doc comments to describe the code instead of the milestone
      that produced it.
- [ ] Remove the empty examples directory.
- [ ] Check the same for `plan.md` and milestone references elsewhere in the
      source.

## Acceptance Criteria

- No milestone identifier appears in a source comment.
- Nothing named `stub` is exported.
- The tests and the scoreboard are unchanged.
