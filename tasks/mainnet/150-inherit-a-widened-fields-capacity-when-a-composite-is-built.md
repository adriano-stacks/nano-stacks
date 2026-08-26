---
id: "150"
title: "Inherit a widened field's capacity when a composite is constructed"
status: pending
priority: critical
effort: medium
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

- [ ] Make a constructed composite inherit its fields' widening: if any field
      carries a nonzero handle at run time, the composite has to be captured
      too. The condition is a run-time OR over the fields' handle slots, which
      is the same shape 149's `capture_filtered_runtime_shape` uses for
      `filter`.
- [ ] Audit every other site that builds a composite out of parts, and cover the
      ones that can carry a widened part: `tuple`, the `list` constructor,
      `append`, `concat`, `as-max-len?`, `merge` (which already has
      `merge_runtime_shape` — confirm it is enough), and the `ok`/`some`/`err`
      wrappers (whose `runtime_size` already recurses through the inner locals,
      so these are expected to be fine — assert it rather than assume it).
- [ ] Keep the arena out of the common case, as 149 does: capture only when a
      field is actually widened, so a tuple of scalars still costs nothing.
- [ ] Regress each covered site with `crosscheck_cost`, and check each new test
      fails without the fix.
- [ ] Re-measure `8979c764…` and close 149's five-dimension criterion with it.

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
