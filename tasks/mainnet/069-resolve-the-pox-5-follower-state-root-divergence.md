---
id: "069"
title: "Resolve the PoX-5 follower state-root divergence"
status: completed
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
- [x] Fix the owning layer and add the smallest fixture that fails before the
      fix. `BitcoinBlockContext` carries the tenure's burn height beside the view's
      and `prepare_phase_reward_cycle` reads it;
      `signers::tests::an_extended_view_does_not_set_up_the_tenures_next_cycle` is
      the minimal case and fails without it. No checkpoint advance and no skipped
      root check.
- [x] Replay from the original checkpoint through height 931 and at least 100
      later blocks, with roots and receipts matching. **100/100 — roots, receipts
      and all five cost dimensions** — from the checkpoint at 900 through 931 and 69
      blocks beyond, and the in-tree 340-block capture stays at 340/340. The
      capture contains no *later* tenure boundary to cross: that chain's sortitions
      stopped at burn 393, which is why 931 exists at all, so that half wants a
      capture from a chain still electing miners.

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

## Closed on the rig that disproved it

The reopening below stands as the record of a close that was made too early, and
this is the close that is not. The same hacknet rig, with the tenure height derived
without a tracker, ran from its sealed height of **930 to 14,417** — about 13,487
blocks, the whole chain to its tip — with **zero state root mismatches**. Block 931
is no longer a frontier; it is thirteen thousand blocks behind the node.

What was missing is now explicit: the node took the tenure's burn height from
`SortitionTracker::height_of_consensus_hash` alone, and a checkpoint that carries no
sortition history seeds no tracker. The tenure is still named by the block's own
`consensus_hash`, so where there is no local chain to ask, its sortition is one
lookup away — and it is only worth asking when the carried burn view has moved off
the tenure, which is what an extend does and which stays true for every block after
it until the next tenure change.

Asking a peer for that height is safe on the same argument the view's sortition
already rests on: it feeds the prepare-phase rule, which decides whether a cycle's
signer set is written, which lands in the state root the block's own header commits
to under threshold signer weight. A peer that lies makes the block fail to seal. It
cannot make this node execute a different chain.

## Reopened: the fix did not reach a node with no sortition chain

Closed too early. Started the hacknet rig this session — the same one whose
`hosted-signer` run produced this task — and it reproduces the original roots
exactly, live:

```
state root mismatch: expected f90f06c983e2c98a…, got e939a7249dc9665e…
executed nothing: sealed at 930, then the round failed
```

The replay path is genuinely fixed (that capture is 100/100). The **node** path is
not, and the reason is that it takes the tenure's burn height from
`SortitionTracker::height_of_consensus_hash` — and that node derives no sortitions
at all: its log has zero `deriving sortitions locally` lines, because its checkpoint
carries no sortition history to seed one from. With no tracker the override never
fires, `tenure_bitcoin_height` stays equal to the view, the prepare phase fires at
burn 399 where the tenure is at 392, and the root parts exactly as before.

So the tenure height must be derivable **without** a tracker, and it is: a block
that carries no tenure change has view equal to tenure, and one that carries an
extend names the view in `burn_view_consensus_hash` while `header.consensus_hash`
still names the tenure. The tenure's burn height is then the sortition of
`header.consensus_hash` — which the node already fetches for the view and can fetch
for the tenure the same way.

Remaining work: derive it from the block and the sortition lookup rather than from
the tracker alone, and re-run this rig until it passes 931.

## The fix

`BitcoinBlockContext` now carries the tenure's own burn height beside the view's,
and `prepare_phase_reward_cycle` reads the tenure. The pox-5 capture goes from
30/100 to **100/100** and the in-tree 340-block capture stays at 340/340.

Getting it *wired* was the whole difficulty, and the first attempt was reverted for
it. Eleven places set a context's burn height and only four had been given the
tenure; the rest left it at whatever the previous context held, so the
prepare-phase rule read another block's height and `binary_restart` and
`execution_stall` stopped the node at `reward cycle 24 has no signer set`. That was
not the rule being wrong — the in-tree capture has **zero** extends, view equals
tenure for all 340 blocks, so the change has to be a no-op there and any breakage
was wiring.

So the field is private and `height` cannot be assigned at all. `move_to_burn_block`
moves both, which is what every one of those eleven callers means, and
`extend_view_to` moves the view alone — used in the two places that know a tenure
was extended, the follower and the replay harness. A path that moves the view and
forgets the tenure no longer compiles.

