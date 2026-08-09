---
id: "060"
title: "Make clarity-wasm the conformant production execution engine"
status: in-progress
priority: critical
effort: large
type: bug
group: mainnet
tags: ["mainnet", "vm", "clarity", "conformance", "release"]
created_at: 2026-08-04
---

# Make clarity-wasm the conformant production execution engine

## Objective

Make clarity-wasm the only production execution path and close every known
difference from stacks-core's Clarity semantics. This is an unconditional
product boundary, not a mainnet configuration choice: a shipped `stacks-node`
must have no path that can execute a transaction, deployment, contract call or
read-only query with the interpreter on any network, in any build profile or
under any failure mode. A clarity-wasm compile, load or runtime failure rejects
the candidate and is fixed in clarity-wasm; it is never retried under another
engine.

The interpreter remains a differential oracle only in separately built test or
diagnostic tooling. That tooling may run the same transaction against both
engines in a rolled-back bracket to localize a disagreement, but it must not be
reachable from the node binary or share its mutable runtime. It must never
answer a production request after clarity-wasm refuses or returns a different
result. [[059-heal-the-contracts-the-interpreter-cannot-run]] is diagnostic
tooling for investigating old compiler-created state, not functionality the
node may invoke.

## Tasks

- [x] Reproduce and minimize every known mainnet clarity-wasm divergence,
      including the trait-reference/wrong-principal failure at 8,668,161.
- [x] Fix the two known compiler/runtime boundary disagreements without routing
      their transaction or deployment through the interpreter.
- [x] Reject `NANO_INTERPRETER_ONLY` on mainnet as an immediate containment
      measure; this guard is not the final boundary.
- [x] Delete `NANO_INTERPRETER_ONLY`, `NANO_INTERPRETER_FALLBACK`,
      `NANO_CROSSCHECK` and `NANO_CROSSCHECK_TRANSACTIONS` handling from the
      production node and VM call path. The shipped node must not recognize an
      environment variable, configuration field or command-line option that can
      select, compare, retry or fall through to the interpreter.
- [x] Remove `Vm::interpret_contract_calls` and every equivalent engine selector
      from the production API. Do not replace them with a mainnet guard, hidden
      feature, emergency mode or unsafe escape hatch; there is no production
      condition under which interpreter execution is allowed.
- [x] Move the interpreter differential oracle, crosscheck and contract healing
      into separately built test or `xtask` tooling whose writes are always
      rolled back and which the `stacks-node` binary cannot call or enable.
- [x] Make every clarity-wasm compile, validation, instantiation, trap and host
      failure reject the candidate without sealing or committing any state, and
      add a regression proving that none is retried with the interpreter.
- [x] Answer `/v2/accounts` without evaluating `(stx-account ...)` through the
      reference interpreter; use direct state access or clarity-wasm.
- [ ] Replay from a pristine checkpoint entirely through clarity-wasm, including
      compiler-hostile deployments and calls, with no healing or engine switch.
      The 8,708,126 frontier is owned by
      [[086-execute-mainnet-block-8708126-without-corrupting-i]] and its cause is
      now pinned: not the trait argument, but a `let`-bound principal whose use
      count the wasm-local pre-pass did not see inside an allowance list, so the
      allowance read it out of a slot that had already been handed back to the
      pool. `be3ec64e` fixes it, bisected pre-/post-fix, and the block seals the
      root the network signed. The later replay crossed task 088's sortition
      frontier and reached 8,724,865; task 098 owns the next VM boundary.
- [x] Compare clarity-wasm with the interpreter before sealing in the
      conformance harness and retain minimized regression fixtures for every
      disagreement found.
- [x] Pin roots, receipts, costs, events and consensus-visible writes for a
      bounded mainnet compiler regression slice; a missing fixture must fail the
      release gate.
- [x] Record the clarity-wasm and compiler revisions in the report produced by
      [[053-pass-the-mainnet-node-release-gate]].
- [x] Bind checkpoint provenance and every reusable compiler fixture to the
      exact clarity-wasm/compiler revision that produced it; a fixture from an
      unknown compiler build cannot satisfy the release gate.
      Split three ways, because the three things have different owners. A
      *checkpoint* is stacks-core's state, so binding it to a nano compiler
      revision would be a false claim; what the state directory records instead is
      every nano build that has opened it (`recorded_engine_identities`), which is
      the right binding for the roots beyond the checkpoint and is what
      `report_state_engines` prints. A *frozen receipt slice* is nano's own output
      and `freeze-receipts` stamps `COMPILER_IDENTITY` into it —
      `the_frozen_mainnet_slice_names_its_compiler` now refuses one that does not,
      via `skip_gate`, so it prints a reason offline and *fails* under
      `NANO_REQUIRE_MAINNET`. **Since re-frozen:** the in-tree slice names compiler
      `sha256:1813d530…`, against an artifact built by `sha256:a544b056…`. Not
      asserted *equal* to
      this artifact's compiler, deliberately — a baseline frozen by an earlier
      build is exactly what catches a change in this one. The `.clar` fixtures are
      inputs minimized from the chain and need no binding.
- [x] Tell a compile refusal at a *call* apart from one at a deploy. The first
      can only ever be a compiler gap; the second is a transaction the network
      also failed. Conflating them makes a gap invisible in the state root.
- [~] Resolve the asymmetric `least_supertype` tuple case where the interpreter
      returns a taken branch's wider tuple but wasm exposes the narrowed layout.
      **Amended:** the ignored test is gone, replaced by a two-sided pin on what
      *each* engine answers, in both branch directions, across every boundary the
      value can cross — an ignored equality is a divergence nobody measures. What
      remains open is the fix, and
      [[068-resolve-asymmetric-tuple-least-supertype-semantics]] records why no
      choice inside clar2wasm closes it: the reference's answer follows the branch
      taken, so the conformant engine is one whose values carry their shape at run
      time.
- [x] Resolve `match` branch bindings that shadow an enclosing local: wasm
      currently accepts a shape the reference interpreter rejects with
      `NameAlreadyUsed`. Pin both the taken and untaken branch behavior.
- [~] Account for every ignored Clarity semantic differential in the release
      report. A known engine disagreement may not be waived merely because it
      has not appeared in the replayed mainnet window.
      An `ignored tests` section, scanned rather than written down: it reads every
      `#[ignore]` in the engine's own suite and the conformance suite out of the
      sources and splits infrastructure reasons from semantic ones. A bare
      `#[ignore]` with no reason counts as semantic, because an unexplained skip is
      what the section exists to make impossible. **Audit correction
      2026-08-07:** this is not complete. `needs to be implemented` is classified
      as infrastructure, which hides the ignored Clarity-4 contract-call cost
      test, and the scan cannot see [[068]] because its replacement tests pin two
      unequal engine answers without an ignore. The report must account for both
      forms under [[074]]. It currently reports **0 semantic** —
      `#[ignore = "see issue #316"]` turned out to hide a wrong *expectation* and
      not a disagreement: both engines answer `(+ 3 4)` for a deploy whose last
      top-level form is a definition, which is what `initialize_contract` and
      `eval_all` both do.
- [x] Reject a contract call through a constant during top-level deployment with
      the reference `ContractCallExpectName` result under
      [[067-reject-contract-call-through-a-constant-while-depl]], and remove its
      ignored differential.

## A new mainnet frontier, 2026-08-07: `Unexpected principal data` at 8,708,126

A restored, healthy mainnet state (`/home/aldur/mainnet-restored`, reflinked from
`mainnet-wasm`) resumed cleanly, executed 279 blocks from 8,707,846, and stopped at
**8,708,126** with:

> Clarity execution error:
> `Internal(InvariantViolation("Expect(\"Internal(Expect(\\\"Unexpected principal data\\\"))\")"))`

The message is not vague once traced. `StandardPrincipalData::new`
(`clarity-types/src/types/mod.rs:95`) raises it for one reason: **a version byte of
32 or more**. A Stacks address version is a small number, so nothing legitimate
produces one — something handed it bytes that are not a version.

Two production sites construct principals from raw bytes, and both are reads at an
offset:

- `clar2wasm/src/wasm_utils.rs:464` — reads the version byte then twenty hash bytes
  straight out of wasm linear memory at a running offset;
- `wasm_utils.rs:1528` — splits a serialized buffer the same way.

So the likely shape is a principal read from the *wrong offset*: the byte that should
be a version is whatever happened to be there. That puts it in the same family as the
spill and offset work under
[[073-decide-whether-a-contract-clarity-wasm-cannot-load]] rather than in a Clarity
semantic rule, and it is a compiler gap rather than a transaction the network also
failed — the network executed this block.

Next: identify the transaction at 8,708,126 and the value it was reading. 7,639
blocks are staged behind it, so the state continues the moment it executes.

**Exact 8,708,126 evidence:** [[086]] bisected the stale-local read to
`be3ec64e`, then replayed the checked-in block twice in each of two independent
fresh reflink scratches. Result, all five costs, all eight ordered events and the
committed root match the network oracle. Focused interpreter differentials pin
each causal value/cost boundary. Exact-prestate full interpreter event/asset
parity for transaction index four remains open because no production API can
execute a partial same-block prefix under an alternate engine without duplicating
private fee, nonce, postcondition and rollback semantics.

## Acceptance Criteria

- Every production node execution, on every network and under every role or
  build profile, uses clarity-wasm. The node contains no interpreter engine
  selector, fallback, crosscheck, healing path or failure recovery route.
- Setting any historical interpreter environment variable cannot change node
  behavior because the production binary does not read or recognize it.
- A forced clarity-wasm compile, load or runtime failure rejects the candidate,
  commits no state and invokes no second execution engine.
- The known principal-routing and compiler-refusal cases match stacks-core's
  results, costs, events and state roots under clarity-wasm.
- The known tuple-supertype and enclosing-local `match` binding cases match the
  reference semantics and are no longer ignored differentials.
- A constant-target contract call during deployment matches the reference error,
  costs and no-write behavior and is no longer an ignored differential.
- Restarting preserves the same clarity-wasm state and root without migration.
- The pristine compiler-only replay matches every captured mainnet root and
  receipt in the release slice.
- Deliberately forcing a clarity-wasm/interpreter disagreement in a regression
  test stops the conformance run before sealing rather than accepting the
  interpreter's answer.
- No public or private production Rust API, RPC route, environment variable,
  configuration field, CLI option or Cargo feature can enable interpreter
  execution before or after mutable chainstate opens.
- Account and read-only RPCs execute no Clarity expression in the reference
  interpreter.

## Non-negotiable production boundary

"Fallback disabled" is not sufficient. A dormant branch, a testnet-only branch,
an environment-gated branch, an emergency switch and a crosscheck that happens
to discard its result are all production interpreter paths and all violate this
task. The node must have one execution engine: clarity-wasm.

The reference interpreter may remain in the dependency closure because
clarity-wasm consumes stacks-core's frontend and ABI types. Dependency presence
does not permit a callable node path. Production crates must not reference
`eval_all`, an interpreter contract-call helper or an engine-selection API;
those references belong only to conformance tests and separately built
diagnostic commands.

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

## 8,665,719 is most likely a third compiler bug, in Pyth payload parsing

The pristine clarity-wasm replay stops here, and the chain and nano disagree on
one transaction:

```
chain   v0-4-market.borrow   success (ok true)
nano    RuntimeFailure("Runtime(UnwrapFailure, Some([])))"   (err none)
```

Ruled out by `xtask eval` against the state at the stopping point, in seconds
each: every `get-stacks-block-info?` read works, the ststx-ratio contract those
feed answers `(ok u1796712)`, `tenure-height` is 251321 and `stacks-block-time`
is 1785402333. No header lookup misses. So it is neither of the things task 055
attributed it to.

The transaction's arguments name the real path:

```
ft          'SP1A27KFY4XERQCCRCARCYD1CC5N7M6688BSYADJ7.wstx
amount      u257669916
price-feeds (some (list 0x504e4155 01 …))
```

`0x504e4155` is `PNAU` — a **Pyth** price update. `borrow`'s first `try!` that
touches it is `write-feeds` → `write-feed` → `contract-call?
'SP1CGXWEAMG6P6FT04W66NVGJ7PQWMDAC19R7PJ0Y.pyth-oracle-v4
verify-and-update-price-feeds`, which parses an 8 KB binary payload — merkle
proofs, VAA signatures, buffer slicing.

That matters because **`pyth-adapter-v1` was one of the four contracts
clarity-wasm previously could not run at all**
([[059-heal-the-contracts-the-interpreter-cannot-run]]). The compiler has a
history in exactly this code. A payload-parsing path is also where the two bugs
already fixed today would have hidden: buffer slicing and placeholder layout.

Next: call `verify-and-update-price-feeds` with that payload under both engines.
It is a `contract-call?` with one buffer argument, so `xtask call-both` can ask
it directly — minutes, not hours.

### Exact resume point for 8,665,719

The Pyth payload is extracted and ready. From the failing transaction's
`price-feeds` argument — `(optional (list 1 (buff 8192)))`, prefix
`0a 0b 00000001 02` — the single feed is **2,007 bytes**, written as a
consensus-serialized `(buff 2007)` argument.

```
cargo xtask call-both --sender SPRSMJ5QYQM8T0YRJGAFZXRFXN3K6PCDRDYE6B2T \
  <state-dir> SP1CGXWEAMG6P6FT04W66NVGJ7PQWMDAC19R7PJ0Y.pyth-oracle-v4 \
  verify-and-update-price-feeds <feed-buff-hex> <config-tuple-hex>
```

Run with the feed alone both engines answer
`RuntimeCheck(IncorrectArgumentCount(2, 1))` — agreeing, which is right:
`verify-and-update-price-feeds` takes the feed *and* an execution-context tuple.
`write-feed` in `v0-4-market` builds it:

```clarity
(contract-call? 'SP1CGXWEAMG6P6FT04W66NVGJ7PQWMDAC19R7PJ0Y.pyth-oracle-v4
  verify-and-update-price-feeds feed
  { pyth-storage-contract: 'SP1CGXWEAMG6P6FT04W66NVGJ7P… , … })
```

So the one remaining step is to serialize that tuple from `write-feed`'s literal
and pass it as the second argument. If the engines then disagree, it is the third
compiler bug; if they agree, the divergence is in `borrow`'s later bindings and
`eval` can walk them one at a time.

## The boundary is structural now

The interpreter lives in `nano-oracle`, a crate `nano-node` does not depend on.
There is no environment variable, configuration field, Cargo feature or failure
mode that can make a shipped node execute a transaction that way, because the
engine is not linked in. `NANO_INTERPRETER_ONLY`, `NANO_INTERPRETER_FALLBACK`,
`NANO_CROSSCHECK`, `NANO_CROSSCHECK_TRANSACTIONS`, `Vm::interpret_contract_calls`
and `Vm::execute` are all gone, and so are `heal_contract` and
`uninterpretable_contracts` — the last two moved to the oracle, where the healing
task always belonged.

`wasm_is_the_engine` checks that rather than a flag: `nano-oracle` absent from
`cargo tree -p nano-node`, no production crate naming `eval_all`,
`initialize_versioned_contract` or `environment.execute_transaction(`, and the
four retired switches appearing nowhere in the tree. It also asserts the oracle
*does* name those three, so it cannot pass because a symbol was renamed.

`/v2/accounts` reads `get_stx_balance_snapshot` and the three unlock heights
directly — the same reads `special_stx_account` makes, in the same order — so no
engine starts to answer an RPC. The two remaining Clarity reads in tests and in
the conformance suite go through a deployed contract, which is the only way a
contract on the chain can read anything.

Found while moving it: the compiled call path passed `None` as the sponsor into
clar2wasm, so `tx-sponsor?` answered `none` in every sponsored contract call.
Consensus-visible, and now threaded through.

## The third compiler bug: a fold over a buffer

`fold` computed the type its accumulator actually carries — the folded function's
second parameter — only when the sequence was a *list*. An initial value written
`(list)`, `none` or `(ok true)` analyses with a placeholder in it, so folding a
buffer sized every allocation and copy from the placeholder and read the
accumulator short.

That is mainnet 8,665,719. `pyth-pnau-decoder-v3::parse-proof` folds over a
`(buff 8192)` with `{ result: (list), ... }`, so each 20-byte merkle hash came
back empty; `check-proof` then `unwrap-panic`s a `slice?` of one and the whole
`v0-4-market::borrow` aborted with `UnwrapFailure` where the chain says
`(ok true)`. Both engines now answer `(ok true)`, and so does the chain.

Element duck-typing stays list-only, and now says why:
`get_sequence_element_type` reports `(buff 1)` for a string as well as a buffer.

## What made it findable in minutes instead of hours

`xtask call-both-tx` replays the staged block above a state's tip through both
engines using the transaction's own arguments. Hand-serializing those arguments
back into `call-both` is where the previous two bugs cost hours each.
`check-module` now reads the contract's source and version from the state rather
than a file, so a bisection cannot start from the wrong text, and
`NANO_TRACE_CALLS` names the deepest cross-contract call before a failure —
which is what pointed at the decoder rather than at the oracle or the market.

## Remaining

- Replay from a pristine checkpoint entirely through clarity-wasm: in progress,
  and past 8,665,988 with no divergence since the fee fix.
- Compare the engines before sealing in the conformance harness, and keep
  minimized fixtures for every disagreement. `wasm_response_fold` and
  `wasm_trait_fold` hold the three found so far.
- Pin roots, receipts, costs and events for a bounded mainnet regression slice.
- Record the clarity-wasm and compiler revisions in checkpoint provenance. The
  *report* half is done: `cargo xtask release-report` prints the tree hash of
  `vendor/clarity-wasm` — a content hash of exactly the source that was compiled,
  which the repository's commit id is not — along with the wasmtime version and
  the pinned stacks-core revision. What a state directory carries is unchanged.

## Three forced refusals, and a hole the state root cannot see

`crates/nano-conformance/tests/conformance/engine_failure.rs` forces all three
refusal classes through `nano_vm::Vm`, none of them needing an open compiler bug:
a source naming an unresolved function (compile), a `let` with 60,000 bindings
(module load — every binding is a wasm local and wasmtime's validator accepts
50,000), and `(- u0 u1)` after a write (runtime trap). Twenty retries each, on
both the deploy path and a *planted* contract that is already in state, which is
the 8,668,161 shape. The positive control is the 60,000-binding contract: the
interpreter deploys and runs it happily, so one engine answers and the other
refuses, and nano's answer is no.

Two facts worth keeping from writing it:

- A function's parameters are capped at **256 by Clarity's analyzer**, so wasm's
  1,000-parameter limit is unreachable from Clarity source, and contract metadata
  is write-once, so a bad module cannot be planted as bytes. Locals are the only
  reachable wasmtime limit.
- **A compile refusal at a call is invisible in the sealed root.** It is reported
  as a failed transaction — deliberately, because a deployment naming a function
  that does not exist is an ordinary failed mainnet transaction and has to stay
  one — and a failed transaction writes nothing, so it seals the root an untouched
  block seals, and the root a legitimate `ArithmeticUnderflow` seals. A
  root-matching replay can therefore hide a compiler gap whose *receipt* is wrong.
  Receipts catch it; roots do not. That is the new item above: a refusal at a call
  can only ever be a gap, because the network accepted the contract once, and it
  should reject the candidate rather than become a receipt.

Also gone: the stale comment in `deploy_contract_with_wasm_in_context` about
`loadable` keeping an analysis out of the way of "the interpreter fallback below",
which no longer exists. Not changed here — `nano-vm` is another agent's file — but
worth someone's `sed`.

## Replay depth 8,666,584, and a fourth bug of the same family

The fee fix carried the replay 863 blocks past 8,665,719. It now stops at
**8,666,585**, and the cause is localised:

```
receipt 487356b6…  RuntimeFailure(… "contract analysis failed:
  SP4SZE494VC2YC5JYG7AYFQ44F5Q4PYV7DVMDPBG.rewards-stx-v1 compiles to a module
  that will not load: type mismatch: expected i64, found i32 (at offset 0x30f8)")
```

**"expected i64, found i32" is the placeholder-layout signature** — the same
error 8,667,467's `let`-bound `none` produced, and the same family as the
fold-over-a-buffer fixed above. A fourth instance, in a contract this block
*deploys*: mainnet accepted the deployment, clarity-wasm emits a module wasmtime
refuses to load, so nano turns it into a failed transaction and the block's state
root differs while every balance in it is right.

Everything else about the block was checked and is correct, which is what makes
this unambiguous:

- all four account balances match the chain at that height, read from
  `/extended/v1/address/…/stx?until_block=8666585`;
- the block's one contract call answers `(ok true)` under both engines and both
  engines write the **same five keys with the same five values** — so
  `call-both-tx` with a write trace now compares write *sets*, not just answers;
- `probe-root` reproduces nano's root from its 16-write journal exactly, and no
  omission and no reordering reaches the chain's, so it is a value or a missing
  write rather than a trie or ordering fault.

## 8,666,585 minimised: two tuple shapes under one `if`

Eight lines, and it reproduces the mainnet error exactly:

```clarity
(define-data-var v uint u0)
(define-public (go (n uint))
  (begin
    (if (> n u0)
      (print { a: n, b: u1 })
      (print { a: n, b: u1, c: true }))
    (ok n)))
```

`type mismatch: expected i64, found i32 (at offset 0x1a37)` — the same signature,
the fourth of the family. `rewards-stx-v1`'s `process-rewards` ends in exactly
this: an `if` whose two arms `print` tuples with different field sets, one carrying
an extra `keeper-only`.

The telling part is that the same `if` **without** `print` is refused by analysis
with "Tuples fields should be typed". So it is `print` accepting two unrelated
tuple types that lets codegen reach a layout it cannot honour — which points at
`print`'s analysed return type rather than at `if`, and is a narrower place to look
than the previous three were.

Pinned as `two_tuple_shapes_under_one_if_compile_to_a_loadable_module` in
`wasm_response_fold`, `#[ignore]`d with the reason, because a red suite teaches
people to ignore red suites. `cargo test -- --ignored` runs it in 0.01 s.

### Two findings, and why the obvious fix is not the fix

**`If` overwrites both branches' types with its own.** `conditionals.rs` does
`set_expr_type(true_branch, expr_ty)` and the same for the false branch, so inside
`Print::traverse` the `print` *expression*'s type is the `if`'s supertype while its
*argument*'s type is the narrow tuple. `Print` reads only the argument's type, for
the locals and for the value it pushes back — so it leaves an arm laid out one slot
short of what the `if` reads. Ducking `print`'s result to the expression's type
looked like the fix, was tried, and did not work; the change is reverted rather
than left in on a hunch.

**`need_ducktyping` compares tuples by position and stops at the shorter one.**

```rust
og_tup_ty.get_type_map().values()
    .zip(tg_tup_ty.get_type_map().values())
    .any(|(og, tg)| need_ducktyping(og, tg))
```

`{ a: uint, b: uint }` against `{ a: uint, b: uint, c: bool }` zips two pairs, finds
them identical, and answers "no conversion needed" — for two types whose layouts
differ by a slot. That is a real latent bug worth fixing on its own, and it is
plainly involved here.

**But widening a tuple is not a representation change.** An arm that produced
`{ a, b }` has no third field to convert; no layout conversion can invent one. So
the question underneath is what the `if`'s type *should* be, and whether
clarity-wasm should be emitting a module here at all. Note that the same `if`
without `print` is refused by analysis with "Tuples fields should be typed" — and
that mainnet **accepted this deployment**, so stacks-core's analysis does not refuse
it. Whatever the interpreter does with a value whose shape depends on the branch
taken is what clarity-wasm has to reproduce, and that is the thing to establish
next: run `min1` through the interpreter and see what type and value come back.

**What the interpreter does, measured.** `xtask eval` on the minimised `if`:

```
Some(Tuple(TupleData { type_signature: { "a": uint, "b": uint }, … }))
```

It returns the **taken branch's own tuple, with that branch's own type**. There is
no widening at runtime: the value's shape depends on which arm ran.

That is the model mismatch. clar2wasm fixes a value's wasm representation from its
*static* type, and `If` gives both arms one static type — so whichever arm does not
match that type is laid out wrongly, and no conversion fixes it because the two
arms genuinely carry different numbers of fields. **Which type the `if` chose, measured.** Printed from `If::traverse`:

```
if analysed as (tuple (a uint) (b uint))
  true  arm { "a": uint, "b": uint }
  false arm { "a": uint, "b": uint, "c": bool }
```

The `if` analyses as the **narrow** tuple. So the false arm has to be *narrowed* —
field `c` dropped — and that **is** a representation change `duck_type` can express.
The fix is a chain of three, and the first two were tried:

1. **`need_ducktyping` must see a differing field set.** Fixed, and it is right:
   compare by name and count rather than zipping positionally. On its own it changes
   nothing here, because nothing was calling `duck_type` on this path.
2. **`Print` must duck its result from its argument's type to the expression's.**
   Applied on top of (1), and it moves the failure from "module will not load" to
   `Incompatible types for duck typing: BoolType / UIntType`. So the duck is now
   firing, and the value is reaching it.
3. **`duck_type`'s own tuple conversion is positional too.** That `BoolType /
   UIntType` is field `c: bool` being matched against `b: uint`. Tuple conversion
   has to map by field *name* and drop fields absent from the target.

(3) is where it stops. All three were reverted rather than left in: (1) and (2)
together turn a load failure into an analysis failure, which is worse, and (3) is a
change to shared conversion machinery that needs clar2wasm's own 1,375 tests and a
mainnet replay behind it, not a hunch at the end of a session.

The diagnosis is complete and the sequence is written down. `min1` in
`wasm_response_fold` reproduces it in 0.01 s, and
`NANO_TRACE_IF_TYPES` — a two-line print in `If::traverse`, also reverted — is how
the analysed type was read.

## Two tools this needed, and now has

`xtask decode-blocks` with `NANO_DUMP_DEPLOYS=<dir>` writes out the source of every
deployment in a block. A deployment that *failed* leaves no contract in the state —
the transaction that would have put it there is the one that did not run — so the
block is the only place its source exists, and there was no way to get it out.

`check-module` no longer *requires* the state to hold the contract. It consults the
state for a version and takes a source file when given one, so a contract that was
never deployed can be compiled against a real chainstate. Both modes verified.

## The next step, exactly

`rewards-stx-v1` is not in the state — the transaction that deploys it is the one
that failed — so `check-module` cannot read its source from there. It is in the
block: extract the `VersionedSmartContract` payload from transaction
`487356b6823569e5392ef0dbe22aa78b1467cf7c41038324c5b64f84e4fc5aff` of block
8,666,585 and run `check-module <state> <id> <version> <source-file>`, which
reproduces the load failure in seconds without executing anything.

The offset `0x30f8` is in the emitted module, and `NANO_DUMP_REFUSED_WASM` writes
a disassembly beside it, so the failing instruction can be read directly rather
than inferred. The two fixes already made both narrowed to a *read* that wanted a
value in a form the binding was not laid out for; this is very likely a third
position of the same mistake, and `duck_type` is the machinery it should be using
— see the note above about `visit_atom`.

## 8,666,585 is passed, and the fourth bug is fixed

Depth moved 8,666,584 → **8,666,650** with no divergence, so the tuple-narrowing
fix is confirmed against the chain and not only against its minimisation. The
`#[ignore]` is gone from
`two_tuple_shapes_under_one_if_compile_to_a_loadable_module`, and
`narrowing_a_tuple_keeps_the_fields_it_kept` covers a dropped middle field, a
nested narrowing, a narrowing beside a list, and — the important one — a
*homogeneous* tuple, which is the only shape where mispairing fields survives wasm
validation and returns a wrong answer instead of refusing to load.

**Four found, four fixed.** `as-contract` leaking its sender on an early return
(8,668,161), a `let`-bound placeholder laid out for the binding (8,667,467), a
fold over a buffer not widening its accumulator (8,665,719), and a tuple narrowed
by position rather than by name (8,666,585). All four were the same underlying
mistake in different positions: a value laid out for one type and read as another.

## 8,667,509: the fifth bug is not the same family

Replay stops at **8,667,509**, and it is *not* a placeholder laid out for the
wrong width. The four before it all said `expected i64, found i32`; this one says

```
SPN5AKG35QZSK2M8GAMR4AFX45659RJHDW353HSG.blacklist-susdh-v1
  type mismatch: values remaining on stack at end of block (at offset 0x2955)
```

An unbalanced path, not a mis-sized one — a slot is pushed that nothing pops.

The contract is on the chain (this block *calls* it, it does not deploy it), so
`check-module` reads its source from the state. `NANO_DUMP_SOURCE` writes that
source out, which is how a 161-line contract became a two-line reproducer:

```clarity
(define-read-only (g (o (optional { soft: bool, full: bool })))
  (default-to { soft: false } o))
```

**What the disassembly showed.** `wasm-objdump -d` on the module
`NANO_DUMP_REFUSED_WASM` wrote puts offset `0x2955` at the closing `end` of
`get-soft-blacklist`'s outermost body block, just before the function postlude:

```
002929: local.set 24    ;; save the payload -- one i32, per the analysed type
00292b: if type[10]     ;; (param i32) (result i32): the default, one i32
00292d:   drop          ;;   the default
00292e:   local.get 24  ;;   the payload
002930: else
002931: end
...
002951: local.set 25
002953: local.get 25
002955: end             ;; two i32 left where the block declares one
```

`map-get?` had pushed **two** i32s — `soft` and `full` — and only one was saved.
The other sat underneath the `if` all the way to the end of the function.

**The cause.** `default-to`'s type is `least_supertype(default, inner)`, and
`least_supertype` for tuples walks the **first** argument's fields and looks each
up in the second, dropping the rest (`clarity-types/src/types/signatures.rs`). So
`(default-to { soft: false } (map-get? blacklist k))` over a
`{ soft: bool, full: bool }` map analyses as the one-field tuple. `default_to.rs`
carried a WORKAROUND that set *both* arguments' expression types to that — but
`map-get?` reads the map's own declared value type and ignores what it was asked
for, so the request was a lie about what was on the stack. `visit_atom` behaves
the same way here: `widen_actions` refuses two tuples with different field counts
and falls through to a plain `local_get` of the wide binding.

**The fix.** Only the *default* is told to be the expression's type — it has to
be, so a `none` literal knows how many slots to fill. The optional is taken as
analysed and its payload converted afterwards with `duck_type`, in the position
where it already sits on top of the stack with the indicator underneath. Same
machinery as the `print` narrowing, one word over.

It also fixed a second symptom of the same override: `(default-to { soft: false }
(some { soft: true, full: true }))` used to fail codegen with "Tuples fields
should be typed", because the narrowed request propagated into the tuple
literal's fields. Both engines now agree on it.

Gates: the new `a_default_naming_fewer_fields_loads_and_reads_the_ones_it_named`
and `a_default_to_with_nothing_to_narrow_is_unchanged` pass, `cargo test --release
-p clar2wasm` is 1376/1376, the workspace is green, clippy is clean with no
`#[allow]`, and `check-module` on the real `blacklist-susdh-v1` against the live
mainnet state now says it loads.

**Nothing was tried and reverted this time.** The first hypothesis — that
`asserts!` inside `(ok …)` was branching out with a value still pushed, which
this contract does four times — was dropped before any code changed, because
bisecting the two-line `default-to` reproducer out of the contract took under a
minute with `check-module` and a source file.

**One thing narrowing does not fix**, measured rather than assumed: handing the
narrowed tuple back *whole* instead of reading it through `get` gives
`{ soft: true }` under the compiler and `{ full: true, soft: true }` under the
interpreter. That is the supertype asymmetry recorded below, now reachable
through `default-to`, which is far more common than a `print` under an `if`.
Pinned as `a_narrowed_default_agrees_wherever_it_escapes_a_get`, `#[ignore]`d
with the reason. `blacklist-susdh-v1` reads all three of its `default-to`s through `get`,
so 8,667,509 does not depend on it.

## Two things this leaves open

**A supertype asymmetry that no conversion reconciles.** `least_supertype` walks
the true arm's fields and looks each up in the false arm's, so
`(if c { a, b } { a, b, c })` types as `{ a, b }` while the reverse is rejected by
analysis outright. When such an `if` is a function's *return value*, the
interpreter yields the taken branch's wide tuple where wasm must yield the
narrowed layout — a genuinely different value. The new tests read surviving fields
back rather than returning the tuple, so they do not paper over it. It needs a
decision at the analysis layer, and a mainnet contract returning such an `if`
across a contract-call boundary or into a receipt would surface it.

**The follower does *not* exit after each round — that was my own harness.**
`stopping: every sealed block is already on disk` is printed by the SIGTERM
handler, not by a completed round. The node was being launched as
`nohup … &` inside a shell that a two-minute command timeout then killed, taking
the whole process group with it. Launched with `setsid` it runs continuously and
depth climbs without help. Recorded because the wrong version of this was
published, and because "the node stops on its own" would have sent someone into
`supervise` looking for a bug that is not there.

## The sixth: a `match` branch rejected before it could be taken (8,668,096)

Not another layout bug, and not an unbalanced stack — the sixth divergence is a
compile-time rejection of something the reference only rejects at run time.

`SP1E0XBN9T4B10E9QMR7XMFJPMA19D77WY3KP2QKC.auto-alex-v3-endpoint-v2-02` contains

```clarity
ok-value (match claimed-response claimed (ok (+ ok-value …)) err (err err))
```

whose *error* branch binds the name `err`, which is also a native function.
`clar2wasm`'s `Match` refused the whole contract for it, so every call into it
failed regardless of which branch it would take. Block 8,668,096 calls `rebase`,
which takes the ok branch: the chain says `(ok u390)` with 89 writes, nano said
`(err none)` with none, and the 89 missing writes are the whole state-root gap.

The reference checks a binding name *where it binds it*
(`eval_with_new_binding`, `clarity/src/vm/functions/options.rs`), so a branch
that never runs never rejects. `check_special_match` in the analyzer checks only
`contract_context.check_name_used` — contract-level definitions — which is why
this contract passed analysis and deployed on mainnet in the first place.

The fix is `when`, not `whether`: a reserved binding name now compiles that
branch to `NameAlreadyUsed` at run time — the same machinery a reused function
argument name and a `let` shadowing a builtin already use
(`generate_name_already_used_error`) — instead of failing the build. Take the
branch and both engines still raise `RuntimeCheck(NameAlreadyUsed)`.

The predicate is untouched, deliberately: only the *timing* moved. The other
`is_reserved_name` sites are `define-map`, `define-data-var`, `define-constant`,
`define-fungible-token`, `define-trait` and `define-read-only`, and for those a
compile-time refusal is the right judgement — the reference's
`check_legal_define` calls `ContractContext::is_name_used`, which *does* include
`is_reserved`, so `(define-map err …)` fails the deploy on chain too. A contract
that cannot deploy and a contract that cannot compile are the same outcome. Only
`match` binds per-branch, and only `match` was wrong.

### What ruled out the shapes that were suggested

- **Not `define-map`/`define-data-var` copying a check the reference does not
  make.** The reference makes it: `check_legal_define` → `is_name_used` →
  `is_reserved`. Read, not inferred. And the contract has no `define-map` at
  all — `grep '(define-map' ` on its source is empty.
- **Not the wrong epoch or Clarity version.** `err` is in
  `lookup_reserved_functions` for every version, so `ensure_wasm_module`'s
  epoch-rebuild fallback had nothing to fall back to, and the version the state
  records for the contract cannot change the answer.
- **Not a layout or stack bug.** Nothing was read at the wrong width and
  nothing was left on the stack; the module was never built.

### The ladder

Rung 0 answered it — the node's own log named the contract and the reason
(`contract analysis failed: Internal error: Name already used
ClarityName("err")`) beside a receipt with zero cost, and the Hiro API said the
same transaction succeeded with `(ok u390)`. Rung 1 (`call-both-tx`) confirmed
the disagreement independently. Neither `probe-root` nor `state-value` was
needed: a receipt that spends nothing where the chain spends 6.6M runtime is not
a write-ordering question.

### Gates

- `clar_match_builtin_name_binds_only_on_its_own_branch` and its optional twin
  in `words/conditionals.rs` — the crosscheck reproducer, three lines of Clarity.
- `wasm_match_binding_name` in the conformance suite — the same shape through
  nano's own deploy and call path, both engines.
- `xtask check-module` on the real contract against a mainnet state: was
  `Name already used ClarityName("err")`, now `compiles to a module that loads`.
- `cargo test --release -p clar2wasm`: 2,579 passed, 10 ignored, 0 failed.
- `cargo test --release --workspace` clean; `cargo clippy --release
  --all-targets` clean, workspace and `clar2wasm`, no `#[allow]`.

### An adjacent divergence left alone, on purpose

The reference's binding check is
`is_reserved(name) || contract_context.lookup_function(name).is_some() ||
inner_context.lookup_variable(name).is_some()`. clar2wasm's `match` checks only
the first, so binding a name that shadows an *enclosing local* — `(let ((x 1))
(match r x … e …))` — is accepted where the interpreter raises
`NameAlreadyUsed`. Deliberately not changed here: no mainnet block needs it,
and widening a rejection is the direction that risks refusing a contract the
network accepted. Recorded so the next person finds it named rather than
guesses.

## The seventh: an allowance over somebody else's NFT (8,671,301)

Replay parked at **8,671,301**, and unlike the six before it this one failed the
*whole block* rather than one transaction:

```
SP3JNSEXAZP4BDSHV0DN3M8R3P0MY0EEBQQZX743X.xtrata-market-sponsored-stx-v1-1::buy
  compiler     Internal(InvariantViolation(… Expect("NoSuchNFT(\"xtrata-inscription\")")))
  interpreter  Response { committed: true, data: Bool(true) }
```

`Expect` is clar2wasm saying "this cannot happen", so it comes back as an
invariant violation rather than a failed transaction, and the node retried the
block forever instead of divergng on a root.

The contract is an NFT marketplace, and `buy` hands an escrowed inscription to
the buyer inside a Clarity 4 allowance:

```clarity
(as-contract? ((with-nft (contract-of nft-contract) NFT-ASSET-NAME (list token-id)))
  (contract-call? nft-contract transfer token-id CONTRACT-PRINCIPAL buyer))
```

**The market defines no NFT.** The asset belongs to whichever inscription core
the listing named — the market is a router with an allowlist. clarity-wasm's
`with_nft` host function needed the asset's *key type* to know how to read the
identifier list out of Wasm memory, and looked it up in
`contract_context().meta_nft` — the **calling** contract. There was nothing
there, so it refused a call mainnet accepted.

**The reference asks nothing of the asset.** `check_allowance_with_nft` requires
only that the third argument is a list of at most `MAX_NFT_IDENTIFIERS`, and
`special_allowance` evaluates the three arguments straight into an
`NftAllowance` — no `meta_nft` lookup, no `get_contract`, no existence check of
any kind. An allowance may name an asset that exists nowhere; it simply never
matches anything. Read, not inferred, in
`clarity/src/vm/functions/post_conditions.rs` and
`analysis/type_checker/v2_1/natives/post_conditions.rs`.

So there was no key type to find, and the fix is not to look somewhere else: the
*compiler* knows the list's type, because analysis gave it one. `with-nft` now
writes that type into literal memory beside the list and the host reads the list
by it, the same way `print` already did — `WasmGenerator::serialized_type_of` is
that shared machinery, and `print` now goes through it too. Both database reads
are gone from the path, including one in the wildcard case that was charging a
contract load for a type the compiler already had.

Three shapes a lookup would have answered *wrongly* rather than not at all are
pinned alongside the mainnet one in `wasm_nft_allowance.rs`: the `"*"` wildcard
over a foreign asset, an allowance naming an asset whose key type is a `(buff
32)` where the list holds `uint`s, and an allowance naming an asset that exists
in no contract. Plus the negative — allowing a *different* identifier still
refuses the transfer — because a fix in the permissive direction here would turn
an allowance into a formality, and that is the dangerous direction.

`cargo test --release -p clar2wasm` is 1,378 passed, 6 ignored. Two test-side
arities had to move with the host's: `standard.wat`'s import (whose sixth and
seventh parameters were both named `$identifiers_offset` — the second was the
length) and the developer-mode stub linker.

### Not the same family as the first six

The four bugs at 8,665,719 / 8,666,585 / 8,667,467 / 8,668,161 were all one
mistake: a value laid out for one type and read as another. The fifth was an
unbalanced stack from the same override, and the sixth was a rejection that came
too early. This one is different again — **a host function reading state to
recover something the compiler already knew**, and reading the wrong state. Worth
naming as a class, because `with_ft` and `with_stacking` are next door and the
same question applies to them.

## The eighth is not a compiler bug at all: a tenure's fees, one tenure late (8,673,846)

```
state root mismatch at height 8673846:
  expected 85ea9fa6ab48a0cea0c6a9ae51210cc8d70e3a0f8a85309170f5881e4d37a163
  got      9fc9a9ff92602b13d5f67dd90c29349720daaa4f675e782caf9efa1f0e5ed87b
```

The block starts tenure 251,421 and holds two transactions, a `tenure_change`
and a `coinbase`, both succeeding at zero cost on chain and in nano. Nothing the
VM does is in question.

**What it was.** A tenure's fee total is not recorded in its own payment
schedule. stacks-core cannot total it until the next tenure change proves the
tenure over, so it writes it into the *following* tenure's schedule as
`MinerPaymentTxFees::Nakamoto { parent_fees }` and pays it out, a maturity later,
to the recipient of that schedule's parent. So two tenures pay out at every
tenure start, one tenure apart:

- the maturing tenure's **coinbase**, to its own recipient;
- the **previous** tenure's **fees**, to *its* own recipient.

nano paid the previous tenure's recipient the *maturing* tenure's fees. At
8,673,846 that is 22,539,119 uSTX — tenure 251,321's fee total — where mainnet
pays 15,114, tenure 251,320's. Same recipient, wrong tenure's money.

The reason it survived 8,128 blocks is the whole lesson. `xtask
capture-fixtures` copied `payments.tx_fees_anchored` into the checkpoint's
`fees` field verbatim, so every tenure the checkpoint carried held its
*predecessor's* total under its own name. Against data shifted like that,
"the maturing tenure's fees to the previous tenure's recipient" and "the
previous tenure's fees to its own recipient" are the same arithmetic. The two
readings first differ at the earliest tenure nano totalled for itself — 251,321,
which starts at 8,665,610, nine blocks past the checkpoint anchor — and that
tenure matures exactly 100 tenures later, at 8,673,846. The bug was scheduled.

The fix names one convention and holds it everywhere: `TenureEarnings::fees` is
the total *this* tenure's own transactions paid. `effects_for_tenure` takes both
halves of the second credit from one entry, `earnings[matured - 1]`, so the
recipient and the amount can no longer come apart. `capture-fixtures` reads
`fees` from the following tenure's schedule, which is where stacks-core keeps it
and what `hacknet/signer-checkpoint.sh` already did.

### What was ruled out, and by which single measurement

The write trace (`NANO_TRACE_WRITES=1`) is twelve writes over eight distinct
keys, and `cargo xtask probe-root` reproduced nano's root from exactly those —
so the trie, the ordering and the journal were all in agreement and the fault
was in a key or a value. `probe-root` also showed no single omitted write and no
permutation reaching the expected root.

- **Five burn operations that wrote nothing.** The strongest-looking lead, and
  dead in one query: `regex 6a4c50 5832` over the raw block from
  `mempool.space/api/block/<hash>/raw` finds exactly five `6a 4c 50` OP_RETURNs
  whose op byte is `5b`, `[`, `LeaderBlockCommit`. Five commits and no
  stack/transfer/delegate/vote op, which the archive corroborates: every burn
  block near 960,341 carries exactly five, and `leader_keys`, `stack_stx`,
  `transfer_stx` and `delegate_stx` are all empty or centuries behind. Five ops
  writing nothing is the *correct* number of writes.
- **`check_and_handle_reward_start`, which nano does not implement at all.**
  Also dead by reading: in 2.5 and later `handle_pox_cycle_start_pox_4` and
  `_pox_5` are `Ok(vec![])` and do not even call `mark_pox_cycle_handled`.
  Missed-slot auto-unlocks ended in Epoch 2.5; the handler writes nothing in
  4.0, at a cycle boundary or anywhere else.
- **`process_stx_unlocks` and the prepare phase.** Burn 960,381 is at offset 331
  of cycle 140, no boundary; nano's unlock write is present and increments the
  liquid supply by zero, which is what the network does too.
- **Every value nano wrote.** `Sha512_256` of the candidate value string against
  the traced `MARFValue` settles a value in one command, because the MARF leaf
  *is* that hash: `printf '%s' 1785491070 | openssl dgst -sha512-256` is
  `67b3ced7…`, the traced `block_time`. Seven of the eight matched what the chain
  implies — `block_time`, `tenure_height` (`0003d61d`), both miner nonces, the
  miner's unchanged balance, the coinbase credit, the SIP-031 mint and the liquid
  supply. The eighth, `SP70B98…`'s balance, did not, and `sqlite3 clarity.sqlite
  'select value from data_table where key like "66635b19…%"'` gave the number
  nano actually wrote: 2,025,225,302 against the chain's 2,002,701,297.

### The cheapest measurement that would have found it first

Hash the value, not the balance. The chain's `/extended/v1/address/…/stx` says
what an account holds; it does not say what nano wrote. Six amounts were
"confirmed against the chain" here and one of them was confirmed against nano's
*intent* — a log line — rather than its trie leaf. `Sha512_256(value_string)`
compared with the traced `MARFValue` is a single-command check that admits no
such gap, and it is available for every write in the trace, not just the ones
an explorer indexes.

Second, and more general: the accounting cross-check the fix ships as a tool.
`cargo xtask repair-ledger <state> <index.sqlite>` now restates every tenure's
fee total from stacks-core's own `payments` rows. Run against the parked state it
moved 110 of 201 entries and left 91 untouched — the 110 being everything the
checkpoint handed over or `repair-ledger` had filled, and the 91 being every
tenure nano totalled itself, agreeing with the archive to the microSTX. A
disagreement in either direction is a bug, and one command finds it a hundred
tenures before the payout does.

### Not the same family as the first seven

The seven before this were all inside clarity-wasm. This one is nano's own
chainstate arithmetic, and it is the first divergence that was *invisible by
construction* rather than merely undiscovered: a checkpoint that carried a field
one tenure out of phase and a reader that read it one tenure out of phase agreed
with each other, and with the chain, for as long as the checkpoint lasted. Any
consensus quantity that arrives both from a checkpoint and from execution needs
its convention asserted at the seam, not inferred at each end.

### Gates

`cargo test --release -p nano-chainstate -p nano-vm`, the conformance suite with
`NANO_MAINNET_CAPTURE` (121 passed), and `cargo clippy --release --workspace
--all-targets` are green. The regression is
`crates/nano-conformance/tests/conformance/tenure_fee_maturity.rs`, a hardcoded
mainnet vector: at 251,421, `SP2N4YMH4…` takes 1,000,000,000 and `SP70B98…`
takes 15,114, and 22,539,119 appears in neither credit until the next tenure
start.

## A compile refusal at a call rejects the block now

The release-gate run found that a compile refusal at a *call* is invisible in the
sealed root: it became a failed transaction, a failed transaction writes nothing,
so it sealed the root an untouched block seals — and the root a legitimate
`ArithmeticUnderflow` seals. Only a receipt diff could tell them apart, and nothing
on the follow path diffs receipts.

The two cases are genuinely different and now part company where they should:

- **At a deploy**, an analysis refusal is a chain outcome. Mainnet rejects bad
  contracts too — a deployment naming a contract that does not exist yet is
  ordinary — so `failed_deployment` records a receipt and carries on. 150 of them
  in the replayed range.
- **At a call**, the contract is *already on chain*, which means the network
  compiled it and ran it. clarity-wasm refusing it is nano's gap and not the
  transaction's, so the block is refused. That is what this task's own boundary
  asks for — every clarity-wasm compile, load or trap failure rejects the candidate
  — and it is how seven of the eight mainnet divergences became findable at all.

Checked against the whole replay before being trusted, because a new rejection on
the live path is the kind of change that can only be judged against the chain: the
only call-site analysis failure in 1,899 log lines mentioning one is
`487356b6…`, which is 8,666,585's `rewards-stx-v1` — the fourth divergence, fixed.
Zero in the current run.

## The engine comparison runs in the harness, before sealing

`NANO_REPLAY_BOTH_ENGINES=1` puts every contract call in every replayed block
through both engines and stops the replay on the first disagreement — **before**
the block is applied, which is the whole point: at that moment the chainstate
stands on the parent, which is the state the call actually ran against. Each call
opens a block on the parent and aborts it, so nothing is sealed and no root moves.

Over the captured 340 blocks it finds nothing, and the honest reading of that is
that the fixture is a hacknet chain the two engines already agree on — its value is
on a mainnet slice, where seven of the eight divergences lived.

**So it was proved to bite rather than assumed to.** Inverting the comparison for
one run stops the replay at block 76, on a real
`ST000000000000000000002AMW42H.pox-5::stake-update` — a PoX contract call, the
awkward kind — with both engines' answers printed. That is the mechanism reaching
real calls at a real depth and refusing to seal. Reverted immediately; the run
either side of it is 340/340.

The minimized fixtures the item also asks for are already here and named after the
bugs they hold: `wasm_response_fold`, `wasm_trait_fold`, `wasm_nft_allowance`,
`wasm_match_binding_name`, `wasm_builds_a_let_bound_placeholder`,
`tenure_fee_maturity`.

**The naming, because the test caught it.** The first spelling of the switch
contained `NANO_CROSSCHECK`, one of the four names task 060 retired from the
production call path — and `wasm_is_the_engine` forbids those strings anywhere in
the tree, so it failed on the first run. That is the boundary check working: a name
that reads like a retired production switch is a hazard even when the thing behind
it is a test, and the comment in `crosschecking_engines` now says so.

## The mainnet regression slice cannot be built as written, and why

The item asks to pin "roots, receipts, costs, events and consensus-visible writes"
for a bounded mainnet compiler slice. Three of those five are not obtainable, and
the reason is worth writing down rather than working around:

`/home/aldur/mainnet-capture` declares `receipts = false` and holds **no**
`events/new_block` at all. That stream is what a stacks-core node's *event
observer* emits as it executes, so it exists only if somebody was listening at the
time. It cannot be fetched afterwards: no public API serves stacks-core's
`new_block` payload for a historical block, and the archive's `payments` and
`matured_rewards` tables carry accounting rather than per-transaction receipts,
costs or events. The hacknet capture has all five precisely because nano's own
harness was the observer while that chain ran.

So the slice can pin **roots** against the chain today and nothing else, which
misses exactly the case this task discovered a week of work later: *a compile
refusal at a call is invisible in the sealed root*, because a failed transaction
writes nothing and seals the root an untouched block seals. A root-only slice would
report green through the one failure it most needs to catch.

Two ways to make it real, neither of them cheap, both recorded so the next person
does not rediscover the dead end:

- **Run a stacks-core node with an event observer over the slice** and capture the
  stream properly. That is what `xtask capture-fixtures` does for hacknet and what
  the mainnet capture skipped.
- **Freeze nano's own receipts from a run whose roots matched the chain.** Not an
  oracle — it is nano checking itself — but a legitimate *regression* gate: given
  the roots are chain-verified, a compiler change that alters a receipt without
  altering a root is caught, which is the invisible case. It needs the 146 GB
  checkpoint import to re-execute, so it cannot live in CI as it stands.

The half that *is* done is the one that stops a missing fixture reading as a pass:
`skip_gate` plus `NANO_REQUIRE_MAINNET`, demonstrated both ways, so a release run
that claims the mainnet gates are green had to actually run them.

## The slice exists now, by the second of the two routes

The note above recorded a dead end and two ways out. The second one is built: nano's
own receipts, frozen from blocks whose `state_index_root` mainnet's signed headers
verified before nano sealed them.

What made it cheap was pointing an event observer at the live mainnet catch-up —
`hacknet/event-sink.py`, the same sink a hacknet capture uses — so the receipts
stream that cannot be fetched for a historical block is simply *recorded* as the
chain is executed. 500 blocks, 8,702,046 to 8,702,592: **1,644 transactions and
209,762 events**.

It is frozen as one digest a block rather than as the payloads, because 500 mainnet
blocks of receipts are 525 MB and this has to live in CI; `receipts.json` is 124 KB.
Inside each digest: every transaction's identity, status, returned value and all
five cost dimensions, and every event in the order the block emitted them. Outside
it: the block's height and its Nakamoto hash — which is the signer signature hash,
which commits to `state_index_root`, so the root is pinned without the payload
carrying one.

**Demonstrated falsifiable, which is the only thing that makes a digest worth
anything.** Adding **1** to the runtime cost of one transaction in one block of the
150-block corpus fails the gate, at that block, saying the transaction and event
counts are unchanged — which is the report the failure needs, because identical
counts with a moved digest is precisely a receipt changing while no state does.

Two honest limits, both in the file's own comment. It is nano checking itself, not
an oracle: the roots are chain-verified but the receipts are not, so it catches a
*regression* and cannot catch a divergence that was already there. And comparing it
against a run needs `NANO_MAINNET_RECEIPTS` to name an observer directory, so that
half `skip_gate`s in a working tree — while the half that cannot skip, and panics
rather than skipping when the fixture is missing, is the contiguity and content
check on the frozen slice itself.

## `get-block-info?` was reading a Stacks height where the chain reads a tenure height

The divergence at 8,706,194 was two defects in one host function, both in
clarity-wasm and both right in the interpreter:

- **The height is a tenure height from epoch 3.0 on.** `get-block-info?` predates
  Nakamoto, so a Clarity 1 or Clarity 2 contract passing `(- block-height u1)` is
  passing a *tenure* height — the same switch `block-height` itself made, so that
  the idiom keeps meaning what it meant. stacks-core translates it in
  `special_get_block_info` before its range check; clarity-wasm passed the number
  straight to the Stacks-height reads, which on mainnet names a block from about
  twenty months earlier. Classic primary testnet is excluded, there and now here.
- **`time` is the burn block time**, not the Nakamoto header's own timestamp.
  `get-stacks-block-info? time` is the one that reads the timestamp;
  clarity-wasm called that for both.

`age009-token-lock::get-tokens-many` compares
`(unwrap-panic (get-block-info? time (- block-height u1)))` against a vesting
timestamp forty times over. Against a time twenty months stale, twenty-three of the
forty took the other branch, returned the contract's own
`ERR-BLOCK-HEIGHT-NOT-REACHED`, and did half the writes — a transaction both engines
called a success, with the value, the write count and the root as the only evidence.

**Verified against the chain, not against a reconstruction.** The production binary,
standing on the reflink-copied state sealed at 8,706,193, executes 8,706,194 and
seals `c081728eee3693c80147983ccc72e486082fe3f80cfc488acca46639bbe51ee6` — the root
the signed header commits to — and 32 blocks after it with no mismatch of any kind.

The regression is `conformance/block_info_tenure_height.rs`, which builds the
smallest chain that can tell the two defects apart: tenure heights advancing at half
the rate of Stacks heights, so no tenure height is ever a Stacks height, and a burn
time that is never a Stacks timestamp. Both engines are asked, and they now agree.

## The `match` shadowing item: the premise was wrong, and one map over is right

The item says wasm accepts a `match` binding that shadows an enclosing local
where the reference raises `NameAlreadyUsed`. Measured, it does not, because
neither engine ever reaches the branch: the *analyzer* refuses the contract.

`check_special_match`'s binding check
(`clarity/src/vm/analysis/type_checker/v2_1/natives/options.rs:309-313`) is two
things, not one:

```rust
checker.contract_context.check_name_used(&bind_name)?;
if inner_context.lookup_variable_type(&bind_name).is_some() {
    return Err(StaticCheckErrorKind::NameAlreadyUsed(bind_name.into()).into());
}
```

The second line is exactly the enclosing-local case, at deploy time. So
`(let ((claimed u1)) (match r claimed … e …))` never deploys under either
engine, and the earlier note — which read the interpreter's
`eval_with_new_binding` and inferred the rest — was reading only half the
reference. Pinned as `a_binding_shadowing_an_enclosing_local_is_refused_by_analysis`
in `wasm_match_binding_name`, both engines, same analysis error.

**What the reference does let through is one map over.** Compare the two lists:

| | analyzer's `check_name_used` | interpreter's `eval_with_new_binding` |
|---|---|---|
| reserved words | only `block-height`, Clarity 3+ | all of them |
| private functions | yes | yes |
| public functions | yes | yes |
| **read-only functions** | **no** | **yes** (`lookup_function` has all three) |
| constants, vars, maps, tokens, traits | yes | no |
| enclosing locals | yes | yes |

Two rows disagree in the direction that matters — a name that passes analysis and
is then rejected when the branch runs. The first is the reserved word, which is
8,668,096 and is fixed. The second is a **read-only function's name**, which
`check_name_used` simply does not consult, and which had the same shape of bug:

```clarity
(define-read-only (fee) u107)
(define-private (charged (r (response uint uint)))
  (match r amount (ok (+ amount u1)) fee (err fee)))
```

deploys, and the error branch answered `(err u107)` under the compiler where the
interpreter raises `NameAlreadyUsed("fee")`. This one is the opposite direction
from 8,668,096: accepting what the network refuses, rather than refusing what it
accepts.

The fix reuses what 8,668,096 built. `block_from_bound_expr` compiled a branch to
a runtime `NameAlreadyUsed` when the name was reserved; it now takes the answer to
a predicate, `binding_name_already_used`, that mirrors all three of the
interpreter's clauses — reserved, any of the contract's own functions, an
enclosing local — asked *before* the branch's own binding is in scope. The
enclosing-local clause can never fire in production, because analysis got there
first; it is in the predicate because the predicate's job is to be the same
sentence the reference is, not a list of cases seen so far.

Gates: `clar_match_read_only_function_name_binds_only_on_its_own_branch` and
`clar_match_shadowing_an_enclosing_local_is_refused_by_analysis` in
`words/conditionals.rs`, and
`a_binding_shadowing_a_read_only_function_is_rejected_only_where_it_binds` in
`wasm_match_binding_name` — taken branch and untaken branch, both engines. The
read-only test fails with the predicate reduced back to `is_reserved_name`; the
shadowing test passes either way, which is itself the evidence that it was never
a divergence.

**What it does not prove.** No mainnet block in the replayed window binds a
read-only function's name in a `match`, so this is conformance ahead of a
divergence. And the predicate reads the contract's *analysis*, which holds every
function the contract defines — where the interpreter's `contract_context` during
a top-level expression at deploy time holds only the ones above it. A top-level
`match` binding the name of a function defined further down the same contract
would be refused a branch the reference would allow. Deliberate: it is one
obscure shape against the whole of the ordinary call path, where the analysis view
and the runtime view are the same set.

## The tuple supertype asymmetry cannot be closed inside clar2wasm

The item asks to make both engines agree on the *complete* value a narrowed
`default-to` or `if` returns, not only on the fields read through `get`. It stays
open, and the reason is not that nobody has picked the right layout — it is that
there is no layout to pick.

Measured on the interpreter alone
(`the_reference_answer_here_has_no_single_static_layout` in
`wasm_response_fold`), `(default-to { soft: false } entry)` over an
`(optional { soft: bool, full: bool })` answers:

```
entry = (some { soft: true, full: true })   { "full": bool, "soft": bool }
entry = none                                { "soft": bool }
```

Two different **shapes**, from one expression, chosen at run time. `default-to`
returns whichever value its branch produced, unconverted, so the answer's type is
not the expression's analysed type — `least_supertype` walks the *default's*
fields and drops the rest, giving the narrow one — and it is not any other single
type either.

clar2wasm fixes a value's representation from one static type. Narrowing, which
is what it does today, reproduces the `none` branch exactly and loses `full` on
the `some` branch. Widening would reproduce the `some` branch and have to invent
a `full` for the other, which no conversion can do. A discriminant carried beside
the value would work and would infect every consumer of every such expression.
So the choice is between two wrong answers, and today's is the one that agrees
with the reference on the branch that has nothing to convert.

That places the fix outside a shipped node. What closes it is the reference
itself: either `least_supertype` refusing the asymmetric pair the way it already
refuses the reverse one, or sanitizing a value to its analysed type when it is
returned. `Value::sanitize_value` already exists and already does exactly this
narrowing — epochs ≥ 2.4 apply it to function *arguments*
(`callables.rs:303`, `sanitize_in_function_invocation`) and to values read out of
the datastore, and not to returns. `special_contract_call` carries a
`type_returns_constraint` only for dynamic trait dispatch, and checks it with
`admits` rather than sanitizing. So a wide tuple does cross a static
contract-call boundary and does land in a receipt.

The differential stays `#[ignore]`d with that reason rather than deleted, and
the measurement above is *not* ignored, so the claim keeps being checked. Written
up because the previous note left it as "needs a decision at the analysis layer",
which reads like an unfinished afternoon rather than a question that has to be
asked of stacks-core.

**What none of this proves.** That the shape is unreachable on mainnet — only
that no block in the replayed window reaches it. `blacklist-susdh-v1` reads all
three of its `default-to`s through `get`; a contract that returns such an `if` or
`default-to` whole, across a contract-call or into a receipt, would diverge on
nano today. It is one known open Clarity semantic differential, tracked in
[[068-resolve-asymmetric-tuple-least-supertype-semantics]]; the deployment-time
constant-target call in [[067-reject-contract-call-through-a-constant-while-depl]]
is the other. Both belong in the release report until their ignored tests are gone.

## It is not one divergence, it is five, and two of them move the state root

The item asks for agreement on the *complete returned value*. Measured through
`default-to` — the reachable route — the disagreement was never confined to a
returned value. `(default-to { soft: false } entry)` over an
`(optional { soft: bool, full: bool })`, with
`entry = (some { soft: true, full: true })`, in every position the value can
reach:

| escape | clarity-wasm | reference |
|---|---|---|
| returned into a receipt | `0c0000000104736f667403` | `0c000000020466756c6c0304736f667403` |
| `var-set` into a narrow var | `(ok { soft: true })`, the var written | `RuntimeCheck(TypeValueError)`, nothing written |
| passed to a narrow parameter | `{ soft: true }`, the call runs | `RuntimeCheck(TypeValueError)` |
| `is-eq` against `{ soft: true }` | `true` (`03`) | `false` (`04`) |
| `print` | event value `{ soft: true }` | event value `{ full: true, soft: true }` |

Eleven bytes against eighteen is the whole of the first row: `0c` tuple, a
`u32` field count of 1 against 2, and `0466756c6c03` — `full: true` — present in
one and absent in the other.

Two of those five abort the transaction under the reference and succeed under
nano, so they are **state-root** divergences and not receipt ones. A wrong root
is the failure mode this task exists to prevent, and a `var-set` of a
`default-to` is not an exotic shape.

### Where each engine decides, exactly

- **The reference's type.** `least_supertype_v2_1`'s `TupleType` arm
  (`clarity-types/src/types/signatures.rs`) walks the *first* argument's
  `type_map` and looks each name up in the second, dropping whatever the second
  has left over; `check_special_default_to` calls it as
  `least_supertype(default, payload)`, so the default's field set wins. The
  reverse pair — a default *wider* than the payload — is a `TypeMismatch` and
  never deploys.
- **The reference's value.** `native_default_to`
  (`clarity/src/vm/functions/options.rs:331`) returns `*data` for `some` and
  `default` for `none`, unconverted either way. Nothing sanitizes a return.
- **clar2wasm's layout.** `words/default_to.rs` sets only the *default* to the
  expression's type and converts the payload with
  `need_ducktyping` → `duck_type`; `duck_type.rs`'s `TupleType`/`TupleType` arm
  of `duck_type_stack` `drop_value`s a source field the target does not name.
  The host then reads the answer back per the function's analysed return type.
- **The reference's run-time checks**, which is what turns a shape into an abort:
  `ClarityDatabase::set_variable` calls `TypeSignature::admits`, and
  `TupleTypeSignature::admits` refuses a differing field count outright;
  `clarity2_implicit_cast` (`clarity/src/vm/callables.rs`) walks the *value's*
  fields on the way into a function and raises `TypeValueError` for one the
  parameter does not name; `Value`'s `PartialEq` on `TupleData` includes the type
  signature, which is the `is-eq` row.

The cast's own comment is the finding worth carrying upstream: *"This should be
unreachable if the type-checker has already run successfully."* The reference's
runtime believes this value cannot exist, and its analyzer hands it out. That is
where the bug lives, and it is why no arrangement of bytes inside clar2wasm is
the fix.

### The one thing that was wrong in the note above

It said epochs ≥ 2.4 sanitize function arguments, so a wide tuple would be
narrowed at that boundary and the engines would agree there. Measured, they do
not: `sanitize_in_function_invocation()` is `>= Epoch40`, and even in 4.0 the
sanitizer never runs on this value because `clarity2_implicit_cast` refuses it
two lines earlier. `Value::sanitize_value`'s tuple arm would have dropped the
field quite happily. Read, then measured, then corrected — the earlier claim was
read from one call site and inferred.

### Why it still cannot be closed inside clar2wasm

Unchanged, and now with a fifth escape behind it. A wasm value's representation
is fixed by one static type; the reference's answer has two shapes and picks one
at run time. Narrowing reproduces the `none` branch exactly and loses a field on
the `some` branch. Widening reproduces the `some` branch and would have to invent
a field for the other. A discriminant carried beside the value reproduces both
and changes the representation of every tuple type in the program.

A fourth option was considered and rejected on measurement rather than taste:
clar2wasm *can* tell a narrowing that drops a real field from any other
duck-typing, at compile time, and it does know at run time which branch it took —
the `if_else` on the optional's indicator is right there. So it could trap on the
wide branch and turn a silent divergence into a refused block, which this task's
own boundary would prefer. It is not shippable: `blacklist-susdh-v1` takes that
branch on every present entry and reads it through `get`, where the two engines
agree, so the trap would stop mainnet replay at 8,667,509 on a contract that is
correct today. A use-site rule — safe under `get`, unsafe everywhere else — is
the shape a real compiler-side fix would take, and it is a static analysis over
uses that clar2wasm does not have.

### What is pinned, and where

- `a_narrowed_default_agrees_wherever_it_escapes_a_get` in `wasm_response_fold`,
  still `#[ignore]`d, now covering all five escapes rather than the returned
  value alone. Un-ignoring it is the single gate for this item.
- Three *not* ignored measurements beside it, so the claim keeps being checked
  and the reference's half can never move quietly:
  `the_reference_answer_here_has_no_single_static_layout` asserts the exact
  serialized bytes of both branches,
  `the_reference_carries_the_branch_into_state_and_into_a_comparison` asserts the
  `var-set` refusal and the `is-eq` answer, and
  `the_reference_refuses_to_pass_the_wide_tuple_to_a_function` asserts the cast
  refusal.
- `clar_default_to_narrowing_answers_with_the_branch_the_reference_took` in
  `clar2wasm/src/words/default_to.rs` — the same three snippets in clar2wasm's
  own crosscheck harness, `#[ignore]`d, so the reproducer sits in the file a
  fixer would open. It is what to send upstream with the `least_supertype`
  question.
- Every differential in `wasm_response_fold` now compares the two engines'
  **consensus serialization and emitted events** as well as their values, because
  two values that print alike and serialize differently are still a divergence.
  That is what caught the `print` event row.
