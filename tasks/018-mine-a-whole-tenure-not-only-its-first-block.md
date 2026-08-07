---
id: "018"
group: build
title: "Mine a whole tenure, not only its first block"
status: completed
priority: high
effort: medium
dependencies: []
tags: ["hacknet", "miner", "conformance"]
created_at: 2026-07-29
completed_at: 2026-07-29
---

# Mine a whole tenure, not only its first block

## Objective

nano mines the tenure-start block of every tenure it wins, and nothing after it,
so the transactions that arrive during its tenures wait for the next miner. A
tenure that outlives its Bitcoin block also has to be extended.

## Tasks

- [x] Take pending transactions from a peer over HTTP and hold them locally.
- [x] Keep mining blocks through a won tenure under the epoch-4 block limits.
- [x] Extend a tenure once it outlives the budget it started with.
- [x] Assert in the Hacknet verification that a nano tenure carried a user
      transaction.

## Acceptance Criteria

- A tenure nano wins contains more than its tenure-change and coinbase.
- A transfer submitted during a nano tenure is confirmed inside that tenure.
