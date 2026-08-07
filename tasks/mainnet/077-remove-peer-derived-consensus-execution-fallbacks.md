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

- [x] Remove the production `LocalView::NoChain` path that uses a peer
      `/v3/sortitions` response as execution context.
- [~] Refuse startup when checkpoint sortition history, PoX history or payout
      calendar cannot seed the local Bitcoin-derived chain.
- [x] Treat `SortitionTracker::resume_or_capture` failure as a runtime startup
      error instead of logging it and continuing.
- [x] Remove the peer `tenure_coinbase_context` fallback; maturity and coinbase
      accounting must come from checkpointed and locally derived state.
- [x] Keep peer sortition responses, if retained, diagnostic-only and prevent
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

## Where this stands, 2026-08-07

The execution fallbacks are gone: `context_for` does not construct a Bitcoin
execution context from `LocalView::NoChain`, tenure coinbase has no peer branch,
and sortition startup failure is fatal. The task was nevertheless marked completed
with every checkbox unchecked and without the adversarial test matrix.

It remains open for two concrete reasons. First, checkpoint contradiction is not
uniformly fail-closed: [[083-refuse-an-unrecoverable-checkpoint-winner-seed-bef]]
records the path that logs an unrecoverable winner seed and continues by sampling
against zero. Second, the tests must prove that each peer-supplied burn field and
accumulated coinbase is unable to reach execution, rather than relying only on a
source-boundary assertion. Task 082 separately owns the reward-cycle liveness
regression exposed by removing the fallback; it is not a reason to restore one.

## Evidence that opened this task

`LocalView::NoChain` currently fills Bitcoin height, burn header hash, timestamp
and VRF seed from a peer. `tenure_coinbase` also asks the peer for accumulated
coinbase outside `LocalView::At`, and failure to start local sortition derivation
is only printed.
