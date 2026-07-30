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
- [ ] Work forward through the fixtures until the cost row reaches full depth.
      The fixtures now come from the pinned revision, so what remains is nano's:
      runtime is high by 843 in 231,186 at the first divergence.
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

The residual is **not** in argument handling. Four fixes and one ruling-out
have failed to move it:

- byte sequences, lists and UTF-8 strings charged at runtime size
- optionals and responses charged at the branch taken
- trait arguments, which declare 276 against a principal's 148, were the best
  remaining theory and are **correct**: a trait crosscheck passes

So it is inside `stake-update`'s body — the trait dispatch to
`signer-manager-validate-stake`, `remove-staker-from-cycles`'s loops, or
`stx-account`. Finding it means bisecting that body the way the earlier eight
were found; every crosscheck case here is the harness to do it with.

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
