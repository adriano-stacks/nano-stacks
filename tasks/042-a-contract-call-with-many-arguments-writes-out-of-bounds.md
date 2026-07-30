---
id: "042"
title: "A contract call with many arguments writes out of bounds"
status: pending
priority: medium
effort: medium
type: bug
dependencies: []
tags: ["vm", "clarity-wasm", "test-harness"]
created_at: 2026-07-30
---

# A contract call with many arguments writes out of bounds

## Objective

`contract_call_no_oom_many_arg` fails in the vendored compiler. A generated
contract call returns

```
Err(Internal(InvariantViolation("UnableToWriteMemory(out of bounds memory access)")))
```

where the interpreter returns the value. It is deterministic — three
consecutive runs fail identically in a third of a second, with no persisted
proptest seed — and it predates the cost work: it reproduces at `32ebf319`,
and the test file is unchanged since it was vendored.

## What it reduces to

Delta-debugging the nine generated arguments, regenerating values from a
deterministic runner, removes eight of them. **One argument fails on its own:**

```clarity
(list 30 (list 17 (string-utf8 31)))
```

**The call itself is fine.** With the same argument and value, and without the
test's padding, it passes. It fails only inside `as_oom_check_snippet`.

## Why the harness cannot express this case

`as_oom_check_snippet` prefixes a buffer sized to leave exactly the room the
arguments need, so that a call writing one byte too many fails. Two things
break for an argument this large.

`get_type_in_memory_size` makes that argument `8 + 30 * (8 + 17 * 132)` =
**67,568 bytes**, more than a 64 KiB page, and the helper takes at most one
extra page. Making it take pages until they fit does not help either, because
the target is unreachable: measured, the most free space the module ever has is
**64,167 bytes**, against the 69,080 the helper is trying to leave. So the
padding lands wherever it lands — 3,544 bytes free — and the call overruns.

`set_memory_pages` sizes memory once, at compile time, as `literal_memory_end +
frame_size + max_work_space`. There is no runtime growth, so how much room a
call has is fixed when it is compiled.

## The open question

The call succeeds with 64,167 bytes free and wants 67,568 by the helper's
reckoning, so the two numbers do not mean the same thing, and which of them is
right is what decides whether there is a defect underneath. If `max_work_space`
genuinely covers the arguments then only the harness is wrong; if it can
under-reserve for a nested list, then a large enough contract could overrun in
production, where nothing would report it as clearly as this test does.

This is **not** currently evidence of a consensus bug: a contract call with this
argument executes correctly.

## Tasks

- [x] Reduce the generated contract to the smallest call that fails.
- [x] Establish whether the call is wrong or the harness is.
- [ ] Decide whether `max_work_space` reserves enough for a nested list
      argument — compare what `set_memory_pages` reserves against what the
      argument actually writes.
- [ ] Fix whichever is wrong, and keep the reduced single-argument case as a
      unit test rather than a generated one.
- [ ] If it is the harness, make it refuse a case it cannot set up instead of
      silently leaving the wrong amount of space.

## Acceptance Criteria

- `cargo test -p clar2wasm --test wasm-generation` is green.
- The single-argument case is a unit test.
- Whether the compiler reserves enough memory for a nested list argument is
  answered in the code, not left to the harness.
