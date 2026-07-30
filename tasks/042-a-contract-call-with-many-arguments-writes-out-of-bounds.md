---
id: "042"
title: "A contract call with many arguments writes out of bounds"
status: completed
priority: high
effort: medium
type: bug
dependencies: []
tags: ["vm", "clarity-wasm", "correctness"]
created_at: 2026-07-30
completed_at: 2026-07-30
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

With the *generated* value — a short list — that call passes unpadded, which
made it look like only the harness was wrong. It is not: with a full-size value
for the same declared type, it fails with no padding at all.

## What it was

A called contract is not sized for the arguments it is given.

The host writes them into the callee's memory before entering it, and nothing
reserved that room. `set_memory_pages` sizes a module once, at compile time, as
`literal_memory_end + frame_size + max_work_space`, and a function definition
never added its parameters to `frame_size` — so the callee compiled to a single
page with **64,060 bytes free whatever its arguments were**, and ran on
whatever the page round-up happened to spare.

A 20x17 list fits in that. A 30x17 one is 67,568 bytes and does not. That is
the entire difference between working and writing out of bounds, and it is why
the generated case looked like it was about having many arguments: it was about
their total size.

The reduction misattributed it twice before landing. It is not the harness —
the same call fails unpadded once the value is full-size, which the first
reduction missed because the generated value was small. And it is not
`ContractCall`'s argument region, which is right: `write_to_memory` stores a
sequence as its eight-byte offset and length, so eight bytes an argument is all
that region needs. The caller was the correctly-sized module all along, at
71,912 bytes over two pages; the callee was the broken one.

## The fix

`traverse_define_function` adds each parameter's `get_type_in_memory_size` to
`frame_size`, for public and read-only functions — the ones a call from outside
can enter. Private functions are unaffected: their arguments arrive on the
wasm stack, not through memory.

Two regression tests pin it, one either side of the old threshold, plus two
that show building the same list — at the top level and in a function — was
always fine. Only being handed one was not.

## Tasks

- [x] Reduce the generated contract to the smallest call that fails.
- [x] Establish whether the call is wrong or the harness is.
- [x] Decide whether the module reserves enough for a nested list argument —
      it did not, and the callee was the module at fault.
- [x] Fix it, and keep the case as unit tests rather than a generated one.

## Acceptance Criteria

- `cargo test -p clar2wasm --test wasm-generation` is green.
- The single-argument case is a unit test.
- Whether the compiler reserves enough memory for a nested list argument is
  answered in the code, not left to the harness.
- The replay still matches, so the change moves no consensus behaviour.
