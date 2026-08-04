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

- [x] Reproduce and minimize every known mainnet clarity-wasm divergence,
      including the trait-reference/wrong-principal failure at 8,668,161.
- [x] Fix the two known compiler/runtime boundary disagreements without routing
      their transaction or deployment through the interpreter.
- [x] Reject `NANO_INTERPRETER_ONLY` on mainnet.
- [ ] Remove or reject `NANO_INTERPRETER_FALLBACK`, `NANO_CROSSCHECK` and
      `NANO_CROSSCHECK_TRANSACTIONS` in the production mainnet node; every path
      that toggles `Vm::interpret_contract_calls` must enforce the same network
      policy.
- [ ] Keep the interpreter differential oracle and contract healing in explicit
      test or `xtask` tooling whose writes are always rolled back and which the
      release runtime cannot invoke.
- [ ] Answer `/v2/accounts` without evaluating `(stx-account ...)` through the
      reference interpreter; use direct state access or clarity-wasm.
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
- Starting mainnet with any interpreter or crosscheck environment switch fails
  before opening mutable chainstate, and no public Rust or RPC path can enable
  interpreter execution afterwards.
- Account and read-only RPCs execute no Clarity expression in the reference
  interpreter.

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

## Interpreter-only is blocked, but the production boundary is not closed

`interpreter_allowed` refuses the switch on mainnet outright rather than
trusting it to be unset, and the automatic deploy fallback is gone. A module
clarity-wasm will not build now stops the ordinary node path and names the
contract, which is how the two bugs above became findable at all.

The wider audit found remaining routes around that guard. The call path still
reads `NANO_INTERPRETER_FALLBACK` and returns the interpreter's answer after a
wasm runtime failure; `NANO_CROSSCHECK` and `NANO_CROSSCHECK_TRANSACTIONS` still
invoke it; and `Vm::interpret_contract_calls(true)` itself has no network check.
The account RPC also calls `Vm::execute`, which evaluates `stx-account` through
`eval_all`. None is enabled in the current pristine run, but a release boundary
must make them impossible rather than depend on operator discipline.

That is also why the reported depth *fell*: 8,673,863 was the interpreter's
work, not nano's.

## Pristine replay, in progress

Running from a fresh state directory with no interpreter switch, importing the
146 GB checkpoint (~17 GB written). This is the only number that counts, and it
is the remaining item.

## Upstream: what is already solved, and what is not

`stx-labs/clarity-wasm` has nine open PRs, and several sit exactly where this
work does. Checked before carrying any of these fixes further:

| PR | What it is | Bearing on this |
|---|---|---|
| 826 | merge changes from stacks-core | the Epoch40/Clarity6 rebase (M8a) |
| 825 | allow contract call to be constant (#816) | **duplicates a fix already made here** in `words/contract.rs` — a constant naming a contract was not recognised as a static call |
| 824 | Clarity 1 return type of contracts (#818) | touches `duck_type.rs`, `wasm_utils.rs` |
| 823 | right type for contract call v1 (#819) | `wasm_generator.rs` +60 |
| 812 | Clarity 4 costs | `cost/clar4.rs` and `costs-4.clar` — this is **W6.3 / M8c**, unimplemented here |
| 620 | copy only after `concat` args traversal | `words/sequences.rs` |
| 734 | gate datastore under `developer-mode` | W6.5's dev-only datastore |

**`duck_type` already exists.** `clar2wasm/src/duck_type.rs` converts a value's
representation from one type to another, handles memory as well as stack, and
`need_ducktyping` already treats `NoType` as needing conversion.
`lookup_constant_variable` calls it for exactly the reason a local binding needs
it — so the placeholder fix here is a narrower re-implementation of machinery
that is already in the tree.

Calling `duck_type` from `visit_atom` directly does *not* work: the error moves
from "expected i64, found i32" to "expected i32, found i64", because it wants
the value in a different form than a flattened read of the binding's locals.
Reconciling the two is the right end state and is not guesswork to do blind —
`widen_actions` stays until then, and this note is why it should not stay
forever.

**Not known upstream:** no open issue or PR covers `as-contract` failing to
restore the sender on an early return. That fix looks genuinely new and is worth
sending up.

**Worth heeding:** issue #575, "let function throwing a too many locals issue".
The `as-contract` fix adds two `i32` locals to *every* function prologue. 1,375
tests stay green, but that is the shape of thing #575 is about, and a cheaper
form — only emitting the save/restore for functions whose body contains an
`as-contract` — would avoid adding to it.

## The fast way to test a compiler fix against mainnet

A pristine import costs 4.5 hours, which is the wrong unit for iterating on
clarity-wasm. Two cheaper routes, in order of preference:

1. **Snapshot after import.** The state directory straight after a checkpoint
   import is byte-identical every time. Copy it once (~30 GB, minutes) and every
   pristine run starts from the copy.

2. **Rewind an existing state.** `ChainState::retract_to` already rewinds
   everything kept *beside* the MARF — executed chain, tenure start heights,
   accounting — and deliberately deletes nothing from the MARF itself, because a
   state is addressed by the block that sealed it. So a state that ran past a
   divergence can be wound back to the block before it and re-executed under a
   changed compiler, with no import at all.

   Two things are missing. The node's tip on startup is
   `SELECT hash FROM marf_block ORDER BY height DESC` — the highest block — so a
   rewind has to *delete* the blocks above the target and their nodes (6,397
   blocks and 1,091,821 nodes, to wind back from 8,673,863 to 8,667,466). The
   comment on `retract_to` about deleting nothing is about forks, where the
   abandoned branch stays reachable; a rewind is not that.

   **And it does not give a pristine state.** The side store keys contract
   definitions by contract, not by block, so a contract the interpreter deployed
   or healed survives a MARF rewind untouched. A rewound state is good for
   asking whether a compiler fix changes a block's outcome; it cannot answer
   [[060]]'s question, which is what clarity-wasm does from a checkpoint with no
   interpreter having touched anything.

   So: build it for iteration, and keep importing for evidence.

Neither was available while chasing 8,667,467 and 8,668,161, which is why both
cost hours of wall-clock each rather than minutes. `xtask call-both` against a
live state is what actually found them, and it is fast — the expensive part was
having no way to *re-run a block* after changing the compiler.
