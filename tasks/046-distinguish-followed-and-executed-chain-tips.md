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
completed_at: 2026-08-08
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

- [x] Give the followed, selected, and executed tips distinct state and names.
- [x] Serve the Stacks-compatible tip fields from the executed canonical state.
- [x] Expose followed-tip and catch-up progress separately through metrics or a
      clearly nano-specific status surface.
- [x] Log every successful execution batch with its start, end, block count and
      final state root; log zero-block batches distinctly.
- [x] Keep all RPC reads in one executed-state snapshot.
- [x] Add a regression where following reaches tip while execution fails.

## Acceptance Criteria

- `/v2/info` never reports a Stacks height above the `ChainAccess` tip it serves.
- A peer at height N and an executor at N-100 are visible as two different facts.
- Mainnet replay evidence can name durable executed heights and root counts
  without inferring them from absence of errors.

## Reconciliation, 2026-08-08

The six bullets above were implemented but this file had never been reconciled
with the code that landed.

- `RpcState` keeps the followed `NodeView`, selected `SelectedTip` and executed
  `SealedTip` separately. `/nano/sync_status` names all three, the selected peer,
  the executed state root and the gap from the followed to the executed height.
- `/v2/info` is built only from `SealedTip`. With no executed tip it returns 503,
  even when the node has already followed a peer.
- `round_report`, `failed_round_report` and `batch_report` name the start, end,
  count and final root of work that executed, and use the distinct sentence
  `executed nothing: sealed at ...` for an empty batch.
- Account and contract reads go through the one executed `ChainAccess`; block,
  tenure, PoX and sortition routes are likewise bounded by the published executed
  snapshot or the archive of blocks this node executed.
- `the_served_tip_is_the_executed_one_not_the_followed_one`,
  `the_followed_selected_and_executed_tips_are_three_separate_answers`,
  `a_node_that_executed_nothing_serves_no_tip` and
  `no_route_serves_a_block_the_node_has_not_executed` pin the original failure.

The live node on port 20492 supplied the real endpoint evidence at revision
`51ab2bcc43a37d944593a87bd3911cfb67ead081` on
2026-08-08T12:49:58Z. `/v2/info` served executed height 8,722,017 and burn height
961,582. In the same sample `/nano/sync_status` reported followed height
8,722,017, selected height 8,721,989 from `http://108.130.44.244:20443/`, executed
height 8,722,017, executed root
`14179091398a764b0bd5012d03c8027323cff7402067fba943c428f389c38901` and zero
blocks behind. The selected poll being older than the already executed state is
also exactly why these are separately named facts rather than one overloaded
"tip" field.
