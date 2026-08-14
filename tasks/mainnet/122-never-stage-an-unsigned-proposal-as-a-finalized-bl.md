---
title: "Never stage an unsigned proposal as a finalized block"
id: "122"
status: in-progress
priority: critical
effort: medium
type: bug
group: mainnet
dependencies: ["079", "131"]
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
- [x] Start a fresh isolated Hacknet and prove nano executes the finalized block
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

## Fresh isolated reward-cycle evidence, 2026-08-14

The `task122n` Hacknet was started from empty project state with a 15-block
reward cycle and 10-block prepare phase. Nano imported an authenticated
checkpoint at Stacks height 360, served the captured cycle-18 set, and derived
the upcoming cycle-19 set from its own local burnchain during the prepare phase.
The hosted stock signer registered for both cycles.

After the network crossed into cycle 19, stock and nano agreed at Stacks height
436 and block `6d6f48547f8e71d49d5b80839b48a6f0cacb39ab7da0c5fb737e08eafe35d37e`.
The fetched finalized block was 535 bytes and carried two signer signatures.
Nano's `staging.sqlite` contained zero rows before and after the verification
gate, so no unsigned form shadowed the finalized block.

```text
hosted_signer::a_stock_signer_answers_proposals_through_nano        PASS
hosted_signer::a_transaction_posted_to_nano_is_admitted_and_reported PASS
hosted_signer::an_observer_on_nano_is_told_what_stacks_core_tells_its_own PASS
hosted_signer suite                                                  3 passed
shared stock/nano observer receipts                                  69 matched
hosted cycle-19 signer threshold                                     7/10 weight
staged rows before / after                                            0 / 0
```

This proves the fresh boundary and finalized-block behavior. The task remains
open until the complete infrastructure qualification is green.
