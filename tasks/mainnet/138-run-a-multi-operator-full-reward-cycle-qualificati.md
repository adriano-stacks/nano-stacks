---
id: "138"
title: "Run a multi-operator full-reward-cycle qualification"
status: pending
priority: critical
effort: large
dependencies: ["106"]
tags: ["mainnet", "release", "liveness", "operations", "conformance"]
created_at: 2026-08-14
parent: 053
type: chore
---

# Run a multi-operator full-reward-cycle qualification

## Objective

Extend the single-machine 24-hour hold into a qualification that spans a complete
mainnet reward-cycle rollover and following cycle across independent operators,
architectures, peers and Bitcoin backends using the exact frozen follower
artifact.

## Tasks

- [ ] Recruit at least three independent operators in distinct infrastructure
      and network failure domains, including x86-64 and AArch64 and independently
      administered Bitcoin nodes.
- [ ] Give each operator the signed artifact and independently reproducible
      checkpoint bundle; do not share mutable state directories or a hosted
      consensus data service.
- [ ] Start before a reward-cycle prepare phase and run continuously through the
      rollover and the complete following reward cycle.
- [ ] Record once per minute the Bitcoin, advertised, selected, followed and
      executed tips; roots/receipts; peer diversity; queue bytes/age/drops; RSS;
      disk/WAL; file descriptors; compile/cache behavior and RPC health.
- [ ] Compare every executed root, receipt, cost and event digest across all nano
      operators and the independent oracle while blocks arrive.
- [ ] Record every fork, rejection, peer change, restart and resource anomaly.
      Planned destructive recovery experiments remain outside each continuous
      evidence interval.
- [ ] Exercise loss of one peer, one Bitcoin backend and one optional edge
      service without changing the canonical result or requiring hosted HTTP.
- [ ] Publish a signed, machine-readable report bound to the artifact,
      checkpoints, operators, commands, raw samples and final verdict.

## Acceptance Criteria

- Every operator completes one continuous full-cycle interval with zero
  consensus-visible difference, process failure or hidden executed-tip lag.
- Root, receipt, cost and event digests agree across architectures and operators
  for every executed block.
- Resource, queue and disk measurements show no unbounded trend; peer diversity
  never collapses below the declared safety floor unnoticed.
- Loss of an allowed dependency degrades or reconnects explicitly without
  accepting peer-derived consensus context.
- The exact released artifact and checkpoint manifests are the ones measured and
  the full evidence bundle is independently verifiable.
