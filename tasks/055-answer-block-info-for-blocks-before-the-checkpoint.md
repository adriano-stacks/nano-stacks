---
id: "055"
title: "Answer block info for blocks before the checkpoint"
status: completed
priority: critical
effort: large
type: bug
group: mainnet
dependencies: ["019"]
tags: ["mainnet", "checkpoint", "vm"]
created_at: 2026-08-02
completed_at: 2026-08-06
---

# Answer block info for blocks before the checkpoint

## Production execution rule

The historical investigation below used interpreter-only and fallback modes to
diagnose old replay failures. Those modes are now forbidden by [[060]] and must
not be retained or restored in the node. The interpreter may be used only by
separate, rolled-back diagnostic tooling; a production clarity-wasm failure
rejects the candidate and invokes no fallback engine.

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
- [x] Export the header fields Clarity can read for the checkpoint's ancestry,
      and import them alongside the trie.
- [x] Backfill the blocks this node executed before it began writing headers
      down, which it can refetch from a peer.
- [x] Serve `HeadersDB` from the persisted store, so a restart answers what the
      run before it answered.
- [x] Distinguish a header that is genuinely absent from one this node never
      carried, rather than answering `none` as though the block never existed.
- [x] Represent field availability explicitly: a partially reconstructed
      header must fail on an unknown field rather than returning a plausible
      zero for miner, reward, burn-spend or tenure information.
- [x] Recover and oracle-check every consensus-visible field, including miner
      address, block reward, burn spends, tenure height and tenure start, for a
      representative pre-checkpoint window.
- [x] Replay across a block whose transactions read pre-checkpoint history and
      compare state roots.

## Acceptance Criteria

- `get-stacks-block-info?` answers for any ancestor of the executed tip,
  including blocks from before the checkpoint.
- `get-tenure-info?` and `get-burn-block-info?` return the same values as
  stacks-core for pre-checkpoint ancestors; an unavailable value is never
  encoded as a real zero.
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

At the time, the plan named the interpreter as an execution fallback, so the
experiment used `NANO_INTERPRETER_FALLBACK` to answer the refused call. That
historical run matched the root and localized the compiler fault; it is not a
permitted production mechanism and cannot be used to close a current gate.

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

## Historical diagnostic: the one contract that needed the interpreter

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

## And fixed: a constant naming a contract is a static call

`SP3EWCDA3V8HCP64CSETSNYXZ25WC4AJ95EC0ZEST.dlmm-adapter` routes every swap
through a `SWAP_ROUTER` constant and would not deploy:

```
Type error: Dynamic argument of contract-call? should be a trait
```

clarity-wasm recognised only the *literal* form of a static call, so a name that
resolves to a contract principal was taken for a trait dispatch and refused for
not being one. The generator already tracks constants by name and type, so the
target is right there. clar2wasm's 1,375 tests stay green and the contract
compiles under Clarity 4 and 5.

That is the fifth clarity-wasm divergence this replay has found —
`as-contract`, `block-height`, a contract principal where a trait is expected, a
deploy that never compiled its callees, and now this — and every one of them was
invisible until a mainnet block happened to depend on it.

## And fixed: merging `none` over an optional field

`SPSX722NK9V3A8D3CVQT0CDY4EBQ3E9FSDDE61FT.governance-v1` compiled to a module
that would not load, so it would not deploy and took its four sibling deploys
with it. Reduced from 55 KB to:

```clarity
(map-set proposals id (merge proposal { execute-at: none }))
```

where the field is `(optional uint)`. `none` analyses as `(optional NoType)`, and
clarity-wasm laid the overriding tuple out with *its own* field types rather than
the result's — writing an i32 where the value is an indicator and two i64s.

This is the same fault as `as-contract`, in a different word: a value built as
the type it was analysed with, where the type it is going into is what decides
its layout. Worth looking for wherever clar2wasm reads `get_expr_type` on a
sub-expression whose width the surrounding type fixes.

Replay reaches **8,666,422**.

## The oracle earns its keep: a compiler divergence proven against the chain

Block 8,666,423's aggregator call is the first divergence settled by the
dual-engine crosscheck rather than by reasoning:

| | 5th | 6th | 7th sub-swap |
|---|---|---|---|
| compiler | `(err u9)` | `(err u2)` | `(err u2)` |
| interpreter | `(err u9)` | `(err u9)` | `(ok u17539)` |
| **mainnet** | `(err u9)` | `(err u9)` | `(ok u17539)` |

The interpreter matches the chain exactly, so clarity-wasm is wrong — no
argument about state, context or ordering is needed. That is the seventh
clarity-wasm divergence this replay has found and the first where the answer
came in one run from a tool rather than a day of narrowing.

The same shape appeared at 8,665,893 and was left uncharacterised; it is this.
The reduction still has to be done — the sub-swaps route through several DEX
adapters and the failing one has to be isolated — but which engine is at fault
is no longer in question.

## Narrowing it: not the stableswap curve

`(err u2)` is the aggregator's fallback when a **stableswap** leg fails —
`SPQC38PW542EQJ5M11CR25P7BS1CA6QT4TBXGB3M.stableswap-stx-ststx-v-1-2` and its
`usda-aeusdc` siblings. Both compile to modules that load, so the fault is in
what they compute rather than whether they build.

`cargo xtask call-both` runs one contract call through each engine against a
node's own state and prints both answers — the finest grain the crosscheck comes
in, and the only way to ask about a contract reachable solely through half a
dozen others. Pointed at `get-y`, the curve solver both legs go through, the two
engines **agree** on every input tried: equal balances, a pool drained to one
unit, a `u128` maximum, and a swap of almost the whole balance.

So the divergence is not the curve. It is in the stateful swap path around it,
which is where to look next.

## Narrowing further: not the quote either

`call-both` now takes arguments as `u123` or `SP....name` and parses them with
Clarity's own parser — hand-encoding a contract principal is a c32 checksum away
from looking like a missing contract, which cost one run — and takes an optional
`--sender`, since a swap called by the wrong principal fails on a balance long
before it reaches anything worth comparing.

Against the stableswap pool both failing legs go through, the engines agree on:

- `get-y`, the curve solver, across equal balances, a pool drained to one unit,
  a `u128` maximum, and a swap of nearly the whole balance;
- `get-dy`, the quote that wraps it;
- `swap-x-for-y` itself, which both refuse identically for want of a balance.

So the divergence is not the curve, not the quote, and not the swap's own
refusal path. Reproducing it needs the transaction's real sender and amounts —
the aggregator calls the leg `as-contract` with a buffer-encoded route — which
means the next step is for the crosscheck to report the *sub-call* that first
differs rather than only the transaction.

## Named: a transfer from a contract to itself

Diffing the two engines' *write* traces across a crosscheck bracket — the arms
are marked, so the traces split cleanly — showed them agreeing for 111 writes and
then parting: the interpreter goes on to a whole further leg, the compiler
finalises the block.

Tracing every cross-contract call the compiler makes, with its arguments, names
it exactly:

```
call token-stx-v-1-2::get-balance(hilt)                        -> (ok u2676101)
call token-stx-v-1-2::transfer(u3645770, hilt, hilt, none)     -> (err u2)
```

The balance is there, and `u2` from `stx-transfer?` is *sender and recipient are
the same principal*. The **recipient argument is wrong**: it is `.hilt` itself
where it should be the pool being swapped through.

Ruled out since, each by running both engines against real state:

- `as-contract`'s `tx-sender`, plainly, nested, through another contract, and
  read back out of a `let` — all four agree;
- `contract-of` on a trait argument, inside and outside `as-contract` — agrees;
- the stableswap curve `get-y`, its quote `get-dy`, and `swap-x-for-y`'s refusal
  path — all agree.

So a principal the aggregator computes for the leg's recipient comes out as the
calling contract under clarity-wasm. `tests/as_contract_sender.rs` holds the
cases already checked, so the next one added has somewhere to go.

`NANO_TRACE_CALLS` prints every cross-contract call with arguments and result.
It is the instrument that turned "the engines disagree somewhere in this
transaction" into a named argument of a named call, and it belongs in the tree.

## Ten ways it is not wrong

`tests/as_contract_sender.rs` now pins ten cases where the two engines agree,
covering the three ways a routing contract works out where to send tokens:

- `as-contract`'s `tx-sender` — plainly, doubly nested, read back out of a
  `let`, through a called contract, and in a contract *called by another
  contract*, which is the case that matters and the one a direct-invocation test
  would miss;
- `contract-of` on a trait argument, inside and outside `as-contract`;
- `element-at?` into a `(list N <trait>)`, at every index, with the element
  handed to a function the way `fold` hands it — the shape that has already
  produced two clarity-wasm bugs on its own.

None of them differs. Hypothesis-testing has stopped paying here: the divergence
is a wrong principal in `transfer(amount, hilt, hilt, none)`, and the way to name
it now is to trace the aggregator's *own* intermediate values rather than guess
which word produced them. `NANO_TRACE_CALLS` reaches call boundaries; what is
missing is inside one.

## Historical fallback experiment

The plan then named pointing `nano-vm` at the Clarity interpreter as its
highest-value fallback, and the crosscheck proved the interpreter right against
mainnet receipts on transactions where clarity-wasm was wrong. The experiment
therefore asked whether that fallback worked. That policy has since been
removed: the results below are diagnostic history, not a node design that may
be restored.

Half the answer is yes: with `NANO_INTERPRETER_ONLY` set, the captured replay
passes all forty blocks **with matching state roots**. The interpreter is not
merely an oracle; it executes.

The other half is one named call. Against mainnet it fails immediately:

```
SP000000000000000000002Q6VF78.pox-5::stake-update left the database in an invalid state
```

That message is `OwnedEnvironment::destruct` returning `None`, which means a
nested context outlived the call that opened it — and `pox-5` is exactly where
nano installs a `SpecialCaseHandler`, the PoX locking hook that fires on the
contract-call boundary ([[plan W6.4]]). The wasm path handles the same call; the
interpreter path does not.

So the fallback is one bug away from being available, and that bug is in the PoX
special-case wiring rather than anywhere general. The error now names the call
that caused it, which is what turned this from "the interpreter does not work"
into a specific thing to fix.

### Where the PoX unwind goes wrong

The chain is short and worth writing down. `handle_contract_call_special_cases`
dispatches `pox-5` to `pox_5::handle_contract_call`, which for `stake-update`
reaches `handle_stake_lockup_update_pox_v5`. That locks the STX and then calls

```rust
global_context.log_stacking(&staker, amount_ustx)?;
```

`log_stacking` appends to the *current* context's asset map. `destruct` refuses
to unwind because a Clarity context — not the database — is still open, which is
what happens when an error propagates out of the handler after the environment
has pushed one. The wasm path drives the same handler through
`stdlib.contract_call` and does not hit it.

So the fix is in how nano's interpreter entry brackets a transaction against what
the special-case handler expects, not in the handler. It is the last thing
between the plan's own fallback and a node that can use it, and it is also
[[M8d]] — the gate that `pox-5.stack-stx` must move locked STX rather than only
map entries.

### The unwind was hiding the error

Reporting only that the unwind failed threw away the half that mattered.
`destruct` returning `None` happens *because* the call errored and left a
context open, so the call's own error is the one to report. With it surfaced:

```
RuntimeCheck(Unreachable("Public function must return response: int"))
```

`pox-5::stake-update` is being taken to return an `int` where a public function
must return a response. That is a contract-analysis answer, not a locking one —
so the suspicion moves off the PoX handler, which merely happened to be the
first thing to trip over it, and onto what nano's interpreter path reads as
`pox-5`'s signature.

Worth keeping as a habit: an error raised while unwinding is almost never the
error worth reading.

## A rejected deployment stopped the whole block

The cost a transaction reports is the block's *running total*, and the caller
subtracts what the block had already spent to get that transaction's own. A
rejected deployment reported `ExecutionCost::ZERO`, so the subtraction underflowed
and the block died with `transaction cost underflow` — a hard stop, where the
transaction merely failing is what the network does.

A rejected deployment costs nothing, which is the running total *unchanged*.
With that, the block executes and 8,666,585 is an ordinary state-root divergence
instead of a node that stops.

## Where replay actually is

Measured rather than assumed, on a clean run: **8,666,584**, advancing about 150
blocks per ten minutes with zero state-root mismatches behind it. The blocks I
had been treating as a wall — 8,665,893, 8,666,423 — go past; the fixes for
`merge`, the constant-named contract, the deploy that never compiled its callees
and the burn-header seeding carried them, and my reading of "stuck" was against a
stale binary.

## 8,666,585: a deploy whose module will not load

`SP4SZE494VC2YC5JYG7AYFQ44F5Q4PYV7DVMDPBG.rewards-stx-v1` compiles and wasmtime
refuses the result — "expected i64, found i32" — so the deploy fails where the
network's succeeded, and both engines agree, which rules out the compiler's
*answers* and leaves its codegen.

The deploy path did not run the loadability check the rebuild path does, so this
surfaced as an `UnableToLoadModule` from `initialize_contract` with no contract
named. It does now, and says so in one run.

Reduced from 3.7 KB to nine forms, kept as
`fixtures/contracts/rewards-stx-v1-reduced.clar`. The shape around it is
`as-contract?` — Clarity 4's replacement for `as-contract`, and a *different*
word in the same file as the type-propagation bug already fixed — holding a
`try!`, inside an `if`/`begin`, followed by a `var-set`. None of those in
isolation reproduces: the plain form, the `try!` form, the `if`/`begin` wrapper
and the `var-set` after it were each tried and each compiles. So the trigger is a
combination still to be found, and the reduction is the place to find it.

### It takes both halves

`process-rewards` has two statements in sequence: a release half — `as-contract?`
holding a `try!`, guarded by an `if`, followed by a `var-set` — and a keeper half,
an `if` whose branches `print` tuples. Each half **on its own compiles to a module
that loads**; the two together do not.

So this is not a single word being generated wrongly. It is something about two
statements in sequence, each of which leaves the stack in a state the other
does not expect — which is the same family as `as-contract` and `merge` (a value
laid out as one type where another is expected) but reached through statement
composition rather than a single expression.

The nine-form reduction is in the tree as
`fixtures/contracts/rewards-stx-v1-reduced.clar`. Ruled out individually and
recorded so nobody repeats them: the plain `as-contract?`, `as-contract?` holding
a `try!`, an `if`/`begin` around it, a trailing `var-set`, two `print` branches of
matching shape, the release half alone, the keeper half alone.

## An unattended run does not accumulate progress

Worth correcting, because it was my own suggestion: leaving the node running does
not find divergences in bulk. A divergence is a **hard stop** — the node retries
the same block for as long as it runs and never reaches the next one. Ten minutes
or ten hours, it sits at 8,666,584 with fifty identical rejections behind it.

So there is no version of "let it run overnight and collect the failures". Every
divergence has to be fixed before the one after it is even visible, which is what
makes the per-divergence cost the whole cost.

That leaves exactly two ways to go faster: fix them faster, or execute a path
that does not produce them. The tooling has done the first as far as it goes —
the last three bugs each fell out in one run. The second is the interpreter.

## The interpreter's pox-5 failure is real, not an artifact

Ruled out the obvious explanation: `call_contract_values_in_context`, which is
how nano makes its own internal reads of `pox-5` and `signers` during signer-set
derivation, always goes through `call_compiled_contract`. It never sees the
interpret switch, so those reads are not what is failing.

`pox-5::stake-update` returning `int` where a public function must return a
response is therefore a genuine transaction-level failure of the interpreter
path, and the remaining thing to understand before the fallback is usable.

## The fallback carries blocks the compiler cannot

Routing *deploys* through the interpreter too was the missing half. The engine
switch only moved contract *calls*, so a contract whose module wasmtime refuses
still could not be deployed, and a deploy that fails stops the block — which is
exactly what 8,666,585 is.

With deploys routed as well, the interpreter **executed straight past
`rewards-stx-v1`** and 66 blocks beyond it, to 8,666,650, with no state-root
mismatch. That is the first direct evidence that the fallback does what plan.md
says it does: carry a chain the compiler cannot.

The captured replay still passes all forty blocks with matching roots under it,
so the switch has not been bought by loosening anything.

## What the interpreter path stops on

`SP4SZE494VC2YC5JYG7AYFQ44F5Q4PYV7DVMDPBG.native-pool-v1::delegate` —

```
RuntimeCheck(Unreachable("Public function must return response: int"))
```

The earlier `pox-5::stake-update` sighting is the same fault seen from one level
down: `delegate` reaches pox-5 through `try! (contract-call? ...)`, and a pox-5
public function is evaluating to an `int` under the interpreter where the
compiler gets a response. Mainnet runs the same interpreter and succeeds, so the
difference is in what nano's state gives it, not in the interpreter.

Naming the call on the way out is what made this findable; the error alone says
a public function returned the wrong shape and not which one.

### Not the source, and not the `.dao` check

Narrowed by elimination rather than guessed:

- `.dao check-is-enabled`, the first thing `delegate` calls, **agrees** between
  the engines against real state;
- nano's stored `pox-5` source has `(define-public (stake …))` and
  `(define-public (stake-update …))`, both declaring responses, so the contract
  the interpreter evaluates is the right one;
- its stored analysis agrees, giving `stake-update` a `ResponseType` return.

So neither the source, the analysis, nor the call before it. What is left is the
invocation itself — `pox-5 stake` reached through
`try! (contract-call? …)` from `delegate` — with the arguments that transaction
actually passes, which is where to pick this up.

### What the network actually did with it

The transaction is
`0x107dab2b…`, calling `native-pool-v1::delegate` with
`(.native-pool-signer, u2500000000, u96)`. Mainnet's outcome:

```
abort_by_post_condition   (ok u2500000000)
```

So the network **rolled it back on a post-condition** — a perfectly ordinary
outcome, not a failure. What nano has to produce is that abort, and under the
interpreter it raises an error instead. That reframes the bug: it is not that a
pox-5 function answers wrongly, it is that the path which should end in a
post-condition rollback ends in an unwind that loses the answer.

(Reproducing it outside the node needs a tip where `.native-pool-signer` exists;
probing at the current sealed tip reports it unknown, so the node is the place to
work on it.)

## Not a VM bug at all: an argument that names nothing

`.native-pool-signer`, the trait argument `delegate` is passed, **does not exist
on mainnet either** — and mainnet still returns `(ok u2500000000)` and aborts on
a post-condition. Passing a contract principal that names nothing is fine; only
*calling* it would fail.

nano compiles every contract-principal argument ahead of the call, because a
trait dispatch will need it and the call cannot say in advance which one. That is
right as an optimisation and wrong as a requirement: one that cannot be compiled
now leaves the call alone instead of failing it.

That reframes three sightings that looked like different bugs — `pox-5
stake-update` returning an `int`, `native-pool-v1::delegate` unwinding, and the
compiler's "unknown contract" — as one thing seen from three angles. It is worth
remembering how far the wrong frame carried: the pox-5 source, its analysis, the
`.dao` call before it and the special-case handler were each investigated and
each was fine.

## The interpreter path is ahead, and has one recurring fault

With deploys routed through it, the interpreter reaches **8,666,676** where the
compiler stops at 8,666,674 on `rewards-stx-v1` — a module it compiles and
wasmtime refuses. Zero state-root mismatches on the way, and every root is still
checked against the header, so the path cannot diverge quietly: a wrong root
stops the node exactly as it does under the compiler.

Its own fault is one error, seen three times now on different contracts:

```
<contract>::<public function>: RuntimeCheck(Unreachable("Public function must return response: int"))
```

most recently `signer-payout-v1::initialize`. Ruled out for that one:

- its stored analysis is right — `initialize` is `ResponseType([Bool, Uint])`;
- the first thing it calls, `.dao check-is-protocol`, answers `(ok true)`
  identically in both engines against real state;
- the contract's metadata is present and the same shape as contracts the
  compiler deployed.

So a public function whose declared return is a response is *evaluating* to an
int under the interpreter. That is the one thing between this path and a replay
with no codegen failures in it, and it is worth more than any individual
clarity-wasm bug because it recurs where those do not.

### The interpreter entry was bracketing the call twice

`execute_transaction` brackets a call itself. nano wrapped it in another
`begin`/`commit` on the way in, which left the environment one level deep when it
came to unwind — and `destruct` refuses that, taking the call's own answer with
it. stacks-core's own callers do not do this.

With the extra level gone, the same call reports what it actually hit:

```
signer-payout-v1::initialize: RuntimeCheck(Unreachable("Public function must return response: int"))
```

and it now reproduces outside the node, in one `call-both`, where the compiler
answers `(ok true)`. That is the whole of the remaining difference between the
two engines, and it is now a two-second question instead of a node restart.

The captured replay still passes under `NANO_INTERPRETER_ONLY` with matching
roots, so removing the bracket did not loosen the path it guards.

## Found: the compiler deploys contracts the interpreter cannot run

The stored `ContractContext` for `signer-payout-v1` holds functions whose bodies
are a **stub**:

```json
"define_type":"Private","arguments":["result"],
"body":{"expr":{"LiteralValue":{"Int":0}},"id":0}
```

That is what clar2wasm's deploy writes, and it is not a bug in it: the real
bodies live in the wasm module, so the context only has to name the functions and
their types. The compiler runs the module and never looks. The **interpreter
evaluates the stored body**, gets the literal `0`, and reports exactly what it
found — "Public function must return response: **int**".

So all three sightings — `pox-5::stake-update`, `native-pool-v1::delegate`,
`signer-payout-v1::initialize` — are one thing: **a contract deployed by the
compiler cannot be executed by the interpreter**. Nothing was wrong with pox-5,
the analyses, or the calls preceding them, which is why none of that
investigation found anything.

It also says exactly what the fallback needs. Routing deploys through the
interpreter, which is already done, is necessary but not sufficient: every
contract deployed by the compiler *before* the switch still has a stub body, and
so does every contract nano deploys if the switch is ever off. Either the
interpreter rebuilds a contract's context from its stored source on demand — the
mirror of `ensure_wasm_module`, and nano already keeps the source — or the two
paths must never be mixed on one chainstate.

The source is stored byte-identically to mainnet's, which is what makes the
rebuild possible and is worth knowing before anyone reaches for the second
option.

### The repair is safe to store, which is the part that was not obvious

A contract's definition lives in `metadata_table` — a side store — and never
reaches the MARF. `import_side_store` copies it whole for exactly that reason:
it holds analyses rather than per-key state. So **rewriting a stored contract
context changes no state root**, which is what makes healing one on demand a
legitimate repair rather than a consensus hazard.

`MarfStore::contract_is_interpretable` answers the question before the call
instead of after: a stored definition whose function bodies are the placeholder
`{"expr":{"LiteralValue":{"Int":0}}}` cannot be run by the interpreter.

What remains is the repair itself: parse the stored source — kept byte-identical
to mainnet's — walk its `define-public`, `define-private` and `define-read-only`
forms, rebuild each `DefinedFunction` with its real body, and write the context
back. `DefinedFunction::new` and `ContractContext::functions` are both public, so
no re-execution of top-level expressions is needed — which matters, because
re-running them would reset every data variable the contract has changed since,
and that *would* corrupt state.

## Built: the interpreter heals what the compiler deployed

A contract the compiler deployed carries placeholder bodies, so the interpreter
cannot run it. Rebuilding one means deploying it again — which would re-run its
top-level expressions and reset every data variable it has changed since. So it
is deployed into a **throwaway in-memory store**: the definition that comes out
is the real one, and every side effect lands somewhere dropped a line later.

The rebuilt definition then replaces the stored one directly.
`insert_contract` refuses to overwrite, which is right for a deploy and wrong for
a repair, and writing the side store is safe precisely because that store is not
the MARF — no state root moves.

It heals the contract a call names *and* the contracts that contract references,
because a nested `contract-call?` lands in one the compiler may also have
deployed, and healing only the named one leaves the failure a level down where
nothing names it.

`signer-payout-v1::initialize`, which answered `(ok true)` under the compiler and
an `int` under the interpreter, now answers `(ok true)` under both. The captured
replay still passes under `NANO_INTERPRETER_ONLY` with matching roots, and the
node reached 8,666,680 with no state-root mismatch before the peer began
rate-limiting again.

### And a one-off pass for what was deployed before the switch

Healing on demand fixes a contract the moment a call reaches it, which leaves
every earlier deploy still stubbed. `cargo xtask heal-contracts` does the whole
state at once, and the scale is reassuring: **27 stubbed definitions out of
146,141**. A checkpoint carries real ones, so the repair is bounded by what this
node deployed itself.

23 of the 27 healed. The four that did not are contracts referencing others —
`constants-v1`, `constants-v2` — which the throwaway store does not have, since
it is empty by design. Rebuilding those needs either their dependencies present
or a rebuild that does not deploy at all: parsing the source and constructing
each `DefinedFunction` directly, which touches no other contract. Both are open;
the second is the better one and needs no state at all.

With the pass run, the node executed to 8,666,680 with no execution error of any
kind — only the peer's rate limiting, which is the one thing left in the way of
measuring how far this path actually goes.

### The last four, and where they actually stick

The throwaway store is now seeded with the definitions a contract names, taken
from this node's own state. That did **not** heal the remaining four, and the
reason is worth writing down rather than guessing at again:
`constants-v1` and `constants-v2` *are* in the state — their
`vm-metadata::9::contract` rows are there — but `get_contract` does not find them
from the block the repair pass begins. `contract_source`, which reads the same
contracts' sources happily, goes by another path. So this is a lookup-scope
question, not missing data.

Which points back at the rebuild that avoids the problem entirely: parse the
source and construct each `DefinedFunction` directly, touching no other contract
and needing no store to deploy into. `DefinedFunction::new` and
`ContractContext::functions` are both public. The deploy-into-memory route was
the first one to reach because it reuses machinery that already worked; its
limit is now a fact rather than a guess.

23 of 27 healed either way, and the four are one deployer's family.

### What the clarity API does and does not offer here

Two facts worth having before anyone tries the last four again:

- `Contract` implements `Deref<Target = ContractContext>`, so a stored
  definition's variables, maps and functions **can** be read once you hold one.
  That is what makes a rebuild-and-merge possible at all.
- There is no public `deserialize` for `Contract`. Reading one back from the
  side store as a string and turning it into a `Contract` is not available, so
  seeding the throwaway store has to go through `get_contract` — which is the
  very lookup that does not find these dependencies.

So the remaining route is the one that needs no `Contract` at all: parse the
source, build each `DefinedFunction` with `DefinedFunction::new`, and write the
functions into a context obtained by `Deref` from the contract being healed —
whose own `get_contract` does work, since it is the one being called.
`MarfStore::stored_contract` is in the tree for reading the raw definition,
which the stub check already needed.



## It stops replay again at 8,669,750, and the cheap fix is ruled out


## Objective

Mainnet replay stops at **8,669,750**:

```
FATAL: no burnchain block height found for Stacks block dd254a16…
```

A contract called `get-block-info?` on an older height. The id resolves fine —
that block is at Stacks height **8,661,474**, and nano's MARF holds it, along
with all 8,669,751 entries the checkpoint imported. What nano does not hold is
its **header**: `block_header` carries 4,150 rows, only from the checkpoint
forward.

So the MARF import is not at fault. The headers table beside it is, and a
checkpointed node has to answer for history it did not execute.

## Why this cannot be guessed

The stall is inside `ClarityDatabase::get_stacks_epoch_for_block`
(`clarity_db.rs:2629`), reached from the `get-block-info?` / `get-tenure-info?`
paths (`1265, 1318, 1333, 1341, 1383`). Those sites want the epoch only to ask
`uses_nakamoto_blocks()` — but they are *about to read a real field*.

That rules out the cheap fix. Synthesising a header to get past the epoch check
makes the contract read invented values, and a contract that reads a wrong
miner address or block time answers wrongly and seals a wrong root. **A wrong
answer is worse than the stall**, because the stall is loud and a wrong answer
is a divergence somewhere else entirely.

It also rules out deriving the epoch from a pinned Stacks-height boundary: it
would clear the check and then feed exactly those invented fields.

## What is available

The capture holds 100 blocks from 8,665,601, so this block is not local; it has
to come from a peer. Everything needed is served by a stock node — no Hiro:

| `BlockHeader` field | source | exact? |
|---|---|---|
| `consensus_hash` | `/v3/blocks/:id` header | yes |
| `stacks_block_time` | `/v3/blocks/:id` header timestamp | yes |
| `block_header_hash` | `block_hash()` of that header | yes |
| `burn_spend_total` | header `burn_spent` | yes |
| `burn_header_hash` | `/v3/sortitions/consensus/:ch` | yes |
| `burn_block_height` | same | yes |
| `burn_block_time` | same | yes |
| `vrf_seed` | same | yes |
| `miner_address` | sortition `miner_public_key_hash` | needs checking |
| `burn_spend_winner` | — | no |
| `block_reward` | — | no |
| `tenure_height` | walk the tenure | no |
| `tenure_start_height` | walk the tenure | no |

`SyncClient::block` and `SyncClient::sortition` already exist and are already
cached, so the first eight are a small change.


## Measured: the checkpoint's state ends one block below the anchor

`cargo xtask probe-header <state-dir> <block-id>` reads a block's tenure height
out of the trie and prints it beside the recorded header, so a rebuilt field can
be checked wherever a recorded one exists. Against the live mainnet state:

| Stacks height | tenure height read | recorded header | verdict |
|---|---|---|---|
| 8,661,474 | 8,661,475 | no | **wrong** — the Stacks height, not a tenure |
| 8,665,000 | 8,665,001 | no | **wrong** — same |
| 8,665,600 | 251,320 | no | right |
| 8,665,601 (anchor) | 251,320 | yes | right |
| 8,669,000 | 251,363 | yes | right, and MATCHES the recorded header |

So `marf_block` holds all 8,669,751 blocks — the block index the ancestor
skip-list needs — but **state** exists only from the anchor's parent forward.
Below that the tenure-height key is absent and the read does not fail: it
answers with the block height.

That is the whole danger of this task in one measurement. Reading pre-checkpoint
state *looks* like it works. A backfill that trusted it would have written
`tenure_height = 8,661,475` into a header, and every `block-height` a contract
read at that block would have been wrong — a divergence with no error attached
to it, surfacing later as a state root that does not match for no visible
reason.

It also rules out the route that looked most promising, which was to recover the
hard fields from the imported trie rather than from a peer. They are not there.
The peer fetch in the task list is not one option among several; it is the only
source, and `burn_spend_winner`, `block_reward`, `miner_address` and
`tenure_height` all still need one that is exact.

The probe prints `NOT A TENURE HEIGHT` rather than a number whenever the answer
equals the block height, so this cannot be misread again.

## What the stalling contract actually reads

`block-info-nakamoto-ststx-ratio-v2` makes exactly one such call:

```clarity
(get-stacks-block-info? id-header-hash block)
```

`id-header-hash` is the `StacksBlockId`, which nano holds for every block in
`marf_block`. So the answer this contract needs is already local; what stops it
is the epoch check in front of the read, and that check wants a burn height.

This splits the task in two, and only the first half is needed to move replay:

1. **Burn context** — `burn_header_hash`, `burn_block_height`, `burn_block_time`,
   `vrf_seed`, plus `consensus_hash`, `stacks_block_time`, `block_header_hash`
   and `burn_spend_total`. All exactly recoverable from a stock peer:
   `/v3/blocks/:id` for the header, `/v3/sortitions/consensus/:ch` for the
   sortition. No invention.
2. **Tenure and reward** — `miner_address`, `burn_spend_winner`, `block_reward`,
   `tenure_height`, `tenure_start_height`. Not in the trie (measured above), not
   in one peer call, and not needed by this contract.

The trap is that `BlockHeader` has no absent state, so a backfill that fills
group 1 has to put *something* in group 2, and zeros read as real answers. A
header rebuilt from a peer therefore needs to record which fields it actually
knows, and a read of one it does not must fail loudly and name the field —
which is the same principle as the probe printing `NOT A TENURE HEIGHT`.

## The oracle caught the reconstruction being wrong, before anything used it

`cargo xtask backfill-header <state-dir> <peer-url> <block-id>` rebuilds a
header from `/v3/blocks/:id` and `/v3/sortitions/consensus/:ch`. When the node
already holds a header for that block it writes nothing and diffs instead,
which is the check that makes a reconstruction trustworthy for a block that
cannot be checked.

Run against a known block it immediately failed 3 of 8, and every one was a real
mistake in the section above:

| field | rebuilt | recorded | what was wrong |
|---|---|---|---|
| `burn_spend_total` | 437,713,689,148 | 0 | `header.bitcoin_spent` is a **running** total; `burn_spend_total` is the per-tenure burn. Different quantity. |
| `burn_block_time` | 1,785,437,160 | 0 | nano leaves this zero for blocks it executes itself |
| `vrf_seed` | sortition's | different | the sortition's seed is not the one nano records |

So the earlier claim that eight fields were exactly recoverable was wrong: five
are. The other three are only *plausible*, and filling them from a peer would
have made one block answer differently from every block beside it — the same
silent-divergence shape as trusting the trie.

Matching nano's own convention (zero) instead brings it to **0 of 8 disagreeing**,
and a backfilled header is then indistinguishable from a recorded one.

Applied to the block that stalls replay:

```
recorded a header for dd254a16… at burn height 960142
exact: burn_header_hash, burn_block_height, consensus_hash,
       stacks_block_time, block_header_hash
NOT FILLED: burn_block_time, vrf_seed, burn_spend_total, miner_address,
       burn_spend_winner, block_reward, tenure_height, tenure_start_height
```

### Still open

- `burn_block_time` and `vrf_seed` are zero for **every** nano header, not just
  backfilled ones. Whether stacks-core answers a real value there is a separate
  question and a real divergence if it does — worth its own check.
- The backfill is a manual command. Wiring it into the node's retry loop is the
  remaining task-list item; the mechanism is now proven and oracle-tested.

## Result

With the header backfilled, replay passed 8,669,750 and reached **8,670,485** —
735 blocks — on a clean log: no state root mismatch, no execution error, and no
`no burnchain block height found` at all.

So the mechanism is proven end to end and oracle-checked. What remains on this
task is wiring it into the node's retry loop so a missing header is fetched
without a manual command, and the two open questions above about
`burn_block_time` and `vrf_seed` being zero for every nano header.

## Closed: burn time and VRF seed were zero for every block, not just backfilled ones

The oracle's third and second disagreements were not backfill bugs. Both
execution paths built a Bitcoin context with the height and burn header hash
from the sortition and left `burn_block_time` at zero, and **nothing in the node
ever set `vrf_seed` at all**. So `(get-tenure-info? time)` and
`(get-tenure-info? vrf-seed)` answered zero for every block nano executed, where
mainnet answers real values.

This is the shape of bug that does not announce itself. A missing header stops
the node and says which block; a zero here is a wrong number in a receipt and a
state root that differs for no visible reason, arbitrarily far from the contract
that read it.

The conformance harness builds its context from the sortition snapshot
(`nano-conformance/src/lib.rs:1175`) and so had the right values all along —
which is why its assertion passed while the node was wrong. The gap was between
the harness and the node, not in the oracle.

Fixed on all three paths; the sortition already carried both values on each.
Verified live: a block executed after the fix records `burn time 1785481080`
where every block before it recorded 0.

**Wiring, also done.** The node now fetches a missing ancestor header itself
rather than needing `xtask backfill-header` by hand. The failure names the
block, the five exactly-recoverable fields are written down, and the ordinary
next round picks it up. The parsing is the fragile part and is what has tests:
the real message, the trailing quote that must not join the id, other failures
that must not be mistaken for this one, and a truncated id refused rather than
padded into some other block.

## The narrow backfill is not full header conformance

The automatic path is enough for the contract encountered in replay because it
needed an identifier and burn context. It still writes zero for fields it
cannot reconstruct: miner address, block reward, burn-spend winner and total,
tenure height and tenure start. Those zeros are observable Clarity answers, not
an internal placeholder.

This task therefore remains open after the replay moved on. Its completion bar
is every historical field, with explicit absence until an exact value is known.
Fetching the five known fields from one HTTP peer is a bootstrap mechanism, not
an authenticated long-term source; [[049-derive-canonical-sortitions-from-the-local-burncha]],
[[050-authenticate-every-followed-nakamoto-block]] and
[[054-join-and-synchronize-over-the-stacks-p2p-network]] must eventually supply
or validate that history.

## The pristine clarity-wasm replay says 8,665,719 is *not* a missing header

Replaying from a freshly imported checkpoint with no interpreter, no healed
contracts and no hand-placed headers, execution reaches 8,665,718 and stops:

```
chain   0x04edb5c4… v0-4-market.borrow   success (ok true)
nano    04edb5c4…   RuntimeFailure("Runtime(UnwrapFailure, Some([])))"  (err none)
```

which is the `(unwrap! (get-stacks-block-info? id-header-hash block) …)` this
task was opened for. But with `NANO_TRACE_WRITES` on, the run records
**zero** "no header for block" lookups — and the node's backfill, which now
takes both the loudly-named block and everything the header lookup could not
answer, finds nothing to fetch.

So `get-stacks-block-info?` is answering `none` **before it ever reaches
`HeadersDB`**. In `ClarityDatabase` that happens in the guard ahead of the
lookup — `if current_height <= tenure_height { return Ok(None) }` — which is
about what this node thinks the *current* height is, not about which headers it
holds.

That redirects this task. The header work stands on its own (the epoch lookup
genuinely does need them, and 8,669,750 was genuinely that), but **8,665,719 is
a height-accounting question**, and the next step is to print the tenure height
the contract asks for beside the one the node believes it is at.

### And it is not the block-info read either

`cargo xtask eval <state-dir> <expression>` evaluates any Clarity expression
against a state directory's tip, read-only and rolled back — seconds, no node,
no peer. Against the pristine state stopped at 8,665,718:

```
(is-some (get-stacks-block-info? id-header-hash u251320))  -> true
                                              u251319     -> true
                                              u251310     -> true
                                              u251300     -> true
                                              u251200     -> true
(contract-call? …block-info-nakamoto-ststx-ratio-v2 get-ststx-ratio) -> (ok u1796712)
```

Every read this task attributed the failure to **works**. So the
`UnwrapFailure` in `v0-4-market.borrow` is some other `unwrap!` in that
contract, and this task's diagnosis has now been wrong twice: it is not a
missing header, and it is not `get-stacks-block-info?`.

The header work is still right for what it was actually needed for — the epoch
lookup at 8,669,750 — and should stay. But **8,665,719 does not belong to this
task**, and the next step is to find which unwrap in `borrow` fails, which
`eval` now makes a matter of minutes.

### Where 8,665,719 actually points

`v0-4-market.borrow` binds, in order: `get-asset`, `write-feeds` (pyth price
feeds), `get-position`, then `accrue-user-debts` / `accrue-user-collateral`,
then `accrue-and-cache` — and that last one keys its cache on
**`stacks-block-time`**:

```clarity
(define-private (accrue-and-cache (aid uint))
  (let ((cache-key { timestamp: stacks-block-time, aid: aid })
        (cached? (map-get? index-cache cache-key)))
```

At the tip where execution stops, `xtask eval` says `stacks-block-time` is
`1785402333` and `tenure-height` is `251321` — both real, both from state. So
the next question is which of `borrow`'s bindings is the failing `try!`/unwrap,
and `eval` can ask each of them directly against the same state.

**Caveat on the probe:** `eval` opens a block with no Bitcoin context, so
`burn-block-height` reads `0` there and means nothing. State-derived answers are
real; burn-context ones are not. Noted in the tool's own documentation so the
zero is not chased.

## The consensus decision: an unknown field stops the node, it does not answer `none`

This is the question the task turned on, and it is settled by reading what
stacks-core does with a `HeadersDB` that answers nothing, rather than by taste.
In `ClarityDatabase` (`clarity_db.rs:1311-1537`) every one of these:

```rust
self.headers_db.get_stacks_block_header_hash_for_block(&id_bhh, &epoch)
    .ok_or_else(|| VmInternalError::Expect("Failed to get block data.".into()))?
self.headers_db.get_burnchain_tokens_spent_for_block(&id_bhh, &query_tip, &epoch)
    .ok_or_else(|| VmInternalError::Expect("FATAL: no total burnchain token spend record for block".into()))?
self.headers_db.get_tokens_earned_for_block(&id_bhh, &query_tip, &epoch)
    .ok_or_else(|| VmInternalError::Expect("FATAL: matured block has no recorded reward".into()))?
```

maps a missing field to a **fatal internal error**, never to a Clarity value. The
`none`s a contract can legitimately see come from the guards *above* the lookup:
a height at or beyond the current one, height zero, and — for `block-reward` —
the maturity window. So a block that reaches `HeadersDB` at all is one the
network answered for.

That makes the choice one-sided:

- **Answering `none`** would be a divergence for every block the network has an
  answer for, and a *silent* one: the contract takes its error path, the
  transaction returns something the chain never returned, and the only symptom is
  a state root that differs for no visible reason, arbitrarily far from the read.
- **Stopping** is identical to stacks-core's own behaviour for a header it does
  not have, it is loud, it names the block and the field, and the node's backfill
  can then fetch exactly what is missing and try the block again.

The one exception is real and worth naming, because it is the only header read
`ClarityDatabase` turns into a Clarity `none`:
`get_block_height_for_tenure_height` returns the `HeadersDB` answer as an
`Option`. A checkpointed node answering that from memory alone told contracts
that every pre-checkpoint tenure never happened, and nothing raised a word. It
now falls back to a `tenure_start` table the export fills.

Where absence is genuinely the chain's answer, the export records it as absence
rather than guessing: an epoch 2.x block has no Nakamoto `timestamp` and no
tenure height (stacks-core reads `timestamp` from the Nakamoto table only), and a
tenure whose reward has not matured has no reward — `matured_rewards` is written
100 *tenures* later while Clarity's guard is 100 *Stacks blocks*, so mainnet
itself would raise `FATAL: matured block has no recorded reward` in that window.
nano now behaves identically there, which is the point.

## Built: the checkpoint carries its ancestry's headers

`cargo xtask export-headers <index.sqlite> <out.sqlite> <to-height>` reads
stacks-core's chainstate index and writes one row per block, through the same
joins stacks-core's own `HeadersDB` uses — which matters, because five of the
thirteen fields are resolved at the block's **tenure start** and not at the
block: `vrf_seed` (the hash of the tenure's proof), `miner_address`,
`burn_spend_total` (`payments.burnchain_sortition_burn`), `burn_spend_winner`
(`burnchain_commit_burn`) and `block_reward` (two `matured_rewards` rows
combined, the child's coinbase and anchored fees plus the parent's streamed).

Measured on the mainnet archive at the capture's own checkpoint height:

```
exported 8679508 headers (8493877 Nakamoto, 185631 from epoch 2.x) up to height 8665600
188014 of them are incomplete, and say so
```

11 minutes, 2.4 GB beside a 380 GB checkpoint. The import is one pass into the
side store — `cargo xtask import-headers <state-dir> <export>` for a state that
already exists, and automatically inside the checkpoint import for a new one,
*inside* the unfinished-import mark, because a state whose trie covers the
ancestry and whose headers stop at the anchor is exactly the state this exists to
stop shipping. 8,679,508 headers imported into a live 11 GB `clarity.sqlite` in
110 seconds.

`capture-fixtures` now runs the export into `checkpoint-H/`, so a capture carries
it without a second command. A field the checkpoint carries needs no peer, which
is the end state the task asked for.

## The oracle: stacks-core's own `HeadersDB`, field by field

`tests/conformance/pre_checkpoint_headers.rs` opens the archive as a read-only
`MARF<StacksBlockId>` — which stacks-core implements `HeadersDB` for — and asks
it all thirteen questions about sixteen blocks spread from height 1 to the
anchor, across the 2.x/Nakamoto boundary. nano is asked the same questions
through its own context after importing the export. **Every field matches, and
every absence matches too.**

Field by field rather than header by header, deliberately: a header compared as a
whole value would compare the zero sitting where an unknown field is against a
real answer and pass.

Two details cost a run each and are worth writing down: the archive must be
opened with `external_blobs`, or the fork index reads the blob column and reports
the archive corrupt; and the epoch passed to each read has to be the epoch of the
*block being read*, since it decides which header table stacks-core looks in.

What the run leaves behind replays with neither archive nor capture: the answers
and a 16-row slice of the export are written to
`fixtures/mainnet/headers/`, and the second test in the file checks them offline.
They are written only after the comparison passes, so the offline fixture cannot
record an answer the oracle disagreed with.

## What a pre-checkpoint block now answers

`cargo xtask block-info <state-dir> <height>` asks through `ClarityDatabase`,
which is where the guards live. `xtask eval` cannot answer this and it is worth
knowing why: it evaluates against `null_context()`, so every header read there is
`none` whatever the state holds — the documented "burn-block-height reads 0"
caveat is really "no chain at all".

Against the live mainnet state with the export imported:

```
get-stacks-block-info? at height 8660000
  id-header-hash    957c5bf4b16420479e7326a8077194c3eeb6788e4e68fc57928173d58bd60b59
  header-hash       1b3644b6bedf928e2e37f970e45d2178aafb32c96ecc983e29d5c0b96d9c9d27
  time              1785343118
get-tenure-info? for the tenure at that height
  burnchain-header-hash 000000000000000000016792d9c1460bfa4db07da3b724db76c3dd0640ca5754
  miner-address         SPVYF6M3FD0738FWDGDMJ84EFMGM38JS0N7DE42K
  block-reward          1084578350
  miner-spend-total     185000
  miner-spend-winner    40000
  vrf-seed              e324660f2da92ea029443b170eb4896de109aa5b15f92dec7b4d2396734b67c2
  time                  1785342267
```

Every one of those is the value stacks-core answers. Before the import the same
state said:

```
header: none, though the block is in this node's index -- this is the case the
node stalls on, and the case a header export fixes
```

## Three kinds of nothing, and why the difference is worth a table

`HeaderKnowledge` now distinguishes `Held`, `NeverCarried` and `Absent`, and
`probe-header` prints which. They are three different faults:

| | means | what to do |
|---|---|---|
| `Held` with every field | this node executed the block, or imported it | nothing |
| `Held` short of a field | a reconstruction that recovered part of one | fetch the rest; a read of a missing field stops the node and names it |
| `NeverCarried` | in the block index, so on this fork, but no header | fetch it |
| `Absent` | not on this fork at all | nothing to fetch — whatever resolved a height to this identifier is the fault |

The index is asked rather than assumed, through a second read-only connection to
`marf.sqlite` — the same reason the headers already have one, since the store is
mutably borrowed while Clarity runs. A read that cannot be made answers "on this
fork", because asking a peer for a header this node turns out not to need costs
one request, while calling a block absent that is really here hides a header that
could have been fetched.

## Two things the traces caught, which reasoning had missed

**A partial header could undo an import.** The node writes down any block a field
read came up empty for, and a header can be short of a field *the chain* has no
answer for — an unmatured reward, an epoch 2.x timestamp. So the very blocks the
export answers twelve fields for are the blocks a peer gets asked about, and
`record_partial_header` would have replaced those twelve with five. It now keeps
whichever record knows more, tie to the one already held. Deliberately not a
field-wise union: a peer will happily produce a plausible `stacks_block_time` for
an epoch 2.x block, where stacks-core answers nothing, and unioning would take it.
Pinned both directions by `a_partial_header_cannot_overwrite_what_is_already_known`.

**The captured window reads no pre-checkpoint history at all.** Measured, not
assumed: the whole conformance suite under `NANO_TRACE_WRITES` makes 14,234 writes
and exactly **zero** reads of a header this node does not hold — the ten traces
that do appear are this task's own test asking every field of every sampled block,
including the ones stacks-core also answers `None` for. That is why the last
task-list item cannot be closed from the fixtures in the tree: no captured block
depends on this, and the block known to depend on it is 8,669,750.

**An in-memory VM wrote a file to disk.** The second read-only connection needs
the trie's path, which is derived from the side store's — and an in-memory store
reports a path naming no file, so the derived sibling was a *relative*
`marf.sqlite` that the connection then created in whatever directory the process
was in. Two turned up in crate roots after one test run. The file name is now
checked rather than assumed.

**A height can name more than one block.** At heights up to 1,000 the export holds
1,074 rows, because 2.x forks left several blocks per height in the archive.
Harmless, and worth knowing: rows are keyed by `index_block_hash`, so no
non-canonical block can shadow a canonical one, and Clarity only ever reaches a
block by resolving a height through the MARF on the fork being executed. The
export therefore does not need an ancestry walk — the extra rows are unreachable.

The refactor for `clippy::too_many_lines` was checked rather than eyeballed: a
second export of every block up to height 1,000 is byte-identical to the 2.4 GB
run in all seventeen columns, all 1,074 rows.

## What is left, and one change outside this branch's files

The last task-list item — replaying across a block whose transactions read
pre-checkpoint history and matching the state root — is not checked off. The
block known to need it is 8,669,750, four thousand blocks past the state
available here, and reaching it needs a peer and hours rather than a command.
What is proven instead is stronger per unit of evidence and weaker in kind: every
field the network answers for such a block is answered, and answered with the
network's value, checked against stacks-core rather than against nano's own
export.

**Needed in `nano-chainstate`, which this branch does not own:**
`backfill_ancestor_header` (`crates/nano-chainstate/src/lib.rs:1937`) builds a
`nano_vm::BlockHeader` with zeros for the eight fields a peer cannot answer and
calls `record_block_header`, which means "every field is the chain's answer". It
should call

```rust
self.vm.record_partial_header(block, header, nano_vm::HeaderFields::PEER_BURN_CONTEXT)
```

instead — one call, no signature change. Until it does, the node's automatic peer
backfill still records those zeros as answers. `HeaderFields::PEER_BURN_CONTEXT`
exists precisely so that change is a one-liner, and `xtask backfill-header`
already records partially, so the mechanism is exercised.

## The block that reads pre-checkpoint history is behind us

This item could not be closed when the header work landed: measured under
`NANO_TRACE_WRITES`, the whole conformance suite makes **zero** reads of a header
this node lacks, so no captured block exercises the path. The block that does is
**8,669,750** — the epoch lookup — about 4,000 blocks past any state that existed
then.

The pristine mainnet replay has since executed it. Depth is past 8,693,400, and
8,669,750 is not among the six heights that have ever diverged (8,665,719,
8,665,722, 8,666,585, 8,667,509, 8,668,096, 8,673,846). It executed and its state
root matched the chain's, with the exported headers in place and no peer asked.

That is the criterion: a block whose transactions read pre-checkpoint history,
replayed, matching the chain. It is live evidence rather than a fixture, and the
distinction matters — the offline oracle
(`pre_checkpoint_headers::…`) proves nano answers all thirteen fields the way
stacks-core's own `HeadersDB` does for sixteen blocks from height 1 to the anchor,
and this proves a real contract reading one of them produces the root the network
produced.

**What the two together do not cover**, and it is worth naming: the *exactness* of
`block_reward` for the ~100 tenures immediately below the checkpoint, where
`matured_rewards` has not been written yet and the export declares the field
absent. A contract reading one gets a loud stop rather than a wrong number, which
is the decision this task made — but no mainnet block in the replayed range has
asked, so the *decision* is proved and its consequence for a contract that does ask
is not.
