---
id: "116"
group: mainnet
title: "Measure and adapt the marf-read-refactor optimizations"
status: completed
priority: medium
effort: medium
dependencies: ["110", "114"]
tags: ["mainnet", "performance", "marf", "storage", "conformance"]
created_at: 2026-08-11
type: feature
completed_at: 2026-08-12
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
- [x] **Profile nano-marf's read path** on the task-110/114 harness
      (mainnet capture replay on a reflink of `mainnet-pristine`): how much
      wall time is node fetch + decode vs allocation vs SQLite, now that
      the arena and 16 KiB pages are in. Name the top allocation sites in
      the walk (perf or heaptrack; `scripts/bench-phases-report.py` for the
      phase split).
- [x] **Adapt what the numbers justify**, in evidence order — likely the
      inline `NodePath` (kill per-node path `Vec`s), borrowed reads from
      the arena instead of `Arc`-clone-then-clone-node, and reference-taking
      batch insert. A blob/mmap layout is in scope only if the profile
      shows SQLite row fetch itself (not decode or allocation) dominating
      and the branch's own numbers show mmap paying for the layout change.
- [x] **Re-run the replay benchmark** before/after each adopted change and
      record the numbers here; drop anything under noise.
- [x] Record the explicit adopt/reject decision per optimization, with the
      measurement that decided it.

## Result — measured rejection, not a speculative port

Two feature-gated instrumented replays sealed the same 4,149 captured
mainnet blocks through 8,669,750 and accepted every header root. The first
ran in 244.26 s wall; the refined run in 284.65 s under different host I/O
contention. Counts were identical, which is the evidence used below:

- 8,040,108 node-cache hits and 128,558 compulsory misses (98.43% hits).
- SQLite node-row fetch: 25.92–38.20 s; decode: 1.99–2.01 s.
- 626,817 copy-on-write node clones moved 4.105 GB but consumed only 2.653 s.
- 22,158 path allocations held only 629,036 bytes; root clones cost 30.1 ms.
- The node-hash cache answered the whole replay without one SQLite read.

Decisions, one per idea:

1. **mmap/blob layout — reject.** The fork's mmap improves its already-flat
   read path by 20%, but applying that whole gain to nano's measured SQLite
   fetch time saves only about 2–3% of the replay. Replacing the
   consensus-critical SQLite node store with a blob format is not justified
   by that ceiling.
2. **Reusable referential reads — reject.** Nano already reuses `Arc`-backed
   cached nodes at a 98.43% hit rate. The remaining misses are first reads,
   and all measured COW clones together cost under 1% of replay wall time.
3. **Inline `NodePath` — reject.** The entire replay allocated 629 KiB of
   paths. Removing those allocations cannot be measured against run noise.
4. **Scratch patch representation — reject.** Nano has no `TrieNodePatch`;
   its direct COW path is the measured 2.653 s above. Porting the fork's
   470-line scratch machinery would add a second representation to solve no
   observed bottleneck.
5. **Reference-taking batch insert — reject.** Nano exposes individual
   borrowed-key inserts rather than stacks-core's clone-owning `insert_batch`.
   There is no corresponding batch clone to remove, and the whole relevant
   clone path is already below 1%.

No MARF optimization was adopted, so there is no before/after production
variant to bless: the honest outcome is less code. The profiling-only branch
keeps its counters and exact logs; the plan branch keeps only the general
offline-replay fix that resolves a winning commitment through the capture's
existing historical leader-key registry. Both full profiling replays used
fresh distinct-inode reflinks and left the pristine source stamps unchanged.

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
