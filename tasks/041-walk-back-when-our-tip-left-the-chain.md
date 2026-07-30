---
id: "041"
title: "Walk back when our tip left the chain"
status: pending
priority: high
effort: medium
type: bug
dependencies: ["026", "027"]
tags: ["node", "sync", "reorg"]
created_at: 2026-07-30
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

- [ ] On a sealed tip the peers do not have, find the last block they do —
      `/v3/tenures/fork_info` gives the burn view both sides share.
- [ ] Hand `ChainState::retract` what the reorganization invalidated and resume
      from the surviving block instead of stopping.
- [ ] Ask the other peers before concluding the chain moved: one peer's 404 is
      that peer's answer, not the network's.
- [ ] Stop only when no peer has any ancestor of the state on disk, which is
      the case a checkpoint really cannot be extended from.

## Acceptance Criteria

- A node whose tip is reorganized away resumes on the canonical chain without
  being restarted or re-checkpointed.
- The blocks it had executed past the fork point are discarded, not silently
  kept.
- A conformance test drives a resume across a fork point offline.
