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

- [x] Inventory every mutable execution side effect outside the VM transaction,
      including tenure accounting, executed ancestry, tenure-start indexes,
      headers, native effects and emitted results.
- [x] Snapshot or stage those effects so root mismatch, validation failure or
      any execution error restores the exact pre-block in-memory state.
- [x] Persist auxiliary state only after the candidate root and all consensus
      checks accept the block.
- [x] Make returning an error from catch-up incapable of persisting rejected
      candidate state.
- [x] Add a deterministic test that retries the same rejected block many times,
      restarts, and proves memory and disk remain byte-for-byte equivalent to
      the state before the first attempt.
- [x] Exercise a real mainnet tenure-start rejection with non-zero fees and
      native maturity effects repeatedly without persisting the rejected
      tenure.
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
- [x] Make the guard bite on fees, not only on the invariant.

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

## Strong live rollback evidence at the maturity boundary

The next divergence is block 8,673,864, a tenure start with two credits, liquid
and SIP-031 emissions, and a matured-tenure payout. It was rejected roughly 138
times. The durable accounting still reports `started = 251421` and contains no
tenure 251422, so the rejected block no longer leaks this non-zero accounting
state across retries.

That is stronger than the zero-fee regression fixture, but it does not close the
task. The auxiliary accounting already lost 44 older tenure records, and the
deterministic restart test still has to cover every side store named in the
acceptance criteria. Close only after the reconstruction in
[[048-carry-complete-mainnet-tenure-accounting]] and the crash boundary in
[[057-commit-and-recover-accepted-block-state-atomically]] agree with the MARF.

## The rollback is structural now, not remembered

`ChainState` had five fields; four of them were the ones a block mutates outside
the MARF, and rolling them back meant a hand-written tuple that named each field
twice — once to snapshot, once to restore. Nothing made the two lists agree, and
nothing at all made a *new* field appear in either. That is how the fee leak got
in.

Those four fields now live in one `ChainLedger`, and `execute_nakamoto_block`
runs the block against a **clone** of it. The clone is moved into place by
`adopt` only after the block seals; on any error it is dropped unread and the
only rollback left is `vm.abort_block()`. There is no restore step to forget.

`adopt` destructures `Self` exhaustively:

```rust
fn adopt(&mut self, sealed: ChainLedger) {
    let Self { vm: _, ledger } = self;
    *ledger = sealed;
}
```

`ChainState` is down to two fields, so a third one does not compile until its
author has decided
whether it belongs in the VM — where a block's writes are already a MARF
transaction — or in the ledger, where a rejected block cannot reach it. That is
the compile-time guarantee the task asked for; it is not a test, and it cannot be
satisfied by forgetting.

What this makes impossible, and what it does not:

- A rejected block cannot leave *any* field of the ledger behind, including ones
  added later. Retrying it a thousand times is the same as not running it.
- It cannot poison the VM's tenure-start map either: the block header is built
  before the seal but written down after it, so a block that does not seal
  contributes no `tenure_starts` entry (that map is first-write-wins, so a
  rejected block's entry would have fixed the tenure's start height for every
  later block) and no `block_header` row.
- It does not stop a block from writing through `accounting_mut()`, which is
  public and is how a node loads its accounting at startup. Nothing in the
  execution path uses it any more.
- It does not make the *VM's* non-MARF state transactional. `set_checkpoint_height`
  in `check_before_executing` and the burn headers recorded from the sortition
  are still unconditional; both are idempotent per block and neither is
  consensus-visible, but they are not rolled back.

Cost is unchanged: the old tuple already cloned the same three collections per
block, so this is the same O(blocks since checkpoint) copy it always was.

## The guard bites on fees now

`retrying_a_rejected_block_leaves_no_state_beside_the_marf` (unit test in
`nano-chainstate`) rejects the captured block 25 times with a state root it
cannot produce — the same failure point a real divergence hits, after the
transactions have run and the fees have been counted — and asserts the *whole*
ledger is unchanged after each attempt, plus the accounting bytes a restart would
read, the sealed tip, the content root it stands on, and that the rejected block
has neither state nor a recorded header. Then it drops the chainstate, reopens
the directory, and asserts the same of the disk. Then it executes the pristine
block and compares root and ledger against a second chainstate in a fresh
directory that never saw the rejected candidate.

It bites on fees, which the old fixture-based guard did not: the captured block
pays 300 uSTX, and the accounting is seeded with earnings for tenure 120 so that
`add_fees` actually counts (the captured checkpoint names no started tenure, so
nothing moved either way — which is exactly why the old test would have passed
before the fix). Verified by putting the bug back: with `add_fees` writing
through to `self.ledger`, attempt 0 fails with `fees: 300` against `fees: 0`.

"Byte-for-byte" is asserted on the accounting JSON, not on the SQLite files: the
MARF and its side store churn pages for reasons that have nothing to do with
this, so the durable assertions are the tip, the content root, the parent link
and the presence or absence of a header and of block state.

## Still open

- The live mainnet accounting is still the repaired file, not a rebuilt one; the
  44 lost tenure records and the unverified older tenures are
  [[048-carry-complete-mainnet-tenure-accounting]]'s.
- Crash consistency between the sealed root and the accounting file is
  [[057-commit-and-recover-accepted-block-state-atomically]], and its remaining
  half needs changes outside `nano-chainstate` — see the inventory there.
