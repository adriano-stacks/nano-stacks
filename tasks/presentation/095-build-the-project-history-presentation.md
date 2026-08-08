---
id: "095"
title: "Build the project history presentation"
status: completed
priority: high
effort: large
dependencies: []
tags: ["presentation", "history", "metrics", "agents"]
created_at: 2026-08-08
completed_at: 2026-08-08
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

## Verification, 2026-08-08

The code comparison archives nano-stacks at `eac1f89d` and stacks-core at the
dependency revision `efc34a07`, so neither repository's dirty working tree enters
the count. The tree-sitter measurement reports 65,372 and 221,771 production
Rust lines respectively. The token script reproduced 9,378,824,535 recorded
tokens before its fixed cutoff and emits only whitelisted aggregate counters.

`presentation/tools/verify_deck.py` loaded all 23 slides with DNS disabled,
exercised button, keyboard and hash navigation, and found no overflow at
1280×720, 1366×768 or 1920×1080. Its print gate produced 23 pages at 960×540
points. The deck contains local assets only. `taskmd validate --strict` accepted
all 94 task records; the figures in the deck exclude this presentation task.
