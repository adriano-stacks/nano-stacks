---
id: "110"
group: mainnet
title: "Outrun stacks-core on the tier-2 replay"
status: completed
priority: medium
effort: medium
dependencies: ["109"]
tags: ["mainnet", "performance", "marf", "vm", "storage"]
created_at: 2026-08-10
completed_at: 2026-08-10
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
