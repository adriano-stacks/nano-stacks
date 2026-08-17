---
id: "135"
title: "Ship a minimal follower-only mainnet artifact"
status: in-progress
priority: critical
effort: large
dependencies: ["130", "131", "132", "133"]
tags: ["mainnet", "node", "architecture", "release", "safety"]
created_at: 2026-08-14
parent: 053
type: feature
---

# Ship a minimal follower-only mainnet artifact

## Objective

Make the first mainnet product contain only what is required to authenticate,
select, execute and durably follow the Epoch 4.0 chain. Move mining, signing,
proposal hosting, TUI and broad compatibility services out of the consensus
artifact and its dependency closure.

## Tasks

- [x] Define the minimal follower capability and dependency matrix: local
      Bitcoin view, Stacks P2P acquisition, authentication/fork choice,
      clarity-wasm execution, durable state, health and metrics.
- [x] Produce a separate follower binary/package that does not link miner,
      signer, TUI, proposal validation, StackerDB hosting or mutation-capable
      compatibility RPC code.
- [x] Default the follower to outbound P2P and loopback health/metrics. Make any
      public serving edge an explicit separately supervised component.
- [ ] Keep optional miner, signer, TUI, event and compatibility adapters as
      separate processes with bounded protocols and no direct chainstate write
      authority.
- [x] Add dependency-tree, route-inventory and binary-inspection gates proving
      forbidden roles and fallback engines are absent rather than disabled by
      configuration.
- [x] Run checkpoint import, P2P catch-up, fork/reorg, restart and tip following
      through the exact packaged follower artifact.
- [ ] Measure whether omitting persistent native modules or inbound service
      violates the documented catch-up/liveness bound; retain neither without a
      measured need.

## Acceptance Criteria

- The follower artifact follows mainnet from an authenticated checkpoint and
  produces the same roots and receipts as the full development node.
- It cannot enable mining, signing, proposal hosting, TUI or mutating RPC routes
  through a flag, environment variable or configuration change.
- Only the follower has chainstate write authority; optional components can be
  stopped, compromised or restarted without corrupting execution state.
- The artifact's feature/dependency and route inventories are mandatory release
  evidence.
- P2P-only catch-up, restart and sustained-tip gates pass on the packaged binary.
