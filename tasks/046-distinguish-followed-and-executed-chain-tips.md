---
id: "046"
title: "Distinguish followed and executed chain tips"
status: completed
priority: critical
effort: medium
type: bug
group: mainnet
dependencies: []
tags: ["mainnet", "rpc", "observability"]
created_at: 2026-08-02
completed_at: 2026-08-02
---

# Distinguish followed and executed chain tips

## Objective

The running node publishes the peer's `NodeInfo` through `/v2/info` before the
executor runs. On 2026-08-02 the RPC reported height 8,688,023 while the durable
MARF ended at 8,665,601. Near-tip samples were consequently mistaken for 18,290
successful state-root checks in [[037-replay-mainnet-from-the-epoch-4-boundary]].

A node must say separately what the peer advertised, what fork it selected, and
what state it actually executed. Account and contract reads must never be served
as though they belong to a newer peer-facing tip.

## Tasks

- [ ] Give the followed, selected, and executed tips distinct state and names.
- [ ] Serve the Stacks-compatible tip fields from the executed canonical state.
- [ ] Expose followed-tip and catch-up progress separately through metrics or a
      clearly nano-specific status surface.
- [ ] Log every successful execution batch with its start, end, block count and
      final state root; log zero-block batches distinctly.
- [ ] Keep all RPC reads in one executed-state snapshot.
- [ ] Add a regression where following reaches tip while execution fails.

## Acceptance Criteria

- `/v2/info` never reports a Stacks height above the `ChainAccess` tip it serves.
- A peer at height N and an executor at N-100 are visible as two different facts.
- Mainnet replay evidence can name durable executed heights and root counts
  without inferring them from absence of errors.
