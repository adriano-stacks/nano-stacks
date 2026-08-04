---
id: "059"
title: "Answer block-info for blocks older than the checkpoint"
status: pending
priority: critical
effort: medium
type: feature
group: mainnet
dependencies: ["037"]
tags: ["mainnet", "checkpoint", "vm"]
created_at: 2026-08-04
---

# Answer block-info for blocks older than the checkpoint

## Objective

Mainnet replay stops at **8,669,750**:

```
FATAL: no burnchain block height found for Stacks block dd254a16…
```

A contract called `get-block-info?` on an older height. The id resolves fine —
that block is at Stacks height **8,661,474**, and nano's MARF holds it, along
with all 8,669,751 entries the checkpoint imported. What nano does not hold is
its **header**: `block_header` carries 4,150 rows, only from the checkpoint
forward.

So the MARF import is not at fault. The headers table beside it is, and a
checkpointed node has to answer for history it did not execute.

## Why this cannot be guessed

The stall is inside `ClarityDatabase::get_stacks_epoch_for_block`
(`clarity_db.rs:2629`), reached from the `get-block-info?` / `get-tenure-info?`
paths (`1265, 1318, 1333, 1341, 1383`). Those sites want the epoch only to ask
`uses_nakamoto_blocks()` — but they are *about to read a real field*.

That rules out the cheap fix. Synthesising a header to get past the epoch check
makes the contract read invented values, and a contract that reads a wrong
miner address or block time answers wrongly and seals a wrong root. **A wrong
answer is worse than the stall**, because the stall is loud and a wrong answer
is a divergence somewhere else entirely.

It also rules out deriving the epoch from a pinned Stacks-height boundary: it
would clear the check and then feed exactly those invented fields.

## What is available

The capture holds 100 blocks from 8,665,601, so this block is not local; it has
to come from a peer. Everything needed is served by a stock node — no Hiro:

| `BlockHeader` field | source | exact? |
|---|---|---|
| `consensus_hash` | `/v3/blocks/:id` header | yes |
| `stacks_block_time` | `/v3/blocks/:id` header timestamp | yes |
| `block_header_hash` | `block_hash()` of that header | yes |
| `burn_spend_total` | header `burn_spent` | yes |
| `burn_header_hash` | `/v3/sortitions/consensus/:ch` | yes |
| `burn_block_height` | same | yes |
| `burn_block_time` | same | yes |
| `vrf_seed` | same | yes |
| `miner_address` | sortition `miner_public_key_hash` | needs checking |
| `burn_spend_winner` | — | no |
| `block_reward` | — | no |
| `tenure_height` | walk the tenure | no |
| `tenure_start_height` | walk the tenure | no |

`SyncClient::block` and `SyncClient::sortition` already exist and are already
cached, so the first eight are a small change.

## Tasks

- [ ] Backfill a missing header from a peer in the node's retry loop: the block
      id is in the error, the block already stalls and retries, and the fetch is
      async there rather than inside the VM's synchronous `HeadersDB`.
- [ ] Derive every field the table above marks exact, and persist to
      `block_header` so a header is fetched once.
- [ ] Establish each remaining field or prove it unreachable. Leave the node
      stalling on a field that cannot be derived rather than inventing it, and
      say which field stalled it.
- [ ] Oracle: for a block nano *does* hold, backfill it from a peer anyway and
      assert the reconstructed header equals the recorded one, field by field.
      That is what says a reconstruction is trustworthy before it is used for
      one that cannot be checked.
- [ ] Decide whether the checkpoint should carry pre-checkpoint headers instead,
      and record why. Fetching is incremental and needs a live peer; carrying
      them is offline and bounded but grows the checkpoint.

## Acceptance Criteria

- Replay passes 8,669,750 and the contract's `get-block-info?` answer matches
  mainnet's receipt for that transaction.
- A reconstructed header equals a known one field by field, in CI.
- No field is ever synthesised: a header nano cannot rebuild exactly stops the
  node and names the field.
- Header fetches are cached, so a replay does not refetch per attempt.
