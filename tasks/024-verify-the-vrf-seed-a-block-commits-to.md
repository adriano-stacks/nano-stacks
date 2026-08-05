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

## What the wiring needs, having looked

`026-survive-a-bitcoin-reorganization` has settled, so the snapshot chain's shape
is no longer the blocker. Three inputs are, and only one of them exists today:

- **The tenure's sortition hash.** `SortitionSnapshot::sortition_hash` carries it
  already, from nano's own burnchain. Nothing has to be asked of a peer.
- **The winning leader key's VRF public key.** Not carried anywhere yet.
  `SortitionSnapshot` records `winner_txid` but not the key, and resolving one
  means following the winning block-commit's `key_block_ptr`/`key_vtxindex` to
  its leader-key registration — which the local sortition derivation already
  reads, so it is a matter of retaining it rather than of finding it.
- **The parent tenure's coinbase proof.** The validator has to keep the proof of
  every tenure it accepts, and a node starting from a checkpoint has no proof for
  the tenure before its first — so the checkpoint has to carry that one, or the
  first tenure's seed check has to be explicitly skipped once and said out loud.
  Skipping it quietly is the failure mode this whole group of tasks is about.

That points at `BitcoinBlockContext` gaining `sortition_hash` and
`winner_vrf_public_key`, and `ChainState` retaining the accepted proof. Both are
validation-only inputs — Clarity reads none of them — so neither moves a state
root, which makes this safe to land against a running replay. Six construction
sites for the context, plus the checkpoint field.

Also still true from the note above: `nano-miner`'s
`hacknet_sortition_hash_verifies_the_winning_vrf_proof` matches on the committed
block header hash, and several miners commit the same hash in one Bitcoin block,
so it should match on the txid.
