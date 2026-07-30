---
id: "040"
title: "Let the conformance tests take any capture"
status: pending
priority: high
effort: medium
type: improvement
dependencies: []
tags: ["conformance", "fixtures"]
created_at: 2026-07-30
---

# Let the conformance tests take any capture

## Objective

A fixture recapture should be a command, not a rewrite. Today several tests
assume the shape of the capture that happens to be in the tree, so installing a
new one breaks them even when nano is right.

Replaying a capture taken from a Hacknet on the pinned revision reached
`replay depth 340/340`, state roots and receipts both full, while eleven unit
tests failed. Six were fixed by giving them the capture's accounting — now
`captured_chainstate` — and four causes remain:

- two tests built a chainstate with no accounting — **fixed**, both now take the
  capture's
- the capture wrote one `stacker_set` and a 340-block window spans more —
  **fixed**, it now writes every cycle the window spans
- the `ops_hash` disagreement at **Bitcoin height 305** was a real nano
  consensus bug, now **fixed**. The reference hash there is `e3b0c442…`, the
  hash of nothing: stacks-core accepted no operations in that block, while
  `missed_commits` holds exactly one commitment intended for it. A miner had
  committed late, and nano hashed every commitment it could parse. See the
  commit; the tests still need updating to hash accepted operations rather than
  parsed ones.

## Tasks

- [x] Give the two paths that took no accounting the capture's.
- [x] Capture a `stacker_set` for every cycle the window spans.
- [x] Find out why nano's operation hash differs from the reference at Bitcoin
      height 305 of the new capture — it was nano, and it is fixed.
- [ ] Hash accepted operations, not parsed ones, in
      `captured_bitcoin_blocks_match_the_recorded_operation_hashes` and
      `captured_sortition_snapshots_match_the_reference_bitcoin_chain`.
- [ ] Drive the remaining tests from the manifest and provenance rather than
      from heights that happen to be true of one capture.

## Acceptance Criteria

- A capture taken with a different height range and window installs with no
  test changes.
- `cargo xtask validate-fixtures` refuses a capture the tests could not consume,
  naming what is missing.
