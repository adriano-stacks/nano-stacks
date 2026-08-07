---
id: "004"
group: build
title: "M3: implement Stacks and PoX addresses"
status: completed
priority: high
effort: medium
dependencies: ["002"]
tags: ["m3", "address"]
created_at: 2026-07-27
completed_at: 2026-07-27
---

# M3: implement Stacks and PoX addresses

## Objective

Provide Stacks and PoX address encodings and the sBTC Taproot output-key
derivation with independently checked protocol vectors.

## Tasks

- [x] Use maintained c32, Base58Check, and Bitcoin address implementations.
- [x] Implement Stacks address parsing and formatting for every valid version byte.
- [x] Implement standard, P2WPKH, P2WSH, and P2TR PoX address forms.
- [x] Implement the PoX-5 sBTC Taproot output-key specialization.
- [x] Add stacks-core differential tests and a published sBTC derivation vector.

## Acceptance Criteria

- C32 address encoding matches stacks-core for all version bytes and random hash160 values.
- All PoX output variants render to the same Bitcoin address as stacks-core.
- The sBTC Taproot derivation matches a published reference vector.
