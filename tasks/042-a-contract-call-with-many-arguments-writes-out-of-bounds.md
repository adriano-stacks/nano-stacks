---
id: "042"
title: "A contract call with many arguments writes out of bounds"
status: pending
priority: high
effort: medium
type: bug
dependencies: []
tags: ["vm", "clarity-wasm", "correctness"]
created_at: 2026-07-30
---

# A contract call with many arguments writes out of bounds

## Objective

`contract_call_no_oom_many_arg` fails in the vendored compiler. A generated
contract call whose arguments are many and large returns

```
Err(Internal(InvariantViolation("UnableToWriteMemory(out of bounds memory access)")))
```

where the interpreter returns the value. The failing case passes a ten-field
tuple of nested lists, UTF-8 strings, principals and buffers.

This is a **correctness** divergence, not a cost one: a transaction the network
executes successfully would fail on nano, so nano would compute a different
chain.

It is not caused by the cost work. It reproduces at `32ebf319`, before any of
the charging fixes, and the test file is unchanged since it was vendored. It
does not reproduce on the pristine upstream source either, because that predates
the rebase and no longer compiles — so the cause lies somewhere in the epoch-4.0
rebase itself ([[021-rebase-clarity-wasm-onto-epoch-4-0]]).

It is deterministic: three consecutive runs fail identically, in a third of a
second, with no persisted proptest seed.

## Tasks

- [ ] Reduce the generated contract to the smallest call that writes out of
      bounds.
- [ ] Find which argument shape overflows — the nesting, the total size, or the
      count.
- [ ] Fix it, and keep the reduced case as a regression test.
- [ ] Check whether the replay fixtures contain a call of that shape; if they
      do not, say so, because 340 green blocks do not cover this.

## Acceptance Criteria

- `cargo test -p clar2wasm --test wasm-generation` is green.
- The reduced case is a unit test, not only a generated one.
