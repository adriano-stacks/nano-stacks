---
id: "021"
title: "Hold mainnet-scale state on disk"
status: completed
priority: critical
effort: large
type: improvement
dependencies: []
tags: ["mainnet", "marf", "storage"]
created_at: 2026-07-30
completed_at: 2026-07-30
---

# Hold mainnet-scale state on disk

## Objective

Nothing nano computes survives a restart, and nothing it holds is bounded. The
plan left storage free-form on purpose; mainnet is where that comes due.

What the current shape does:

- `crates/nano-marf/src/lib.rs:627` deep-clones the entire trie for every block.
- `MarfStore.states` (`crates/nano-vm/src/lib.rs:643`) keeps every block's write
  set in memory forever, and a read walks the parent chain block by block.
- `import_checkpoint` reads the whole blob file with `fs::read` and loads the
  full node graph into memory.
- Only signer slot versions and the miner's sortition hash are persisted.
  Restarting means re-importing the checkpoint and replaying every block since.

The Hacknet checkpoint is 8.3 MB at Stacks height 400. Mainnet chainstate is tens
of gigabytes and grows every tenure.

## Tasks

- [x] Back MARF nodes and the Clarity side store with durable storage instead of
      `BTreeMap` and a copied `SQLite` connection.
- [x] Share unchanged subtries between versions rather than cloning per block.
- [x] Bound the read path so a lookup does not walk the chain linearly.
- [ ] Persist sortition snapshots, tenure accounting and headers. These live in
      `nano-sortition` and `nano-chainstate`, which this change did not touch.
- [x] Import a checkpoint without holding its blobs in memory.
- [x] Recover the tip from disk on start, and replay only what is missing.

## Acceptance Criteria

- A node restarts and resumes from its persisted tip without re-importing.
- Memory stays flat while replaying the captured fixtures.
- The fixture replay still reports depth 600/600.
- Replaying from a checkpoint an order of magnitude larger than the Hacknet one
  completes within its own state's footprint.

## Notes

`ChainState::open_from_checkpoint` now forwards to `Vm::open_from_checkpoint`,
so the durable path is on the route a node takes, and a test closes and reopens
the chainstate between blocks to prove a restart is not a silent fork. What is
left is a chainstate directory in the node's configuration, which belongs to
[[030-ship-one-node-binary-with-a-configuration-file]].

Sortition snapshots, tenure accounting and the header index are still memory
only; they live in `nano-sortition` and `nano-chainstate`. So is
`BitcoinContext.headers`, which grows a record per executed block.
