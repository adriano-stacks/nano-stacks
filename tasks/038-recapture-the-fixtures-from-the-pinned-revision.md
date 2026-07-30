---
id: "038"
title: "Recapture the fixtures from the pinned stacks-core revision"
status: pending
priority: critical
effort: medium
type: bug
dependencies: []
tags: ["conformance", "fixtures", "costs"]
created_at: 2026-07-30
---

# Recapture the fixtures from the pinned stacks-core revision

## Objective

The captured fixtures and the pinned stacks-core disagree about what epoch 4.0
costs, and the scoreboard's `replay: costs` row cannot go green while they do.

`crates/nano-conformance/fixtures/provenance.toml` records
`hacknet_commit = "bf821e9d..."`. The revision every conformance oracle compares
against is `efc34a07`. Running the block-22 transaction through the *pinned*
interpreter, on the checkpoint's own state and burn view:

| schedule | interpreter runtime | fixture expects |
|---|---|---|
| `Epoch40` → `costs-5` | 239,738 | — |
| `Epoch34` → `costs-4` | **481,082** | **481,082** |

The other four dimensions are identical under both schedules, which is exactly
the shape of the divergence: only runtime moved, by about half. The pinned
clarity maps `Epoch40` to `COSTS_5`, so the node that produced these fixtures
was charging `costs-4` runtime at epoch 4.0.

`replay: costs` therefore asks for something no correct implementation can
give: the interpreter oracle demands `costs-5`, the fixture demands `costs-4`.

## Why now

A Hacknet booted on 2026-07-30 reports
`stacks-node 4.0.1 (efc34a0, debug build)` — the pinned revision exactly. A
capture taken from it is directly comparable to every in-process oracle, which
this fixture tree is not.

## Tasks

- [x] Record the receipts a capture reads. Hacknet gives its nodes one event
      observer, the signer, whose keys carry no `new_block`, so nothing wrote
      them — `harness.sh observe` adds a sink and restarts the miners onto it.
- [x] Grow a Hacknet on the pinned revision past a checkpoint plus the replay
      window.
- [x] Recapture with `cargo xtask capture-fixtures`, recording the revision in
      `provenance.toml` alongside the Hacknet commit.
- [x] Confirm `replay: state root` and `replay: receipts` stay at their full
      depth, and see where `replay: costs` lands.
- [ ] Capture a window whose maturing tenures are all Nakamoto ones, so the
      accounting the replay needs exists (see below), and install it.
- [ ] Include the checkpoint height's own block, so the attestation test in
      [[031-establish-a-trust-root-for-the-checkpoint]] can attest `checkpoint-H`
      itself rather than standing in a later block. The capture starts one block
      after the checkpoint, so that header is missing today.
- [x] Make the capture refuse to record a node whose revision is not the pinned
      one, so this cannot recur silently.

## Acceptance Criteria

- The fixtures and the pinned stacks-core agree about the cost schedule.
- `replay: costs` either reaches full depth or fails for a reason that is nano's.
- `provenance.toml` names the stacks-core revision the capture came from.

## The guard

`cargo xtask capture-fixtures` now reads the pinned revision out of the
workspace lockfile — not a copy restated in the tool, which could drift — asks
the node for its `server_version`, and refuses the capture unless they agree.
Checked against the running Hacknet: lockfile `efc34a07a225…`, node
`stacks-node 4.0.1 (efc34a0, debug build)`, accepted.

The capture itself still needs a Hacknet grown past a checkpoint plus the
replay window. The one that is up was booted from genesis today and is still
short of it.

## The missing observer

The reason a recapture was not simply a command: Hacknet configures exactly one
event observer per node, the signer, with
`events_keys = ["stackerdb", "block_proposal", "burn_blocks"]`. `new_block` is
not among them, so the per-transaction receipts the capture reads were never
written by anything.

`harness.sh observe` adds a second observer with `events_keys = ["*"]`, pointed
at a sink container on the Hacknet network, and restarts the miners onto it —
a restart, not a wipe, so the chain carries on. The sink writes
`new_block/<height>-<hash>.json`, the name the capture looks for.

It has to run inside the network: a node here cannot reach a host port through
the bridge gateway, and an observer it cannot reach it retries forever, which
is its own way of stalling a run.

## The measurement

A capture was taken from a Hacknet built at the pinned revision — 340 blocks
from Stacks height 461, checkpoint at 460, receipts recorded by the sink above.
Replayed:

```
replay: state root   340/340
replay: receipts     340/340
replay: costs         75/340   block 76: expected (54, 139581, 231186, 17, 1391)
                                              got (54, 139581, 232029, 17, 1391)
```

Against the stale fixtures the same row reads `expected … 481082 … got 240515`
— a factor of two. Against a node built at the revision the oracles compare
with, four dimensions match exactly and runtime differs by **843 in 231,186,
which is 0.36%**.

That is the residual [[023-close-the-execution-cost-divergence]] already
identified and measured: `InnerTypeCheckCost` and the value-copy cost are
charged on the declared type's maximum size rather than the value's actual
size. The costs-4/costs-5 question is settled — the schedule is right, the
fixtures were old.

## Why the capture is not installed

The capture replays only 75 blocks before failing on
`tenure 21 matured without accounting: its rewards are neither checkpointed nor
executed`.

Rewards mature a hundred tenures later, so a window starting at tenure T needs
priced tenures back to T-100. This window reaches tenure 21, which predates
Nakamoto and has no row in `nakamoto_tenure_events`, so `write_native_effects`
skips it and the replay finds a hole. The old fixtures happen to start late
enough that every maturing tenure is a Nakamoto one.

The next capture needs `--first-height` chosen so that the earliest maturing
tenure is at or after the first Nakamoto tenure — or `write_native_effects`
taught to price a pre-Nakamoto tenure rather than skip it. The Hacknet that
produced this one stalled at Stacks 809 before a later window was available.

The fixtures in the tree are unchanged, and the tree is green at 600/600.
