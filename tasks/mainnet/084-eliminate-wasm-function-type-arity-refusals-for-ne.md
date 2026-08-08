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

- [ ] Measure the smallest source that exceeds WebAssembly parameter and result
      arity separately, including nested tuples, optionals, responses, public
      functions, read-only functions and cross-contract calls.
- [ ] Establish network validity with the pinned stacks-core analyzer, source and
      transaction size limits, deploy costs and an actual stock-node deployment;
      interpreter acceptance alone is not sufficient release evidence.
- [~] Measure the maximum flattened parameter/result arity across every contract in
      the imported mainnet state and account for every contract the sweep cannot
      compile under task 073. **The second half is done — [[093]] classified all
      eight, and seven now load. The first half had a hole: 073's sweep called
      `clar2wasm::compile` and never handed the result to wasmtime, and arity is a
      *validator* limit, so a module exceeding it compiles cleanly and fails to
      load. That sweep could not have seen one. `cargo xtask sweep-contracts`
      closes the hole — it runs `Vm::check_module`, which is `compile_under`
      followed by `loadable`, over every contract in a state, read-only and in one
      process.**
- [ ] If a network-valid source reaches the limit, change the clarity-wasm ABI to
      pass oversized composites through linear memory or another vetted bounded
      representation instead of flattened WebAssembly slots.
- [ ] Differentially test returned values, costs, events, writes and contract-call
      ABI behavior on both sides of the former boundary.
- [ ] Preserve compile, module-load, host and runtime failure-path coverage with
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
