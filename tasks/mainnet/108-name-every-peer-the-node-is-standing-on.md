---
id: "108"
group: mainnet
title: "Name every peer the node is standing on"
status: completed
priority: medium
effort: small
dependencies: []
tags: ["mainnet", "p2p", "sync", "operations", "tui"]
created_at: 2026-08-10
type: bug
completed_at: 2026-08-10
---

# Name every peer the node is standing on

## Objective

Two related honesty problems in how the node reports its peer pool:

1. **The pool can hold the same peer twice.** `follow_endpoints` de-duplicates
   configured against discovered peers by raw string, but a configured
   `http://host:20443/` and the same peer's handshake-advertised
   `http://host:20443` differ only in what `Url::parse` normalizes away — so
   the pool held `172.96.141.17` twice, requests double up on that host, and
   the per-peer serving attribution the release evidence needs counts one peer
   as two.
2. **The TUI makes the node look single-homed.** It shows only
   `selected_from_peer` from `/nano/sync_status`, so a node fetching from
   seven peers over four p2p sessions reads as "connected to one peer".
   `/nano/sync_status` says nothing about the pool at all.

## Evidence

2026-08-10, `/home/aldur/mainnet-tip` run log:

```
fetching history from 8 peers: http://172.96.141.17:20443/, …,
  http://172.96.140.79:20443/, http://172.96.141.17:20443/, …
```

— eight entries, seven distinct. The TUI meanwhile showed only
`sync source http://172.96.140.79:20443/`.

## Tasks

- [x] De-duplicate the pool on the parsed URL, where every consumer already
      goes through (`PeerPool::from_endpoints`), so history fetching and
      StackerDB replication both get it. Regression test pins the two
      spellings of one peer to one pool entry.
- [x] Publish the pool to `/nano/sync_status`: the endpoints being fetched
      from, and the p2p session/known counts discovery reports.
- [x] Show it in the TUI: a `peer pool` line under the sync source with the
      pool's size, live p2p sessions and known peers.
- [x] Deploy after the old 107 diagnostic window showed the remaining
      whole-tenure clone and was no longer release evidence. Clean commit
      `063215c6f4e62f78901c844b86c67fe9e43719d7` built artifact SHA-256
      `ef6629d966e110974beabc543a51399f451de7a41875e70379b1441f8c82acae`;
      the exact binary is retained as `stacks-node.063215c6` and deployed as
      `/home/aldur/mainnet-tip/stacks-node`.

## Acceptance Criteria

- A peer both configured and discovered appears once in "fetching history
  from" and once in StackerDB replication.
- `/nano/sync_status` names the fetch pool and the p2p counts.
- The TUI dashboard shows pool size and p2p sessions alongside the sync
  source.

## Deployment evidence

The process started at `2026-08-10T15:50:41Z`. Its first live report named four
unique fetch endpoints, four P2P sessions and 97 known peers. The startup log
names the same four unique endpoints for both `fetching history` and
`replicating StackerDB chunks`; the duplicate trailing-slash spelling is gone.
The TUI consumes those exact `fetching_from_peers` and `p2p_sessions` fields on
its `peer pool` row.
