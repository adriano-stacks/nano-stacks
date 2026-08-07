---
id: "008"
group: build
title: "M7: implement bit-exact MARF and checkpoint import"
status: completed
priority: critical
effort: large
dependencies: ["002"]
tags: ["m7", "marf"]
created_at: 2026-07-27
completed_at: 2026-07-27
---

# M7: implement bit-exact MARF and checkpoint import

## Objective

Reproduce Stacks-Core MARF roots for fresh writes, forks, copy-on-write, and
PCS checkpoint imports without a production Stacks-Core dependency.

## Tasks

- [x] Match all node and leaf consensus preimages.
- [x] Match insert, promotion, fork, and copy-on-write root sequences.
- [x] Import the captured PCS checkpoint and verify its published root.
- [x] Extend the imported checkpoint with a reference-verified write.

## Acceptance Criteria

- Randomized fork/copy-on-write scripts match Stacks-Core after every write.
- The captured PCS checkpoint root and its first extension match Stacks-Core.
