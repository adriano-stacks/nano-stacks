---
id: "107"
group: mainnet
title: "Hold the follower's memory flat while it follows at tip"
status: in-progress
priority: critical
effort: medium
dependencies: []
tags: ["mainnet", "liveness", "operations", "release", "marf", "vm", "sync"]
created_at: 2026-08-10
type: bug
---

# Hold the follower's memory flat while it follows at tip

## Objective

The follower grows ~0.6–0.75 GB of anonymous heap per hour while doing nothing
but following at tip, and the kernel eventually kills it. A node that cannot
hold tip for a day on a 31 GB machine fails [[106-hold-the-release-candidate-at-mainnet-tip-for-24-hours]]
before it starts.

## Evidence

- `journalctl -k`, 2026-08-10 04:54:05: `Out of memory: Killed process 90263
  (stacks-node) total-vm:19943412kB, anon-rss:18346600kB` — 18.3 GB after 15.3 h
  of following (`/home/aldur/mainnet-tip/run-133651.log`, 6,459 blocks executed,
  no miner, no signer, no event observers).
- Reproduced on the restarted node 2026-08-10: 6.83 → 7.17 GiB in 29 minutes at
  tip with no backlog, all of it `Anonymous`/`Private_Dirty` in `smaps_rollup`.
  ~2.9 GB of the baseline is the two SQLite page caches, which are configured
  and bounded; the growth is not.

## Causes, ranked by magnitude

1. **The MARF node cache is bounded in entries, not bytes**
   (`crates/nano-marf/src/storage.rs`, `NODE_CACHE = 1_000_000`, two
   generations ⇒ ~2M resident). Its sizing comment assumes a node is a couple
   hundred bytes, but a decoded child costs 56 bytes in memory, so a dense
   Node256 is ~14 KB — and the ancestry-walk working set the cache exists for
   is exactly those wide nodes. A cold-generation hit is additionally cloned
   into the hot generation without being removed from cold, holding the hot
   working set twice.
2. **The compiled-contract cache never evicts**
   (`vendor/clarity-wasm/clar2wasm/src/lib.rs`, `ModuleCache.contracts`; owner
   `crates/nano-vm/src/lib.rs`, `Vm.modules`). Every contract ever touched
   retains its wasm bytes, its `wasmtime::Module`, and a full
   `ContractAnalysis` — the whole AST plus one boxed `TypeSignature` per AST
   node — for the life of the process.
3. **The followed-tenure history is unbounded and deep-cloned three times per
   poll round** (`crates/nano-sync/src/lib.rs`, `TenureFollower.history`;
   clones in `Node::view()` and the RPC's published snapshots). Gigabytes per
   minute of allocate-and-free churn under glibc malloc ratchets RSS.
4. **A non-mining follower's mempool is insert-only**
   (`crates/nano-mempool/src/lib.rs`): expiry runs only inside
   `Mempool::advance`, which only the miner calls. Small (~2–3 MB/h), but a
   follower must age its pool.

## Tasks

- [x] Bound the MARF node cache by resident bytes rather than entry count, and
      stop the cold→hot promotion holding an entry in both generations.
      Measured first: near tip 42% of cached nodes are wide (~2.5 KB serialized,
      ~14 KB decoded), so 2×1M entries was ~12 GB. Now 1.5 GB per generation.
- [x] Give the module cache an eviction policy with a byte budget.
      Least-recently-called beyond 512 MB estimated; `get` keeps `&self` via a
      `Cell` stamp, so no caller changed.
- [x] Window `TenureFollower.history` to what its consumers can name, and stop
      the per-round deep clones of the whole history. Sixteen kept (fork check
      walks 10, winner scan wants 8); the winner scan reads the slice in place.
- [x] Age the mempool on a follower, not only inside the miner's `advance`.
      `admit_relayed` advances the pool under the locks it already holds.
- [x] Redeploy `/home/aldur/mainnet-tip` on the fixed binary and measure the
      slope at tip again. Docker records the `8e27c16b` process starting at
      2026-08-10 11:22:06 UTC, two minutes after that commit; it returned to
      tip in one round at 248 MiB (previously 6.8 GiB after sync). Slope
      measurement running.

## Acceptance Criteria

- Steady-state RSS at tip is flat to within page-cache noise over ≥ 2 hours
  (previously +1.2–1.5 GB over the same window).
- The node still follows at tip: executed height tracks the sync peer's
  advertised height, state roots verifying.
- Existing conformance and unit suites stay green; clippy stays clean.
