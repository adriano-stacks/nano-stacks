---
id: "056"
title: "Roll back everything a rejected block touched"
status: in-progress
priority: critical
effort: medium
type: bug
group: mainnet
dependencies: []
tags: ["mainnet", "chainstate", "correctness"]
created_at: 2026-08-03
---

# Roll back everything a rejected block touched

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
