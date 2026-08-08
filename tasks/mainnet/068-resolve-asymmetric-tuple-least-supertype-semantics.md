---
id: "068"
title: "Resolve asymmetric tuple least-supertype semantics"
status: in-progress
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

- [x] Minimize both operand orders and both taken branches, asserting the whole
      value rather than only fields common to the inferred type.
      `wasm_response_fold::the_wider_operand_first_is_refused_before_it_runs` and
      `a_narrowed_default_parts_from_the_reference_only_where_it_must`.
- [x] Carry the minimized value through a public function, a contract call and
      a transaction receipt to identify every ABI boundary that can expose the
      mismatch. `a_contract_call_does_not_normalise_the_narrowed_value`.
- [x] Determine whether the conformant change belongs in the Clarity analyser,
      value sanitization or clarity-wasm's runtime representation, using the
      pinned stacks-core revision as the oracle. See *Where it belongs* below.
- [ ] Implement the fix in the shared Clarity/clarity-wasm boundary without a
      special case for the captured expression and without interpreter
      execution in the node. **Blocked on the finding below: no static layout can
      be conformant.** What is needed is a value representation that carries its
      shape at run time, which is a clar2wasm architecture change and is not a
      choice inside this word.
- [x] Add differential coverage for nested tuples, optionals and responses whose
      branches union to different tuple shapes.
      `every_narrowing_kind_parts_on_the_some_branch_only`.
- [x] Remove the ignored differential and verify the exact returned value,
      receipt serialization, costs and writes in both engines.
      The ignore is gone. What replaced it is not an equality: it pins what *each*
      engine answers, in both branch directions, so the divergence cannot move in
      either engine without turning the suite red — including if it moves in the
      reference, which is the half that decides the chain.

## Where it belongs

`least_supertype_v2_1`'s tuple arm (`clarity-types/src/types/signatures.rs`)
walks the **first** operand's fields, raises `TypeMismatch` on one the second does
not have, and silently drops the ones the second has and the first does not. That
settles the operand-order half: with the wider operand first the contract does not
deploy, in either engine, so only one direction can reach a value at all.

In that direction the analysed type is the narrow one and the reference's
`native_default_to` hands back whichever branch produced the value, unconverted.
So the *same statically analysed expression* returns a two-field tuple on the
`some` branch and a one-field tuple on the `none` branch — measured, byte for
byte, in `the_reference_answer_here_has_no_single_static_layout`.

That is not a choice clar2wasm gets to make:

- **Narrowing** (what it does) reproduces the `none` branch and drops a field on
  the `some` branch.
- **Widening** reproduces the `some` branch and would have to invent a field on
  the other.
- **The analyser** cannot fix it either. Widening `least_supertype` makes the
  `none` branch narrower than the analysed type instead — the reference converts
  neither, so it is shape-dynamic in whichever direction the type is chosen.
- **Sanitization** does not reach it. `special_contract_call` sanitizes its result
  against `type_of(&result)` — the value's *own* type — so field dropping is a
  no-op there and the wide tuple crosses a `contract-call?` intact. Asserted.

So the conformant engine for this case is one whose values carry their shape at
run time, which is what the interpreter is and what clar2wasm is not. Reproducing
it needs a shape discriminant propagated to every consumer of such a value — the
reference's own runtime refuses the wide tuple at two of the five escapes
(`set_variable` and `clarity2_implicit_cast`, the latter under the comment "This
should be unreachable if the type-checker has already run successfully"), so the
value that must be reproduced is one stacks-core itself calls impossible.

## Release accounting

This is a **measured, mainnet-unreached divergence with a named cause**, not an
ignored test. `blacklist-susdh-v1` — the contract that produced the shape — reads
every one of its `default-to`s through `get`, and no shape found on the chain so
far depends on which value comes back. The dangerous escape is `is-eq`, which is
control flow and therefore state: a contract branching on the comparison would
take different branches in the two engines. Nothing on the chain does yet.

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

## A mainnet contract reaches this, 2026-08-08

Until now this task had minimized cases and no deployed one.
`SP1J70VWT7MRRP635NZ6E3J86PFE78JFXS0QR5ZAH.trajan-endorsement-alpha` is a
deployed contract that clarity-wasm cannot compile at all because of it — found
by task 073's sweep over every contract in the imported mainnet state, and traced
here by [[093-load-the-eight-mainnet-contracts-clarity-wasm-refuses]].

It appends a seven-field tuple to a list whose declared element type has five
(lines 263 and 49). The analyser narrows the literal to the list's element type
and `words/tuples.rs` is then asked to build the narrow shape out of a wider
literal, which it refuses:

```
Tuples fields should be typed: the literal names `profile-sender`, and the
  analysed type holds ["date-event", "date-sent", "endorsement",
                       "endorsementURI", "title"]
```

That is this task's *refusal* direction rather than its silent-divergence one, so
it is the safe half — but it means the outstanding architecture change now has a
deployed contract behind it and not only a reduction. The network accepted this
contract; nano cannot load it.
