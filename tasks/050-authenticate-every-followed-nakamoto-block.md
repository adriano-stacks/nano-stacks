---
id: "050"
title: "Authenticate every followed Nakamoto block"
status: in-progress
priority: critical
effort: large
type: feature
group: mainnet
dependencies: ["024", "049"]
tags: ["mainnet", "chainstate", "consensus"]
created_at: 2026-08-02
---

# Authenticate every followed Nakamoto block

## Objective

The follow path currently checks block decoding, Merkle root, parent, height,
timestamp and eventual state root. It does not verify signer weight, the miner
signature, the winning leader, VRF proof or committed seed. Several decoded
consensus fields and transaction network constraints are also not checked.

Put one validation boundary before execution, supplied only by nano's executed
state and local burn view.

## Tasks

- [ ] Resolve the active reward set from executed state and verify ordered signer
      signatures and threshold weight.
- [ ] Verify the miner signature against the local sortition winner and leader
      key.
- [ ] Finish [[024-verify-the-vrf-seed-a-block-commits-to]] on this path.
- [ ] Validate tenure-change and coinbase semantics against the local snapshot.
- [x] Enforce the header version for the active epoch.
- [ ] Enforce `bitcoin_spent`, PoX treatment and problematic transaction rules.
- [x] Enforce transaction version, chain ID, network and anchor-mode constraints
      on followed blocks, not only in the mempool.
- [x] Reject before beginning VM execution and return a distinct consensus error.

## Acceptance Criteria

- Every captured mainnet block passes the complete validator before replay.
- Mutating each authenticated field produces a focused rejection test.
- No signer, miner, VRF or sortition validation input comes from the peer that
  supplied the candidate block.
- A block with a self-consistent state root but invalid consensus authentication
  is never sealed.

## One boundary, before anything runs

`ChainState::authenticate_block` is that boundary, called from
`execute_nakamoto_block` before the VM is touched. It answers only from this
node's own network configuration — nothing is asked of the peer that supplied
the block — and returns `ConsensusError`, which is distinct from an execution
failure so a caller can tell "not our chain" from "did not compute".

It checks the epoch's header version, ignoring the shadow flag above it, and per
transaction: the version byte's network, the chain identifier, and that the
anchor mode is not off-chain, which names microblocks that 4.0 does not have.

None of these is something a state root would catch. A node that executes them
computes a perfectly self-consistent state for a chain nobody else is on, which
is the whole reason they belong before execution rather than after.

`tests/block_authentication.rs` gives each its own rejection, mutating a real
captured block — the transaction cases by changing a byte and decoding again,
which is also what arriving from a peer looks like — and pins that a block the
network accepted still authenticates, with the shadow flag set or not. Mainnet
replay to 8,666,422 raises none of them.

Still to do here: signer weight and ordering, the miner signature against the
local sortition winner, the VRF seed ([[024-verify-the-vrf-seed-a-block-commits-to]]),
tenure-change and coinbase semantics, `bitcoin_spent` and PoX treatment.

