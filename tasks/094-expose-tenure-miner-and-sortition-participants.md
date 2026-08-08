---
title: "Expose tenure miner and sortition participants"
id: "094"
status: completed
priority: high
effort: medium
type: feature
group: mainnet
dependencies: ["091"]
tags: ["tui", "rpc", "sortition", "explorer"]
touches: ["crates/nano-sortition", "crates/nano-sync", "crates/nano-rpc", "crates/nano-node", "crates/nano-tui"]
created_at: "2026-08-08"
---

# Expose tenure miner and sortition participants

## Objective

Explain who is mining the current tenure and how the local burnchain sortition
selected that miner, including the other candidate commitments.

## Tasks

- [x] Retain the locally derived commitment distribution on each sortition snapshot.
- [x] Expose winner, participant, burn and sampling-window details through RPC.
- [x] Add current-miner and competition context to the dashboard.
- [x] Add a navigable mining view listing every candidate commitment.
- [x] Show the selected participant's miner keys, transaction, commitment and weight.
- [x] Distinguish missing competition data from an uncontested sortition.
- [x] Cover derivation, RPC serialization, navigation and default-width rendering.
- [x] Run rustfmt, focused tests and strict clippy without warnings.

## Acceptance Criteria

- The TUI identifies the current tenure's miner and the winning commitment.
- The TUI shows every locally derived sortition participant and enough burn/window
  context to explain relative selection weight without calling it a probability.
- Nodes or historical views without retained competition data say it is unavailable.
- Existing stock-compatible sortition fields remain unchanged.
