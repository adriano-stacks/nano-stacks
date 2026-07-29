---
id: "018"
title: "Mine a whole tenure, not only its first block"
status: pending
priority: high
dependencies: []
tags: []
created_at: 2026-07-29
---

# Mine a whole tenure, not only its first block

## Objective

nano mines the tenure-start block of every tenure it wins, and nothing after it,
so the transactions that arrive during its tenures wait for the next miner. A
tenure that outlives its Bitcoin block also has to be extended.

## Tasks

- [ ] Take pending transactions from a peer over HTTP and hold them locally.
- [ ] Keep mining blocks through a won tenure under the epoch-4 block limits.
- [ ] Extend a tenure when the burn view advances without a sortition.
- [ ] Assert in the Hacknet verification that a nano tenure carried a user
      transaction.

## Acceptance Criteria

- A tenure nano wins contains more than its tenure-change and coinbase.
- A transfer submitted during a nano tenure is confirmed inside that tenure.
