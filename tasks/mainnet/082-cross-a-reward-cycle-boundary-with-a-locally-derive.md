---
id: "082"
group: mainnet
title: "Cross a reward cycle boundary with a locally derived sortition chain"
status: in-progress
priority: critical
effort: large
dependencies: ["049", "077"]
tags: ["mainnet", "sortition", "consensus", "release"]
created_at: 2026-08-07
type: bug
---

# Cross a reward cycle boundary with a locally derived sortition chain

## Objective

A locally derived sortition chain stops dead at the first reward cycle boundary it
meets. `SortitionTracker::advance` refuses by design:

> burn N opens a reward cycle, which adds a bit to the `PoX` history the consensus
> hash mixes, and this node cannot yet say whether that cycle chose an anchor block

The refusal is right — a consensus hash mixes the `PoxId`, so guessing the bit
derives a wrong hash for every block after it, silently. What is missing is the
answer: whether the opening cycle selected a PoX anchor block, which is a fact about
this node's own executed state and not something to ask a peer for.

## Why this is critical rather than theoretical

Two places, and the second is the release.

**The conformance rigs.** `follow_path` and `catch_up_rounds` replay a capture
spanning burns 360–479 with a cycle length of 20, so the run crosses boundaries at
380, 400, 420, 440 and 460. Seeded at burn 360 the chain derives forward, executes
blocks 462–470, and stops at burn **379** — one short of the first boundary. These
rigs used to execute under the peer's `/v3/sortitions` answer, which is the path
[[077-remove-peer-derived-consensus-execution-fallbacks]] removed, so the limit was
invisible until the fallback went.

**The live mainnet follower.** It is on cycle 140 and `/v2/pox` puts cycle 141 about
713 burn blocks out. A node that cannot cross a boundary cannot hold tip through one,
which is exactly what [[053-pass-the-mainnet-node-release-gate]] requires: *"tracks
across ≥2 reward cycles incl. prepare/rollover"*. The 24-hour soak and the cycle-141
gates both sit on the far side of this.

## Tasks

- [ ] Derive whether an opening reward cycle selected a PoX anchor block from this
      node's own executed state, and extend the `PoxId` with the bit it implies.
- [ ] Decide it at the cycle's *prepare phase*, where stacks-core decides it, rather
      than at the boundary block — the answer has to be known before the first
      sortition of the new cycle is derived.
- [ ] Keep the refusal for the case that remains genuinely unanswerable, and say
      which of the two it is: a node that cannot yet decide, and a node whose state
      does not reach the prepare phase at all.
- [ ] Extend the captured-fixture gate across at least one boundary, so the rigs
      that lost their peer fallback replay the whole capture locally again.
- [ ] Cross a boundary on mainnet with the derived chain and compare the resulting
      consensus hashes against a stock node's for the whole cycle after it.

## The shape of the fix, measured rather than guessed

The mechanism already exists: `PoxId::extend_with_anchor(present: bool)`
(`nano-sortition`). Nothing needs inventing to *carry* the bit — what is missing is
the caller that decides it, and the tracker is the wrong place to decide it from
because it holds no chain state.

So the decision belongs to the executor, which has both: it derives the cycle's
reward set at the prepare phase (`signers::update_signer_set`, and
`ChainState::recorded_signer_set` reads one back), and it owns the tracker. The
executor tells the tracker the bit as the boundary is reached, and `advance` takes it
instead of refusing.

Two things to establish before writing it, and neither is a guess to make blind:

- **What "selected an anchor" means in epoch 4.0.** Every history observed so far is
  all ones — mainnet's is 142 bits of `1`, hacknet's 21 — so the bit has never been
  zero on a chain nano has seen. That is a reason to be careful rather than a licence
  to hardcode `true`: a cycle with no reward set is exactly the case the plan calls
  fatal, and the two must not be conflated.
- **When it is known.** stacks-core decides at the prepare phase, so the answer has
  to be in hand *before* the first sortition of the new cycle is derived — not at the
  boundary block itself, which is already too late for the consensus hash that mixes
  it.

## Measured: the reward-set rule is not the rule

Half of this is now in the tree and half is deliberately not.

**In:** the tracker can be *told*. `SortitionTracker::decide_anchor(opens_at,
selected)` records the bit, `anchor_decided` answers whether anything has, and
`advance` extends the `PoX` history with it at a boundary — or refuses exactly as
before when nothing decided. The refusal is kept on purpose: an undecided bit and a
bit decided wrongly produce the same wrong consensus hash, and only one of them says
so.

**Out, and this is the finding:** the obvious decider is wrong. "The cycle recorded a
signer set, therefore it selected an anchor" was implemented against
`ChainState::recorded_signer_set` and run on the captured fixture, which crosses five
boundaries. It crossed all five and answered **0 every time**, where the capture's own
`PoxId` is all ones:

```
the cycle opening at burn 380 selected no an anchor block, as this node's own
reward set for it says, so the PoX history the consensus hash mixes gains a 0
```

So it was reverted rather than left in. A node that crosses a boundary with a wrong
bit derives a wrong consensus hash for every block after it and reports nothing,
which is worse than the node that stops.

What that leaves for whoever picks this up: `recorded_signer_set` at the opening
height is either the wrong question or asked at the wrong height — the anchor is
chosen in the *previous* cycle's prepare phase, so the state to interrogate is the
one at the prepare phase, not at the boundary. That is the next thing to establish,
and there is now a mechanism waiting for the answer and a fixture that crosses five
boundaries to check it against.

## Acceptance Criteria

- A locally derived chain crosses a reward cycle boundary and derives the same
  consensus hashes, sortition identifiers and winners as stacks-core for the cycle
  that follows.
- `follow_path` and `catch_up_rounds` replay the whole capture with no peer
  sortition answer anywhere in the path.
- A node holds mainnet tip across a cycle rollover, which is the precondition for
  053's cycle-141 gates and its sustained soak.
- Where the bit genuinely cannot be decided, the node refuses with a message naming
  which of the two causes it is, and does not guess.

## Evidence that opened this task

2026-08-07. After [[077-remove-peer-derived-consensus-execution-fallbacks]] removed
the peer sortition fallback, seven conformance rigs failed. Seeding them from the
capture's own history closed the seeding half and left this: the chain derives
forward from burn 360, executes blocks 462–470, and stops at burn 379 with the
reward-cycle refusal above. The same refusal is what a mainnet follower will meet at
cycle 141.
