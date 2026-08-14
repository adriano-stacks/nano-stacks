---
id: "082"
group: mainnet
title: "Cross a reward cycle boundary with a locally derived sortition chain"
status: in-progress
priority: critical
effort: large
dependencies: ["049", "077", "122"]
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

- [x] Derive whether an opening reward cycle selected a PoX anchor block from this
      node's own executed state, and extend the `PoxId` with the bit it implies.
- [x] Decide it at the cycle's *prepare phase*, where stacks-core decides it, rather
      than at the boundary block — the answer has to be known before the first
      sortition of the new cycle is derived.
- [x] Keep the refusal for the case that remains genuinely unanswerable, and say
      which of the two it is: a node that cannot yet decide, and a node whose state
      does not reach the prepare phase at all.
- [x] Derive the captured Bitcoin snapshots across a boundary without a peer.
      Both execution rigs now replay the complete 340-block capture across all
      five boundaries and assert that no peer sortition route was called.
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

**It was asked at the wrong height, and that is now fixed.**
`CheckpointExecutor::recorded_signer_set` moves the context to *the executor's own*
burn height before reading — so it answers "the cycle I am in", and the question here
is about a cycle ahead of it. Every one of those five zeroes was that override, not
the rule. The decider asks `ChainState::recorded_signer_set` directly now, at the
opening height.

**And it fails closed, which is why it is safe to keep.** It decides only when the
chainstate positively reports a non-empty signer set for that cycle; anything else
leaves the bit undecided and the chain refuses exactly as before. Re-run against the
fixture, it decides nothing and stops at burn 379 — the capture's state cannot answer
`get-signers` for the opening cycle — so the rigs are still red and the node is still
correct.

What that leaves is **verification**: no state available here can answer, so the rule
"a recorded signer set means an anchor was selected" is reasoned rather than
measured. The check that would settle it is a live one — mainnet's `PoxId` is 142
bits of `1` and the node holds cycle 140's signer set, so asking the decider about
cycle 140's opening height must answer `true`. That needs a healthy mainnet state,
which is the other thing outstanding.

## Four verification routes, all exhausted, 2026-08-07

The rule "a recorded signer set for a cycle means that cycle selected an anchor" is
reasoned and not measured, and it stayed that way after trying every source on this
machine. Written down so the next attempt starts somewhere new:

- **The captured fixture.** Its state cannot answer `get-signers` for the cycle that
  opens at burn 380, so the decider decides nothing and the chain refuses. Correct
  behaviour, no evidence.
- **The hacknet rig.** Its `PoxId` is 21 bits of `1` and it would have been ideal.
  The chain is down: `harness.sh host 3` stopped participant 3, and miners 1 and 2
  are no longer running either, so nano has no peer and the stock nodes serve
  nothing.
- **The mainnet capture.** Spans fifteen burn blocks, 960,219 to 960,233, and holds
  one stacker set (cycle 140). No cycle boundary falls inside it.
- **The live mainnet state.** The one place both facts exist together — 142 bits of
  `1` and cycle 140's signer set — and it is the state the two-writer corruption
  damaged.

**One point survives, offline and on disk.** Mainnet cycle 140 has a recorded signer
set of 25 entries, and mainnet's `PoX` history at the checkpoint is 142 bits with no
zero among them — so the bit for a cycle with a recorded set is 1, which is the
direction the rule claims. `a_mainnet_cycle_with_a_signer_set_has_its_pox_anchor_bit_set`
pins both halves against the checked-in capture, so it is a regression rather than a
note.

It is one confirming point and not a proof: mainnet has never had a cycle that
selected no anchor, so nothing here exhibits the `0` case and the converse stays
unmeasured. That is survivable by construction — the decider decides only on a
*positively* recorded set, so an unmeasured converse can leave a boundary uncrossed
but cannot produce a wrong hash.

The remaining verification is a live crossing, which is blocked on having a healthy
mainnet state, and that makes replacing that state the first move here rather than
more code. Until then the
decider stays as it is: it decides only on a positively recorded non-empty signer
set, and refuses otherwise, so an unverified rule cannot silently produce a wrong
consensus hash — it can only fail to unblock a boundary.

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

## Resolved 2026-08-07: there is nothing to decide

The fifth verification route was stacks-core itself, which is a dev-dependency
oracle and had not been read. It settles the question outright.

`load_nakamoto_reward_set` builds exactly one anchor status,
`PoxAnchorBlockStatus::SelectedAndKnown`
(`stackslib/src/chainstate/nakamoto/coordinator/mod.rs:543`). So
`is_reward_info_known` is unconditionally true and `make_next_pox_id`
unconditionally calls `extend_with_present_block`. Its own comment states the
rule: *"In Nakamoto, every reward cycle **must** have a PoX anchor block;
otherwise, the chain halts."* The other outcome that path has is `Ok(None)` — the
anchor is not processed *yet* — which is a wait, not a zero. `NotSelected` and
`SelectedAndUnknown` are reachable only through the epoch-2.x path and the first
reward cycle of epoch 3.0, and a node that starts at or after the 4.0 boundary is
never asked about either.

Epoch 4.0 therefore has no undecidable case and no zero case. The bit is one.
`SortitionTracker::advance` extends with it, and `decide_anchor`,
`anchor_decided` and `decide_pox_anchors` are removed rather than left as a seam
nothing can fill.

**Measured rather than reasoned**, which is what four earlier routes could not
manage. `pox_boundary::a_derived_chain_crosses_five_boundaries_and_stays_on_the_chain`
derives from burn 360 to 479 through `catch_up` — the production path — and
compares the sortition identifier *and* the consensus hash at every one of the
119 blocks with what stacks-core wrote, across boundaries at 380, 400, 420, 440
and 460. The identifier is the burn header hash and the PoX vector hashed
together, so a wrong bit cannot hide in it: it diverges at 380 and stays
diverged. `the_capture_states_the_pox_bit_at_every_boundary_it_crosses` reads the
vector off every captured identifier independently and asserts one bit per
boundary and no bit anywhere else — 20 bits at 379, 25 at 479. Neither is
skip-gated.

The historical conformance count above predates later gates. Two acceptance
criteria remain: a whole-capture execution run with zero peer sortition requests,
and a live mainnet rollover/whole-following-cycle comparison.

## Reconciliation, 2026-08-09

The focused production-path differential now also compares `winner_txid` with the
captured winner at every burn from 361 through 479, beside the existing sortition
identifier and consensus-hash checks. The exact current-tree test passed from the
freshly built conformance artifact:

```text
pox_boundary::a_derived_chain_crosses_five_boundaries_and_stays_on_the_chain ... ok
```

This closes the missing winner half of the first acceptance criterion. It does not
substitute for executing the captured Nakamoto blocks across the boundary, and it
does not supply the live rollover evidence, so task 082 remains in progress.

## Reconciliation, 2026-08-10

The execution half now covers the whole capture too. `follow_path` serves all 340
blocks to a node whose peer lies about every sortition field; the node executes the
same tip and roots as the control while `sortitions_asked() == 0` across burns
360–479. `catch_up_rounds` independently closes the complete gap through the
production round driver and asserts the same zero-request invariant.

The exact gates passed on the current tree:

```text
catch_up_rounds::the_full_capture_closes_without_peer_sortitions_across_reward_cycles ... ok
follow_path::peer_sortition_lies_never_reach_execution ... ok
catch_up_rounds:: ... 9 passed
follow_path:: ... 6 passed
cargo clippy -p nano-conformance --all-targets -- -D warnings ... ok
```

Only the live mainnet rollover and following-cycle comparison remains; offline
execution evidence is not substituted for it.

## Live cycle 141 comparison in progress, 2026-08-13

The continuously running mainnet follower crossed the cycle-141 opening at burn
962,150. The retained local record shows 142 PoX-history bits at 962,149 and 143
at 962,150. At the boundary and every recorded burn view after it, the local burn
hash, sortition identifier, consensus hash, winner, parent, committed block and
VRF seed match two independent stacks-core nodes (`4.0.1` and `4.0.2`). No
non-null oracle observation disagrees.

The first monitor used `/v3/sortitions/latest_and_last`, which intentionally
omits Bitcoin blocks that elected nobody. Commit `391f62ed` corrects the live
evidence collector to read nano's current `/v3/sortitions` view and each stock
oracle's exact `/v3/sortitions/burn_height/{height}` response. The corrected
watcher is running persistently and appending to
`/home/aldur/mainnet-tip/cycle141-sortitions.jsonl`.

This is a real rollover, but only 186 of cycle 141's 2,100 burn blocks have
elapsed. The task stays open until the watcher covers the entire following cycle
and the earlier no-sortition gaps are reconciled against nano's persisted local
consensus history.
