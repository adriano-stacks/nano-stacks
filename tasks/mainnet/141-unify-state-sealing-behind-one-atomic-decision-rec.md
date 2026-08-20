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
