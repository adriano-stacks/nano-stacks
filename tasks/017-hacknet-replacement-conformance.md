---
id: "017"
title: "Run nano-stacks as a Hacknet replacement"
status: in-progress
priority: critical
effort: large
dependencies: []
tags: ["hacknet", "conformance"]
created_at: 2026-07-29
---

# Run nano-stacks as a Hacknet replacement

## Objective

Make a clean Core-main Hacknet run reliably with one stock signer or miner
replaced by nano-stacks, and make that run reproducible as conformance coverage.

## Tasks

- [x] Identify and fix the Core-main Hacknet stall.
- [x] Add a repeatable replacement harness.
- [x] Replace a signer and verify continued canonical block production.
- [x] Replace a miner and verify continued canonical block production.
- [ ] Run the harness in the test suite and document its prerequisites.

## Acceptance Criteria

- A fresh isolated Hacknet reaches and continues past PoX-5.
- Replacing either supported role with nano-stacks does not stop block production.
- The harness fails clearly on a stalled network or invalid nano state.
