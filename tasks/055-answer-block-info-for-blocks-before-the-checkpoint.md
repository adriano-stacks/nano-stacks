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
- [x] Backfill the blocks this node executed before it began writing headers
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

## Where it got to

The backfill wrote down all 118 headers this node's state was missing, and the
crosscheck then answered the question the whole task turned on. With the headers
present the **interpreter succeeds with `(ok true)` — exactly mainnet's answer —
while clarity-wasm still fails.** So the remaining fault is a compiler bug, not
state and not the arguments.

This plan names the interpreter as the execution path to fall back to, so it can
now answer a call the compiler refuses, behind `NANO_INTERPRETER_FALLBACK`. With
that on, the block executed and **its state root matched**, which is the
strongest evidence available that the interpreter's answer is the consensus one.

Replay then moved on and found two more things the checkpoint was not carrying:
accounting written before the maturity window existed, which is now refused
rather than discovered a hundred tenures later; and the coinbase schedule,
without which a node cannot price a tenure it executes itself.

It now stops at **8,665,722**, the first tenure start past the checkpoint, which
credits two recipients and mints the 1,000 STX coinbase — all of which look
right, so the divergence is in one of their amounts or in the SIP-031 mint
beside them.

The clarity-wasm bug itself is still to be found; every word that path stands on
has been pinned and agrees, and the crosscheck is the oracle to bisect it with.

## The one contract still needing the interpreter

Knowing which epoch a Bitcoin height was in, and rebuilding a rejected contract
newest-epoch-first rather than oldest, took the contracts needing the
interpreter from **forty crosschecks to one**:
`SP2H674PRTZV6YW56K0FMR7GDGZE4ZC5HMYZ3CDEV.flea`, whose module wasmtime refuses
with "expected i64, found i32" at byte offset 74,624.

It is deliberately awkward — one trait returning
`(response (list 20 (response uint uint)) uint)` and forty near-identical
functions passing that trait to one that calls it — but the awkwardness is not
the trigger. Kept as `fixtures/contracts/flea.clar` and checked by
`tests/mainnet_codegen.rs`, **it compiles and loads on its own under every
Clarity version**, as do both of its shapes grown synthetically to sixty-four
functions.

So the trigger is the linking context: the node compiles it beside the contracts
it calls, and one of those modules is what is refused. That is where the next
look goes, and the guard is in the tree so a fix has something to satisfy.

## The refused module is not flea's

Deploying flea, closing the state and reopening it — the path a resumed node
takes, which rebuilds a module from stored source rather than using the one
built at deploy time — still loads and runs it. The call gets as far as the
trait dispatch, so the module is fine.

The transaction that fails passes `SP2H674PRTZV6YW56K0FMR7GDGZE4ZC5HMYZ3CDEV.hilt`
as that trait: 30 KB importing six traits of its own, which fits the
74,624-byte offset in the error far better than flea's own module does. It
needs its trait-defining contracts deployed alongside it to reproduce, which is
the next step.

## The receipts are right; the state root is not

Mainnet block 8,665,780 executes five transactions and nano now produces **five
receipts identical to mainnet's** — same status, same returned value, same
`write_length`, `write_count`, `read_length` and `read_count` — including the
flea call the interpreter answered (653/30/455,375/236 on both sides). Only one
unrelated transaction's `runtime` differs, 247,635 against 222,230, and costs
are not in a state root.

By the plan's own rule that leaves write ordering or a native effect, not the
VM's answers. It is the sharpest divergence signal the project has had: the
execution is right and something around it is not.

## The two engines agree on a state root

The obvious suspect for a matching-receipts root divergence was the interpreter
fallback writing in a different order than the compiler. `engine_state_roots`
asks it directly — same contract, same call, same block identifier, one run
through each engine — and **the roots are equal** for a contract that writes two
variables and two map entries under a branch.

That is evidence, not a guarantee, but it moves the suspicion off the fallback
and onto write ordering across transactions or a native effect around them.

## The compiler bug, found and fixed

`as-contract` did not pass its own type down to the expression it wraps, so that
expression was laid out with the type it was *analysed* with. `(ok u1)` alone is
`(response uint NoType)`; written where a `(response uint uint)` belongs, the
error slot gets one `i32` where two `i64`s are needed. The module compiles
without a diagnostic and wasmtime refuses it. `begin` and `as-contract?` already
propagated their type — `as-contract` was the one that did not.

The whole fix is three lines in `words/contract.rs`. clar2wasm's own suite —
1,375 tests — stays green.

Getting there took three things:

- **Naming the culprit.** nano now validates a module at the point it compiles
  one, so the fault is attributed to the contract that owns it rather than the
  contract that was called. Until then the error pointed at `.flea`, which only
  passes `.hilt` as a trait argument.
- **A checker that runs in seconds.** `cargo xtask check-module` compiles one
  source against a node's own state. Reproducing this from source alone is not
  possible — analysing `.hilt` needs 376 contracts and 2.9 MB — but a node
  already has them.
- **Delta debugging.** 30 KB reduced to one line, which needs no traits, no
  folds and no other contracts:

  ```clarity
  (define-public (sr (a (response bool uint))) (as-contract (begin (try! a) (ok u1))))
  ```

  A short return is what makes it visible; without one the two layouts agree.
  Pinned by `tests/as_contract_codegen.rs`.

## What the fix settled, and what it left

With `as-contract` fixed the node runs block 8,665,780 entirely on clarity-wasm:
no module refused, no crosscheck, no interpreter. The five receipts still match
mainnet exactly, so the compiler and the interpreter agree on every value, cost
dimension and event for this block.

The state root still does not match — and it **changed**, from `ff87845b…` under
the interpreter to `684…` under the compiler. So the two engines *do* write
differently here, which `engine_state_roots` did not catch on a simpler
contract, and neither of them writes what mainnet wrote.

That narrows it further than before. Every transaction's `write_count` matches
the network's, so the number of Clarity writes is right; what differs is their
order, or something written outside them. That is the next thing to look at, and
the fallback is no longer needed to get there.

## The divergence is not one extra write

`cargo xtask probe-root` replays a block's writes straight into the MARF from a
`NANO_TRACE_WRITES` log and seals it the way the node does — under the
placeholder identifier the node executes with, renamed on seal. It reproduces
the node's root for 8,665,780 exactly, which is what makes it worth trusting,
and it runs in seconds against the real state instead of minutes of replay.

Leaving out each of the block's 30 distinct keys in turn reaches the expected
root in **no** case. So the difference is not a single write the network did not
make. What is left is a missing write, a wrong value under a right key, or the
order the keys first arrive in — and the probe is the way to test the last of
those, since it can seal the same set in any order.

Ruled out along the way, each against stacks-core's own source: the liquid
supply increment is unconditional there too (`finish_block`), so nano writing it
when nothing unlocked is correct and guarding it broke the captured replay at
its first block; `block_time` is MARFed from epoch 3.3 and written once, as
nano does; SIP-031 is gated on a new tenure in both. There were no lockups at
8,665,779, 8,665,780 or 8,665,781, so unlocks are not it either.

## Nor is it order

The probe seals the same keys and values in four orders — as traced, sorted by
key, sorted by trie path, and reversed — and all four give the same root. That
is not a bug: thirty scattered `Sha512_256` paths in a trie of 8.6 million keys
essentially never share a node, and order is only consensus for keys that do.
It does mean ordering is ruled out here.

So the difference is a **missing write** or a **wrong value under a right key**.
Both are now cheap to test: the probe reproduces the node's root offline in
seconds and, since it reads `pending_root` and aborts, leaves nothing behind.

It did leave something behind at first — thirty-two blocks sealed at height
8,665,780, one of them under the real block identifier, which would have made
the node refuse to execute that block at all. They were removed and the probe
no longer commits anything.

## Every value nano writes is the network's; one it does not write is missing

Checked against mainnet at block 8,665,780's own tip, key by key:

- **twelve STX balances** — every one equal, to the microSTX;
- **four nonces** — each exactly the transaction's nonce plus one, and the block
  is decoded from nano's own staging store to confirm it;
- **four pool reserves** (`xyk-pool-stx-aeusdc` v-1-1 and v-1-2, `x-balance` and
  `y-balance`) — byte-identical serialized Clarity values.

Together with receipts that already matched — status, result, and all four
non-runtime cost dimensions — this says nano computes the right answer for
everything it touches. The block diverges on something it does not touch.

So the search is now for a **missing write**: a key the network wrote in this
block and nano did not. It is not one of nano's writes being wrong, not an extra
one, and not the order. `cargo xtask decode-blocks` now prints each
transaction's origin, nonce and fee, which is what settled the nonces.

