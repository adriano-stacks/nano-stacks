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
      takes the sortition hash and the winner's registration from it, and
      execution now takes the two burn spends `miner-spend-total` and
      `miner-spend-winner`, which are Clarity-visible. `vrf_seed`,
      `burn_block_time` and the burn header hash still come from the peer.
- [x] Persist snapshots and resume without trusting a peer's current burn view.
- [x] Name the winner when several commitments compete: all fourteen.
- [x] Apply [[026-survive-a-bitcoin-reorganization]] to the production burnchain
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


## The two one-line changes outside this task's files have landed

Both items the section above listed as unwired are wired, and were already wired
when this was read again: `payout_schedule` chains
`.activating_epoch_four_at(pox.pox_5_activation_height)`, so the seven blocks from
the 4.0 boundary are weighed on their own block; and the `(candidates == 1)` hedge
around `winner_vrf_public_key` is gone, so the winner is published as derived and
`SortitionTracker::candidates` is a report. Nothing in the node gates on it now.

## What a sortition costs per block: nothing, and the number that said otherwise

`local` read as 0.11 s per Stacks block on mainnet against the 0.014 s of the peer
requests it replaced — 24.99 s over 225 blocks — which looks like a reason to make
the arithmetic cheaper. It is the fifth wrong attribution of a performance problem
in this project ([[047]] records the first four), and the node's own timing lines
falsify it without any new instrumentation:

```
timing over 275 blocks on 7 views: ... local 26.09s
timing over 300 blocks on 7 views: ... local 26.09s
timing over 325 blocks on 7 views: ... local 26.09s
timing over 350 blocks on 7 views: ... local 26.09s
```

The phase does not move between one Stacks block and the next. It moves when the
**burn view** does. A sortition is a fact about a Bitcoin block and many Stacks
blocks stand on one, so per-Stacks-block is the wrong denominator: seventy-five
blocks of that round cost zero, and the 26 s was one restart's catch-up plus six
burn views. There is nothing per-block to remove because there is nothing
per-block — `catch_up` returns immediately once the chain stands where the block
does.

`CatchUp` now counts what is left apart, because the two halves are not the same
kind of thing, and a live mainnet run says:

```
derived 1 sortitions locally, now standing on burn 960475 (0.73s reading 1 burn blocks, 0.000s deriving)
the derived sortition chain is written down (0.20s)
```

Per burn view: **0.7–1.3 s waiting on the burnchain, 0.2–0.3 s writing the chain
down, and 1 ms of hashes.** The arithmetic is not the cost and could not become
one; the cost is a mainnet Bitcoin block fetched from a hosted Esplora, which is
the price of not asking a peer, and it is paid once a tenure.

Two things worth knowing that came out of the same measurement:

- **Priming costs about seven seconds on every start** — the six burn blocks
  behind the seed that `MINING_COMMITMENT_WINDOW` is weighed over — and it used to
  print nothing at all, because no sortition comes out of it. It says so now.
  Removing it means writing the commitment window down beside the snapshot, which
  is a new serialised format for a once-per-start cost, so it is measured and left
  alone rather than built.
- **The 12 MB consensus-hash history is rewritten whole per burn view**, for one
  hash appended. At 0.2 s a tenure that is not worth an append-only format, but it
  is the only part of this phase that grows with the length of the chain.

## Wiring the Bitcoin reorganization: what the measurement above changes about it

Still open, and reading it against the timing above moved where it has to go.

Detection is already free and already reaches the node: `BitcoinRestSource::block_at`
verifies on every read that the block it read last is still at that height
(`check_last_read`) and returns `Reorganized { height }`, which arrives in
`local_sortition` as a `TrackerError::Bitcoin`. What that does today is exactly
wrong — it disables the local derivation and goes back to the peer's sortitions,
which is to say a Bitcoin reorganization hands the node's consensus hashes back to
the peer.

What must not happen is the obvious fix. `SnapshotChain::find_fork` walks the
snapshots comparing each against Bitcoin, so calling it from `local_sortition`
would cost one burnchain round trip **per Stacks block** — 0.2 s each, reinstating
precisely the per-block cost the section above establishes does not exist. The
check has to be gated on the burn view moving, the way `catch_up` already is.

The rest is in files this change did not own, and it is three things rather than
one: `BitcoinSource` has no `invalidate_from`, so the `PreStx` window cannot be
invalidated through the trait; `ChainState::retract` needs the executed tip and the
staging directory to be walked back with it, which is the round's business
(`runtime.rs`, `execute_staged`) and not a sortition's; and the depth guard is
still a constant rather than a function of the history held, which is the
unresolved half recorded above.

## A resumed chain named the wrong winner where nobody had been elected

Making the coinbase proof checkable ([[024]]) surfaced this immediately, and it had
been there since resuming was added. The sampling of a sortition mixes the **most
recent winner's** VRF seed — not the tip's, because a burn block that elects nobody
mixes nothing, and mainnet leaves four such blocks in every fifteen. A chain
seeded at one of them holds no snapshot with a seed at all, and
`unanimous_winner_seed` recovered one from the seed block's own commitments, which
carry the seed of the tenure they were *bidding* for. A different value of the
right shape.

Nothing downstream disagrees. Every candidate in a Nakamoto burn block carries the
same `new_seed`, so a wrongly named winner still produces the right sortition hash,
the right consensus hash and the right running burn total — the whole of what this
task checks against the capture and against a signed header. The only thing it
changes is *which leader key* the tenure's proof is checked against, so for as long
as that key was `None` the bug was unobservable.

It became observable the moment the registry landed. A live node restarted onto
burn 960,487 — `was_sortition: false` — and refused the tenure at 960,488 with
*"coinbase VRF proof was not produced by the winning leader key"*, retrying a block
the network had accepted, forever.

The fix is to write the seed down rather than recover it:
`SnapshotChain::effective_winner_seed` names the rule, `save` records it beside the
tip, and `seed_snapshot` refuses a snapshot that carries neither it nor whether its
block elected anybody — a chain saved by an older binary is re-derived from the
checkpoint instead, saying so while it walks, because guessing costs a wrongly named
winner one restart in three.

Two things are worth keeping from how this was found:

- **The `snapshots` table's own `sortition` column is the discriminator**, and the
  capture already carried it. The saved form now carries it too, so a seed either
  states the winning seed, or states that its block had a winner and lets `prime`
  recover it from that block's commitments, or is refused. There is no fourth case
  and no default.
- **A wrong answer that agrees with every oracle is what a checkpoint-resumed chain
  produces by default.** Four fields derived exactly and the fifth was wrong for
  three months. What caught it was making a *sixth* thing depend on it.

`a_chain_resumed_at_a_sortitionless_burn_block_names_the_same_winner` pins it
offline on mainnet data — the capture's burn 960,222 is exactly this shape — by
running one chain through and stopping another at the sortition-less block, saving
it, reading it back the way a restart does, and requiring the same winner. It fails
if the saved field is taken away. Live, after the fix: resumed at burn 960,491,
which elected nobody, and the proof of every tenure from 960,492 to 960,496
verified.

## The numbers, from the re-derivation this caused

Re-deriving 269 burn blocks from the checkpoint's anchor is the largest sortition
workload a mainnet node ever runs, and it prints its own split:

```
derived 144 sortitions locally, now standing on burn 960363 (205.94s reading 150 burn blocks, 6 of them priming the mining window, 0.006s deriving)
derived 125 sortitions locally, now standing on burn 960488 (104.42s reading 125 burn blocks, 0.008s deriving)
```

**40 microseconds of arithmetic per sortition, against 0.8–1.4 s of Bitcoin block.**
Four orders of magnitude, and the slow side is a hosted Esplora rather than
anything this crate does. There is nothing to optimise here that is not a caching
decision about the burnchain.

## The reorganization is wired, and it costs one request a round

`CheckpointExecutor::check_burnchain`, called from `catch_up` before anything is
executed. The gating is what the measurement above demanded: **one
`block_hash_at` per round**, for the height the sortition chain's tip stands on,
and the walk of `find_fork` only when that answer differs. Not per Stacks block —
a sortition is a fact about a Bitcoin block and many Stacks blocks stand on one,
which is the per-block cost this task established does not exist and must not be
reinstated.

What it does with the news, in order: `find_fork` against Bitcoin,
`SortitionEngine::retract_above` at the fork point, `BitcoinSource::invalidate_from`
so the surviving chain's `PreStx` window is the only one left, `ChainState::retract`
for the Stacks blocks the invalidated tenures carried, the derived chain written
down *before* anything is executed on the replacement branch, staging cleared, and
the executor stood on the surviving block.

Three things about it that were decisions rather than mechanics:

- **A burnchain that cannot be read is not a burnchain that moved.** A failed
  `block_hash_at` is reported and the round carries on; treating the two alike
  would retract a chain over a network error.
- **A reorganization reaching below the chain's own root is refused**, naming the
  height and saying the state needs a checkpoint Bitcoin agrees with. Nothing
  local can say what replaced a checkpoint's burn anchor.
- **What it replaces was worse than doing nothing.** A `TrackerError::Bitcoin`
  used to disable the local derivation and go back to the peer's sortitions — a
  disagreement about the burnchain answered by trusting the peer *more*.

`BitcoinSource::invalidate_from` is new on the trait, with a default no-op body:
both real sources have had one for months and the node could not reach either
through the trait it holds them behind, which is why this was recorded as unwired.
A source that keeps nothing — a fixture, a recorded window — has nothing to forget,
and requiring the method would put an empty body in every one of them.

`follow_path::a_bitcoin_reorganization_retracts_the_blocks_it_invalidated` drives
it through `catch_up` over the captured chain: the burnchain gives back the block
the last executed tenure was elected in, one sortition is retracted, two Stacks
blocks are given back, the surviving chain is a prefix of the one executed, the
executor stands on the surviving block and staging is empty.

## The captured hacknet chain derives — and why it did not

Writing that test needed a *derived* sortition chain over the fixture capture,
because `find_fork` walks the snapshots a node took and a checkpoint-seeded chain
has one. It did not derive: the consensus hash was wrong at every block above the
seed and the running burn total never moved at all.

The cause is a rule this crate does not have. **The number of payout outputs a
commitment pays is the number of recipients the cycle's reward set holds, capped
at `OUTPUTS_PER_COMMIT`.** `PayoutSchedule::outputs_at` answers two in a reward
phase unconditionally. The captured chain has *one* stacker, so every commitment
there pays one output of 20,000 sats and then its own change — and counting two
makes each candidate's weight the size of its wallet, which is exactly the trap
this task records for mainnet, one layer down. With the wrong count no winner is
selected at all, so the burn total stops moving and every consensus hash after it
is wrong for that reason alone.

Measured both ways on burn 362–367 of the capture:

```
two payout outputs:  no winner at any block, total_burn frozen at the seed's 7,380,000
one payout output:   all five consensus hashes and the running total derive exactly
                     (7,440,000 → 7,440,000 → 7,440,000 → 7,460,000 → 7,480,000)
```

It does not bite on mainnet, where every cycle has thousands of recipients and the
count is two — which is why the mainnet window derives and this one never did. It
bites on **every** small chain: hacknet, a private testnet, and the tail of a
mainnet cycle where a reward set could run short. The fix is `outputs_at` reading
the recipient count of the cycle's reward set rather than assuming two, which is a
`nano-sortition` change and another agent's file; `follow_path` passes the schedule
that matches what the chain actually did and says so where it does it.

Two smaller things the same work turned up:

- **A chain cannot be derived across a reward-cycle boundary it has not resolved.**
  `advance` refuses at a block that opens a cycle, because the consensus hash mixes
  one bit per cycle and whether that cycle chose an anchor block is not yet
  knowable. Correct, and it means a test or a node seeding inside a cycle has to
  stay inside it — the fixture's boundary at burn 361 is why `follow_path` seeds at
  the second executed tenure rather than the first.
- **`SortitionTracker::save` now makes its directory.** A chain with nowhere to be
  written down is re-derived from the checkpoint on the next start, one Bitcoin
  block download per burn block, and the only sign of it is a line in a log.

## Two Clarity-visible burn fields the production path leaves at zero

Found while pointing the follow path at the captured chain, and it belongs to the
`[~]` item above rather than to the reorganization work.

`BitcoinBlockContext` carries `burn_spend_total` and `burn_spend_winner` — what
every miner spent on a sortition and the winner's share — and they land in the
recorded header, where `get-burn-block-info?` reads them back. The offline replay
harness fills both from the captured Bitcoin block. **`CheckpointExecutor::execute_staged`
fills neither**, so a node following the chain executes every block with both at
zero while the replay that verifies the state roots executes with the real numbers.

Nothing has caught it, and the reason is worth writing down rather than relying on:
no contract in the captured chain or in the replayed mainnet window reads either
field, so the two paths seal the same roots. A contract that read one would diverge
on a *production* node and agree in every offline replay — the one shape of
divergence the north-star metric cannot see, because the metric does not run the
production path.

It is derivable rather than borrowable: the burn distribution the sortition tracker
already computes has both, per block, for the same window `total_burn` comes from.
Left unfixed here on purpose — it changes what a production node writes into a
header, which is a state-root decision and wants its own measurement against the
mainnet replay rather than a change made in passing.

## The two burn spends are derived now, and what settled the payout count

Both fields the note above left at zero are filled from the sortition tracker's own
commitment window: `SortitionEngine::burn_spends` sums every eligible commitment's
payout burn in the tip's burn block and picks the winner's out of it, reading the
same window the distribution was weighed over rather than the burnchain again. They
travel together or not at all — the Clarity documentation promises the winner's is a
positive number no larger than the total, and half an invariant offered to a
contract is worse than an absence — and `None` is a burn block that elected nobody,
which no tenure stands on.

**The payout-output count is not the size of the reward set.** That was the standing
suspicion recorded here, and it is wrong in a way stacks-core is explicit about: a
short reward set is *padded with burn addresses* to the full count
(`RewardSetInfo::into_commit_outs`, and `check_pox_pre_waterfall`'s "if the number
of recipients in the set was odd, we need to pad with a burn address"), so a
one-stacker cycle still pays two outputs and the count never moves with the
recipients. `get_num_pox_payouts` is a function of the height alone. The captured
hacknet chain pays *one* because it is past the waterfall, which nano already knew
how to answer; what it had never been told is where the waterfall began.

Three oracles say so, cheapest first, in `conformance/burn_spends.rs`:

- `RewardSetInfo::commit_outs_for` — stacks-core's own "single source of truth
  shared by the miner and the parser" — is a pure function and is *called*, for a
  one-recipient set, a full set, a prepare phase and the waterfall.
- the archive's `pox_payouts` column states the count for every captured burn
  block, and `amount × addresses.len()` equals the running `total_burn`'s own step
  at every block that elected somebody — on mainnet's reward phase (×2) and on the
  hacknet capture's waterfall (×1) alike.
- the tracker's derived spends equal that column, on the capture whose window used
  to derive nothing at all, and per block inside `mainnet_sortition`'s window walk.

The conformance harness had the same trap one layer up: its `burn_spend_total`
oracle summed *every* output of every commitment, change included, which on mainnet
reads a 30,000-sat commitment as the 16–23 million behind it. It takes the payout
count from the archive now, so the replay's oracle for that field is the archive
rather than nano's own arithmetic.

## Fifty minutes at SYN-SENT, and the peer a round could not get past

The live mainnet catch-up stopped at 8,699,006 with 28,458 blocks already staged and
executed none of them for fifty minutes, printing `executing the peer's chain
failed: ... error sending request for url (http://<peer>:20443/v3/sortitions/consensus/<ch>)`
once a round. Sampling the process found it at 1 tick of CPU per 20 seconds and one
socket in `SYN-SENT`.

Two things were wrong and both are this task's, because both are the node asking a
peer for a sortition it derives itself:

- **`execute_staged` asked one peer.** The pool that `catch_up` already threads
  through the descent — `TenureSource`, which round-robins, sets a rate-limited peer
  aside and spreads the work — was not used for the sortition lookup or for the
  coinbase walk behind it, so one unreachable peer failed the round, and a failed
  round abandons everything staged. It asks the pool now. The three duplicated
  round-robin bodies became one `spread`, and the sortition lookup refuses an answer
  that does not carry the consensus hash it asked for: that is the one field of a
  sortition a peer must not choose, since every other one is checked by the state
  root the block's own header commits to under threshold signer weight.
- **A peer that cannot be reached was waited on for thirty seconds, per request.**
  Discovery learns a peer's *p2p* port and its HTTP port is an assumption about the
  port beside it, so a pool of strangers holds several addresses whose 20443 never
  answers. `connect_timeout` is four seconds now, and a peer that fails to connect
  or times out is set aside for the rest of the round like a throttled one — but
  only for unreachability: a 404 is an ordinary answer in a walk over strangers, and
  setting those aside would empty the pool within one descent.

Restarted with both, the same state resumed and executed 80–195 blocks a minute
against the same peer set. The lesson is the one already written on
[[047-make-mainnet-synchronization-monotonic-and-restart]]: sample the process
before believing any story about where time goes.

## All three remaining execution inputs come from this node's own burnchain

`vrf_seed`, `burn_block_time` and the burn header hash were the three
Clarity-visible fields the note above left with the peer. They come from the
locally derived snapshot now, along with the two burn spends, and one thing had to
be added to make it possible: a Bitcoin block carries its header time and nothing
was reading it. `BitcoinBlock::timestamp`, `SortitionSnapshot::bitcoin_timestamp`,
and the capture's own `burn_header_timestamp` column for a chain's seed — which a
resumed chain has to state, because the tenure standing on the seed's own burn view
is executed before the chain advances once.

**Why this can be switched over rather than compared forever.** A wrong answer to
any of the five does not corrupt state: it changes the root the block seals, the
header the network signed states a different one, and the block is refused with
nothing committed. That is the opposite of the position the *validation* inputs are
in, where a wrong answer is invisible — and it is why the sortition hash and the
winner's registration were derived locally months before these were.

The oracles are the archive's own columns, per burn block, in
`mainnet_sortition::the_node_tracker_derives_the_same_window`: consensus hash,
sortition hash, running burn total, winner, burn header hash, **burn header time**
and the two spends, for every block of the captured mainnet window. The peer's
answer is still fetched and still compared — `report_disagreements` names any of
the four fields that parts company — because a difference tells an operator which
of two chains of Bitcoin blocks is not the network's, and the state root that
refuses the block afterwards does not say which field caused it.

What the peer still supplies is the burn *view* of a block whose tenure change this
node did not execute, and the height that view sits at. Both are checked rather
than trusted: the pool's answer must carry the consensus hash it was asked for, the
tracker derives the consensus hash at the height it walked to, and a header whose
cumulative burn disagrees with the derived total stops the round.
