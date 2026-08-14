---
title: "Carry Clarity values by reference until mutation"
id: "117"
group: mainnet
status: completed
priority: high
effort: large
type: feature
dependencies: ["115"]
tags: ["mainnet", "performance", "vm", "clarity-wasm", "storage"]
created_at: "2026-08-11"
completed_at: 2026-08-11
---

# Carry Clarity values by reference until mutation

## Objective

Most Clarity execution is reads, but a value read from the side store currently
becomes an owned `String`, is cloned through the read cache and Clarity database,
is deserialized into an owned `Value`, and is then copied again across wasm
linear memory. Carry an immutable value as a navigable view over borrowed
backing bytes from SQLite/mapped storage through the Clarity VM and clarity-wasm
host boundary, and materialize owned storage only when the value is actually
mutated, written, or required in wasm linear memory.

The target is not merely `Cow<Value>` after eagerly decoding the value. A
`ValueRef<'a>` should retain the backing byte slice and interpret only the part
an operation asks for: indexing a list returns a view at that element's byte
offset without first building a `Vec<Value>`, and selecting a tuple field or an
optional/response branch likewise returns a borrowed sub-view. With mmap those
bytes are the OS page-cache bytes; with SQLite they are the bytes exposed by the
row. The persisted representation therefore has to be directly navigable, or
carry a deterministic offset/index sidecar built once on write rather than
reconstructed on every read.

This is the end-to-end representation refactor behind task 115's handle-passing
candidate. It must remove physical copies without changing Clarity's
consensus-visible copy costs or value semantics.

## Design Constraints

- Use safe, explicit lifetimes. A `rusqlite::types::ValueRef` obtained with
  `Row::get_ref` cannot outlive its row/statement; consume it under a callback
  or promote it into call-scoped stable storage rather than extending its
  lifetime unsafely.
- Prefer one borrowed/owned abstraction (`Cow`, `Arc` plus `make_mut`, or a
  small `ValueRef`/value handle) across the path instead of a separate wrapper
  at every layer. Its borrowed form retains bytes, not an eagerly constructed
  `Value` tree.
- Make the on-disk representation self-describing and bounds-checkable, with
  direct offsets for variable-width children. If the bytes committed through
  the MARF cannot change, keep their exact hash/preimage and place any offset
  table or zero-copy encoding beside them; storage optimization may not move a
  state root.
- Keep serialized bytes borrowed when the operation only inspects, compares,
  indexes, slices, passes or returns the value. Accessing one child must not
  decode or allocate its siblings. Decode or copy directly into wasm memory
  only when the generated ABI needs the structured value there.
- A mutation materializes once. A write persists the same consensus bytes and
  preserves write ordering, sanitization and rollback behavior.
- Physical allocation is an implementation detail: `read_length`, runtime copy
  charges, receipts, events and state roots do not move.

## Tasks

- [x] Instrument the current path and count allocations, clones and bytes at
      SQLite extraction, the side-value cache, `BackingStore`/`ClarityDatabase`,
      `Value` decoding and host-to-wasm marshalling for task 114's pox-5 and
      meme-token reads.
- [x] Price the maximum plausible win against task 115's full-corpus phase
      timings before changing the storage representation or public VM APIs.
- [x] Specify the safe implementation boundary far enough to make the retain/
      remove decision: a SQLite borrow must remain inside a row callback or be
      promoted into call-scoped stable storage, and direct child navigation
      needs a deterministic offset sidecar beside the unchanged MARF preimage.
- [x] Reject the representation rewrite after measurement: the whole current
      value read/write path is about 1 ms/call, while the change would span the
      SQLite lifetime, cache, Clarity value and Wasm ABI boundaries and would
      still retain the final linear-memory copy.
- [x] Remove the instrumentation spike and temporary mainnet harness so no
      production overhead, new representation or compiler-identity change is
      retained.
- [x] Preserve the existing consensus implementation and its already-green task
      115 corpus, scoreboard, receipt, both-engine and strict-Clippy gates.

## Measured decision: keep owned Clarity values

The env-gated spike at commit `18dd489f` counted physical traffic around two
successful captured mainnet calls executed on a private reflink of the retained
state with each block's recorded Bitcoin context:

| boundary | pox-5 `stake` operations / bytes | loto `ri` operations / bytes |
|---|---:|---:|
| SQLite string extraction | 13 / 636 | 483 / 36,429 |
| cache clone | 252 / 11,352 | 32,678 / 1,336,750 |
| backing-store value return | 252 / 11,352 | 32,678 / 1,336,750 |
| value decode | 37 / 2,092 | 23,920 / 436,590 |
| full `Value` clone | 3 / 965 | 1,500 / 7,356,432 |
| write into Wasm | 59 / 1,634 | 60,628 / 2,559,828 |

The traffic is real, especially in the deliberately heavy loto call. It is not
the dominant wall-time seam. Task 115's 321-call attribution measured all DB
host buckets at 0.80 ms/call and the combined value read/write path at roughly
1 ms/call, versus 14.4 ms/call after the retained parse-cache fixes. Its full
4,149-block corpus already reached 1.52x aggregate speedup, with loto at 1.43x.
Even eliminating every counted owned copy would therefore cap the expected win
near 7%; the unavoidable Wasm-memory writes make the real ceiling smaller.

Obtaining that ceiling would require a new directly navigable persisted view or
sidecar, callback-bound SQLite lifetimes, cache/database API changes, lazy
Clarity children and a second borrowed/owned contract at the Wasm host boundary.
That risk is disproportionate to the measured ceiling. The spike and its
temporary two-call harness were removed. The retained implementation, storage
bytes, roots, receipts and compiler identity are unchanged.
## Reconciled against the post-120/121 engine, 2026-08-13

The rejection was priced against task 115's 1.52× state; the engine has since
moved and the arithmetic only hardens:

- Task 120 (2.06×, wasm 15.6 s vs interpreter 32.0 s on the same corpus)
  consumed the largest counted traffic rows directly: `runtime_shape_size(handle)`
  ended the whole-value writes into linear memory per size measurement (the
  bulk of loto's 60,628 wasm-write operations), and arena-memoized sizes ended
  the repeated decodes behind them. Its residual list keeps only "per-op
  metadata clones in the var/map closures" from this path.
- Task 121's vendor adoption left cost vectors and receipts byte-identical
  (scoreboard 340/340 costs, 500/500 digests), so 120's attribution stands.
- The remaining ceiling, taken at the old ≲1 ms/call bound it no longer fills:
  2,564 corpus calls × 1 ms ≈ 2.6 s per 4,149-block replay — under the
  harness's ±10 s noise band (task 110's shipping rule), on a range current
  main replays at 147 s user CPU (task 116's confirmation run, 2026-08-13,
  all roots sealed). The largest residual byte mover, the copy into linear
  memory where the ABI consumes the value, is the one this refactor cannot
  remove.

Decision reaffirmed on today's HEAD: keep owned Clarity values; reopen only
with a new attribution showing the value path material relative to run
variance.

## Acceptance Criteria

- The current ownership path is measured on representative captured calls at
  every named boundary rather than judged from source inspection.
- The measured traffic is reconciled with full-corpus wall-time attribution;
  a representation rewrite is retained only if its plausible ceiling is
  material relative to run variance and its lifetime/storage risk.
- If rejected, the instrumentation and prototype are removed and the measured
  reason is retained in the task record rather than leaving speculative code.
- No borrowed SQLite memory or unsafe lifetime extension is introduced, and the
  canonical storage bytes, roots, receipts, costs, events and writes remain
  unchanged.

## Context

- Task 115 owns the measured clarity-wasm performance target and first
  attribution; this task owns the larger ownership/lifetime refactor if those
  measurements continue to justify it.
- `crates/nano-vm/src/lib.rs::data_from_side_store` currently returns an owned
  `String` and clones cache hits/inserts.
- `vendor/clarity-wasm/clar2wasm/src/linker.rs` is the final host boundary where
  values are decoded and written into wasm linear memory.
