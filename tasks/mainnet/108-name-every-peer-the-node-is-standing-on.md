---
id: "108"
group: mainnet
title: "Name every peer the node is standing on"
status: in-progress
priority: medium
effort: small
dependencies: []
tags: ["mainnet", "p2p", "sync", "operations", "tui"]
created_at: 2026-08-10
type: bug
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

- [ ] De-duplicate the pool on the parsed URL, where every consumer already
      goes through (`PeerPool::from_endpoints`), so history fetching and
      StackerDB replication both get it.
- [ ] Publish the pool to `/nano/sync_status`: the endpoints being fetched
      from, and the p2p session/known counts discovery reports.
- [ ] Show it in the TUI: the sync source is one peer *of* a pool, with the
      pool's size and the p2p session count visible.

## Acceptance Criteria

- A peer both configured and discovered appears once in "fetching history
  from" and once in StackerDB replication.
- `/nano/sync_status` names the fetch pool and the p2p counts.
- The TUI dashboard shows pool size and p2p sessions alongside the sync
  source.
