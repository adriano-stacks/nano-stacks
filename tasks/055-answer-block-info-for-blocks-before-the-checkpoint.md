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

## Where the missing write is not

Checked against stacks-core's own `setup_block` and `finish_block`, in order:

- `setup_block_metadata` writes `block_time` once, from epoch 3.3 — as nano does;
- `set_tenure_height` is gated on a new tenure in both, and this block is not one;
- `process_epoch_transition` writes only when the epoch changes;
- `check_and_handle_reward_start` returns early once a cycle has been handled,
  and burn 960,234 is 184 blocks into cycle 140, so it was handled long before
  the checkpoint;
- `check_and_handle_prepare_phase_start` fires at the prepare phase, which for
  the next cycle begins at burn 962,050;
- burnchain stacking, transfer and delegate ops come from the tenure's own burn
  block and are empty mid-tenure;
- `increment_ustx_liquid_supply` is unconditional after unlocks in both;
- SIP-031 is gated on a new tenure in both.

## What would actually find it

Guessing has run out. The parent's root matches the network's, so the ancestor
skip-list is identical and the difference is the block's own content hash —
which means the root node's children differ, and exactly which child differs is
a fact that can be read rather than inferred.

`/v2/map_entry/...?proof=1` returns a MARF merkle proof, and stacks-core's proof
for a `Node256` carries the hashes of every child but the one on the path. One
proof against block 8,665,780 therefore yields all 256 of the network's root
children; comparing them with nano's gives the first nibble of the missing key's
path, and recursing gives the rest.

That needs `TrieMerkleProof` deserialization, which `nano-conformance` already
has through `stackslib`, and a way to read the children of nano's *pending* root.
It is the next thing to build; nothing cheaper will name the key.

## Found: two consensus values a contract stores

The trie diff worked. One merkle proof against block 8,665,780 gave 255 of the
network's root children; comparing them with nano's left exactly one child that
disagreed for a reason other than being the proof's own path — `0xdc` — and this
block writes exactly one key under it:
`SP1Y5YSTAHZ88XYK1VPDH24GY0HPX5J4JECTMY4A1.univ2-core::0::pools::u6`.

That map entry records a pool's reserves *alongside two chain values*, and both
were wrong:

- **`block-height`** — the network wrote 251,323, nano wrote 8,665,780. From
  epoch 3.0 the interpreter answers `block-height` with the **tenure** height
  (`vm::variables`, `NativeVariables::BlockHeight`); clarity-wasm's host function
  returned `get_current_block_height()` whatever the epoch. Fixed in the vendored
  compiler and pinned by `tests/block_height_keyword.rs`; clar2wasm's own 1,375
  tests stay green.
- **`burn-block-height`** — the network wrote 960,235, nano wrote 960,234. A
  tenure that outlives the burn block electing it is *extended*, and the
  extension moves the burn view forward, so a block mid-tenure sees a later burn
  height than its own sortition. The node asked its peer for the sortition of the
  block's tenure; it now asks for the sortition of the **burn view**, and a
  resumed node — which never executed the tenure change that stated the view —
  walks back through the tenure to find it.

With both fixed the node executes 8,665,780 and 8,665,781 and stops at
**8,665,782**, which is the first movement in this replay since the checkpoint's
own tail.

This is worth generalising: a contract that stores a chain value makes that value
consensus, and it stays invisible until one does. Both bugs had been executing
wrongly for every block before this one and cost nothing until a contract wrote
one down.

## Next: 8,665,782, and a trait argument the compiler will not type

The same diff run against 8,665,782 named two root children with no writes under
them at all, and the trace says why: the block's contract call produced *no
writes*, because it failed with

```
contract analysis failed: Type error: contract-call? argument must be typed
```

The call is `SP2H674PRTZV6YW56K0FMR7GDGZE4ZC5HMYZ3CDEV.loto`, passed a contract
principal to use as a trait — the same shape as `.flea` before it. `.loto` is
2 KB and `cargo xtask check-module` compiles it to a module that loads under
**every** Clarity version, so the contract is not the problem: typing a trait
argument arriving as a serialized principal at the top-level call boundary is.

This is the third clarity-wasm divergence this block-by-block replay has turned
up, after `as-contract` and `block-height`, and it has the same shape: the
interpreter accepts what the compiler refuses, and mainnet is the interpreter.

## And fixed: a contract principal where a trait is expected

Naming the contract in a compile failure — the same lesson as the module check,
applied to analysis errors — turned this one up in one run:
`SP2C2YFP12AJZB4MABJBAJ55XECVS7E4PMMZ89YZR.arkadiko-swap-v2-1`. From there it
reduced to three lines:

```clarity
(define-public (g (x uint))
  (contract-call? .arkadiko-dao mint-token .wrapped-stx-token x tx-sender))
```

A contract principal written as a literal where the callee expects a trait
carries no annotation from the type checker, so clarity-wasm refused the whole
contract. On the wire it is a principal either way, which is what its size and
layout depend on, so that is the fallback. clar2wasm's 1,375 tests stay green
and the real 25 KB contract compiles to a module that loads.

The transaction merely *failed* rather than erroring loudly, which is why the
block's root diverged with all three receipts present: a failed call writes
nothing, and nothing is a state the network never had.

## Replay depth: 8,665,781 to 8,665,892

With the three compiler and burn-view fixes the node executed **111 further
blocks** in one run, across two tenures and three burn blocks, and stopped at
8,665,893.

That block is a different kind of divergence, and the first where nano's
*receipts* disagree with the network's:

| | mainnet | nano |
|---|---|---|
| `e855cacc` | `(ok (list (err u9) (err u9)))`, 44 writes | `(ok ((ok u1529) (err u9)))`, 45 writes |
| `0a599bb5` | `… (err u9) (err u9) (ok u3742) (err u9) (ok u287)…`, 152 writes | `… (err u9) (err u2) (err u2) (err u2)…` |

nano's first sub-swap *succeeds* where the network's fails, and later ones return
the aggregator's `u2` where the network gets `u9` or a result. No contract failed
to compile in this block, so this is a behavioural difference inside one of the
pool contracts the aggregator routes through — the opposite direction from the
last three, where nano did too little.

## A dual-engine oracle, and 8,665,970

The crosscheck inside the VM only fires when the compiler *refuses* a call,
which is no use when it answers and answers differently. `Vm` now has a runtime
switch between engines, and `NANO_CROSSCHECK_TRANSACTIONS` runs each transaction
through the interpreter inside a rolled-back bracket before running it for real,
reporting any disagreement in status or value. Rolling the first run back is what
makes comparing a *successful* call safe.

Replay now reaches **8,665,970** — about 370 blocks past the checkpoint, from 179
at the start of this work — and stops at 8,665,971, seven transactions. Verified
without the crosscheck enabled, so the oracle is not masking anything.

The oracle is noisy in one direction worth knowing about: the interpreter arm
sometimes reports `contract execution left the database in an invalid state`,
which is the bracket rather than the contract, and it maps an aborting response
to `Success` where the compiler says `AbortedByResponse` — the same value and
commit flag either way. Real disagreements are the ones where the *value*
differs.

## 8,665,971: an sBTC withdrawal the network accepted

Two transactions diverge here, and the second is the clearer:
`e220dcfb` calls `SM3VDXK3WZZSA84XXFKAFAF15NNZX32CTSG82JFQ4.sbtc-withdrawal`
`accept-withdrawal-request`, which does

```clarity
(asserts! (is-eq (some burn-hash) (get-burn-header burn-height)) ERR_INVALID_BURN_HASH)
```

with `burn-height u960240`. The network returns `(ok true)` and six writes; nano
returns `(err u508)` — `ERR_INVALID_BURN_HASH` — and none. **Both engines agree**,
so this is state or context rather than the compiler.

The node now seeds the header hashes of the 32 burn blocks behind the one it is
executing from its own Bitcoin source, skipping heights it already knows and
saying so when a header cannot be had — a checkpoint-started node has executed
under almost none of them, and an unanswered height rejects a withdrawal the
network accepted. `decode_block_hash` keeps display order, which is the order the
contract compares against, and the seeded value for 960,240 matches
mempool.space.

It is still `(err u508)`, and the seeding reports no missing header, so
`get-burn-block-info? header-hash u960240` is answering with something other
than what was recorded — the next thing to look at is the path Clarity takes to
reach it, which goes through `get_tip_sortition_id` and a sortition identifier
rather than the height directly.

The other transaction, `055db235`, returns the aggregator's `(err u2)` where the
network gets `(err u9)` on its third sub-swap — same family as 8,665,893's, and
also not a compile failure.

## Fixed: the burn headers were seeded on one path of three

`get-burn-block-info? header-hash u960240` was answering `none`, and the seeding
that was supposed to prevent that reported nothing missing — because it ran on
the path a following node takes and not on the two a resuming or catching-up node
takes. Tracing the lookup itself said so in one run.

With the window seeded wherever a Bitcoin context is built, 960,240 answers with
`00000000000000000000f8ca2be9f81dd567c0bd4802334e3063a0dcbf82a825` — exactly what
the withdrawal compares against — and the block executes.

**Replay then ran from 8,665,971 to 8,666,264 without a single state root
mismatch**, and was still going. That is about 660 blocks past the checkpoint,
from 179 when this work started.

Worth keeping: a lookup that can reject a withdrawal the network accepted is
worth being able to watch, so `NANO_TRACE_BURN_HEADERS` prints what every one
answered.

## 8,666,265: a deploy that does not commit

Replay ran clean from 8,665,971 to **8,666,264** and stops on the next block with
a hard error rather than a root mismatch:

```
no contract commitment for SPSX722NK9V3A8D3CVQT0CDY4EBQ3E9FSDDE61FT.linear-kinked-ir-v1
Clarity evaluation error: NotInDatabase("compiled contract ...")
```

That contract is *deployed in this very block* — 8,666,265 — and something in the
same block calls it. nano's deploy leaves no commitment, so the call cannot find
it and the block dies.

The contract is not the problem: fetched from the chain it is 8 KB and
`cargo xtask check-module` compiles it to a module that loads under Clarity 3
and 4 against nano's own state. So the deploy *transaction* is failing for some
other reason — its version byte, a post-condition, or the deploy path itself —
and that is the next thing to look at. Reading the source out of the block bytes
by hand does not work; it has to be decoded properly, which is worth adding to
`decode-blocks`.

## Fixed: a deploy never compiled what it calls

The deploy that would not commit was not `linear-kinked-ir-v1` at all. Making a
failed deployment say why named the real one in a single run:

```
a deployment failed and stopped the block:
  NotInDatabase("compiled contract SP3M2BYF7RGF8WKW5FVDNJ6WR8D7AR9BHDXAKPXZE.constants-v1")
```

A contract's top-level expressions run at deploy time and may call other
contracts. Only the *call* path ensured a callee's module was built; the deploy
path did not, so a deploy that calls anything died with its callee reported
missing — and a block of five dependent deploys from one address, which is what
8,666,265 is, fails on the first of them and takes the rest with it.

Replay is now at **8,666,344** and running.

`decode-blocks` prints which contract each transaction publishes, for both the
plain and versioned payloads, since that is what says where to look next.

