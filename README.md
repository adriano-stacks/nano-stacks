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

[0]: https://github.com/stacks-network/hacknet
[1]: https://github.com/stacks-network/stacks-core/
