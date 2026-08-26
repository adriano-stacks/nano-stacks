---
id: "150"
title: "Inherit a widened field's capacity when a composite is constructed"
status: completed
priority: critical
effort: large
dependencies: []
tags: ["mainnet", "vm", "costs", "release"]
created_at: 2026-08-26
completed_at: 2026-08-26
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
- [x] **The audit is done, by measurement — and it had to be done twice.** The
      first pass wrote itself up as complete while never measuring `concat`,
      which its own task list named. Both it and `replace-at?` were wrong:
      - `concat` — `SequenceData::concat` on lists is `ListData::append`, whose
        result is `new_list(entry, self.max_len + other.max_len)`: the *sum* of
        the arguments' capacities, pairwise for more than two. Fixed by
        inheriting the first argument's shape and adding the rest as a number,
        through a `runtime_shape_list_capacity` host call.
      - `replace-at?` — two things. `SequenceData::replace_at` writes one slot
        and leaves `type_signature` alone, so the result inherits the input's
        capacity like `filter` does; *and* `special_replace_at` charges
        `TypeSignature::type_of(&seq).size()`, the input value's size, where the
        compiler charged its element count.
      `err` and `slice?` were measured as well and are already right, which is
      now asserted rather than assumed.
- [x] **The rest of the audit.** `some`, `ok` and `merge` are fine:
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
- [x] **`as-max-len?` and `append` are fixed.** One generalised host call
      covers all three inheriting words, because they differ only in how they
      adjust the capacity they inherit: `filter` keeps it, `append` adds one
      (`special_append`: `new_list(next_entry_type, size + 1)`), and
      `as-max-len?` reduces it (`special_as_max_len`:
      `type_signature.reduce_max_len(expected)` — a ceiling on the inherited
      value, not a replacement for it). Each has a regression that fails without
      its own fix.
- [x] **The `list` constructor was never wrong, and the read-back was never the
      problem.** Both were premises this task opened with, and measuring
      contradicted both: `read_from_wasm` reads each element through
      `read_from_wasm_indirect`, which honours the element's own handle, so
      inner capacity does survive the arena. A capture at `ListCons` was written
      and then **reverted**, because no test could be made to fail without it.
      `a_list_of_a_widened_element_is_measured_at_its_declared_width` asserts
      the property instead.
- [x] **`element-at?` is fixed, and it is the one place the reference
      *narrows*.** `list_cons` builds its result with `Value::cons_list` — the
      sanitizing constructor — so each element is rebuilt against the derived
      entry type and any capacity it was not using is dropped: an empty
      `(list 12000 uint)` element is stored as `(list 0 NoType)`, which is why
      the reference charges `print [6]` and not 192,006. The compiler kept the
      element's shape handle when writing it into the list, so extraction
      returned the widened value and *over*-charged — the direction that refuses
      a block the network accepted. Zeroing the handle slot on the stored
      element is that narrowing exactly, because a handle-zero list is measured
      inline from its own byte length. The reference says two things at once —
      the list is measured at its elements' declared width, and an element read
      back out is only as big as what it holds — and one handle slot cannot say
      both. `ListCons` says them **in order**: capture the list while its
      elements still carry their handles, which fixes the entry type in the
      arena, and only then narrow what is left in memory for whoever extracts an
      element. Narrowing only *list* elements, because a handle on a response or
      an optional also carries what a `NoType` branch cannot represent inline
      and dropping it there loses the value rather than its width — the first
      attempt did, and `map_principal_destruct` failed with
      `InvalidNoTypeInValue`. The *charge* is untouched and has to be:
      `list_cons` charges the sum of `a.size()` over the elements as they
      arrived, before sanitization.

## Scope note

Eight sites are fixed and regressed: `filter` twice (in 149), `tuple`, `append`,
`as-max-len?`, `element-at?`, `concat` and `replace-at?`. `some`, `ok`, `merge`, `err`, `slice?` and the
`list` constructor were measured and found already correct. `ignored-tests.toml` now
lists **no** `semantic` and no `unclassified` entry, which is the state the
plan's *STRENGTHENED — exact receipts and costs* amendment asks for.

Re-verified after the last fix: `8979c764…` charges runtime 3,480,582,
read_count 7, read_length 22,193, write_count 1 and write_length 18 against the
real state at 8,832,028 in both engines, and the canonical receipt replay of
block 8,832,029 is green.

Two of this task's opening premises were wrong, and measuring is what said so:
the `list` constructor was not losing capacity, and the arena read-back was not
dropping inner handles. A capture written for `ListCons` on that premise was
reverted because no test could be made to fail without it.

The family has one rule with two exceptions, and all three are now stated by a
test: a word that hands its argument back keeps the capacity that argument
carried (`filter` and `replace-at?` unchanged, `append` plus one, `as-max-len?`
reduced, `concat` summed), while `list_cons` *sanitizes* and therefore narrows
what it stores. `replace-at?` also has to be *charged* over that capacity, not
over the element count, which is a separate thing from carrying it.

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
