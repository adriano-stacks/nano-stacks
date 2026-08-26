---
id: "150"
title: "Inherit a widened field's capacity when a composite is constructed"
status: pending
priority: critical
effort: large
dependencies: []
tags: ["mainnet", "vm", "costs", "release"]
created_at: 2026-08-26
type: bug
---

# Inherit a widened field's capacity when a composite is constructed

## Objective

A tuple built in Wasm from a field that carries a *widened* runtime shape is
sized as though the field were as short as its run-time length. The reference
sizes it by the capacity the field was constructed with, so every measurement of
such a tuple — `print` above all — under-charges.

Found while closing [[149]], which it now blocks: with 149's two `filter` fixes
in, mainnet transaction
`8979c764c3503eca8ab58fc8b42d4eb7bb74d456e42f344acaf90017ca694cc2` matches the
canonical record on `read_count`, `read_length`, `write_count` and
`write_length`, and its `runtime` is 375 low. That 375 is this defect and
nothing else.

## The rule being broken

A list value's size is `type_signature_size + max_len × entry.size()`
(`ListTypeData::inner_size`), and a value carries its own `max_len` — so a list
read from storage under a declared `(list 12000 uint)` is sized by 12,000
however few elements it holds. The compiler represents such a value with a
nonzero *runtime-shape handle* naming the arena entry that remembers it, and
`runtime_size`'s tuple arm says so plainly:

> A tuple's first local is its runtime-shape handle. Zero means nothing widened
> this value — widening is a preservation or host crossing, and crossings assign
> handles — so its size is the fixed tuple overhead plus its fields'.

That premise is false for a tuple **constructed from** a widened field.
`TupleCons` pushes a literal handle `0` and captures a shape only when the
source and result types differ or serialization needs a projection:

```rust
builder.i32_const(0);
...
if source_ty != result_ty || generator.type_for_serialization(&source_ty) != source_ty {
    generator.capture_runtime_shape(builder, &source_ty)?;
```

So a tuple whose types already agree keeps handle `0`, the inline sum measures
the widened field by its run-time byte length, and the capacity is gone.

## Measured

Reduced from the mainnet transaction; both engines on the same state, current
cost schedule, contract semantics `Epoch31`/`Clarity3`:

```
charge cost_print [192534] -> rt 407     # interpreter
probe  <print>    [   534] -> rt  32     # compiler
```

192,534 − 534 = 192,000 = 12,000 × 16 — the whole declared capacity of the
`(list 12000 uint)` field, contributed by the interpreter and by nothing on the
compiler's side. `cost_print` is shallow in its input, so the totals differ by
only 375.

The reproduction is a `print` of a tuple holding a list read from a map:

```clarity
(define-map holder uint {items: (list 12000 uint)})
(map-set holder u1 {items: (list )})
(define-public (run)
  (let ((d (unwrap-panic (map-get? holder u1))))
    (begin (print {a: "x", data: {items: (get items d), n: u1}}) (ok u0))))
```

Note that a `print` of the *field itself* agrees, and a `print` of a tuple whose
list field is a fresh literal agrees: it is specifically re-wrapping a widened
value in a newly constructed composite that loses it.

## Tasks

- [x] **`tuple` is fixed.** A constructed composite now inherits its fields'
      widening: if any field carries a handle at run time, the composite is
      captured too. Only fields whose *type* can carry one are considered, so a
      tuple of scalars is skipped at compile time. Regressed two ways — a tuple
      bound in a `let`, and a `fold` that ran zero times and handed its initial
      accumulator back, which is how the mainnet transaction reached it.
- [x] **The audit is done, by measurement.** `some`, `ok` and `merge` are fine:
      `runtime_size` recurses through an optional's or response's inner locals,
      and `merge` already has `merge_runtime_shape`. Three sites are not, and
      each has a *different* reference rule:
      - `as-max-len?` — `special_as_max_len` calls
        `type_signature.reduce_max_len(expected)`, so the result is sized by
        `min(input max_len, expected)`.
      - `append` — `special_append` builds
        `ListTypeData::new_list(next_entry_type, size + 1)`, so the result
        inherits the input's capacity plus one.
      - the `list` constructor — the outer `max_len` is right but the *entry
        type* is not, and capturing the outer value cannot fix it:
        `read_from_wasm` rebuilds inner lists with `cons_list_unsanitized`, so
        the arena read-back path loses inner handles as well.
- [x] **Re-measured `8979c764…`**: with 149's two fixes and `tuple`, the
      transaction charges runtime 3,480,582, read_count 7, read_length 22,193,
      write_count 1, write_length 18 against the real state at 8,832,028 — the
      canonical record exactly, on every dimension, in both engines.
- [ ] **Carry capacity through the three remaining constructors.** Each is
      inventoried as a failing `#[ignore]`d test classed `semantic` against this
      task in `ignored-tests.toml`:
      `as_max_len_keeps_the_capacity_it_reduced`,
      `an_appended_list_keeps_the_capacity_it_grew_from`,
      `a_list_of_a_widened_element_keeps_its_capacity`.
- [ ] **Preserve inner handles on arena read-back**, which is what the `list`
      case needs and what makes this task `large` rather than `medium`: the
      general statement is that run-time capacity has to survive every
      sequence-producing word, not just the ones a mainnet block has hit.

## Scope note

The two sites a mainnet block actually reached — `filter` (in 149) and `tuple`
— are fixed and exact. The three left are legal Clarity that no observed mainnet
transaction has hit, so they do not stop a node; they are cost differentials all
the same and therefore block [[053-pass-the-mainnet-node-release-gate]] under
the plan's *STRENGTHENED — exact receipts and costs* amendment. They are
recorded as open rather than green, which is what that amendment asks for.

## Notes

- This is a cost-only divergence: the values are right, which is why it survived
  1,561 crosscheck tests. It is release-blocking under the plan's
  *STRENGTHENED — exact receipts and costs* amendment all the same.
- The charge trace that isolated it is the technique in
  `/home/aldur/stacks-core-cost-trace`: patch `vendor/clarity-wasm/Cargo.toml`
  in a throwaway worktree, then `NANO_TRACE_CHARGES=1 NANO_TRACE_COSTS=1`, and
  compare the `charge`/`probe` streams either side of the last `probe` line.
- Every clarity-wasm edit moves `COMPILER_IDENTITY`, so land this *before* the
  attestation ceremony and the fresh import that 106 needs — otherwise both are
  paid twice.
