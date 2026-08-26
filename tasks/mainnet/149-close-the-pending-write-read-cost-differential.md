---
id: "149"
title: "Guard filter over an empty sequence and close the runtime gap it leaves"
status: in-progress
priority: critical
effort: medium
dependencies: ["150"]
tags: ["mainnet", "vm", "costs", "liveness", "release"]
created_at: 2026-08-25
type: bug
---

# Guard filter over an empty sequence and close the runtime gap it leaves

## Objective

A mainnet transaction that mainnet executed for `read_count` **7** was charged
`read_count` **303,863** here — more than ten times the epoch-4.0 block limit of
30,000 — so it aborted with `CostBalanceExceeded` instead of the
`abort_by_post_condition` the network recorded. The receipt differed, the block's
state root differed, and execution stopped.

The cause is found and fixed. `read_count`, `read_length` and both write
dimensions now equal the canonical record; `runtime` does not yet, and that
remainder is what keeps this task open.

## What was observed

The release subject (`nano-stacks-follower-0.1.0-88920833e521`,
`/home/aldur/release-subject-88920833`) has refused Stacks **8,832,029** since
2026-08-24 20:06, after its stall supervisor gave up at twenty restarts. The
port-20492 node reaches the same block and stops the same way, once its fork is
retracted:

```
state root mismatch at height 8832029: tenure start false, 4 transactions,
  4 receipts, Bitcoin height 963864, tenure height 253795
  receipt 8979c764c3503eca8ab58fc8b42d4eb7bb74d456e42f344acaf90017ca694cc2
    RuntimeFailure("RuntimeCheck(CostBalanceExceeded(
      ExecutionCost { write_length: 18, write_count: 1,
                      read_length: 5491601, read_count: 303863,
                      runtime: 56779882 }, ...)))")
```

The block limit printed is correct for epoch 4.0. The charge was not.

## Root cause

`clar2wasm`'s `filter` emitted a **do-while**: the loop body always ran once, and
the end check subtracted an element size from the remaining length and branched
while the result was non-zero.

At length **zero** it therefore read an element that was not there, and left the
length *negative* — so `br_if` kept looping, down through every multiple of the
element size, 268 million iterations for a `uint` before the counter wraps back
to zero. `fold` and `map` already guard their loops on a zero length
(`sequences.rs`); `filter` did not.

It never gets that far, and both endings are consensus outcomes:

- **It traps.** `(filter f <empty stored list>)` fails with an out-of-bounds
  memory access where the interpreter answers the empty list.
- **It burns the budget.** Where the garbage it reads happens to fail the
  predicate, the run returns the *right value* and charges for every iteration
  until a cost dimension is exhausted. That is mainnet 8,832,029: a filter over
  an empty `cycles-to-unstake` on
  `SM1793C4R5PZ4NS4VQ4WMP7SKKYVH8JZEWSZ9HCCR.stableswap-staking-stx-ststx-v-1-4`,
  whose stored record at 8,832,028 holds `cycles-staked` of 60 and
  `cycles-to-unstake` of **0**.

The second ending is why this hid: the value and both write dimensions were
already correct, so nothing but the cost said anything was wrong — exactly the
case the plan's *cost divergence is invisible until a block nears a limit* note
is about.

## Measured, before and after

`cargo xtask cost-both-tx` against the real state at 8,832,028, under that
block's Bitcoin view (burn 963,864):

| dimension | canonical / interpreter | nano before | nano after |
|---|---|---|---|
| `read_count` | 7 | 303,863 | **7** |
| `read_length` | 22,193 | 5,491,601 | **22,193** |
| `write_count` | 1 | 1 | **1** |
| `write_length` | 18 | 18 | **18** |
| `runtime` | 3,480,582 | 56,779,882 | 3,096,207 |
| result | `(ok u0)` | `CostBalanceExceeded` | **`(ok u0)`** |

## What is fixed

Commit `49ed12fd` guards `filter`'s loop on a zero length, the way `fold` and
`map` do. Regression tests in `clar2wasm/src/words/conditionals.rs` assert cost
equality for a filter over an empty stored list and over an empty stored buffer,
and that a short stored list is unchanged. The whole `clar2wasm` suite is green
(1,561 unit tests and 1,200-odd integration tests, no failures).

## What is left

- [x] **Close the `runtime` gap that was `filter`'s.** 384,000 of the 384,375
      was a second `filter` defect, traced with the dual-engine charge trace to
      `cost_lookup_variable_size [192006] -> rt 384013` in the interpreter
      against `[6]` in the compiler. The reference's `filter` mutates its
      argument in place and returns the same value, so the result keeps the
      input's `type_signature` — and a list is sized by `max_len`, not by its
      length. The compiler rebuilt the result from the kept elements and sized
      it by the kept count. Fixed by having the result inherit the input's list
      type (capacity *and* entry type: an emptied list's own entry type is
      `NoType`, sized 1 where a `uint` is 16), through a new
      `save_filtered_runtime_shape` host call taken only when the input was
      widened or something was dropped.
- [ ] **The remaining 375 is [[150]]**, a different defect: a tuple constructed
      from a widened field loses that field's capacity, so `print` of it
      under-charges. Split out rather than folded in here, because the mechanism
      and the sites are different and it predates 149.
- [ ] **Regress the receipt, not only the root.** The canonical receipt for
      `8979c764…` should be a fixture, so a wrong error identity cannot pass on a
      matching root.
- [x] **Deployed to the port-20492 node, by repinning its state.** Editing
      `vendor/clarity-wasm` moved the compatibility fingerprint from
      `6a83746edc16895eb6886c37474ab7693bc31272b5d350366fc4606663965a35` to
      `4741d57fb27317c3385ec1de364f92bd10688d315e1419efc77264ce17b30180`, and a
      node refuses state pinned to the old one. Rather than re-import, the
      20492 node's own pin was moved by hand — the `consensus_profile` row in
      `chainstate/clarity.sqlite` and `profile_fingerprint` in
      `chainstate/checkpoint-provenance.toml` (backup:
      `/home/aldur/mainnet-tip/checkpoint-provenance.toml.bak-6a83746e`).
      It then executed 8,832,029 and caught up to tip. See the caveat below.
- [ ] **Re-attest and re-import for release evidence.** The repin above is the
      operator shortcut, not the sanctioned path: the design has deliberately no
      repin subcommand, because a compiler change means a fresh import. The
      20492 node is therefore **no longer provably one compiler's continuation of
      the attested checkpoint** and must not be presented as release evidence or
      as a receipt witness. The release subject
      (`/home/aldur/release-subject-88920833`) is untouched and still refuses
      8,832,029; it needs the real ceremony
      (`/home/aldur/checkpoint-builder-keys/run-ceremony-*.sh`, then a fresh
      import from the re-issued bundle).

      What makes the repin defensible *for a diagnostic node* and not for
      release: every block this state executed sealed to the canonical
      `state_index_root`, so its ledger is root-identical to the network's under
      either compiler. The bug could only change a value where the garbage it
      read passed the predicate, and such a block would have failed its root
      check and been refused. Cost is not in the root, which is exactly why this
      hid — and exactly why the argument is about this state and not a general
      licence.
- [ ] **Sweep for the same shape.** `filter` was the only unguarded loop of the
      three, but confirm nothing else emits a do-while over a sequence length.

## Live result

The port-20492 node (`nano-stacks 0.1.0-74e82b7bfbad`) executed **8,832,029**
without a root mismatch, then caught up 11,201 blocks and reached chain tip on
2026-08-26. At burn 964,103 it agreed exactly with three independent sources —
two stock 4.0.1 peers and Hiro — on `stacks_tip_consensus_hash`
`f7f64cc8de758fe830662eae0dbb5facd79482c4` and the same `pox_consensus`, and its
tip block `0df975e4…` is confirmed canonical. It has tracked tip since, within
the one-to-two blocks of ordinary propagation latency. Zero state-root mismatches
across the whole 11,000-block run.

## Notes

- The transaction, its contract and the canonical costs are all fetchable
  offline; the reproduction needs only the state at 8,832,028.
- Do not raise the block limit to make this pass. The limit printed is right.
