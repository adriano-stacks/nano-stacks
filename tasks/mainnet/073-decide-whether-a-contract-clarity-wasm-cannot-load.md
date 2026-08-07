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

- [ ] Measure the boundary rather than assume it: find the smallest contract, in
      each shape that produces locals (`let` width, `let` nesting, `match`/`try!`
      bindings, function count × prologue locals), that clar2wasm compiles and
      wasmtime refuses. Report the count per shape, not one number.
- [x] Establish whether stacks-core accepts those same sources — analyzer limits,
      read-length and the deploy cost budget — so the answer distinguishes "the
      network would reject this too" from "the network accepts what nano cannot
      load". **Answered: the network accepts what nano cannot load** — see
      findings below.
- [ ] Search the imported mainnet state for contracts near the boundary. The
      checkpoint carries every deployed contract, so this is a measurement over
      real state, not a guess: report the highest local count any mainnet contract
      reaches and its margin.
- [ ] Make the `as-contract` prologue pay for itself: emit the save/restore only
      for functions whose body contains an `as-contract`, and re-measure. This is
      worth doing whatever the answer above is, because it is a cost every mainnet
      contract currently carries.
- [ ] If a mainnet-valid contract cannot load, fix code generation so it does —
      no interpreter fallback, no healing path, no per-contract exception. Reuse
      of locals across disjoint scopes is the obvious lever and belongs upstream.
- [ ] Keep the manufactured module-load refusal in `engine_failure.rs` working
      whatever the fix is. A gate exercisable only while a divergence is open
      stops working the moment somebody closes one, and that gate is about the
      *boundary*, not about this bug.
- [ ] Record the outcome in the release report either way: a measured margin is
      evidence, "no mainnet block has hit it" is not.

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

## Evidence that opened this task

`engine_failure.rs:234` asserts on `too many locals`, reached with a 60,000
binding `let`; [[053-pass-the-mainnet-node-release-gate]] records how that case
was found and why it was the only reachable one. [[060]] records the
`as-contract` prologue's two locals per function and upstream issue #575. No
replayed mainnet block has hit it, which is the reason this is a measurement task
and not an outage — and, under the release rule, not a reason to close it.
