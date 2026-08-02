---
id: "050"
title: "Authenticate every followed Nakamoto block"
status: pending
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
- [ ] Enforce header version, `bitcoin_spent`, PoX treatment and problematic
      transaction rules for the active epoch.
- [ ] Enforce transaction version, chain ID, network and anchor-mode constraints
      on followed blocks, not only in the mempool.
- [ ] Reject before beginning VM execution and return a distinct consensus error.

## Acceptance Criteria

- Every captured mainnet block passes the complete validator before replay.
- Mutating each authenticated field produces a focused rejection test.
- No signer, miner, VRF or sortition validation input comes from the peer that
  supplied the candidate block.
- A block with a self-consistent state root but invalid consensus authentication
  is never sealed.
