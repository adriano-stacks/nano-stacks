---
title: "Rebuild reorganization and tenure context after restart"
id: "058"
status: completed
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
- [x] Restart immediately before a Bitcoin reorganization and a Stacks fork,
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

## The reorganization half: writing the test found two bugs

The restart-then-retract case turned out not to need a fixture with a fork in it.
`retract_to` names an ancestor and a competing block is one the *test* makes: the
captured tenure-start block with one second added to its timestamp is a different
block sealing a different state over the same parent, which is exactly a fork. The
same block replayed is not, and the MARF refuses it — `VersionAlreadyExists`.

Writing it found two things a retraction did not give back. Both are invisible to
every state root, both were confirmed by putting the old code back, and neither
had anything to do with restarting: a retraction in one process was already wrong.

**The VM's tenure-start map is not keyed by branch.** `retract` rewound
`ChainLedger::tenure_start_heights` and left `BitcoinContext::tenure_starts` — the
map `get_stacks_height_for_tenure_height` answers `get-tenure-info?` from — holding
the abandoned branch's heights. Two branches can genuinely disagree about the
Stacks height a tenure height started at. Put back, the test reports
`Some(493)` where the retracted tenure must answer `None`.

**The parent tenure's coinbase proof was the abandoned tenure's.** Only the *last*
tenure's proof is kept, so nothing in a field-by-field rewind could restore the
surviving one. The consequence is not subtle: the honest tenure that replaces the
retracted one commits a seed that hashes to the tenure before it, so
`verify_committed_vrf_seed` refuses it —

```
InvalidTransaction("committed seed is not the hash of the parent tenure's VRF proof")
```

— and a node that reorganizes is stuck on a fork it has already left. That is the
error the fork test gets with the old rewind in place.

Both have one fix, and it is the same route a restart takes: `stand_on` reads back
the ledger the surviving block committed. Not a rewind at all. The four fields are
one row, that row is immutable and keyed by the block that wrote it, so the state
after a retraction *is* a state some block already sealed. There is no second
rewind to keep in step with the first, and the field that no rewind could reach
comes for free. `TenureAccounting::retract_from` survives only for the case where
every executed block is retracted and the chain goes back to the checkpoint, which
has no ledger row of its own.

Three tests in `restart.rs`, each run once in a single process and once across a
restart, demanding equality:

- `a_restart_before_a_bitcoin_reorganization_retracts_the_same_suffix` — retracts
  the last tenure the capture starts, and asserts the discarded suffix, the
  resumed block, both tenure-start maps and the proof directly, then compares the
  whole canonical value between the two runs.
- `a_restart_before_a_stacks_fork_reaches_the_same_canonical_state` — retracts to
  the block before that tenure and executes a competing tenure over it. The
  competing block's **state root** is the assertion: it is sealed after the
  retraction, so it reads the tenure heights and the accounting the retraction
  left, and a restart that recovered any of them differently seals a different
  root with every receipt matching.
- `a_retraction_leaves_the_disk_where_it_found_it` — pins the crash window rather
  than closing it. See below.

`Canonical` carries both tenure-start answers, the durable one and Clarity's, so
the two disagreeing is itself an inequality. And the direct assertions matter as
much as the comparison: a bug that moves neither map moves neither map in both
runs, and equality alone would be satisfied.

## The crash window after a retraction is pinned, not closed

`retract` writes nothing. It reads a row that was already there, so the disk after
a retraction is byte-identical to the disk before it, and a crash in the window
between a fork switch and the next sealed block leaves the abandoned chain —
`marf.tip()` is the abandoned block and the recovered ledger is its. `nano-node`
then walks back for a block the network still has and stands on that block's
ledger, which is how the switch is re-derived.

`a_retraction_leaves_the_disk_where_it_found_it` asserts exactly that, and the
fork test asserts `tip()` is still the abandoned block after the switch. Making the
retraction durable was considered and rejected: it would be a second durable
answer to "which chain am I on", beside the sortitions and the peers that decided
it, and reconciling two is the failure this group of tasks exists to remove.

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
  `recover_ledger_at` pushes the recovered map into it. It *replaces* rather than
  merges, which the retraction bug above is why: merging keeps answers belonging
  to a chain this node has left.
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
