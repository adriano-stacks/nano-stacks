---
id: "008"
title: "M7: implement bit-exact MARF and checkpoint import"
status: in-progress
priority: critical
effort: large
dependencies: ["002"]
tags: ["m7", "marf"]
created_at: 2026-07-27
---

# M7: implement bit-exact MARF and checkpoint import

## Objective

Reproduce Stacks-Core MARF roots for fresh writes, forks, copy-on-write, and
PCS checkpoint imports without a production Stacks-Core dependency.

## Tasks

- [x] Match all node and leaf consensus preimages.
- [x] Match insert, promotion, fork, and copy-on-write root sequences.
- [x] Import the captured PCS checkpoint and verify its published root.
- [ ] Extend the imported checkpoint with a reference-verified write.

## Acceptance Criteria

- Randomized fork/copy-on-write scripts match Stacks-Core after every write.
- The captured PCS checkpoint root and its first extension match Stacks-Core.
