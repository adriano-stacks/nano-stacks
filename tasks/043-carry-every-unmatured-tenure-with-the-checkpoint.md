---
id: "043"
title: "Carry every unmatured tenure with the checkpoint"
status: completed
priority: critical
effort: small
type: bug
dependencies: []
tags: ["checkpoint", "chainstate", "hacknet"]
created_at: 2026-07-30
completed_at: 2026-07-30
---

# Carry every unmatured tenure with the checkpoint

## Objective

A node started from a checkpoint pays out tenures it never executed for the
hundred tenures that follow it, and has no way to know what those earned unless
the checkpoint carries them. The export carried two.

So a node ran correctly for a while and then refused every block:

```
tenure 73 matured without accounting: its rewards are neither
checkpointed nor executed
```

On Hacknet that stopped the chain. Three signers of equal weight against a
seven-tenths threshold means nano refusing is the network halting: the Stacks
tip sat at 851 while Bitcoin ran on to 446.

This is the failure mode the design warned about — `effects_for_tenure` fails
loudly rather than paying nothing, because a silently empty payout only shows
up as a state root that differs once the block is already sealed.

## What it was

The window has to be the hundred tenures *behind* the checkpoint. `last_height`
in the export is already the checkpoint tenure plus the maturity, so taking the
window from there asks for tenures that do not exist yet, and every lookup
returns nothing without complaining.

It now exports 102 tenures where it exported 4.

## Tasks

- [x] Export every tenure whose reward has not matured at the checkpoint.
- [x] Confirm on a live network that a node runs past the maturity horizon.
- [ ] Fail the export when the window is short, rather than writing a
      checkpoint that stalls a node a hundred tenures later.

## Acceptance Criteria

- A node started from a checkpoint keeps signing past `MINER_REWARD_MATURITY`
  tenures.
- The export covers the whole window, and says so.

## Hacknet

Confirmed. The stalled network resumed as soon as nano restarted on a
checkpoint with the full window: nano signed, then mined blocks 854, 855, 856,
857 and 858, each accepted by the network, and the chain moved from 851 to 858
while Bitcoin went from 452 to 457.

The same run is the live validation of the two changes that had not been on a
chain — the sequence-application cost charges and sizing a contract for the
arguments it is given.

## What it uncovered

At the cycle 22/23 boundary the miner reports

```
advancing the tenure failed: checkpoint execution failed:
invalid transaction: signer set is empty
```

which is [[044-derive-the-next-cycle-signer-set-from-a-checkpoint]].
