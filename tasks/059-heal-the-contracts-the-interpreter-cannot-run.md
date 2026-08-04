---
id: "059"
title: "Heal the contracts the interpreter cannot run"
status: completed
priority: critical
effort: medium
type: bug
group: mainnet
dependencies: ["037"]
tags: ["mainnet", "vm", "clarity"]
created_at: 2026-08-03
---

# Heal the contracts the interpreter cannot run

## Objective

A contract deployed by clar2wasm stores placeholder function bodies, because the
real ones live in the wasm module. The interpreter cannot run such a contract,
so any path that falls back to it fails on a contract the compiler deployed.

This is diagnostic and migration tooling, not the production execution plan.
Mainnet conformance requires clarity-wasm to execute without interpreter
fallback as tracked by
[[060-make-the-consensus-execution-engine-explicit-and-r]]. A replay that heals
contracts or switches to the interpreter can locate a compiler bug, but cannot
close the release gate.


## All 27 contracts heal, and the compiler stopped being able to stop the chain

The last four could not be rebuilt by deploying into a throwaway store, however
many dependencies were put in first: they name contracts this node's state does
not hold, and a contract cannot be deployed beside nothing. `Contract` has no
public `deserialize`, so seeding the store from the side store was closed too.

The route that needs no `Contract` works. Parse the source, build each function
with `DefinedFunction::new`, and merge them into the contract's *own* stored
context — reachable because `From<ContractContext> for Contract` exists and
`Deref` gives the context back. Nothing is deployed, which matters twice: no
other contract has to be present, and no top-level expression runs. Re-running
them would reset every data variable the contract has changed since, corrupting
state rather than healing it. A second pass reports `0 contracts`.

That moved the node 8,666,816 → 8,667,466 and then stopped on a different
fault, which turned out to be the more important one.

### A clarity-wasm codegen bug can no longer stop a mainnet replay

Block 8,667,467 failed on `v0-egroup`, which clar2wasm builds into a module
wasmtime refuses: "expected i64, found i32". Delta-debugging its 49 top-level
forms to four, then by hand to two, names the cause exactly:

```clarity
(define-private (it (m uint) (acc {t: uint, r: (optional uint)})) acc)
(define-private (f (target uint) (masks (list 128 uint)))
  (let ((init { t: target, r: none }))     ;; <- (optional NoType), one slot
    (get r (fold it masks init))))          ;; <- read as three
```

Passing the same tuple *inline* compiles, because `fold` sets the expected type
on the expression it is about to lay out; a `let` has already stored the narrow
one by then. `words/tuples.rs` carries two workarounds for this same
typechecker limitation. Fixing it properly is unification, and the chain does
not need it — **mainnet runs the interpreter**. So where the compiler cannot
build a loadable module, the interpreter decides, at both boundaries:

- **deploy** — it stores what is sound and rejects what is not
- **call** — that one failure is answered by it

Deliberately narrower than the `NANO_INTERPRETER_FALLBACK` beside it, which
replaces any runtime failure. A genuine runtime error is a real answer;
substituting it would hide a divergence instead of carrying one forward.

### A consensus gap the fallback exposed

The interpreter deploy path called `initialize_versioned_contract` **without
ever running the static analysis stacks-core runs first**, so it accepted
contracts the chain rejects — a contract naming a map that does not exist
deployed cleanly. Found only because the fallback made that path load-bearing.
It now type-checks first, and `compiler_refusal_fallback.rs` pins all three
cases, including that an unsound contract is still refused.

### Where replay stands

**8,666,680 → 8,668,160** this session (+1,480). The node then stops at
**8,668,161**, and this one is a different class: *every receipt succeeds and
only the state root differs*. Per plan.md that reads as MARF or write ordering
rather than execution — the first divergence in a while that is not a VM bug,
and the next thing to look at.


## The divergence at 8,668,161 was not the MARF, and it invoked the tripwire

The receipts *looked* fine because every transaction succeeded. Against
mainnet's own receipts two differ:

| tx | mainnet | nano (compiler) |
|---|---|---|
| `3ff1aff7` | `(ok u418181) (err u9) (err u9) …` | `(ok u418181) (err u9) (err u2) …` |
| `88d21a09` | `(err u9) (ok u748)` | `(err u9) (err u2)` |

`(err u2)` from `stx-transfer?` is sender == recipient — the wrong-principal
class again. `xtask call-both` on `SP2H674PRTZV6YW56K0FMR7GDGZE4ZC5HMYZ3CDEV
.loto::ri`, whose argument is a trait reference to `.hilt`, settles it:

```
compiler     [(err u9), (err u2)]
interpreter  [(err u9), (err u9)]
```

The engines disagree, so it is clarity-wasm — the third such bug in this same
routing shape, after `as-contract` typing and `merge` coercion. **Checking
receipts against the chain rather than only reading them is what told the
difference**; a root mismatch with all-successful receipts reads as MARF, and
this was not.

### `NANO_INTERPRETER_ONLY=1` replays mainnet where the compiler cannot

plan.md's highest-value tripwire, invoked: *"clarity-wasm not compiling … point
`nano-vm` at the clarity interpreter instead."* The switch already existed. With
it set the node cleared 8,668,161 and kept going:

**8,668,160 → 8,669,750, zero state root mismatches.**

That is the answer to the write-ordering worry recorded beside the fallback: the
two engines were only *evidenced* to seal the same roots by `engine_state_roots`,
and 1,590 consecutive mainnet blocks is much better evidence.

One thing it needs, and it is not optional: **a contract the compiler deployed
cannot be run by the interpreter**, so switching engines mid-chain strands every
contract deployed by the earlier arm. 39 had accumulated over the compiler's
1,344 blocks, and `xtask heal-contracts` fixed all 39. Deploys in interpreter
mode store real bodies, so this is self-correcting from here — but a run that
switches engines must heal first, or it stops on the first call into one of them
(here `pox-5::stake`, which is how it was found).

### Where replay stands

**8,666,680 → 8,669,750 this session (+3,070).** Mainnet tip is 8,698,348, so
the remaining gap is ~28,600 blocks and the checkpoint means those are the only
ones that ever need executing.

## Next blocker: a contract asking about a block older than the checkpoint

At 8,669,750 replay stops on:

```
FATAL: no burnchain block height found for Stacks block dd254a16…
```

That is `ClarityDatabase::get_stacks_epoch_for_block`, reached from the
`get-block-info?` / `get-tenure-info?` paths (`clarity_db.rs:1265, 1318, 1333,
1341, 1383`). A contract asks about an older height, the id is resolved through
the MARF's `__MARF_BLOCK_HEIGHT_TO_HASH`, and `HeadersDB` is then asked for that
block's burn height — which nano does not have, because it holds headers only
from the checkpoint forward.

Note what is *not* wrong: the MARF answers correctly, so the checkpoint import
is fine. The gap is the headers table beside it.

The epoch is all these call sites want, and only to ask
`epoch.uses_nakamoto_blocks()`. Two routes, neither yet chosen:

- carry pre-checkpoint headers (or just burn heights) in the checkpoint, beside
  the `(height → block_hash, archival_root_hash)` table PCS already imports
- answer from the mainnet epoch schedule, whose boundaries are fixed constants

The second is far less data but has to be exactly right about the 3.0 boundary,
since a wrong `uses_nakamoto_blocks()` changes what a contract reads. It wants
its own oracle test against stacks-core before it goes in — this is consensus,
and guessing it would be worse than the stall.
