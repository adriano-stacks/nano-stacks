---
id: "037"
title: "Replay mainnet from the epoch 4.0 boundary"
status: pending
priority: critical
effort: large
type: feature
dependencies: ["020", "021", "022", "023", "024", "025"]
tags: ["mainnet", "replay", "conformance"]
created_at: 2026-07-30
---

# Replay mainnet from the epoch 4.0 boundary

## Objective

The milestone that decides whether any of the rest of it worked. M10 proved nano
computes the same chain state as stacks-core for 600 Hacknet blocks from a
regtest checkpoint. This is the same claim against the chain that matters.

Everything before it is a component check. The oracle is the same as M10's —
`state_index_root` per header and every receipt from the event observer — pointed
at mainnet blocks after the 4.0 boundary instead of captured Hacknet ones.

Replay depth is the metric again, and it stays at zero until the blockers this
depends on are done.

## Tasks

- [ ] Capture a mainnet checkpoint at or after the 4.0 boundary, with the blocks,
      the burn blocks and the receipts that follow it.
- [ ] Teach the fixture tooling and the scoreboard about a mainnet capture.
- [ ] Replay forward and report the first divergence with the field that
      diverged.
- [ ] Work the divergence point forward until it stops moving for a real reason
      or reaches the tip.
- [ ] Keep a bounded slice of the capture in CI as a regression gate.

## Acceptance Criteria

- `cargo xtask scoreboard` reports a mainnet replay depth alongside the Hacknet
  one.
- Every replayed mainnet header has the matching `state_index_root`.
- Every replayed transaction has the matching receipt, including status, costs
  and events.
- The replay runs offline from captured fixtures.
