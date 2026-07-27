---
id: "006"
title: "M5: implement Bitcoin operation parsing"
status: completed
priority: high
effort: large
dependencies: ["005"]
tags: ["m5", "bitcoin"]
created_at: 2026-07-27
completed_at: 2026-07-27
---

# M5: implement Bitcoin operation parsing

## Objective

Decode raw Bitcoin blocks and classify the protocol operations needed by
sortition without a production dependency on Stacks-Core.

## Tasks

- [x] Decode Bitcoin consensus blocks with rust-bitcoin.
- [x] Require a standard output-zero `OP_RETURN` packet with Hacknet's `T3`
  marker before classifying an operation.
- [x] Parse the fixed operation packet fields for all epoch-4 operation kinds.
- [x] Validate transaction inputs/outputs and resolve PreStx sender pairings.
- [x] Differentially compare every captured operation and field against the
  reference parser.

## Acceptance Criteria

- Every captured raw Bitcoin block decodes with Hacknet's protocol marker.
- Operation sets and fields match the reference parser for every fixture block.
