---
id: "011"
title: "M10: implement full block execution and fixture replay"
status: in-progress
priority: critical
effort: large
dependencies: ["008", "009", "010", "075"]
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
- [x] Enforce post-conditions and transaction-level runtime error semantics.
- [x] Carry block-level cost limits and receipt costs/events through execution.
- [x] Replay captured blocks from the checkpoint and report the exact first divergence.
- [ ] Restore the regressed bounded replay under
      [[075-make-the-consensus-scoreboard-an-authoritative-gat]].

## Acceptance Criteria

- Every captured block has matching transaction receipts, including status, costs, and events.
- Every replayed header has the matching `state_index_root`.
- The scoreboard reports replay depth and the exact first divergence.

## Regression status

The original implementation reached 340/340, but M10 is not currently green. On
2026-08-07 replay stopped at 75/340 on a transaction-status difference while the
scoreboard exited zero. Task 075 owns the regression and the authoritative gate;
this milestone remains in progress until that task passes.
