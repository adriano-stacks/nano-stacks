---
id: "019"
group: build
title: "Resolve the clarity-wasm is-eq divergence on contract principals"
status: completed
priority: medium
effort: medium
dependencies: []
tags: ["vm", "clarity", "conformance"]
created_at: 2026-07-29
completed_at: 2026-07-29
---

# Resolve the clarity-wasm is-eq divergence on contract principals

## Objective

`clar2wasm`'s own generation suite intermittently reports that `is-eq` over a
list of contract principals disagrees between the compiler and the interpreter:

```
Compiled and interpreted results diverge!
(is-eq (list 'SH205N8RY76BDEA8Q0VP13GNS70M3CSA42KPRX0MB.A
             'S720QWDM2GQYP70TPDH62VHCCTWZ8Q4RC6YFH8BW3.A))
```

The inputs are generated, so the failure only appears on some runs. Equality on
principals is consensus-visible, so a real divergence would show up as a wrong
receipt or a wrong state root.

## Tasks

- [x] Reproduce it deterministically from the reported inputs.
- [x] Decide which side is wrong against the interpreter's own semantics.
- [x] Fix the vendored compiler and keep the case as a regression test.

## Acceptance Criteria

- The generation suite passes repeatedly.
- A hardcoded case covers equality over contract-principal lists.

## The compiler was wrong, and it did not run at all

The two reported principals name *different* contracts, so the list's item type
is not a single callable — it is `ListUnionType`, a union of callable subtypes.
`serialization_size_runtime` was the one place in the compiler that recognised
only `TypeSignature::PrincipalType`, so sizing an item of that list fell through
to "Mismatched value for type principal" and the whole snippet failed to build:

```
compiled     Err(… Compilation failure … "Type error: Mismatched value for type principal")
interpreted  Ok(Some(Bool(true)))
```

So the divergence was not a wrong answer but no answer, and the interpreter's
`true` is right — equality on two principals of the same shape is decided on the
serialized bytes, which all four principal-like types share. Every other place
the compiler switches on a type already grouped them together; the size
computation now does too.

The fix and the reproducer landed in `3629a7a7`. What is added here is only the
proof that the reproducer is load-bearing: put `CallableType`, `ListUnionType`
and `TraitReferenceType` back on the error arm and
`equal::is_eq_over_a_list_of_contract_principals` fails with exactly the
divergence above, generated inputs or not. The harness also used to *skip* this
class whenever the compiler named the contract in its error, which is why only
the unnamed union case ever surfaced; that skip is gone.

**What it does not prove.** Nothing about the runtime path: the module was never
built, so no comparison of *emitted* principal comparisons happened. It also
says nothing about principals of the same contract, which were always sized
correctly and were never the failing case.
