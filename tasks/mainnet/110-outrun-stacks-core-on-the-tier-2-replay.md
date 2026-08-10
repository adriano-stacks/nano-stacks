---
id: "110"
group: mainnet
title: "Outrun stacks-core on the tier-2 replay"
status: in-progress
priority: medium
effort: medium
dependencies: ["109"]
tags: ["mainnet", "performance", "marf", "vm", "storage"]
created_at: 2026-08-10
type: chore
---

# Outrun stacks-core on the tier-2 replay

## Objective

Task 109's tier-2 replay had nano at 24.1 s against stacks-core's 20.8 s for
the same 99 mainnet blocks. Find where the time goes and remove it, without
touching any consensus surface.

## Where the time went, measured

- fsync: ~1.2 s (eatmydata differential) — not the lever.
- Clarity side-store reads: ~1.5 s (page-cache warming differential).
- Contract compilation: 3.4 s of CPU on first touch of 33 contracts —
  amortized by the native-module disk cache, which a fresh checkpoint import
  doesn't carry; warming it into the anchor state saved the CPU but almost no
  wall clock.
- **The rest was the MARF ancestry walk**: ~170k serial 4 KiB point reads at
  ~80 µs a miss, on a disk that does 2 GB/s sequentially. Latency-bound
  pointer chasing, and every `pread` a syscall even on page-cache hits. A
  detail that hid this earlier: btrfs reflink copies are distinct inodes, so
  the page cache never carries between benchmark runs — every run is cold.

## What changed (commit 05a122c0)

1. `PRAGMA mmap_size = 64 GiB` on both stores, plus
   `SQLITE_MAX_MMAP_SIZE` raised at build time (`.cargo/config.toml` `[env]`)
   because the stock ceiling of 2 GB reduced the pragma to mapping the file's
   first two gigabytes. A regression test asserts the effective values.
2. 16 KiB pages for newly created databases. Nodes of one block are adjacent
   under the `(block, idx)` primary key, so a wide page carries what four
   narrow ones did. Existing databases keep their size; `VACUUM INTO` under
   the new binary upgrades them offline — and shrank the mainnet MARF
   23 GB → 14 GB (defrag + fewer pages) with clarity.sqlite unchanged.

## Result — same 99 blocks, same anchor, same machine

| configuration | wall | user CPU |
|---|---|---|
| nano before (4 KiB, pread) | 24.1 s | 7.0 s |
| + warm native-module cache | 23.5 s | 3.5 s |
| + mmap | 18.7 s | 3.2 s |
| + 16 KiB MARF (defrag alone: 17.0 s) | 15.5 s | 3.0 s |
| + 16 KiB clarity | **12.4–14.3 s** | 2.9 s |
| stacks-core validate-only, same range | 20.8–23.7 s | 2.2 s |

nano now replays the range ~1.6× faster than stacks-core — while committing
every block durably, which stacks-core's validate mode does not.

## Integrity evidence

- The replay itself checks `state_index_root` block by block; every run
  exited 0.
- Green after the change: MARF lockstep (roots vs stacks-core's own MARF),
  checkpoint suite, `kill_during_replay`, `kill_during_import`, `restart`,
  `write_journal`, nano-marf and nano-vm unit suites, workspace clippy.

## Why mmap is safe here, and its one trade-off

`mmap_size` maps address space, not RAM: pages materialize on first touch,
live in the same kernel page cache `pread` filled, and evict cleanly under
pressure — SQLite never writes through the map (writes stay on the
`write()` + WAL path, which is what the kill suites verify). A database
larger than memory therefore degrades to exactly the old `pread` behavior on
misses while keeping cheaper hits; a file larger than the 64 GiB pragma falls
back to `pread` past that offset. The trade-off: a disk-level I/O error under
mmap is a SIGBUS crash rather than a typed storage error. That converts rare
hardware failure into crash-and-restart, which this node's recovery is tested
for and the container restart policy absorbs.

## Follow-ups

- Migrate `/home/aldur/mainnet-tip` to 16 KiB stores (`VACUUM INTO`, ~2 min
  for 23 GB) at the next planned restart; the mmap gain arrives with the
  binary alone.
- Remaining CPU gap candidates, deliberately not taken now: clar2wasm
  recompiles a contract to *find* its native-cache key (keying the store
  semantically would skip codegen on hits), and the wasmtime linker rebuilds
  223 host functions per call (blocked on `ClarityWasmContext`'s lifetimes).

## Reopened 2026-08-10: the longer benchmark reverses the verdict

Extending the capture to 6,182 blocks (task 109's fixtures now carry the
archive's canonical chain, its sortition snapshots to burn 960341 and the raw
Bitcoin blocks) showed the 99-block result did not generalize:

| same 4,149 blocks (8,665,602–8,669,750) | wall | user | sys |
|---|---|---|---|
| nano, 16 KiB + mmap | 29:14 | 501 s | 924 s |
| nano, 16 KiB + pread (`1a9be3a1`) | 23:36 | 501 s | 183 s |
| stacks-core validate-only | **5:54** | 158 s | 72 s |

- The short-range "win" was startup asymmetry: stacks-core pays ~14 s opening
  the 722 GB chainstate, nano ~4 s. Per block, stacks-core was always faster.
- mmap reverted (`1a9be3a1`): identical user CPU, but at scale the kernel
  fault path cost 924 s vs 183 s through `pread` — with the map, SQLite
  bypasses its own cache and leans entirely on a kernel page cache that is
  under pressure on a box that also runs a follower. 16 KiB pages stay.
- The offline replay stops at block 8,669,751: a transaction reads a header
  outside the carried ancestry, which production backfills from a peer and
  offline replay correctly refuses. That bounds the comparable range.

## What closing the remaining 4× takes, measured

Per block: nano 121 ms user + 44 ms sys + ~176 ms wait; stacks-core
38 + 17 + ~30. Two fronts, both needing profiling-grade attribution
(the box has `perf_event_paranoid = 3` and no ptrace, so an instrumented
build is the tool):

1. **User CPU, 3.2×**: candidates are trie-node decode volume, clar2wasm
   recompiling contracts to find their native-cache key, per-call linker
   setup (223 host functions), and per-seal hashing.
2. **Read wait, ~6×**: nano reads ~7 MB per block for the ancestry walk;
   stacks-core's flat blob layout reads a fraction of that from a file 30×
   larger. Read amplification, not disk speed, is the gap.

## Second round, 2026-08-10 evening: the arena (commit 35487894)

An instrumented build split the user CPU: 221 of 389 seconds of a
1,500-block replay were spent inside node *decode* — which blocks on
resolving back-pointer children to ancestor block hashes through an 8 MB
id→hash cache standing against 8.7 million blocks. Replaced with a flat
append-only arena (one sequential scan at open, ~280 MB for the whole
graph, retraction-safe because deleted ids are unreachable and `forget`
rebuilds it).

| same 4,149 blocks | wall | user | sys | CPU |
|---|---|---|---|---|
| nano, 16 KiB + pread | 23:36 | 501 s | 183 s | 48% |
| nano + arena | **7:46** | 409 s | 60 s | 100% |
| stacks-core validate-only | 5:54 | 158 s | 72 s | 65% |

Gap now 1.32×, and nano is compute-bound: with the arena, MARF fetch and
decode are 5.7 s of 159 s user CPU on the instrumented 1,500-block run.
The remainder lives in the VM/host path. Next candidates, in likely order
of size: the per-call wasmtime linker rebuild (223 host functions per
call; caching blocked on `ClarityWasmContext` lifetimes), boundary value
copies, `compile_under` running full codegen to find a native-cache key,
and per-seal hashing. Lockstep, checkpoint and kill-during suites green
after the change.

## Third round: allocator and codegen (commits 35487894, f9ed1e95)

| same 4,149 blocks, durable commits | wall |
|---|---|
| nano, pre-optimization | 23:36 |
| + block-hash arena | 7:46 |
| + mimalloc | 6:40 |
| + thin LTO, one codegen unit (+`target-cpu=native`, bench only) | **6:34** |
| stacks-core validate-only, no commits | 5:54 |

The gap is 1.11×, with a structural asymmetry in stacks-core's favor
still in the number: nano writes and fsyncs every block durably; core
validates and discards (~60 s of nano's remaining time is that WAL
traffic). Per block executed-and-committed, nano is at 95 ms against
core's 85 ms validated-only.

Remaining path to parity and beyond, in measured order: the per-call
wasmtime linker rebuild (223 host functions per call — blocked on
`ClarityWasmContext` lifetimes), `compile_under` running full codegen to
find a native-cache key on process-cold contracts, boundary value
copies, and a validate-only replay mode if an exactly-symmetric
benchmark is ever wanted.

## Fourth round: the last measured levers (6e694ef6, 491a109c)

- WAL checkpoints spaced 16× (`wal_autocheckpoint = 16384`): kill suites
  green; neutral-to-small on the replay, kept for fewer write stalls at tip.
- Side-store value cache under the value hash, present-values only
  (a remembered absence could go stale when a later block writes that hash):
  ~10 s of the replay.
- Warmed native-module cache in the anchor state: 486 contracts, −50 s of
  user CPU, but the wall trades compile CPU for module-load I/O — net ~zero.
- **Measured dead end**: the per-call wasmtime linker rebuild costs 5 ms
  over 52 builds per 1,500 blocks — it is per contract *initialization*,
  not per call. The audit's per-call concern does not apply to this path.

Standing: nano 6:34–6:48 (run-to-run variance under the live follower's
load), stacks-core 5:54, both on the 4,149-block range — nano committing
durably, core discarding. The remaining ~40 s is wasm-runtime and host-path
CPU (294 s user vs core's 158 s), where fetch, decode, hashes, linking,
compilation and the side store are now all measured small. Next: profile
inside the VM boundary (value serialization at the host boundary, per-call
Store/Instance setup, memory copies) — instrumented builds, since the box
allows no perf or ptrace.
