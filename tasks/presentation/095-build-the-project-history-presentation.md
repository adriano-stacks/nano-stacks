---
id: "095"
title: "Build the project history presentation"
status: in-progress
priority: high
effort: large
dependencies: []
tags: ["presentation", "history", "metrics", "agents"]
created_at: 2026-08-08
---

# Build the project history presentation

## Objective

Build a concise presentation that explains the architecture, measured production
code size, clarity-wasm progress, remaining release caveats and the development
history. Use ASD-STE100-style language and cite local evidence.

## Tasks

- [x] Reconstruct the repository history from the plan, tasks and commits.
- [x] Measure production Rust code in nano-stacks and stacks-core with tree-sitter.
- [x] Count token use in available Claude, Kimi and Codex sessions.
- [x] Record user continuation prompts and agent mistakes with transcript evidence.
- [x] Build an editable web presentation with dry text and useful visuals.
- [x] Verify the measurements, presentation controls and task metadata.

## Acceptance Criteria

- The deck explains the current node architecture and its production boundaries.
- The code comparison excludes Rust tests by syntax, not by file-name heuristics only.
- Token totals state the counting method and the limits of the available logs.
- Claims about agent failures link to an evidence appendix without exposing secrets.
- The deck runs locally without a network connection and fits common 16:9 screens.
