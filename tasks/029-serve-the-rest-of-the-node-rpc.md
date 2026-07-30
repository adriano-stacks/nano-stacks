---
id: "029"
title: "Serve the rest of the node RPC and the event dispatcher"
status: pending
priority: high
effort: large
type: feature
dependencies: ["028"]
tags: ["mainnet", "rpc"]
created_at: 2026-07-30
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

- [ ] Serve the account, read-only call and transaction submission endpoints.
- [ ] Serve `/v3/block_proposal` with its authorization header.
- [ ] Serve the reward set, block upload and StackerDB endpoints.
- [ ] Dispatch the event observer POSTs to configured observers.
- [ ] Diff nano's own `new_block` payloads against stacks-core's in
      `nano-conformance`.

## Acceptance Criteria

- Stock `stacks-signer` runs against nano unmodified.
- An event observer receives the same payloads from nano and from stacks-core
  for the same blocks.
- The RPC surface is served from the executed state, not from a followed peer.
