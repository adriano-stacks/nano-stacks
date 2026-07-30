---
id: "021"
title: "Hold mainnet-scale state on disk"
status: pending
priority: critical
effort: large
type: improvement
dependencies: []
tags: ["mainnet", "marf", "storage"]
created_at: 2026-07-30
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

- [ ] Back MARF nodes and the Clarity side store with durable storage instead of
      `BTreeMap` and a copied `SQLite` connection.
- [ ] Share unchanged subtries between versions rather than cloning per block.
- [ ] Bound the read path so a lookup does not walk the chain linearly.
- [ ] Persist sortition snapshots, tenure accounting and headers.
- [ ] Import a checkpoint without holding its blobs in memory.
- [ ] Recover the tip from disk on start, and replay only what is missing.

## Acceptance Criteria

- A node restarts and resumes from its persisted tip without re-importing.
- Memory stays flat while replaying the captured fixtures.
- The fixture replay still reports depth 600/600.
- Replaying from a checkpoint an order of magnitude larger than the Hacknet one
  completes within its own state's footprint.
