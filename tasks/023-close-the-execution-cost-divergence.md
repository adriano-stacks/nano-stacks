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
- [ ] Charge a fold over a native word the application it inlines, 31 an
      element. It is *not* a user-function application — charging one costs 57
      an element and overshoots — so what the interpreter pays there has to be
      identified first. Then un-ignore `charges_a_native_fold`.
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

One known divergence is left, and it is not the 65: a fold over a *native*
word is short by **31 an element**, because nano inlines the word without
paying to apply it. Over a user-defined function a fold is exact.

Charging a user-function application per element was the obvious guess and it
is wrong — that costs 57 an element and overshoots by 26. So the 31 is
something else, and it needs identifying rather than fitting.

What accounts for the 65 itself is not yet identified. The row is not green
and the task is not done.

Also still open, and in the other direction: `fold`/`map`/`filter` miss the
function-argument lookup, 16 plus 1 per element.

Reducing `stake-update` to the snippet that over-charges is the next step, the
same way the earlier eight were found.

## Hacknet

Not validated on a live chain. Two runs tried. The first ended on nano's side,
which is now fixed ([[041-walk-back-when-our-tip-left-the-chain]]); on the
second nano ran for minutes, signing for its cycle, while the network itself
stayed frozen — stock signers looping on `Last accepted block has timed out`
and `Cannot validate block, no global signer state`, with the tip already stuck
before nano rejoined.

That Hacknet has now deadlocked itself in three of five boots, so a live
measurement of these cost changes needs a network that stays up, not another
attempt.

The offline evidence stands on its own — every cost change here is crosschecked
against the interpreter, and the 340-block replay matches state roots and
receipts — but a live chain has not seen these changes.
