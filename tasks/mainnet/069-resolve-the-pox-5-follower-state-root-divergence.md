---
id: "069"
title: "Resolve the PoX-5 follower state-root divergence"
status: in-progress
priority: critical
effort: large
dependencies: []
tags: ["mainnet", "replay", "pox5", "conformance"]
created_at: 2026-08-06
type: bug
---

# Resolve the PoX-5 follower state-root divergence

## Objective

Reproduce and close the state-root mismatch that stopped nano while following a
live epoch-4.0 PoX-5 Hacknet at Stacks height 931. Nano expected
`f90f06c9...` from the signed header and sealed `e939a724...` while executing a
two-transfer, mid-tenure block. This is independent evidence from the mainnet
replay and must be localized before signer or PoX-5 release results can count.

## Tasks

- [x] Preserve the checkpoint, burn blocks, Nakamoto blocks, both stock and nano
      observer receipts, node configuration, and compiler identity needed to
      reproduce height 931 offline.
      `/home/aldur/pox5-capture` — checkpoint at 900, blocks 901–1000, burn
      375–399, the stock observer's `new_block` stream for every one of them, and
      the three leader-key registrations the chain's commitments name. Captured
      from the live pox-5 hacknet (`stacks-miner-1`, stacks-node 4.0.1 `026bcbc`),
      whose chainstate is copied to `/home/aldur/pox5-node`.
- [x] Re-run from a pristine parent and establish whether the mismatch is
      deterministic rather than residue from a rejected block or incomplete
      checkpoint. **Deterministic.** From the capture's own checkpoint at 900 the
      replay reaches 30/100 and stops at block 31 = height 931 with exactly the two
      roots this task recorded from the live run:
      `f90f06c983e2c98a… != e939a7249dc9665e…`. Nothing carried over from a
      rejected block: the parent is 930, sealed clean, and the run is repeatable.
- [~] Compare transaction results, all five cost dimensions, events, native
      writes and the ordered MARF journal before changing consensus code.
      **Costs match** — the cost row reports 30/100 with no divergence of its own,
      so the block's five dimensions agree and the receipts row stops only because
      the root check runs first. The write trace (`NANO_TRACE_WRITES`) is captured
      and localizes the block's native effects; see *What 931 turns out to be*. The
      ordered stacks-core journal for this block is the remaining half.
- [x] Use the identical-journal MARF oracle if receipts and writes agree, and
      the clarity-wasm differential oracle if they do not. **Neither was needed.**
      The write trace named four keys, and the *live stacks-core node* answered
      whether it had written them: `/v2/data_var/…/signers/last-set-cycle` is uint
      **19** and `/v2/map_entry/…/cycle-set-height` for cycle 20 is `none`. nano
      wrote cycle 20's signer set at 931 and stacks-core never wrote it at all.
- [~] Fix the owning layer and add the smallest fixture that fails before the
      fix. **The owning layer is named and the fix is measured, and it is not
      landed** — see *The fix, and why it is not in the tree yet*.
- [~] Replay from the original checkpoint through height 931 and at least 100
      later blocks, including the next tenure boundary, with roots and receipts
      matching. With the fix applied the capture replays **100/100 — roots,
      receipts and all five cost dimensions** — from the checkpoint at 900 through
      931 and 69 blocks beyond. It contains no *later* tenure boundary to cross:
      the chain's sortitions stopped at burn 393, which is why 931 exists at all,
      so that half needs a capture from a chain still electing miners.

## The cause, named

`check_and_handle_prepare_phase_start` is driven in stacks-core from
`tenure_block_snapshot.block_height` — the burn block that elected the block's
**tenure** (`setup_block`, `stackslib/src/chainstate/nakamoto/mod.rs:4438`). nano
drives it from the block's Clarity **burn view**.

The two are the same block until a tenure is extended. At 931 they part by seven:
tenure at burn 392, view at burn 399, which is inside cycle 19's prepare phase. So
nano set cycle 20's signer set where stacks-core set nothing, and the live chain
agrees with stacks-core — `last-set-cycle` there is still 19. Four keys of
difference, identical receipts, identical costs, and the roots parted.

## The fix, and why it is not in the tree yet

Carrying the tenure's own burn height beside the view's in `BitcoinBlockContext`
and reading *that* in `prepare_phase_reward_cycle` takes the pox-5 capture from
30/100 to **100/100**, and leaves the in-tree 340-block replay at 340/340. That is
the fix, measured on both.

It is reverted rather than landed because it is a consensus rule whose input has to
be correct on **every** path that builds a burn context, and it is not yet:
`binary_restart` and `execution_stall` stop the node at `reward cycle 24 has no
signer set: nothing stacked for it`. On the in-tree hacknet capture the node's burn
*views* reach 479 — inside cycle 23's prepare phase — while its blocks' tenures do
not, so the set those tests rely on stops being written. Four context-building sites
were wired (`at_height`, the two followed-tenure sites, `LocalSortition::record`)
and it still reproduced, so at least one more path leaves the tenure height stale.

Landing half of it would leave a node whose prepare-phase rule reads a field some
paths do not set, which is worse than the divergence it fixes. The remaining work
is exactly: audit every writer of `BitcoinBlockContext::height` in the production
path, then land the field, the rule and the regression together.



## What 931 turns out to be (superseded by *The cause, named* above)

Height 931 is not an ordinary mid-tenure block. It is the block where the
**Clarity burn view jumps seven burn blocks in one step**:

```
block 929  tenure_burn 392  view a06c505c  view_burn 392
block 930  tenure_burn 392  view a06c505c  view_burn 392
block 931  tenure_burn 392  view b841c9f1  view_burn 399   <-- +7
```

Every earlier block in the capture advances its view one burn block at a time. 931
carries a tenure **extend** whose `burn_view_consensus_hash` names burn 399, so in
one block the view crosses burn 395 — the first block of cycle 19's prepare phase
(length 20, prepare 5). The write trace shows what that makes the block do: it
writes `.signers`' `cycle-set-height`, `cycle-signer-set` and
`stackerdb-signer-slots-0` for cycle **20** (`…0014`), on top of the transfers.

So the divergence is in a *native effect at a skipped-over reward-phase boundary*,
not in the two transfers — which is consistent with the costs agreeing exactly.

`update_signer_set` is already idempotent per cycle (`last-set-cycle` guards it),
so a double write is ruled out; what remains to be established against
stacks-core's journal is whether the *content* of the set differs, or whether
another per-burn-block effect the skip crossed — `process_stx_unlocks`,
`check_and_handle_reward_start`, the maturity accounting — is applied once for the
destination where stacks-core applies it per crossed burn block.

## A capture bug found on the way, and fixed

The first capture of this block could not even be replayed: it stopped at 931 with
`block Bitcoin view is absent from captured Bitcoin snapshots`. `burn_span` took
the capture's burn window from the blocks' own `consensus_hash` — the sortition
that elected each *tenure* — and a tenure extend moves the view forward while the
tenure's name stays put. The span ended at 393 and block 931 executes at 399.

That silently truncated **any** capture containing an extend, and it reported the
missing fixture in the words of a divergence. `burn_span` now unions the tenure
names with `nakamoto_block_headers.burn_view`, which is stacks-core's own answer
for the same question. Re-capturing with the fix moved the span to 375–399 and the
replay from "view absent" to the real root mismatch — which is why the two roots
above could be measured at all.

## What bounds the search

The in-tree `fixtures/` capture is a *different* chain (hacknet commit `bf821e9d`,
checkpoint 460, blocks 461–800) and reaches no height 931; the reproduction above
comes from a fresh capture of the live pox-5 chain instead. Three things bound
where the fault can be:

- The 340-block capture replays with **roots, receipts and all five cost
  dimensions matching**, and it contains a real pox-5 stake window —
  `pox_five_replay.rs` diffs every `stx_lock_event` the chain published against
  nano's own handler. So the divergence is not a general pox-5 locking fault; it
  is specific to that chain, that height, or that node's state.
- The `NANO_MAINNET_STATE` scoreboard row and the frozen mainnet receipt
  regression (500/500) both stay green, so it is not the mainnet compiler
  frontier either.
- The evidence note below still holds and is the reason this blocks 052 and 053:
  a follower frozen at 931 is not successful live interoperability, and the later
  `/v3/sortitions/latest_and_last` failures in that run are symptoms of a stale
  executed tip.

The first item is therefore the whole task for now: stand the PoX-5 Hacknet back
up, capture height 931 with its checkpoint, burn blocks, both observers' receipts
and the compiler identity, and the rest becomes offline work.

## Acceptance Criteria

- The original height-931 block executes from a clean parent to the signed
  `state_index_root`, with matching receipts and events.
- The cause is named and pinned by an offline regression that distinguishes VM,
  native-effect, checkpoint and MARF faults.
- A clean PoX-5 follower remains advancing beyond the failing block and through
  a tenure boundary after restart.
- The signer/RPC and release reports no longer treat a follower frozen at 931 as
  successful live interoperability.

## Evidence that opened this task

The live stock-signer run under
[[052-wire-the-complete-rpc-and-event-surface-into-the-n]] exercised the RPC
surface but froze nano's executed view at this mismatch. Consequently later
`/v3/sortitions/latest_and_last` failures in that run are symptoms of a stale
executed tip, not independent RPC-shape failures.
