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
- [ ] Measure the maximum flattened parameter/result arity across every contract in
      the imported mainnet state and account for every contract the sweep cannot
      compile under task 073.
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
