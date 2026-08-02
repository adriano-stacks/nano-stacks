---
id: "055"
title: "Answer block info for blocks before the checkpoint"
status: in-progress
priority: critical
effort: large
type: bug
group: mainnet
dependencies: ["019"]
tags: ["mainnet", "checkpoint", "vm"]
created_at: 2026-08-02
---

# Answer block info for blocks before the checkpoint

## Objective

`HeadersDB` answers every `get-stacks-block-info?`, `get-tenure-info?` and
`get-burn-block-info?` from an in-memory map holding only the blocks nano has
executed itself. A checkpoint carries no headers, so every block before it is
`none`, and any contract consulting chain history gets an answer the network
never gave.

That stopped a mainnet replay at height 8,665,719. `v0-4-market.borrow` reaches
`block-info-nakamoto-ststx-ratio-v2.get-ststx-ratio-at-block`, which does

```clarity
(block-hash (unwrap! (get-stacks-block-info? id-header-hash block) (err ERR_BLOCK_INFO)))
```

and the failure surfaces as an `unwrap-panic` further up. mainnet returned
`(ok true)`; nano returned `(err none)`.

The map is unbounded in the other direction too: it grows with every block
executed, is never written down, and a restart loses it.

## Tasks

- [x] Persist executed block headers rather than holding them in memory.
- [ ] Export the header fields Clarity can read for the checkpoint's ancestry,
      and import them alongside the trie.
- [ ] Backfill the blocks this node executed before it began writing headers
      down, which it can refetch from a peer.
- [x] Serve `HeadersDB` from the persisted store, so a restart answers what the
      run before it answered.
- [ ] Distinguish a header that is genuinely absent from one this node never
      carried, rather than answering `none` as though the block never existed.
- [ ] Replay across a block whose transactions read pre-checkpoint history and
      compare state roots.

## Acceptance Criteria

- `get-stacks-block-info?` answers for any ancestor of the executed tip,
  including blocks from before the checkpoint.
- The answer survives a restart.
- Memory does not grow with distance from the checkpoint.
- Mainnet replay passes 8,665,719.

## It was header coverage after all, and the interpreter proved it

The read trace said no header lookup missed, and that reading was wrong. Asking
the **interpreter** the same call settled it in one run:

```
crosscheck v0-4-market::borrow: wasm failed with Runtime(UnwrapFailure, Some([])),
  interpreter answered Internal(Expect("FATAL: no burnchain block height found
  for Stacks block 76ff2ef7…"))
```

That block is **8,665,718 — the node's own sealed tip**, not something from
before the checkpoint. Two things had hidden it. The header map is only
populated by executing a block, so a process that resumes from disk has none for
any block, including the one it is standing on; and the trace only reported a
lookup that reached the store and found nothing, where this one never reached it
because the map answered first in the run that wrote it.

The `block_header` table is empty for exactly that reason: the blocks nano has
executed were executed before it existed. So the remaining work is both halves —
export the checkpoint's ancestry, and backfill the blocks this node executed
before it began writing headers down.

The crosscheck that found it is worth keeping. clarity-wasm is checked against
the interpreter by design, and the interpreter is in the tree, so a call the
compiler refuses can simply be asked of it: a disagreement names a compiler bug,
and agreement — as here — says the state is what differs.

## What the archive already holds

`chainstate/vm/index.sqlite` carries `block_headers` for all 8.6 million blocks,
with the burn header hash, burn height, times, VRF seed, miner address, burn
spends and reward — every field `BlockHeader` needs. So this is an export and an
import, not a reconstruction.
