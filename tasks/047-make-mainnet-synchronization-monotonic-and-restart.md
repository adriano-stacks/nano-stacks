---
id: "047"
title: "Make mainnet synchronization monotonic and restart-safe"
status: in-progress
priority: critical
effort: large
type: bug
group: mainnet
dependencies: ["046", "048", "056", "057"]
tags: ["mainnet", "sync", "persistence"]
created_at: 2026-08-02
---

# Make mainnet synchronization monotonic and restart-safe

## Objective

Both sync layers can discard useful progress. `TenureFollower` drops a partial
same-tenure extension when its 32-block walk does not reach a moving tip. The
live node's partial response repeatedly stopped at height 8,689,223 while the
peer advanced, followed by `peer tenure does not extend the followed chain` on
every poll.

`CheckpointExecutor::follow_to_tip` is worse: it walks the entire gap backward
into a `Vec` before executing anything. One 429 discards the walk and the next
round begins again. The mainnet store still ends at its 8,665,601 anchor after
hundreds of failed attempts.

## Tasks

- [x] Retain every validated partial tenure extension and resume from its last
      block on the next poll.
- [x] Replace whole-gap buffering with bounded forward execution chunks.
- [x] Make rate limits and bounded peer pages end a round successfully after all
      available progress is committed.
- [x] Persist executed tip, parent links and tenure accounting together only for
      accepted blocks at each chunk boundary; this depends on
      [[056-make-rejected-block-execution-leave-no-state]] and
      [[057-commit-and-recover-accepted-block-state-atomically]].
- [x] Resume after an orderly process stop without refetching or re-executing
      sealed blocks.
- [x] Bound caches, response bodies and in-memory ancestry independently of
      distance from tip or peer-controlled response size.
- [ ] Test gaps spanning long tenures and multiple tenures with deterministic
      429s, short pages, tip movement and a restart after every chunk boundary.

## Acceptance Criteria

- Executed height advances monotonically from a checkpoint 20,000 blocks behind
  under forced rate limits and bounded responses.
- Restarting at any committed block produces the same final root and accounting
  as uninterrupted execution.
- A live mainnet soak crosses tenure boundaries without a permanent `Fork` loop
  and reports executed, rather than followed, lag.
- Repeated failure of the same candidate does not change or persist any
  accounting or chain context.

## The persistence checkbox was not true

The mainnet run retried the root mismatch at 8,665,780 1,417 times. Each attempt
added the block's 458,250 uSTX fee before root verification, and aborting the VM
did not roll that accounting back. The runtime then persisted it after catch-up
returned the error. The resulting tenure total is exactly the original 24,851
plus those 1,417 failed attempts.

The durable executed tip is still 8,665,779, so monotonic MARF progress is real,
but auxiliary state was not committed with it. The unchecked persistence item
now names the two transactional and crash-recovery tasks required to make that
claim true.

## Restarting reaches the same state

`crates/nano-conformance/tests/restart.rs` replays forty captured blocks twice:
once uninterrupted, and once in two halves with the chainstate closed and
reopened between them, resuming from the accounting the first half wrote out.
Both reach the same sealed tip and owe the same.

That is the property a catch-up depends on, and it is checked offline against
the captured fixture rather than by stopping a live node and hoping. The
remaining unchecked item is a deterministic harness for rate limits and short
pages, which the live mainnet run exercises but no test yet pins.

## The derived sortition chain is written down now

It was not, and a chain that is not written down is re-derived from the
checkpoint's burn anchor on every start over a run that grows for as long as the
chain does. `SortitionTracker::save` writes the tip and the whole consensus-hash
history in the capture's own format, so `resume_or_capture` is the same loader
either way and a saved chain cannot be read more loosely than a captured one. It
is written as the chain advances rather than at shutdown, because a node that is
killed is exactly the one that must not start over, and through a rename so a torn
history and a tip that does not end it can never be left behind.

**A correction, because an earlier version of this note got it wrong.** It said
the minutes of silence after a restart *were* that re-derivation, measured. That
was an inference from the last log line before the silence, and the code
contradicts it: `check_local_sortition` returns early unless the peer's sortition
is exactly one above the tracker's tip, and with the tracker seeded at burn
960,219 while the replay executes under burn 960,25x that condition never holds —
so the tracker was not being walked at all. The header backfill is not it either;
it prints per ancestor and printed nothing. What the silence actually was is not
yet measured, and measuring it needs a replay that is not stopped at a
divergence, which this one is.

Persisting the chain is still right, and is done. Attributing minutes to it was
not.

## Measured feedback-loop costs, and the standing lesson

Numbers from 2026-08-05, kept here rather than in a separate document because the
task list is where this project records things.

| | before | after |
|---|---|---|
| node startup silence on a mainnet state | 6+ min | 20 s |
| state snapshot, making an experiment reversible | 4.5 h re-import | 3 s, no extra disk |
| release-dependency audit | 243 s | 16 s |
| rebuild after a one-line change to a hot crate | 303 s CPU | 202 s CPU |
| free disk | 494 GB (76% full) | 962 GB (53%) |

The startup cost was `open_chainstate` walking `parent_of` to the root of the
MARF — a checkpoint import brings the whole ancestry, so 8.6 million SQLite
lookups against a 23 GB database, building a 277 MB list, to use the first entry
unless a peer had lost our tip. Bounded to 256.

`target/debug` was 452 GB against 8.8 GB of release, in a workspace that never
builds debug on purpose; half of why it grew was a conformance test using
`cargo check --all-targets`. `vendor/clarity-wasm/target` was a second 15 GB build
of the same graph — `cargo test -p clar2wasm` works from the workspace root, so it
never needed to exist.

The 28 conformance test targets became one: 303 s → 202 s CPU per rebuild, with an
empty test-name diff either side. Nothing in the suite needed its own process; the
reason previously given for not doing it (`oom_checker`) turned out to be a
clar2wasm test under `vendor/`, never one of the 28.

`[profile.loop]` exists — `inherits = "release"` with `incremental = true` — and is
deliberately **not** adopted, because that flag changes codegen-unit partitioning
and this node's replay throughput is worth measuring before trading. Iterate with
`--profile loop`; leave `release` as the profile whose numbers mean something.

**The standing lesson, which cost most of a day.** Four attributions of a
performance problem were wrong, each fixed only by sampling the live process:
restart cost blamed on sortition re-derivation (the tracker could not advance
there at all); "the follower exits after each round" (that message is the SIGTERM
handler — a harness timeout was killing the process group); "all cores saturated"
(the process was at 16% of one core waiting on the network); and a dead
`event_observers` entry pointing at the node's own RPC port, costing five retries
per block, which corrupted every throughput number until it was found. Sample
`/proc/<pid>/stat` and `/proc/<pid>/io` before believing any story about where
time goes.

## The persistence checkbox is true now, and what made it true

Both tasks it named have closed. The ledger — executed chain, tenure start
heights, accounting, reorganization reach, parent tenure proof — is written in the
same transaction that seals the block's state root, with `prepare_commit` at
`synchronous = FULL` and `marf.seal_to` as the decision, so a hard kill leaves
either the complete parent or the complete child and never a mixture. Twenty
scattered `SIGKILL`s and eight aimed at tenure transitions each reopen with a
ledger whose executed suffix ends exactly at the tip that has state, and the
survivor replays forward to the same final root as an uninterrupted run.

A rejected block now rolls its fees back with everything else, and the retry loop
that put 1,417 attempts' fees into a tenure total cannot recur.

**One thing this exposed that no test covered:** the recovery path validated the
ledger not at all. `check_maturity_window` ran on the checkpoint and on
`accounting.json` and not on a recovered ledger — the one artifact a running node
executes from. It does now. Written up on
[[048-carry-complete-mainnet-tenure-accounting]], because the hole it found was
that task's.

The two items still open are the memory bounds — `TenureAccounting::earnings` and
`BitcoinContext::headers` both grow with the chain — and a deterministic harness
for rate limits and short pages, which the live mainnet run exercises and no test
pins.

## The three things that grew with the chain

All three were in-memory structures with no relationship to distance from the
tip, and one of them was also a **per-block cost**, which is what makes this a
throughput item and not only a leak.

**`TenureAccounting::earnings`** is serialized into the chain ledger with every
block, so letting it grow meant a write that got slower forever — about 130 bytes
a tenure, and mainnet adds a tenure every ten minutes. Bounded at 201, which is
the deepest read (`matured - 1`, 101 below the tip) plus a full maturity window of
margin, and the same 201 a checkpoint has to carry. Not a coincidence: both
numbers are "what a node can still be asked to pay". Pruning the bottom is safe
because every reader counts down from the top, and a tenure's `block_reward`
outlives the map in the header store, which records it when the block is sealed.

**`BitcoinContext::headers`** and **`burn_headers`** are caches in front of the
side store, consulted because a block being executed is asked about before it is
written. Both fall through to the store on a miss, which is what makes bounding
them safe rather than merely smaller. `headers` is keyed by block identifier — a
hash — so the map's own ordering says nothing about staleness and the insertion
order is kept beside it; `burn_headers` is keyed by height, where it does.

**The test asserts both halves, deliberately.** A bound that let the map grow
would pass a loosely written length assertion, and a bound that pruned what
`effects_for_tenure` reaches would pass a tightly written one — so
`earnings_are_bounded_and_a_payout_still_resolves` walks a thousand tenures past
the cap and *then* asks for the payout at the top, which reads 101 down, and
checks the span a startup validation reads is still long enough to judge.

Writing it caught something worth recording: the expectation was drafted on the
pre-8,673,846 fee rule and the test refused it. The second credit's amount and
recipient both come from `earnings[matured - 1]`, and the tree is right.

## The MARF node cache was a fifth of one block's working set

The replay was reading **900 MB per block** — 145 MB/s of `rchar` against a 30 GB
`marf.sqlite` — and running at about 40 blocks a minute. Not a missing index: the
node table is `WITHOUT ROWID` on `(block, idx)` and the plan is a primary-key
search. It was the cache.

A MARF lookup walks back-pointers into ancestor states, and a node standing on a
checkpoint has 8.6 million ancestors, so one block's working set is on the order of
200,000 nodes. `NODE_CACHE` was 20,000 per generation — under a fifth of that — so
the cache thrashed on every block and the same nodes were decoded again for the
next one. And `node_hash` had **no cache at all**, on a path that is read with the
fanout of the trie: sealing a block hashes every node it touched, and each preimage
needs every sibling's hash.

Both are immutable per `(block, index)` — a state is addressed by the block that
sealed it, so nothing rewrites one — which is what makes caching them safe rather
than merely faster.

| | before | after |
|---|---|---|
| replay throughput | ~40 blocks/min | **140 blocks/min** |
| read volume | ~900 MB/block | ~250 MB/block |
| execution, per block | 1.1 s | 0.30 s |
| resident memory | ~2.5 GB | 3.3 GB |

Three and a half hours to mainnet tip instead of twelve.

**One thing that had to go in with it**, and is the reason a cache like this is not
free: `forget()` drops every cache after a rolled-back write, because the cache
would otherwise address rows the database no longer holds. A stale *hash* is worse
than a stale node — it goes straight into a parent's preimage and moves a root — so
the new cache is cleared there too. `marf_lockstep` and the whole conformance suite
are what say it did not change an answer.

`local` is still 0.06 s a block, halved but not gone; that is
[[049-derive-canonical-sortitions-from-the-local-burncha]]'s.
