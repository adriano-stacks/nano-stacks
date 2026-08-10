---
title: "Pin the untested arms of the stack-layout measurement rule"
id: "112"
status: completed
priority: high
type: improvement
tags: ["mainnet", "wasm", "conformance", "costs"]
created_at: "2026-08-10"
parent: 111
effort: small
completed_at: 2026-08-10
---

# Pin the untested arms of the stack-layout measurement rule

## Objective

`43eefade` made `value_type_before_context` return the type describing the
layout a traversal actually leaves on the stack, for all three producers a
call-site measurement can see: a widened `let` binding, a duck-typed constant
and a duck-typed user-function result. Only the binding arm has a focused
regression (`local_call_widens_a_nested_none_argument`, from the block
8,733,929 stall). The constants and call-result arms changed behavior on the
same reasoning — the old measurement was misaligned wherever the conversion
fired — but no test pins them, and by this repo's standard an untested
consensus-adjacent rule is not a closed one.

The composition constraint is part of what the tests must hold in place:
`traverse_call_user_defined` now duck-types each argument from `value_ty` to
the parameter signature after measuring it, and that line is only correct if
`value_ty` names the on-stack layout in every arm. Narrowing the rule back to
bindings alone would silently re-break the other two.

## Tasks

- [x] Add a crosscheck regression for a duck-typed **constant** argument: a
      `define-constant` whose analysed type needs duck-typing to the parameter
      type of a local call (e.g. a constant tuple with a `none` field passed
      where the field is `(optional uint)`), asserting result equality and,
      via the crosscheck harness, cost equality with the interpreter.
- [x] Add the same for a duck-typed **user-function result** argument: an
      inner call whose declared return type needs duck-typing to the outer
      call's parameter type.
- [x] Make both tests fail against the pre-`43eefade` measurement (module
      refused or size misread) so they are pinned to the rule, not to
      incidental codegen.
- [x] Sweep the imported mainnet state (`cargo xtask sweep-contracts`) after
      the tests land, confirming no contract regressed to a module-load
      refusal.

## Acceptance Criteria

- Both new crosschecks are green in the clar2wasm suite and red when the
  measurement rule is reverted to the producer's own type.
- The full-state sweep reports no new refusals.

## Context

- Rule and rationale: `vendor/clarity-wasm/clar2wasm/src/wasm_generator.rs`,
  `value_type_before_context` doc comment.
- Original stall and binding-arm regression: task 111, mainnet block
  8,733,929, `SPMPMA1V6P430M8C91QS1G9XJ95S59JS1TZFZ4Q4.fastpool-max500-signer-manager`.

## Evidence

- Commit `b129274b` adds the constant and user-function-result crosschecks.
  `cargo test -p clar2wasm --lib
  wasm_generator::tests::local_call_widens_a_ -- --nocapture
  --test-threads=1` passes all three producer cases, and strict all-target
  Clippy passes.
- With only `value_type_before_context` temporarily restored to its
  pre-`43eefade` implementation, both new exact tests fail at module load:
  the constant case at Wasm offset 6,969 and the function-result case at
  offset 10,117, each with `type mismatch: expected i32, found i64`. The
  production rule was restored and the three-test slice rerun green before
  committing.
- The read-only full-state sweep at compiler
  `sha256:06f5a88eda5d3b81f9f5216014d8955020e6396e4296d62dabed4bee89dde5bd`
  and state tip `63f2beda310a00e9c790f9f8e2e41f42f6f145f034e6cac040cd9bd46746c2b6`
  reports 137,284/137,284 current-tip contracts compile and load, from
  137,342 distinct candidates/146,346 metadata rows. It excludes 58 proven
  stale metadata candidates and reports zero unmeasured contracts. The log is
  retained at `/tmp/task112-sweep-b129274b.log`.
