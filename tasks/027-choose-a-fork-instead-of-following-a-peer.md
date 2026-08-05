---
id: "027"
title: "Choose a fork instead of following a peer"
status: in-progress
priority: critical
effort: large
type: improvement
dependencies: ["026", "049", "050"]
tags: ["mainnet", "sync"]
created_at: 2026-07-30
---

# Choose a fork instead of following a peer

## Objective

`TenureFollower` tracks one peer's `/v3/tenures/info` tip and rejects anything
that does not extend the history it already holds (`SyncError::Fork`,
`crates/nano-sync/src/lib.rs:586`). That is not fork choice, it is obedience to
whichever node the operator configured.

W9 asked for fork choice on chain length with valid signature weight against the
burn view. On Hacknet the single peer is cooperative and the distinction never
shows. On mainnet a single trusted HTTP peer is a liveness dependency and a
censorship dependency at once, and a peer that reorganizes past nano's history
strands it.

## Tasks

- [x] Implement a `PeerPool` that can gather candidate tips, rank them
      deterministically and distrust a peer that serves invalid data.
- [x] Wire the production runtime and `catch_up` to poll that pool instead of
      selecting the first reachable HTTP peer and constructing one
      `SyncClient`.
- [ ] Choose between candidates using enforced signer authentication and the
      locally derived burn view from [[049-derive-canonical-sortitions-from-the-local-burncha]]
      and [[050-authenticate-every-followed-nakamoto-block]], not peer claims.
- [x] Give back the blocks after a named ancestor, so a heavier fork can be
      taken instead of refused.
- [x] Give the executor a fork switch that finds where the chains parted and
      stands there.
- [x] Call it from the follow path instead of stalling.
- [x] Use `/v3/tenures/fork_info` to find where a candidate diverged.
- [ ] Exercise two simultaneous peers through the production runtime: one
      stale, withholding or invalid, and one serving the canonical chain.

## Acceptance Criteria

- A peer that reorganizes does not stall nano.
- Given two candidate forks, nano selects the one stacks-core selects.
- No single peer can withhold the canonical tip from a node with other peers.

## Remaining

`PeerPool` gathers candidate tips, `weigh_tip`/`choose_canonical_tip` pick one on
signer weight then length (ties by block identifier, so peer order never decides),
`SyncClient::tenure_fork_info` and `fork_point` locate where two views parted, and
`PeerPool::distrust` sets a lying peer aside instead of raising.

What is not done is the loop that acts on all of it. `TenureFollower::poll` still
follows one client and still answers `SyncError::Fork` when the peer's chain does
not extend its history, where it should instead take the chosen tip, find the fork
point, hand `ChainState::retract` the blocks below it, and replay forward. The
pieces on both sides now exist — `ChainRetraction` from
[[026-survive-a-bitcoin-reorganization]] and the choice here — so this is wiring,
in `TenureFollower` and in `nano-node`'s `ExecutingNode`.

## Standing on an ancestor again

`ChainState::retract_to` is the half a fork switch needs from the chainstate.
`retract` already existed for *Bitcoin* reorganizations, which invalidate
consensus hashes; a Stacks fork invalidates none — the sortitions still stand and
what changes is which chain of blocks is heaviest — so the ancestor is named
directly instead of derived.

Retracting is cheap because nothing is deleted. The MARF addresses a state by the
block that sealed it, so an abandoned branch merely stops being reachable, and if
the fork changes its mind those states are still there to stand on. What has to
be rewound is everything kept *beside* the MARF — the executed chain, the tenure
start heights, the accounting — none of it addressed by block, all of it
otherwise describing a chain this node is no longer on. That is the same class of
bug as [[056-make-rejected-block-execution-leave-no-state]].

Retracting to a block this node never executed does nothing, which is not a
detail: the ancestor in a real switch is named by a *peer*, and a node must not
be talked into emptying its own chain by being told about someone else's.

Both are pinned in `tests/fork_retraction.rs`. What remains is the follow path
noticing the heavier fork and calling this, rather than raising `SyncError::Fork`.

### From a burn block to a Stacks block

`fork_point` compares two peers' views and answers with a **consensus hash** —
a fork is agreed in burn blocks — while a retraction has to name a **Stacks
block**. `last_block_of_tenure` is the bridge: everything up to the last block
executed under that tenure is on both chains by construction, and everything
after it is what the fork disputes.

A tenure this node never executed names nothing, for the same reason retracting
to an unknown block does nothing. Both answers come from what this node
executed, never from what a peer says about it.

With those two the chainstate side is complete: given a fork point from
`/v3/tenures/fork_info`, a node can name where to stand and give back the rest.
What is left is the follow path calling them instead of raising
`SyncError::Fork`.

### The switch itself

`CheckpointExecutor::switch_to_fork` takes the tenure a peer is on, asks it for
`/v3/tenures/fork_info` back to the oldest tenure this node executed, compares
that against `executed_tenures`, and stands on the last block of the tenure they
agree about.

Both sides are checked and neither is taken on trust. A fork point neither side
reaches, or one naming a tenure this node never executed, changes nothing — a
peer must not be able to talk a node off its own chain, which is the failure mode
that makes "fork choice" worth having over "follow whoever is configured".

`fork_point_of` was added alongside because a node comparing its own chain has
consensus hashes and nothing else: it did not learn its chain from a `fork_info`
answer, and making it fabricate burn heights to throw them away would be
inventing evidence.

### What a fork looks like from the follow path

Not an error, as it turns out. A descent walks back from the peer's tip until it
reaches a block at or below this node's height — and on a fork it reaches that
height on *another branch*, stages it, and stops. Nothing raises. What happens
instead is that nothing staged extends this node's tip, so the round executes
zero blocks, and no later round ever will either.

So the signal is: **fetched blocks, executed none**. `catch_up` now takes that as
the question worth asking, and asks it of the peer's own tenure. A healthy chain
never reaches it, because a healthy round executes what it fetched — confirmed
against mainnet, where the switch has not fired once.

## Reopened after production-path audit

The fork-switch machinery above is real, but the completion claim was not.
`nano-node` still calls `reachable_peer`, takes the first configured HTTP peer
that answers and passes one `SyncClient` into `catch_up`. `PeerPool` is therefore
library machinery, not the source of the production chain view, and no second
peer can currently prevent withholding or override a stale first peer.

This task closes only when the assembled runtime uses the pool and the
acceptance criteria pass end to end. [[054-join-and-synchronize-over-the-stacks-p2p-network]]
will replace hosted HTTP transport, but it must feed this same locally
authenticated selection boundary rather than becoming another single source of
truth.

## The runtime polls the pool now

`reachable_peer` no longer takes the first configured HTTP peer that answers.
`follow_pool` builds a `PeerPool` from the configured endpoints *and* the ones
[[054-join-and-synchronize-over-the-stacks-p2p-network]] discovers over p2p, and
`better_peer` re-weighs it whenever a round is let down — through
`PeerPool::choose_source`, which compares a tip on signer weight and length from
headers this node fetched rather than on a peer's claim about its own height.

`node.peers` may now be empty: mainnet falls back to stacks-core's public
bootstrap nodes, so the peers a node follows are found rather than configured, and
no one of them is anybody's product. That is the part of this task that was about
liveness and censorship rather than about forks.

**What that does not yet close.** The two remaining items are the ones that need
the *validated* boundary rather than the transport: choosing between candidates on
enforced signer authentication (which waits on
[[050-authenticate-every-followed-nakamoto-block]], and on mainnet reaching
epoch 4.0 before a pox-5 reward set exists to enforce against), and a test that
runs two simultaneous peers through the production runtime with one of them
stale, withholding or lying. `nano-p2p`'s `loopback.rs` isolates a peer that
signs with the wrong key and one on the wrong network; a peer that serves a
plausible but wrong *chain* through the follow path is the case still missing.
