---
id: "080"
title: "Make taskmd strict validation clean"
status: pending
priority: low
effort: small
type: chore
group: build
dependencies: []
tags: ["tasks", "tooling", "hygiene"]
created_at: 2026-08-07
---

# Make taskmd strict validation clean

## Objective

Make the executable task plan validate without warnings and detect ambiguous task
IDs across groups.

## Tasks

- [ ] Add the missing group, effort and tag metadata reported by strict
      validation without changing task meaning or historical status.
- [ ] Check task IDs globally across subdirectories; fail validation or CI on a
      duplicate rather than silently dropping one from reports.
- [ ] Document the safe add-and-validate workflow for grouped tasks.
- [ ] Run taskmd's dependency and duplicate checks after the metadata cleanup.

## Acceptance Criteria

- `taskmd -d tasks validate --strict` exits zero with no warnings.
- Every task has a unique ID and appears exactly once in `taskmd report`.
- Adding tasks in different groups cannot silently assign the same next ID.

## Evidence that opened this task

Strict validation reported 49 metadata warnings. During this audit, consecutive
`taskmd add --group` calls also assigned ID 078 to two tasks, and ordinary
validation did not report the collision.
