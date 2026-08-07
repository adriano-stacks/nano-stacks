---
id: "030"
group: build
title: "Ship one node binary with a configuration file"
status: completed
priority: high
effort: medium
type: feature
dependencies: ["021", "029"]
tags: ["mainnet", "node"]
created_at: 2026-07-30
completed_at: 2026-07-30
---

# Ship one node binary with a configuration file

## Objective

`nano-node` is a library. What runs is `nano-miner`, `nano-rpc` and
`nano-signer`, plus four miner sub-binaries, coordinated by `hacknet/harness.sh`
and a pile of environment variables and cached JSON files.

That is a test harness, and it worked as one. It is not something an operator can
run. W13 wanted a binary that takes `start --config`, holds its chainstate in one
place, and shuts down cleanly on `SIGTERM`.

## Tasks

- [x] Give `nano-node` a binary that starts from a configuration file.
- [x] Describe the network, the checkpoint, the peers, the Bitcoin RPC, the
      keys, the RPC bind and the event observers in that file.
- [x] Run following, signing and mining as roles in one process.
- [x] Keep all state under one configured directory.
- [x] Shut down cleanly on `SIGTERM` without losing the tip.
- [x] Reduce the harness to starting this binary.

## Acceptance Criteria

- One binary, one configuration file, one state directory.
- The Hacknet replacement run passes driving that binary.
- Stopping and restarting mid-tenure resumes without re-importing.

## Validation

`hacknet/harness.sh verify`, driving `stacks-node start --config` for both
roles:

```
observed 20 canonical blocks across cycles 15..=16
every one of the 20 blocks carries nano's signature
nano mined 11 of the 20 canonical blocks, at heights [342 .. 361]
12 transfer, 4 deploy, 29 call, 5 tenure change, 5 coinbase transactions,
  each with one the network reports as success
5 sortitions across 2 distinct miners
reward cycle 16 pays a waterfall set in which nano holds weight 10 of 30
```

Hacknet runs three signers of equal weight against a seven-tenths threshold, so
no block is accepted without all three: a network that keeps producing with
nano in place is proof its signature counted. The run also restarted the node
to switch the mining role on, so resuming from state on disk is on that path.

Three earlier attempts failed for reasons worth keeping:

- the export and the fixture capture published different checkpoint manifests,
  so the node could not read a Hacknet checkpoint at all — fixed
- a stale miner identity ended the whole process, taking the signer with it —
  [[039-keep-the-node-alive-when-one-role-fails]]
- two Hacknets deadlocked on their own, stock signers looping on `Last accepted
  block has timed out` with nano restored out of the network entirely. Not
  nano's, but worth knowing this environment does it.

## What this left

- The signer keeps a chainstate of its own, under the same working directory,
  because it executes candidate blocks that are not canonical yet. A node that
  both signs and serves therefore imports the checkpoint twice.
- Only `mined_nakamoto_block` is posted to the event observers. `new_block`
  needs a block event context — burnchain hashes, matured rewards, the reward
  set — that nothing assembles yet, and half a payload is worse than none.
