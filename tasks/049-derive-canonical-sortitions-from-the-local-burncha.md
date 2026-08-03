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

- [~] Feed locally decoded Bitcoin operations into a `SnapshotChain` the node
      owns — done, though not yet persistent nor driving execution.
- [x] Derive consensus hash, sortition hash, winning commit transaction and
      total burn locally, checked against a captured mainnet window.
- [x] Match the captured mainnet sortition window field for field.
- [ ] Hand the local snapshot to block validation and execution.
- [ ] Persist snapshots and resume without trusting a peer's current burn view.
- [ ] Apply [[026-survive-a-bitcoin-reorganization]] to the production burnchain
      path and replay the affected Stacks tenures.

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
the hash. `nano_sortition::commit_lands_in_block` is that rule.

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

