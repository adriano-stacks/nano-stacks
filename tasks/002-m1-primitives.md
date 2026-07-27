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
- [ ] Add stacks-core differential tests for all primitive surfaces.

## Acceptance Criteria

- Unit and property tests cover the supported primitive operations.
- `cargo clippy --workspace --all-targets -- -D warnings` and the workspace test
  suite pass.
- Differential tests compare the same random inputs with stacks-core before the
  milestone is marked complete.
