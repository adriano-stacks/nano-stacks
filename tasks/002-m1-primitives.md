---
id: "002"
title: "M1: implement primitive hashes, Uint256, and BitVec"
status: completed
priority: critical
effort: large
dependencies: ["001"]
tags: ["m1", "primitives"]
created_at: 2026-07-27
completed_at: 2026-07-27
---

# M1: implement primitive hashes, Uint256, and BitVec

## Objective

Provide the fixed-size values used by every consensus crate, with canonical
hashing, bounded bit-vector wire encoding, and integer-only arithmetic.

## Tasks

- [x] Add the `nano-primitives` crate and fixed-size hash newtypes.
- [x] Implement SHA-512/256, SHA-256, SHA-512, and HASH160 helpers.
- [x] Implement `Uint256` addition, subtraction, multiplication, division, and
  canonical big-endian conversion.
- [x] Implement bounded `BitVec` construction and wire encoding.
- [x] Add stacks-core differential tests for all primitive surfaces.

## Acceptance Criteria

- Unit and property tests cover the supported primitive operations.
- `cargo clippy --workspace --all-targets -- -D warnings` and the workspace test
  suite pass.
- Differential tests compare the same random inputs with stacks-core before the
  milestone is marked complete.

## The two gaps were subtraction and the byte orders

Every primitive surface this task built is compared against stacks-core on random
inputs — the five hashes, `Uint256` addition, multiplication and division, `BitVec`
set/get and its wire format — and two were missing: subtraction, and the canonical
byte conversions this task's own item list names.

Adding them found nothing wrong with nano and something worth knowing about the
oracle. `Uint256::to_u8_slice` is stacks-core's **little**-endian conversion and
`to_u8_slice_be` its big-endian one; the first assertion written here compared
nano's big-endian bytes against `to_u8_slice` and failed on the correct answer.
Both directions are asserted now, which is what makes the pair unmistakable.

Subtraction is asserted only where it does not underflow, deliberately:
`primitive_types` refuses an underflow and stacks-core's `Uint256` wraps, so the
two disagree there by construction and the proptest would be pinning a difference
that has no consensus meaning — nothing in the sortition arithmetic subtracts past
zero.
