---
id: "052"
title: "Wire the complete RPC and event surface into the node"
status: pending
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

- [ ] Construct and share the mempool, block/proposal channel, proposal token,
      reward sets and StackerDB configuration in runtime.
- [ ] Admit uploaded blocks and proposals through the same validator as followed
      blocks.
- [ ] Publish `new_block` only after execution, with nano's actual receipts,
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
