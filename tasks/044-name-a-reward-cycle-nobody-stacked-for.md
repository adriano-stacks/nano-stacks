---
id: "044"
group: mainnet
title: "Name a reward cycle nobody stacked for"
status: in-progress
priority: medium
effort: small
type: bug
dependencies: ["043"]
tags: ["chainstate", "signer", "hacknet"]
created_at: 2026-07-30
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
- [x] Give the Hacknet harness a way to keep stacking ahead of the prepare
      phase, so a long run does not deadlock at a cycle boundary. `W13` already
      notes the stacker needed a pox-5 path; this is the rest of it. The harness
      now locks for 12 cycles, reports the future pox-5 signer-set horizon, and
      has a `cycles` command that fails on a missing set or frozen Stacks tip.
- [ ] Find why a fresh genesis wedges on `Missing canonical anchor block` at
      the first cycle boundary, and whether the boot needs a longer pre-Nakamoto
      run or a prepared snapshot.

## Acceptance Criteria

- A cycle with no stackers is reported as such, naming the cycle.
- The distinction is visible in the log without reading the source.

## Harness boundary controls, 2026-08-08

The accounting worktree contained two ideas. Only the reward-cycle controls
belong here; its unrelated multi-network compose renderer was not ported.

`hacknet/harness.sh` now passes `STACKING_CYCLES=12` to Hacknet's existing
stacker. Twelve is pox-4's maximum and is accepted by pox-5. `stacking` queries
`get-signer-set-first-item-for-cycle` for each future cycle and names the first
unprepared boundary. `cycles` crosses live boundaries and requires both a moving
Stacks tip and a non-empty `/v3/stacker_set` on the far side. The offline shell
test pins the compose inputs, Clarity uint encoding, horizon diagnosis and
reward-set summary without starting Docker.

The earlier-Nakamoto idea is still a hypothesis. The harness passes the four
epoch heights as overrides but keeps Hacknet's defaults. The README gives the
`222..225` diagnostic command, followed by `cycles 2`; until that stock-only run
crosses the boundary, the fresh-genesis item above stays open.

## The stock coordinator confirms the missing condition

The pinned stacks-core source removes the ambiguity in the log. For a
post-Epoch30 cycle, `load_nakamoto_reward_set` enumerates the preceding prepare
phase's sortitions and selects the first one for which chainstate can read a
processed tenure-start or Stacks header. With no such header it returns `None`;
the burn-block coordinator turns that exact answer into `Missing canonical
anchor block`.

That is the retained state measured above: the prepare window contained no
processed tenure start. It is not a missing signer set, a different PoX bit or
an anchor-selection arithmetic error. stacks-core's own consensus harness also
mines out of a pre-Nakamoto prepare phase before switching block formats, with
the comment that otherwise it may fail to calculate the PoX anchor.

The proposed control therefore changes the three inputs that can make the
required header exist: Nakamoto begins earlier, Bitcoin waits 30 seconds per
block, and the prepare window is ten blocks. Source inspection proves that this
targets the refusal; only the fresh stock-only run can prove that the timing is
sufficient, so the task remains open until `cycles 2` crosses it.
