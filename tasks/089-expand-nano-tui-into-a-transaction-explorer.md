---
title: "Expand nano-tui into a transaction explorer"
id: "089"
status: completed
priority: high
effort: medium
type: feature
group: mainnet
dependencies: []
tags: ["tui", "rpc", "explorer"]
touches: ["crates/nano-tui", "Cargo.lock"]
created_at: "2026-08-07"
verify:
  - type: bash
    run: "nix develop -c cargo test -p nano-tui"
  - type: bash
    run: "nix develop -c cargo clippy -p nano-tui --all-targets -- -D warnings"
completed_at: 2026-08-07
---

# Expand nano-tui into a transaction explorer

## Objective

Turn the block dashboard into a navigable block and transaction explorer. A
contract-call transaction must show the exact contract, public function and
decoded Clarity arguments that the node read from the consensus block bytes.

## Tasks

- [x] Preserve transaction authorization and payload details while decoding blocks.
- [x] Add block -> transaction navigation with an explicit back path.
- [x] Render full transaction metadata and payload-specific fields.
- [x] Decode and render every contract-call argument as a Clarity value.
- [x] Support scrolling when a transaction is taller than the terminal.
- [x] Cover decoding, formatting and navigation with tests.
- [x] Run rustfmt, tests and clippy without warnings.

## Acceptance Criteria

- Opening a block presents a selectable transaction list.
- Opening a contract call shows its contract identifier, function name and every
  argument value rather than only an argument count.
- The transaction view shows txid, sender, sponsor where present, nonce, fee,
  authorization, anchor mode and post-condition policy.
- Transfer, deployment, tenure-change, coinbase and poison payloads retain useful
  payload-specific details.
- Long transaction details can be scrolled without changing the selected block or
  transaction.
- `cargo test -p nano-tui` and `cargo clippy -p nano-tui --all-targets -- -D warnings`
  pass.
