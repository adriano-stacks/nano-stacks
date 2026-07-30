---
id: "024"
title: "Verify the VRF seed a block commits to"
status: pending
priority: high
effort: small
type: feature
dependencies: []
tags: ["mainnet", "chainstate", "consensus"]
created_at: 2026-07-30
---

# Verify the VRF seed a block commits to

## Objective

`nano-crypto` proves and verifies VRF, and `nano-sortition` mixes the seed, but
`nano-chainstate` never checks one: the word `vrf` does not appear in the crate.
A nano follower therefore accepts a tenure-start block whose coinbase proof does
not correspond to the winning leader key, and accepts a `new seed` its commitment
did not derive.

stacks-core validates this before it will build on a block. nano has to as well,
or it will follow a chain the network will not.

## Tasks

- [ ] Resolve the winning leader key's VRF public key for the tenure being
      started.
- [ ] Verify the coinbase proof against that key and the parent's seed.
- [ ] Check the seed the commitment carries is the one the proof derives.
- [ ] Reject a tenure-start block that fails either check.

## Acceptance Criteria

- Every captured tenure-start block passes verification.
- A block with a tampered proof or seed is rejected with a distinct error.
- The fixture replay still reports depth 600/600.
