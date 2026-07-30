---
id: "026"
title: "Survive a Bitcoin reorganization"
status: in-progress
priority: high
effort: medium
type: feature
dependencies: []
tags: ["mainnet", "burnchain"]
created_at: 2026-07-30
---

# Survive a Bitcoin reorganization

## Objective

`BitcoinRpcSource::block_at` walks heights forward and caches `PreStx` outputs
across a six-block window. It never asks whether the block it read last is still
the block at that height. `SnapshotChain` is a `Vec` in memory with no way to
retract a snapshot.

Hacknet's regtest chain does not reorganize. Mainnet does, routinely at one
block and occasionally deeper, and every sortition after the reorganized block is
then wrong: consensus hash, winner, total burn, and the seed that follows from
them.

## Tasks

- [x] Detect that a burn block nano processed is no longer canonical.
- [x] Retract the sortition snapshots that descend from it and reprocess.
- [x] Invalidate the `PreStx` pairings the retracted blocks contributed.
- [ ] Carry the Stacks state that descended from a retracted sortition back to a
      valid ancestor.
- [x] Cover a reorganization in the sortition conformance tests.

## Boundary

`SnapshotChain::retract_above` returns a `SortitionReorg`: the surviving
ancestor, the snapshots taken back, the Bitcoin height to read again, and
`invalidated_consensus_hashes()`. That is the whole burnchain side of the
rollback. Unwinding the Clarity state and the tenures those consensus hashes
carried belongs to `nano-chainstate`, which owns the MARF and the executed tip:
it has to discard every Stacks block whose consensus hash the reorganization
invalidated and re-execute from the last block under the surviving ancestor.

## Acceptance Criteria

- Replaying a burn range that reorganizes reaches the same snapshots as
  stacks-core.
- No retracted burn block leaves a snapshot, a pairing or a tenure behind.
- A reorganization deeper than the commitment window is handled or refused
  explicitly, never processed as if it had not happened.
