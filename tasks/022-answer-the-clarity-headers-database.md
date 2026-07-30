---
id: "022"
title: "Answer the Clarity headers database"
status: in-progress
priority: critical
effort: medium
type: feature
dependencies: []
tags: ["mainnet", "vm", "consensus"]
created_at: 2026-07-30
---

# Answer the Clarity headers database

## Objective

`nano-vm` passes `NULL_HEADER_DB` on every execution path, including the
production one (`crates/nano-vm/src/lib.rs:2084`). `BurnStateDB` is real —
`BitcoinContext` implements it — but the headers side answers `None` to
everything, with a regtest first block and a burn height of 1 as its only
non-empty replies.

So `get-stacks-block-info?`, `get-tenure-info?`, `get-burn-block-info?`, the
miner address, the VRF seed and the burn header hash are all wrong. The 600
captured blocks never ask; mainnet contracts ask constantly. W6.5 required this
and it is the piece of it that was not built.

## Tasks

- [ ] Keep the header fields Clarity can read for every block nano executes.
- [ ] Implement `HeadersDB` over that index and use it wherever `NULL_HEADER_DB`
      is passed today.
- [ ] Cross-check each accessor against stacks-core on the captured chain.
- [ ] Cover the tenure-height and stacks-height mappings, which are not header
      fields.

## Acceptance Criteria

- No production path constructs a `ClarityDatabase` with `NULL_HEADER_DB`.
- Every `HeadersDB` accessor returns the same value as stacks-core for every
  captured block.
- A contract calling `get-stacks-block-info?` and `get-tenure-info?` replays with
  matching receipts.
