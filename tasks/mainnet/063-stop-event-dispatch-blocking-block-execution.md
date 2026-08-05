---
id: "063"
title: "Stop event dispatch blocking block execution"
status: pending
priority: high
effort: small
type: bug
group: mainnet
dependencies: ["029", "052"]
tags: ["mainnet", "rpc", "events", "performance"]
created_at: 2026-08-05
---

# Stop event dispatch blocking block execution

## Objective

`EventDispatcher::post` retries five times with 0/100/200/300/400 ms backoff, and
it is awaited inline per block from the executor. Against an observer that does
not answer that is about **a second of sleeping per block** — measured, and it was
most of the 28-34 blocks/min a mainnet replay showed for a whole day.

The immediate cause was a configuration error: `event_observers` pointed at the
node's own RPC port, which does not serve `/new_block`. That is fixed. The design
is the remaining hazard — a node's block execution must not be gated on an HTTP
POST to a third party, and a slow or dead observer must cost throughput
approximately nothing and must never stall the chain.

## Tasks

- [ ] Hand events to a task or queue that drains independently, so
      `execute_staged` returns as soon as the block is durable.
- [ ] Decide the backpressure policy and make it **visible**. Unbounded is a
      memory leak on a dead observer; dropping is a silent hole in an observer's
      view of the chain. Either way an observer that missed events must be able
      to tell, and the node must say so.
- [ ] Establish whether `new_block` must arrive in height order for the stock
      `stacks-signer` and the Hiro API before choosing anything concurrent.
- [ ] Decide whether a durably executed block's events may be lost on a clean
      shutdown, and record the reasoning.

## Acceptance Criteria

- An observer that refuses connections costs almost no throughput, measured on
  the same snapshot against a run with no observer at all.
- A live observer still receives every event, in the same order the inline path
  produced, asserted against a recording sink.
- A slow observer does not stall execution, and the backpressure policy is
  observable in the log.
- What an observer is owed by tasks 029 and 052 is unchanged.

## Where it came from

Found while chasing replay throughput on 2026-08-05, after three wrong
attributions. The process was at 16% of one core and 7 MB/s of disk while
executing six blocks in thirty seconds — not CPU-bound, not IO-bound, asleep.
