---
id: "152"
title: "Store Clarity values packed instead of as hex text"
status: pending
priority: high
effort: large
dependencies: ["053"]
tags: ["mainnet", "vm", "marf", "storage", "performance", "upstream"]
created_at: 2026-08-27
type: improvement
---

# Store Clarity values packed instead of as hex text

## Objective

Our Clarity value side store is `data_table (key TEXT PRIMARY KEY, value TEXT
NOT NULL)` (`crates/nano-vm/src/lib.rs:3486`) and the value is a *hex string* of
the consensus serialization. That is the layout stacks-core is currently
replacing, and on our own disks it is the second-largest thing a node holds.

Evaluate and then adopt a packed binary side-store payload in nano-owned code,
using `cylewitruk-stacks/stacks-core` branch `perf/clarity-binary-value-storage`
as the design reference and its test corpus as a dev-only oracle.

## What the reference is

Branch head `e4768ee0` (2026-08-24), 11 commits ahead of `main`, +10,072/-388
across 56 files. It separates cleanly into layers, which is how we should take
it:

| Layer | Where | Size |
|---|---|---|
| packed value codec | `clarity-types/src/types/codec/packed/{encode,decode,shape,directory,layout,primitive,reconstruct}.rs` | ~2,700 |
| Binary V1 side store | `stackslib/src/clarity_vm/database/binary_value_store{,/schema,/metadata,/migration}.rs` | ~3,700 |
| offline migrator + docs | `contrib/clarity-value-storage-migrate/`, `docs/clarity-side-store-migration.md` | ~420 |
| codec tests and fuzzing | `clarity-types/src/tests/types/packed_codec.rs`, `clarity/fuzz/fuzz_targets/packed_value_codec.rs`, seed corpus | ~1,200 |

The codec-only slice is upstream as **draft PR #7534** (opened 2026-08-26, still
moving as of 2026-08-27, base `main`, +4,676/-20, part of
`stx-labs/core-epics#425`). Storage integration and migration are stated as
later PRs. So the design is being reviewed in public but nothing here is merged
or stable yet.

**Their measured result:** a mainnet migration produced a 38.56 GiB destination
from a 147.72 GiB source, about 3.8x. Codec verification claims golden vectors,
cross-schema property tests and 258,529,405 clean fuzz executions.

## Why it matters here

Measured on this machine, 2026-08-27:

| State | `clarity.sqlite` | `marf.sqlite` |
|---|---|---|
| `mainnet-tip/state-88920833` (at tip) | 48 GB | 84 GB |
| `release-subject-05aaf07c` (mid-replay, 8.7M) | 20 GB | 33 GB |

So the side store is roughly a third of a node's state, and disk is already a
live constraint — a fresh import costs about 100 GB and long imports are guarded
by a disk floor for that reason. A 3.8x cut to that column is the largest
single-line disk win available to us, and it is the *only* one that does not
touch consensus bytes.

There is also a stated long-term reason to understand the format: the PR says a
future hard fork could make this the consensus serialization to reduce
`read_length` costs. If that ever happens our engine has to implement it, so
reading the grammar now is cheap insurance either way.

## Two wins, separable — do them in this order

1. **Hex text to `BLOB`.** Roughly 2x on the value column and on the key, needs
   no format grammar, no shape descriptors and no reconstruction argument. This
   is a storage-encoding change to `write_value`/`data_from_side_store` and the
   import path, nothing more. Land and measure it on its own.
2. **The packed payload.** The remaining reduction, and the part that needs a
   format, a shape descriptor and an exactness proof.

Do not bundle them. If step 1 delivers most of the disk win on our data, step 2
may not be worth its risk before other work.

## Design points worth taking regardless of how far we go

- The record begins with the **equivalent consensus byte length** as a
  little-endian `u32`, so read-length cost accounting never has to decode the
  body. We charge `read_length` from the stored string today; keep that property
  or costs get slower, not just different.
- Packed bytes depend **only on the active `Value`** — never declared bounds,
  never the epoch. That is what keeps the store content-addressable.
- `ValueShape` records *only* what packing omits (tuple field names, active
  optional/response/list shape), interned in a dictionary table with an optional
  `value_shape_id` column, so the hot read path (`GET_TYPED`) skips the join
  entirely and only the schema-free read (`GET_GENERIC`) pays for it.
- `AdmittedValue` is a move-only type whose constructor performs the epoch-aware
  admission check once; the encoder accepts only that type, so the check cannot
  be repeated or skipped. Good pattern for our own admission boundary.

## The consensus trap, which is the whole risk

`MARFValue[0..32]` is `Sha512_256` over the **consensus serialization**. A
packed store is only safe if it reconstructs those bytes *exactly*, including
representations that are not canonical. We have four fixes from this month in
precisely that area — `7bba9859`, `4e015023`, `ca982902`, `b434743d` — and the
reference's own fuzz seed corpus is named `noncanonical-consensus`,
`unsanitized-list-elements` and `unsanitized-list-elements-independent-{a,b}`,
which says they hit the same wall.

A reconstruction that tidies bytes up changes `MARFValue`, which changes the
state root, which is a silent consensus fork. Sanitization is the specific case
to attack first.

## Constraints

- Both trees are GPL-3.0-only, so licensing does not block a port. But
  `plan.md` forbids stacks-core in the production dependency graph, so this is a
  nano-owned implementation informed by their design, with their vectors and
  corpus used only in `nano-conformance`.
- Their work is an unmerged draft on a fork. Do not pin a dev-dependency to a
  fork branch; lift vectors as data.
- **Sequencing:** this rewrites the on-disk state format. It invalidates
  in-flight replays, and because the checkpoint bundle carries the value side
  store, it moves the bundle's content root even though `COMPILER_IDENTITY` —
  which covers only `vendor/clarity-wasm` — does not change. Hence the
  dependency on 053: not before the release gate. Decide explicitly whether
  import transcodes a legacy bundle or whether bundles get re-issued.

## Tasks

- [ ] Measure first: what fraction of `clarity.sqlite` is `data_table` value
      bytes, versus keys, metadata, ledger rows and index overhead? Their 3.8x
      is a claim about their schema, not a prediction about ours.
- [ ] Land the hex-to-`BLOB` change alone, on a throwaway copy, and re-measure.
- [ ] Read the codec grammar (`layout.rs`, `shape.rs`, `directory.rs`) and write
      our own specification of what we would store, in our own words, before any
      code.
- [ ] Decide the read boundary. `MarfStore::get` returns `Option<String>` today;
      if it keeps returning hex, a packed store re-serializes on every read and
      the win evaporates. Name the new return type first.
- [ ] Differential test: for every value in a captured mainnet slice,
      pack then reconstruct and require byte-identical consensus bytes and an
      identical `MARFValue`.
- [ ] Fuzz the reconstruction with their seed cases ported as data, sanitization
      and non-canonical encodings first.
- [ ] Replay a fixture slice on the new store and require identical state roots
      and receipts at every block.
- [ ] Decide and implement the import story for existing bundles and states, or
      state plainly that they must be re-imported.
- [ ] Re-measure the disk and the read path, and record both numbers against the
      before figures in this task.

## Acceptance Criteria

- A measured reduction on our own data, reported next to the before numbers, not
  an inherited 3.8x.
- Byte-identical consensus reconstruction for every value in the test corpus,
  including non-canonical and unsanitized encodings, with `MARFValue` equality
  asserted rather than assumed.
- A replay slice produces identical state roots and receipts to the hex store.
- No stacks-core code or dependency in the production graph; oracles live in
  `nano-conformance` only.
- The read path is no slower, and `read_length` accounting still costs nothing
  to obtain.
