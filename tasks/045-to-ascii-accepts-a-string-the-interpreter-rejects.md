---
id: "045"
title: "to-ascii? accepts a string the interpreter rejects"
status: pending
priority: high
effort: medium
type: bug
dependencies: []
tags: ["vm", "clarity-wasm", "correctness"]
created_at: 2026-07-30
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

## Where it came from

A random case, hit during a full-suite run on 2026-07-30 and persisted by
proptest under `tests/proptest-regressions/to_ascii.txt` — which is gitignored,
so it does not travel with the repository. It reproduces on demand once that
file exists and disappears with it, which is worth knowing before concluding it
is fixed.

It is not caused by the charging work of the same day: it reproduces with those
changes stashed and with `words/` checked out from before them.

## Tasks

- [ ] Reduce it to the shortest UTF-8 input that converts differently.
- [ ] Decide which byte classes the compiled path is admitting or dropping, and
      make it apply `string_ascii_from_bytes`' rule.
- [ ] Keep the reduced case as a unit test, so it does not depend on a
      gitignored seed file.
- [ ] Check the neighbouring conversions — `to-utf8?`, `string-to-int?`,
      `string-to-uint?` — for the same leniency.

## Acceptance Criteria

- `cargo test -p clar2wasm --test wasm-generation` is green with the seed file
  present and with it deleted.
- The reduced input is a checked-in unit test.
