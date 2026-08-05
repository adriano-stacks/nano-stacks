---
id: "049"
title: "Derive canonical sortitions from the local burnchain"
status: in-progress
priority: critical
effort: large
type: feature
group: mainnet
dependencies: ["026"]
tags: ["mainnet", "burnchain", "consensus"]
created_at: 2026-08-02
---

# Derive canonical sortitions from the local burnchain

## Objective

The production executor asks its one Stacks peer for `/v3/sortitions` and uses
that answer as the Bitcoin height and tenure context. `nano-node` does not depend
on `nano-sortition`, although it already downloads the raw Bitcoin blocks. The
peer therefore chooses nano's consensus hashes, winners and canonical fork.

Run `SnapshotChain` in the node and derive those facts from the configured
Bitcoin source. Peer sortition responses may be diagnostics or download hints,
never validation inputs.

## Tasks

- [x] Feed locally decoded Bitcoin operations into a `SnapshotChain` the node
      owns.
- [x] Derive consensus hash, sortition hash, winning commit transaction and
      total burn locally, checked against a captured mainnet window.
- [x] Match the captured mainnet sortition window field for field.
- [~] Hand the local snapshot to block validation and execution — validation
      takes the sortition hash from it; execution's Clarity-visible inputs
      (`vrf_seed`, `burn_block_time`, the burn header hash) still come from the
      peer, because they move state roots.
- [x] Persist snapshots and resume without trusting a peer's current burn view.
- [x] Name the winner when several commitments compete: all fourteen.
- [ ] Apply [[026-survive-a-bitcoin-reorganization]] to the production burnchain
      path and replay the affected Stacks tenures. Nothing calls `find_fork`,
      `SortitionEngine::retract_above` or `ChainState::retract` outside tests;
      one bug the reading found is fixed below.

## Acceptance Criteria

- Removing `/v3/sortitions` access does not stop a node with a Bitcoin source.
- Tampered peer sortition data cannot change the selected or executed chain.
- Mainnet captures match stacks-core for every consensus-visible snapshot field.
- A Bitcoin reorganization selects the same surviving snapshot and Stacks fork
  as stacks-core after restart as well as in-process.

## The captured mainnet window derives exactly

`crates/nano-conformance/tests/mainnet_sortition.rs` replays a captured window
of mainnet snapshots from the raw Bitcoin blocks beneath them, taking only the
first as given. **All fourteen derive**: the same operations found in each
block, the same `ops_hash` over them, the same winning commitment identified
among them, and the same `sortition_hash` chained from one to the next — none of
it asked of a peer.

Getting there found a real rule nano did not apply. At burn 960,230 nano hashed
five commitments where mainnet hashed four, and hashing subsets and orderings
against the captured value named the odd one in a pass: mainnet's hash is over
the first four **in nano's own order**, so only membership was ever wrong.

The archive settles what it is. `block_commits` has no row for that txid and
`missed_commits` does:

```
308dab22… | ["350c1699…",3] | 6147668178a7…
```

A commitment carries the modulus of the block it was built against and is only
an operation in the block that follows —
`(burn_parent_modulus % 5 + 1) % 5 == block_height % 5`. One that arrives late
is a *missed* commitment: still a transaction, still able to chain its UTXO so
the mining window survives a gap, but not part of the sortition and not part of
the hash. `nano_sortition::commitment_miss_distance` is that rule.

Two things were ruled out on the way, each by evidence rather than reasoning:
it is not the waterfall rule, which starts at 962,150 — the cycle *after* pox-5
activates; and it is not the leader key, because all five name keys that are
registered and reused tens of thousands of times.

## Every consensus-visible field now derives

With the hash history and the `PoX` history in hand, the window derives **all
four**: operations hash, consensus hash, sortition identifier and sortition
hash, for all fourteen blocks after its seed.

The `PoxId` came from the capture itself rather than a guess. A sortition
identifier is the burn header hash and the `PoxId` hashed together, so the
identifier says which bit vector produced it: at burn 960,219 mainnet's is
**142 bits, every one set** — every reward cycle mainnet has had chose an anchor
block. That is pinned by
`nano_sortition::pox_id_tests::mainnet_pox_history_is_unbroken_at_the_epoch_four_boundary`.

## What a window still cannot prove

The leader-key rule cannot be applied here — a commitment is only an operation
if it names a registered key, and the window proves it cannot check that rather
than assuming so: **zero leader keys are registered inside those fifteen
blocks**, so every commitment names one from before.

`nano_sortition::LeaderKeys` holds that registry, with its own test, ready for
the chain that can use it. And it is a small thing to carry: mainnet has
**2,477 leader keys** in total.

### Reaching past a checkpoint without replaying to it

A chain does not have to be replayed from genesis to derive a consensus hash —
it has to *know the hashes behind it*, which is twenty bytes a block.
`SnapshotChain::with_history` takes them, so a chain starting at a checkpoint
mixes the same skip-list the network did. Mainnet's whole history is 294,170
hashes, twelve megabytes, and the capture now carries it as
`sortition/consensus-hashes.json`.

That is necessary and not yet sufficient: seeded with it, the consensus hash
still does not derive, because it also mixes the `PoxId` — one bit per reward
cycle — and the replay passes `PoxId::initial()`. Deriving that bit vector is
the next input, and it is a smaller thing than replaying a chain.

## The chain is in the node

`nano_node::sortition::SortitionTracker` owns a `SnapshotChain`, starts from a
seed and the consensus hashes behind it, and advances a block at a time from
whatever burnchain the node is configured with. It applies the missed-commit
rule, so its operation set is the network's.

`tests/mainnet_sortition.rs` drives it over the captured window and it derives
the same consensus hash the network did at every block — the same claim the
direct `SnapshotChain` test makes, through the code path a node actually runs.

It is wired into the node too. A checkpoint that carries a sortition history
starts one at the burn height the node is sealed at, and every block the
follower executes advances it from the node's own Bitcoin source and compares
the consensus hash it derives against the peer's answer.

Reported rather than enforced while it is being brought up: a node that stopped
on its own arithmetic before that arithmetic was trusted would be worse off than
one that says so and carries on. Once it agrees over a long enough run,
execution takes the local answer and the peer stops being asked.

What is left is persisting the chain across a restart, the running burn total
(the one field a Bitcoin block does not carry, still taken from the peer), and
choosing between several eligible commitments — the burn distribution's
business; the tracker answers only where a block leaves no choice. Choosing between several eligible commitments is the
burn distribution's business and still to come; the tracker currently answers
only where a block leaves no choice to make.

## The running burn total is not the sum of what a block spent

Feeding the tracker a burn total it accumulated itself was the obvious next
step, and it is wrong. Summing the paid outputs of a block's eligible
commitments matches the capture at burn 960,220 and 960,221 — and then adds
100,000 at 960,222, where the network added nothing and recorded no sortition.

The three commitments there are *not* missed: stacks-core's rule is

```rust
let intended_modulus = (self.burn_block_mined_at() + 1) % BURN_BLOCK_MINED_AT_MODULUS;
let actual_modulus = self.block_height % BURN_BLOCK_MINED_AT_MODULUS;
```

which is exactly what `commit_lands_in_block` already implements, and all three
satisfy it. What the snapshot actually accumulates is

```rust
let block_burn_total = state_transition.total_burns();   // over the burn *distribution*
let next_burn_total = last_burn_total.checked_add(block_burn_total);
```

so the number comes from the burn distribution over the six-block mining
commitment window, not from the block in front of it — and an empty distribution
is what makes a block have no sortition at all. Deriving it therefore waits on
the distribution work already listed above; there is no shortcut.

Meanwhile the tracker takes the total from the Nakamoto header's
`bitcoin_spent`, which *is* that number and carries threshold signer weight. The
capture's window is the offline oracle for everything else, and it stays green.

A tracker also cannot be seeded anywhere except where its consensus-hash history
ends — every hash after that has to be derived rather than quoted, which is the
whole point — so a live node needs a capture reaching its own tip.

## The burn total does derive, and the burn distribution is why

The paragraph above is now wrong in its conclusion and right in its reasoning.
The running total is indeed the burn *distribution*'s total rather than the sum of
what a block's commitments paid — and `SortitionEngine` already computes that
distribution, so the number derives. What was missing was three inputs it was
never given:

- **How many of a commitment's outputs are payouts.** Two in a reward phase, one
  in a prepare phase, one under the waterfall; everything after them is the
  miner's change, which is the output the next commitment spends to chain through
  the window. Counting every output makes a candidate's weight the size of its
  wallet: mainnet miners chain 16–23 million sats behind a 30,000-sat commitment.
  `nano_sortition::PayoutSchedule` is the rule, built in the node from the same
  `/v2/pox` constants every `BitcoinBlockContext` is already made of.
- **Six blocks of mining window behind the seed.** The distribution weighs a
  candidate over `MINING_COMMITMENT_WINDOW` blocks, and a chain starting at a
  checkpoint has none of them. Priming with seven instead of six moves each
  candidate's median burn and turns mainnet's sortition at burn 960,226 into no
  sortition at all — a short or long window is not a rougher answer, it is a
  different one.
- **The seed's own winning VRF seed**, which the sampling of the block after it
  mixes. A capture does not record it, but every eligible commitment in a Nakamoto
  burn block carries the same `new_seed` — the hash of the parent tenure's
  coinbase proof, which every miner computes identically — so the seed's own burn
  block states it. Burn 960,230 has five commitments naming five different leader
  keys and one seed between them.

With those, the captured window derives **the running burn total at all fourteen
blocks**, alongside the consensus hash, sortition identifier and sortition hash it
already did. `tests/mainnet_sortition.rs::the_node_tracker_derives_the_same_window`
now hands the tracker nothing but Bitcoin blocks and asserts all four.

The total is also the one field with an oracle on a *live* chain: a Nakamoto
header's `bitcoin_spent` is the burn view's running total under threshold signer
weight, so `SortitionTracker::agrees_with_header` puts the derived distribution
against something the reward set signed, at every tenure. A disagreement stops the
derivation rather than being logged — every consensus hash after it would be
derived from a wrong number, and reporting that once per block for the rest of the
run says nothing the first line did not.

## The `PoxId` was one bit where mainnet has 142

The production wiring passed `PoxId::initial()`. The consensus hash mixes the
`PoX` history, so every hash the node derived was wrong for that reason alone,
however right the rest of the arithmetic was — and nothing said so, because the
check never ran far enough to compare one.

It does not need configuring. A sortition identifier is the burn header hash and
the bit vector hashed together, so a capture that records the identifier states
the vector: `nano_sortition::unbroken_pox_id_for` searches unbroken histories —
one bit per reward cycle, every bit set — and mainnet's seed resolves to 142. Only
unbroken ones are searched on purpose: the space of arbitrary vectors is
exponential, and a vector that happens to hash right is not evidence. A chain that
missed an anchor block does not resolve, and says so instead of guessing.

## The catch-up, and its bound

`SortitionTracker::catch_up` walks every burn block between where the chain stands
and the block being executed. Nothing is skipped — a consensus hash mixes the ones
behind it, so a height left out changes every hash after it — which is why the
previous version could not work: it advanced exactly one block and bailed out
otherwise, so on mainnet, where the checkpoint's seed is twelve blocks below the
first block executed, the check never ran once.

The bound is 144 burn blocks a round, about a day of Bitcoin. It covers the two
gaps that legitimately occur — the checkpoint's own, and the run of sortition-less
burn blocks between two tenures — and refuses a burn height further off, which is
a tracker seeded on another chain or a peer on one rather than a gap to walk. Each
step costs a full Bitcoin block download, which is what made the unbounded version
of this walk (commit 2ee576b8) so expensive. Bounded per round, so a round that
runs out keeps what it derived.

## The winner derives for all fourteen: the mining window is not always six

The winner's identity used to derive for 12 of the captured 14, and the two it
missed — burn 960,230 and 960,233 — both named a different commitment carrying the
*same* `new_seed`, so the sortition hash still derived and only the leader key
[[024-verify-the-vrf-seed-a-block-commits-to]] needs was wrong. It derives for all
fourteen now, and `tests/mainnet_sortition.rs` asserts the winner per block rather
than counting how many it got: `WINNERS_FLOOR` is gone.

The suspected cause was wrong, and finding that out was the whole of the work.
The note here said the difference was in `make_burn_sample`'s min-median
weighting of window slots a candidate has no commitment in — that nano fills with
1 and that a median over only the occupied slots fixed two blocks and broke a
third. It is not that. **nano fills empty slots with 1 exactly as stacks-core
does** (`distribution.rs`: "use 1 as the linked commit min. this gives a miner a
_small_ chance of winning a block even if they haven't performed chained utxos
yet"), and the earlier 13-of-14 variant was a coincidence, not a rule.

### The oracle that settled it

The distribution is a *pure function of a commitment window*, so stacks-core's own
`BurnSamplePoint::make_min_median_distribution` can be **called** rather than
inferred from fourteen recorded winners — the cheapest rung of the oracle ladder,
and it should have been the first thing built here.
`mainnet_sortition::the_burn_distribution_matches_stacks_core` converts each
window and compares candidate order, `burns`, `median_burn`, `frequency` and both
sortition-range endpoints, per candidate, per block. It reported **exact agreement
on all fourteen windows** — which is what proved the weighting was never the
problem and moved the search to what the distribution is computed *over*.

### The rule: an epoch boundary inside the window collapses it to one block

`Burnchain::from_block_ops` windows a sortition over `MINING_COMMITMENT_WINDOW`
blocks only when three things hold:

```rust
if !burnchain.is_in_prepare_phase(parent_snapshot.block_height + 1)
    && !is_after_pox_sunset_end(parent_snapshot.block_height + 1, epoch_id)
    && (epoch_id < StacksEpochId::Epoch30 || window_start_epoch_id == epoch_id)
```

where `window_start_epoch_id` is the epoch at `parent_snapshot.block_height -
MINING_COMMITMENT_WINDOW`, i.e. **seven blocks back**. Otherwise the block is
weighed alone.

`BITCOIN_MAINNET_STACKS_40_BURN_HEIGHT = 960_230`. Epoch 3.4 ends there and 4.0
begins there, so for burn 960,230 through 960,236 the epoch seven blocks back is
3.4 while the epoch at the block is 4.0, and mainnet weighed each of those seven
sortitions **on its own block alone**. nano weighed them over six. Burn 960,230
and 960,233 are the two of those seven where a one-block window and a six-block
window disagree about the winner; 960,231, 960,232 and the three sortition-less
blocks agreed by luck, which is exactly why a 12-of-14 count named the wrong
suspect.

A one-block window is not a rougher answer than a six-block one. It changes three
things at once: each candidate's weight becomes what it actually paid, the
windowed median becomes the block's own total so the assumed-total-commit
carryover is always 1 and the null miner can never win, and the minimum mining
frequency drops to 1.

`nano_sortition::PayoutSchedule::mining_window_at` is that rule, and it needs no
new configuration: pox-5's activation height *is* epoch 4.0's start
(`validate_epochs` requires it), so `/v2/pox` already states it. One boundary is
enough, because a 4.0-only node starting at or after that boundary can never have
another transition in its window — mainnet's previous one, epoch 3.4 at burn
943,333, is seventeen thousand blocks back.

### Two more differences the same reading found

**The prepare-phase predicate was off by one.** stacks-core's classic predicate
(`PoxConstants::static_is_in_prepare_phase`, and it is the classic one both the
commitment parser and the distribution use) is `reward_index == 0 || reward_index
> reward_cycle_length - prepare_length`. nano had `offset >= length - prepare`:
the same window shifted down by one at both ends, so it called the last
reward-paying block of a cycle a prepare block and the next cycle's "mod 0" block
a reward block. Invisible in every capture — they all sit deep in a reward phase,
mainnet's window at offsets 169–183 of 2100 — and wrong twice per cycle forever.
It decides both how many of a commitment's outputs are payouts and whether the
window collapses, so it is not cosmetic. `PayoutSchedule::is_in_prepare_phase` is
now the classic form, with the mod-0 block included on purpose.

**A missed commitment belongs to the sortition it aimed at, not the one it
arrived in.** stacks-core stores it under `intended_sortition` and reads it back
with `get_missed_commits_by_intended`, so a window slot holds the misses of the
block *above* it; nano filed each miss in the block it landed in, one slot too
high, which lets a chain reach one block further back than the network's does. And
a miss of more than one block is refused outright
(`check_intended_sortition` → `BlockCommitMissDistanceTooBig`, because a miner
able to file arbitrarily late could bunch a whole window into one Bitcoin block
and skip the six-block warm-up), so its UTXO chains nothing at all —
`commitment_window_block` now drops it.

The captured window **cannot falsify either half of this**: it holds exactly one
missed commitment, at burn 960,230, and nothing chains to it, so both placements
give the same distribution there. That is why
`a_chain_reaching_through_a_missed_commitment_matches_stacks_core` builds the
window that does tell them apart — a candidate spending a miss that spends an
older commitment — and checks it against stacks-core rather than against an
expectation written down by hand. Under the filing rule the chain is two long;
under the arrived-in placement it would be three, with a different median.

### What still has to be wired outside this task's files

Two one-line changes in `crates/nano-node/src/lib.rs`, which another agent owns:

- `payout_schedule` must chain `.activating_epoch_four_at(pox.pox_5_activation_height)`
  — it already reads that field to derive the waterfall height. Without it the node
  weighs the seven blocks from the 4.0 boundary over six blocks instead of one. It
  costs nothing today, because the mainnet node stands past burn 960,236, and the
  prepare-phase collapse (the one that recurs every cycle) needs no new input and
  is live.
- The `(candidates == 1)` hedge around `winner_vrf_public_key` can go: the winner
  derives whether or not the burn block left a choice. `SortitionTracker::candidates`
  is a report now, not a gate.

## A reorganization must not leave the replacement branch weighed short

Nothing in `nano-node` calls `find_fork`, `SortitionEngine::retract_above` or
`ChainState::retract(reorg)` — the Bitcoin-reorganization path exists and is
tested in the library and is unwired in production, which is the remaining item
above. Wiring it needs the node's own round (`crates/nano-node/src/lib.rs`,
`runtime.rs`) to ask Bitcoin whether the heights it snapshotted still hold,
invalidate the `PreStx` window, and hand the `SortitionReorg` to the chainstate.

Reading it turned up one real bug, in the engine rather than the wiring, and it is
the same failure the mining-window work above is about. `SortitionEngine` kept
exactly `MINING_COMMITMENT_WINDOW` blocks of commitment history, and a retraction
drops one entry per retracted snapshot — so a reorganization two blocks deep left
five, and the first replayed sortition was weighed over five blocks where the
network used six. The blocks that would have refilled it sit below the fork point
and are never read again. It kept `RETAINED_COMMITMENT_BLOCKS = 2 *
MINING_COMMITMENT_WINDOW` now, which covers every depth `retract_above` admits,
and `the_retained_history_refills_the_window_after_the_deepest_retraction` fails
if that slack is taken away.

What is *not* closed: the depth guard is still a constant rather than a function of
the history actually held. A chain freshly seeded from a checkpoint has only
`MINING_COMMITMENT_WINDOW` blocks behind it — `SortitionTracker::prime` reads that
many and the capture carries no more — so until it has run a window past its seed,
a reorganization it accepts can still be weighed short. Making the guard refuse on
the retained length instead would also refuse the legitimately short window of a
chain younger than six blocks, which stacks-core allows ("Mining commitment window
shortened because block height is less than window size"), so telling those two
apart is part of the item above rather than a one-line change.

## It keeps pace on mainnet

Against a mainnet state at Stacks height 8,666,584, the node closed the
checkpoint's gap in one round (`derived 33 sortitions locally, now standing on
burn 960252`) and then advanced one burn block at a time with execution, through
960,255, 960,256 and 960,257 — reporting no consensus-hash difference, no VRF-seed
difference and no burn-total difference with the headers at any of them. Burn
960,257's derived `sortition_id` `d9d17c4f…` and `consensus_hash` `67d48bbf…`
match `api.hiro.so/v3/sortitions/burn_height/960257` exactly. A restart resumed
from the saved chain at burn 960,254 rather than re-deriving from the capture.

## The capture needs six blocks below its span

`xtask capture` writes Bitcoin blocks only for the burn span its Stacks blocks sit
in, so a capture cannot fill the mining window behind its own seed. The six blocks
below `/home/aldur/mainnet-capture`'s span were added by hand; the test finds them
by walking the previous-block hash out of each header, which also proves they are
the seed's real ancestors. `xtask capture` should reach
`MINING_COMMITMENT_WINDOW - 1` blocks below the span, and until it does the
tracker test skips with that as its reason rather than quietly asserting less.

