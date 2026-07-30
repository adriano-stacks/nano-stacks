---
id: "045"
title: "to-ascii? accepts a string the interpreter rejects"
status: completed
priority: high
effort: medium
type: bug
dependencies: []
tags: ["vm", "clarity-wasm", "correctness"]
created_at: 2026-07-30
completed_at: 2026-07-30
---

# `to-ascii?` accepts a string the interpreter rejects

## Objective

`to_ascii::clarity_v4::to_ascii_string_utf8` diverges: for one generated UTF-8
string the compiled path returns

```
Ok(Response { committed: true, data: String("\t\\") })
```

where the interpreter returns `(err u1)`.

Clarity's ASCII rule — `clarity-types/src/types/mod.rs`,
`string_ascii_from_bytes` — admits a byte that is alphanumeric, punctuation or
whitespace. A tab and a backslash are both admissible, so the value the
compiled path produced is *itself* valid; what it cannot be is the conversion
of the input, because the interpreter rejected that input as non-ASCII.

So the compiled path is not merely lenient — it appears to mangle a string
containing characters outside ASCII into a shorter valid one and report
success. That is a **correctness** divergence, not a cost one: a transaction
would take a different branch on nano than on the network.

## What the seed reduces to

`u"\u{0}"` — a single NUL. proptest shrank it that far itself.

The compiled path breaks its loop only on the high bit, so any byte under 128
is accepted, NUL included. That is the fault.

## Which rule applies

Asking the interpreter directly settles it. `to-ascii?` on a one-character
string:

| byte | interpreter |
|---|---|
| NUL `0x00`, vertical tab `0x0b`, DEL `0x7f` | `(err u1)` |
| tab, newline, form feed, return, space, `~`, letters | `ok` |

That is `string_ascii_from_bytes` exactly — alphanumeric, punctuation or
whitespace — and it is neither of the other two candidates. The compiled path
admitted everything under `0x80`, so it took all three rejected bytes. The
tests here admitted `(0x20u8..0x7e)`, which is *stricter*: it rejects a tab the
network accepts, and rejects `0x7e` as well.

So both were wrong, in opposite directions, and the failing case was the two of
them disagreeing rather than either disagreeing with the network.

## The fix

The compiled path now breaks on a byte outside `0x20..=0x7e` unless it is a
tab, newline, form feed or carriage return, and the two proptests assert
`string_ascii_from_bytes`' rule rather than an approximation of it. The table
above is a unit test, so this no longer depends on a gitignored seed.

Worth recording: this fix was written, reverted, and then restored unchanged.
The revert was the right call at the time — it was a permissive change on an
unverified reading, and accepting what the network rejects is the worse
direction to be wrong in. What made it safe was measuring the interpreter
rather than reading it.

## Where it came from

A random case, hit during a full-suite run on 2026-07-30 and persisted by
proptest under `tests/proptest-regressions/to_ascii.txt` — which is gitignored,
so it does not travel with the repository. Verified: the tests pass six of six
with that file moved aside and fail two of six with it in place. The seed line
is

```
cc 209793d50b98eeaad07990d183079a0d022d81c76ea3398cd494089146c5293b
```

It is not caused by the charging work of the same day: both tests fail with
those changes stashed and with `words/` checked out from before them.

## Tasks

- [x] Reduce it to the shortest UTF-8 input that converts differently — a NUL.
- [x] Settle which rule `to-ascii?` applies — measured, not read — and only
      then change the compiled path.
- [x] Keep the reduced case as a unit test, so it does not depend on a
      gitignored seed file.
- [ ] Check the neighbouring conversions — `to-utf8?`, `string-to-int?`,
      `string-to-uint?` — for the same leniency.

## Acceptance Criteria

- `cargo test -p clar2wasm --test wasm-generation` is green with the seed file
  present and with it deleted.
- The reduced input is a checked-in unit test.
