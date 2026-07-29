---
id: "016"
title: "Verify nano-stacks against released Hacknet baseline"
status: completed
priority: high
effort: medium
dependencies: []
tags: ["hacknet", "pox5", "interop", "verification"]
created_at: 2026-07-29
completed_at: 2026-07-29
---

# Verify nano-stacks against released Hacknet baseline

## Objective

Run nano-stacks' full verification suite after moving Hacknet to released
PoX-5 dependencies, and resolve any incompatibilities it reveals.

## Tasks

- [ ] Run all workspace tests.
- [ ] Run Clippy with warnings denied for every target.
- [ ] Diagnose and fix any compatibility regressions.

## Acceptance Criteria

- All workspace tests pass.
- Clippy reports no warnings for any workspace target.
