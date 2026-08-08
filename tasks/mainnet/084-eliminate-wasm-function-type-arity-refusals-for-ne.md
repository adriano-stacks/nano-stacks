---
title: "Eliminate WASM function type arity refusals for network valid contracts"
id: "084"
status: in-progress
priority: critical
effort: large
type: bug
group: mainnet
dependencies: ["067"]
tags: ["mainnet", "vm", "clarity", "wasm", "conformance", "release"]
created_at: "2026-08-07"
---

# Eliminate WASM function type arity refusals for network valid contracts

## Objective

Task 073 removed the reachable WebAssembly locals limit, then moved the manufactured
module-load refusal to a 600-field tuple return. Clarity-wasm flattens that tuple to
1,200 WebAssembly results and wasmparser enforces a 1,000-result function-type
limit, while the reference interpreter deploys the same source. Establish whether
the source is network-valid and eliminate every arity refusal that the network can
accept. Keeping a failure-path test must not depend on retaining a real conformance
bug.

## Tasks

- [x] Measure the smallest source that exceeds WebAssembly parameter and result
      arity separately, including nested tuples, optionals, responses, public
      functions, read-only functions and cross-contract calls.
- [ ] Establish network validity with the pinned stacks-core analyzer, source and
      transaction size limits, deploy costs and an actual stock-node deployment;
      interpreter acceptance alone is not sufficient release evidence.
- [ ] Measure the maximum flattened parameter/result arity across every contract in
      the imported mainnet state and account for every contract the sweep cannot
      compile under task 073. **The second half is done — [[093]] classified all
      eight, and seven now load. The first half had a hole: 073's sweep called
      `clar2wasm::compile` and never handed the result to wasmtime, and arity is a
      *validator* limit, so a module exceeding it compiles cleanly and fails to
      load. That sweep could not have seen one. `cargo xtask sweep-contracts`
      now has the measurement path — it runs `Vm::inspect_module`, the
      report-preserving form of `compile_under` followed by `loadable`, over every
      contract in a state, read-only and in one process. **This remains open until
      that new inventory is run over the full imported state and every named but
      unmeasurable entry is accounted for.**
- [x] If a network-valid source reaches the limit, change the clarity-wasm ABI to
      pass oversized composites through linear memory or another vetted bounded
      representation instead of flattened WebAssembly slots.
- [x] Differentially test returned values, costs, events, writes and contract-call
      ABI behavior on both sides of the former boundary.
- [x] Preserve compile, module-load, host and runtime failure-path coverage with
      faults that are not valid Clarity programs the production node is required
      to execute.
- [ ] Add the measured boundary and verdict to the release report, and make any
      unresolved network-valid refusal fail it.

## Acceptance Criteria

- No source that stacks-core accepts for the target chain is refused because its
  flattened WebAssembly function type exceeds an engine limit.
- The full mainnet contract inventory has a reproducible arity measurement and no
  unclassified compile failure.
- The production fix contains no interpreter fallback, contract exception or
  raised engine limit that merely moves the same reachable wall.
- Engine-failure tests still exercise every refusal class without preserving a
  known semantic or ABI differential.

## Evidence that opened this task

Task 073 records a 600-field tuple whose 1,200 flattened results exceed
wasmparser's `MAX_WASM_FUNCTION_RETURNS = 1,000`; clarity-wasm compiles it, the
interpreter deploys it and Wasmtime refuses the module. The task calls this a
follow-up observation while also claiming that no stacks-core-accepted source is
refused, so the question needs its own release-blocking task.

## The old sweep could not have seen an arity refusal, 2026-08-08

Worth stating plainly because it changes what task 073's "137,332 compile" means
for *this* task: that sweep compiled and did not load. The arity limit is
wasmparser's, so it is enforced when a module is handed to the engine, not when
`clar2wasm::compile` returns. A mainnet contract flattening past 1,000 results
would have counted as compiling.

`cargo xtask sweep-contracts <state-dir>` is the measurement that can see one. It
takes the production path — `Vm::check_module` is `compile_under` then
`loadable` — over every contract the state's `metadata_table` names, groups the
refusals by reason rather than listing them one per line, and exits non-zero if
any contract refuses. Read-only, through task 087's opener, so it can be pointed
at an operator's state without writing to it.

It is slow: 137,340 contracts compiled *and* validated is over half an hour, which
is why the number had not been measured this way before.

## Measured over the whole state, 2026-08-08 — and the wall is reachable

`cargo xtask sweep-contracts /home/aldur/mainnet-8716986/state`, which compiles
**and loads** every contract:

```
146273/146280 contracts compile and load (146346 named by the state)
```

Seven refuse. One is [[068]]'s (`trajan-endorsement-alpha`). **The other six
compile to a module wasmtime will not load**, which is the class this task is
about and which no previous sweep could see — 073's called `clar2wasm::compile`
and never handed the result to an engine, and every one of these compiles fine.

Two of the six are the arity limit itself:

| contract | refusal |
|---|---|
| `SPXGT7ADNZNARR4SVSJN56QGSZFHGATJEQMFPMJW.pox-4` | `function returns size is out of bounds (at offset 0x651)` |
| `SP33WGC0P4HRB395QXWKEWAP27SH47F9CDQX6FXW2.mix-sender-dope` | `function params size is out of bounds (at offset 0x123)` |

So the arity wall is not a manufactured case: two deployed contracts reach it and
the network accepted both.

**They are not boot contracts, and an earlier note here said one was.**
`SPXGT7ADNZNARR4SVSJN56QGSZFHGATJEQMFPMJW.pox-4` is a user contract that shares
the name; the boot `SP000000000000000000002Q6VF78.pox-4` and `.pox-5` were
checked afterwards and both compile and load. Reading a boot contract out of a
name was the mistake, and it briefly turned a real conformance bug into a
foundational emergency it is not.

What both actually are is aggregators: four lines, a `define-read-only` returning
one 55-field tuple built from a dozen cross-contract calls — a "give me
everything" view function. That is the shape to reduce from, and it is a shape
the network will keep producing.

The remaining four are a **different defect and probably not this task's**:

| contract | refusal |
|---|---|
| `SP3XR2EN9C51B09MJE7EF73Q3GR4HXY1Z28KR4QY8.STX` | `type mismatch: expected i64 but nothing on stack (at offset 0x2550)` |
| `SP673Z4BPB4R73359K9HE55F2X91V5BJTN5SXZ5T.xip130` | `type mismatch: expected i64, found i32 (at offset 0x4058)` |
| `SP34FHX44NK9KZ8KJC08WR2NHP8NEGFTTT7MTH7XD.citycoins-vote-v1` | `type mismatch: expected i32, found i64 (at offset 0x9c4b)` |
| `SP1KK89R86W73SJE6RQNQPRDM471008S9JY4FQA62.treasury-grant-v4` | `(at-block ...) is not available in this epoch` — analysis, not load; **[[066]]**, and worth confirming its recorded epoch is right |

`type mismatch: expected i64, found i32` is the *exact* symptom `be3ec64e`
describes: a wasm-local use-count that frees a slot still read, so the stack
carries the wrong thing. `expected i64 but nothing on stack` is the same family
one step worse. That commit fixed one instance (`keepgoing-safe`, and
`v0-5-market` with it — see [[086]]); these three are further instances or a
sibling defect, and they belong with that work rather than with arity.

## The reachable boundary is lowered through memory, 2026-08-08

Commit `72f980ab` keeps the engine's 1,000-slot limit and changes the ABI rather
than raising it. Function parameters/results, wide control results and the
top-level return use a memory-backed representation only when their flattened
source type exceeds the boundary. `ArityReport` records the pre-lowering widths:
maximum function parameters/results, control parameters/results and top-level
results.

The committed boundary tests cover exactly 1,000 and 1,001 flattened slots across
tuple, optional and response values, read-only and public functions, top-level
results and cross-contract parameters/results. The differential exercises values,
costs, events, committed and rolled-back writes, and the cross-contract ABI on
both sides. The engine-failure fixture no longer keeps a valid over-wide Clarity
program broken: malformed Wasm reaches the production module-load boundary, while
separate fixtures retain compile, host and runtime failure coverage.

The inventory bridge keeps the report beside the result of the same compilation;
it does not compile each contract twice and does not copy the 1,000-slot constant.
`sweep-contracts` prints all five numeric maxima, every exact contract whose raw
arity crosses the boundary, and whether that module loaded. A contract which
cannot be parsed, sourced or assigned its recorded deploy epoch is now
`UNMEASURED` and makes the verdict fail instead of disappearing from the
denominator. `release-report --state` consumes the same verdict and fails for an
unmeasured or refusing contract. No full-state run of the new measurement, actual
stock-node deployment, or release qualification has been claimed yet; those are
why the corresponding checklist items remain open.

### One caution, recorded because it cost a whole run

The first sweep compiled every contract as epoch 4.0 and reported **878**
contracts refusing `at-block`. That is the epoch-4.0 rule applied to contracts
that were never under it: `ensure_wasm_module` compiles under the epoch the chain
*records*, because it decides which words exist. `sweep-contracts` reads the
recorded epoch now (`Vm::recorded_deploy_epoch`). A sweep that assumes an epoch
measures its own assumption.
