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
- [x] Charge an aborted expression for what it did, not for the enclosing work
      it never reached. One of the nine divergences was this.
- [x] Charge `print` for the value rather than for its type's name.
- [x] Crosscheck against the real `.pox-5` rather than a snippet — it deploys
      into the harness, and found the constants.
- [x] Seed the state a call runs against, not just the contract — a driver
      contract over two environments does it, and found another 31.
- [ ] Bisect `.pox-5 stake` for that 31, and the 66 for `stake-update`.
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

### An aborted expression paid for work it never did — fixed

A word the interpreter treats as a **native function** is charged through
`dispatch_args`, once its arguments have been evaluated. nano charged several
of them before. While everything succeeds that is invisible — the same charges
land, in a different order — and the moment an operand aborts it overcharges,
because the enclosing work was paid for and never done.

It compounds with nesting:

| enclosing operations | 0 | 1 | 2 | 3 |
|---|---|---|---|---|
| over by | 31 | 62 | 93 | 124 |

`ok`, `err`, `some` and `begin` now charge after their operands, and the
variadic dispatcher evaluates every argument before charging rather than
folding pairwise as it goes. Special forms such as `map` and `if` genuinely do
charge first, which is why this is per word and not a rule about all of them.

This is consensus-visible — stacks-core records the lower cost — and it shows
on real chain data. Block 146 is a `.pox-5 stake-update` that aborts with
`(err u47)` and was over by exactly 31. It now matches, and the capture's cost
divergences go from **nine to eight**.

Finding it needed a wrong turn first: a probe showed arithmetic on a
`burn-block-height` argument diverging, which was the snippet underflowing,
because `burn-block-height` is zero in the test environment. The divergence was
real but it was the *abort* it caused, not the arithmetic. Two of the three
false leads this task has produced were snippets that measured the engines
doing something other than the work.

### `print` charged for the type's name — fixed

`special_print` charges for `input.size()`. nano charged for the length of the
type's *textual form*: a different quantity that sits close for simple values
and drifts as soon as a tuple carries long field names — which is the shape
every `.pox-5` event print has, and `stake-update` prints a merged eleven-field
tuple.

Eleven shapes crosscheck exactly now, where a tuple with long names was short
by one and a forty-field one by two.

It moves the first divergence from 65 to **66**, because nano was under here
and is over elsewhere. Both are right and the net is not the measure of either
— the same thing happened when the fold lookup went in.

### Crosschecking against the real `.pox-5` — and what it found

The harness *can* take the real contract after all. `.pox-5` deploys into it
with the sBTC literal substituted the way `make_pox_5_body` substitutes it
off-mainnet, a stub token and a signer-manager beside it — 3,851 lines, both
engines, no chainstate.

Every read-only function was out by the same **1,032**, whichever one was
called, which is the shape of a fixed charge rather than of any function body.
It was the contract load: `save_constant` inserted a constant's value and left
`data_size` alone, so every constant a contract defines was free to load. The
deficit is exactly the value's size — a bool 1, a uint 16, a response 17 — and
`.pox-5` defines around sixty.

A `contract-call?` pays `LoadContract` for the contract's size, so **every call
into a contract nano deployed was charged less than stacks-core charges**. Its
read-only functions now crosscheck exactly.

It does not move this capture, whose `.pox-5` was deployed by stacks-core and
carries its size — which is also why replaying someone else's chain never
showed it.

### Seeding the state — where the next one is

The harness runs a *sequence* against the real contract, not just one call:
build two `TestEnvironment`s, deploy `sbtc-token`, `pox-5`, a signer-manager
and a driver into both, then call through the driver, snapshotting
`cost_tracker.get_total()` either side of each call. That seeds `.pox-5`'s own
maps with `.pox-5`'s own code, which is as close to the real thing as anything
offline gets.

Doing that turns up **another 31**: `stake` returning `ERR_SIGNER_NOT_FOUND`
costs 31 too much, while `stake-update` returning `ERR_NOT_STAKING` on the same
state is exact.

It is not the shapes already fixed. A failing `unwrap!` returning a constant is
exact, directly and inside a `let`; so is an error propagated by `try!` out of
another contract, with or without work after it, and so is `unwrap-err!` across
the same boundary.

So it is somewhere in `stake`'s own path, and finding it means bisecting 3,851
lines rather than guessing at shapes — the reduction the earlier ones needed,
against a contract large enough that it wants doing carefully.

### What the remaining 66 is not

The eight that remain are all **successful** transactions: seven `.pox-5
stake-update` and one `.pox-5 stake`.

Trait arguments fitted those numbers and are not the cause. A probe saying
otherwise had called the functions with fewer arguments than they declare; with
the signature matched, traits, `uint` and `principal` arguments all crosscheck
exactly, as do `contract-of`, `stx-account`, `merge` and `print`.

Nor is it dispatch: a `contract-call?` through a trait crosschecks exactly, at
one trait argument and at two, as does a static one. Nor read-only calls,
nested or otherwise, nor the block-height keywords.

Nor is it any single word `stake-update` uses: `map-set` over a scalar and a
tuple, `map-insert`, `map-delete`, `var-set`, `var-get`, `get`, nested `get`,
`default-to`, tuple construction, `let`, `begin`, `match`, `is-some`,
`to-uint`, `unwrap!` — every one exact.

Nor the pox-5 iteration shape: a `fold` over `(unwrap-panic (slice? (list u0 …
u95) u0 n))` with a response-tuple accumulator — what `remove-staker-from-cycles`
and `add-staker-to-signer-cycles` do — is exact, as are map reads and writes
whose declared types are far larger than what they hold.

Nano adds nothing outside the VM either: the transaction's cost tracker starts
at zero, so what remains is inside the compiler.

What is left is the state the call runs against. The contract is no longer the
unknown — it crosschecks exactly on a fresh store — so what differs must be the
maps `stake-update` reads and writes on a chain that has been running, and the
sizes of what they hold. Seeding that state, rather than the contract, is the
next step.

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
