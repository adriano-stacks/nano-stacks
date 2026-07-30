---
id: "027"
title: "Choose a fork instead of following a peer"
status: pending
priority: high
effort: large
type: improvement
dependencies: ["026"]
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

- [ ] Follow several peers and keep the candidate tips they report.
- [ ] Choose between candidates on chain length and valid signature weight
      against the burn view, not on arrival.
- [ ] Reorganize onto a heavier fork instead of failing.
- [ ] Use `/v3/tenures/fork_info` to find where a candidate diverged.
- [ ] Treat a peer that serves an invalid block as untrusted rather than fatal.

## Acceptance Criteria

- A peer that reorganizes does not stall nano.
- Given two candidate forks, nano selects the one stacks-core selects.
- No single peer can withhold the canonical tip from a node with other peers.
