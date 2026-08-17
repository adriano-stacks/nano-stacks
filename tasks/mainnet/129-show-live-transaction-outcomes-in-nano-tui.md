---
id: "129"
title: "Show live transaction outcomes in nano-tui"
status: completed
priority: medium
effort: medium
type: feature
group: mainnet
dependencies: ["126", "128"]
tags: ["tui", "explorer", "rpc", "events"]
touches: ["crates/nano-tui"]
created_at: 2026-08-14
verify:
  - type: bash
    run: "nix develop -c cargo test -p nano-tui"
  - type: bash
    run: "nix develop -c cargo clippy -p nano-tui --all-targets -- -D warnings"
completed_at: 2026-08-17
---

# Show live transaction outcomes in nano-tui

## Objective

Complete the explorer's intent-only transaction view with the result, events and
cost this node observed while executing live blocks.

This is delivery slice 4 from task 125's usability study.

## Tasks

- [x] Subscribe to the node's existing `/events` SSE stream without blocking the
      render loop or ordinary status refresh.
- [x] Join new-block transaction receipts to decoded transactions by block and
      transaction ID.
- [x] Show success, abort or VM error; committed result; emitted events; and all
      charged cost dimensions.
- [x] Present exact costs together with their share of the current limit.
- [x] Distinguish a known empty event list from an outcome that was not retained.
- [x] Keep event history bounded with the same lifetime and selection guarantees
      as block history.
- [x] Reconnect after stream loss and mark the uncovered interval rather than
      silently implying complete receipt history.
- [x] Cover successful, aborted, error, eventful, empty, missed and reconnected
      streams.
- [x] Run rustfmt, tests and strict clippy without warnings.

## Acceptance Criteria

- A live transaction's detail view distinguishes what it requested from what
  execution returned and committed.
- STX, fungible-token, NFT and contract events retain their transaction and event
  order and are readable without raw JSON.
- A backfilled block or missed SSE interval says the outcome is unavailable; it
  is never rendered as success or as zero events.
- Losing and restoring the event stream does not freeze navigation or discard
  decoded blocks.
- No receipt-specific archive or new RPC route is added until user validation
  demonstrates that live outcomes are insufficient.
