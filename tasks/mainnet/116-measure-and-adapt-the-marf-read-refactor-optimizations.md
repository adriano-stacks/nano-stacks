---
id: "116"
group: mainnet
title: "Measure and adapt the marf-read-refactor optimizations"
status: in-progress
priority: medium
effort: medium
dependencies: ["110", "114"]
tags: ["mainnet", "performance", "marf", "storage", "conformance"]
created_at: 2026-08-11
type: feature
---

# Measure and adapt the marf-read-refactor optimizations

## Objective

[cylewitruk-stacks/stacks-core `perf/marf-read-refactor`](https://github.com/cylewitruk-stacks/stacks-core/tree/perf/marf-read-refactor/stackslib/src/chainstate/stacks/index)
rebuilds stacks-core's MARF read path for speed. Task 110 already made
nano-marf reads the second-biggest replay cost (the ancestry walk was ~170k
serial point reads before the 16 KiB pages + arena work); if this branch's
ideas are worth what its author thinks, some belong in nano-marf. Measure
the branch on stacks-core's side, profile where nano-marf's remaining read
time goes, and port only what the numbers justify — the MARF is
consensus-critical, so every adopted change must leave roots bit-exact.

## What the branch does (surveyed 2026-08-11, head `70462db132`)

27 commits ahead of upstream `main`, confined to
`stackslib/src/chainstate/stacks/index` plus a new `contrib/marf-inspect`
CLI. Author's changelog: *"Improve MARF read performance with optional
memory-mapped trie blob access and reusable referential read state,
controlled by the new `node.marf_mmap` configuration option (enabled by
default)."* Pieces:

1. **mmap read path over the trie blob file**, with improved `pread` logic
   as the fallback (`TrieFile`), gated by `node.marf_mmap`. Avoids a
   syscall + copy per node read.
2. **Reusable referential read state** — lookups borrow from mapped/cached
   storage instead of reallocating per walk.
3. **`NodePath` type** replacing `Vec<u8>` for node paths — paths are ≤32
   bytes, so an inline array kills an allocation per node touched.
4. **Scratch buffers** (`scratch.rs`, ~470 lines) in patch handling, and
   `TrieNodePatch` replaced by `patch_depth`/`last_patch_source` fields on
   the node structs.
5. **`insert_batch` generic over key/value references** — callers stop
   cloning keys and values into the batch.
6. **Bug fixes found en route** (lost cow-ptr, `make_node_patch` failing to
   restore `cur_block`, read-only instances never refreshing the MARF) —
   worth reading as a hazard list even if we adopt nothing.

Prior mapping to nano-marf: (1) and (2) don't port literally — nano-marf
stores nodes in SQLite rows, not a flat blob, so "mmap the blob" has no
direct analogue; the transferable idea is zero-copy/borrowed reads. (3) and
(5) map directly — `crates/nano-marf/src/lib.rs` allocates `Vec<u8>` path
suffixes and clones nodes throughout the walk/insert paths. (4) is
stacks-core-specific: nano skips `TrieNodePatch` by design (plan W5).

## Tasks

- [x] **Measure the branch itself**: build the branch and its upstream
      `main` merge-base, run its own `marf_perfs`/long marf tests (the
      branch has "committing for benchmark runs" commits and a reduced test
      matrix for exactly this) on the same machine, and record the read-path
      speedup it actually delivers, mmap on and off. This bounds what any
      port can be worth.
- [ ] **Profile nano-marf's read path** on the task-110/114 harness
      (mainnet capture replay on a reflink of `mainnet-pristine`): how much
      wall time is node fetch + decode vs allocation vs SQLite, now that
      the arena and 16 KiB pages are in. Name the top allocation sites in
      the walk (perf or heaptrack; `scripts/bench-phases-report.py` for the
      phase split).
- [ ] **Adapt what the numbers justify**, in evidence order — likely the
      inline `NodePath` (kill per-node path `Vec`s), borrowed reads from
      the arena instead of `Arc`-clone-then-clone-node, and reference-taking
      batch insert. A blob/mmap layout is in scope only if the profile
      shows SQLite row fetch itself (not decode or allocation) dominating
      and the branch's own numbers show mmap paying for the layout change.
- [ ] **Re-run the replay benchmark** before/after each adopted change and
      record the numbers here; drop anything under noise.
- [ ] Record the explicit adopt/reject decision per optimization, with the
      measurement that decided it.

## Consensus safety

Read-path changes must not change a single hash. Every adopted change keeps
green: MARF lockstep vs stacks-core's own MARF, the node byte vectors, PCS
import, `write_journal`, nano-marf/nano-vm unit suites, and offline replay
sealing the network's `state_index_root` at the task-110 frontier. The
branch is a fork in flight, not merged upstream — treat it as a source of
measured ideas, not vendorable code; anything we take is reimplemented
against nano-marf's storage model.

## Acceptance Criteria

- The branch's own speedup on stacks-core is measured and recorded (mmap on
  vs off vs merge-base), not taken on faith.
- A profile names where nano-marf read time goes today, with allocation
  sites in the walk quantified.
- Each of the five optimization ideas has a recorded adopt/reject decision
  tied to a measurement.
- Adopted changes show a before/after replay number in this file and leave
  MARF lockstep, PCS import and the offline replay state roots bit-exact.
