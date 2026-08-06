---
id: "067"
title: "Reject contract-call through a constant while deploying"
status: pending
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

- [ ] Preserve the current ignored differential as a minimal fixture, including
      the callee, the deploying contract and the exact executing epoch.
- [ ] Record stacks-core's error identity, receipt, cost dimensions and writes
      for the failed deployment instead of inferring them from error text.
- [ ] Expose the reference engine's `contract_context.is_deploying` distinction
      to the compiled path and reject a constant target before the callee or
      arguments can cause effects.
- [ ] Map `ContractCallExpectName` without turning it into `WasmError::Expect`
      or another block-rejecting internal error.
- [ ] Crosscheck direct named calls, constant calls during deployment and the
      same constant call after deployment so the fix is not wider than the
      reference rule.
- [ ] Remove the ignore from
      `a_constant_contract_call_while_deploying_agrees` and include it in the
      clarity-wasm and workspace conformance gates.

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
