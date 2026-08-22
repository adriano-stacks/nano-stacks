---
id: "138"
title: "Run a multi-operator full-reward-cycle qualification"
status: cancelled
priority: critical
effort: large
dependencies: ["106"]
tags: ["mainnet", "release", "liveness", "operations", "conformance"]
created_at: 2026-08-14
parent: 053
type: chore
cancelled_at: 2026-08-23
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

## Disposition

Cancelled 2026-08-23 on the same project-owner direction that cancelled
[[139-complete-an-independent-mainnet-security-audit-and]]: nano-stacks is a
personal debugging project, and recruiting three independently administered
operators in distinct infrastructure and network failure domains is not
something it can execute. That direction also already redistributed
"full-cycle operator comparison" into 106 and 142, so this task's premise was
partly retired at that point.

Cancelling it must not drop the evidence it asked for that a single operator
*can* produce, so those parts move rather than disappear:

- **Continuous full-cycle interval** — a run started before a prepare phase and
  held through the rollover and the whole following cycle. Not covered by 106,
  which is 24 hours, nor by 082, which crosses one boundary. Moved to 142 as an
  explicit requirement.
- **Dependency-loss exercises** — losing a peer, a Bitcoin backend and an
  optional edge service without changing the canonical result or falling back to
  hosted HTTP. Moved to 142.
- **Per-minute sampling, digest comparison against an independent oracle, and
  anomaly recording** — already the hold apparatus in 106
  (`scripts/hold-follower-mainnet.sh`), which completed a clean 86,431-second
  interval over 6,243 blocks against two stock oracles.
- **Cross-architecture digest agreement** — already gated by 137's
  cross-architecture job, which compares x86-64 and AArch64 consensus-visible
  output on every push.
- **Signed machine-readable report bound to artifact and evidence** — already
  142's own final subtask.

What genuinely dies with this task is only the multi-operator, multi-failure-
domain dimension: independent administration, independent network position and
independent Bitcoin operation. That is a real reduction in assurance and is
recorded as such rather than absorbed silently — a single-operator qualification
cannot distinguish a fault correlated with this machine, this network path or
this Bitcoin node from a fault in nano. Reopen this task if nano-stacks ever
stops being a single-operator project.

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
