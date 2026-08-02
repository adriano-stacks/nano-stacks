---
id: "049"
title: "Derive canonical sortitions from the local burnchain"
status: pending
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
- [ ] Match the captured mainnet sortition window field for field.
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
