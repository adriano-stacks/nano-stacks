---
id: "068"
title: "Resolve asymmetric tuple least-supertype semantics"
status: pending
priority: critical
effort: large
dependencies: []
tags: ["mainnet", "vm", "clarity", "conformance"]
created_at: 2026-08-06
type: bug
---

# Resolve asymmetric tuple least-supertype semantics

## Objective

Resolve the known asymmetric `least_supertype` differential instead of waiving
it because no replayed mainnet block has reached it. The reference engine can
return different tuple widths from the same statically analysed expression,
depending on the branch taken and operand order; clarity-wasm currently exposes
the narrowed static layout. The whole returned value is consensus-visible when
it crosses a contract-call or receipt boundary even if later `get` operations
observe only common fields.

## Tasks

- [ ] Minimize both operand orders and both taken branches, asserting the whole
      value rather than only fields common to the inferred type.
- [ ] Carry the minimized value through a public function, a contract call and
      a transaction receipt to identify every ABI boundary that can expose the
      mismatch.
- [ ] Determine whether the conformant change belongs in the Clarity analyser,
      value sanitization or clarity-wasm's runtime representation, using the
      pinned stacks-core revision as the oracle.
- [ ] Implement the fix in the shared Clarity/clarity-wasm boundary without a
      special case for the captured expression and without interpreter
      execution in the node.
- [ ] Add differential coverage for nested tuples, optionals and responses whose
      branches union to different tuple shapes.
- [ ] Remove the ignored differential and verify the exact returned value,
      receipt serialization, costs and writes in both engines.

## Acceptance Criteria

- The minimized asymmetric cases return byte-identical values in clarity-wasm
  and the reference engine for both branch directions and operand orders.
- Values crossing contract-call and receipt boundaries are represented or
  rejected exactly as stacks-core represents or rejects them.
- No test hides the mismatch by reading only common tuple fields, and no ignore
  remains for the known case.
- The production fix contains no interpreter fallback, healing path or
  expression-specific mainnet exception.

## Evidence that opened this task

[[060-make-the-consensus-execution-engine-explicit-and-r]] records the minimized
case and explains why the existing compiler layout cannot close it locally. A
recent commit description said the least-supertype issue was fixed, but the
actual differential remains ignored. This task, not the commit description, is
the release accounting for that gap.
