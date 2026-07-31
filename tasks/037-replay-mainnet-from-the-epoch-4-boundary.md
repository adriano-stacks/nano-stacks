---
id: "037"
title: "Replay mainnet from the epoch 4.0 boundary"
status: pending
priority: critical
effort: large
type: feature
dependencies: ["020", "021", "022", "023", "024", "025"]
tags: ["mainnet", "replay", "conformance"]
created_at: 2026-07-30
---

# Replay mainnet from the epoch 4.0 boundary

## Objective

The milestone that decides whether any of the rest of it worked. M10 proved nano
computes the same chain state as stacks-core for 600 Hacknet blocks from a
regtest checkpoint. This is the same claim against the chain that matters.

Everything before it is a component check. The oracle is the same as M10's —
`state_index_root` per header and every receipt from the event observer — pointed
at mainnet blocks after the 4.0 boundary instead of captured Hacknet ones.

Replay depth is the metric again, and it stays at zero until the blockers this
depends on are done.

## Tasks

- [ ] Capture a mainnet checkpoint at or after the 4.0 boundary, with the blocks,
      the burn blocks and the receipts that follow it.
- [ ] Teach the fixture tooling and the scoreboard about a mainnet capture.
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

## The transfer works; the chainstate does not fit

The archive downloads fine. The host throttles **per connection** — one stream
settles to 6 MB/s while a second fresh one gets 42 — so twelve parallel ranges
pull it at about 110 MB/s, and all 223,511,739,939 bytes arrived and joined to
exactly that size.

Two things went wrong on the way, both worth keeping:

- `curl -C -` and `-r` are incompatible, and retrying a *range* while appending
  with `>>` duplicates it: curl's own `--retry` re-requests the whole range.
  That produced an archive 7.4 GB too long. Each part must be written with
  `-o`, so a retry overwrites.
- **It does not fit.** `chainstate/vm/clarity/marf.sqlite.blobs` alone is
  228 GB and `marf.sqlite` more than 146 GB, and extraction needs the 208 GiB
  archive present throughout. Peak demand is over 600 GB against the 612 GB
  free here, and the extraction filled the disk to 100% before being stopped.

That was foreseeable from the archive's own contents and should have been
computed before spending the bandwidth. Everything downloaded has been removed
and the disk restored.

So the blocker is no longer the data — it is **room for it**. What this needs
is a machine with roughly a terabyte free, or better, running the capture where
a synced node already lives, which avoids the archive entirely.

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

The fixtures this task replays cannot be taken from a public API:

- the MARF checkpoint needs a stacks-core node's `chainstate/vm/clarity`
  directory, or a published PCS export — `cargo xtask capture-fixtures` takes a
  `--state-dir`, not a URL
- `sortition/snapshots.json` is a dump of a node's `burnchain/sortition/marf.sqlite`
- `events/new_block/*.json` come from an event observer attached to a node

Blocks are the one part `/v3/blocks/:id` can serve. So this needs either a
synced mainnet stacks-core node or a checkpoint published by someone who has
one, which is the same dependency [[031-establish-a-trust-root-for-the-checkpoint]]
has to answer anyway.

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

Five of them and the reward set are kept under `fixtures/mainnet/`, so it runs
offline in CI as a gate, and `verify-block` takes any block a node will serve
for a wider check.

This is M9 against mainnet. It says nothing about execution, which is the half
this task is really about and which still waits on a chainstate.
