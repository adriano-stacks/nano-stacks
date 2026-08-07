---
id: "044"
group: mainnet
title: "Name a reward cycle nobody stacked for"
status: completed
priority: medium
effort: small
type: bug
dependencies: ["043"]
tags: ["chainstate", "signer", "hacknet"]
created_at: 2026-07-30
completed_at: 2026-07-30
---

# Name a reward cycle nobody stacked for

## Objective

Mining a tenure across the cycle 22 to 23 boundary failed:

```
advancing the tenure failed: checkpoint execution failed:
invalid transaction: signer set is empty
```

The first reading was that nano could not derive a signer set from a
checkpoint. That is wrong, and the way it was wrong is worth keeping.

## What it actually was

**Nothing stacked for cycle 23**, so the cycle has no signer set at all.

The peer says so: `/v3/stacker_set/22` returns a set and `/v3/stacker_set/23`
returns nothing. Hacknet's stacker says why:

```
PoX-5 staking updates are skipped during prepare phase
```

It never extended stacking into cycle 23 before the prepare phase began, and
during the prepare phase it refuses to — and the chain cannot leave the prepare
phase without a set. The network deadlocks against its own cadence.

With nano removed entirely and the stock participant restored, the Stacks tip
stayed at 858 while Bitcoin ran on. That is the diagnostic that settles it: the
stall is the network's, not nano's.

This very likely explains the three earlier runs that deadlocked on stock
signers — `Last accepted block has timed out`, `Cannot validate block, no
global signer state` — which were put down to flakiness at the time. A network
that outlives its stacking stops at the next cycle boundary, whoever is
signing.

## What was wrong in nano

Only the name. An absent reward set is not an invalid transaction: the chain is
well-formed and the block is honest, there is simply nobody to sign the cycle.
Reporting it as a consensus fault sent the diagnosis to the wrong place.

`ChainStateError::NoSignerSet(cycle)` now says

```
reward cycle 23 has no signer set: nothing stacked for it
```

It stays fatal, because a cycle no one signs cannot be extended into. Only the
diagnosis changed, not the behaviour.

## A second way it deadlocks

Rebuilding the network from genesis to validate later work hit a different
stall, and a harder one. The stock nodes wedge processing burn block 241,
looping:

```
Process burn block 241 reward cycle 12 ... is_rc_start: true
No PoX anchor block known yet for cycle 12
ERRO Missing canonical anchor block
```

Bitcoin ran on to 253 and past; the Stacks tip never left 8. **nano was not
running at all** — it had not been introduced yet — so this is stacks-core
failing to bootstrap its own chain, not a participant misbehaving.

Two distinct deadlocks now, then: this one at the first reward-cycle boundary
after Nakamoto on a fresh genesis, and the stacking-horizon one above on a
long-lived network. Between them a Hacknet run is only reliably usable for a
window in the middle, which is worth knowing before planning to validate on it.

## Tasks

- [x] Establish whether the empty set is nano's or the network's — it is the
      network's.
- [x] Report an unstacked cycle as itself rather than as an invalid
      transaction.
- [ ] Give the Hacknet harness a way to keep stacking ahead of the prepare
      phase, so a long run does not deadlock at a cycle boundary. `W13` already
      notes the stacker needed a pox-5 path; this is the rest of it.
- [ ] Find why a fresh genesis wedges on `Missing canonical anchor block` at
      the first cycle boundary, and whether the boot needs a longer pre-Nakamoto
      run or a prepared snapshot.

## Acceptance Criteria

- A cycle with no stackers is reported as such, naming the cycle.
- The distinction is visible in the log without reading the source.
