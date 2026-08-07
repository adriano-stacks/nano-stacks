---
id: "073"
title: "Decide whether a contract clarity-wasm cannot load is mainnet-valid"
status: in-progress
priority: critical
effort: large
dependencies: ["060"]
tags: ["mainnet", "vm", "clarity", "conformance", "release"]
created_at: 2026-08-07
type: bug
group: mainnet
---

# Decide whether a contract clarity-wasm cannot load is mainnet-valid

## Objective

`clar2wasm` emits a module wasmtime refuses to load — `too many locals: locals
exceed maximum` — for a contract the Clarity analyzer accepts. Nothing between
the source and the validator counts locals, and a `let` binding becomes one.

Today this is only known as a *manufactured* case: `engine_failure.rs` forces it
with a `let` of 60,000 bindings, precisely because it was the one reachable way to
make the runtime refuse a module without breaking the compiler, and the positive
control there is that the interpreter deploys and runs the same source without
complaint. Same source, same state, one engine answers and the other refuses.

That is a manufactured gap in a gate and a real one in the engine. The question
this task answers is **where the boundary actually is** — how many bindings, in
what shapes, and whether any of it is reachable by a contract the network would
accept. If it is, the node refuses a valid block, which under the release rule is
a conformance bug and not a thing to accept, waive or route around.

The rule this task is bound by: production never falls back to the interpreter. If
a mainnet-valid contract cannot load, the fix is in code generation.

## Why now

Two independent pressures push the local count up, and both are nano's:

- The `as-contract` sender-restore fix adds **two `i32` locals to every function
  prologue**, whether or not the function contains an `as-contract`. 1,375 tests
  stay green, and that is exactly the shape upstream issue #575 ("let function
  throwing a too many locals issue") is about. [[060]] records this and says the
  cheaper form — emitting the save/restore only for functions whose body contains
  an `as-contract` — should not wait forever.
- Nothing bounds a real contract's `let` nesting except the analyzer, and the
  analyzer's caps are elsewhere: a function's parameters are capped at 256, far
  below wasm's 1,000, but locals have no such gate.

## Tasks

- [x] Measure the boundary rather than assume it: find the smallest contract, in
      each shape that produces locals (`let` width, `let` nesting, `match`/`try!`
      bindings, function count × prologue locals), that clar2wasm compiles and
      wasmtime refuses. Report the count per shape, not one number. **Measured
      per shape 2026-08-07: composite-copy flood (poc2) ~51,800 → 998;
      60,000-binding let, one read → 8; 26,000 all-read bindings 52,006 → 8;
      mainnet sweep max 16,505 (see "Mainnet-state margin"). `match` binds ≤2
      names and cannot reach the wall; prologue locals are 3/function. No
      source-level shape reaches the locals limit anymore — the wall moved to
      function-type arity (see "B2 shipped").**
- [x] Establish whether stacks-core accepts those same sources — analyzer limits,
      read-length and the deploy cost budget — so the answer distinguishes "the
      network would reject this too" from "the network accepts what nano cannot
      load". **Answered: the network accepts what nano cannot load** — see
      findings below.
- [x] Search the imported mainnet state for contracts near the boundary. The
      checkpoint carries every deployed contract, so this is a measurement over
      real state, not a guess: report the highest local count any mainnet contract
      reaches and its margin. **Done 2026-08-07: 137,332/137,340 compile; highest
      peak 16,505 of 50,000 (3.0× margin; ~7× organic; 99.6% under 1k) — see
      "Mainnet-state margin".**
- [ ] Run the eight contracts that failed the margin sweep through nano-vm's exact
      production `compile_under` path. Classify every one as a sweep-harness fault,
      network-invalid source or a distinct clarity-wasm conformance bug, and open a
      blocking task for every confirmed bug rather than excluding it from the
      denominator.
- [x] Make the `as-contract` prologue pay for itself: emit the save/restore only
      for functions whose body contains an `as-contract`, and re-measure. This is
      worth doing whatever the answer above is, because it is a cost every mainnet
      contract currently carries. **Moved to
      [[081-emit-the-as-contract-sender-restore-prologue-only]] (2026-08-07) — an
      optimization, not part of this bug.**
- [x] If a mainnet-valid contract cannot load, fix code generation so it does —
      no interpreter fallback, no healing path, no per-contract exception. Reuse
      of locals across disjoint scopes is the obvious lever and belongs upstream.
      **All three mechanisms shipped 2026-08-07: A (scoped reuse, 0cc14f60), B1
      (use-based liveness, f0d91c38), B2 (wide-scope spilling, 23196b51). Every
      measured shape now loads: poc2 (~51,800→998), 60k-let (→8), 26k all-read
      (52,006→8), all 137,332 compilable mainnet contracts (max 16,505).**
- [x] Keep the manufactured module-load refusal in `engine_failure.rs` working
      whatever the fix is. A gate exercisable only while a divergence is open
      stops working the moment somebody closes one, and that gate is about the
      *boundary*, not about this bug. **Gate green through three forcing-case
      migrations: 60k-binding let (pre-A) → 26k all-read bindings (B1) →
      600-field tuple return (B2, wasmparser's 1,000-result type limit). Each
      migration's clar2wasm-side test pins the shape still compiles and the
      runtime still refuses; the positive control pins the interpreter accepts
      the same source.**
- [ ] Close the function-type arity wall under
      [[084-eliminate-wasm-function-type-arity-refusals-for-ne]] before treating
      the moved refusal as harmless. The 600-field tuple still demonstrates an
      interpreter-accepted source that clarity-wasm cannot load; its network
      validity and ABI fix are release questions, not failure-test details.
- [ ] Record the outcome in the release report either way: a measured margin is
      evidence, "no mainnet block has hit it" is not. **Recorded here:
      "Mainnet-state margin" (max peak 16,505/50,000 over all 137,332
      compilable mainnet contracts, 3.0× worst-case, ~7× organic, 99.6% under
      1k) plus per-shape before/after numbers in "A+G shipped", "B1 shipped",
      "B2 shipped". This remains open until the gate-time report consumes the
      measurement and the unresolved eight-contract and arity results.**

## Findings

**The network accepts what nano cannot load — for the source measured so far.**
`engine_failure.rs`'s positive control is the whole argument: the interpreter is
the engine the network runs, and it deploys and executes the 60,000-binding `let`
without complaint, from the same source and the same state where clarity-wasm's
module is refused at load. One engine answers and the other refuses, so the
refusal is nano's, not Clarity's.

What that does **not** yet establish is the part the release decision turns on:
whether a contract small enough to be a real mainnet transaction can reach the
limit. A 60,000-binding `let` is about a megabyte of source and is answerable on
read-length and deploy cost grounds alone. The smallest failing contract per
shape is the first subtask above for exactly that reason, and until it has a
number this task states the direction of the gap and not its reach.

## Acceptance Criteria

- The local-count boundary is measured per shape, and the margin between it and
  the highest count reached by any contract in the imported mainnet state is
  stated as a number.
- No source that stacks-core accepts and executes is refused by the node's
  engine at load time.
- The `as-contract` prologue cost is paid only by functions that need it.
- The production node contains no interpreter fallback for a module-load
  refusal, and `engine_failure.rs` still forces all three refusal classes.
- The release report states the measured margin rather than the absence of an
  observed failure.
- All 137,340 imported contracts have a production-path verdict; none is omitted
  because the measurement harness could not compile it.
- Task 084 has established and, where necessary, removed the network-valid
  function-type arity boundary.

## Reopened by the 2026-08-07 audit

The locals work is real and remains complete. The task status was not: eight
mainnet contracts still have no production-path verdict, the replacement
600-field-tuple failure has not had its network validity established, and the
release report does not emit the claimed margin. Those are direct exceptions to
this task's own "no accepted source is refused" acceptance criterion, so the task
is in progress until the unchecked items above close.

## Findings (2026-08-07)

The headline question is settled: **a mainnet-valid contract reaches the
failure**, and it does not take 60,000 bindings.

- The compact witness is the `poc2` generator in
  `vendor/clarity-wasm/clar2wasm/benches/comparison.rs:429` — a 258-field
  nested tuple bound once, then duplicated `i` times as `(list a a … a)`. At
  `i=100` the source is **1,814 bytes**. `clar2wasm` compiles it to a 1.2 MB
  module that wasmtime 15.0.1 refuses at validation: `too many locals: locals
  exceed maximum`. The boundary for this shape sits between 95 and 100 copies —
  roughly 500 wasm locals per 2 bytes of Clarity source. Verified by direct
  probe against the vendored `clar2wasm`/`wasmtime` rlibs.
- stacks-core accepts the same source everywhere it matters: AST build and
  analysis pass, the interpreter deploys it (`Ok(None)`), and the total deploy
  cost is runtime 442,362 against the 5,000,000,000 block budget (~0.009%),
  write_length 880 against 15,000,000. The contract uses only Clarity 1
  constructs (`define-public`, `let`, `list`, tuple literals), so this holds in
  every epoch, and 1.8 KB is nowhere near any transaction or block size limit.
- The limit is not a tunable: `MAX_WASM_FUNCTION_LOCALS = 50000` is a hardcoded
  const in wasmparser (0.116 through 0.254) and wasmtime 15 exposes no knob.
  Raising it would not help anyway — at ~500 locals per 2 source bytes, a
  100 KB contract (5% of a block) would need ~25M locals. The fix is code
  generation, as this task already suspected: the copies' lifetimes do not
  overlap (the witness discards the list immediately), so local reuse or
  spilling composite temporaries to memory collapses the count.
- Root cause is the value representation: the interpreter never flattens
  tuples, while clar2wasm assigns one local per leaf field per copy. Same
  family as the asymmetric-tuple problem, different symptom — that one is a
  wrong-answer risk, this one is a no-answer stall.

Still open: the per-shape boundary table (`let` width/nesting, `match`/`try!`,
function count × prologue), the search of imported mainnet state for the
highest local count any deployed contract reaches, the `as-contract` prologue
fix, and the codegen fix itself — now confirmed release-blocking rather than
hypothetical.

## Solution sketch (2026-08-07)

Root cause, in code: `save_to_locals` (`wasm_generator.rs:2143`) always
allocates fresh locals and never frees them; a `let` scope exit restores the
name map but not the LocalIds. A pool exists (`borrow_local`/`BorrowedLocal`,
`wasm_generator.rs:1573`) but only small scalar temporaries inside word
implementations use it. Named bindings and sequence-construction saves bypass
it. Allocation is monotonic per function — that is the whole bug.

Options considered:

- **A. Scoped local reuse (chosen, near-term).** Allocate from the pool in
  `save_to_locals` and return a binding's locals when its lexical scope
  closes. Sound because reuse is purely lexical (no closures in Clarity, and
  the name-map restore already makes dead locals unreadable). Collapses poc2
  from ~50k locals to ~1–2k: the binding lives once, the per-element
  temporaries share one slot set. Crucially, the 60,000-binding `let` keeps
  all bindings live in one scope simultaneously, so it still refuses and the
  `engine_failure.rs` gate keeps working unchanged. Residual exposure after A:
  a single scope with >50k simultaneously-live leaves — whether that shape is
  mainnet-valid is what the boundary table answers.
- **B. Box large composites in linear memory (chosen, structural).** Above a
  leaf-count threshold, pass composite values as one `i32` pointer into
  workspace memory. Collapses every shape, including 60k bindings. Large
  blast radius: composite words, the user-function calling convention, the
  `standard.wasm` ABI. Design jointly with the asymmetric-tuple work
  ([[068-resolve-asymmetric-tuple-least-supertype-semantics]]) — both are
  value-representation problems.
- C. Post-pass local coalescing (wasm-opt-style). General and codegen-free,
  but adds a consensus-critical external transformation and breaks the
  manufactured gate, which would need a new forcing case. Fallback if B
  stalls.
- D. Function outlining. Raises the per-function ceiling without removing the
  blowup; early-return control flow across function boundaries is messy.
  Rejected as primary.
- E. Raising the limit. Hardcoded in wasmparser, no wasmtime knob, does not
  scale (100 KB contract → ~25M locals). Rejected.
- F. Interpreter fallback. Forbidden by the release rule. Rejected.
- **G. Compile-time local counting (chosen, complementary).** Count locals
  during generation, refuse early with a precise error, and emit the
  per-shape boundary table this task's first checkbox asks for. Does not fix
  conformance; makes the residual exposure a measured number.

Plan: A + G first in worktree `agent/locals`, merge, then B.

Upstream status (checked 2026-08-07): [stx-labs/clarity-wasm#575](https://github.com/stx-labs/clarity-wasm/issues/575)
("let function throwing a too many locals issue") is **open, unassigned, no
fix in flight**. The most recent maintainer comment (2026-05-27) proposes
exactly option B — detect the overflow and spill locals to memory with
compile-time-known offsets — so B is likely welcome upstream, and A/G are a
contribution candidate as well.

### B design inventory (2026-08-07, full representation survey)

- Good seams (change the helper, callers follow): `clar2wasm_ty`
  (`wasm_generator.rs:234-269`), `write_to_memory`/`read_from_memory`
  (`:1589-1878`), `save_to_locals`/`drop_value`, and the already-memory
  host boundaries (maps, data-vars, `contract-call?`, `print`).
- Hard spots (open-coded flattened assumptions): `words/tuples.rs` (all three
  words), `equal.rs:429-496`, `duck_type.rs:144-181`, `copy.rs:157-174`,
  `deserialize.rs:980-1068`, the `widen_actions` machinery in `visit_atom`
  (`wasm_generator.rs:2255-2307`, assumes 1:1 leaf-slot correspondence), and
  `runtime_size` optional/response slicing.
- The decisive constraint: **the host ABI is the flattened mapping** —
  `pass_argument_to_wasm` / `wasm_to_clarity_value` / `wasm_value_types`
  (`wasm_utils.rs`) and the exported function signatures. Boxing must
  therefore be internal-only: keep flattened export wrappers for
  public/read-only functions (and `.top-level`) that box/unbox at the
  boundary, or the host side of nano-vm changes too. Stdlib calls never see
  composites (scalars + memory pairs only), so `standard.wasm` is unaffected.
- Duplicates to update in lockstep: `wasm_value_types`
  (`wasm_utils.rs:1444-1480`) mirrors `clar2wasm_ty`; `get_type_size` exists
  twice (`layout.rs:6-21`, `wasm_utils.rs:646-669`).
- Mechanical blast radius: 74 `clar2wasm_ty` uses in 17 files, 46
  `save_to_locals` uses in 14 files.

## A+G shipped (2026-08-07, merge 0cc14f60)

A (scoped local reuse) and G (compile-time measurement) are merged to main.
`save_to_locals` allocates from the local pool and slots are released at
lexical scope exit (`let` bindings, variadic args, fold accumulators, tuple
construction/projection, and the per-read copy-charge save in
`clarity_value_size_on_stack` — the dominant leak). G surfaces
`CompileResult.locals_report`: peak simultaneously-live locals per function.

Measured on the poc2 witness (100 copies): **998 peak live locals, down from
~51,800** — a 52× reduction, 50× under wasmtime's 50,000 limit. The
60,000-binding `let` still peaks above the limit (all bindings live in one
scope), still compiles, and is still refused at module load, so the
`engine_failure.rs` gate keeps forcing all three refusal classes unchanged.

Validation on the merged commit: clar2wasm lib suite 1,398 passed / 0 failed;
`engine_failure` 6/6; clippy clean on clar2wasm and nano-vm; regression tests
`wide_tuple_read_many_times_stays_loadable` (crosscheck + wasmtime load +
<10,000 peak assertion) and
`more_simultaneous_bindings_than_wasmtime_allows_still_compiles`.

Residual exposure after A: a single scope holding >50k simultaneously-live
leaf values (e.g. the 60k-binding let) still refuses. Whether that shape is
mainnet-valid under cost/size limits is what the per-shape boundary table
(first checkbox) and the mainnet-state search answer; B closes the class
structurally. A broad pool-conversion sweep (remaining raw
`module.locals.add` sites across 22 word files) was tried and **discarded**
2026-08-07: it broke `cost::word::map_v{2,3}_with_cost` — a borrowed slot
reused before its last read in the `map` word's charge path — real aliasing
risk for a modest locals gain A already delivers where it matters.

### The residual class is mainnet-reachable, and B must cover scalars
(2026-08-07)

Measured on the exact 60,000-binding `let` from `engine_failure.rs`: 708,925
source bytes (under `MAX_BLOCK_LEN` = 2 MiB,
`stacks-codec/src/transaction.rs:55`), interpreter deploys it and the call
answers `(ok u1)`. Cost fits with wide margin (per-binding analysis is
linear; even ~1,000 runtime/binding is 60M against the 5e9 block budget, and
the deploy's write_length is the ~0.7 MB source against 15M). So the residual
class is not theoretical: a miner could include it.

Consequence for B's design: those are **scalar** locals — boxing large
*composites* does not shrink a `let` with 60k uint bindings, and an
all-live-and-used variant is constructible flatly (`(list a0 a1 … aN)`, no
nesting-depth cap). Closing the class therefore needs more than composite
boxing: use-based liveness for never-read bindings, a coalescing/linear-scan
pass over the generated IR, or the upstream-suggested spill-to-memory with
compile-time offsets. Note the gate interplay: a fix that makes the 60k-let
loadable retires the manufactured refusal, so `engine_failure.rs` needs a new
forcing case at that point (as this task already anticipated).

### B refined (2026-08-07): two sub-cases, two mechanisms

- **B1 — use-based (last-use) liveness for named bindings.** A's release is
  scope-based; a pre-pass over each function body can count atom uses per
  binding, drop the save entirely for never-read bindings (the value is still
  evaluated for cost and side effects), and release at last use otherwise.
  This kills the never/rarely-used scalar flood (the 60k-let reads only
  `a0`). Moderate change, highest conformance value per effort. Caveat:
  walrus/wasmparser count *declared* locals per function, and pooled reuse
  makes declarations track peak concurrency — so the hard core that survives
  B1 is "50k locals genuinely live at one program point", e.g. 26k bindings
  all read inside one final expression.
- **B2 — composite boxing / spill.** A single live binding can still
  overflow: `MAX_VALUE_SIZE` is 1 MiB
  (`clarity-types/src/types/mod.rs:43`), so one tuple of ~26k int fields
  (~440 KiB value, ~1 MiB literal — under the 2 MiB block) flattens to ~52k
  i64 locals on its own. Boxing large composites as an i32 pointer (the
  upstream-suggested direction) covers this and the B1-hard core alike;
  spill-to-memory of locals is the general form.
- Sequencing: B1 first (covers the demonstrated reachable case), B2 after,
  with the per-shape boundary table quantifying what remains at each step.
  The `engine_failure` gate needs a new manufactured refusal once B1 lands
  (an all-used-at-one-point binding set is the candidate shape).

B1 implementation sketch: pre-pass per function body (and `.top-level`)
walking the typed AST to compute, per lexically-bound name, its use count and
last-use `SymbolicExpression` id. Then:

- zero uses: `Let::traverse` still traverses the value expression (cost and
  side effects) but `drop_value`s it instead of `save_to_locals` +
  `bindings.insert`; collision checks still apply.
- otherwise: release the binding's locals at the last read — inside
  `visit_atom`, after the `local.get`s, when the atom's id is the recorded
  last-use id. Sound because the gets have already executed in program order
  and the copy-charge path (`clarity_value_size_on_stack`) re-saves from the
  stack into its own slots.
- `match` arm bindings get the same treatment; function parameters stay
  live-for-body (few).
- Gate consequence: with B1 the 60k-let compiles to a tiny module that
  LOADS, retiring the current forcing case. The replacement: N ~25k uint
  bindings all read inside one final `(list a0 … aN)` — all live at the list
  construction point, so ~50k+ declared locals, still compiled by clar2wasm
  and still refused by wasmtime. Pin with a test before flipping
  `engine_failure.rs` over to it.

## B1 shipped (2026-08-07, merge f0d91c38)

Use-based liveness landed: a `BindingUses` pre-pass counts reads of every
`let`/`match` binding (scope-mirrored, shadowing-safe; parameters excluded),
`visit_atom`/`traverse_callable_reference` release a binding's locals at its
last read, and zero-use bindings are evaluated then dropped. Measured:
**60,000-binding `let` with one read peaks at 8 live locals and wasmtime
loads it** (was: refused at >100k); poc2@100 unchanged at 998. The
module-load refusal class stays reachable via 26,000 all-read bindings (peak
52,006 → still refused); `engine_failure.rs` swapped its forcing source to
that shape and stays 6/6 green. Validated independently: clar2wasm lib suite
1,402/0, gate 6/6, clippy clean both crates.

### B1 diverged on mainnet, and the fix is generation order (2026-08-07)

B1's soundness argument — "code generation traverses each expression exactly
once, so a binding's count reaches zero at its last read" — held for counting
and failed for **order**. `If::traverse` generated both branches *before* the
condition, so the condition's read, which runs first, looked like the last
one: the slots went back to the pool and the condition's own temporaries took
them, and the branch that had not run yet then read whatever was left.

Mainnet block **8,716,986** is that bug, in the only place it shows: a receipt.
`SM1FKXGNZ…dlmm-liquidity-router-v-1-2::add-liquidity-multi`
(`af3e472f…b372e6`) is `success` on chain and
`RuntimeFailure(Runtime(DivisionByZero))` in nano, because
`dlmm-core-v-1-1.add-liquidity` guards its division with
`(or (is-eq bin-shares u0) (is-eq bin-liquidity-value u0))` and then divides by
that same `bin-liquidity-value`. Reduced to eight lines and confirmed against
the two engines:

```clarity
(define-read-only (f (x uint)) (let ((d (* u5 u7))) (if (is-eq d u0) u0 (/ x d))))
(f u70)   ;; interpreter u2, compiler DivisionByZero
```

Bisected to this commit: the same call replays clean at `d3731c10` and
diverges at `23196b51`. The condition is generated first now — generation
order is execution order there — and the transaction returns the chain's own
150-element list on both engines. Three crosschecks pin the shape
(`words/conditionals.rs`): a binding read in a condition and a branch, the same
through `or` with mainnet's values, and one read only in a branch.

An audit of every word that builds an instruction sequence found `If` to be the
only inversion; the invariant is stated where the release happens, because a
future word that builds a block before the code preceding it breaks consensus
silently.

## Mainnet-state margin (2026-08-07, measured over real state)

Compiled **every contract in the imported mainnet state** (137,340; checkpoint
tip) under each contract's own stored epoch/Clarity version, with a shared
analysis store and fixpoint passes for trait resolution: **137,332 compile**,
8 fail (see below). Peak live locals per contract, measured by
`locals_report` (A+G codegen; B1 only lowers these further):

| peak live locals | contracts |
|---|---|
| 0–1,000 | 136,796 |
| 1k–5k | 494 |
| 5k–10k | 37 |
| 10k–25k | 5 |
| 25k–50k | 0 |
| >50k | 0 |

**Highest peak on mainnet: 16,505** (`SP3EGAD1…t-a-1`; the next are
`this-13` 16,035, `anxious-peach-leopon` 15,005 — generated/test-looking
contracts; the highest organic ones are the nakamoto-airdrop family at
7,176). Margin to wasmtime's 50,000: **3.0× worst-case, ~7× for organic
contracts, and 99.6% of mainnet sits under 1k.** This is the measured-margin
evidence the acceptance criteria ask for.

The 8 compile failures are NOT locals-related and warrant their own look
(reported to the release-gate task): 3× `Not implemented`
(`amm-swap003`, two `.pool` contracts), 4× duck-typing buffer errors
(`gated-pages*`), 1× `Tuples fields should be typed`
(`trajan-endorsement-alpha`). Some may be harness artifacts (plain
`compile()` differs from nano-vm's `compile_under` in cost-epoch handling);
each needs confirming against the production path before counting as a real
differential.

Method (reproducible): sources+epochs extracted read-only from
`mainnet-tip/state/chainstate/clarity.sqlite` `metadata_table`; per contract
`clar2wasm::compile` under its stored epoch/version; successful analyses
planted back (`insert_contract_hash` + `set_metadata(analysis)` — the
deploy-recipe; a bare `AnalysisDatabase::insert_contract` writes metadata the
read path never finds, since reads key on the contract-hash table). Raw data:
`/tmp/mainnet_margin.out` (per-contract peaks), extraction
`/tmp/mainnet_contracts.jsonl`.

## B2 shipped (2026-08-07, merge 23196b51)

Wide-scope spilling landed: a `let` scope wider than 1,000 bindings keeps
them in the function frame at constant byte offsets (`InnerBindings::Spilled`)
instead of wasm locals. The 26,000-all-read case — the last reachable
locals-based refusal — **peaks at 8 live locals (was 52,006) and wasmtime
loads it**; interpreter and compiler agree. poc2@100 (998) and the 60k-let
(8) unchanged. Normal contracts peak 6–36. Soundness pinned by the full lib
suite at the real threshold AND with the threshold forced to 0 (every `let`
spilled): 1,403/0 each; engine_failure 6/6; clippy clean both crates.

**Locals are no longer a reachable validator limit from source.** Measured
during B2: `MAX_WASM_FUNCTION_SIZE` (128 KiB) is a dead constant in
wasmparser (defined, never enforced); memory is not validator-limited. The
refusal wall now sits at wasmparser's **function-type arity limits**
(`MAX_WASM_FUNCTION_PARAMS`/`RETURNS = 1,000`, reader-enforced): a 600-field
tuple return flattens to 1,200 results, and the interpreter deploys that
source — so the module-load refusal class stays reachable (and
`engine_failure.rs` now forces it with that shape, 6/6 green) but the same
family of conformance gap potentially persists there too: a function whose
flattened parameter or return arity exceeds 1,000 needs its own
mainnet-validity evaluation and, if valid, an ABI-level fix (composite
params/returns through memory instead of flattened slots — the host ABI
constraint recorded in the B inventory). Filed as the follow-up observation
below; out of this task's locals scope.

## Evidence that opened this task

`engine_failure.rs:234` asserts on `too many locals`, reached with a 60,000
binding `let`; [[053-pass-the-mainnet-node-release-gate]] records how that case
was found and why it was the only reachable one. [[060]] records the
`as-contract` prologue's two locals per function and upstream issue #575. No
replayed mainnet block has hit it, which is the reason this is a measurement task
and not an outage — and, under the release rule, not a reason to close it.
