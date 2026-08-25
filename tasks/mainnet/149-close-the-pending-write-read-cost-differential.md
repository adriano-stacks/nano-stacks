---
id: "149"
title: "Close the read-cost differential on values already written this transaction"
status: pending
priority: critical
effort: medium
dependencies: []
tags: ["mainnet", "vm", "costs", "liveness", "release"]
created_at: 2026-08-25
type: bug
---

# Close the read-cost differential on values already written this transaction

## Objective

A mainnet transaction that mainnet executed for `read_count` **7** is charged
`read_count` **303,863** here — more than ten times the epoch-4.0 block limit of
30,000 — so it aborts with `CostBalanceExceeded` instead of the
`abort_by_post_condition` the network recorded. The receipt differs, the block's
state root differs, and execution stops.

This is a hard consensus differential of exactly the kind the plan's
**STRENGTHENED — exact receipts and costs** amendment says blocks release, and it
is currently the wall both live mainnet nodes are stopped against.

## What was observed

The release subject (`nano-stacks-follower-0.1.0-88920833e521`,
`/home/aldur/release-subject-88920833`) has refused Stacks **8,832,029** since
2026-08-24 20:06, after its stall supervisor gave up at twenty restarts:

```
state root mismatch at height 8832029: tenure start false, 4 transactions,
  4 receipts, Bitcoin height 963864, tenure height 253795
  receipt 8979c764c3503eca8ab58fc8b42d4eb7bb74d456e42f344acaf90017ca694cc2
    RuntimeFailure("RuntimeCheck(CostBalanceExceeded(
      ExecutionCost { write_length: 18, write_count: 1,
                      read_length: 5491601, read_count: 303863,
                      runtime: 56779882 },
      ExecutionCost { write_length: 15000000, write_count: 15000,
                      read_length: 200000000, read_count: 30000,
                      runtime: 5000000000 })))")
state root mismatch: expected e042574a33b522f5d631822b787a3710bfe192b55a4700c58c6b16a694a574b6,
                     got      484c331a01dfbe557741ad29bdba409e5fb232a8439940ee9eb617d2da0c1cce
```

The block limit printed is correct for epoch 4.0 (`read_count` and `read_length`
doubled from 3.x). The charge is not.

### The canonical record

`8979c764…` is a contract call on
`SM1793C4R5PZ4NS4VQ4WMP7SKKYVH8JZEWSZ9HCCR.stableswap-staking-stx-ststx-v-1-4`,
function `unstake-lp-tokens`, in canonical block 8,832,029. What mainnet charged
it, from the hosted record:

| dimension | mainnet | nano | ratio |
|---|---|---|---|
| `runtime` | 3,480,582 | 56,779,882 | 16.3× |
| `read_count` | **7** | **303,863** | 43,409× |
| `read_length` | 22,193 | 5,491,601 | 247× |
| `write_count` | 1 | 1 | — |
| `write_length` | 18 | 18 | — |

Its real outcome was `abort_by_post_condition`, not a cost failure.

Both write dimensions match exactly. Only the read dimensions and runtime
diverge, which points at how reads are counted rather than at what the function
does.

### The shape the contract gives it

`unstake-lp-tokens` iterates lists declared `(list 12000 uint)`:

```clarity
(define-data-var helper-value uint u0)

(define-map user-data principal {
  cycles-staked: (list 12000 uint),
  cycles-to-unstake: (list 12000 uint),
  lp-staked: uint
})

(define-public (unstake-lp-tokens)
  (let (
    (helper-value-current-cycle (var-set helper-value current-cycle))
    ...
    (filtered-user-cycles-to-unstake
       (filter filter-values-lte-helper-value user-cycles-to-unstake))
    (unstake-data
       (fold fold-cycles-to-unstakeable-cycles filtered-user-cycles-to-unstake ...))
```

The predicate reads `helper-value` per element, and `helper-value` was
`var-set` **earlier in the same transaction**. Two candidate causes, and they
are distinguishable by measurement rather than by reading:

1. **A pending write is charged as a store read.** stacks-core's
   `RollbackWrapper` serves a key already in this transaction's lookup map from
   memory. If nano charges `read_count`/`read_length` for those, a loop over a
   just-written variable bills a store read per element where the network bills
   none. `read_length / read_count` here is 5,491,601 / 303,863 = **18.07
   bytes**, and 18 is exactly this transaction's `write_length` — the serialized
   size of the `uint` that was written. That is the stronger candidate.
2. **A loop runs to the declared capacity.** If `filter`/`fold` iterate the
   list's declared 12,000 rather than its actual length, the element count is
   wrong regardless of what each element is charged.

They are not exclusive and the ratios do not cleanly resolve to either alone
(303,856 / 12,000 = 25.3), so the first step is to measure, not to patch.

## Why it matters

- It is a **receipt and state-root** differential, not a performance question:
  the network committed a post-condition abort and nano commits a cost failure.
- It stops both live nodes. The release subject has been dead on it for a day,
  and the port-20492 node reaches the same block once its fork is retracted
  ([[148-recover-from-an-unnameable-burn-view-without-a-restart]] and the two
  fixes below it).
- Any contract that writes a variable and then loops reading it is exposed, so
  this is not one transaction's problem.

## Acceptance criteria

- [ ] Reproduce the charge offline for `8979c764…` and state which of the two
      causes above it is, with the per-dimension numbers.
- [ ] Crosscheck the reproduction against the interpreter in the rolled-back
      conformance tooling, per **STRENGTHENED — WASM only**: the production
      binary must not gain an interpreter path.
- [ ] Fix the accounting so all five dimensions equal the canonical record for
      this transaction.
- [ ] Regress it against the captured receipt, not only against the state root —
      a root-only check would hide the error identity.
- [ ] Confirm no other cost dimension moved: the existing cost conformance
      suites stay green.
- [ ] Execute canonical block 8,832,029 to the recorded
      `state_index_root e042574a33b522f5d631822b787a3710bfe192b55a4700c58c6b16a694a574b6`
      on a live node.

## Notes

- The transaction, its contract and the canonical costs are all fetchable
  offline; no live node is needed to reproduce.
- `cargo xtask cost-both-tx` and `cargo xtask replay-window` already exist for
  this shape of question.
- Do not raise the block limit to make this pass. The limit printed is right;
  the charge is wrong.
