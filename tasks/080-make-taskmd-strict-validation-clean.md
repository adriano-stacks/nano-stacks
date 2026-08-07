---
id: "080"
title: "Make taskmd strict validation clean"
status: completed
priority: low
effort: small
type: chore
group: build
dependencies: []
tags: ["tasks", "tooling", "hygiene"]
created_at: 2026-08-07
completed_at: 2026-08-07
---

# Make taskmd strict validation clean

## Objective

Make the executable task plan validate without warnings and detect ambiguous task
IDs across groups.

## Tasks

- [x] Add the missing group, effort and tag metadata reported by strict
      validation without changing task meaning or historical status.
- [x] Check task IDs globally across subdirectories; fail validation or CI on a
      duplicate rather than silently dropping one from reports.
- [x] Document the safe add-and-validate workflow for grouped tasks.
- [x] Run taskmd's dependency and duplicate checks after the metadata cleanup.

## Acceptance Criteria

- `taskmd -d tasks validate --strict` exits zero with no warnings.
- Every task has a unique ID and appears exactly once in `taskmd report`.
- Adding tasks in different groups cannot silently assign the same next ID.

## What it came to

`--strict` exits clean: 81 tasks, zero warnings. The 45 missing groups are filled by
what the tasks *are* rather than by where their files sit -- `build` below 037, which
is where the plan turns from components to mainnet replay, and `mainnet` from 037 on,
matching the `mainnet/` directory. Two tasks were also missing `effort` and `tags`.

The duplicate-id half turned out to be narrower than it looked. `next-id` does read
subdirectories and answers 082 correctly; `deduplicate` reports none. What it cannot
do is *reserve* a number, so two `add` calls that both read the board before either
writes are both told the same one -- which is what happened. That is a workflow
property, not a bug to fix in the data, so it is written down in `tasks/AGENTS.md`
where an agent adding a task will read it.

The three ids that appear twice under `grep` are example frontmatter inside
`AGENTS.md`, `CLAUDE.md` and `TASKMD_SPEC.md`. taskmd is right to ignore them.

## Evidence that opened this task

Strict validation reported 49 metadata warnings. During this audit, consecutive
`taskmd add --group` calls also assigned ID 078 to two tasks, and ordinary
validation did not report the collision.
