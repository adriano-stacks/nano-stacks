---
title: "Never stage an unsigned proposal as a finalized block"
id: "122"
status: in-progress
priority: critical
effort: medium
type: bug
group: mainnet
dependencies: ["079"]
tags: ["consensus", "signer", "rpc", "staging", "release"]
created_at: "2026-08-14"
---

# Never stage an unsigned proposal as a finalized block

## Objective

An unsigned Nakamoto proposal must be transient input to the proposal validator,
never a block offered to execution or peers. A proposal and its signer-finalized
block have the same block ID, because signer signatures are excluded from that
identity. Staging the proposal can therefore make a later descent stop at the ID
while retaining the unsigned bytes.

## Tasks

- [x] Keep proposal validation on the proposal-validator channel and keep the
      unsigned proposal off the finalized block sink and relay.
- [x] Pin the successful validator path: it emits an `Ok` proposal verdict but
      offers no block to staging.
- [x] Re-run the stock-signer proposal gates and strict Clippy.
- [ ] Start a fresh isolated Hacknet and prove nano executes the finalized block
      after validating its unsigned proposal, including across the next reward
      cycle boundary.

## Acceptance Criteria

- An approved proposal never appears on the block-upload sink.
- The finalized form of that proposal is fetched, authenticated with signer
  threshold, and executed rather than hidden behind the proposal's block ID.
- A fresh hosted-signer Hacknet crosses its reward-cycle boundary without an
  unsigned row at the selected tip in `staging.sqlite`.
- The complete infrastructure qualification is green.

## Evidence that opened this task

The 2026-08-14 release qualification stopped at Stacks height 273 while both
stock nodes were at 274. The stock `/v3/blocks` response for
`2e692ef74235a22e60ef07693a79ff5e8bcc0fb1989be2324ea2a71c95c83213`
was 600 bytes and carried three signer signatures. Nano's live
`staging.sqlite` held the same block ID at height 274 as 405 bytes, with the
signature count at wire offset 206 equal to zero. Execution consequently
reported `InsufficientWeight` forever. The RPC proposal path had enqueued the
proposal on the same sink as finalized block uploads before the proposal
validator answered.

## Code boundary proven, 2026-08-14

`judge_proposal` now sends an unexecuted candidate only to `ProposalRequest`.
`RpcState::blocks` is again solely the sink for an authenticated
`/v3/blocks/upload` block. The exact successful-validator regression answers the
request with `Ok`, observes the `Ok` proposal event, and proves the finalized
block channel is empty.

```text
nano-rpc tests::a_proposal_never_enters_the_finalized_block_sink  1 passed
nano-rpc --lib                                                   38 passed; 1 infrastructure ignore
nano-conformance proposal_failover::                             2 passed
clippy nano-rpc,nano-node --all-targets -D warnings              PASS
taskmd validate                                                  115 valid
```

The original Hacknet remains preserved as failure evidence. Its unsigned staged
row is not edited away. The final task item needs a new isolated network built
from this source, because continuing the old state would test manual repair rather
than prevention.
