---
id: "077"
title: "Remove peer-derived consensus execution fallbacks"
status: in-progress
priority: critical
effort: medium
type: bug
group: mainnet
dependencies: ["049", "051"]
tags: ["mainnet", "consensus", "sortition", "checkpoint"]
created_at: 2026-08-07
---

# Remove peer-derived consensus execution fallbacks

## Objective

Ensure a Stacks peer can serve only candidate data. No absence or failure of the
local sortition chain may let a peer choose consensus-visible execution context.

## Tasks

- [ ] Remove the production `LocalView::NoChain` path that uses a peer
      `/v3/sortitions` response as execution context.
- [ ] Refuse startup when checkpoint sortition history, PoX history or payout
      calendar cannot seed the local Bitcoin-derived chain.
- [ ] Treat `SortitionTracker::resume_or_capture` failure as a runtime startup
      error instead of logging it and continuing.
- [ ] Remove the peer `tenure_coinbase_context` fallback; maturity and coinbase
      accounting must come from checkpointed and locally derived state.
- [ ] Keep peer sortition responses, if retained, diagnostic-only and prevent
      their fields from reaching Clarity headers, validation or fork choice.
- [ ] Add adversarial tests in which peers lie about Bitcoin height, burn hash,
      timestamp, VRF seed and accumulated coinbase while local execution remains
      unchanged or refuses before execution.

## Acceptance Criteria

- No production path executes a block without a locally derived burn view.
- A missing or contradictory local consensus chain causes typed startup refusal.
- Removing `/v3/sortitions` and tenure-coinbase HTTP access cannot affect a node
  with a complete checkpoint and Bitcoin source.
- A peer cannot change a Clarity-visible burn value, reward maturity, canonical
  fork or accepted state root.

## Evidence that opened this task

`LocalView::NoChain` currently fills Bitcoin height, burn header hash, timestamp
and VRF seed from a peer. `tenure_coinbase` also asks the peer for accumulated
coinbase outside `LocalView::At`, and failure to start local sortition derivation
is only printed.
