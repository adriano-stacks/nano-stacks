---
id: "141"
title: "Unify state sealing behind one atomic decision record"
status: blocked
priority: high
effort: large
dependencies: ["079", "140"]
tags: ["mainnet", "storage", "consensus", "recovery", "architecture"]
created_at: 2026-08-14
type: improvement
---

# Unify state sealing behind one atomic decision record

## Objective

Give each executed block one durable linearization point and one canonical
decision record, reducing the reasoning and recovery surface of the current MARF
plus side-store durability protocol without sacrificing checkpoint compatibility.

## Tasks

- [x] Specify a content-addressed decision record binding parent, authenticated
      consensus inputs, writes, ledger/header metadata, state root, receipts,
      costs, events and compiler/epoch identity.
- [x] Evaluate one database/transaction first; compare it with a rigorously
      proven multi-file atomic transaction or append-only seal protocol. Document
      filesystem, SQLite journal-mode and power-loss assumptions.
- [ ] Prototype and benchmark the simplest design against mainnet catch-up,
      steady-state latency, storage growth, pruning and checkpoint export.
- [ ] Make the decision record the only committed-block visibility point for
      chain reads, RPC, events, restart and fork switching.
- [ ] Provide a streaming, restart-safe migration from existing Epoch 4.0 state
      that verifies every root and can preserve the original state for rollback.
- [ ] Inject failures at every write, flush, rename and commit boundary, including
      power-loss simulation, `ENOSPC`, `EIO`, corruption and interrupted migration.
- [ ] Remove the old two-store commit path after a complete shadow replay and
      recovery qualification; do not retain a runtime fallback.

## Measured status, 2026-08-23

As with [[140-extract-the-epoch-4-0-consensus-firewall]], the checkboxes
understate the tree, so this records what is in it.

**The sealed record is durable already.** `crates/nano-chainstate/src/decision.rs`
holds it and commit `290abcc6` made it durable beside the ledger, with the type
also reached from `nano-vm` and `epoch4-consensus`.

**Fault injection is largely built, for the current protocol.**
`nano-conformance/tests/conformance/storage_faults.rs` carries seven tests
covering `ENOSPC`, `EIO`, corruption, `fsync`, rename, truncation and interrupted
writes, and `kill_during_replay.rs` six more. What the task asks for beyond that
is power-loss simulation and an *interrupted migration*, and the latter cannot be
tested before the migration in the fifth box exists.

**What is genuinely absent is the visibility switch.** Nothing in
`nano-node/src/runtime.rs` reads a decision record, so it is not yet the only
committed-block visibility point for chain reads, RPC, events, restart and fork
switching. Until it is, the streaming migration, the benchmark set against
mainnet catch-up and pruning, and the removal of the two-store commit path all
stay ahead rather than behind.

The nearest real-world datum arrived today from the release import rather than
from a test: a checkpoint import is *not* resumable, because journalling is off
while it runs and a partial trie cannot be told apart from a complete one by
reading it. That is exactly the class of torn intermediate state this task exists
to abolish, and it is worth carrying into the design as a worked example.

## Acceptance Criteria

- A block is either absent or fully visible with its exact decision record after
  every injected failure; there is no reachable orphan or partial metadata.
- One documented operation is the durable linearization point on every supported
  filesystem/configuration.
- Existing state migrates with identical roots and receipts, resumes at the same
  tip, and the source remains recoverable until explicit operator confirmation.
- Performance remains inside the declared catch-up and tip-following bounds.
- The old durability protocol and compatibility fallback are absent from the
  final production closure.
