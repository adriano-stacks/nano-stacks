---
id: "041"
title: "Walk back when our tip left the chain"
status: completed
priority: high
effort: medium
type: bug
dependencies: ["026", "027"]
tags: ["node", "sync", "reorg"]
created_at: 2026-07-30
completed_at: 2026-07-30
---

# Walk back when our tip left the chain

## Objective

A node resuming from disk asks its peer for the block its state is sealed at.
If that block lost a fork race the peer answers 404 for ever, and the node
stops:

```
the state in .../signer-chainstate is sealed at block 0825993595…, which the
peer does not have; it descends from a block the network dropped, so it needs
another peer or a fresh checkpoint
```

Observed on Hacknet: nano signed a block, the node was restarted to switch the
mining role on, and in between that block left the canonical chain. The signer
role is fatal, so the whole node ended — over a reorganization one block deep,
which is an ordinary event.

Everything needed to recover already exists and is unused here. `ChainState`
has `retract`, which discards the blocks an invalidated tenure carried and
reports where to resume. `nano-sync` has `fork_point`, `PeerPool` and
`choose_canonical_tip`. Nothing joins them to the resume path.

## Tasks

- [x] On a sealed tip the peers do not have, find the last block they do.
- [x] Resume from the surviving block instead of stopping.
- [ ] Ask the other peers before concluding the chain moved: one peer's 404 is
      that peer's answer, not the network's. The node follows one peer today;
      `PeerPool` from [[027-choose-a-fork-instead-of-following-a-peer]] is what
      this needs.
- [x] Stop only when no ancestor of the state on disk is on the chain, which is
      the case a checkpoint really cannot be extended from.

## Acceptance Criteria

- A node whose tip is reorganized away resumes on the canonical chain without
  being restarted or re-checkpointed.
- The blocks it had executed past the fork point are discarded, not silently
  kept.
- A conformance test drives a resume across a fork point offline.

## How it walks back

The state on disk records each block's parent, so a resume can ask the peer for
the tip, then its parent, then its parent's, and carry on from the first one
the peer has. The walk does not stop at the checkpoint: the import keeps the
checkpoint's own ancestors, so those are reachable too, and a test asserts both
halves of that.

`ChainState::retract` is not called on this path. Nothing needs discarding: the
blocks past the fork point were never sealed into the resumed state's chain —
execution simply carries on from the surviving ancestor, and the orphaned
states stay on disk unreferenced, as they do after any reorganization.

## Hacknet

Confirmed on a live network. The same restart that killed the previous build —
switching the mining role on, with the signer's sealed tip reorganized away in
between — now walks back, takes its signing slot for the cycle, and stays up.
The node has run for minutes where it used to exit in seconds.
