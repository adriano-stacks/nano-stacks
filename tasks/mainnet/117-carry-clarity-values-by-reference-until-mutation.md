---
title: "Carry Clarity values by reference until mutation"
id: "117"
group: mainnet
status: pending
priority: high
effort: large
type: feature
dependencies: ["115"]
tags: ["mainnet", "performance", "vm", "clarity-wasm", "storage"]
created_at: "2026-08-11"
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

- [ ] Instrument the current path and count allocations, clones and bytes at
      SQLite extraction, the side-value cache, `BackingStore`/`ClarityDatabase`,
      `Value` decoding and host-to-wasm marshalling for task 114's pox-5 and
      meme-token reads.
- [ ] Specify the borrowed storage encoding and `ValueRef<'a>` API. Cover tags,
      lengths, direct list-element offsets, tuple-field lookup, optional/response
      branches and sequence slices; state how canonical MARF bytes and the
      zero-copy/indexed representation coexist without changing roots.
- [ ] Introduce the smallest safe borrowed/owned value representation and make
      side-store reads use `Row::get_ref` (or the equivalent blob API) without
      first constructing an owned `String`/`Vec<u8>`.
- [ ] Carry that representation through the Clarity database APIs and read
      cache; remove eager clones and whole-value deserialization on read-only
      paths, including nested collection access.
- [ ] Teach clarity-wasm host functions to consume the borrowed bytes/handle
      directly, copying once into linear memory only where the wasm ABI requires
      it and retaining a handle when a value round-trips without inspection.
- [ ] Materialize on mutation/write, with tests covering nested tuples, lists,
      optionals/responses, large buffers/strings, cached reads, rollback and a
      read followed by a write to the same key.
- [ ] Re-run task 114's engine corpus and record before/after allocation counts,
      bytes copied, aggregate engine time and the pox-5/meme-token rows.
- [ ] Run workspace clippy, nano-vm/nano-chainstate suites, the 340-block
      scoreboard, frozen receipt digests and `NANO_REPLAY_BOTH_ENGINES` replay.

## Acceptance Criteria

- A representative read-only data-var and map read creates no owned
  `String`/`Vec<u8>` or cloned `Value` between the SQLite/cache source and its
  final consumer; the only unavoidable copy is into wasm linear memory when the
  ABI actually consumes the value. A regression test or explicit counters prove
  the path rather than relying on source inspection.
- Indexing an element in a variable-width list and selecting a nested tuple
  field return borrowed sub-views, perform no heap allocation, and do not visit
  or deserialize unrelated elements. Tests assert byte ranges and allocation/
  decode counters on values large enough to expose an accidental full walk.
- Mutating a borrowed value promotes it exactly once, and unchanged values can
  be returned or written back without deserialize/clone/serialize churn.
- No borrowed SQLite memory escapes the row/statement lifetime, no unsafe
  lifetime extension is introduced, and nested/re-entrant reads remain valid.
- The task 114 corpus records a reduction in allocations and bytes copied and
  the corresponding timing change. An optimization below run variance is
  removed or documented rather than retained on intuition.
- Receipts, execution costs, events, writes and state roots remain byte-exact;
  both engines report zero disagreements and all Clarity/VM tests and workspace
  clippy pass without warnings.

## Context

- Task 115 owns the measured clarity-wasm performance target and first
  attribution; this task owns the larger ownership/lifetime refactor if those
  measurements continue to justify it.
- `crates/nano-vm/src/lib.rs::data_from_side_store` currently returns an owned
  `String` and clones cache hits/inserts.
- `vendor/clarity-wasm/clar2wasm/src/linker.rs` is the final host boundary where
  values are decoded and written into wasm linear memory.
