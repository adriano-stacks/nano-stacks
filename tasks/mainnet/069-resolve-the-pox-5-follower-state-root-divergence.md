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
