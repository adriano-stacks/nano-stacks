---
id: "096"
group: mainnet
title: "Cross a Stacks fork inside one sortition chain"
status: completed
priority: critical
effort: medium
dependencies: ["047", "049"]
tags: ["mainnet", "sync", "fork-choice", "liveness", "release"]
created_at: 2026-08-09
type: bug
completed_at: 2026-08-09
---

# Cross a Stacks fork inside one sortition chain

## Objective

A follower whose tip is orphaned by the *next* sortition can never rejoin the
chain. Every recovery path this node has names a fork point by **tenure**, and
this fork has no tenure to name: both branches stand on the same, unreorganized
sortition history. The node holds the whole canonical branch on disk, executes
none of it, and stays where it is for as long as it runs.

## Evidence

`/home/aldur/mainnet-tip`, binary revision `51ab2bcc`, stalled 17 hours at
Stacks height 8724697 while the network reached 8726174 — 728 consecutive rounds
that executed nothing:

```
burn view c3e49a756335af51bc4be85d96db6b3a234578d6: 34 blocks from http://172.96.141.17:20443/
executing the peer's chain failed: node synchronization failed: HTTP sync error:
  HTTP status client error (400 Bad Request) for url
  (http://172.96.141.17:20443/v3/tenures/fork_info/c3e49a…/234d42…)
executed nothing: sealed at 8724697, then the round failed
```

What actually happened on mainnet, from the burnchain and the peers:

| burn | consensus hash | block at 8724697 | parent |
|---|---|---|---|
| 961648 | `4996f757` | — | — |
| 961649 | `43741340` | `7288f7dc` — **this node's tip** | `cca117bb` (8724696) |
| 961650 | `5b2bf6a0` | `c2b7cbfd` — **the network's** | `cca117bb` (8724696) |

The 961650 miner committed to parent tenure `4996f757`, not to `43741340`: it
did not build on the block this node had executed. Both blocks are height
8724697, both tenures are canonical burn views, and the fork point is a *block* —
`cca117bb` at 8724696 — that no burn view names.

Staging held 1509 blocks, 8724698 … 8726206, and could execute none of them: the
one block linking them to the fork point, `c2b7cbfd`, was missing.

## Why nothing recovered

Five defects, of which the first three are what strand the node:

1. **The descent's gap test is a height comparison.** `descent_resumes_at` is
   filtered on `lowest > executed_height + 1`. The lowest staged block *was*
   `executed_height + 1`, so the resume was skipped every round — but its parent
   was this node's tip's **sibling**, not this node's tip. Height cannot answer
   "is there a gap"; parent identity can.
2. **`remove_to(executed_height)` deletes the fork's root.** The one staged block
   that bridges to the fork point sits at exactly the executed tip's height, so
   the round that staged it deleted it before the round that needed it.
3. **`switch_to_fork` speaks only in tenures.** `fork_point_of` matches this
   node's own tip tenure `43741340` — it is on the peer's sortition fork —
   `retract_to` discards nothing, and the switch answers `None`. Its own doc says
   two branches inside one tenure cannot be resolved this way; this is two
   branches inside one *sortition chain*, which is the same wall.
4. **The running binary inverts stacks-core's `fork_info` bounds**, so the walk
   answers 400 `NotInSameFork` and the `?` fails the whole round — the log line
   above. Fixed at HEAD by `a383ab59`; the node was 26 commits behind and had
   never been restarted on it.
5. **The `fork_info` walk is a single request.** stacks-core truncates at
   `DEPTH_LIMIT = 10` sortitions *without saying so*
   (`stackslib/src/net/api/get_tenures_fork_info.rs:38`), so one answer reaches
   burn 961671 while the fork point is at 961648. A tenure-level fork deeper than
   ten sortitions is indistinguishable from no fork at all.

## Acceptance

- The gap test asks whether the lowest staged block's parent is a block this node
  executed, not how high it is.
- A staged branch that is higher than the executed tip and descends from a block
  this node executed retracts to that block and executes — fork choice by block,
  decided locally, with no peer able to name a block this node did not execute.
- Staging keeps siblings of the executed tip.
- The `fork_info` walk pages past `DEPTH_LIMIT`, and a peer's answer to it can
  never fail a catch-up round.
- Conformance covers a peer whose branch parts inside one sortition chain, and
  the harness truncates `fork_info` at ten sortitions as stacks-core does.
- `/home/aldur/mainnet-tip` rejoins the canonical chain. Done: it gave back the
  orphan and executed 8724696 → 8724864. Holding tip is not this task's to
  finish — it is blocked on
  [[098-read-a-trait-reference-back-out-of-a-nested-value]].

## Hardening completed

The live recovery exposed three commitments that the first retraction-only test
did not prove, so this task is reopened rather than treating one successful
operator restart as the implementation boundary:

- [x] The conformance fork test must run the next catch-up round and execute the
      retained replacement branch to its higher tip/root, not stop after giving
      the orphan back.
- [x] Staging must select one coherent linked branch deterministically when it
      retains siblings; `highest`, descent resumption and `child_of` may not each
      choose unrelated rows of the same height.
- [x] Fetch and validate the local ancestor before mutating durable chainstate,
      and save the sortition tracker one burn view behind execution so a kill at
      the retraction boundary can restart on the common parent.

The exact fork gate now creates one fixture-only orphan, retains the byte-exact
captured replacement branch, retracts to the precise common block, drops and
reopens the executor from the saved sortition state, and executes the replacement
to its higher tip with staging empty:

```
cargo test -p nano-conformance --test conformance \
  follow_path::a_branch_that_parts_at_a_block_is_followed_onto_the_fork \
  -- --exact --nocapture --test-threads=1
# 1 passed

cargo test -p nano-node \
  staging::tests::competing_siblings_select_one_coherent_longest_branch \
  -- --exact --nocapture
# 1 passed

cargo test -p nano-node \
  sortition::tests::a_saved_tracker_can_restart_one_burn_view_before_execution \
  -- --exact --nocapture
# 1 passed

cargo test -p nano-conformance --test conformance \
  follow_path::a_burn_view_walk_reaches_past_one_answer \
  -- --exact --nocapture --test-threads=1
# 1 passed
```

The frozen batch also passes the complete module and strict Clippy:

```
cargo test -p nano-conformance --test conformance follow_path:: \
  -- --nocapture --test-threads=1
# 6 passed

cargo clippy -p nano-chainstate -p nano-node -p nano-sync \
  -p nano-conformance --all-targets -- -D warnings
# passed
```

The live node retracted the orphan and executed through 8724864; its next
refusal is the independent VM defect tracked by task 098.
