---
id: "146"
title: "Close the trait-dispatch cost differential against the canonical record"
status: pending
priority: critical
effort: large
dependencies: ["097", "098"]
tags: ["mainnet", "vm", "costs", "conformance", "release"]
created_at: 2026-08-21
type: bug
---

# Close the trait-dispatch cost differential against the canonical record

## Objective

nano charges more than the canonical stacks-core record for at least one
mainnet trait-dispatch contract call, on three of the five cost dimensions,
while sealing the identical state root. Costs are consensus-visible near the
block limits, so per the plan's own rule — every cost differential blocks
release, whether or not a current block hits it — this is open until the
accounting matches or the difference is proven to be the oracle's.

## The measurement that opened this

2026-08-21, from the completed 24-hour hold's deferred receipt verification.
Block 8,808,752 (lone transaction `0xfc71c88f…d604`, a successful
`contract-call?` to `SPNWZ5V2TPWGQGVDR6T7B6RQ4XMGZ4PXTEE0VQ0S.change-price-v1
::change-price-a`):

| dimension | nano (16e0928a follower AND witness, and the older dc447744 node) | canonical (Hiro record of stacks-core) | difference |
|---|---|---|---|
| read_count | 30 | 30 | 0 |
| read_length | 77,622 | 76,653 | **+969** |
| runtime | 165,496 | 161,448 | **+4,048** |
| write_count | 4 | 4 | 0 |
| write_length | 901 | 341 | **+560** |

Three independent facts bound the defect:

- **The root matched.** The hold verified this block's consensus identity,
  including the sealed state root, byte-for-byte against two independent
  stock nodes. The durable writes are identical; only nano's *accounting* of
  lengths and runtime differs.
- **Both nano lineages agree with each other.** The frozen 16e0928a follower,
  the same-revision witness and the older pre-map-write-cost-fix node all
  report the identical inflated figures, so this is not the recent compiler
  change — it is long-standing.
- **It is path-specific.** The same field-by-field verifier passed complete
  blocks elsewhere in the window (e.g. 8,800,750), and hacknet's stock-node
  receipt comparisons have always matched on all five dimensions — hacknet
  traffic never exercised this shape.

The contract is trait-heavy: `change-price-a` dynamically dispatches
`unlist-asset` and `list-asset` through trait references into two other
marketplace contracts, which is the cost surface tasks 097 and 098 worked —
this looks like a residual on the *charging* side of dynamic dispatch
(callee load sizes and write-length accounting), not the semantic side.

## Why self-checks never saw it

The mainnet receipt gates replay against payloads nano's own observer
recorded, which proves determinism, not canonical agreement. The hacknet
gates do compare against stock observers, but never ran this call shape. The
hold's deferred verifier is the first mainnet-scale field-by-field
cost comparison against the canonical record — and it caught this on its
second block.

## Tasks

- [ ] Complete the block-level cost sweep over the hold window (in flight at
      creation time: `/home/aldur/mainnet-hold/cost-mismatches.jsonl`) and
      classify every mismatching block by contract and call shape.
- [ ] Reproduce the differential offline through the dual-engine tooling on a
      state snapshot, dimension by dimension, and localize the charging site
      (dynamic callee load, trait argument casting, write-length measurement).
- [ ] Fix the accounting in the nano-owned cost path; the interpreter remains
      a dev-only oracle.
- [ ] Add the reproduced call shape to the conformance corpus so the gate
      that catches it cannot skip itself, and re-run the mainnet cost sweep
      to zero mismatches.
- [ ] Re-run a hold window's deferred receipt verification end to end green,
      which is what unblocks task 106.

## Acceptance Criteria

- Every cost dimension of every transaction in a verified mainnet window
  matches the canonical record exactly.
- The differential's call shape is a permanent regression test.
- No production path consults the interpreter to compute or repair costs.
