---
id: "029"
title: "Serve the rest of the node RPC and the event dispatcher"
status: completed
priority: high
effort: large
type: feature
dependencies: ["028"]
tags: ["mainnet", "rpc"]
created_at: 2026-07-30
completed_at: 2026-07-30
---

# Serve the rest of the node RPC and the event dispatcher

## Objective

`nano-rpc` serves six routes: `/v2/info`, `/v2/pox`,
`/v3/sortitions/consensus/:hash`, `/v3/tenures/info`, `/v3/tenures/:id` and
`/v3/blocks/:id`. W12 named roughly fifteen, plus the event observer.

Missing: `/v2/accounts/:principal`, `/v2/contracts/call-read/...`,
`/v2/transactions`, `/v3/block_proposal`, `/v3/stacker_set/:cycle`,
`/v3/blocks/upload`, `/v2/stackerdb/...`, and every event POST — `new_block`,
`new_burn_block`, `stackerdb_chunks`, `proposal_response`,
`mined_nakamoto_block`.

Without them nothing else can use nano as a node: no wallet can submit, no
indexer can follow, stock `stacks-signer` cannot be pointed at it, and nano
produces no `new_block` payloads of its own to diff receipts against.

## Tasks

- [x] Serve the account, read-only call and transaction submission endpoints.
- [x] Serve `/v3/block_proposal` with its authorization header.
- [x] Serve the reward set, block upload and StackerDB endpoints.
- [x] Dispatch the event observer POSTs to configured observers.
- [x] Diff nano's own `new_block` payloads against stacks-core's in
      `nano-conformance`.
- [x] Hand the routes a live chain: `ChainState` owns the `Vm` privately and
      exposes no account nonce, so nothing but a bare `Vm` can implement
      `ChainAccess` yet.
- [x] Run the dispatcher from a node: the shipped binary executes mainnet blocks
      and posts their receipts to a real observer.

## Acceptance Criteria

- Stock `stacks-signer` runs against nano unmodified.
- An event observer receives the same payloads from nano and from stacks-core
  for the same blocks.
- The RPC surface is served from the executed state, not from a followed peer.

## The dispatcher has been run from a node, on mainnet

The premise of this item — a binary that follows without executing, so it has no
receipts to publish — stopped being true when
[[052-wire-the-complete-rpc-and-event-surface-into-the-n]] wired the executed state
into the RPC. What closed it is an ordinary mainnet catch-up with an observer
configured:

```
[node]
event_observers = ["http://127.0.0.1:20470"]
```

`hacknet/event-sink.py`, the same sink a fixture capture uses, recorded **3,200+**
`new_block` payloads from blocks the node executed and whose state roots the signed
headers verified — per-transaction receipts, statuses, all five cost dimensions and
every event, half a gigabyte of them.

Five hundred of those are frozen as the mainnet regression slice
(`fixtures/mainnet/receipts.json`, and `mainnet_receipts.rs` compares a run against
it), which is what makes this more than an anecdote about a log line: the payloads
were not merely emitted, they were read back and pinned.

What it does not close is the acceptance criterion above it — an observer receiving
*the same* payloads from nano and from stacks-core for the same blocks. That needs a
stacks-core observer on the same chain, which mainnet cannot provide retroactively;
the hacknet capture is where the two are diffed, and
[[060-make-the-consensus-execution-engine-explicit-and-r]] records why mainnet cannot
be.
