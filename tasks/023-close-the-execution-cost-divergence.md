---
id: "023"
title: "Close the execution cost divergence"
status: pending
priority: critical
effort: large
type: bug
dependencies: []
tags: ["mainnet", "vm", "costs"]
created_at: 2026-07-30
---

# Close the execution cost divergence

## Objective

The scoreboard's cost row fails at block 22:

```
replay: costs   receipt cost dimensions   21/600
block 22: transaction 0 (e5586509…) cost differs:
  expected (62, 134492, 481082, 19, 1667)
  got      (60, 134664, 366479, 19, 1657)
```

The plan's tripwire said to ship this and fix it after M10, on the grounds that
costs only diverge near a block limit. That reasoning does not survive mainnet.
`crates/nano-chainstate/src/lib.rs:836` enforces `EPOCH_4_BLOCK_LIMIT`, and
SIP-034 per-dimension tenure extends are triggered by these same dimensions. A
`read_length` that is thirty percent low decides differently from stacks-core
about when a block is full and when a tenure must be extended.

## Tasks

- [x] Reduce the block-22 transaction to the smallest snippet that diverges.
- [x] Fix the vendored compiler's charging for it and keep the case as a
      regression test.
- [x] Reduce the remaining divergence to the words that cause it.
- [x] Stop charging a copy for operands the interpreter reads in place.
- [x] Account for `fold`'s function-argument lookup.
- [x] Charge `asserts!` its predicate copy, and `list` by the size of what it
      holds rather than how many.
- [x] Charge a list of a sequence type by the elements' runtime sizes, and
      `append` by the size of an entry rather than how many there are.
- [x] Charge a fold over a native word the application it inlines, and the
      function-argument lookup `map` and `filter` skip. `charges_a_native_fold`
      is un-ignored and covers all three forms.
- [ ] Find what remains of the 65.
- [x] Assert per-snippet dimension equality against the interpreter, not only
      block-level acceptance.

## Acceptance Criteria

- The scoreboard's cost row matches its state-root and receipt rows.
- All five dimensions match the interpreter on the crosscheck suite.
- The reduced cases stay in the suite as regression coverage.

## Where it got to

Four of the five dimensions now match the node exactly, where three diverged
before:

```
before  got (60, 134492 -> 134664, 481082 -> 366479, 19, 1657)
after   got (62, 134492,            240515,          19, 1667)
```

Eight real charging bugs were behind that, the largest being that a rebase
had silently dropped every cost patch nano had added, so epoch 4.0 was paying
Clarity-3 prices for arithmetic, sequences, tuples, hashing and data access,
and never paid for resolving a name to a function at all.

The row still cannot reach full depth against the fixtures in the tree, and the
reason is not nano's: they charge `costs-4` runtime at epoch 4.0 while the
pinned stacks-core charges `costs-5`.

That is now measured rather than argued. A capture taken from a Hacknet built
at the pinned revision replays with four dimensions matching exactly and
runtime differing by 843 in 231,186 — **0.36%**, the residual named below —
where the stale fixtures differ by a factor of two. See
[[038-recapture-the-fixtures-from-the-pinned-revision]].

Charging an argument for what it holds rather than for what its type allows is
now done for byte sequences: from epoch 3.3 the interpreter charges
`arg.size()`, and a buffer or ASCII string carries its length on the wasm
stack, so `4 + len` is known where the compiler stands. A crosscheck case pins
it — it fails without the change and passes with it.

Lists and UTF-8 strings now take their runtime size too. A list's stack length
is bytes over the entry's width (`words/sequences.rs`, `Len`) and a UTF-8
string is four-byte scalars, which makes it `4 + length` like a buffer. Every
case is crosschecked against the interpreter.

Optionals and responses now do too. A wrapper's declared size is its widest
branch — `(optional (buff 500))` holding two bytes declares 505 and is 7 — so
the size follows the discriminant and the branch actually taken. Before this a
single such argument was charged 2,237 runtime against the interpreter's 245.

What remains, measured against a capture from the pinned revision, is runtime
high by **843 in 231,186 — 0.36%** — at the first divergence. That transaction
is a `.pox-5 stake-update` call.

Bisecting `stake-update`'s body found it, and it was not argument handling at
all. An **ordered comparison against a bound name** cost 33 too much and an
**`if` with a computed condition** cost 31 — both constant, both once per bound
name, neither varying with the operand types. Two literals compared exactly,
and so did `is-eq` and `+` over the same binding.

The cause: the interpreter reads those operands where they are, while nano
charged them as copies out of their bindings. `traverse_expr_without_value_copy_charge`
already existed for exactly this and the comparisons did not use it. `<`, `<=`,
`>`, `>=` and `if`'s condition now do, and every case is a passing crosscheck
rather than a reproduction.

Sweeping the words pox-5 leans on found the same pattern in three more places
— `and`, `or` and `stx-account` all charged a copy for an operand read in
place — and one omission: `fold` never paid to resolve the function it folds,
which every other application pays for.

The comparison and branch fixes took the divergence from **843 to 513**; adding
the fold lookup put it back to **545**, because that charge was genuinely
missing and nano over-charges elsewhere. Both are right and the net number is
not the measure of either.

Sweeping again found the largest one of all: **`map-get?` charged its key as a
copy**, 33 a lookup, and pox-5 reads maps constantly. Fixing it moved the
transaction by 660.

Where it now stands, against a capture from the pinned revision:

| | runtime | against 231,186 |
|---|---|---|
| stale fixtures | 240,515 | a factor of two out |
| pinned fixtures, before this work | 232,029 | **843 over** |
| after | 231,251 | **65 over** |

Two more came out of the sweep after that: `asserts!` was suppressing a copy
the interpreter does make, and `list` charged how many elements it holds rather
than the sum of their sizes, which is what `list_cons` charges. The second one
also closed the fold gap — a fold's under-charge was its list literal all along.

Fifteen charging bugs are fixed, each a passing crosscheck against the
interpreter, and the copy cost turns out to be exactly `2n + 1`: a bool 3, a
uint 33, a principal 297. That is what made each one recognisable.

Two more followed from the same reading: a list of a sequence type now
measures its elements at runtime rather than taking the type's maximum — only
for sequence elements, because measuring a scalar would burn a wasm local an
element and a large list exceeds the limit — and `append` charges the size of
an entry rather than how many entries the result has.

Seventeen charging bugs are fixed, each a passing crosscheck, and `.pox-5
stake-update` sits **65 out of 231,186 — 0.028%, from 0.36%**.

### Applying a word to every element — fixed

`fold`, `map` and `filter` call a word's `visit` directly, which skips the
dispatcher, and two charges went with it.

A **variadic** word is charged by its *caller* rather than by its own `visit`,
so `(fold * ...)` never paid for the multiply — 31 an element. A non-variadic
word charges itself, so only the variadic branch was short; charging both
double-charged `(map not ...)` by the same 31, which is how the asymmetry
showed itself.

Resolving the applied function's name is a flat 16 that `fold` already paid and
`map` and `filter` did not. And `map` charged for how long its sequences turned
out to be, where `special_map` charges for how many arguments it was applied
to.

All exact now, on one to forty elements, over natives and user functions alike.
Eight recorded word-cost snapshots moved with it and were re-recorded.

### What the 65 is not

Enumerating every cost divergence across the 340 blocks gives nine, all
runtime-only, all nano **over**, and all in one contract:

| blocks | function | over by |
|---|---|---|
| 76, 134, 177, 237, 339 | `.pox-5 stake-update` | 65 |
| 77, 79 | `.pox-5 stake-update` | 65 |
| 146 | `.pox-5 stake-update` | 31 |
| 179 | `.pox-5 stake` | 32 |

That the same function is out by 65 on one path and 31 on another says it is
charged per *something* the path varies, not once a call.

`stake-update` takes two trait arguments and `stake` one, which fit 65 and 32
well enough to be worth testing. They are not the cause: `contract-of`,
`stx-account`, `merge` and `print` all crosscheck exactly, and so do trait,
`uint` and `principal` arguments.

A probe suggesting otherwise was wrong — it called functions with fewer
arguments than they declare, and `cost_crosscheck`'s argument list has to match
the signature or both engines are measured doing something other than the work.
Worth remembering: a divergence that scales with parameter count is the shape
that artefact produces.

## Hacknet

**Validated.** With all seventeen charging fixes in, `harness.sh verify`
against a fresh network:

```
observed 47 canonical blocks across cycles 15..=16
every one of the 47 blocks carries nano's signature
nano mined 16 of the 47 canonical blocks
29 transfer, 4 deploy, 59 call, 8 tenure change, 8 coinbase transactions,
  each with one the network reports as success
9 sortitions across 3 distinct miners
reward cycle 16 pays a waterfall set in which nano holds weight 10 of 30
```

Hacknet runs three signers of equal weight against a seven-tenths threshold, so
no block is accepted without all three: a network that keeps producing with
nano signing and mining is proof the cost changes did not move anything the
network disagrees with.

It took four attempts. Three earlier networks deadlocked on their own stock
signers — `Last accepted block has timed out`, `Cannot validate block, no
global signer state` — which was confirmed as not nano's by restoring the stock
participant and watching the tip stay frozen anyway.

The offline evidence stands on its own — every cost change here is crosschecked
against the interpreter, and the 340-block replay matches state roots and
receipts.

The sequence-application fixes landed after that run and have not themselves
been on a live chain.
