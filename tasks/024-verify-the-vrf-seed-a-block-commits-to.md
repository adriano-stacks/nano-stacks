---
id: "024"
title: "Verify the VRF seed a block commits to"
status: in-progress
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

- [x] Resolve the winning leader key's VRF public key for the tenure being
      started.
- [x] Verify the coinbase proof against that key and the parent's seed.
- [x] Check the seed the commitment carries is the one the proof derives.
- [ ] Reject a tenure-start block that fails either check.

## Acceptance Criteria

- Every captured tenure-start block passes verification.
- A block with a tampered proof or seed is rejected with a distinct error.
- The fixture replay still reports depth 600/600.

## Remaining

The rules are `nano_chainstate::{verify_coinbase_vrf_proof,
verify_committed_vrf_seed}` and every captured tenure is checked against both.
Nothing calls them on the follow path yet.

Wiring needs the tenure's sortition hash, and taking that from a peer would
mean trusting the peer for a validation input. It has to come from nano's own
`SnapshotChain`, which carries `sortition_hash` already, so this lands once
[[026-survive-a-bitcoin-reorganization]] settles the snapshot chain's shape.

Checking the committed seed also needs the parent tenure's coinbase proof,
which means the validator has to retain the proof of each tenure it accepts.

While proving the rules against the capture: several miners commit the same
block header hash in one Bitcoin block, so a sortition winner is identified by
its transaction, never by what it committed to. `nano-miner`'s
`hacknet_sortition_hash_verifies_the_winning_vrf_proof` still matches on the
committed hash and should move to the txid.
