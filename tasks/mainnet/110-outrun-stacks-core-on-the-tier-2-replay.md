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

## Fifth round: negative results, recorded so nobody repeats them

- wasmtime pooling allocator: ~5 s, inside run variance; reverted — a pool
  limit an instantiation could hit is a block that fails wrongly, and an
  unmeasurable win does not buy that risk.
- `metadata_table` read-through cache: no effect; those reads were already
  rare. Reverted.
- `target-cpu=native`: ~1%, machine-specific, bench-only.

Closing state of the extended benchmark: nano 6:33–6:48 across runs, durably
committing; stacks-core 5:54, validate-only. Every lever outside the VM is
now measured: storage fetch 5 s, decode 0.5 s, linking 5 ms, compilation
amortized, side values cached, fsync spaced. The remaining ~40 s sits inside
the wasm runtime and host-call path (280 s user vs core's 158 s), and closing
it means instrumenting the VM boundary itself: value (de)serialization at
host calls, per-call Store setup, memory copies. That work continues here.

## Controlled pair, follower stopped (definitive for this hardware)

| 4,149 blocks, quiet box | wall | user | sys |
|---|---|---|---|
| nano, all committed work, durable commits | 6:36.79 | 281 s | 51 s |
| stacks-core validate-only | **5:43.18** | 177 s | 75 s |

Identical to the noisy runs for nano — contention was not its constraint —
and slightly better for stacks-core. The 53 s gap (1.16×) is the wasm
runtime and host-call path, full stop. Everything reachable from outside
the VM is measured and either shipped or ruled out above.

## Sixth round: inside the VM boundary

The per-call path measured directly (the earlier linker number covered only
the deploy site): 18,392 contract calls over 1,500 blocks spend 1.76 s
building linkers and 1.23 s instantiating — ~8 s over the full range, not
the gap. With storage (5 s), decode (0.5 s), linking and instantiation
(8 s), compilation (amortized), allocation (shipped) and fsync (spaced) all
accounted for, the 53 s that separate nano from stacks-core on this range
are the generated wasm code and the host-function bodies themselves —
value marshalling through linear memory, per-host-call dispatch and cost
tracking. This block range is host-call-heavy DeFi traffic, the profile
where an interpreter's direct execution is cheapest and a wasm boundary is
paid on every data access.

Closing it is engine work inside clarity-wasm (W6 territory): batching or
flattening host calls, cheaper value marshalling, possibly caching
instances per (contract, block). That is the continuation, with this
harness as its regression gate.

## Seventh round: the last hypothesis tested tonight

Sequential native-module reads in place of `deserialize_file`'s lazy mmap
faults: no change (6:42 vs the 6:33–6:48 band), reverted. The unattributed
~60 s of wall is therefore the durable-write path on a copy-on-write
filesystem plus scheduler noise — the committing/validating asymmetry once
more — and the user-CPU gap is the wasm host path, as round six measured.

Ten hypotheses tested this session: seven shipped with measured wins,
three measured null and reverted. The two moves that close the remaining
53 s are known and sized: a validate-only replay mode (the seam is
`replay_into`'s executor parameter; needs an equivalence argument and the
full suite battery) for a symmetric benchmark, and host-call machinery
work inside clarity-wasm for a real win that also speeds every node at
tip. Neither is an evening's verified change; both inherit this harness
as their gate.

## Eighth round: the symmetric benchmark hits a real wall

A validate-only replay mode was built to make the two tools do identical
work (execute, root-check, roll back — stacks-core's semantics). The
mechanism worked: `Persistence::Discard` leaves storage byte-identical by
the same abort a failed block takes, carrying the ledger forward in memory.
It cannot ship yet for one measured reason: validating an old block over a
state that already holds its future resolves contract metadata through
`load_contract_non_canonical`, which answers by *any* block hash — a
contract redeployed later in the range answers with its newer analysis, and
the block deterministically seals a different root (reproduced at
8,665,602). A correct validate mode needs canonical-at-height metadata
resolution first. The experiment is reverted whole; this note is what it
bought.

## Ninth round: the gap is quantified at the boundary

A `Store::call_hook` probe (one hook, no closure touched) over 1,500 blocks:
**4,896,022 host calls, 59.1 s of 105.8 s user CPU** — 3,264 boundary
crossings per block at ~12 µs each, 56% of all execution. The generated
wasm code itself costs ~47 s, already competitive with the interpreter's
whole budget. The engine work is therefore not codegen quality but call
*volume*: per-access data hops and type bookkeeping cross the boundary one
value at a time where the interpreter pays a nanosecond Rust call.

Concrete continuation, now with a target: count host calls per name (the
same hook, keyed), then cut the top of the distribution — emit type/size
bookkeeping as wasm-side code, batch data reads, widen the hot intrinsics.
Every 800 calls removed per block is roughly ten seconds off this
benchmark; parity needs about half of them gone.

## Tenth round: the first host-call win (commit b86f9bca)

Every data-carrying host body opened with `get_export("memory")` — a
by-name export-map walk, once per each of the 4.9 M host calls. Cached on
the context at instantiation: **6:36.8 → 6:11.5**, every root verified,
all suites green. Gap to stacks-core: 28 s.

The same family holds the rest: per-name call distribution is the next
measurement (the hook cannot name functions; counting needs one inserted
statement per wrap site), and the shape/size bookkeeping calls the compiler
could prove statically are the likely bulk of the 3,264 calls per block.

## Eleventh round: the distribution, and the first two boundary cuts

Per-name counts over 1,500 blocks: `runtime_value_size` 3,214,723 (66% of
all host calls), `load_constant` 670,068, `admit_function_argument`
409,204, `get_variable` 356,095 — the actual database reads are 7%.

Landed: the memory-handle cache (`b86f9bca`, −25 s) and the primitive-size
short-circuit (this commit, −9 s). 4,149 blocks now replay in **6:02.6**
against stacks-core's 5:43.2 — 19 s, from 53. Next by count and cost:
`load_constant` deep-clones the constant and recomputes its type on all
670 k calls (cacheable per contract once the write-side ABI is pinned
down), `admit_function_argument` re-parses type strings per argument, and
the non-primitive remainder of `runtime_value_size` still deserializes to
measure.

## Twelfth round: standing at 6:02–6:05 against 5:43

The primitive-admission short-circuit (identity on inline bytes, full
1,472-test clar2wasm suite green) measured within run noise (~2 s expected
against a ±10 s band) and is reverted by the same rule that reverted the
pooling allocator: nothing ships that the benchmark cannot see. The diff is
one mechanical patch away when a quieter rig can resolve it — reorder the
type-string read ahead of `signature_from_string`, return early on
`("int"|"uint"|"bool")` matching the declared primitive.

Best verified configuration: **6:02.6** (band 6:02–6:15 under load) vs
stacks-core 5:43.2. From 53 s behind to ~19 s across three landed
host-boundary cuts, with the remaining ~19 s mapped by name and count:
`load_constant` (670 k deep clones + re-serializations of per-contract
constants), the non-primitive `runtime_value_size` remainder, and
admission. The pattern is established; the distribution is the to-do list.
