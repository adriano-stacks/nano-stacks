---
id: "030"
title: "Ship one node binary with a configuration file"
status: pending
priority: high
effort: medium
type: feature
dependencies: ["021", "029"]
tags: ["mainnet", "node"]
created_at: 2026-07-30
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

- [ ] Give `nano-node` a binary that starts from a configuration file.
- [ ] Describe the network, the checkpoint, the peers, the Bitcoin RPC, the
      keys, the RPC bind and the event observers in that file.
- [ ] Run following, signing and mining as roles in one process.
- [ ] Keep all state under one configured directory.
- [ ] Shut down cleanly on `SIGTERM` without losing the tip.
- [ ] Reduce the harness to starting this binary.

## Acceptance Criteria

- One binary, one configuration file, one state directory.
- The Hacknet replacement run passes driving that binary.
- Stopping and restarting mid-tenure resumes without re-importing.
