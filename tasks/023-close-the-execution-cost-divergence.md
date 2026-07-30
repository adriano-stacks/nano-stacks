---
id: "023"
title: "Close the execution cost divergence"
status: pending
priority: critical
effort: large
type: bug
dependencies: []
tags: ["mainnet", "vm", "costs"]
created_at: 2026-07-30
---

# Close the execution cost divergence

## Objective

The scoreboard's cost row fails at block 22:

```
replay: costs   receipt cost dimensions   21/600
block 22: transaction 0 (e5586509…) cost differs:
  expected (62, 134492, 481082, 19, 1667)
  got      (60, 134664, 366479, 19, 1657)
```

The plan's tripwire said to ship this and fix it after M10, on the grounds that
costs only diverge near a block limit. That reasoning does not survive mainnet.
`crates/nano-chainstate/src/lib.rs:836` enforces `EPOCH_4_BLOCK_LIMIT`, and
SIP-034 per-dimension tenure extends are triggered by these same dimensions. A
`read_length` that is thirty percent low decides differently from stacks-core
about when a block is full and when a tenure must be extended.

## Tasks

- [ ] Reduce the block-22 transaction to the smallest snippet that diverges.
- [ ] Fix the vendored compiler's charging for it and keep the case as a
      regression test.
- [ ] Work forward through the fixtures until the cost row reaches 600/600.
- [ ] Assert per-snippet dimension equality against the interpreter, not only
      block-level acceptance.

## Acceptance Criteria

- The scoreboard's cost row matches its state-root and receipt rows.
- All five dimensions match the interpreter on the crosscheck suite.
- The reduced cases stay in the suite as regression coverage.
