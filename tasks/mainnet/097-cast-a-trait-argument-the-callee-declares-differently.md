---
id: "097"
group: mainnet
title: "Cast a trait argument the callee declares differently"
status: completed
priority: critical
effort: small
dependencies: ["060"]
tags: ["mainnet", "vm", "clarity-wasm", "consensus", "release"]
created_at: 2026-08-09
completed_at: 2026-08-09
type: bug
---

# Cast a trait argument the callee declares differently

## Objective

A contract-call whose argument carries a trait tag the callee does not declare is
refused, where the interpreter re-tags it and runs. The refusal is consensus:
the transaction is kept with a `vm_error` nobody else recorded, its cost is the
cost of a refusal rather than of the call, and the state root parts.

## Evidence

Mainnet block 8724865, reached only once
[[096-cross-a-stacks-fork-inside-one-sortition-chain]] let this node off the
branch it was stranded on:

```
state root mismatch at height 8724865: tenure start false, 3 transactions, 3 receipts,
  Bitcoin height 961651, tenure height 252263
  receipt 24d63204…c061558 RuntimeFailure("RuntimeCheck(TypeValueError(
    SequenceType(ListType(ListTypeData { max_len: 100, entry_type: TupleType({
      "asset": <SP2VCQ….ft-trait>, "lp-token": <SP2VCQ….ft-trait>, "oracle": <SP2VCQ….oracle-trait>}) })),
    "…Tuple({"asset": <SP2VCQ….ft-trait>, "lp-token": <SP2VCQ….ft-mint-trait>, …})…")")
  committed false returned (err none) cost { runtime: 10526781, read_count: 218, … }
executing the peer's chain failed: state root mismatch:
  expected 4d8de1d7b3917b2be4fdc93c9ebdc54caa081e10f4025b4de9f3219dd29287ee,
  got     f4cb97cd8c4a1d97e4473c9a59a54e5aca69b38eae25fd6d34c46c19039da77e
```

The callee declares `lp-token: <ft-trait>`; the value arrived tagged
`<ft-mint-trait>`.

## Cause

`implicit_contract_cast` (`clar2wasm/src/initialize.rs`) implemented half of
`clarity2_implicit_cast` (`clarity/src/vm/callables.rs:431`). The interpreter's
own comment states the whole of it — "implicitly cast principals to traits **and
traits to other traits** as needed" — and it carries two callable arms:

| value | interpreter | nano, before |
|---|---|---|
| `Principal(Contract)` | tag with the declared trait | tag with the declared trait |
| `CallableContract(other trait)` | **re-tag** with the declared trait | left untouched → `admits` refuses |

Two smaller divergences in the same function, found by reading the two side by
side:

- the list arm derived its signature from the cast elements
  (`cons_list_unsanitized` — least supertype, actual length) instead of carrying
  the declared entry type over the value's `max_len`;
- the tuple arm passed a field the callee's type does not name straight through,
  where the interpreter refuses the value with `TypeValueError`.

## Acceptance

- `implicit_contract_cast` mirrors `clarity2_implicit_cast` arm for arm.
- The cast is asserted directly, the way the interpreter asserts its own
  (`test_implicit_cast`), on the mainnet shape — a trait tag on a field of a
  tuple inside a list. A snippet crosscheck cannot reach it: `admits` is the only
  thing downstream that tells the two tags apart, and once a value is past it
  both engines agree.
- The refusal path still costs what the interpreter charges
  (`a_wider_tuple_is_refused_the_way_the_interpreter_refuses_it`) — returning the
  cast's error early skipped `charge_refused_application` and halved it.
- Block 8724865 executes to the state root the network published. **Not done**:
  the argument now gets past `admits` and fails deeper, in
  [[098-read-a-trait-reference-back-out-of-a-nested-value]].
