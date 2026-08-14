# `nano-stacks`

> [!WARNING]
> Do not use in production, this will eat your bytes. You have been warned.

Adriano's summer 2026 hackday project.

## What

> Re-implement [stacks-core][1] from scratch as **nano-stacks**: a Stacks node
  supporting **epoch 4.0 only** (no epoch 2.x/3.x transition machinery), which
  starts from an attested checkpoint, syncs, follows and **executes mainnet**,
  and mines/signs inside [hacknet][0].

`nano-stacks` shows what a Stacks node could be. In a future, not too far,
parallel universe.

It’s a Stacks node, that:

- Remains at chain tip.
- Supports Epoch 4.0 only.
- Deploys and runs Clarity WASM contracts.
- Reimplements the full Stacks protocol from scratch, while relying on
  external libraries for what’s outside the business domain.

## Run it

See [Run a mainnet node](docs/running-a-mainnet-node.md) for the full follower
setup, covering the checkpoint files, Bitcoin access, configuration, build,
start, health checks, and restart. See [Build a mainnet
checkpoint](docs/build-mainnet-checkpoint.md) for how to download and convert
the Hiro archive as a checkpoint.

## Inspect it

`nano-tui` is a read-only dashboard and block/transaction explorer for one
running node:

```bash
nix develop -c cargo run -p nano-tui -- --rpc-url http://127.0.0.1:20443
```

Use the arrow keys to select, Enter or Right to open, Escape or Left to go back,
`m` for the current miner election, `r` to refresh and `q` to quit. Add `--once`
to print one 110x32 frame for a log or script. `--help` lists all command-line
options, including the optional metrics endpoint. A one-frame check exits 0 when
every source answered, 2 for a partial/degraded snapshot and 3 when the node is
unreachable.

[0]: https://github.com/stacks-network/hacknet
[1]: https://github.com/stacks-network/stacks-core/
