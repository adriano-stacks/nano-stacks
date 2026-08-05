---
id: "063"
title: "Stop event dispatch blocking block execution"
status: completed
priority: high
effort: small
type: bug
group: mainnet
dependencies: ["029", "052"]
tags: ["mainnet", "rpc", "events", "performance"]
created_at: 2026-08-05
completed_at: 2026-08-05
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

- [x] Hand events to a task or queue that drains independently, so
      `execute_staged` returns as soon as the block is durable.
- [x] Decide the backpressure policy and make it **visible**. Unbounded is a
      memory leak on a dead observer; dropping is a silent hole in an observer's
      view of the chain. Either way an observer that missed events must be able
      to tell, and the node must say so.
- [x] Establish whether `new_block` must arrive in height order for the stock
      `stacks-signer` and the Hiro API before choosing anything concurrent.
- [x] Decide whether a durably executed block's events may be lost on a clean
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

## What it is now, and the four decisions

`EventDispatcher::dispatch` no longer awaits anything. It serializes the payload
once, shares the bytes, and hands them to **one bounded queue and one drain task
per observer**. `execute_staged` returns as soon as the block is durable, and the
round's own timing line says so: `dispatch 0.05s` over 100 mainnet blocks, which
is the serialization and the enqueue and nothing else.

Each of the four questions had an answer that follows from what a node is for.

**Order: per observer, exactly dispatch order.** One queue per observer rather
than one pool of workers, because an indexer applying `new_block` needs the
parent before the child and a `stacks-signer` reads its slots as a sequence of
state transitions. Nothing concurrent *within* an observer, so the question of
whether height order is required never has to be answered optimistically.
Observers do not wait on each other, which is the only concurrency there is.

**Backpressure: bounded in bytes, drop the newest, and say so.** Unbounded is a
memory leak on a dead observer; a silent drop is a hole in its view of the chain.
So the queue is bounded — in bytes rather than events, because payloads span
three orders of magnitude, from a couple of hundred bytes for an empty
`stackerdb_chunks` to hundreds of kilobytes for a mainnet `new_block`, and any
event count either wastes the budget or blows through it. 32 MB is several
hundred mainnet blocks of slack, so an observer that restarts or pauses for a GC
catches up with no gap at all.

An observer that *is* dropped from finds out two independent ways, both in the
headers of an event it did receive: `x-nano-event-seq` counts every event
offered, so a gap in it is exactly where the node dropped something, and
`x-nano-events-dropped` carries the running count. The body stays stacks-core's
byte for byte, so an observer that does not read either header is unaffected. The
node also complains on stderr, at most once every 30 seconds — per event it would
be a line per block for as long as the observer stayed down.

**A dead observer is tried once, not five times.** The backoff exists to give a
transient failure a second chance; spending it on every event of an observer
known to be down only fills its queue faster. Same reasoning for an observer that
*answers* 4xx: a 404 says it serves no such endpoint and a 4xx that it will not
have this payload, so asking again cannot change its mind. Only a request that
never arrived, or one the observer failed to handle, is repeated.

**Events may be lost on a clean shutdown, deliberately.** `settle(timeout)`
gives the queues a bounded moment and then says what it abandoned. Waiting
without a limit is the stall this whole queue exists to remove, and an event is
not chain data: a block is durable before its event is dispatched, and an
observer's own record of what it last saw is how it asks for the rest. A node
that would not exit until a third party accepted an HTTP POST would be worse.

## What is measured, and what is not

At the dispatcher: 50 events at an observer refusing connections cost under
100 ms to dispatch, against the ~50 s of backoff the inline path would have
spent (`dispatching_to_a_dead_observer_does_not_wait_for_it`). A slow observer —
50 ms per request — falls behind and is dropped from without stalling the
dispatcher, and the events it does receive carry the gap
(`an_observer_that_falls_behind_is_dropped_from_and_told_so`). A live observer
receives every event in dispatch order
(`every_event_reaches_the_observer_on_its_own_path`,
`a_slow_observer_receives_every_replayed_block_in_order`).

**Not** measured: the same comparison at the *node* level, two runs over one
snapshot with and without a dead observer. It would be a stronger statement of
the acceptance criterion and it is not what found the bug — the timing line and
`/proc/<pid>/io` were. Recorded as not done rather than implied by the
dispatcher numbers.
