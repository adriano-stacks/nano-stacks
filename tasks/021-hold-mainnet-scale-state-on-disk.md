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
- [x] Persist sortition snapshots, tenure accounting and headers. These live in
      `nano-sortition` and `nano-chainstate`, which this change did not touch —
      all three are durable now, by the three tasks named below.
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

## All three are durable, and none of it was done here

The item is closed by what came after it rather than by anything in this task, so
it is worth naming which is which:

- **Sortition snapshots.** `SortitionTracker::save` writes the tip and the whole
  consensus-hash history in the capture's own format, as the chain advances rather
  than at shutdown, through a rename so a torn history cannot be left behind. On
  [[049-derive-canonical-sortitions-from-the-local-burncha]]. Without it a node
  re-derived from the checkpoint's burn anchor on every start, one Bitcoin block
  download per burn block.
- **Tenure accounting.** The whole ledger — executed chain, tenure start heights,
  earnings, reorganization reach, parent tenure proof — is written in the *same
  transaction* that seals the block's state root, so a hard kill leaves either the
  complete parent or the complete child. On
  [[057-commit-and-recover-accepted-block-state-atomically]], with the maturity
  window validated on recovery per [[048-carry-complete-mainnet-tenure-accounting]].
- **Headers.** `record_burn_header` and `backfill_block_header` write what Clarity
  reads back through `get-burn-block-info?` and `get-block-info?`, outside any
  block's commit for the burn headers because a Bitcoin fact is true before the
  block that reads it exists. On
  [[022-answer-the-clarity-headers-database]] and
  [[055-answer-block-info-for-blocks-before-the-checkpoint]].

What proves it rather than asserts it is the restart suite: twenty scattered
`SIGKILL`s and eight aimed at tenure transitions, each reopening with a ledger whose
executed suffix ends exactly at the tip that has state, replaying forward to the
same final root as an uninterrupted run.
