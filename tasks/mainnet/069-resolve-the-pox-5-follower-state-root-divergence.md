---
id: "069"
title: "Resolve the PoX-5 follower state-root divergence"
status: pending
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

- [ ] Preserve the checkpoint, burn blocks, Nakamoto blocks, both stock and nano
      observer receipts, node configuration, and compiler identity needed to
      reproduce height 931 offline.
- [ ] Re-run from a pristine parent and establish whether the mismatch is
      deterministic rather than residue from a rejected block or incomplete
      checkpoint.
- [ ] Compare transaction results, all five cost dimensions, events, native
      writes and the ordered MARF journal before changing consensus code.
- [ ] Use the identical-journal MARF oracle if receipts and writes agree, and
      the clarity-wasm differential oracle if they do not.
- [ ] Fix the owning layer and add the smallest fixture that fails before the
      fix; do not paper over the block with a checkpoint advance or a skipped
      root check.
- [ ] Replay from the original checkpoint through height 931 and at least 100
      later blocks, including the next tenure boundary, with roots and receipts
      matching.

## Blocked on a capture that does not exist yet

Every item here starts at the first one, and the first one cannot be done from
this tree. The failing block was height 931 of a *live* epoch-4.0 PoX-5 Hacknet;
the capture in `fixtures/` is a different chain (hacknet commit
`bf821e9d`, chain id `2147483648`, checkpoint at height 460, blocks 461–800), and
nothing in it reaches 931. `xtask capture-fixtures` reads a running node's
`blocks.sqlite` and `sortition/marf.sqlite`, so re-capturing needs that Hacknet
back.

What can be said without it, and is worth saying because it bounds the search:

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
