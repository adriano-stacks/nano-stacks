---
id: "011"
group: build
title: "M10: implement full block execution and fixture replay"
status: completed
priority: critical
effort: large
dependencies: ["008", "009", "010", "075"]
tags: ["m10", "replay"]
created_at: 2026-07-27
completed_at: 2026-08-09
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
- [x] Restore the regressed bounded replay under
      [[075-make-the-consensus-scoreboard-an-authoritative-gat]].

## Acceptance Criteria

- Every captured block has matching transaction receipts, including status, costs, and events.
- Every replayed header has the matching `state_index_root`.
- The scoreboard reports replay depth and the exact first divergence.

## Regression status

The task-075 gate is authoritative and green on the current compiler tree.
`cargo xtask scoreboard` reports 340/340 matching state roots, receipts and all
five cost dimensions, plus 500/500 frozen mainnet receipt digests. Retained
output is `/tmp/scoreboard-task068-final.txt`, SHA-256
`8546a5eb0791f642f5dfec3a077d9b2941c0c6443990bf4304e7eab790dfbef0`.
