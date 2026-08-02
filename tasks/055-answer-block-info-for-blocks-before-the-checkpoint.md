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

## Header coverage was not what stopped 8,665,719

Persisting the headers was right on its own terms — the map grew with the chain
and a restart lost it — but tracing every `get_block_at_height` and header read
during the failing block showed **no miss at all**, and no contract failing to
compile either. So the `UnwrapFailure` is a Clarity-level unwrap of an `err` the
contract itself produced, and the read behind it is one nano answers with the
wrong *value* rather than with nothing.

Narrowing that needs tracing at the granularity of a single Clarity read: which
key, in which contract, returning what. The next step is that trace, not more
guessing at which surface is missing.

## What the read trace ruled out, and what it found

Tracing every Clarity read through the failing block ruled out the obvious
suspects one at a time. No `get_block_at_height` missed, no header missed, no
contract failed to compile, and the words a VAA verification stands on —
`keccak256`, `sha256`, a `secp256k1` recovery, `slice?` across its bounds, and
`map` across two lists — all agree with stacks-core. Those are pinned as tests.

Two things the trace did find:

- **Compiling on demand re-executes the whole call.** The transaction reaches 23
  contracts and read their commitments 1,596 times, because each missing module
  is discovered by failing, compiling one, and running again from the start.
  That is quadratic, and it is bounded by `MISSING_MODULE_ATTEMPTS = 64`, so a
  call reaching more contracts than that fails for want of attempts rather than
  for any reason of its own. Compiling the transitive closure up front would do
  it once.
- **The cost is seven and a half times mainnet's**: 92,687,934 runtime against
  12,352,456, at 28 reads against 300. Each retry starts from a fresh clone of
  the cost tracker, so that is one attempt's real work, not accumulated waste —
  which means nano is genuinely doing much more of it on this path.

Every word that path stands on has now been pinned against stacks-core and
agrees: `keccak256`, `sha256`, a `secp256k1` recovery, `slice?` over both a list
and a buffer including its bounds, `buff-to-uint-be` at an offset, and `map`
across two lists. Block, header and module lookups all answer. So the divergence
is not a missing surface and not one of the primitives.

The failing expression is inside wormhole's guardian-set verification, after the
set is read and before anything is written. `unwrap-panic` there has no
fallback, and clar2wasm leaves the Clarity stack trace empty, so the message
says only that something could not be unwrapped.

The runtime cost is the sharpest remaining clue: **92,687,934 against mainnet's
12,352,456**, and that is one attempt's real work rather than accumulated
retries. nano is doing seven and a half times the work on a path that then
fails, which is what a loop running too many times looks like.

Two ways forward, in order of cost: populate clar2wasm's stack trace so the
error names its own expression, or lift the VAA bytes out of the transaction and
run the verification against the interpreter in isolation, where the crosscheck
harness can bisect it.

## What the archive already holds

`chainstate/vm/index.sqlite` carries `block_headers` for all 8.6 million blocks,
with the burn header hash, burn height, times, VRF seed, miner address, burn
spends and reward — every field `BlockHeader` needs. So this is an export and an
import, not a reconstruction.
