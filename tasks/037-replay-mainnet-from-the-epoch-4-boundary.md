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
