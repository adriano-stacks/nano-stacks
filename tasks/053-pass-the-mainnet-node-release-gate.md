---
id: "053"
title: "Pass the mainnet node release gate"
status: pending
priority: critical
effort: medium
type: improvement
group: mainnet
dependencies: ["027", "037", "051", "052"]
tags: ["mainnet", "conformance", "release"]
created_at: 2026-08-02
---

# Pass the mainnet node release gate

## Objective

No component result or peer-facing height is enough to call nano a mainnet node.
Exercise the assembled binary from a fresh, attested checkpoint through catch-up
and steady state, with evidence tied to the durable executed chain.

## Tasks

- [ ] Bootstrap a clean state directory from an attested mainnet checkpoint.
- [ ] Catch up using a local Bitcoin source and multiple Stacks peers while
      recording every executed height and verified root.
- [ ] Restart during catch-up and at tip, then prove the same durable tip, root
      and tenure accounting are resumed.
- [ ] Remove and lie through one Stacks peer and prove neither event changes the
      canonical executed result.
- [ ] Exercise a Bitcoin reorganization and a Stacks fork switch.
- [ ] Run the stock signer/client-facing RPC and an event observer against the
      same executed chain.
- [ ] Hold mainnet tip for at least 24 hours across tenure and Bitcoin boundaries.
- [ ] Publish the exact commands, versions, checkpoint provenance and resulting
      conformance report.

## Acceptance Criteria

- Offline mainnet replay and receipt gates are green before the live run starts.
- The executed tip, not the followed tip, remains within the documented sync
  bound and survives restart.
- Every accepted block passed local burnchain, signer, miner, VRF and state-root
  validation.
- Peer failure, peer equivocation and ordinary reorganization do not stall or
  fork the node.
- RPC responses and events describe the same durable executed state.

