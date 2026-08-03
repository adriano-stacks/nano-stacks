---
id: "027"
title: "Choose a fork instead of following a peer"
status: in-progress
priority: high
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

- [x] Follow several peers and keep the candidate tips they report.
- [x] Choose between candidates on chain length and valid signature weight
      against the burn view, not on arrival.
- [x] Give back the blocks after a named ancestor, so a heavier fork can be
      taken instead of refused.
- [ ] Detect the heavier fork on the follow path and drive the retraction from
      it.
- [x] Use `/v3/tenures/fork_info` to find where a candidate diverged.
- [x] Treat a peer that serves an invalid block as untrusted rather than fatal.

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

