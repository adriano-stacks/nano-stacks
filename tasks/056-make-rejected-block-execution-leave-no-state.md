---
title: "Make rejected block execution leave no state"
id: "056"
status: in-progress
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


---

# Merged: roll back everything a rejected block touched

Filed separately and the same bug; kept here because [[053]] depends on this id.


## Objective

A node retries a block it cannot execute for as long as it is running. Aborting
the MARF is not a rollback: fees reach the tenure accounting *before* the state
root is checked, that accounting lives outside the MARF, and the runtime
persists it separately — so every failed attempt added the same fees again.

The live evidence was exact. Mainnet block 8,665,780 fails its state root and
carries 458,250 uSTX of fees; after 1,417 attempts the tenure recorded
`24,851 + 1,417 x 458,250 = 649,365,101`. It kept climbing while the node ran,
reaching 657,935,281.

The MARF tip stayed clean at 8,665,779 the whole time, which is what made this
invisible: every root matched, and the state beside the roots was wrong.

## Tasks

- [x] Snapshot every non-MARF field before a block runs and restore it on
      rejection — accounting, tenure start heights, and the executed chain.
- [x] Guard it with a test that rejects the same block many times and asserts
      nothing moved.
- [x] Repair tenure 251323 in the live state directory.
- [ ] Find out whether earlier tenures were inflated by earlier retry loops, and
      rebuild the accounting rather than patching it if so.
- [ ] Make the guard bite on fees, not only on the invariant.

## Acceptance Criteria

- Rejecting a block any number of times leaves the tip, the tenure accounting
  and the executed chain byte-for-byte unchanged.
- The live state directory's accounting matches what its MARF tip implies.

## The guard is weaker than the bug

`rejected_blocks` asserts the invariant and passes, but the captured chain is a
weak witness: its blocks carry no fees, so the accounting a rejection moves
there is zero and the test would have passed before the fix too. The invariant
is the right thing to assert and the test is worth keeping; it is not evidence
that this particular bug is gone.

The strong witness is a mainnet block, and running one needs the real checkpoint
opened — minutes, not a unit test. That version was written and dropped rather
than committed unverified.

## The live state is repaired but not proven

Tenure 251323 is corrected to 1,459,493 uSTX, computed from the 46 blocks of
that tenure nano actually executed (8,665,734 through 8,665,779). The previous
value is kept beside it as `accounting.json.corrupt`.

Every other tenure is **unverified**. The node stalled at other heights earlier
in its life, so the same loop may have inflated them; a spot check of tenure
251252 was attempted and the fee figures it returned were unusable, so nothing
is concluded. Until the whole file is rebuilt from the chain, this state
directory cannot be trusted to mature miner rewards correctly — maturity is 100
tenures, so the affected ones start paying out soon.
