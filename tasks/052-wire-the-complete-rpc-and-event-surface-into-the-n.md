---
id: "052"
title: "Wire the complete RPC and event surface into the node"
status: in-progress
priority: high
effort: large
type: feature
group: mainnet
dependencies: ["029", "046", "047", "050"]
tags: ["mainnet", "rpc", "events"]
created_at: 2026-08-02
---

# Wire the complete RPC and event surface into the node

## Objective

[[029-serve-the-rest-of-the-node-rpc]] implemented routes and event builders,
but runtime constructs `RpcState` with only `with_chain`. There is no mempool,
block sink, proposal token or published reward set, and followed blocks do not
dispatch `new_block`. The available endpoints are consequently unavailable,
empty, or backed by different tips.

Wire the implemented pieces into one node after consensus validation and
execution.

## Tasks

- [x] Construct and share the mempool in runtime.
- [ ] Construct and share the block/proposal channel, proposal token, reward sets
      and StackerDB configuration in runtime.
- [ ] Admit uploaded blocks and proposals through the same validator as followed
      blocks.
- [x] Publish `new_block` only after execution, with nano's actual receipts,
      costs and events.
- [ ] Dispatch burn-block, signer, proposal-response and mined-block events from
      their production transitions.
- [ ] Serve every route from the coherent executed snapshot established by
      [[046-distinguish-followed-and-executed-chain-tips]].
- [ ] Exercise a stock `stacks-signer`, transaction submitter and event observer
      against the binary.

## Acceptance Criteria

- A stock signer runs against nano without an RPC compatibility shim.
- Transaction submission, block proposal/upload, reward-set and StackerDB routes
  are live rather than `Unavailable` or empty defaults.
- An observer receives receipt-equivalent payloads for the same executed blocks
  as stacks-core.
- No RPC endpoint advertises or mutates state newer than the executed tip.

## `new_block` now leaves the node

The dispatcher was built from the configuration and then handed to the miner
alone, so a node that only follows executed every block in silence. It is now
given to the **executor**, which is the only part that knows a block was
executed rather than merely downloaded, and every applied block is announced
from there.

The payload is built synchronously and dispatched with owned values: holding the
chainstate across the await makes the future non-`Send`, since a `ChainState`
carries `RefCell`s and a sqlite connection.

Only the fields a follower can answer are filled in — the parent, the burn block
and its height, and the unlock heights — and the rest are left at their defaults
rather than invented. An observer comparing nano against stacks-core is better
served by a field that is plainly absent than by one that is confidently wrong.
Filling in the matured rewards, the reward set and the miner's winning txid is
the remaining work on this item.

`tests/event_observer.rs` already checks nano builds the same payload stacks-core
published, which is the harder half and says nothing about whether anything sends
it. `tests/event_delivery.rs` is the other half: a listener, a dispatch, and the
body arriving.

## One mempool, shared

The miner built its own `Mempool` and the RPC had none, so `/v2/transactions`
answered `Unavailable` and there was nowhere for a submitted transaction to go.
Worse would have been giving the RPC a second pool: a node that admits
transactions the miner cannot see accepts them and never mines them, which reads
as acceptance and behaves as a black hole.

So the runtime builds one and hands it to both. The miner takes the lock only
while it touches the pool — it awaits a peer between those points, and the RPC
admits into the same pool meanwhile.

`/v2/transactions` now decodes and rejects (`400`, "failed to decode
transaction") rather than reporting itself unavailable, which is the route being
live.

## `/v2/info` answered `503` with a perfectly good tip

It asked a *peer view* for one field — the chain identifier — and refused the
whole request when no peer had been heard from, even with an executed tip sitting
right there, which `/nano/sync_status` reported happily.

The chain a node is on is its own configuration; no peer needs to be asked. With
it taken from there, `/v2/info` answers from the executed tip alone:

```json
{"burn_block_height":960248,"stacks_tip_height":8666422,
 "stacks_tip":"3b0e826e…","stacks_tip_consensus_hash":"61f3f614…"}
```

That is also the acceptance criterion about no route advertising state newer than
the executed tip: this one now cannot, because the tip is the only thing it
reads.
