---
id: "022"
title: "Answer the Clarity headers database"
status: completed
priority: critical
effort: medium
type: feature
dependencies: []
tags: ["mainnet", "vm", "consensus"]
created_at: 2026-07-30
completed_at: 2026-07-30
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

- [x] Keep the header fields Clarity can read for every block nano executes.
- [x] Implement `HeadersDB` over that index and use it wherever `NULL_HEADER_DB`
      is passed today.
- [x] Cross-check each accessor against stacks-core on the captured chain.
- [x] Cover the tenure-height and stacks-height mappings, which are not header
      fields.

## Acceptance Criteria

- No production path constructs a `ClarityDatabase` with `NULL_HEADER_DB`.
- Every `HeadersDB` accessor returns the same value as stacks-core for every
  captured block.
- A contract calling `get-stacks-block-info?` and `get-tenure-info?` replays with
  matching receipts.

## Known limits

Two answers are still not what stacks-core would give, and both need work that
is not this task's:

- `block-reward` is what a tenure earned, and a tenure's reward is not known
  until it matures a hundred tenures later. nano records what the accounting
  holds at execution time, which is zero until then. Correcting it means
  revising a header record once its rewards mature.
- A block older than the checkpoint has no header record at all, so every
  accessor answers `none` for it where a node with full history answers. This
  is inherent to starting from a checkpoint; see
  [[031-establish-a-trust-root-for-the-checkpoint]].

`vm-epoch::epoch-version` is only ever written inside the rolled-back
transaction the cost tracker uses, so a store nano did not import reads as
Epoch 2.0. Production always imports a checkpoint that carries the right value,
but a genesis `Vm` does not, and Clarity takes its pre-3.0 branches there.
