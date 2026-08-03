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

- [ ] Feed locally decoded Bitcoin operations into a persistent `SnapshotChain`.
- [ ] Derive consensus hash, sortition hash, winning commit transaction, leader
      key, total burn and accumulated coinbase locally.
- [~] Match the captured mainnet sortition window field for field — ten of
      fifteen do, and the eleventh is the epoch 4.0 boundary.
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

## Ten mainnet sortitions derive exactly

`crates/nano-conformance/tests/mainnet_sortition.rs` replays a captured window
of mainnet snapshots from the raw Bitcoin blocks beneath them, taking only the
first as given. For **ten consecutive burn blocks** nano finds the same
operations, hashes them to the same `ops_hash`, identifies the same winning
commitment among them, and chains the same `sortition_hash` from one to the
next — none of it asked of a peer.

It diverges at burn **960,230**, on the operations hash, where nano finds five
operations and all of them are leader block commits.

**The network rejects exactly one of them.** Hashing subsets and orderings of
the five against the captured `ops_hash` finds it in one pass: mainnet's hash is
over the **first four, in nano's own order**. So the ordering is right, the
decoding is right, and the fifth commit — `308dab22…`, at transaction index 573
— is one nano accepts and the network does not.

It is not malformed. All five carry the same shape, two `PoX` outputs and
change: `[20000, 20000, 1406698]` against `[15000, 15000, …]` and
`[21250, 21250, …]`. And it is not the waterfall rule, which rejects a commit
paying nothing but only applies from `first_pox_waterfall_block` — the first
block of the cycle *after* pox-5 activates. Cycle 140 runs 960,050 to 962,149,
so that rule starts at 962,150, well past this block.

What is left is the validation nano does not do: **a commitment is only an
operation if it names a leader key that was registered and not already spent.**
nano accepts every commit it decodes. Keeping a registry of leader keys across
the snapshot chain, and dropping commits that do not resolve against it, is the
next task here — and it is the same registry the VRF check in
[[024-verify-the-vrf-seed-a-block-commits-to]] needs to find a tenure's public
key.

The consensus hash is not checked by this test and cannot be: it mixes prior
consensus hashes at power-of-two offsets reaching back thousands of blocks, so
it needs a chain replayed from its own genesis rather than a slice of one. That
is what a node building the chain itself will exercise.

The test is a depth gate, like replay depth: ten is the floor already reached,
and raising it is the measure of progress.
