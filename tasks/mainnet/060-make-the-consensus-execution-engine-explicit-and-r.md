---
id: "060"
title: "Make clarity-wasm the conformant production execution engine"
status: pending
priority: critical
effort: medium
type: bug
group: mainnet
tags: ["mainnet", "vm", "clarity", "conformance", "release"]
created_at: 2026-08-04
---

# Make clarity-wasm the conformant production execution engine

## Objective

Make clarity-wasm the only production consensus execution path and close every
known difference from stacks-core's Clarity semantics. Mainnet replay currently
advances only with `NANO_INTERPRETER_ONLY=1`; the clarity-wasm path has produced
different principal routing and transaction results on accepted mainnet blocks.
That is a compiler conformance bug, not permission to substitute another engine.

The interpreter remains a differential oracle: run the same transaction against
both engines in a rolled-back diagnostic bracket to localize a disagreement.
It must not answer a production transaction after clarity-wasm refuses or
returns a different result. [[059-heal-the-contracts-the-interpreter-cannot-run]]
is useful for investigating an old compiler-created state, but healing or an
engine switch is not mainnet conformance evidence.

## Tasks

- [ ] Reproduce and minimize every known mainnet clarity-wasm divergence,
      including the trait-reference/wrong-principal failure at 8,668,161.
- [ ] Fix the compiler, runtime ABI or generated-code boundary responsible for
      each disagreement; do not route the transaction or deployment through the
      interpreter in production.
- [ ] Remove interpreter-only and interpreter-fallback behavior from the
      production node configuration, or make startup reject those diagnostic
      switches on mainnet.
- [ ] Keep the interpreter differential oracle behind an explicit test or
      diagnostic mode whose writes are always rolled back.
- [ ] Replay from a pristine checkpoint entirely through clarity-wasm, including
      compiler-hostile deployments and calls, with no healing or engine switch.
- [ ] Compare clarity-wasm with the interpreter before sealing in the
      conformance harness and retain minimized regression fixtures for every
      disagreement found.
- [ ] Pin roots, receipts, costs, events and consensus-visible writes for a
      bounded mainnet compiler regression slice; a missing fixture must fail the
      release gate.
- [ ] Record the clarity-wasm and compiler revisions in checkpoint provenance
      and in the report produced by [[053-pass-the-mainnet-node-release-gate]].

## Acceptance Criteria

- A normal mainnet start executes every deployment and call through
  clarity-wasm; no interpreter environment variable, fallback or healing step
  is required or permitted by the release configuration.
- The known principal-routing and compiler-refusal cases match stacks-core's
  results, costs, events and state roots under clarity-wasm.
- Restarting preserves the same clarity-wasm state and root without migration.
- The pristine compiler-only replay matches every captured mainnet root and
  receipt in the release slice.
- Deliberately forcing a clarity-wasm/interpreter disagreement in a regression
  test stops the conformance run before sealing rather than accepting the
  interpreter's answer.

## Where the 8,668,161 divergence is, and two shapes that are not it

The path is `.loto::ri` → `r` → `(contract-call? t ss b)` on a trait reference,
into `SP2H674PRTZV6YW56K0FMR7GDGZE4ZC5HMYZ3CDEV.hilt::ss`. `.loto` is a pure
router; all of it happens in `hilt`.

`xtask call-both` against the live state gives the disagreement directly:

```
compiler     [(err u9), (err u2)]
interpreter  [(err u9), (err u9)]
```

`u9` is `hilt::sr`'s own `(asserts! (>= v5 v2) (err (if (> v5 u0) u9 u8)))`.
`u2` is not from `sr` at all — it is a token reporting sender and recipient
equal, propagated by `try!`. `sr` binds `(v3 tx-sender)` and then calls
`(contract-call? v4 transfer v2 v3 (as-contract tx-sender) none)`, so under the
compiler `v3` and `(as-contract tx-sender)` came out the same principal.

`tests/wasm_trait_fold.rs` holds the minimizations. Two plausible shapes are
**ruled out** — both engines agree on them:

- an empty `(list)` in a tuple literal used as a fold accumulator, folded over
  trait references and then mapped (`hilt::ss`'s outer shape, and the same
  family as `v0-egroup`'s `none` at 8,667,467)
- `as-contract` failing to restore `tx-sender` when it appears as an argument
  beside a reader of it, or beside a `let` binding that captured it

So the wrong principal comes from somewhere else in `sr`: the remaining
candidates are `(element-at? kft v0)` picking the wrong contract from the
constant list, the nested `as-contract (begin (fold sw …))`, or the second
`transfer` inside it. Bisect `sr` against the live state with `call-both`
rather than synthetically — the synthetic shapes keep agreeing.

## Both known divergences are fixed in the compiler

**8,668,161 — `as-contract` leaked its sender on an early return.** It compiles
to `enter_as_contract`, the body, `exit_as_contract`. An `asserts!` or `try!`
inside the body branches straight to the function's return block and never
reaches the exit, so the host's sender and caller stacks stay pushed and
whatever runs next inherits the contract as `tx-sender`. `hilt::sr` asserts its
way out with `(err u9)`, `map` calls it again, and the second call transfers to
itself — `(err u2)`.

```
before   compiler [(err u9), (err u2)]   interpreter [(err u9), (err u9)]
after    compiler [(err u9), (err u9)]   interpreter [(err u9), (err u9)]
```

and the chain says `(err u9)`. Fixed at the *function* boundary rather than at
`as-contract`: a function records the stack depths on entry and unwinds to them
in its postlude, which already runs on every path including the early return.

What located it was bisection, not a hypothesis. `sr` on **either chunk alone**
agrees between the engines; only the two in sequence disagree, which points at
what the first call leaves behind rather than at what either computes.

**8,667,467 — a `let`-bound placeholder was laid out for the binding.** A `let`
stores a binding laid out for the type its *value* analysed as, and
`{ t: target, r: none }` analyses `none` as `(optional NoType)`: an indicator
and one `i32`, where `(optional uint)` is an indicator and two `i64`s. `fold`
then sets its accumulator's type on the expression it is about to read, and
reads a value two slots short — "expected i64, found i32".

The `let` cannot know; the type comes from a use it has not reached. So the
widening happens at the *read*, where both types are in hand. `v0-egroup` now
compiles and deploys under clarity-wasm.

Both fixes keep clar2wasm's own 1,375 tests green.

## The interpreter cannot execute mainnet any more

`interpreter_allowed` refuses the switch on mainnet outright rather than
trusting it to be unset, and both production fallbacks — deploy and call — are
gone. A module clarity-wasm will not build now stops the node and names the
contract, which is how the two bugs above became findable at all.

That is also why the reported depth *fell*: 8,673,863 was the interpreter's
work, not nano's.

## Pristine replay, in progress

Running from a fresh state directory with no interpreter switch, importing the
146 GB checkpoint (~17 GB written). This is the only number that counts, and it
is the remaining item.
