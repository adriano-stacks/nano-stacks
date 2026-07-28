---
id: "011"
title: "M10: implement full block execution and fixture replay"
status: in-progress
priority: critical
effort: large
dependencies: ["008", "009", "010"]
tags: ["m10", "replay"]
created_at: 2026-07-27
---

# M10: implement full block execution and fixture replay

## Objective

Replay captured Nakamoto blocks from the checkpoint, producing the same
Clarity receipts and state roots as stacks-core.

## Tasks

- [x] Execute decoded contract deployments and contract calls through the Epoch 4 VM.
- [x] Execute decoded token-transfer payloads through the Epoch 4 VM.
- [x] Apply transaction fees and account nonces.
- [x] Apply coinbase and tenure changes.
- [ ] Enforce post-conditions and transaction-level runtime error semantics.
- [x] Carry block-level cost limits and receipt costs/events through execution.
- [x] Replay captured blocks from the checkpoint and report the exact first divergence.

## Acceptance Criteria

- Every captured block has matching transaction receipts, including status, costs, and events.
- Every replayed header has the matching `state_index_root`.
- The scoreboard reports replay depth and the exact first divergence.
