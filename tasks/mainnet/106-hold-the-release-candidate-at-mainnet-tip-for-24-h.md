---
title: "Hold the release candidate at mainnet tip for 24 hours"
id: "106"
status: pending
priority: critical
effort: large
type: chore
group: mainnet
parent: "053"
dependencies: ["037", "046", "047", "054", "079", "082", "107"]
tags: ["mainnet", "liveness", "operations", "release"]
created_at: "2026-08-09"
---

# Hold the release candidate at mainnet tip for 24 hours

## Description

Hold the same release binary at the public mainnet tip for one continuous
24-hour interval. Use P2P only for synchronization and liveness.

## Tasks

- [ ] Start the hold only after the clean replay in task 037 and the no-hosted
  P2P qualification in task 054 pass for the same release binary.
- [ ] Run with no hosted data service for 24 continuous hours.
- [ ] Sample once per minute: Bitcoin tip, selected Stacks tip, followed tip,
  executed tip, peer count, queue depths, disk use, memory use, open file count,
  RPC health, and observer backlog.
- [ ] Record every peer change, fork, restart, and block rejection.
- [ ] Compare each new executed state root and receipt set with the oracle.
- [ ] Confirm that RPC and P2P service never expose staged or unexecuted data.
- [ ] Check resource measurements for an unbounded trend.
- [ ] Restart the complete 24-hour measurement after a node defect or process
  stop. Keep planned recovery tests outside the continuous interval.

## Evidence

- Start and end times in UTC.
- One-minute health and resource samples.
- Per-block root and receipt comparisons.
- Peer, fork, rejection, and service logs.
- Final selected, followed, executed, and network tip values.

## Acceptance criteria

- One continuous 24-hour interval has no process failure or consensus
  difference.
- The node remains within the stated catch-up bound after each new block.
- Selected, followed, and executed tips do not hide persistent lag.
- Resource use has no unbounded trend.
- All served data was validated and executed locally.
