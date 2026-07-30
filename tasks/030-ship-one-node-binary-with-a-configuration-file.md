---
id: "030"
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

## What this left

- The signer keeps a chainstate of its own, under the same working directory,
  because it executes candidate blocks that are not canonical yet. A node that
  both signs and serves therefore imports the checkpoint twice.
- Only `mined_nakamoto_block` is posted to the event observers. `new_block`
  needs a block event context — burnchain hashes, matured rewards, the reward
  set — that nothing assembles yet, and half a payload is worse than none.
