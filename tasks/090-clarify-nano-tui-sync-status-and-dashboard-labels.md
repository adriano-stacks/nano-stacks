---
title: "Clarify nano-tui sync status and dashboard labels"
id: "090"
status: completed
priority: high
effort: small
type: feature
group: mainnet
dependencies: ["089"]
tags: ["tui", "ux", "explorer"]
touches: ["crates/nano-tui"]
created_at: "2026-08-07"
verify:
  - type: bash
    run: "nix develop -c cargo test -p nano-tui"
  - type: bash
    run: "nix develop -c cargo clippy -p nano-tui --all-targets -- -D warnings"
completed_at: 2026-08-07
---

# Clarify nano-tui sync status and dashboard labels

## Objective

Make the dashboard describe sync state in operator language instead of exposing
internal field names and unactionable identifiers.

## Tasks

- [x] Give the three Stacks heights explicit provenance and meaning.
- [x] Show the full selected peer on its own line.
- [x] Remove tip, root and tenure hashes from the overview.
- [x] Rewrite tenure and sortition panels around readable status and countdowns.
- [x] Keep identifiers available in the block explorer where they have context.
- [x] Cover the new labels and full peer rendering with tests.
- [x] Run rustfmt, tests and strict clippy.

## Acceptance Criteria

- Local execution, fork choice and peer reports cannot be mistaken for one another.
- The selected peer is not shortened by the hash formatter.
- Raw chain identifiers do not dominate the dashboard.
- Reward-cycle and burnchain state read as status rather than RPC field names.
