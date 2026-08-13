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
completed_at: 2026-08-13
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

## Branch measurement, 2026-08-12

Isolated clones (`task116-stacks-core{,-base}.20260812`), branch head
`70462db132` against merge-base `6d58b498d`, both built identically on the
pinned Rust 1.96.0 (opt-level 3, thin LTO, one codegen unit, debug 0, one
job, independent targets — a reused target let Cargo mix branch/base clarity
artifacts and was excluded from evidence). The branch's checked-in read
benchmark was unusable as-is (opened a `:memory:` MARF with default options,
looked up a block the generator never wrote, and `MarfConnection::get` does
*different* traversal counts on each side); repaired harnesses generate one
immutable external-blob fixture (128 KiB index + 501,740,804-byte blob,
1,048,576 keys), prepare all `TrieHash`es before the timer, and time exactly
one `get_from_hash` per lookup. One warm-up, five position-balanced
alternating rounds, replay stopped:

| variant | ns/read (median of 5) | vs merge-base |
|---|---|---|
| merge-base pread | 16,026 (15,545–16,521) | 1.00× |
| branch pread (`marf_mmap` off) | 7,489 (7,179–7,892) | **2.14×** |
| branch mmap | 6,262 (6,074–6,413) | **2.56×** (1.20× vs branch pread) |

The refactor itself (reusable read state, `NodePath`, fewer copies) delivers
2.14×; mmap contributes only the last 1.20×. The branch's reduced long-MARF
safety matrix passed 35/35 in 369 s on the branch binary. Run log SHA-256
`a662ea64cde7465dca89e2390c0dba8d5256692884181d69055879c0acf963be`
(`task116-marf-read-runs.20260812.log`; fixture and generation logs kept
beside it).

## nano-marf read profile, 2026-08-12/13

Feature-gated counters (`marf-profile`, this repo) at every read, decode,
allocation and clone site in `nano-marf`, run over the task-110 harness:
4,149 blocks (8,665,602–8,669,750) of `mainnet-capture-long` against a cold
btrfs reflink of `mainnet-pristine`, every block sealing the network's
`state_index_root`, exit 0:

| counter | value | reading |
|---|---|---|
| node cache hits / misses | 8,040,108 / 128,558 | **98.4% of node reads never reach SQLite** |
| SQLite node fetch | 25.9–38.2 s wall, 135.4 MB rows | cold-I/O latency on the 1.6% misses (~200–300 µs each), not CPU |
| node decode | 2.0 s | already small |
| path suffix `Vec`s | 22,158 allocs / 629 KB | ~5 per block — the `NodePath` target is empty |
| children vec allocs | 106,460 / 747 MB | decode-side, inside the 2.0 s |
| node clones (COW/materialize) | 626,817 / 4.1 GB / **2.65 s CPU** | the whole borrowed-reads target |
| root children clones | 4,149 / 59.5 MB / 30 ms | one per block, by design |

Totals against the run: user CPU 174–178 s, wall 4:04–4:45 (instrumented,
shared box). Everything the branch's portable ideas could touch — paths,
clones, decode — sums to **<5 s of CPU**, under the harness's ±10 s noise
band (task 110). The only large number, SQLite fetch, is cold-read latency
on a store the node-cache already shields at 98.4%.

Re-run on current main (2026-08-13, same instrumentation cherry-picked, same
reflink recipe) to confirm the numbers held after the task-115/120/121 VM
work: every deterministic counter byte-identical (same hits, misses, allocs,
clone counts and byte totals), SQLite fetch 39.2 s, decode 2.05 s, node
clones 2.12 s, root clones 26 ms — user CPU now 147.4 s (the VM got faster;
the MARF share did not move), all 4,149 roots sealed, exit 0. Wall 10:20 on
a box concurrently running a hacknet stack; the counters are
load-independent, the wall is not.

## Adopt/reject decisions

| # | branch idea | decision | deciding measurement |
|---|---|---|---|
| 1 | mmap read path / blob layout | **reject** | Fetch cost is cold-I/O on 1.6% of reads, not per-read syscall CPU; task 110 already measured SQLite mmap at replay scale (sys 924 s vs 183 s pread) and reverted it; the branch's own split credits mmap with only 1.20× of its 2.56× |
| 2 | reusable referential read state | **reject — hit path equivalent, miss path measured** | cache hits (98.4%) return `Arc<TrieNode>` copy-free; misses still pay `pread`'s kernel→userspace copy plus decode, but that traffic is bounded by the measured 135.4 MB of rows (tens of ms of memcpy) inside the fetch/decode buckets, and the write-side COW clones cost 2.65 s per 4,149 blocks — all under the ±10 s noise band |
| 3 | inline `NodePath` | **reject** | 22,158 path allocations / 629 KB across the whole replay (~5 per block); a free implementation would be invisible |
| 4 | scratch buffers / patch-depth fields | **reject — not applicable** | nano-marf has no `TrieNodePatch` by design (plan W5); there is no patch path to buffer |
| 5 | reference-taking `insert_batch` | **reject — already satisfied** | `MarfTrie::insert(key: &[u8], MarfValue)` takes the key by reference; `MarfValue` is an inline `[u8; 40]`; no owned-`Vec` batch API exists |
| 6 | bug hazard list | **reviewed, no action** | lost cow-ptr ≙ nano's preserve-nonzero `referenced_block` rule, gated by MARF lockstep; `cur_block` restore ≙ no patch machinery; stale read-only instances ≙ no long-lived read-only MARF (immutable-URI opens are per-question) |

Nothing is adopted, so there is no before/after pair to record: the branch
solves stacks-core's 16 µs/read problem, and nano's arena + `Arc` node cache
already hold the equivalent path at ~4.9 µs *including* cold I/O (40 s of
fetch+decode over 8.17 M reads) — warm hits are pointer loads, and the miss
path's kernel→userspace copies are inside those measured buckets. Removing
the miss-path copy is exactly the mmap trade, priced twice: the branch's own
split values syscall+copy avoidance at ~1.2 µs/read (7,489→6,262 ns), which
over 128,558 misses is ~0.15 s per replay, and task 110's at-scale SQLite
mmap run was net negative. A cold major fault reads the same disk block
either way. The instrumentation ships feature-gated so the profile stays one
command away; the release binary compiles it out.

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
