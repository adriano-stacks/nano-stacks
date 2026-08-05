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

- [x] Persist or deterministically rebuild the canonical executed ancestry and
      tenure-start mapping before accepting another block.
- [x] Rebuild derived context even when the tip header already exists; presence
      of one row must not short-circuit recovery.
- [x] Verify every rebuilt parent, consensus hash and tenure boundary against
      durable chain data.
- [x] Make historical-header persistence failures fatal to acceptance and
      recovery.
- [x] Restart on a non-tenure-start block, execute its child, and compare all
      `get-tenure-info?` and block-header answers with uninterrupted execution.
- [ ] Restart immediately before a Bitcoin reorganization and a Stacks fork,
      retract the invalid suffix, and compare the resulting canonical state
      with uninterrupted execution.
- [x] Keep reconstructed indexes bounded or disk-backed at mainnet depth.

## Acceptance Criteria

- Restarting at any accepted block reconstructs the same tenure start, parent
  chain and historical-header answers as the process that sealed it.
- A reorganization after restart retracts every invalid Stacks block and its
  auxiliary state without requiring a fresh checkpoint.
- Executing the first child after restart produces the same root, receipts and
  accounting as uninterrupted execution.
- Recovery work and memory are bounded independently of distance from the
  checkpoint.

## Recovered rather than rebuilt

Nothing is re-derived. The context is written down with the block that produced
it — see [[057]] for the commit protocol — and a restart reads the row for the
block it resumes at. That is one `SELECT`, so recovery is bounded by nothing at
all, let alone by distance from the checkpoint.

Three findings shaped it:

- **The tenure-start map is recorded for every block, not only for tenure
  starts.** A node that begins mid-tenure — at a checkpoint, or at a restart —
  never sees that tenure's start block, and the answer it gives for it is the
  height of the first block of that tenure it *did* execute. The VM's own map
  already worked that way, first-write-wins; the ledger did not, so the map
  recovered was missing exactly the tenure in flight, which is the one being asked
  about. `note_executed_block` now `or_insert`s for every block and reads the
  entry back, so `ChainLedger::tenure_start_heights` and the VM's `tenure_starts`
  are the same map by construction. No uninterrupted execution changes: the VM
  answered this way already, and `header.tenure_start_height` is read by nothing
  but that map.
- **The VM's map has to be reseeded, not just the chainstate's.**
  `get_stacks_height_for_tenure_height` — `get-tenure-info?`, consensus-visible —
  answers from `BitcoinContext::tenure_starts`, which is memory. So
  `recover_ledger_at` pushes the recovered map into it.
- **Burn headers were memory-only too.** `get-burn-block-info?` answered `none`
  after a restart for any burn block outside the 32 the node re-seeds, where the
  run before it answered a hash. They are now a table in the side store, written as
  they are learned, because a Bitcoin block at a height is a fact about Bitcoin
  rather than about any Stacks block.

Bounding: `executed` is capped at `REORG_REACH` = 256, the same horizon
`nano-node`'s `RESUME_ANCESTORS` already fixes for finding a tip the network still
has. Beyond it there is nothing to walk back *to*, and unbounded it grew with
uptime — and is now serialized on every block. `chain_ledger` keeps the last 256
rows for the same reason: a restart whose tip lost a fork race stands on an
ancestor, and the ledger it stands on has to be that ancestor's, so
`recover_ledger_at` is given the block `resume_from` chose rather than the deepest
one sealed.

## Still open

The reorganization half. `retract` and `retract_to` mutate the ledger *outside* any
block commit, so between a fork switch and the next sealed block the row on disk
still describes the chain that was abandoned. A crash there is not incoherent —
`marf.tip()` is the abandoned block, the peer does not have it, `resume_from` walks
back and recovers *that* ancestor's ledger, which is the right one — but it has not
been tested, and the acceptance criterion asks for a restart immediately before a
Bitcoin reorganization and a Stacks fork compared against uninterrupted execution.
`fork_retraction.rs` covers the retraction in one process; the restart-then-retract
case needs a fixture with a fork in it.
