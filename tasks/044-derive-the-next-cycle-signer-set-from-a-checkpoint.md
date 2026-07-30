---
id: "044"
title: "Derive the next cycle's signer set from a checkpoint"
status: pending
priority: high
effort: medium
type: bug
dependencies: ["043"]
tags: ["chainstate", "signer", "hacknet"]
created_at: 2026-07-30
---

# Derive the next cycle's signer set from a checkpoint

## Objective

Mining a tenure across a reward cycle boundary fails:

```
advancing the tenure failed: checkpoint execution failed:
invalid transaction: signer set is empty
```

Observed on Hacknet at the cycle 22 to 23 boundary, on a node running from a
checkpoint. An empty signer set is fatal by design — the plan calls for
asserting it at startup — so producing one here stops the node from taking the
tenures it won.

Signing and following are unaffected: the same node signed and mined through
cycle 22, and the network accepted every block. Only the cycle rollover fails.

## Tasks

- [ ] Find whether the set is empty because the pox-5 linked list is unreachable
      from the checkpoint's state, or because the cycle it is asked for is wrong.
- [ ] Derive the set for the cycle the tenure belongs to, not the tip's.
- [ ] Cover a rollover offline, so this does not need a live network to catch.

## Acceptance Criteria

- A node from a checkpoint mines across a reward cycle boundary.
- A conformance test drives a rollover from an imported checkpoint.
