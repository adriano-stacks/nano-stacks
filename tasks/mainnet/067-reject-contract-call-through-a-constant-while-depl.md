---
id: "067"
title: "Reject contract-call through a constant while deploying"
status: completed
priority: critical
effort: medium
dependencies: []
tags: ["mainnet", "vm", "clarity", "conformance"]
created_at: 2026-08-06
type: bug
---

# Reject contract-call through a constant while deploying

## Objective

Make a contract call through a constant during top-level deployment produce the
same result as stacks-core. The minimized differential is:

```clarity
(define-constant target .callee)
(contract-call? target foo)
```

At epoch 4.0 the compiler currently accepts and executes this deployment while
the reference engine rejects it with
`RuntimeCheckErrorKind::ContractCallExpectName`. A failed deployment writes no
contract state, so accepting it is an immediate state-root divergence. This is
not an interpreter-fallback case: the production fix belongs in clarity-wasm
and its host boundary.

## Tasks

- [x] Preserve the current ignored differential as a minimal fixture, including
      the callee, the deploying contract and the exact executing epoch.
      `words::contract::tests::constant_call_targets`, at a named epoch and
      version rather than an inherited one: two of the three conditions *are* the
      epoch and the version, so a default hides which one answered.
- [x] Record stacks-core's error identity, receipt, cost dimensions and writes
      for the failed deployment instead of inferring them from error text.
      `crosscheck_multi_contract_with_env` compares the whole
      `Result<Option<Value>, VmExecutionError>` and the event batches, per
      contract, against the interpreter running the same deploy.
- [x] Expose the reference engine's `contract_context.is_deploying` distinction
      to the compiled path and reject a constant target before the callee or
      arguments can cause effects.
      **Amended:** *after* the arguments, not before. `special_contract_call`
      evaluates every argument — charging costs that land in the failing
      transaction's receipt — and only then asks whether the atom names a
      dispatchable target. Rejecting earlier would have made the receipt cheaper
      than the chain's.
- [x] Map `ContractCallExpectName` without turning it into `WasmError::Expect`
      or another block-rejecting internal error.
- [x] Crosscheck direct named calls, constant calls during deployment and the
      same constant call after deployment so the fix is not wider than the
      reference rule.
- [x] Remove the ignore from
      `a_constant_contract_call_while_deploying_agrees` and include it in the
      clarity-wasm and workspace conformance gates.

## What the original measurement missed

The ignored test ran at `TestEnvironment::default()`, which is Epoch 3.3 /
Clarity 4 — and `supports_call_with_constant()` is false before Epoch 3.4. So the
reference was refusing that case for the *epoch*, not for the deploy, and the
measurement recorded in this task named the wrong one of the three conditions.

Worse, the crosscheck harness never raised `is_deploying` at all: it calls
`eval_all` directly rather than through `Contract::initialize_from_ast`, so the
oracle dispatched a constant call the chain refuses. It brackets `eval_all` now,
which is what makes the deploying half measurable in either engine.

All three conditions are checked in the host, at the executing epoch, and only on
the branch that resolved its target through a `define-constant`: a literal
`.callee` and a `let`- or parameter-bound callable reach other branches of the
word, and the reference leaves those ungated.

## Acceptance Criteria

- Both engines return the same `ContractCallExpectName` failure with identical
  costs and no committed deployment state.
- A direct contract-name call during deployment and a supported constant call
  outside deployment retain the reference behavior.
- The production node rejects the candidate through clarity-wasm alone; it
  never retries, heals or executes it with the interpreter.
- No ignored differential remains for this case, and the release report names
  the regression test that closes it.

## Evidence that opened this task

The exhaustive runtime-predicate audit in
[[066-refuse-at-block-at-run-time-as-epoch-4-0-does]] found the missing
`!contract_context.is_deploying` half of stacks-core's
`supports_call_with_constant()` check. The same audit found that the current
error clone mangles `ContractCallExpectName`. Keeping this separate from
`at-block` prevents a completed epoch-gate fix from hiding a different state
root bug.
