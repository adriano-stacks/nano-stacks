---
title: "Show per-transaction tenure budget consumption in nano-tui"
id: "143"
status: completed
priority: high
effort: medium
type: feature
group: mainnet
dependencies: ["129"]
tags: ["tui", "rpc", "execution-cost", "ux"]
created_at: "2026-08-14"
verify:
  - type: bash
    run: "nix develop -c cargo test -p nano-chainstate -p nano-rpc -p nano-tui"
  - type: bash
    run: "nix develop -c cargo clippy -p nano-chainstate -p nano-rpc -p nano-tui --all-targets -- -D warnings"
completed_at: 2026-08-17
---

# Show per-transaction tenure budget consumption in nano-tui

## Objective

Let a user inspecting one transaction see the execution resources it actually
consumed and how much of the applicable tenure budget those raw costs represent.
Expose this only from execution receipts the node can retain authoritatively;
never estimate cost from transaction shape or divide against an unrelated epoch.

## Tasks

- [x] Trace the block-execution result and confirm whether authoritative
      per-transaction `ExecutionCost` receipts exist at the execution boundary.
- [x] If they are discarded, retain the minimal cost receipt beside the executed
      transaction without re-executing it or expanding consensus state.
- [x] Expose raw read count, read length, write count, write length and runtime
      through the nano block/receipt surface consumed by `nano-tui`.
- [x] Associate each receipt with the epoch/tenure limit that applied when it was
      executed; return unavailable rather than comparing unlike budgets.
- [x] Render each raw dimension and its percentage of the applicable tenure
      budget in the single-transaction inspector, including a concise aggregate
      explanation that the dimensions are independent limits.
- [x] Distinguish zero cost, unavailable receipts and unavailable limits.
- [x] Cover transactions with cost, zero-cost dimensions, historical epoch
      limits and nodes that do not retain receipts.
- [x] Run rustfmt, focused tests and strict clippy without warnings.

## Acceptance Criteria

- The transaction page shows authoritative raw cost and percentage for every
  execution-cost dimension when both a receipt and its applicable limit exist.
- Percentages are computed from the limits that governed that transaction, not
  the current epoch or the remaining tenure budget at viewing time.
- Missing data is labelled unavailable and never rendered as zero or inferred.
- The UI explains that no single percentage can accurately collapse the five
  independent execution limits.
