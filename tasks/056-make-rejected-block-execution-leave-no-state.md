---
title: "Make rejected block execution leave no state"
id: "056"
status: pending
priority: critical
type: bug
group: mainnet
tags: ["mainnet", "chainstate", "accounting", "persistence"]
created_at: "2026-08-03"
effort: medium
dependencies: ["043"]
---

# Make rejected block execution leave no state

## Objective

Treat execution of a candidate block as a transaction across every mutable part
of `ChainState`, not only the VM/MARF transaction. A block whose expected state
root does not match currently aborts the VM but retains its fees in tenure
accounting. The runtime then writes that accounting after `catch_up` returns an
error.

This is observable in the mainnet state at height 8,665,780. The same rejected
block was attempted 1,417 times with a fee of 458,250 uSTX, and the persisted
tenure total is exactly:

```
24,851 + 1,417 * 458,250 = 649,365,101
```

The MARF is still sealed at 8,665,779, but the accounting beside it is already
wrong. It must be rebuilt from the checkpoint and accepted blocks; subtracting
the observed retries is not a recovery procedure.

## Tasks

- [ ] Inventory every mutable execution side effect outside the VM transaction,
      including tenure accounting, executed ancestry, tenure-start indexes,
      headers, native effects and emitted results.
- [ ] Snapshot or stage those effects so root mismatch, validation failure or
      any execution error restores the exact pre-block in-memory state.
- [ ] Persist auxiliary state only after the candidate root and all consensus
      checks accept the block.
- [ ] Make returning an error from catch-up incapable of persisting rejected
      candidate state.
- [ ] Add a deterministic test that retries the same rejected block many times,
      restarts, and proves memory and disk remain byte-for-byte equivalent to
      the state before the first attempt.
- [ ] Rebuild the live mainnet accounting from the attested checkpoint and
      accepted chain after the fix; do not repair it with a retry-count-specific
      adjustment.

## Acceptance Criteria

- Any rejected block leaves the sealed root, accounting, parent links, headers,
  tenure indexes and emitted effects unchanged in memory and on disk.
- One attempt and 1,000 attempts at the same rejected block produce identical
  durable state, including after restart.
- An accepted retry produces the same root and accounting as uninterrupted
  execution that never saw the rejected candidate.
- The mainnet replay starts from freshly reconstructed accounting and no longer
  grows tenure fees while stopped at a state-root mismatch.
