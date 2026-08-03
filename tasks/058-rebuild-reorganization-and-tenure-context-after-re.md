---
title: "Rebuild reorganization and tenure context after restart"
id: "058"
status: pending
priority: critical
type: bug
group: mainnet
tags: ["mainnet", "restart", "reorg", "headers"]
created_at: "2026-08-03"
effort: medium
dependencies: ["026", "041", "055", "056"]
---

# Rebuild reorganization and tenure context after restart

## Objective

Restore all derived chain context needed to execute and retract blocks after a
restart. The durable root and header table are not enough while
`tenure_start_heights` and the executed-chain retraction history remain
memory-only.

On reopen those collections are empty. Header backfill may return early when the
tip header exists without rebuilding the tenure-start map. The first
non-tenure-start block can then use its own height as the tenure start, and a
reorganization after restart has no complete executed suffix to retract.

## Tasks

- [ ] Persist or deterministically rebuild the canonical executed ancestry and
      tenure-start mapping before accepting another block.
- [ ] Rebuild derived context even when the tip header already exists; presence
      of one row must not short-circuit recovery.
- [ ] Verify every rebuilt parent, consensus hash and tenure boundary against
      durable chain data.
- [ ] Make historical-header persistence failures fatal to acceptance and
      recovery.
- [ ] Restart on a non-tenure-start block, execute its child, and compare all
      `get-tenure-info?` and block-header answers with uninterrupted execution.
- [ ] Restart immediately before a Bitcoin reorganization and a Stacks fork,
      retract the invalid suffix, and compare the resulting canonical state
      with uninterrupted execution.
- [ ] Keep reconstructed indexes bounded or disk-backed at mainnet depth.

## Acceptance Criteria

- Restarting at any accepted block reconstructs the same tenure start, parent
  chain and historical-header answers as the process that sealed it.
- A reorganization after restart retracts every invalid Stacks block and its
  auxiliary state without requiring a fresh checkpoint.
- Executing the first child after restart produces the same root, receipts and
  accounting as uninterrupted execution.
- Recovery work and memory are bounded independently of distance from the
  checkpoint.
