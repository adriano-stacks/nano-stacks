---
title: "Prevent nano-tui queue counter reset panic"
id: "144"
status: completed
priority: high
type: bug
tags: ["tui", "bug", "testing"]
created_at: "2026-08-17"
completed_at: 2026-08-17
effort: small
---

# Prevent nano-tui queue counter reset panic

## Steps to Reproduce

1. Open `nano-tui` while a queue counter has a positive value.
2. Let the node restart or otherwise reset that counter below the opening value.
3. Let the TUI evaluate stalled-node health.

## Expected Behavior

The reset counter is not reported as queue growth and health evaluation continues.

## Actual Behavior

The TUI subtracts the larger opening value from the smaller current value before
checking the comparison and panics with unsigned subtraction overflow.

## Environment

- Component: `nano-tui`
- Source: `crates/nano-tui/src/main.rs:1038`

## Tasks

- [x] Make queue growth subtraction lazy after the comparison.
- [x] Add a regression test for a queue counter reset.
- [x] Pass focused tests, formatting, and clippy without warnings.

## Acceptance Criteria

- A current queue value below its opening value returns no growth.
- A current queue value above its opening value still reports the exact delta.
- `nano-tui` tests and clippy pass.
