---
id: "007"
title: "M6: implement sortition"
status: in-progress
priority: high
effort: large
dependencies: ["006"]
tags: ["m6", "sortition"]
created_at: 2026-07-27
---

# M6: implement sortition

## Objective

Reproduce epoch-4 sortition snapshots from Bitcoin operations without a
production dependency on Stacks-Core.

## Tasks

- [x] Implement operation, consensus, and rolling sortition hashes.
- [x] Build the six-block commit distribution and assumed-total-commit rules.
- [x] Select winners and maintain snapshot state across Bitcoin blocks.
- [ ] Replay captured snapshots and compare every consensus-critical field.

## Acceptance Criteria

- Captured snapshots match their operations, consensus hashes, sortition hashes,
  total burns, and winners.
