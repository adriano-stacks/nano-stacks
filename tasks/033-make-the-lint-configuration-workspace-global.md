---
id: "033"
title: "Make the lint configuration workspace-global"
status: pending
priority: low
effort: small
type: chore
dependencies: []
tags: ["notes", "build"]
created_at: 2026-07-30
---

# Make the lint configuration workspace-global

## Objective

From the code review in `notes.md`.

`#![forbid(unsafe_code)]` is repeated at the top of twenty-odd source files. The
workspace already forbids `unsafe_code` in `[workspace.lints.rust]`, so every one
of those attributes is a copy of a rule stated centrally, and a new file that
forgets it looks different without being different.

The review also asks whether the lint set is strict enough to be worth having.

## Tasks

- [ ] Drop the per-file `#![forbid(unsafe_code)]` in favour of the workspace lint.
- [x] Review the allowed pedantic and nursery lints and justify each one that
      stays.
- [x] Confirm every crate opts into `[lints] workspace = true`.

## Acceptance Criteria

- No source file states a lint the workspace already states.
- `cargo clippy --workspace --all-targets` is clean.
- Each `allow` in the workspace lint table has a reason next to it.

## Audit result

Every crate opts in, so the workspace `unsafe_code = "forbid"` covers the whole
tree and the per-file attributes are duplication.

Of the three allowed lints, `module_name_repetitions` fired nowhere at all and
is gone. `missing_errors_doc` and `missing_panics_doc` fire on nine items
between them and stay, with the reason written next to them.

Dropping the twenty-odd `#![forbid(unsafe_code)]` lines is deliberately held
back: it rewrites the first line of nearly every source file, and three agents
are editing those files. It lands as one sweep once their branches are merged.
