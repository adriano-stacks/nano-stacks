---
id: "037"
title: "Replay mainnet from the epoch 4.0 boundary"
status: pending
priority: critical
effort: large
type: feature
dependencies: ["020", "021", "022", "023", "024", "025", "048"]
tags: ["mainnet", "replay", "conformance"]
created_at: 2026-07-30
---

# Replay mainnet from the epoch 4.0 boundary

## Objective

The milestone that decides whether any of the rest of it worked. M10 proved nano
computes the same chain state as stacks-core for 340 Hacknet blocks from a
regtest checkpoint. This is the same claim against the chain that matters.

Everything before it is a component check. The oracle is the same as M10's —
`state_index_root` per header and every receipt from the event observer — pointed
at mainnet blocks after the 4.0 boundary instead of captured Hacknet ones.

Replay depth is the metric again, and it stays at zero until the blockers this
depends on are done.

## Tasks

- [x] Capture a mainnet checkpoint at or after the 4.0 boundary, with the
      blocks and the burn blocks that follow it. Receipts need an observer.
- [x] Teach the fixture tooling and the scoreboard about a mainnet capture.
- [x] Make `import_checkpoint` work in bounded memory, so a mainnet-sized MARF
      can be imported at all.
- [ ] Replay forward and report the first divergence with the field that
      diverged.
- [ ] Work the divergence point forward until it stops moving for a real reason
      or reaches the tip.
- [ ] Keep a bounded slice of the capture in CI as a regression gate.
- [x] Check what mainnet *can* serve without a chainstate — the block envelope
      against the published reward set — and keep that in CI meanwhile.

## Acceptance Criteria

- `cargo xtask scoreboard` reports a mainnet replay depth alongside the Hacknet
  one.
- Every replayed mainnet header has the matching `state_index_root`.
- Every replayed transaction has the matching receipt, including status, costs
  and events.
- The replay runs offline from captured fixtures.

## The boundary is now

Checked against `api.mainnet.hiro.so` on 2026-07-30:

- mainnet runs `stacks-node 4.0.1`
- `.pox-5` activates at burn height **960,230**, and `validate_epochs` requires
  `pox_5_activation_height == Epoch40.start`, so that is the 4.0 boundary
- the burn tip was **960,227** when this was written — three Bitcoin blocks out

So the checkpoint this task needs becomes takeable within the hour, and every
mainnet parameter is now known rather than assumed: reward cycle 2100, prepare
phase 100, first burn block 666,050, boot address
`SP000000000000000000002Q6VF78`, chain id `0x00000001`.

It also means SIP-031 is live and not hypothetical. The mainnet schedule starts
at burn 907,740, long past, so every tenure nano executes mints 475 STX to
`.sip-031`, rising to 1,140 at burn 960,300 — seventy-odd blocks after the fork.
[[025-apply-the-sip-031-emission]] is load-bearing from the first block.

## The chainstate is obtainable after all

Hiro publishes dated archives of a synced mainnet chainstate, and one of them
is a **4.0.1 node dated 2026-07-30** — the same day mainnet crossed the epoch
4.0 boundary at burn 960,230:

```
https://archive.hiro.so/mainnet/stacks-blockchain/mainnet-stacks-blockchain-4.0.1-20260730.tar.zst
```

208 GiB compressed, and it holds exactly the three databases a capture reads —
`chainstate/vm/`, `chainstate/blocks/nakamoto.sqlite` and
`burnchain/sortition/marf.sqlite`. So "this needs a node nobody here has" was
wrong: it needs a large download, which is a different kind of problem.

Two things were needed to use it, one of them now done:

- `cargo xtask capture-fixtures` assumed Hacknet's layout, where a node's
  directories sit under `stacks-miner-1/nakamoto-neon`. **`--node-root` now
  names the directory directly**, which is what an archive extracts to.
- Streaming the archive through `zstd | tar` cannot resume, and a 208 GiB
  stream *will* be interrupted — it was, twice, with `Unexpected EOF in
  archive`. It has to land on disk first, resumably.

## The capture works

The archive downloads and extracts, and **a mainnet capture succeeds**:

```
captured 100 real Nakamoto blocks with a portable MARF checkpoint
chain_id = 1
checkpoint_stacks_height = 8665600
checkpoint_state_index_root = "67596465d4a6642ad6fcec1df57c6ef758fcdb0003c7ed7f952e3ced1d7f44ec"
first_stacks_height = 8665601
stacks_core_rev = "62e03cc"
```

Getting there needed care in five places, each found by running the command
rather than predicting it — see the commit. Two are worth repeating here:
mainnet has a million burn blocks and more than one snapshot per height, so the
capture takes the canonical one across the window; and mainnet runs `62e03cc`
where the in-process oracles pin `efc34a0`, so `--accept-node-revision` takes
the build by name and records it, rather than waving the guard through.

Also learned about the archive itself:

- The host throttles **per connection** — one stream settles to 6 MB/s while a
  second fresh one gets 42 — so twelve parallel ranges pull it at ~110 MB/s.
- `curl -C -` and `-r` are incompatible, and retrying a range while appending
  duplicates it, because curl re-requests the whole range. Parts must be
  written with `-o`.
- The archive dated the day mainnet crossed the boundary stops **27 burn blocks
  short of it**. The next day's reaches burn 960,341.

## The near-tip claim was the followed view, not execution

On 2026-08-01 the node's `/v2/info` stayed within zero to three blocks of the
peer for eighty minutes. That was recorded here as roughly 18,290 mainnet state
roots matching. A read of the durable store on 2026-08-02 disproved it:

```
select count(*), max(height) from marf_block;
8665602|8665601
```

The store still ends at the single anchor applied after the checkpoint. Startup
said the same thing — `sealed at ... height 8665601` — and no `accounting.json`
exists, which means `CheckpointExecutor::follow_to_tip` has never returned one
successful batch. The near-tip heights came from `NodeView.node_info`, copied
from the peer and published before execution. Absence of `StateRootMismatch` in
the log therefore proved that no mismatching block was executed, not that every
peer block was executed successfully.

The live process later fell more than a thousand blocks behind and entered a
stable loop of `peer tenure does not extend the followed chain`, so the follower
improvements are useful but not yet a durable mainnet sync result either. See
[[046-distinguish-followed-and-executed-chain-tips]] and
[[047-make-mainnet-synchronization-monotonic-and-restart]].

The state-root half of this task remains open. Evidence must come from the
offline scoreboard or from the durable executed tip and an explicit count of
roots verified, never from the peer-facing RPC height or from absence of an
execution error.

## Historical blocker: checkpoint import memory

The first import was killed after exceeding 15 GB resident against a 142 GB MARF
and 229 GB blob store. The lazy, reachable-record import work fixed that blocker:
the mainnet checkpoint now imports into the 31 GB machine and the node resumes
from it. This establishes bounded-enough bootstrap behavior, not forward replay.

## The unlock heights the capture needs

Checked against stacks-core rather than transcribed, because a wrong one
changes what execution sees:

| flag | mainnet |
|---|---|
| `--pox-v1-unlock-height` | 781,552 |
| `--pox-v2-unlock-height` | 787,652 |
| `--pox-v3-unlock-height` | 840,361 |
| `--pox-v4-unlock-height` | 960,230 |

The first three are `POX_V{1,2,3}_MAINNET_EARLY_UNLOCK_HEIGHT`, each one past
its epoch's burn height. The fourth is pox-5's activation, which
`validate_epochs` ties to the epoch 4.0 boundary and which
`api.mainnet.hiro.so/v2/pox` reports as the same 960,230.
`mainnet_unlock_heights_match_stacks_core` pins all four.

## What the archive still cannot give

**Receipts.** `events/new_block/*.json` come from an event observer attached to
a running node, and an archive holds no events. So a mainnet capture can carry
`state_index_root` per header — the claim this task is really about — and not
the per-transaction receipts that the Hacknet capture checks alongside it.

Getting those means either running a node against the restored chainstate with
an observer attached, or reading `execution_cost` back from
`/extended/v1/tx/:txid`, which is a weaker oracle than the event stream and
covers costs rather than events.

## What still blocks it

The archive supplied the MARF and sortition snapshots, and the public API
supplied the blocks. The remaining fixture problems are now concrete:

- `native-effects.json` predates the complete maturity-window export and cannot
  carry execution across the next tenure; see
  [[048-carry-complete-mainnet-tenure-accounting]]
- receipts and events still require an observer attached to stacks-core
- the 100-block capture has not produced an offline scoreboard result
- no bounded mainnet slice runs in the repository's CI

## What mainnet already proves

Execution needs a chainstate. **The envelope does not** — the reward set is
published at `/v3/stacker_set/:cycle` and the envelope is self-contained — so
that half is now checked against the chain that matters rather than against
Hacknet.

`cargo xtask verify-block <block.bin> <stacker_set.json>` checks a block
against the set published for its cycle. Twenty consecutive blocks from the
mainnet tip were **accepted, none rejected**, carrying between fourteen and
nineteen signatures of twenty-five signers and between seven and eight tenths
of the weight. nano derives the same signer signature hash mainnet signed,
recovers the same keys from it, orders them the same way, and counts the same
weight against the same threshold.

Five of them and the reward set are kept under `fixtures/mainnet/`, so the test
runs offline, and `verify-block` takes any block a node will serve for a wider
check. The repository has no root CI workflow invoking that gate yet.

This is M9 against mainnet. It says nothing about execution, which is the half
this task is really about and which remains open until the captured chainstate
replays with an explicit depth and first-divergence result.
