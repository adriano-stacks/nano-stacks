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

- `checkpoint_executor_executes_captured_descendants` builds a `Checkpoint`
  with `accounting: None`, and
  `a_chainstate_reopened_between_blocks_matches_one_that_never_closed` uses
  `ChainState::open_from_checkpoint`, which takes no accounting at all. Both
  then fail the moment a tenure earned before the window matures.
- `captured_bitcoin_blocks_match_the_recorded_operation_hashes` and
  `captured_sortition_snapshots_match_the_reference_bitcoin_chain` disagree with
  the reference at the first captured burn height. A window that opens mid-chain
  has no `PreStx` pairings from the six blocks before it, which the operation
  hash depends on.
- `captured_blocks_have_the_expected_signer_weight` cannot read a block whose
  reward cycle the capture did not write a `stacker_set` for — the capture
  records one cycle and a 340-block window spans more.

## Tasks

- [ ] Give `ChainState::open_from_checkpoint` and `Checkpoint` a way to carry
      the accounting a mid-chain window needs.
- [ ] Capture the burn blocks before the first replayed one, so `PreStx`
      pairing has the window it needs, or record the pairings themselves.
- [ ] Capture a `stacker_set` for every cycle the window spans.
- [ ] Drive the remaining tests from the manifest and provenance rather than
      from heights that happen to be true of one capture.

## Acceptance Criteria

- A capture taken with a different height range and window installs with no
  test changes.
- `cargo xtask validate-fixtures` refuses a capture the tests could not consume,
  naming what is missing.
