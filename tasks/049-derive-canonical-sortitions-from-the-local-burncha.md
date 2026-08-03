---
id: "049"
title: "Derive canonical sortitions from the local burnchain"
status: in-progress
priority: critical
effort: large
type: feature
group: mainnet
dependencies: ["026"]
tags: ["mainnet", "burnchain", "consensus"]
created_at: 2026-08-02
---

# Derive canonical sortitions from the local burnchain

## Objective

The production executor asks its one Stacks peer for `/v3/sortitions` and uses
that answer as the Bitcoin height and tenure context. `nano-node` does not depend
on `nano-sortition`, although it already downloads the raw Bitcoin blocks. The
peer therefore chooses nano's consensus hashes, winners and canonical fork.

Run `SnapshotChain` in the node and derive those facts from the configured
Bitcoin source. Peer sortition responses may be diagnostics or download hints,
never validation inputs.

## Tasks

- [ ] Feed locally decoded Bitcoin operations into a persistent `SnapshotChain`.
- [ ] Derive consensus hash, sortition hash, winning commit transaction, leader
      key, total burn and accumulated coinbase locally.
- [~] Match the captured mainnet sortition window field for field — ten of
      fifteen do, and the eleventh is the epoch 4.0 boundary.
- [ ] Hand the local snapshot to block validation and execution.
- [ ] Persist snapshots and resume without trusting a peer's current burn view.
- [ ] Apply [[026-survive-a-bitcoin-reorganization]] to the production burnchain
      path and replay the affected Stacks tenures.

## Acceptance Criteria

- Removing `/v3/sortitions` access does not stop a node with a Bitcoin source.
- Tampered peer sortition data cannot change the selected or executed chain.
- Mainnet captures match stacks-core for every consensus-visible snapshot field.
- A Bitcoin reorganization selects the same surviving snapshot and Stacks fork
  as stacks-core after restart as well as in-process.

## Ten mainnet sortitions derive exactly

`crates/nano-conformance/tests/mainnet_sortition.rs` replays a captured window
of mainnet snapshots from the raw Bitcoin blocks beneath them, taking only the
first as given. For **ten consecutive burn blocks** nano finds the same
operations, hashes them to the same `ops_hash`, identifies the same winning
commitment among them, and chains the same `sortition_hash` from one to the
next — none of it asked of a peer.

It diverges at burn **960,230**, on the operations hash. That is the epoch 4.0
boundary, where the set of transactions that count as burnchain operations
changes, so it is the right place for the first disagreement and a precise thing
to fix rather than a vague one.

The consensus hash is not checked by this test and cannot be: it mixes prior
consensus hashes at power-of-two offsets reaching back thousands of blocks, so
it needs a chain replayed from its own genesis rather than a slice of one. That
is what a node building the chain itself will exercise.

The test is a depth gate, like replay depth: ten is the floor already reached,
and raising it is the measure of progress.
